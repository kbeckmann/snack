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
    };
}

pub trait Backlog
{
    fn messages(&self) -> &Vec<Message>;
    fn messages_mut(&mut self) -> &mut Vec<Message>;
    fn read_marker(&self) -> Option<usize>;
    fn set_read_marker(&mut self, m: Option<usize>);
    fn oldest_loaded_id(&self) -> Option<i64>;
    fn set_oldest_loaded_id(&mut self, id: Option<i64>);
    fn has_older(&self) -> bool;
    fn set_has_older(&mut self, v: bool);
    fn set_at_live_tail(&mut self, v: bool);
    fn anchored_bottom(&self) -> bool;
    fn set_anchored_bottom(&mut self, v: bool);

    // Recompute the oldest loaded DB id by finding the front-most persisted
    // (Chat) message. Events carry no id and are skipped.
    fn refresh_oldest_id(&mut self)
    {
        let id = self.messages().iter().find_map(|m| match m
        {
            Message::Chat { id, .. } => Some(*id),
            _ => None,
        });
        self.set_oldest_loaded_id(id);
    }

    // Replace the window with a freshly loaded live tail (oldest-first).
    fn seed_tail(&mut self, tail: Vec<StoredMessage>, has_older: bool)
    {
        *self.messages_mut() = tail.into_iter().map(to_message).collect();
        self.set_has_older(has_older);
        self.set_at_live_tail(true);
        self.set_anchored_bottom(true);
        self.refresh_oldest_id();
    }

    // Append a live message to the tail. Trims the front back to LIVE_TAIL_CAP
    // when the user is parked at the bottom (so the trim is off-screen), and
    // unconditionally once MAX_LOADED is exceeded as a safety valve.
    fn append_live(&mut self, msg: Message)
    {
        self.messages_mut().push(msg);

        let len = self.messages().len();
        if (self.anchored_bottom() && len > LIVE_TAIL_CAP) || len > MAX_LOADED
        {
            self.trim_front_to(LIVE_TAIL_CAP);
        }
    }

    // Page older messages (oldest-first) in at the front, preserving the read
    // marker's logical target.
    fn prepend_older(&mut self, older: Vec<StoredMessage>, has_older: bool)
    {
        if older.is_empty()
        {
            self.set_has_older(has_older);
            return;
        }

        let n = older.len();
        let mut head: Vec<Message> = older.into_iter().map(to_message).collect();
        head.append(self.messages_mut());
        *self.messages_mut() = head;

        if let Some(m) = self.read_marker()
        {
            self.set_read_marker(Some(m + n));
        }
        self.set_has_older(has_older);
        self.refresh_oldest_id();
    }

    // Drop the oldest messages so at most `cap` remain, keeping the newest.
    fn trim_front_to(&mut self, cap: usize)
    {
        let len = self.messages().len();
        if len <= cap
        {
            return;
        }

        let drop = len - cap;
        self.messages_mut().drain(0..drop);

        if let Some(m) = self.read_marker()
        {
            self.set_read_marker(Some(m.saturating_sub(drop)));
        }
        // Anything we just dropped is still in the DB, so older history exists.
        self.set_has_older(true);
        self.refresh_oldest_id();
    }

    // Replace the window with a detached slice centred on a search hit.
    fn set_window(&mut self, window: Vec<StoredMessage>, has_older: bool, at_tail: bool)
    {
        *self.messages_mut() = window.into_iter().map(to_message).collect();
        self.set_has_older(has_older);
        self.set_at_live_tail(at_tail);
        self.set_anchored_bottom(at_tail);
        self.set_read_marker(None);
        self.refresh_oldest_id();
    }
}
