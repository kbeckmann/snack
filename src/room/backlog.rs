// Windowed history shared by rooms and DMs.
//
// The full chat log lives in SQLite (see storage::history). In memory each
// conversation holds only a bounded *window* of it:
//
//   * Normally the window is the live tail — the newest messages — and new
//     live messages extend it. It is trimmed back to LIVE_TAIL_CAP whenever the
//     user is parked at the bottom, so a multi-hour session never grows the
//     rendered set (and thus per-frame layout cost) without bound.
//   * Scrolling up pages older chunks in at the front (PAGE_SIZE at a time),
//     letting the window grow up to MAX_LOADED while browsing.
//   * Jumping to a search hit replaces the window with a slice centred on the
//     hit (a "detached" window); live messages then only go to the DB until the
//     user returns to the live tail.
//
// All of this is index-backed in SQLite, so extracting any sub-window is O(log
// n) regardless of how long the log is.

use chrono::{ DateTime, Utc };

use crate::room::message::Message;
use crate::storage::history::StoredMessage;

// Messages kept in memory at the live tail. Bounds steady-state render cost.
pub const LIVE_TAIL_CAP: usize = 300;
// Older messages pulled in per scroll-up page.
pub const PAGE_SIZE: usize = 150;
// Hard ceiling on the in-memory window while browsing history; a safety valve
// so runaway streaming or very deep scrolling can't grow it unbounded.
pub const MAX_LOADED: usize = 1500;
// How many messages of context to load on either side of a search hit.
pub const JUMP_CONTEXT: usize = 120;

fn to_message(s: StoredMessage) -> Message
{
    return Message::Chat
    {
        id: s.id,
        from: s.sender,
        body: s.body,
        received: s.received,
        status: s.delivery.into(),
    };
}

fn chronological_key(message: &Message) -> (i64, i64)
{
    match message {
        Message::Chat { id, received, .. } => (received.timestamp_millis(), if *id > 0 { *id } else { i64::MAX }),
        Message::Event { received, .. } => (received.timestamp_millis(), i64::MAX),
    }
}

// A durable row identity (with a chronological fallback) survives replacing
// the rendered window. `after` also represents the empty "next message" boundary.
#[derive(Clone, Copy, Debug)]
pub struct ReadBoundary {
    pub id: Option<i64>,
    pub received: Option<DateTime<Utc>>,
    pub after: bool,
}

impl ReadBoundary {
    fn from_message(message: &Message, after: bool) -> Self {
        match message {
            Message::Chat { id, received, .. } => Self { id: Some(*id), received: Some(*received), after },
            Message::Event { received, .. } => Self { id: None, received: Some(*received), after },
        }
    }
    fn key(self) -> Option<(i64, i64)> {
        self.received.map(|received| (received.timestamp_millis(), self.id.filter(|id| *id > 0).unwrap_or(i64::MAX)))
    }
}

pub trait Backlog
{
    fn messages(&self) -> &Vec<Message>;
    fn messages_mut(&mut self) -> &mut Vec<Message>;
    fn parked_messages_mut(&mut self) -> &mut Vec<Message>;

    fn park_unpersisted(&mut self)
    {
        let old = std::mem::take(self.messages_mut());
        self.parked_messages_mut().extend(old.into_iter().filter(|m|
            matches!(m, Message::Chat { id, .. } if *id < 0)));
    }
    fn promote_message(&mut self, temp_id: i64, stored: &StoredMessage)
    {
        self.remember_read_boundary();
        self.remap_read_identity(temp_id, stored.id, stored.received);
        self.remap_read_identity(stored.id, stored.id, stored.received);
        self.parked_messages_mut().retain(|m| !matches!(m, Message::Chat { id, .. } if *id == temp_id));
        // A retained outgoing copy may already have its durable ID. Only a
        // different row with the canonical ID makes the old row redundant.
        let already_loaded = temp_id != stored.id
            && self.messages().iter().any(|m| matches!(m, Message::Chat { id, .. } if *id == stored.id));
        if already_loaded {
            if let Some(index) = self.messages().iter().position(|m| matches!(m, Message::Chat { id, .. } if *id == temp_id)) {
                self.messages_mut().remove(index);
                if let Some(marker) = self.read_marker() {
                    self.set_read_marker(Some(marker - usize::from(marker > index)));
                }
                // An identity merge may move reconnect boundaries. Drop those
                // presentation-only markers rather than pointing at wrong rows.
                self.reset_history_markers();
            }
        }
        // Refresh the surviving canonical copy as well as a newly promoted
        // row: storage may have corrected its timestamp or delivery state.
        for m in self.messages_mut() {
            if matches!(m, Message::Chat { id, .. } if *id == temp_id || *id == stored.id) {
                *m = to_message(stored.clone());
                break;
            }
        }
        self.sort_chronologically();
    }

    fn reconcile_stored(&mut self, stored: &StoredMessage, merged: &[i64])
    {
        self.remember_read_boundary();
        self.remap_read_identity(stored.id, stored.id, stored.received);
        for id in merged { self.remap_read_identity(*id, stored.id, stored.received); }
        let mut present = self.messages().iter().any(|message| matches!(message, Message::Chat { id, .. } if *id == stored.id));
        let mut removed = false;
        for index in (0..self.messages().len()).rev() {
            let duplicate = matches!(&self.messages()[index], Message::Chat { id, .. } if merged.contains(id));
            if duplicate {
                if !present {
                    self.messages_mut()[index] = to_message(stored.clone());
                    present = true;
                } else {
                    self.messages_mut().remove(index);
                    if let Some(marker) = self.read_marker() { self.set_read_marker(Some(marker - usize::from(marker > index))); }
                }
                removed = true;
            }
        }
        for message in self.messages_mut() {
            if matches!(message, Message::Chat { id, .. } if *id == stored.id) { *message = to_message(stored.clone()); }
        }
        if removed { self.reset_history_markers(); }
        self.sort_chronologically();
    }

    fn read_boundary(&self) -> Option<ReadBoundary>;
    fn set_read_boundary(&mut self, boundary: Option<ReadBoundary>);

    fn remember_read_boundary(&mut self) {
        if self.read_boundary().is_some() { return; }
        let Some(index) = self.read_marker() else { return; };
        let boundary = self.messages().get(index).map(|message| ReadBoundary::from_message(message, false))
            .or_else(|| self.messages().last().map(|message| ReadBoundary::from_message(message, true)))
            .unwrap_or(ReadBoundary { id: None, received: None, after: true });
        self.set_read_boundary(Some(boundary));
    }

    fn refresh_read_marker(&mut self) {
        let Some(boundary) = self.read_boundary() else { return; };
        let exact = boundary.id.and_then(|target| self.messages().iter().position(|message|
            matches!(message, Message::Chat { id, .. } if *id == target)));
        let marker = exact.map(|index| index + usize::from(boundary.after)).unwrap_or_else(|| {
            let Some(key) = boundary.key() else { return 0; };
            self.messages().iter().position(|message| {
                let candidate = chronological_key(message);
                if boundary.after { candidate > key } else { candidate >= key }
            }).unwrap_or(self.messages().len())
        });
        self.set_read_marker(Some(marker));
    }

    fn stamp_read_boundary(&mut self) {
        self.set_read_boundary(None);
        self.set_read_marker(Some(self.messages().len()));
        self.remember_read_boundary();
    }

    fn clear_read_boundary(&mut self) {
        self.set_read_boundary(None);
        self.set_read_marker(None);
    }

    fn note_unread(&mut self, id: i64, received: DateTime<Utc>) {
        self.remember_read_boundary();
        let candidate = ReadBoundary { id: Some(id), received: Some(received), after: false };
        if self.read_boundary().is_none_or(|old| old.after || candidate.key() < old.key()) {
            self.set_read_boundary(Some(candidate));
        }
        self.refresh_read_marker();
    }

    fn remap_read_identity(&mut self, previous: i64, id: i64, received: DateTime<Utc>) {
        if let Some(mut boundary) = self.read_boundary() {
            if boundary.id == Some(previous) {
                boundary.id = Some(id);
                boundary.received = Some(received);
                self.set_read_boundary(Some(boundary));
            }
        }
    }

    fn read_marker(&self) -> Option<usize>;
    fn set_read_marker(&mut self, m: Option<usize>);
    // Rooms track reconnect boundaries in the current window; DMs have none.
    fn reset_history_markers(&mut self) {}
    fn shift_history_markers(&mut self, _removed: usize, _added: usize) {}
    fn remap_history_markers(&mut self, _positions: &[usize]) {}

    // Match SQLite's (millisecond timestamp, row ID) ordering. Keep transient
    // rows/events stable within a millisecond until a durable identity exists.
    // Remap presentation boundaries by their original rows, not by timestamp.
    fn sort_chronologically(&mut self) -> Vec<usize>
    {
        self.remember_read_boundary();
        let mut indexed: Vec<_> = std::mem::take(self.messages_mut()).into_iter().enumerate().collect();
        indexed.sort_by_key(|(_, message)| chronological_key(message));
        let mut positions = vec![0; indexed.len()];
        for (position, (old, _)) in indexed.iter().enumerate() { positions[*old] = position; }
        *self.messages_mut() = indexed.into_iter().map(|(_, message)| message).collect();
        if let Some(marker) = self.read_marker() {
            self.set_read_marker(Some(positions.get(marker).copied().unwrap_or(positions.len())));
        }
        self.remap_history_markers(&positions);
        self.refresh_read_marker();
        self.refresh_oldest_id();
        positions
    }
    fn oldest_loaded_id(&self) -> Option<i64>;
    fn set_oldest_loaded_id(&mut self, id: Option<i64>);
    fn local_cursor(&self) -> Option<(i64, i64)>;
    fn set_local_cursor(&mut self, cursor: Option<(i64, i64)>);
    fn has_older(&self) -> bool;
    fn set_has_older(&mut self, v: bool);
    fn set_at_live_tail(&mut self, v: bool);
    fn anchored_bottom(&self) -> bool;
    fn set_anchored_bottom(&mut self, v: bool);
    // XEP-0313 MAM paging state: whether a server archive query is outstanding,
    // whether the server archive is exhausted, and the RSM cursor for the next
    // older page.
    fn mam_in_flight(&self) -> bool;
    fn set_mam_in_flight(&mut self, v: bool);
    fn mam_exhausted(&self) -> bool;
    fn set_mam_exhausted(&mut self, v: bool);
    fn mam_cursor(&self) -> Option<String>;
    fn set_mam_cursor(&mut self, v: Option<String>);

    // The DB paging frontier is independent of later timestamp corrections.
    // Moving a visible row earlier must not skip rows before its old position.
    // Moving the earliest visible row later can expose an additional gap.
    fn local_page_cursor(&self) -> Option<(i64, i64)>
    {
        let visible = self.messages().iter().find(|message| matches!(message, Message::Chat { id, .. } if *id > 0))
            .map(chronological_key);
        self.local_cursor().max(visible)
    }

    fn loaded_ids(&self) -> Vec<i64>
    {
        self.messages().iter().filter_map(|message| match message {
            Message::Chat { id, .. } if *id > 0 => Some(*id), _ => None,
        }).collect()
    }

    // Whether more history might exist either locally (DB) or on the server —
    // drives the "earlier messages" affordance and scroll-up fetching.
    fn more_history(&self) -> bool
    {
        return self.has_older() || !self.mam_exhausted();
    }

    // Recompute the oldest loaded DB id by finding the front-most persisted
    // (Chat) message. Events carry no id and are skipped.
    fn refresh_oldest_id(&mut self)
    {
        let visible = self.messages().iter().find(|message| matches!(message, Message::Chat { id, .. } if *id > 0))
            .map(chronological_key);
        let previous = self.local_cursor();
        self.set_oldest_loaded_id(visible.map(|(_, id)| id));
        if previous.is_some() && visible > previous { self.set_has_older(true); }
        self.set_local_cursor(previous.max(visible));
    }

    // Replace the window with a freshly loaded live tail (oldest-first).
    fn seed_tail(&mut self, tail: Vec<StoredMessage>, has_older: bool)
    {
        self.remember_read_boundary();
        self.park_unpersisted();
        self.set_local_cursor(tail.first().map(|row| (row.received.timestamp_millis(), row.id)));
        let mut messages: Vec<_> = tail.into_iter().map(to_message).collect();
        messages.append(self.parked_messages_mut());
        messages.sort_by_key(chronological_key);
        self.reset_history_markers();
        *self.messages_mut() = messages;
        self.refresh_read_marker();
        self.set_has_older(has_older);
        self.set_at_live_tail(true);
        self.set_anchored_bottom(true);
        self.trim_front_to(LIVE_TAIL_CAP);
        self.refresh_oldest_id();
    }

    // Append a live message to the tail. Trims the front back to LIVE_TAIL_CAP
    // when the user is parked at the bottom (so the trim is off-screen), and
    // unconditionally once MAX_LOADED is exceeded as a safety valve.
    // Returns the inserted row's visible index, or None when it was older
    // than the retained window and trimmed immediately.
    fn append_live(&mut self, msg: Message) -> Option<usize>
    {
        self.remember_read_boundary();
        let old_len = self.messages().len();
        self.messages_mut().push(msg);
        let positions = self.sort_chronologically();
        let inserted = positions[old_len];
        let len = self.messages().len();
        let removed = if (self.anchored_bottom() && len > LIVE_TAIL_CAP) || len > MAX_LOADED {
            len - LIVE_TAIL_CAP
        } else { 0 };
        if removed > 0 { self.trim_front_to(LIVE_TAIL_CAP); }
        inserted.checked_sub(removed)
    }

    // Page older messages (oldest-first) in at the front, preserving the read
    // marker's logical target.
    fn prepend_older(&mut self, older: Vec<StoredMessage>, has_older: bool)
    {
        self.set_local_cursor(self.local_page_cursor());
        if older.is_empty()
        {
            self.set_has_older(has_older);
            return;
        }

        self.remember_read_boundary();
        // Keep the window's upper frontier. In-flight archive results can
        // arrive inside a range just read; SQL excludes visible IDs to advance.
        let positions: std::collections::HashMap<_, _> = self.messages().iter().enumerate().filter_map(|(index, message)| match message {
            Message::Chat { id, .. } if *id > 0 => Some((*id, index)), _ => None,
        }).collect();
        let mut head = Vec::new();
        for stored in older {
            if let Some(&index) = positions.get(&stored.id) {
                // A corrected row can cross the frontier and appear again in
                // a later DB page. Refresh its canonical copy without duplication.
                self.remap_read_identity(stored.id, stored.id, stored.received);
                self.messages_mut()[index] = to_message(stored);
            } else { head.push(to_message(stored)); }
        }
        let n = head.len();
        self.shift_history_markers(0, n);
        head.append(self.messages_mut());
        *self.messages_mut() = head;

        if let Some(m) = self.read_marker()
        {
            self.set_read_marker(Some(m + n));
        }
        self.set_has_older(has_older);
        self.sort_chronologically();
    }

    // Drop the oldest messages so at most `cap` remain, keeping the newest.
    fn trim_front_to(&mut self, cap: usize)
    {
        let len = self.messages().len();
        if len <= cap
        {
            return;
        }

        self.remember_read_boundary();
        let drop = len - cap;
        let removed: Vec<_> = self.messages_mut().drain(0..drop).collect();
        let dropped_persisted = removed.iter().any(|m| matches!(m, Message::Chat { id, .. } if *id > 0));
        self.parked_messages_mut().extend(removed.into_iter().filter(|m|
            matches!(m, Message::Chat { id, .. } if *id < 0)));
        self.shift_history_markers(drop, 0);

        if let Some(m) = self.read_marker()
        {
            self.set_read_marker(Some(m.saturating_sub(drop)));
        }
        if dropped_persisted {
            self.set_has_older(true);
            if !self.messages().iter().any(|message| matches!(message, Message::Chat { id, .. } if *id > 0)) {
                self.set_local_cursor(None);
            }
        }
        self.refresh_read_marker();
        self.refresh_oldest_id();
    }

    // Replace the window with a detached slice centred on a search hit.
    fn set_window(&mut self, window: Vec<StoredMessage>, has_older: bool, at_tail: bool)
    {
        self.remember_read_boundary();
        self.park_unpersisted();
        self.reset_history_markers();
        self.set_local_cursor(window.first().map(|row| (row.received.timestamp_millis(), row.id)));
        *self.messages_mut() = window.into_iter().map(to_message).collect();
        self.set_has_older(has_older);
        self.set_at_live_tail(at_tail);
        // Including the newest row does not prove a search hit is at the
        // bottom of the viewport. Layout/scroll feedback establishes that.
        self.set_anchored_bottom(false);
        self.refresh_read_marker();
        self.refresh_oldest_id();
    }
}
