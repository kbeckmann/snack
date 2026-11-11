use crate::storage::history::UnqueuedDelivery;
use iced::Task;
use iced::widget::{ text_editor, Id };
use log::{ error, warn };

use crate::app::{
    delay_then, focus_find_input, focus_input, focus_jid_input, focus_join_input, snap_to_bottom,
    AppState, ArchiveContext, RoomConnection, NickCompleteState, Selection, Snack,
    ACCOUNT_PASSWORD_INPUT_ID, MESSAGE_INPUT_ID,
};
use crate::message::Message;
use crate::room::message::ChatStatus;
use crate::room::backlog::{ self, Backlog };
use crate::storage::history::History;
use crate::{ room, storage, xmpp };

// An optimistically-shown outgoing room message stays badge-free for this long,
// so a normal fast echo never flickers a "sending…" indicator; only a slow send
// grows one.
const SEND_PENDING_GRACE: std::time::Duration = std::time::Duration::from_millis(500);
// Yield between bounded batches while filling a large offline gap.
const CATCHUP_MAX_PAGES: u32 = 40;
const HISTORY_RETRY_BATCH: usize = 100;
// Longest wait for a rejoined room's subject (the end of its join history)
// before replaying queued sends without it.
const ROOM_HISTORY_REPLAY_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
// A delivery check reads the archive from shortly before the oldest queued
// send (local and server clocks differ) and gives up after this many pages.
const DELIVERY_CHECK_SKEW: chrono::TimeDelta = chrono::TimeDelta::minutes(5);
const DELIVERY_CHECK_PAGES: u32 = 10;

enum OwnDirectCopy
{
    Stored,
    Unmatched,
    Pending(i64),
}

enum MessageErrorTarget
{
    Queued(storage::history::OutboxItem),
    Unqueued(String, storage::history::StoredMessage),
}

// A known ID still needs its sender checked against the original session.
// Read errors retain the report; a successful lookup of an unrelated sender
// or unknown origin can be discarded without changing any delivery state.
fn stored_error_target(history: &History, error: &crate::app::PendingMessageError) -> Result<Option<MessageErrorTarget>, ()>
{
    let durable_unavailable = match history.outgoing_by_origin(&error.origin_id) {
        Ok(Some(message)) => return Ok(message.accepts_error_from(error.from.as_deref(), &error.own_jid)
            .then_some(MessageErrorTarget::Queued(message))),
        Ok(None) => false,
        Err(_) => true,
    };
    match history.unqueued_direct(&error.origin_id) {
        Ok(Some((conversation, stored))) => {
            let context = storage::history::OutboxItem { id: 0, origin_id: error.origin_id.clone(),
                conversation: conversation.clone(), is_room: false, body: stored.body.clone() };
            Ok((stored.delivery != storage::history::DeliveryState::UnqueuedFailed
                && context.accepts_error_from(error.from.as_deref(), &error.own_jid))
                .then_some(MessageErrorTarget::Unqueued(conversation, stored)))
        }
        Ok(None) if !durable_unavailable => Ok(None),
        _ => Err(()),
    }
}

// An authenticated exact origin confirms delivery even when the supplied server
// ID already belongs to unrelated history. Preserve that row and decline the
// conflicting alias instead of retaining an unrecoverable pending write.
fn store_own_direct_copy(history: &History, conversation: &str, body: &str, origin: &str,
    received: chrono::DateTime<chrono::Utc>, stanza_id: Option<&str>, delayed: bool)
    -> rusqlite::Result<Option<(storage::history::StoredMessage, Vec<i64>)>>
{
    match history.reconcile_own_direct_copy(conversation, body, origin, received, stanza_id, delayed) {
        Err(rusqlite::Error::InvalidQuery) if stanza_id.is_some() =>
            history.reconcile_own_direct_copy(conversation, body, origin, received, None, delayed),
        result => result,
    }
}

// The caller has verified the current occupant identity, or retained that
// authority from a live echo before an outage. A conflicting server alias
// must not prevent exact-origin delivery confirmation or overwrite its owner.
fn store_own_room_copy(history: &History, conversation: &str, nick: &str, body: &str, origin: &str,
    received: chrono::DateTime<chrono::Utc>, stanza_id: Option<&str>)
    -> rusqlite::Result<Option<(storage::history::StoredMessage, Vec<i64>, bool)>>
{
    match history.reconcile_own_room_origin(conversation, nick, body, origin, received, stanza_id) {
        Err(rusqlite::Error::InvalidQuery) if stanza_id.is_some() =>
            history.reconcile_own_room_origin(conversation, nick, body, origin, received, None),
        result => result,
    }
}

fn store_pending_history(history: &History, pending: &crate::app::PendingHistoryWrite)
    -> rusqlite::Result<(storage::history::StoredMessage, Vec<i64>)>
{
    if let Some(origin) = &pending.own_origin {
        let found = if pending.own_room_origin {
            store_own_room_copy(history, &pending.conversation, &pending.sender, &pending.body, origin, pending.received, pending.stanza_id.as_deref())
                .map(|found| found.map(|(stored, ids, _)| (stored, ids)))
        } else { store_own_direct_copy(history, &pending.conversation, &pending.body, origin, pending.received, pending.stanza_id.as_deref(), pending.delayed) }?;
        if let Some(found) = found { return Ok(found); }
        return history.store_received(pending.receipt, &pending.conversation, &pending.sender, &pending.body, pending.received,
            pending.delayed, pending.stanza_id.as_deref(), false).map(|(stored, _)| (stored, Vec::new()));
    }
    if let Some(outbox_id) = pending.outbox_id {
        history.store_direct_message(&pending.conversation, &pending.sender, &pending.body, pending.received, outbox_id)
    } else {
        history.store_received(pending.receipt, &pending.conversation, &pending.sender, &pending.body,
            pending.received, pending.delayed, pending.stanza_id.as_deref(), pending.allow_outgoing_fuzzy).map(|(stored, _)| stored)
    }.map(|stored| (stored, Vec::new()))
}

// Load a conversation's persisted live tail into its in-memory window. When the
// history DB is unavailable this just marks the window live (no persistence).
fn seed_tail_for(history: &Option<History>, conv: &str, b: &mut dyn Backlog)
{
    if let Some(h) = history
    {
        let result = (|| -> rusqlite::Result<_> {
            let tail = h.load_tail(conv, backlog::LIVE_TAIL_CAP)?;
            let has_older = tail.first().map(|m| h.has_older(conv, m.received.timestamp_millis(), m.id))
                .transpose()?.unwrap_or(false);
            Ok((tail, has_older))
        })();
        match result {
            Ok((tail, has_older)) => b.seed_tail(tail, has_older),
            Err(e) => {
                warn!("Cannot load history for {}: {}", conv, e);
                // The old window is still usable, but it cannot establish
                // that the latest messages are visible or have been read.
                b.set_at_live_tail(false);
            }
        }
    }
    else
    {
        b.set_at_live_tail(true);
        b.set_anchored_bottom(true);
    }
}

// The URL under an editor position. Columns are byte offsets within the line;
// the end is inclusive because clicking a link's last glyph on its right half
// places the caret after it.
pub(crate) fn url_at(text: &str, position: text_editor::Position) -> Option<String>
{
    let line = text.split('\n').nth(position.line)?;
    let mut start = 0;
    for (part, is_url) in crate::ui::chat::parse_urls(line) {
        let end = start + part.len();
        if is_url && (start..=end).contains(&position.column) { return Some(part.to_owned()); }
        start = end;
    }
    None
}

impl Snack
{
    // Persist a chat message, deduplicating. Returns its history row id, None for
    // a dropped duplicate, or a unique temporary id when persistence is unavailable.
    fn persist_message(
        &mut self,
        conversation: &str,
        sender: &str,
        body: &str,
        received: chrono::DateTime<chrono::Utc>,
        delayed: bool,
        stanza_id: Option<&str>,
    ) -> Option<i64>
    {
        self.persist_message_with_policy(conversation, sender, body, received, delayed, stanza_id, true)
    }

    fn persist_message_with_policy(
        &mut self,
        conversation: &str,
        sender: &str,
        body: &str,
        received: chrono::DateTime<chrono::Utc>,
        delayed: bool,
        stanza_id: Option<&str>,
        allow_outgoing_fuzzy: bool,
    ) -> Option<i64>
    {
        let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid).to_string());
        if let Some(pending) = self.pending_history_writes.iter_mut().find(|pending|
            pending.account == account && pending.conversation == conversation && pending.own_origin.is_none() &&
            // Display names are not sender authority. Apply the same outgoing
            // fallback policy to pending rows as to rows already in SQLite.
            (allow_outgoing_fuzzy || pending.outbox_id.is_none()) &&
            match (pending.stanza_id.as_deref(), stanza_id) {
                (Some(a), Some(b)) => a == b,
                (None, None) => delayed && pending.sender == sender && pending.body == body && pending.received.timestamp_millis() == received.timestamp_millis(),
                _ if delayed => pending.sender == sender && pending.body == body && pending.received.timestamp_millis() == received.timestamp_millis(),
                _ => false,
            }) {
            pending.allow_outgoing_fuzzy &= allow_outgoing_fuzzy;
            if delayed {
                // As with stored incoming rows, the first archived copy
                // replaces local receipt time; later replays retain that stamp.
                if !pending.delayed { pending.received = received; }
                pending.delayed = true;
                if pending.stanza_id.is_none() { pending.stanza_id = stanza_id.map(str::to_string); }
            }
            return None;
        }
        let receipt = uuid::Uuid::new_v4();
        if let Some(h) = &self.history
        {
            match h.store_received(receipt, conversation, sender, body, received, delayed, stanza_id, allow_outgoing_fuzzy)
            {
                Ok((stored, inserted)) => {
                    // A duplicate can still adopt an authoritative archive
                    // timestamp. Refresh any loaded copy without reloading the
                    // window or treating the replay as a new unread message.
                    if !inserted {
                        if let Some(backlog) = self.backlog_by_jid_mut(conversation) {
                            backlog.reconcile_stored(&stored, &[]);
                        }
                        self.remap_message_ui(conversation, stored.id, &stored.body, stored.received, []);
                    }
                    return inserted.then_some(stored.id);
                },
                Err(e) => warn!("Cannot save chat message in {}: {}", conversation, e),
            }
        }
        let temp_id = self.next_pending_id();
        self.pending_history_writes.push(crate::app::PendingHistoryWrite { receipt,
            account, conversation: conversation.into(), sender: sender.into(), body: body.into(),
            received, delayed, stanza_id: stanza_id.map(str::to_string), temp_id, outbox_id: None, own_origin: None, own_room_origin: false, allow_outgoing_fuzzy,
        });
        return Some(temp_id);
    }

    fn persist_own_direct_copy(&mut self, conversation: &str, sender: &str, body: &str,
        received: chrono::DateTime<chrono::Utc>, stanza_id: Option<&str>, origin: &str, delayed: bool) -> OwnDirectCopy
    {
        self.retry_unqueued_dms();
        if let Some(history) = &self.history {
            match store_own_direct_copy(history, conversation, body, origin, received, stanza_id, delayed) {
                Ok(Some((stored, merged))) => {
                    self.accept_unqueued_copy(origin, &stored);
                    if let Some(backlog) = self.backlog_by_jid_mut(conversation) { backlog.reconcile_stored(&stored, &merged); }
                    self.remap_message_ui(conversation, stored.id, &stored.body, stored.received, merged.iter().copied());
                    return OwnDirectCopy::Stored;
                }
                Ok(None) => return OwnDirectCopy::Unmatched,
                Err(error) => warn!("Cannot reconcile outgoing message copy: {}", error),
            }
        }
        let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid).to_string());
        if let Some(pending) = self.pending_history_writes.iter_mut().find(|pending| pending.account == account
            && pending.conversation == conversation && pending.body == body
            && pending.own_origin.as_deref() == Some(origin) && pending.stanza_id.as_deref() == stanza_id) {
            // Local receipt clocks are not authoritative. Match storage's
            // first-server-stamp rule, then keep the earliest archived copy;
            // a later live echo must never replace an archived timestamp.
            if delayed {
                pending.received = if pending.delayed { pending.received.min(received) } else { received };
                pending.delayed = true;
            }
            return OwnDirectCopy::Pending(pending.temp_id);
        }
        // A write failure need not duplicate the optimistic message: resolve
        // the exact local origin context with reads or an unqueued send record.
        let known_id = self.unqueued_dm_sends.iter().find(|send| send.account == account
            && send.conversation == conversation && send.body == body && send.origin_id == origin).map(|send| send.message_id)
            .or_else(|| self.history.as_ref().and_then(|history| history.unqueued_direct(origin).ok().flatten())
                .filter(|(conv, stored)| conv == conversation && stored.body == body).map(|(_, stored)| stored.id))
            .or_else(|| {
                let queued = self.history.as_ref()?.outgoing_by_origin(origin).ok()??;
                if queued.is_room || queued.conversation != conversation || queued.body != body { return None; }
                let chat = self.chats.iter().find(|chat| chat.jid == conversation)?;
                chat.messages.iter().chain(&chat.parked_messages)
                    .find_map(|message| match message {
                        room::message::Message::Chat { id, status: ChatStatus::Queued(outbox) | ChatStatus::Submitted(outbox) | ChatStatus::Failed(outbox), .. }
                            if *outbox == queued.id => Some(*id),
                        _ => None,
                    })
            });
        let temp_id = known_id.unwrap_or_else(|| self.next_pending_id());
        self.pending_history_writes.push(crate::app::PendingHistoryWrite { receipt: uuid::Uuid::new_v4(),
            account, conversation: conversation.into(), sender: sender.into(), body: body.into(), received,
            delayed, stanza_id: stanza_id.map(str::to_string), temp_id, outbox_id: None,
            own_origin: Some(origin.into()), own_room_origin: false, allow_outgoing_fuzzy: false,
        });
        OwnDirectCopy::Pending(temp_id)
    }

    // Retained writes belong to their original login even after switching or
    // disconnecting. Reopen those stores without swapping active account state
    // or promoting their row IDs into another account's visible conversation.
    fn retry_detached_history(&mut self, current: &Option<String>) -> bool
    {
        let owners: std::collections::BTreeSet<String> = self.pending_history_writes.iter().map(|pending| &pending.account)
            .chain(self.unqueued_dm_sends.iter().filter(|send| send.dirty).map(|send| &send.account))
            .chain(self.pending_outbox_deletes.iter().map(|(owner, _)| owner))
            .chain(self.pending_message_errors.iter().map(|error| &error.account))
            .chain(self.rejected_sends.iter().filter(|send| !send.saved).map(|send| &send.account))
            .filter(|owner| *owner != current).filter_map(|owner| owner.clone()).collect();
        let mut owners: Vec<_> = owners.into_iter().collect();
        if let Some(previous) = &self.detached_retry_after {
            let start = owners.partition_point(|owner| owner <= previous);
            owners.rotate_left(start);
        }
        if owners.is_empty() { self.detached_retry_after = None; }
        let mut remaining = HISTORY_RETRY_BATCH;
        let mut progress = false;
        // Bound failed opens too, and resume after the last examined account.
        for owner in owners.into_iter().take(HISTORY_RETRY_BATCH) {
            if remaining == 0 { break; }
            self.detached_retry_after = Some(owner.clone());
            let Some(history) = storage::open_history(&owner, self.saved_config.jid.as_deref()) else { continue; };
            let mut failed = false;
            // Persist exact outgoing origins before reconciling archived copies.
            for send in self.unqueued_dm_sends.iter_mut().filter(|send| send.account.as_deref() == Some(&owner) && send.dirty) {
                if remaining == 0 { break; }
                remaining -= 1;
                match history.store_unqueued_direct(&send.origin_id, &send.conversation, &send.sender, &send.body, send.received, send.state) {
                    Ok(stored) => {
                        send.message_id = stored.id;
                        if stored.delivery == storage::history::DeliveryState::UnqueuedSubmitted {
                            send.state = UnqueuedDelivery::Submitted; send.awaiting_result = false;
                        }
                        if stored.delivery == storage::history::DeliveryState::UnqueuedFailed {
                            send.state = UnqueuedDelivery::Failed; send.awaiting_result = false;
                        }
                        send.dirty = false;
                        progress = true;
                    }
                    Err(_) => { failed = true; break; }
                }
            }
            // Routing errors can still be saved when only the messages table
            // is unavailable. Never validate them against a newer login/JID.
            // Stop a blocked phase early so other recovery work gets a turn.
            let account = Some(owner.clone());
            let mut next_cursor = None;
            for index in self.rejection_retry_indices(&account) {
                if remaining == 0 { break; }
                remaining -= 1;
                let rejected = &mut self.rejected_sends[index];
                next_cursor = Some(rejected.token);
                if history.reject_outbox(rejected.outbox_id, &rejected.reason).is_ok() {
                    rejected.saved = true; progress = true;
                } else { break; }
            }
            if let Some(cursor) = next_cursor { self.rejected_retry_cursors.insert(account, cursor); }
            let mut recovered_errors = Vec::new();
            // One blocked report store must not consume the entire batch on
            // repeated failures. Other tables/accounts may still be writable.
            let mut report_failed = false;
            let mut retained_prefix = 0;
            self.pending_message_errors.retain(|error| {
                if remaining == 0 || report_failed { return true; }
                let keep = (|| {
                    if error.account.as_deref() != Some(&owner) { return true; }
                    // Unknown stored origin is inconclusive while its local context
                    // is still waiting to be written in this batch.
                    if self.unqueued_dm_sends.iter().any(|send| send.account == error.account && send.origin_id == error.origin_id && send.dirty) { return true; }
                    remaining -= 1;
                    match stored_error_target(&history, error) {
                        Ok(Some(MessageErrorTarget::Queued(message))) => {
                            if history.reject_outbox(message.id, &error.reason).is_err() { report_failed = true; return true; }
                            recovered_errors.push((message.id, message.conversation, message.body, error.reason.clone(), None));
                        }
                        Ok(Some(MessageErrorTarget::Unqueued(conversation, stored))) => {
                            let Ok(failed) = history.store_unqueued_direct(&error.origin_id, &conversation, &stored.sender, &stored.body,
                                stored.received, UnqueuedDelivery::Failed) else { report_failed = true; return true; };
                            recovered_errors.push((0, conversation, stored.body, error.reason.clone(), Some((error.origin_id.clone(), failed.id))));
                        }
                        Ok(None) => {},
                        Err(()) => { report_failed = true; return true; },
                    }
                    progress = true;
                    false
                })();
                retained_prefix += usize::from(keep);
                keep
            });
            if report_failed || remaining == 0 { self.pending_message_errors.rotate_left(retained_prefix); }
            for (mut id, conversation, body, reason, unqueued) in recovered_errors {
                if let Some((origin, stored_id)) = unqueued {
                    id = if let Some(send) = self.unqueued_dm_sends.iter_mut().find(|send| send.account.as_deref() == Some(&owner) && send.origin_id == origin) {
                        send.message_id = stored_id; send.state = UnqueuedDelivery::Failed;
                        send.dirty = false; send.awaiting_result = false;
                        send.command_id
                    } else { self.next_pending_id() };
                }
                if let Some(rejected) = self.rejected_sends.iter_mut().find(|send| send.account.as_deref() == Some(&owner) && send.outbox_id == id) {
                    rejected.reason = reason; rejected.saved = true;
                } else {
                    self.rejected_sends.push(crate::app::RejectedSend { token: uuid::Uuid::new_v4(), account: Some(owner.clone()),
                        outbox_id: id, conversation, body, reason, saved: true });
                }
            }
            if failed || self.unqueued_dm_sends.iter().any(|send| send.account.as_deref() == Some(&owner) && send.dirty) { continue; }
            self.pending_history_writes.retain(|pending| {
                if pending.account.as_deref() != Some(&owner) || remaining == 0 || failed { return true; }
                remaining -= 1;
                match store_pending_history(&history, pending) {
                    Ok(_) => { progress = true; false },
                    Err(_) => { failed = true; true },
                }
            });
            let account = Some(owner.clone());
            for id in self.outbox_delete_retry_ids(&account) {
                if remaining == 0 || failed { break; }
                remaining -= 1;
                self.outbox_delete_retry_after.insert(account.clone(), id);
                let key = (account.clone(), id);
                let discarded = self.discarded_outbox.contains(&key);
                // Discard is idempotent: its context can already be gone when
                // deletion committed before the UI observed success. Delivery
                // confirmation still needs its original durable send context.
                if !discarded && !matches!(history.has_outbox_context(id), Ok(true)) { break; }
                let result = if discarded { history.remove_outbox(id) }
                    else { history.confirm_outbox(id) };
                match result {
                    Ok(()) => { self.pending_outbox_deletes.remove(&key); self.discarded_outbox.remove(&key); progress = true; },
                    Err(_) => break,
                }
            }
        }
        self.unqueued_dm_sends.retain(|send| send.dirty || send.awaiting_result);
        self.prune_rejection_retry_cursors();
        self.prune_outbox_delete_retry_cursors();
        let more = self.pending_history_writes.iter().any(|pending| pending.account.is_some() && &pending.account != current)
            || self.unqueued_dm_sends.iter().any(|send| send.dirty && send.account.is_some() && &send.account != current)
            || self.pending_outbox_deletes.iter().any(|(owner, _)| owner.is_some() && owner != current)
            || self.pending_message_errors.iter().any(|error| error.account.is_some() && &error.account != current)
            || self.rejected_sends.iter().any(|send| !send.saved && send.account.is_some() && &send.account != current);
        if !more { self.detached_retry_after = None; }
        progress && more
    }

    fn retry_history_writes(&mut self) -> bool
    {
        let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid).to_string());
        if self.history.is_none() {
            if let Some(account) = account.as_deref() {
                self.history = storage::open_history(account, self.saved_config.jid.as_deref());
            }
        }
        let unqueued_more = self.retry_unqueued_dms();
        let detached_more = self.retry_detached_history(&account);
        let Some(h) = &self.history else { return detached_more; };
        let mut completed = Vec::new();
        let mut attempts = 0;
        let mut failed = false;
        self.pending_history_writes.retain(|pending| {
            if pending.account != account || attempts == HISTORY_RETRY_BATCH || failed { return true; }
            attempts += 1;
            match store_pending_history(h, pending) {
                Ok((stored, merged)) => {
                    completed.push((pending.conversation.clone(), pending.temp_id, stored, merged, pending.own_origin.clone()));
                    false
                }
                Err(_) => { failed = true; true },
            }
        });
        let continue_retry = !failed && !completed.is_empty()
            && self.pending_history_writes.iter().any(|pending| pending.account == account);
        let mut conversations = std::collections::HashSet::new();
        for (conversation, temp_id, stored, merged, origin) in completed {
            if let Some(origin) = origin { self.accept_unqueued_copy(&origin, &stored); }
            if let Some(b) = self.backlog_by_jid_mut(&conversation) {
                b.promote_message(temp_id, &stored);
                b.reconcile_stored(&stored, &merged);
            }
            self.remap_message_ui(&conversation, stored.id, &stored.body, stored.received, merged.iter().copied().chain([temp_id]));
            conversations.insert(conversation);
        }
        for conversation in conversations {
            if self.active_conv_jid().as_deref() == Some(&conversation)
                && self.active_backlog().is_some_and(|b| b.anchored_bottom()) {
                match self.active {
                    Some(Selection::Room(i)) => seed_tail_for(&self.history, &conversation, &mut self.rooms[i]),
                    Some(Selection::Chat(i)) => seed_tail_for(&self.history, &conversation, &mut self.chats[i]),
                    None => {},
                }
            }
            // Older-page requests can coexist with catch-up. Only unsaved
            // messages block recovery; start_catchup guards an existing sweep.
            if !self.pending_history_writes.iter().any(|pending| pending.account == account && pending.conversation == conversation) {
                self.start_catchup(&conversation);
            }
        }
        self.mark_active_read_if_visible();
        return continue_retry || detached_more || unqueued_more;
    }

    // Stable row IDs let retries resume beyond a failed item even after
    // successful rows disappear from the set. Keep each account independent.
    fn outbox_delete_retry_ids(&self, account: &Option<String>) -> Vec<i64>
    {
        let mut ids: Vec<_> = self.pending_outbox_deletes.iter()
            .filter(|(owner, _)| owner == account).map(|(_, id)| *id).collect();
        ids.sort_unstable();
        if let Some(after) = self.outbox_delete_retry_after.get(account) {
            let start = ids.partition_point(|id| id <= after);
            ids.rotate_left(start);
        }
        ids
    }

    fn prune_outbox_delete_retry_cursors(&mut self)
    {
        let pending = &self.pending_outbox_deletes;
        self.outbox_delete_retry_after.retain(|account, _| pending.iter().any(|(owner, _)| owner == account));
    }

    fn retry_outbox_deletes(&mut self) -> bool
    {
        let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid).to_string());
        let ids = self.outbox_delete_retry_ids(&account);
        let Some(h) = &self.history else { return false; };
        let mut completed = 0;
        let mut delivered = Vec::new();
        let mut failed = false;
        for id in ids.into_iter().take(HISTORY_RETRY_BATCH) {
            self.outbox_delete_retry_after.insert(account.clone(), id);
            let key = (account.clone(), id);
            let discarded = self.discarded_outbox.contains(&key);
            let result = if discarded { h.remove_outbox(id) }
                else { h.confirm_outbox(id) };
            match result {
                Ok(()) => { completed += 1; delivered.push((id, discarded)); self.pending_outbox_deletes.remove(&key); },
                Err(e) => { warn!("Cannot remove delivered outbox message {}: {}", id, e); failed = true; break; },
            }
        }
        for (id, discarded) in delivered {
            self.set_outgoing_delivery(id, if discarded { ChatStatus::Failed(id) } else { ChatStatus::Submitted(id) });
        }
        self.discarded_outbox.retain(|key| self.pending_outbox_deletes.contains(key));
        self.prune_outbox_delete_retry_cursors();
        completed > 0 && !failed && self.pending_outbox_deletes.iter().any(|(owner, _)| *owner == account)
    }

    fn accept_unqueued_copy(&mut self, origin: &str, stored: &storage::history::StoredMessage)
    {
        let state = match stored.delivery {
            storage::history::DeliveryState::UnqueuedSubmitted => UnqueuedDelivery::Submitted,
            storage::history::DeliveryState::UnqueuedFailed => UnqueuedDelivery::Failed,
            _ => return,
        };
        let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid).to_string());
        for send in self.unqueued_dm_sends.iter_mut().filter(|send| send.account == account && send.origin_id == origin) {
            send.state = state; send.dirty = false; send.awaiting_result = false;
        }
    }

    fn retry_unqueued_dms(&mut self) -> bool
    {
        let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid).to_string());
        let mut completed = Vec::new();
        let mut failed = false;
        if let Some(history) = &self.history {
            for send in self.unqueued_dm_sends.iter_mut().filter(|send| send.account == account && send.dirty).take(HISTORY_RETRY_BATCH) {
                match history.store_unqueued_direct(&send.origin_id, &send.conversation, &send.sender, &send.body, send.received, send.state) {
                    Ok(stored) => {
                        completed.push((send.conversation.clone(), send.message_id, stored.clone()));
                        send.message_id = stored.id;
                        if stored.delivery == storage::history::DeliveryState::UnqueuedSubmitted {
                            send.state = UnqueuedDelivery::Submitted;
                            send.awaiting_result = false;
                        }
                        if stored.delivery == storage::history::DeliveryState::UnqueuedFailed {
                            send.state = UnqueuedDelivery::Failed;
                            send.awaiting_result = false;
                        }
                        send.dirty = false;
                    }
                    Err(_) => { failed = true; break; },
                }
            }
        }
        // A successful bounded batch must resume promptly while work remains.
        // Failed stores keep the normal retry timer rather than spinning.
        let more = !failed && !completed.is_empty()
            && self.unqueued_dm_sends.iter().any(|send| send.account == account && send.dirty);
        for (conversation, old_id, stored) in completed {
            if let Some(backlog) = self.backlog_by_jid_mut(&conversation) {
                if old_id != stored.id { backlog.promote_message(old_id, &stored); }
                else {
                    for message in backlog.messages_mut() {
                        if let room::message::Message::Chat { id, status, .. } = message {
                            if *id == stored.id { *status = stored.delivery.into(); }
                        }
                    }
                }
            }
            self.remap_message_ui(&conversation, stored.id, &stored.body, stored.received, [old_id]);
        }
        self.unqueued_dm_sends.retain(|send| send.dirty || send.awaiting_result);
        more
    }

    fn end_unqueued_attempts(&mut self)
    {
        for send in &mut self.unqueued_dm_sends { send.awaiting_result = false; }
    }

    fn set_outgoing_delivery(&mut self, outbox_id: i64, delivery: ChatStatus)
    {
        for backlog in self.chats.iter_mut().map(|chat| chat as &mut dyn Backlog)
            .chain(self.rooms.iter_mut().map(|room| room as &mut dyn Backlog)) {
            let update = |messages: &mut Vec<room::message::Message>| {
                for message in messages {
                    if let room::message::Message::Chat { status, .. } = message {
                        if matches!(*status, ChatStatus::Queued(id) | ChatStatus::Submitted(id) | ChatStatus::Failed(id) if id == outbox_id)
                            && !(matches!(delivery, ChatStatus::Submitted(_)) && matches!(status, ChatStatus::Failed(_))) {
                            *status = delivery;
                        }
                    }
                }
            };
            update(backlog.messages_mut());
            update(backlog.parked_messages_mut());
        }
    }

    fn record_rejected_send(&mut self, id: i64, conversation: String, body: String, reason: String, saved: bool)
    {
        if id > 0 { self.set_outgoing_delivery(id, ChatStatus::Failed(id)); }
        let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid).to_string());
        if let Some(send) = self.rejected_sends.iter_mut().find(|send| id != 0 && send.account == account && send.outbox_id == id) {
            if !saved || send.saved { send.reason = reason; send.saved = saved; }
        } else {
            self.rejected_sends.push(crate::app::RejectedSend { token: uuid::Uuid::new_v4(), account,
                outbox_id: id, conversation, body, reason, saved: saved || id <= 0 });
        }
    }

    fn rejection_retry_indices(&self, account: &Option<String>) -> Vec<usize>
    {
        let mut indices: Vec<_> = self.rejected_sends.iter().enumerate()
            .filter(|(_, send)| &send.account == account && !send.saved).map(|(index, _)| index).collect();
        let cursor = self.rejected_retry_cursors.get(account)
            .and_then(|token| self.rejected_sends.iter().position(|send| send.token == *token))
            .map_or(0, |index| index + 1);
        let start = indices.partition_point(|index| *index < cursor);
        indices.rotate_left(start);
        indices
    }

    fn prune_rejection_retry_cursors(&mut self)
    {
        let pending: std::collections::HashSet<_> = self.rejected_sends.iter().filter(|send| !send.saved)
            .map(|send| send.account.as_ref()).collect();
        self.rejected_retry_cursors.retain(|account, _| pending.contains(&account.as_ref()));
    }

    // Bound report lookups and rejection writes separately so neither phase
    // starves the other. Continue promptly only when a pass made progress.
    fn retry_send_rejections(&mut self) -> bool
    {
        let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid).to_string());
        let mut rejected = Vec::new();
        let mut unqueued_rejected = Vec::new();
        let pending_before = self.pending_message_errors.len();
        let mut reports = 0;
        let mut retained_prefix = 0;
        self.pending_message_errors.retain(|error| {
            if reports == HISTORY_RETRY_BATCH { return true; }
            let keep = (|| {
                if error.account != account { return true; }
                reports += 1;
                let memory = self.unqueued_dm_sends.iter().find(|send| send.account == account && send.origin_id == error.origin_id);
                let unqueued = if let Some(send) = memory {
                    if send.state == UnqueuedDelivery::Failed { return false; }
                    Some((send.conversation.clone(), storage::history::StoredMessage {
                        id: send.message_id, sender: send.sender.clone(), body: send.body.clone(), received: send.received,
                        delivery: storage::history::DeliveryState::Unconfirmed,
                    }))
                } else if let Some(history) = &self.history {
                    match stored_error_target(history, error) {
                        Ok(Some(MessageErrorTarget::Queued(message))) => { rejected.push((message, error.reason.clone())); return false; }
                        Ok(Some(MessageErrorTarget::Unqueued(conversation, stored))) => Some((conversation, stored)),
                        Ok(None) => return false,
                        Err(()) => return true,
                    }
                } else { return true; };
                if let Some((conversation, stored)) = unqueued {
                    let context = storage::history::OutboxItem { id: 0, origin_id: error.origin_id.clone(),
                        conversation: conversation.clone(), is_room: false, body: stored.body.clone() };
                    if stored.delivery != storage::history::DeliveryState::UnqueuedFailed && context.accepts_error_from(error.from.as_deref(), &error.own_jid) {
                        unqueued_rejected.push((error.origin_id.clone(), conversation, stored, error.reason.clone()));
                    }
                    return false;
                }
                false
            })();
            retained_prefix += usize::from(keep);
            keep
        });
        if reports == HISTORY_RETRY_BATCH {
            // Failed reports go behind unattempted work. A corrupt/unavailable
            // origin must not permanently block later independent reports.
            self.pending_message_errors.rotate_left(retained_prefix);
        }
        let mut progress = self.pending_message_errors.len() < pending_before;
        for (origin_id, conversation, stored, reason) in unqueued_rejected {
            let command_id = if let Some(send) = self.unqueued_dm_sends.iter_mut().find(|send| send.account == account && send.origin_id == origin_id) {
                send.state = UnqueuedDelivery::Failed; send.dirty = true; send.awaiting_result = false;
                send.command_id
            } else {
                let command_id = self.next_pending_id();
                self.unqueued_dm_sends.push(crate::app::UnqueuedDmSend {
                    account: account.clone(), command_id, origin_id, conversation: conversation.clone(), sender: stored.sender,
                    body: stored.body.clone(), received: stored.received, message_id: stored.id,
                    state: UnqueuedDelivery::Failed, dirty: true, awaiting_result: false,
                });
                command_id
            };
            self.record_rejected_send(command_id, conversation, stored.body, reason, true);
        }
        for (message, reason) in rejected {
            self.record_rejected_send(message.id, message.conversation, message.body, reason, false);
        }
        if let Some(history) = &self.history {
            let indices = self.rejection_retry_indices(&account);
            let mut next_cursor = None;
            for index in indices.into_iter().take(HISTORY_RETRY_BATCH) {
                let send = &mut self.rejected_sends[index];
                next_cursor = Some(send.token);
                if history.reject_outbox(send.outbox_id, &send.reason).is_ok() { send.saved = true; progress = true; }
            }
            if let Some(cursor) = next_cursor { self.rejected_retry_cursors.insert(account.clone(), cursor); }
        }
        self.prune_rejection_retry_cursors();
        progress && (self.pending_message_errors.iter().any(|error| error.account == account)
            || self.rejected_sends.iter().any(|send| send.account == account && !send.saved))
    }

    // Retain overflow on the UI side rather than losing commands when the
    // bounded worker channel is full. One retry subscription drains it in FIFO
    // order; dropping a session clears it without retaining sender clones.
    fn send_xmpp_command(&mut self, mut command: xmpp::XmppCommand) -> bool
    {
        if self.session_is_draining() { return false; }
        if self.xmpp_cmd_tx.is_none() { return false; }
        match &mut command {
            xmpp::XmppCommand::JoinRoom(room) | xmpp::XmppCommand::LeaveRoom { room, .. } => {
                if let Some(key) = self.tracked_room_key(room) { *room = key.to_string(); }
            }
            _ => {},
        }
        match &command {
            xmpp::XmppCommand::SendRoomMessage { room, .. } if !self.room_is_joined(room) => return false,
            xmpp::XmppCommand::JoinRoom(room) => {
                if ::xmpp::validate_room_jid(room).is_err() { return false; }
                self.room_connections.insert(room.clone(), RoomConnection::Joining);
                self.outbox_replay_pending.remove(&Some(room.clone()));
            }
            _ => {},
        }
        // Repeated join/recovery events can replay the outbox before the first
        // send's result reaches the UI. A durable row gets one attempt at a time,
        // including while its command is waiting for worker queue capacity.
        let outbox_id = match &command {
            xmpp::XmppCommand::SendRoomMessage { id, .. }
                | xmpp::XmppCommand::SendDirectMessage { id, .. } if *id > 0 => Some(*id),
            _ => None,
        };
        if let Some(id) = outbox_id {
            let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid).to_string());
            if self.rejected_sends.iter().any(|send| send.account == account && send.outbox_id == id) { return true; }
            if self.pending_outbox_deletes.contains(&(account, id)) { return true; }
            if !self.outbox_in_flight.insert(id) { return true; }
        }
        let room = match &command {
            xmpp::XmppCommand::JoinRoom(room) | xmpp::XmppCommand::LeaveRoom { room, .. } => Some((room.clone(), true)),
            xmpp::XmppCommand::SendRoomMessage { room, .. } => Some((room.clone(), false)),
            _ => None,
        };
        let operation = room.and_then(|(room, replace)| {
            if replace {
                if let Some(previous) = self.room_operations.remove(&room) { previous.cancel(); }
                self.room_operations.insert(room.clone(), xmpp::RoomOperation::new());
            }
            self.room_operations.get(&room).cloned()
        });
        let command = if let Some(operation) = operation { xmpp::XmppCommand::ScopedRoom { operation, command: Box::new(command) } } else { command };
        self.xmpp_pending_commands.push_back(command);
        self.flush_xmpp_commands();
        return self.xmpp_cmd_tx.is_some();
    }

    fn flush_xmpp_commands(&mut self)
    {
        let Some(tx) = self.xmpp_cmd_tx.clone() else { return; };
        while let Some(command) = self.xmpp_pending_commands.pop_front()
        {
            if let xmpp::XmppCommand::SendRoomMessage { id, room, .. } = command.inner() {
                if !command.is_current() || !self.room_is_joined(room) {
                    self.outbox_in_flight.remove(id);
                    if *id <= 0 {
                        if let xmpp::XmppCommand::SendRoomMessage { room, body, .. } = command.inner() {
                            self.record_rejected_send(0, room.clone(), body.clone(), "Room entry is not confirmed. Copy this message to send it again after joining.".into(), true);
                        }
                    }
                    continue;
                }
            }
            if !command.should_execute() { continue; }
            let durable_send = match command.inner() {
                xmpp::XmppCommand::SendRoomMessage { id, .. } | xmpp::XmppCommand::SendDirectMessage { id, .. } if *id > 0 => Some(*id),
                _ => None,
            };
            match tx.try_send(command)
            {
                Ok(()) => if let Some(id) = durable_send { self.mark_outbox_attempted(id); },
                Err(tokio::sync::mpsc::error::TrySendError::Full(command)) => {
                    self.xmpp_pending_commands.push_front(command);
                    return;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(command)) => {
                    self.xmpp_pending_commands.push_front(command);
                    self.xmpp_cmd_tx = None;
                    self.end_unqueued_attempts();
                    self.cancel_room_operations();
                    for state in self.room_connections.values_mut() { *state = RoomConnection::Failed("Connection closed before room entry was confirmed.".into()); }
                    self.outbox_in_flight.clear();
                    self.outbox_replay_pending.clear();
                    self.send_error = None;
                    while let Some(command) = self.xmpp_pending_commands.pop_front() {
                        match command.into_inner() {
                            xmpp::XmppCommand::QueryArchive { query_id, .. } => {
                                if let Some(ArchiveContext { conversation: conv, .. }) = self.mam_pending.remove(&query_id) {
                                    self.release_failed_archive_query(&query_id, &conv);
                                }
                            }
                            xmpp::XmppCommand::JoinRoom(room) => {
                                if self.joining_room.as_deref() == Some(&room) {
                                    self.joining_room = None;
                                    self.join_error = Some("Connection closed before joining the room".into());
                                }
                            }
                            _ => {}, // Sends remain in the persistent outbox.
                        }
                    }
                    warn!("XMPP command channel closed before queued commands could be delivered");
                    return;
                }
            }
        }
    }

    // Saved lists from older versions may contain aliases of one room. Queue
    // one join per resolved key so later aliases cannot cancel that operation.
    fn join_rooms_once(&mut self, rooms: impl IntoIterator<Item = String>)
    {
        let mut attempted: Vec<String> = Vec::new();
        for jid in rooms {
            let key = self.tracked_room_key(&jid).unwrap_or(&jid).to_string();
            if !attempted.iter().any(|previous| previous == &key || ::xmpp::jid_matches(previous, &key)) {
                attempted.push(key.clone());
                self.send_xmpp_command(xmpp::XmppCommand::JoinRoom(key));
            }
        }
    }

    // Membership and pending departures own their key, even when the room is
    // closed in the UI. Reuse that key before consulting saved preferences.
    fn tracked_room_key(&self, jid: &str) -> Option<&str>
    {
        let tracked = || self.rooms.iter().map(|room| room.jid.as_str())
            .chain(self.room_operations.keys().map(String::as_str))
            .chain(self.room_connections.keys().map(String::as_str));
        tracked().find(|key| *key == jid)
            .or_else(|| tracked().find(|key| ::xmpp::jid_matches(key, jid)))
            .or_else(|| self.saved_config.rooms.iter().find(|key| key.as_str() == jid).map(String::as_str))
            .or_else(|| self.saved_config.rooms.iter().find(|key| ::xmpp::jid_matches(key, jid)).map(String::as_str))
    }

    fn room_is_joined(&self, room: &str) -> bool
    {
        matches!(self.room_connections.get(room), Some(RoomConnection::Joined))
    }

    fn cancel_room_operations(&mut self)
    {
        for (_, operation) in self.room_operations.drain() { operation.cancel(); }
    }

    fn room_join_failed(&mut self, room: String, reason: String) -> Task<Message>
    {
        let reason: String = reason.chars().take(1024).collect();
        let manual = self.joining_room.as_deref() == Some(&room);
        if !manual && !self.room_connections.contains_key(&room) { return Task::none(); }
        self.room_connections.insert(room.clone(), RoomConnection::Failed(reason.clone()));
        self.outbox_replay_pending.remove(&Some(room.clone()));
        let mut removed = Vec::new();
        self.xmpp_pending_commands.retain(|command| match command.inner() {
            xmpp::XmppCommand::SendRoomMessage { id, room: target, body, .. } if target == &room => { removed.push((*id, body.clone())); false }
            xmpp::XmppCommand::JoinRoom(target) if target == &room => false,
            _ => true,
        });
        for (id, body) in removed {
            self.outbox_in_flight.remove(&id);
            if id <= 0 { self.record_rejected_send(0, room.clone(), body, reason.clone(), true); }
        }
        if manual {
            self.joining_room = None;
            self.join_error = Some(reason);
            return focus_join_input();
        }
        Task::none()
    }

    // Persist an outgoing message and its wire identity for delivery retries.
    // None means persistence failed (degraded, best-effort send only).
    fn enqueue_outbox(&self, conversation: &str, is_room: bool, body: &str) -> Option<storage::history::OutboxItem>
    {
        return self.history.as_ref().and_then(|h| h.enqueue_outbox_message(conversation, is_room, body));
    }

    // Re-send the queued messages matching `keep` (a room just (re)joined, or all
    // DMs on reconnect). Each carries its outbox id, so its SendResult drops it
    // from the queue after server confirmation; failed ones stay queued.
    fn resend_outbox(&mut self, keep: impl Fn(&storage::history::OutboxItem) -> bool) -> bool
    {
        self.retry_outbox_deletes();
        let (Some(_), Some(h)) = (self.xmpp_cmd_tx.as_ref(), self.history.as_ref()) else
        {
            return false;
        };
        let items = match h.load_outbox() {
            Ok(items) => items,
            Err(e) => { warn!("Cannot read queued outgoing messages: {}", e); return false; }
        };
        let rejected = match h.load_rejected_outbox() {
            Ok(rejected) => rejected,
            Err(e) => { warn!("Cannot load rejected outgoing messages: {}", e); return false; }
        };
        let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid).to_string());
        for (item, reason) in rejected {
            if !self.discarded_outbox.contains(&(account.clone(), item.id)) {
                self.record_rejected_send(item.id, item.conversation, item.body, reason, true);
            }
        }
        let mut conversations: Vec<(String, bool)> = Vec::new();
        for item in items.into_iter().filter(|i| keep(i)) {
            if item.is_room && !self.room_is_joined(&item.conversation) { continue; }
            // Same exclusions as dispatch: confirmed, rejected or in-flight rows
            // are not replayed and need no archive lookup.
            if self.outbox_in_flight.contains(&item.id)
                || self.pending_outbox_deletes.contains(&(account.clone(), item.id))
                || self.rejected_sends.iter().any(|send| send.account == account && send.outbox_id == item.id) { continue; }
            if !conversations.iter().any(|(conv, is_room)| *conv == item.conversation && *is_room == item.is_room) {
                conversations.push((item.conversation, item.is_room));
            }
        }
        for (conv, is_room) in conversations { self.start_delivery_check(&conv, is_room); }
        self.xmpp_cmd_tx.is_some()
    }

    // Look the conversation's queued sends up in the archive before replaying
    // them. A send whose acknowledgement was lost (the link died after the
    // server received it) shows up there with our origin ID and is retired;
    // only the rest is sent again. Without an archive, replay proceeds.
    fn start_delivery_check(&mut self, conv: &str, is_room: bool)
    {
        if self.mam_pending.iter().any(|(id, context)| id.starts_with("mam-d-") && context.conversation == conv) { return; }
        let Some(history) = self.history.as_ref() else { return; };
        // Sends that never reached the connection cannot be in the archive.
        let attempted = match history.attempted_queued(conv, is_room) {
            Ok(ids) => !ids.is_empty(),
            Err(error) => { warn!("Cannot read queued sends for {}: {}", conv, error); true },
        };
        let attempted = attempted || history.load_outbox().is_ok_and(|items| items.iter()
            .any(|item| item.conversation == conv && item.is_room == is_room && self.attempted_outbox.contains(&item.id)));
        if !attempted { self.resend_checked_outbox(conv, is_room); return; }
        let start = match history.oldest_queued(conv, is_room) {
            Ok(Some(oldest)) => (oldest - DELIVERY_CHECK_SKEW).to_rfc3339(),
            Ok(None) => return,
            Err(error) => { warn!("Cannot read queued sends for {}: {}", conv, error); self.resend_checked_outbox(conv, is_room); return; },
        };
        self.delivery_checks.insert(conv.to_string(), crate::app::DeliveryCheck { is_room, start, pages: 0 });
        self.send_delivery_check_page(conv, None);
    }

    fn send_delivery_check_page(&mut self, conv: &str, cursor: Option<String>)
    {
        let Some(check) = self.delivery_checks.get_mut(conv) else { return; };
        check.pages += 1;
        let (is_room, start) = (check.is_room, check.start.clone());
        let (to, with) = if is_room { (Some(conv.to_string()), None) } else { (None, Some(conv.to_string())) };
        let query_id = format!("mam-d-{}", self.mam_seq);
        self.mam_seq = self.mam_seq.wrapping_add(1);
        self.mam_pending.insert(query_id.clone(), ArchiveContext { conversation: conv.to_string(), is_muc: is_room });
        if !self.send_xmpp_command(xmpp::XmppCommand::QueryArchive {
            query_id: query_id.clone(), to, with, start: Some(start), end: None,
            forward: true, cursor, max: backlog::PAGE_SIZE as u32,
        }) {
            self.mam_pending.remove(&query_id);
            self.finish_delivery_check(conv);
        }
    }

    fn finish_delivery_check(&mut self, conv: &str)
    {
        let Some(check) = self.delivery_checks.remove(conv) else { return; };
        self.resend_checked_outbox(conv, check.is_room);
    }

    // The worker may now deliver this send; a later replay must check first.
    fn mark_outbox_attempted(&mut self, id: i64)
    {
        if self.attempted_outbox.contains(&id) { return; }
        let marked = self.history.as_ref().is_some_and(|history| history.mark_outbox_attempted(id).is_ok());
        if !marked { self.attempted_outbox.insert(id); }
    }

    // Send what is still queued for one conversation after its delivery check.
    fn resend_checked_outbox(&mut self, conv: &str, is_room: bool)
    {
        let Some(h) = self.history.as_ref() else { return; };
        let items = match h.load_outbox() {
            Ok(items) => items,
            Err(e) => {
                warn!("Cannot read queued outgoing messages: {}", e);
                self.request_outbox_replay(if is_room { Some(conv.to_string()) } else { None });
                return;
            }
        };
        for item in items.into_iter().filter(|item| item.conversation == conv && item.is_room == is_room)
        {
            let cmd = if item.is_room
            {
                xmpp::XmppCommand::SendRoomMessage { id: item.id, origin_id: Some(item.origin_id), room: item.conversation, body: item.body }
            }
            else
            {
                xmpp::XmppCommand::SendDirectMessage { id: item.id, origin_id: Some(item.origin_id), to: item.conversation, body: item.body }
            };
            self.send_xmpp_command(cmd);
        }
    }

    // An archived room copy carrying the origin of a still-queued send with
    // its exact text proves the room delivered that send.
    fn retire_queued_room_copy(&mut self, room: &str, nick: &str, body: &str, origin: &str,
        received: chrono::DateTime<chrono::Utc>, stanza_id: Option<&str>) -> bool
    {
        // Service text from the bare room address is not an occupant's send.
        if nick.is_empty() { return false; }
        let Some(history) = &self.history else { return false; };
        if !matches!(history.has_queued_room_origin(room, body, origin), Ok(true)) { return false; }
        let (stored, merged) = match store_own_room_copy(history, room, nick, body, origin, received, stanza_id) {
            Ok(Some((stored, merged, _))) => (stored, merged),
            Ok(None) => return false,
            Err(error) => { warn!("Cannot reconcile archived room copy: {}", error); return false; },
        };
        if let Some(idx) = self.rooms.iter().position(|known| known.jid == room) {
            self.confirm_pending(idx, nick, body, stored.id, stored.received, Some(origin), false);
        }
        if let Some(backlog) = self.backlog_by_jid_mut(room) { backlog.reconcile_stored(&stored, &merged); }
        self.remap_message_ui(room, stored.id, &stored.body, stored.received, merged.iter().copied());
        true
    }

    fn request_outbox_replay(&mut self, room: Option<String>)
    {
        if self.reconnecting || self.xmpp_cmd_tx.is_none() { return; }
        self.outbox_replay_pending.insert(room);
        self.retry_outbox_replay();
    }

    fn retry_outbox_replay(&mut self)
    {
        if self.outbox_replay_pending.is_empty() || self.reconnecting || self.xmpp_cmd_tx.is_none() { return; }
        // A send whose acknowledgement was lost may already be in the room.
        // Hold a rejoined room's replay until its join history has arrived, so
        // that copy can retire the queued row instead of a second delivery.
        let now = std::time::Instant::now();
        self.room_history_pending.retain(|_, deadline| *deadline > now);
        let targets: std::collections::HashSet<_> = self.outbox_replay_pending.iter()
            .filter(|target| target.as_ref().is_none_or(|room| !self.room_history_pending.contains_key(room)))
            .cloned().collect();
        if targets.is_empty() { return; }
        if self.resend_outbox(|item| targets.contains(&if item.is_room { Some(item.conversation.clone()) } else { None })) {
            self.outbox_replay_pending.retain(|target| !targets.contains(target));
        }
    }

    fn active_conv_jid(&self) -> Option<String>
    {
        return match self.active
        {
            Some(Selection::Room(i)) => self.rooms.get(i).map(|r| r.jid.clone()),
            Some(Selection::Chat(i)) => self.chats.get(i).map(|c| c.jid.clone()),
            None => None,
        };
    }

    fn mark_active_read_if_visible(&mut self)
    {
        if !self.window_focused || self.join_panel_visible() { return; }
        match self.active {
            Some(Selection::Room(index)) => {
                if let Some(room) = self.rooms.get_mut(index) {
                    if room.at_live_tail && room.anchored_bottom { room.unread = false; }
                }
            }
            Some(Selection::Chat(index)) => {
                if let Some(chat) = self.chats.get_mut(index) {
                    if chat.at_live_tail && chat.anchored_bottom { chat.unread = false; }
                }
            }
            None => {},
        }
    }

    fn active_backlog(&self) -> Option<&dyn Backlog>
    {
        return match self.active
        {
            Some(Selection::Room(i)) => self.rooms.get(i).map(|r| r as &dyn Backlog),
            Some(Selection::Chat(i)) => self.chats.get(i).map(|c| c as &dyn Backlog),
            None => None,
        };
    }

    fn active_backlog_mut(&mut self) -> Option<&mut dyn Backlog>
    {
        return match self.active
        {
            Some(Selection::Room(i)) => self.rooms.get_mut(i).map(|r| r as &mut dyn Backlog),
            Some(Selection::Chat(i)) => self.chats.get_mut(i).map(|c| c as &mut dyn Backlog),
            None => None,
        };
    }

    // Page the next older chunk of the active conversation in at the front.
    // Returns whether anything was loaded.
    fn page_older_active(&mut self) -> rusqlite::Result<bool>
    {
        let Some(jid) = self.active_conv_jid() else { return Ok(false); };
        let Some(b) = self.active_backlog() else { return Ok(false); };

        if !b.has_older() || b.messages().len() >= backlog::MAX_LOADED
        {
            return Ok(false);
        }
        let cursor = b.local_page_cursor();
        let loaded = b.loaded_ids();
        let Some(h) = &self.history else { return Ok(false); };

        // Search windows need not align with PAGE_SIZE. A final full page
        // could otherwise push the rendered history beyond its hard limit.
        let limit = backlog::PAGE_SIZE.min(backlog::MAX_LOADED - b.messages().len());
        let older = h.load_unloaded_older(&jid, cursor, &loaded, limit)?;
        if older.is_empty()
        {
            if let Some(b) = self.active_backlog_mut() { b.set_local_cursor(cursor); b.set_has_older(false); }
            return Ok(false);
        }
        let new_has_older = older.first()
            .map(|m| h.has_unloaded_older(&jid, Some((m.received.timestamp_millis(), m.id)), &loaded)).transpose()?.unwrap_or(false);

        if let Some(b) = self.active_backlog_mut()
        {
            b.prepend_older(older, new_has_older);
        }
        return Ok(true);
    }

    // Account resources identify devices; room resources identify different
    // people. Known room context also covers legacy PMs without a muc#user tag.
    fn direct_peer_key(&self, jid: &str, private_room: bool) -> Option<String>
    {
        ::xmpp::validate_message(jid, "").ok()?;
        let (bare, resource) = jid.split_once('/').map_or((jid, None), |(bare, resource)| (bare, Some(resource)));
        let room = self.tracked_room_key(bare).or_else(|| self.chats.iter().filter_map(|chat| chat.jid.split_once('/').map(|(room, _)| room))
            .find(|room| ::xmpp::jid_matches(room, bare)));
        if private_room || (resource.is_some() && room.is_some()) {
            Some(format!("{}/{}", room.unwrap_or(bare), resource?))
        } else { Some(bare.to_string()) }
    }

    // Add a direct conversation to the sidebar, seeded from history, and
    // remember it as open so it is restored after a restart.
    fn push_direct_chat(&mut self, peer: String) -> usize
    {
        let title = storage::history::direct_chat_title(&peer);
        if let Some(history) = &self.history {
            if let Err(error) = history.record_direct_chat(&peer, &title, true) {
                warn!("Could not record direct conversation: {error}");
            }
        }
        let conv = peer.clone();
        self.chats.push(room::chat::Chat
        {
            jid: peer,
            title,
            messages: Vec::new(),
            parked_messages: Vec::new(),
            unread: false,
            read_marker: None,
            read_boundary: None,
            oldest_loaded_id: None, local_cursor: None,
            has_older: false,
            at_live_tail: true,
            anchored_bottom: true,
            mam_in_flight: false,
            mam_exhausted: false,
            mam_cursor: None,
        });
        let i = self.chats.len() - 1;
        seed_tail_for(&self.history, &conv, &mut self.chats[i]);
        self.refresh_earlier_chats();
        i
    }

    // Closed direct conversations for the sidebar's Earlier list.
    fn refresh_earlier_chats(&mut self)
    {
        let Some(history) = &self.history else { self.earlier_chats.clear(); return; };
        match history.direct_chats() {
            Ok(chats) => {
                self.earlier_chats = chats.into_iter()
                    .filter(|chat| !self.chats.iter().any(|open| ::xmpp::jid_matches(&open.jid, &chat.conversation)))
                    .collect();
            }
            Err(error) => warn!("Could not list direct conversations: {error}"),
        }
    }

    // After login, list existing direct conversations and reopen the ones
    // that were open when Snack last ran.
    fn restore_direct_chats(&mut self, account: &str)
    {
        let Some(history) = &self.history else { return; };
        let own_name = account.split('@').next().unwrap_or(account);
        let rooms: std::collections::HashSet<String> = self.saved_config.rooms.iter().cloned().collect();
        if let Err(error) = history.backfill_direct_chats(own_name, &rooms) {
            warn!("Could not list earlier direct conversations: {error}");
        }
        let open: Vec<String> = history.direct_chats().unwrap_or_default().into_iter()
            .filter(|chat| chat.open).map(|chat| chat.conversation).collect();
        for peer in open {
            if self.direct_chat_index(&peer).is_none() { self.push_direct_chat(peer); }
        }
        self.refresh_earlier_chats();
    }

    // Prefer an exact stored key when older sessions created equivalent aliases.
    fn direct_chat_index(&self, peer: &str) -> Option<usize>
    {
        self.chats.iter().position(|chat| chat.jid == peer)
            .or_else(|| self.chats.iter().position(|chat| ::xmpp::jid_matches(&chat.jid, peer)))
    }

    fn backlog_by_jid(&self, jid: &str) -> Option<&dyn Backlog>
    {
        if let Some(r) = self.rooms.iter().find(|r| r.jid == jid) { return Some(r as &dyn Backlog); }
        if let Some(c) = self.chats.iter().find(|c| c.jid == jid) { return Some(c as &dyn Backlog); }
        return None;
    }

    fn backlog_by_jid_mut(&mut self, jid: &str) -> Option<&mut dyn Backlog>
    {
        if let Some(i) = self.rooms.iter().position(|r| r.jid == jid)
        {
            return Some(&mut self.rooms[i] as &mut dyn Backlog);
        }
        if let Some(i) = self.chats.iter().position(|c| c.jid == jid)
        {
            return Some(&mut self.chats[i] as &mut dyn Backlog);
        }
        return None;
    }

    // Load the next older page from the local DB into the named conversation's
    // window and prepend it. Returns how many messages were loaded.
    fn load_older_into(&mut self, jid: &str) -> rusqlite::Result<usize>
    {
        let backlog = self.backlog_by_jid(jid);
        let cursor = backlog.and_then(|b| b.local_page_cursor());
        let loaded = backlog.map_or_else(Vec::new, |b| b.loaded_ids());
        let available = backlog.map_or(0, |b| backlog::MAX_LOADED.saturating_sub(b.messages().len()));
        let empty = backlog.is_none_or(|b| b.messages().is_empty());
        let Some(h) = &self.history else { return Err(rusqlite::Error::InvalidQuery); };
        if available == 0 {
            let has_older = h.has_unloaded_older(jid, cursor, &loaded)?;
            if let Some(b) = self.backlog_by_jid_mut(jid) { b.set_local_cursor(cursor); b.set_has_older(has_older); }
            return Ok(0);
        }

        let limit = if cursor.is_some() { backlog::PAGE_SIZE } else { backlog::LIVE_TAIL_CAP };
        let older = h.load_unloaded_older(jid, cursor, &loaded, limit.min(available))?;
        let n = older.len();
        if n == 0 {
            if let Some(b) = self.backlog_by_jid_mut(jid) { b.set_local_cursor(cursor); b.set_has_older(false); }
            return Ok(0);
        }
        let new_has_older = older.first()
            .map(|m| h.has_unloaded_older(jid, Some((m.received.timestamp_millis(), m.id)), &loaded)).transpose()?.unwrap_or(false);

        if let Some(b) = self.backlog_by_jid_mut(jid)
        {
            if cursor.is_some() || !empty { b.prepend_older(older, new_has_older); }
            else { b.seed_tail(older, new_has_older); }
        }
        return Ok(n);
    }

    // The caller has removed this query from mam_pending. Release only its
    // role; another request for the same conversation can still be active.
    fn release_failed_archive_query(&mut self, query_id: &str, conversation: &str)
    {
        // Delivery checks own no paging state; they only gate a replay.
        if query_id.starts_with("mam-d-") { return; }
        let catchup_failed = query_id.starts_with("mam-c-");
        let mut catchup_pending = false;
        let mut backward_pending = false;
        for (id, context) in &self.mam_pending {
            if context.conversation != conversation || id.starts_with("mam-d-") { continue; }
            if id.starts_with("mam-c-") { catchup_pending = true; }
            else { backward_pending = true; }
        }
        if catchup_failed {
            if !catchup_pending { self.mam_catchup.remove(conversation); }
            self.mam_caught_up.remove(conversation);
        }
        if let Some(backlog) = self.backlog_by_jid_mut(conversation) {
            backlog.set_mam_in_flight(backward_pending);
            if !catchup_failed && !backward_pending { backlog.set_mam_exhausted(false); }
        }
    }

    // Page older history from the server archive (XEP-0313 MAM) for the active
    // conversation, when the local DB is exhausted, the server archive isn't,
    // nothing is already in flight, and we are connected.
    fn fetch_archive_active(&mut self)
    {
        if self.reconnecting || self.xmpp_cmd_tx.is_none()
        {
            return;
        }
        let Some(history) = self.history.as_ref() else { return; };
        let Some(sel) = self.active else { return; };

        let (jid, is_muc) = match sel
        {
            Selection::Room(i) => match self.rooms.get(i) { Some(r) => (r.jid.clone(), true), None => return },
            Selection::Chat(i) => match self.chats.get(i) { Some(c) => (c.jid.clone(), false), None => return },
        };

        let (cursor, end) = match self.backlog_by_jid(&jid)
        {
            Some(b) if !b.mam_in_flight() && !b.mam_exhausted()
                && b.messages().len() < backlog::MAX_LOADED =>
            {
                let cursor = b.mam_cursor();
                // Use server time only, read from the durable row rather than
                // a potentially stale view. Without archive provenance start
                // at the latest page; RSM then walks past any duplicate pages.
                let end = if cursor.is_none() {
                    match b.oldest_loaded_id().map(|id| history.archived_received(&jid, id)).transpose() {
                        Ok(stamp) => stamp.flatten().map(|stamp| stamp.to_rfc3339()),
                        Err(error) => { warn!("Cannot read archive boundary for {}: {}", jid, error); return; },
                    }
                } else { None };
                (cursor, end)
            }
            _ => return,
        };

        // "mam-b-" marks a backward scroll-up query; catch-up uses "mam-c-".
        let query_id = format!("mam-b-{}", self.mam_seq);
        self.mam_seq = self.mam_seq.wrapping_add(1);
        self.mam_pending.insert(query_id.clone(), ArchiveContext { conversation: jid.clone(), is_muc });
        if let Some(b) = self.backlog_by_jid_mut(&jid) {
            b.set_local_cursor(b.local_page_cursor());
            b.set_mam_in_flight(true);
        }

        let (to, with) = if is_muc { (Some(jid.clone()), None) } else { (None, Some(jid.clone())) };
        if !self.send_xmpp_command(xmpp::XmppCommand::QueryArchive {
            query_id: query_id.clone(), to, with, start: None, end,
            forward: false, cursor, max: backlog::PAGE_SIZE as u32,
        }) {
            if let Some(context) = self.mam_pending.remove(&query_id) {
                self.release_failed_archive_query(&query_id, &context.conversation);
            }
        }
    }

    // Catch up history missed while offline. Sweeps the recent server archive
    // BACKWARDS from the present (not forward from our newest message — an
    // offline gap sits in the *middle* of the timeline, older than the most
    // recent messages we already hold). The sweep continues to completion or
    // the bounded page limit, so a duplicate-only page cannot hide a gap. Triggered when
    // a room is joined or a conversation selected.
    fn start_catchup(&mut self, conv: &str)
    {
        if self.reconnecting || self.xmpp_cmd_tx.is_none()
        {
            return;
        }
        // Only open conversations can start a new sweep. A completed write
        // retry for a closed room must not start a direct-account query.
        let is_muc = if self.rooms.iter().any(|room| room.jid == conv) { true }
            else if self.chats.iter().any(|chat| chat.jid == conv) { false }
            else { return; };
        if is_muc && !self.room_is_joined(conv) { return; }
        // One sweep per conversation per session.
        if self.mam_catchup.contains_key(conv) || self.mam_caught_up.contains(conv)
        {
            return;
        }
        // With no local history there is no gap to bridge; a fresh conversation
        // is filled by scrolling up instead.
        let have_history = match self.history.as_ref().map(|h| h.newest_received(conv)).transpose() {
            Ok(result) => result.flatten().is_some(),
            Err(e) => { warn!("Cannot check history for {}: {}", conv, e); return; }
        };
        if !have_history
        {
            return;
        }

        let sync = match self.history.as_ref().map(|history| history.archive_sync(conv)).transpose() {
            Ok(state) => state.unwrap_or_default(),
            Err(error) => { warn!("Cannot read archive sync checkpoint for {}: {}", conv, error); return; },
        };
        let before = sync.before.clone();
        let mut progress = crate::app::CatchupProgress::default();
        progress.resumed = before.is_some();
        progress.sync = sync;
        if let Some(before) = &before { progress.remember_cursor(before); }
        self.mam_catchup.insert(conv.to_string(), progress);
        self.send_catchup_page(conv, is_muc, before);
    }

    fn save_catchup_progress(&self, conv: &str, sync: &storage::history::ArchiveSync) -> bool {
        if let Some(history) = &self.history {
            if let Err(error) = history.save_archive_sync(conv, sync) {
                warn!("Cannot save archive sync checkpoint for {}: {}", conv, error);
                return false;
            }
        }
        true
    }

    fn pause_catchup(&mut self, conv: &str, mut progress: crate::app::CatchupProgress) -> Task<Message> {
        progress.next_batch();
        let token = uuid::Uuid::new_v4();
        progress.continuation = Some(token);
        self.mam_catchup.insert(conv.to_string(), progress);
        delay_then(std::time::Duration::from_secs(5), Message::ContinueArchiveSync {
            context: self.room_retry_context(conv), token,
        })
    }

    // Walk only the range since the durable checkpoint, preserving its start
    // bound across pages and restarts. Older scrolling has its own cursor.
    fn send_catchup_page(&mut self, conv: &str, is_muc: bool, cursor: Option<String>)
    {
        let (to, with) = if is_muc
        {
            (Some(conv.to_string()), None)
        }
        else
        {
            (None, Some(conv.to_string()))
        };

        let query_id = format!("mam-c-{}", self.mam_seq);
        self.mam_seq = self.mam_seq.wrapping_add(1);
        self.mam_pending.insert(query_id.clone(), ArchiveContext { conversation: conv.to_string(), is_muc });
        let start = self.mam_catchup.get(conv).and_then(|progress| progress.sync.received).map(|stamp| stamp.to_rfc3339());

        if !self.send_xmpp_command(xmpp::XmppCommand::QueryArchive {
            query_id: query_id.clone(), to, with, start, end: None,
            forward: false, cursor, max: backlog::PAGE_SIZE as u32,
        }) {
            if let Some(context) = self.mam_pending.remove(&query_id) {
                self.release_failed_archive_query(&query_id, &context.conversation);
            }
        }
    }

    // Re-run the find search against the DB for the current query and scope,
    // ordering hits oldest-first and focusing the most recent one.
    fn run_find_search(&mut self)
    {
        let Some(find) = self.find.as_ref() else { return; };
        let query = find.query.clone();
        let all_scope = find.all_scope;

        if let Some(find) = self.find.as_mut() {
            find.pending_scroll = None;
            find.scroll_id = uuid::Uuid::new_v4();
        }
        let mut error = None;
        let result = if query.trim().is_empty() {
            Ok(Vec::new())
        } else if let Some(h) = &self.history {
            if all_scope {
                let open: Vec<_> = self.rooms.iter().map(|r| r.jid.as_str())
                    .chain(self.chats.iter().map(|c| c.jid.as_str())).collect();
                h.search_conversations(&open, &query, 2000).map_err(|e| e.to_string())
            }
            else if let Some(conv) = self.active_conv_jid() {
                h.search(Some(&conv), &query, 2000).map_err(|e| e.to_string())
            } else { Ok(Vec::new()) }
        } else { Err("local history is unavailable".into()) };
        let matches = match result {
            Ok(hits) => hits.into_iter()
                .map(|hit| crate::app::FindMatch { conversation: hit.conversation, id: hit.id, received: hit.received }).collect::<Vec<_>>(),
            Err(e) => {
                warn!("Cannot search history: {}", e);
                error = Some("unavailable".into());
                Vec::new()
            }
        };
        if let Some(find) = self.find.as_mut() {
            find.index = matches.len().saturating_sub(1);
            find.matches = matches;
            find.error = error;
        }
    }

    // Bring the currently-focused find match on screen: switch to its
    // conversation if needed, load a window centred on it, and scroll to it.
    fn jump_to_current_match(&mut self) -> Task<Message>
    {
        let Some(find) = self.find.as_ref() else { return Task::none(); };
        let Some(m) = find.matches.get(find.index).cloned() else { return Task::none(); };

        // A cached global result may outlive its sidebar conversation. Resolve
        // the destination before moving draft history or clearing selection.
        let destination = if self.active_conv_jid().as_deref() == Some(m.conversation.as_str()) {
            self.active
        } else {
            self.rooms.iter().position(|r| r.jid == m.conversation).map(Selection::Room)
                .or_else(|| self.chats.iter().position(|c| c.jid == m.conversation).map(Selection::Chat))
        };
        let Some(destination) = destination else { return Task::none(); };

        // Read the entire replacement before changing selection or drafts. A
        // failed read (or a deleted search hit) must preserve the visible log.
        let conv = m.conversation.clone();
        let stored_window = if let Some(h) = &self.history {
            let result = (|| -> rusqlite::Result<_> {
                let ctx = backlog::JUMP_CONTEXT;
                let window = h.load_window_around(&conv, m.id, ctx, ctx)?;
                let has_older = window.first().map(|w| h.has_older(&conv, w.received.timestamp_millis(), w.id))
                    .transpose()?.unwrap_or(false);
                let at_tail = !window.last().map(|w| h.has_newer(&conv, w.received.timestamp_millis(), w.id))
                    .transpose()?.unwrap_or(false);
                Ok((window, has_older, at_tail))
            })();
            match result {
                Ok((window, has_older, at_tail)) if window.iter().any(|row| row.id == m.id) => Some((window, has_older, at_tail)),
                Ok(_) => return Task::none(),
                Err(e) => { warn!("Cannot load search result for {}: {}", conv, e); return Task::none(); }
            }
        } else { None };

        // Switch conversation if the hit is elsewhere (global scope).
        if self.active != Some(destination)
        {
            self.stash_active_draft();
            self.stamp_active_read_marker(Some(destination));
            self.selected_msgs.clear();
            self.select_anchor = None;
            self.body_editors.clear();
            self.body_drag = None;
            self.active = Some(destination);
            self.load_draft(destination);
            self.persist_settings();
        }

        // Load a window centred on the hit (detached from the live tail).
        if let Some((window, has_older, at_tail)) = stored_window
        {
            if let Some(b) = self.active_backlog_mut()
            {
                b.set_window(window, has_older, at_tail);
            }
        }

        // The row's rendered bounds determine its scroll position, including
        // wrapped/multiline text, dividers, badges and link controls.
        let token = uuid::Uuid::new_v4();
        if let Some(find) = self.find.as_mut() { find.pending_scroll = Some(token); find.scroll_id = token; }
        return iced::advanced::widget::operate(crate::ui::search_scroll::SearchScroll::new(token));
    }

    fn update_scroll_position(&mut self, at_bottom: bool, near_top: bool) -> Task<Message>
    {
        let was_bottom = self.active_backlog()
            .map(|b| b.anchored_bottom())
            .unwrap_or(true);
        if let Some(b) = self.active_backlog_mut()
        {
            b.set_anchored_bottom(at_bottom);
        }

        if at_bottom && !was_bottom
        {
            // Back at the live tail: reattach, re-bound the window, and
            // reveal anything that arrived while we were scrolled up.
            if let Some(jid) = self.active_conv_jid()
            {
                match self.active
                {
                    Some(Selection::Room(i)) => seed_tail_for(&self.history, &jid, &mut self.rooms[i]),
                    Some(Selection::Chat(i)) => seed_tail_for(&self.history, &jid, &mut self.chats[i]),
                    None => {}
                }
                self.mark_active_read_if_visible();
                return snap_to_bottom();
            }
        }
        else if near_top
        {
            // Page older history in at the front; the bottom anchor keeps
            // the user's current content visually fixed across the prepend.
            // When the local DB is exhausted, reach to the server archive.
            match self.page_older_active() {
                Ok(false) => self.fetch_archive_active(),
                Ok(true) => {},
                Err(e) => warn!("Cannot load older history: {}", e),
            }
        }
        Task::none()
    }

    // When switching away from the currently active room/chat to `next`, stamp
    // the read marker on the one we're leaving so messages arriving while away
    // are flagged as new. No-op when staying on the same selection.
    fn stamp_active_read_marker(&mut self, next: Option<Selection>)
    {
        match self.active
        {
            Some(Selection::Room(old_idx)) if next != Some(Selection::Room(old_idx)) =>
            {
                if let Some(r) = self.rooms.get_mut(old_idx)
                {
                    if !r.unread && r.at_live_tail && r.anchored_bottom { r.stamp_read_boundary(); }
                }
            }
            Some(Selection::Chat(old_idx)) if next != Some(Selection::Chat(old_idx)) =>
            {
                if let Some(c) = self.chats.get_mut(old_idx)
                {
                    if !c.unread && c.at_live_tail && c.anchored_bottom { c.stamp_read_boundary(); }
                }
            }
            _ => {}
        }
    }

    // JID identifying a sidebar selection; used as the key into the draft stash.
    fn selection_jid(&self, sel: Selection) -> Option<String>
    {
        return match sel
        {
            Selection::Room(i) => self.rooms.get(i).map(|r| r.jid.clone()),
            Selection::Chat(i) => self.chats.get(i).map(|c| c.jid.clone()),
        };
    }

    // Move the active conversation's in-progress text into the draft stash, or
    // clear its entry when blank. Call before changing `self.active` so each
    // room/DM keeps its own unsent text when the user switches away.
    fn stash_active_draft(&mut self)
    {
        let Some(jid) = self.active.and_then(|s| self.selection_jid(s)) else
        {
            return;
        };

        self.input_history.break_group();
        self.draft_histories.insert(jid, std::mem::take(&mut self.input_history));
        self.snapshot_active_draft();
    }

    // Saving/failed-close retries keep the active editor and its undo history.
    fn snapshot_active_draft(&mut self)
    {
        let Some(jid) = self.active.and_then(|selection| self.selection_jid(selection)) else { return; };
        let text = self.message_input.text();
        if text.trim().is_empty()
        {
            self.saved_config.drafts.remove(&jid);
        }
        else
        {
            self.saved_config.drafts.insert(jid, text);
        }
    }

    // Load the draft stashed for `sel` into the editor (cursor at the end), or
    // empty the editor when none is stashed.
    fn load_draft(&mut self, sel: Selection)
    {
        self.composer_id = uuid::Uuid::new_v4();
        self.send_error = None;
        // Matching draft text does not mean the previous room's completion
        // candidates belong to this conversation.
        self.nick_complete = None;
        self.input_history = self.selection_jid(sel)
            .and_then(|jid| self.draft_histories.remove(&jid)).unwrap_or_default();
        let draft = self.selection_jid(sel)
            .and_then(|jid| self.saved_config.drafts.get(&jid).cloned())
            .unwrap_or_default();

        self.message_input = text_editor::Content::with_text(&draft);
        self.message_input.perform(text_editor::Action::Move(
            iced::widget::text_editor::Motion::DocumentEnd,
        ));
    }

    // Drop the active conversation's stashed draft (e.g. after sending or
    // closing it). Returns whether an entry was actually removed.
    fn clear_active_draft(&mut self) -> bool
    {
        let Some(jid) = self.active.and_then(|s| self.selection_jid(s)) else
        {
            return false;
        };
        return self.saved_config.drafts.remove(&jid).is_some();
    }

    // A restored keyring secret belongs to the saved login. Do not leave it
    // in an editable form after failure/cancel, or label later manual edits as
    // an unchanged automatic attempt. Manually entered passwords remain editable.
    fn finish_login_attempt(&mut self)
    {
        if self.auto_login_attempt { self.password_input.clear(); }
        self.pending_save_password = None;
        self.auto_login_attempt = false;
    }

    fn persist_settings(&mut self) -> bool
    {
        match storage::save(&self.saved_config) {
            Ok(()) => { self.settings_save_error = None; true },
            Err(error) => {
                warn!("Failed to persist settings and drafts: {}", error);
                self.settings_save_error = Some(format!("Settings and drafts could not be saved: {error}"));
                false
            }
        }
    }

    // Step through the flat list of sidebar entries (rooms first, then chats),
    // wrapping at either end. Activate within this action so queued shortcuts
    // each see the preceding selection and cannot leave a deferred target behind.
    fn step_selection(&mut self, forward: bool) -> Option<Task<Message>>
    {
        if self.state != AppState::Connected
        {
            return None;
        }

        let total = self.rooms.len() + self.chats.len();
        if total == 0
        {
            return None;
        }

        let next_idx = match self.active
        {
            None => if forward { 0 } else { total - 1 },
            Some(Selection::Room(i)) =>
            {
                let cur = i;
                if forward { (cur + 1) % total } else { (cur + total - 1) % total }
            }
            Some(Selection::Chat(i)) =>
            {
                let cur = self.rooms.len() + i;
                if forward { (cur + 1) % total } else { (cur + total - 1) % total }
            }
        };

        if next_idx < self.rooms.len()
        {
            return Some(self.update_inner(Message::SelectRoom(self.rooms[next_idx].jid.clone())));
        }

        return Some(self.update_inner(Message::SelectChat(self.chats[next_idx - self.rooms.len()].jid.clone())));
    }

    // Completion policy after dispatch from the focused composer. Unhandled
    // Tab events use TraverseFocus and never enter nickname completion.
    fn is_message_input_context(&self) -> bool
    {
        return self.state == AppState::Connected
            && !self.join_panel_visible()
            && matches!(self.active, Some(Selection::Room(_)));
    }

    fn cycle_nick_completion(&mut self, backward: bool) -> Task<Message>
    {
        let Some(Selection::Room(idx)) = self.active else { return Task::none(); };
        let current_input = self.message_input.text();
        let cursor = self.message_input.cursor();
        let previous = self.nick_complete.take()
            .filter(|state| state.last_output == current_input && state.last_cursor == cursor);
        // Completion must not silently overwrite an explicit text selection.
        if cursor.selection.is_some_and(|start| start != cursor.position) { return Task::none(); }
        let Some(line) = self.message_input.line(cursor.position.line) else { return Task::none(); };
        let line = line.text.to_string();
        let Some(before) = line.get(..cursor.position.column) else { return Task::none(); };
        let (start, prefix, old_nick, end) = if let Some(state) = previous {
            (state.prefix_start, state.prefix, Some(state.nick), cursor.position)
        } else {
            let column = before.char_indices().rev().find(|(_, c)| c.is_whitespace())
                .map(|(i, c)| i + c.len_utf8()).unwrap_or(0);
            let prefix = before[column..].to_string();
            if prefix.is_empty() { return Task::none(); }
            // Replace the rest of this word too when the caret is inside it.
            let tail = &line[cursor.position.column..];
            let end = cursor.position.column + tail.find(char::is_whitespace).unwrap_or(tail.len());
            (text_editor::Position { line: cursor.position.line, column }, prefix, None,
                text_editor::Position { line: cursor.position.line, column: end })
        };
        let prefix_lower = prefix.to_lowercase();
        let mut matches: Vec<_> = self.rooms[idx].users.iter().map(|user| user.name.clone())
            .filter(|name| name.to_lowercase().starts_with(&prefix_lower)).collect();
        let compare = |a: &str, b: &str| a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b));
        matches.sort_by(|a, b| compare(a, b));
        matches.dedup();
        if matches.is_empty() { return Task::none(); }
        // Rebuild candidates on every step so joins, departures and renames do
        // not leave stale nicknames in an otherwise unchanged completion cycle.
        let index = if let Some(old) = &old_nick {
            if backward {
                matches.iter().rposition(|name| compare(name, old).is_lt()).unwrap_or(matches.len() - 1)
            } else {
                matches.iter().position(|name| compare(name, old).is_gt()).unwrap_or(0)
            }
        } else if backward { matches.len() - 1 } else { 0 };
        let nick = matches.swap_remove(index);
        let following_whitespace = line.get(end.column..).and_then(|tail| tail.chars().next())
            .map_or(end.line + 1 < self.message_input.line_count(), char::is_whitespace);
        let suffix = if start.line == 0 && start.column == 0 {
            if following_whitespace { ":" } else { ": " }
        } else if following_whitespace { "" } else { " " };
        // Even the first completion can already match the entire word. Compare
        // the actual replacement so unchanged completions retain undo and redo.
        let replacement = format!("{nick}{suffix}");
        if line.get(start.column..end.column) != Some(replacement.as_str()) {
            let action = text_editor::Action::Edit(text_editor::Edit::Paste(std::sync::Arc::new(replacement)));
            // Capture the user's caret before selecting the replacement, so Undo
            // restores both their original draft and their original selection.
            self.input_history.record(&self.message_input, &action);
            self.message_input.move_to(text_editor::Cursor { position: end, selection: Some(start) });
            self.message_input.perform(action);
            self.send_error = None;
        } else {
            self.input_history.break_group();
            self.message_input.move_to(text_editor::Cursor { position: end, selection: None });
        }
        self.nick_complete = Some(NickCompleteState {
            prefix_start: start, prefix, nick,
            last_output: self.message_input.text(), last_cursor: self.message_input.cursor(),
        });
        return iced::widget::operation::focus(Id::new(MESSAGE_INPUT_ID));
    }

    // Checkboxes and Cmd/Ctrl-click toggle individual messages. Shift-click
    // replaces the range from the last explicitly selected anchor, so moving
    // the endpoint backward shrinks the range as well as expanding it.
    fn toggle_message_selection(&mut self, id: i64)
    {
        let Some(b) = self.active_backlog() else { return; };

        if !b.messages().iter().any(|m| matches!(m, room::message::Message::Chat { id: mid, .. } if *mid == id))
        { return; }

        if self.shift_down
        {
            if let Some(anchor) = self.select_anchor
            {
                let ids: Vec<i64> = b.messages().iter()
                    .filter_map(|m| match m
                    {
                        room::message::Message::Chat { id, .. } => Some(*id),
                        _ => None,
                    })
                    .collect();
                let a = ids.iter().position(|x| *x == anchor);
                let c = ids.iter().position(|x| *x == id);
                if let (Some(a), Some(c)) = (a, c)
                {
                    let (lo, hi) = if a <= c { (a, c) } else { (c, a) };
                    self.selected_msgs.clear();
                    for x in &ids[lo..=hi]
                    {
                        self.selected_msgs.insert(*x);
                    }
                    // Leave the anchor where it is so the user can keep adjusting
                    // the range from the same pivot.
                    return;
                }
            }
            // No usable anchor: fall through to a plain toggle.
        }

        if self.selected_msgs.remove(&id)
        {
            // Deselected; only reset the anchor if it pointed here.
            if self.select_anchor == Some(id)
            {
                self.select_anchor = None;
            }
        }
        else
        {
            self.selected_msgs.insert(id);
            self.select_anchor = Some(id);
        }
    }

    // Use the complete local timestamp even for today's messages so copied
    // transcripts remain meaningful when pasted elsewhere or read later.
    fn selected_message_text(&self) -> Option<String>
    {
        let backlog = self.active_backlog()?;
        let lines: Vec<String> = backlog.messages().iter().filter_map(|message| match message {
            room::message::Message::Chat { id, from, body, received, .. }
                if self.selected_msgs.contains(id) => Some(format!("[{}] {}: {}",
                    received.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S"), room::message::sender_label(from), body)),
            _ => None,
        }).collect();
        if lines.is_empty() { None } else { Some(lines.join("\n")) }
    }

    // Hand out a fresh negative temp id for an optimistically-shown outgoing message.
    fn next_pending_id(&mut self) -> i64
    {
        self.pending_seq -= 1;
        return self.pending_seq;
    }

    // The server echoed one of our own room messages: upgrade the matching
    // optimistic entry by exact origin ID. Legacy live echoes without an ID
    // can match the oldest unconfirmed entry with the same body. Confirm it
    // using the server's nickname and timestamp. The caller
    // verifies this is our echo; the room may have changed our nickname since
    // the optimistic entry was created. Returns whether a
    // match was found, so the caller can skip appending a duplicate.
    fn confirm_pending(
        &mut self,
        room_idx: usize,
        from: &str,
        body: &str,
        db_id: i64,
        received: chrono::DateTime<chrono::Utc>,
        origin_id: Option<&str>,
        delayed: bool,
    ) -> bool
    {
        // An explicitly different origin must never fall back to body matching.
        // Historical nicknames do not establish current occupant ownership,
        // even when the sender copied a known (non-secret) origin ID.
        if delayed || from.is_empty() { return false; }
        let origins = &self.room_send_origins;
        let room = &mut self.rooms[room_idx];
        let candidate = room.messages.iter().chain(&room.parked_messages).filter_map(|m| match m {
            room::message::Message::Chat { id, body: b, received, status, .. }
                if matches!(status, ChatStatus::Sending(_) | ChatStatus::Pending(_)) && b == body
                    && origin_id.is_none_or(|origin| origins.get(id).is_some_and(|expected| expected == origin)) => Some((*received, std::cmp::Reverse(*id))),
            _ => None,
        }).min().map(|(_, id)| id.0);
        let Some(candidate) = candidate else { return false; };
        let conversation = room.jid.clone();
        self.remap_message_ui(&conversation, db_id, body, received, [candidate]);
        let room = &mut self.rooms[room_idx];
        room.remember_read_boundary();
        room.remap_read_identity(candidate, db_id, received);
        // A confirmed parked message is now recoverable from history. Remove
        // its transient copy instead of keeping an invisible duplicate forever.
        if db_id > 0 {
            if let Some(index) = room.parked_messages.iter().position(|m|
                matches!(m, room::message::Message::Chat { id, .. } if *id == candidate)) {
                room.parked_messages.swap_remove(index);
                room.refresh_read_marker();
                return true;
            }
        }
        for m in room.messages.iter_mut().chain(&mut room.parked_messages)
        {
            if let room::message::Message::Chat { id, from: author, received: r, status, .. } = m
            {
                if *id == candidate {
                    *id = db_id;
                    *author = from.into();
                    *r = received;
                    *status = ChatStatus::Confirmed;
                    room.sort_chronologically();
                    return true;
                }
            }
        }
        return false;
    }

    // Reveal the slow-send state for an unconfirmed optimistic entry after
    // the grace period. Confirmed or trimmed entries are left alone.
    fn transition_pending(&mut self, conv: &str, temp_id: i64, to: ChatStatus)
    {
        if let Some(r) = self.rooms.iter_mut().find(|r| r.jid == conv)
        {
            for m in r.messages.iter_mut().chain(&mut r.parked_messages)
            {
                if let room::message::Message::Chat { status, .. } = m
                {
                    let matches = matches!(status,
                        ChatStatus::Sending(id) | ChatStatus::Pending(id)
                            if *id == temp_id);
                    if matches
                    {
                        *status = to;
                        return;
                    }
                }
            }
        }
    }

    fn composer_selects_text(&self) -> bool
    {
        let cursor = self.message_input.cursor();
        cursor.selection.is_some_and(|start| start != cursor.position)
    }

    fn composer_action(&mut self, action: text_editor::Action)
    {
        let _ = self.update_inner(Message::InputAction(action));
    }

    // Place the caret at `position`, extending the selection when `select`.
    fn place_composer_caret(&mut self, position: text_editor::Position, select: bool)
    {
        let cursor = self.message_input.cursor();
        let anchor = cursor.selection.unwrap_or(cursor.position);
        if !select {
            // move_to without a selection keeps the old one; a motion clears it.
            self.message_input.perform(text_editor::Action::Move(text_editor::Motion::DocumentStart));
        }
        self.message_input.move_to(text_editor::Cursor { position, selection: select.then_some(anchor) });
        self.input_history.break_group();
        self.clear_body_selection();
        self.nick_complete = None;
    }

    // Delete the selection, or else the text from the caret to a boundary.
    fn delete_composer_text(&mut self, boundary: text_editor::Motion, forward: bool)
    {
        if !self.composer_selects_text() {
            self.composer_action(text_editor::Action::Select(boundary));
            if !self.composer_selects_text() { return; }
        }
        self.composer_action(text_editor::Action::Edit(if forward { text_editor::Edit::Delete } else { text_editor::Edit::Backspace }));
    }

    // Platform editing commands from crate::ui::composer_keys. Positions are
    // logical lines (paragraphs) and byte columns.
    fn composer_edit(&mut self, command: crate::ui::composer_keys::EditCommand)
    {
        use crate::ui::composer_keys::EditCommand;
        use text_editor::{Action, Motion, Position};
        let text = self.message_input.text();
        let lines: Vec<&str> = text.split('\n').collect();
        let position = self.message_input.cursor().position;
        let line = lines.get(position.line).copied().unwrap_or_default();
        let at = |line: usize, column: usize| Position { line, column };
        match command {
            EditCommand::DeleteWordBackward => self.delete_composer_text(Motion::WordLeft, false),
            EditCommand::DeleteWordForward => self.delete_composer_text(Motion::WordRight, true),
            EditCommand::DeleteToLineStart => self.delete_composer_text(Motion::Home, false),
            EditCommand::Vertical { down, select } => {
                let step = if down { Motion::Down } else { Motion::Up };
                self.composer_action(if select { Action::Select(step) } else { Action::Move(step) });
                // NSTextView moves to the start/end past the first/last line.
                if self.message_input.cursor().position == position {
                    let edge = if down { Motion::DocumentEnd } else { Motion::DocumentStart };
                    self.composer_action(if select { Action::Select(edge) } else { Action::Move(edge) });
                }
            }
            EditCommand::ParagraphStep { forward, select, next_start } => {
                let target = if !forward {
                    if position.column > 0 { at(position.line, 0) }
                    else if position.line > 0 { at(position.line - 1, 0) }
                    else { position }
                } else if next_start {
                    if position.line + 1 < lines.len() { at(position.line + 1, 0) } else { at(position.line, line.len()) }
                } else if position.column < line.len() { at(position.line, line.len()) }
                else if position.line + 1 < lines.len() { at(position.line + 1, lines[position.line + 1].len()) }
                else { position };
                self.place_composer_caret(target, select);
            }
        }
    }

    // A press on a link opens it only if it ended as a plain click: dragging
    // a selection across or out of the link never launches a browser.
    pub(crate) fn take_link_click(&mut self) -> Option<String>
    {
        let (id, url) = self.link_press.take()?;
        if !self.body_editors.contains_key(&id) {
            log::debug!("Link release ignored: message {id} is no longer shown");
            return None;
        }
        // A click that moved slightly reports a drag with an empty range.
        let selected = self.body_editors.values().any(|content| {
            let cursor = content.cursor();
            cursor.selection.is_some_and(|start| start != cursor.position)
        });
        if selected {
            log::debug!("Link release ignored: the press became a text selection");
            return None;
        }
        log::debug!("Opening link from message {id}: {url}");
        Some(url)
    }

    fn clear_body_selection(&mut self)
    {
        self.body_drag = None;
        for content in self.body_editors.values_mut()
        {
            let mut cursor = content.cursor();
            cursor.selection = None;
            // move_to with selection=None leaves Iced's old selection intact.
            // A motion clears it; then restore the original caret position.
            content.perform(text_editor::Action::Move(text_editor::Motion::DocumentStart));
            content.move_to(cursor);
        }
    }

    fn extend_body_selection(&mut self, id: i64, point: iced::Point)
    {
        let Some((anchor_id, anchor)) = self.body_drag else { return; };
        let ids: Vec<i64> = self.active_backlog().map(|b| b.messages().iter().filter_map(|m| match m {
            room::message::Message::Chat { id, .. } => Some(*id), _ => None,
        }).collect()).unwrap_or_default();
        let (Some(start), Some(end)) = (ids.iter().position(|x| *x == anchor_id), ids.iter().position(|x| *x == id)) else { return; };
        let Some(content) = self.body_editors.get_mut(&id) else { return; };
        content.perform(text_editor::Action::Click(point));
        let endpoint = content.cursor().position;
        for (index, id) in ids.iter().enumerate() {
            let Some(content) = self.body_editors.get_mut(id) else { continue; };
            content.perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
            let last = content.cursor().position;
            let first = text_editor::Position { line: 0, column: 0 };
            let range = if start == end && index == start { Some((anchor.position, endpoint)) }
                else if index < start.min(end) || index > start.max(end) { None }
                else if index == start { Some((anchor.position, if start < end { last } else { first })) }
                else if index == end { Some((if start < end { first } else { last }, endpoint)) }
                else { Some((first, last)) };
            content.move_to(match range {
                Some((a, b)) => text_editor::Cursor { position: b, selection: Some(a) },
                None => text_editor::Cursor { position: first, selection: None },
            });
        }
    }

    // Track the selected message identities and actual free-text ranges. This
    // covers changes from history reconciliation as well as direct gestures,
    // without invalidating Copy on unrelated input or delivery-status events.
    fn selection_stamp(&self) -> (Vec<i64>, Vec<(i64, usize, usize, usize, usize)>) {
        let mut messages: Vec<_> = self.selected_msgs.iter().copied().collect();
        messages.sort_unstable();
        let mut ranges: Vec<_> = self.body_editors.iter().filter_map(|(id, content)| {
            let cursor = content.cursor();
            cursor.selection.filter(|start| *start != cursor.position).map(|start|
                (*id, start.line, start.column, cursor.position.line, cursor.position.column))
        }).collect();
        ranges.sort_unstable();
        (messages, ranges)
    }

    pub(crate) fn update(&mut self, message: Message) -> Task<Message>
    {
        let selection = self.selection_stamp();
        let task = self.update_inner(message);
        if !self.room_send_origins.is_empty() {
            let pending_room_ids: std::collections::HashSet<_> = self.rooms.iter().flat_map(|room|
                room.messages.iter().chain(&room.parked_messages)).filter_map(|message| match message {
                    room::message::Message::Chat { id, .. } if *id < 0 => Some(*id), _ => None,
                }).collect();
            self.room_send_origins.retain(|id, _| pending_room_ids.contains(id));
        }
        self.retry_unqueued_dms();
        let owner = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid));
        for send in self.unqueued_dm_sends.iter().filter(|send| send.account.as_deref() == owner) {
            if let Some(chat) = self.chats.iter_mut().find(|chat| chat.jid == send.conversation) {
                for message in chat.messages.iter_mut().chain(&mut chat.parked_messages) {
                    if let room::message::Message::Chat { id, status, .. } = message {
                        if *id == send.message_id { *status = match send.state { UnqueuedDelivery::Submitted => ChatStatus::Confirmed, UnqueuedDelivery::Failed => ChatStatus::Failed(0), UnqueuedDelivery::Unconfirmed => ChatStatus::Unconfirmed }; }
                    }
                }
            }
        }
        // A reload can read an older delivery state while saving a rejection is
        // failing. Keep the account's known failure visible until storage catches up.
        let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid));
        let failed: std::collections::HashSet<_> = self.rejected_sends.iter()
            .filter(|send| send.account.as_deref() == account && send.outbox_id > 0)
            .map(|send| send.outbox_id).collect();
        for id in failed { self.set_outgoing_delivery(id, ChatStatus::Failed(id)); }
        // Only the active conversation is rendered, so keep its per-message body
        // editors reconciled after every state change.
        self.sync_body_editors();
        if selection != self.selection_stamp() { self.selection_id = uuid::Uuid::new_v4(); }
        return task;
    }

    // Storage promotion/dedup changes row IDs without changing the logical
    // message. Carry search results and active selection along with that mapping.
    fn remap_message_ui(&mut self, conversation: &str, new_id: i64, body: &str,
        received: chrono::DateTime<chrono::Utc>, old_ids: impl IntoIterator<Item = i64>)
    {
        let old_ids: Vec<_> = old_ids.into_iter().collect();
        // Global find can reference a conversation that is not currently
        // rendered. Follow row identity merges before the active-view guard.
        if let Some(find) = self.find.as_mut() {
            let mut focused = find.matches.get(find.index).cloned();
            let mut changed = false;
            for hit in &mut find.matches {
                if hit.conversation == conversation && (hit.id == new_id || old_ids.contains(&hit.id)) {
                    changed |= hit.id != new_id || hit.received.timestamp_millis() != received.timestamp_millis();
                    hit.id = new_id;
                    hit.received = received;
                }
            }
            if changed {
                if let Some(hit) = focused.as_mut() {
                    if hit.conversation == conversation && old_ids.contains(&hit.id) { hit.id = new_id; }
                }
                // Keep one occurrence, then restore chronological navigation.
                // Focus follows the logical message rather than its old index.
                let mut seen = false;
                find.matches.retain(|hit| hit.conversation != conversation || hit.id != new_id
                    || !std::mem::replace(&mut seen, true));
                find.matches.sort_by_key(|hit| (hit.received.timestamp_millis(), hit.id));
                find.index = focused.and_then(|focused| find.matches.iter().position(|hit|
                    hit.conversation == focused.conversation && hit.id == focused.id))
                    .unwrap_or_else(|| find.index.min(find.matches.len().saturating_sub(1)));
                find.pending_scroll = None;
                find.scroll_id = uuid::Uuid::new_v4();
            }
        }
        if self.active_conv_jid().as_deref() != Some(conversation) { return; }
        let has_selection = |content: &text_editor::Content| {
            let cursor = content.cursor();
            cursor.selection.is_some_and(|start| start != cursor.position)
        };
        // Never keep an editor containing superseded text under a canonical ID.
        if self.body_editors.get(&new_id).is_some_and(|content| content.text() != body) {
            self.body_editors.remove(&new_id);
            if self.body_drag.is_some_and(|(id, _)| id == new_id) { self.body_drag = None; }
        }
        for old_id in old_ids {
            if old_id == new_id { continue; }
            if self.selected_msgs.remove(&old_id) { self.selected_msgs.insert(new_id); }
            if self.select_anchor == Some(old_id) { self.select_anchor = Some(new_id); }
            let dragging = self.body_drag.is_some_and(|(id, _)| id == old_id);
            if let Some(content) = self.body_editors.remove(&old_id) {
                if content.text() == body {
                    let replace = dragging || self.body_editors.get(&new_id)
                        .is_none_or(|target| has_selection(&content) && !has_selection(target));
                    if replace { self.body_editors.insert(new_id, content); }
                    if dragging {
                        if let Some((id, _)) = &mut self.body_drag { *id = new_id; }
                    }
                } else if dragging { self.body_drag = None; }
            } else if dragging { self.body_drag = None; }
        }
    }

    // Reconcile `body_editors` with the active conversation's Chat messages: add
    // an editor for each newly-rendered message body and drop editors for ids no
    // longer present. Builds the Content from the message body (read-only), so
    // its text-selection state persists across renders.
    fn sync_body_editors(&mut self)
    {
        let Some(b) = self.active_backlog() else
        {
            // Nothing rendered; release any stale editors.
            self.body_editors.clear();
            self.selected_msgs.clear();
            self.select_anchor = None;
            self.body_drag = None;
            return;
        };

        let mut present: std::collections::HashSet<i64> = std::collections::HashSet::new();
        let mut to_insert: Vec<(i64, String)> = Vec::new();
        for m in b.messages()
        {
            if let room::message::Message::Chat { id, body, .. } = m
            {
                present.insert(*id);
                if !self.body_editors.contains_key(id)
                {
                    to_insert.push((*id, body.clone()));
                }
            }
        }

        // Drop editors for messages no longer in the active window.
        self.body_editors.retain(|id, _| present.contains(id));
        self.selected_msgs.retain(|id| present.contains(id));
        if self.select_anchor.is_some_and(|id| !present.contains(&id)) { self.select_anchor = None; }
        if self.body_drag.is_some_and(|(id, _)| !present.contains(&id)) { self.body_drag = None; }
        // Add editors for newly-rendered messages.
        for (id, body) in to_insert
        {
            self.body_editors.insert(id, text_editor::Content::with_text(&body));
        }
    }

    pub(crate) fn session_is_draining(&self) -> bool {
        self.disconnecting || self.pending_close.is_some_and(|close| close.draining)
    }

    fn begin_window_shutdown(&mut self, window: iced::window::Id, discard: bool) -> Task<Message> {
        if let Some(channel) = self.xmpp_cmd_rx.as_ref().filter(|_| matches!(self.state, AppState::Connected | AppState::Connecting)) {
            self.pending_close = Some(crate::app::PendingClose { window, token: uuid::Uuid::new_v4(), draining: true, discard });
            channel.request_stop();
            Task::none()
        } else {
            self.pending_close = None;
            iced::window::close(window)
        }
    }

    fn update_inner(&mut self, message: Message) -> Task<Message>
    {
        if self.session_is_draining()
            && !matches!(&message, Message::XmppEvent(_) | Message::ScopedXmppEvent { .. }
                | Message::RetryHistoryWrites | Message::WindowCloseRequested(_)) {
            return Task::none();
        }
        match message
        {
            Message::Ignore => {}
            Message::ScopedShortcut { context, action } => {
                if self.state != AppState::Connected || context.view != self.composer_id
                    || context.search != self.find.as_ref().map(|find| find.id) { return Task::none(); }
                use crate::message::ShortcutAction;
                let acts_on_selection = matches!(action, ShortcutAction::Copy)
                    || (matches!(action, ShortcutAction::Escape) && context.search.is_none());
                if acts_on_selection && context.selection != self.selection_id { return Task::none(); }
                return self.update_inner(match action {
                    ShortcutAction::Escape => Message::CloseFind,
                    ShortcutAction::Copy => Message::CopySelection,
                    ShortcutAction::Join => Message::ShowJoinPanel,
                    ShortcutAction::Find => Message::ToggleFind,
                });
            }
            Message::FlushXmppCommands => self.flush_xmpp_commands(),
            Message::RetryHistoryWrites => {
                let more_history = self.retry_history_writes();
                let more_rejections = self.retry_send_rejections();
                let more_deletes = self.retry_outbox_deletes();
                self.retry_outbox_replay();
                if more_history || more_rejections || more_deletes {
                    return delay_then(std::time::Duration::from_millis(50), Message::RetryHistoryWrites);
                }
            }
            Message::ContinueArchiveSync { context, token } => {
                if self.mam_catchup.get(&context.room).and_then(|progress| progress.continuation) != Some(token) {
                    return Task::none();
                }
                if self.state != AppState::Connected || self.reconnecting || self.session_is_draining()
                    || context != self.room_retry_context(&context.room) {
                    self.mam_catchup.remove(&context.room);
                    return Task::none();
                }
                let is_muc = self.rooms.iter().any(|room| room.jid == context.room);
                if (is_muc && !self.room_is_joined(&context.room))
                    || (!is_muc && !self.chats.iter().any(|chat| chat.jid == context.room)) {
                    self.mam_catchup.remove(&context.room);
                    return Task::none();
                }
                let mut progress = self.mam_catchup.remove(&context.room).unwrap();
                if !self.save_catchup_progress(&context.room, &progress.sync) {
                    return self.pause_catchup(&context.room, progress);
                }
                progress.continuation = None;
                let before = progress.sync.before.clone();
                self.mam_catchup.insert(context.room.clone(), progress);
                self.send_catchup_page(&context.room, is_muc, before);
            }
            Message::ScopedFocus { context, backward } => {
                if context != self.focus_context() { return Task::none(); }
                return self.update_inner(Message::TraverseFocus { backward });
            }
            Message::TraverseFocus { backward } => {
                return if backward { iced::widget::operation::focus_previous() }
                    else { iced::widget::operation::focus_next() };
            }
            Message::TabPressed =>
            {
                if self.is_message_input_context()
                {
                    return self.cycle_nick_completion(false);
                }
                return iced::widget::operation::focus_next();
            }
            Message::ShiftTabPressed =>
            {
                if self.is_message_input_context()
                {
                    return self.cycle_nick_completion(true);
                }
                return iced::widget::operation::focus_previous();
            }
            Message::Navigate { context, forward } => {
                // Navigation is relative to the account's sidebar, so repeated
                // events may cross conversations but never cross connections.
                if self.state != AppState::Connected || context != self.navigation_context() { return Task::none(); }
                return self.update_inner(if forward { Message::NextSelection } else { Message::PrevSelection });
            }
            Message::NextSelection =>
            {
                if let Some(task) = self.step_selection(true)
                {
                    return task;
                }
            }
            Message::PrevSelection =>
            {
                if let Some(task) = self.step_selection(false)
                {
                    return task;
                }
            }
            Message::ScopedLogin { context, phase, action } => {
                if context != self.login_form_id || phase != self.state
                    || !matches!(self.state, AppState::Login | AppState::Connecting) {
                    return Task::none();
                }
                use crate::message::LoginAction;
                return self.update_inner(match action {
                    LoginAction::Jid(value) => Message::JidInputChanged(value),
                    LoginAction::Password(value) => Message::PasswordInputChanged(value),
                    LoginAction::Remember(value) => Message::RememberMeToggled(value),
                    LoginAction::FocusPassword => Message::FocusPassword,
                    LoginAction::Connect => Message::Connect,
                    LoginAction::Cancel => Message::CancelConnect,
                    LoginAction::RetrySavedLogin => Message::RetrySavedLogin,
                    LoginAction::RetryCredentialRemoval => Message::RetryCredentialRemoval,
                    LoginAction::RetryConfigLoad => Message::RetryConfigLoad,
                    LoginAction::RecoverConfiguration => Message::RecoverConfiguration,
                });
            }
            Message::JidInputChanged(value) =>
            {
                if self.state != AppState::Login { return Task::none(); }
                self.finish_login_attempt();
                self.jid_input = value;
            }
            Message::PasswordInputChanged(value) =>
            {
                if self.state != AppState::Login { return Task::none(); }
                self.finish_login_attempt();
                self.password_input = value;
            }
            Message::RememberMeToggled(value) =>
            {
                if self.state != AppState::Login { return Task::none(); }
                self.remember_me = value;

                // Unchecking "Remember me" on the login form immediately clears
                // any saved credentials so the user isn't locked into auto-login.
                if !value
                {
                    self.saved_login_error = None;
                    self.credential_error = storage::remove_auto_login(&mut self.saved_config).err();
                }
            }
            Message::SaveRoomToggled(value) =>
            {
                self.save_room = value;
            }
            Message::FocusPassword =>
            {
                return iced::widget::operation::focus(Id::new(ACCOUNT_PASSWORD_INPUT_ID));
            }
            Message::Connect =>
            {
                if self.state != AppState::Login { return Task::none(); }
                if self.saved_config.load_error.is_some() { self.finish_login_attempt(); return Task::none(); }
                let jid = self.jid_input.trim().to_string();
                let password = self.password_input.clone();

                if jid.is_empty() || password.is_empty()
                {
                    error!("Connection failed: JID and password are required");
                    self.finish_login_attempt();
                    self.connect_error = Some("JID and password are required.".to_string());

                    return Task::none();
                }

                if let Err(e) = ::xmpp::validate_account_jid(&jid)
                {
                    error!("Connection failed: {}", e);
                    self.finish_login_attempt();
                    self.connect_error = Some(e.to_string());

                    return Task::none();
                }

                let account = jid.split('/').next().unwrap_or(&jid).to_string();
                self.login_form_id = uuid::Uuid::new_v4();
                self.composer_id = uuid::Uuid::new_v4();
                self.end_unqueued_attempts();
                self.cancel_room_operations();
                self.room_connections.clear();
                if self.saved_config.activate_account(&account) {
                    self.draft_histories.clear();
                }
                self.persist_settings();
                self.history = storage::open_history(&account, self.saved_config.jid.as_deref());
                self.restore_direct_chats(&account);
                self.connected_jid = Some(jid.clone());
                self.bound_jid = None;
                self.connect_error = None;
                self.saved_login_error = None;
                self.pending_save_password = Some(password.clone());

                let (cmd_tx, cmd_rx) = xmpp::new_command_channel(jid, password);
                self.xmpp_pending_commands.clear();
                self.outbox_in_flight.clear();
                self.outbox_replay_pending.clear();
                self.send_error = None;
                self.xmpp_cmd_tx = Some(cmd_tx);
                self.xmpp_cmd_rx = Some(cmd_rx);

                self.state = AppState::Connecting;

                return Task::none();
            }
            Message::ForceReconnect =>
            {
                // Manual "get me out of a stuck reconnect": tell the worker to
                // skip the rest of the backoff and retry now.
                if self.reconnecting
                {
                    if self.xmpp_cmd_tx.is_some()
                    {
                        self.send_xmpp_command(xmpp::XmppCommand::ForceReconnect);
                    }
                }

                return Task::none();
            }
            Message::CancelConnect =>
            {
                if self.state != AppState::Connecting { return Task::none(); }
                self.login_form_id = uuid::Uuid::new_v4();
                // Revoke credential restoration immediately, but retain this
                // account's incoming subscription until native close finishes.
                self.finish_login_attempt();
                self.connect_error = None;
                self.xmpp_pending_commands.clear();
                return self.update_inner(Message::Disconnect);
            }
            Message::ScopedXmppEvent { channel, event } =>
            {
                // Dropping a subscription cannot retract events already queued
                // by its worker. Retain the channel identity with each event so
                // logout and new logins cannot consume the previous worker's data.
                if self.xmpp_cmd_rx.as_ref() != Some(&channel) { return Task::none(); }
                return self.update_inner(Message::XmppEvent(event));
            }
            Message::XmppEvent(event) =>
            {
                log::debug!("UI received XmppEvent: {:?}", event);
                let (event, unstamped_replay) = event.split_unstamped_replay();
                if self.session_is_draining()
                    && matches!(&event, xmpp::XmppEvent::Connected | xmpp::XmppEvent::Reconnected { .. }
                        | xmpp::XmppEvent::Reconnecting | xmpp::XmppEvent::Disconnected(_)) {
                    return Task::none();
                }
                let private_room = matches!(&event, xmpp::XmppEvent::RoomPrivateMessage { .. });
                match event
                {
                    xmpp::XmppEvent::UnstampedReplay(_) => unreachable!("replay wrappers were unpacked before dispatch"),
                    xmpp::XmppEvent::Stopped => {
                        if self.disconnecting && !self.pending_close.is_some_and(|close| close.draining) {
                            self.disconnecting = false;
                            self.xmpp_cmd_tx = None;
                            self.xmpp_cmd_rx = None;
                            self.retry_history_writes();
                            self.retry_send_rejections();
                            self.retry_outbox_deletes();
                            return self.update_inner(Message::Disconnect);
                        }
                        let Some(close) = self.pending_close.filter(|close| close.draining) else {
                            return self.update_inner(Message::XmppEvent(xmpp::XmppEvent::Disconnected("Connection ended.".into())));
                        };
                        self.disconnecting = false;
                        self.xmpp_cmd_tx = None;
                        self.xmpp_cmd_rx = None;
                        self.xmpp_pending_commands.clear();
                        self.end_unqueued_attempts();
                        self.snapshot_active_draft();
                        self.retry_history_writes();
                        self.retry_send_rejections();
                        self.retry_outbox_deletes();
                        let saved = self.persist_settings();
                        if close.discard || (saved && !self.has_unsaved_message_updates()) {
                            // Retain the draining guard until the OS close runs.
                            return iced::window::close(close.window);
                        }
                        self.pending_close = Some(crate::app::PendingClose { window: close.window, token: uuid::Uuid::new_v4(), draining: false, discard: false });
                        // The connection is already closed. Preserve its draft
                        // before returning to login and exposing save recovery.
                        let _ = self.update_inner(Message::Disconnect);
                    }
                    xmpp::XmppEvent::RoomEvent { room, context, event } => {
                        let current = self.room_operations.get(&room).is_some_and(|operation| operation.is_current() && operation.id == context);
                        if current { return self.update_inner(Message::XmppEvent((*event).with_unstamped_replay(unstamped_replay))); }
                        let (event, inner_unstamped) = (*event).split_unstamped_replay();
                        // Old control events must not alter a replacement entry.
                        // Actual messages still belong in this account's history.
                        if let xmpp::XmppEvent::RoomMessage { room: source, nick, body, timestamp, delayed, stanza_id, origin_id: _ } = event {
                            if source == room && self.connected_jid.is_some() { self.persist_message_with_policy(&room, &nick, &body, timestamp,
                                delayed && !unstamped_replay && !inner_unstamped, stanza_id.as_deref(), false); }
                        }
                    }
                    xmpp::XmppEvent::BoundJid(jid) => {
                        self.bound_jid = Some(jid);
                        return Task::none();
                    }
                    xmpp::XmppEvent::Connected =>
                    {
                        self.send_xmpp_command(xmpp::XmppCommand::SessionReady(0));
                        let password = self.pending_save_password.take();
                        let was_auto_login = self.auto_login_attempt;
                        self.password_input.clear();
                        self.state = AppState::Connected;
                        self.auto_login_attempt = false;

                        // Any MAM query that was outstanding when the link dropped
                        // will never complete; clear the in-flight flags so paging
                        // can resume after a reconnect. Re-arm catch-up so the
                        // freshly-reconnected session backfills the new gap.
                        self.mam_pending.clear();
                        self.mam_catchup.clear();
                        self.mam_caught_up.clear();
                        for r in self.rooms.iter_mut() { r.mam_in_flight = false; }
                        for c in self.chats.iter_mut() { c.mam_in_flight = false; }

                        let jid = self.connected_jid.clone().unwrap_or_default();

                        // Persist credential changes in an order that keeps failed
                        // removal retryable and never deletes a newly saved login.
                        let result = if self.remember_me {
                            storage::remember_login(&mut self.saved_config, &jid,
                                if was_auto_login { None } else { password.as_deref() })
                        } else { storage::remove_auto_login(&mut self.saved_config) };
                        self.credential_error = result.err();

                        // Auto-join any saved rooms.
                        if self.xmpp_cmd_tx.is_some()
                        {
                            self.join_rooms_once(self.saved_config.rooms.clone());
                        }

                        // Flush any DMs queued from a previous run; rooms flush
                        // per-room on RoomJoined once the join is confirmed.
                        self.request_outbox_replay(None);

                        return focus_join_input();
                    }
                    xmpp::XmppEvent::Reconnecting =>
                    {
                        // The session dropped and the worker is retrying. Keep
                        // rooms/chats/selection in place and just flag the UI.
                        self.reconnecting = true;
                        self.end_unqueued_attempts();
                        self.cancel_room_operations();
                        for state in self.room_connections.values_mut() { *state = RoomConnection::Joining; }
                        self.xmpp_pending_commands.clear();
                        self.outbox_in_flight.clear();
                        self.outbox_replay_pending.clear();
                        self.send_error = None;
                        self.mam_pending.clear();
                        self.mam_catchup.clear();
                        self.mam_caught_up.clear();
                        for r in &mut self.rooms { r.mam_in_flight = false; r.mam_exhausted = false; r.mam_cursor = None; }
                        for c in &mut self.chats { c.mam_in_flight = false; c.mam_exhausted = false; c.mam_cursor = None; }

                        return Task::none();
                    }
                    xmpp::XmppEvent::Reconnected { generation } =>
                    {
                        self.send_xmpp_command(xmpp::XmppCommand::SessionReady(generation));
                        // The session is back. Re-join every open room (saved or
                        // ad-hoc); RoomJoined refreshes members without disturbing
                        // history or the current selection.
                        self.reconnecting = false;

                        if self.xmpp_cmd_tx.is_some()
                        {
                            let rooms: std::collections::BTreeSet<_> = self.rooms.iter().map(|room| room.jid.clone())
                                .chain(self.room_connections.keys().cloned()).chain(self.saved_config.rooms.iter().cloned()).collect();
                            self.join_rooms_once(rooms);
                        }

                        if let Some(jid) = self.joining_room.clone() {
                            if !matches!(self.room_connections.get(&jid), Some(RoomConnection::Joining)) {
                                self.send_xmpp_command(xmpp::XmppCommand::JoinRoom(jid));
                            }
                        }

                        // Flush queued DMs now; queued room messages flush per-room
                        // on RoomJoined, once the (re)join is confirmed.
                        self.request_outbox_replay(None);
                        let chats: Vec<_> = self.chats.iter().map(|c| c.jid.clone()).collect();
                        for jid in chats { self.start_catchup(&jid); }

                        return Task::none();
                    }
                    // Delivery result for a queued outgoing message: drop it from the
                    // outbox after server confirmation; on failure leave it
                    // queued to be retried on the next (re)join/reconnect.
                    xmpp::XmppEvent::SendResult { id, ok } =>
                    {
                        if id < 0 {
                            if let Some(send) = self.unqueued_dm_sends.iter_mut().find(|send| send.command_id == id) {
                                if send.state != UnqueuedDelivery::Failed { send.state = if ok { UnqueuedDelivery::Submitted } else { UnqueuedDelivery::Unconfirmed }; }
                                send.dirty = true; send.awaiting_result = false;
                            }
                        }
                        self.outbox_in_flight.remove(&id);
                        if ok && id > 0
                        {
                            let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid).to_string());
                            self.pending_outbox_deletes.insert((account, id));
                            self.retry_outbox_deletes();
                        }
                    }
                    xmpp::XmppEvent::SendRejected { id, conversation, body, reason } =>
                    {
                        self.outbox_in_flight.remove(&id);
                        if id < 0 {
                            if let Some(send) = self.unqueued_dm_sends.iter_mut().find(|send| send.command_id == id) {
                                send.state = UnqueuedDelivery::Failed; send.dirty = true; send.awaiting_result = false;
                            }
                        }
                        self.record_rejected_send(id, conversation, body, reason, false);
                        self.retry_send_rejections();
                    }
                    xmpp::XmppEvent::MessageError { origin_id, from, error_type, condition, text } =>
                    {
                        // 'continue' is explicitly a warning, not a failed send.
                        if error_type == "continue" { return Task::none(); }
                        if uuid::Uuid::parse_str(&origin_id).is_err() || from.as_ref().is_some_and(|from| from.len() > ::xmpp::MAX_JID_BYTES) { return Task::none(); }
                        let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid).to_string());
                        let own_jid = self.own_jid().map(|jid| jid.split('/').next().unwrap_or(jid)).unwrap_or("").to_string();
                        let summary = match condition.as_str() {
                            "forbidden" | "not-allowed" | "not-authorized" => "The recipient or server refused this message.",
                            "recipient-unavailable" | "service-unavailable" | "remote-server-timeout" | "remote-server-not-found" => "The recipient or messaging service is unavailable.",
                            "jid-malformed" | "bad-request" => "The message address or format was rejected.",
                            _ => "The message could not be delivered.",
                        };
                        let reason = if let Some(text) = text.filter(|text| !text.trim().is_empty()) {
                            format!("{} {}", summary, text.chars().take(1024).collect::<String>())
                        } else { summary.to_string() };
                        self.retry_send_rejections();
                        if !self.pending_message_errors.iter().any(|error|
                            error.account == account && error.origin_id == origin_id && error.from == from && error.own_jid == own_jid) {
                            if self.pending_message_errors.iter().filter(|error| error.account == account).count() >= 1024 {
                                warn!("Delivery report queue is full while storage is unavailable");
                                self.send_error = Some("Some delivery reports could not be processed while storage was unavailable.".into());
                            } else {
                                self.pending_message_errors.push(crate::app::PendingMessageError { account, own_jid, origin_id, from, reason });
                            }
                        }
                        self.retry_send_rejections();
                    }
                    xmpp::XmppEvent::Disconnected(reason) =>
                    {
                        self.login_form_id = uuid::Uuid::new_v4();
                        self.composer_id = uuid::Uuid::new_v4();
                        error!("Disconnected: {}", reason);
                        // Terminal failures (including rejected reconnects) clear
                        // conversations below. Save the active draft first.
                        self.stash_active_draft();
                        self.persist_settings();

                        // A connection failure does not prove the stored password
                        // is wrong. Keep it until the user explicitly changes or
                        // removes it; successful login replaces edited credentials.
                        self.connect_error = Some(reason);
                        self.connected_jid = None;
                        self.bound_jid = None;
                        self.state = AppState::Login;
                        self.reconnecting = false;
                        self.rooms.clear();
                        self.end_unqueued_attempts();
                        self.cancel_room_operations();
                        self.room_connections.clear();
                        self.chats.clear();
                        self.earlier_chats.clear();
                        self.active = None;
                        self.message_input = text_editor::Content::new();
                        self.input_history = Default::default();
                        self.show_join_panel = false;
                        self.joining_room = None;
                        self.join_error = None;
                        self.join_input.clear();
                        self.xmpp_cmd_tx = None;
                        self.xmpp_pending_commands.clear();
                        self.outbox_in_flight.clear();
                        self.outbox_replay_pending.clear();
                        self.send_error = None;
                        self.xmpp_cmd_rx = None;
                        self.finish_login_attempt();

                        return focus_jid_input();
                    }
                    xmpp::XmppEvent::RoomJoined { room: jid, members, own_nick } =>
                    {
                        // A queued confirmation cannot restore membership after
                        // the user left or the entry attempt already failed.
                        if self.reconnecting || !matches!(self.room_connections.get(&jid), Some(RoomConnection::Joining | RoomConnection::Joined)) {
                            return Task::none();
                        }
                        self.room_connections.insert(jid.clone(), RoomConnection::Joined);
                        // A new room operation owns a new continuation token.
                        // Resume any durable progress without accepting its old timer.
                        if self.mam_catchup.get(&jid).is_some_and(|progress| progress.continuation.is_some()) {
                            self.mam_catchup.remove(&jid);
                        }
                        // `jid` is moved into the room entry below; keep a copy for
                        // the catch-up trigger at the end.
                        let conv_jid = jid.clone();
                        let requested = self.joining_room.as_deref() == Some(jid.as_str());
                        if requested {
                            self.joining_room = None;
                            self.join_error = None;
                        }
                        let activate = requested || self.active.is_none();

                        // The room is (re)joined: flush any messages queued for it
                        // while offline (now that the server will accept them),
                        // once its join history ends with the subject.
                        self.room_history_pending.insert(conv_jid.clone(), std::time::Instant::now() + ROOM_HISTORY_REPLAY_WAIT);
                        self.request_outbox_replay(Some(conv_jid.clone()));

                        let index = if let Some(pos) = self.rooms.iter().position(|r| r.jid == jid)
                        {
                            // Already open (e.g. re-joined after a reconnect):
                            // refresh members but leave the selection untouched.
                            self.rooms[pos].users = members.into_iter().map(|m| room::user::User
                            {
                                jid: m.jid,
                                name: m.nick,
                                show: m.show,
                                status: m.status,
                            }).collect();
                            self.rooms[pos].own_nick = own_nick.clone();
                            self.rooms[pos].privacy = Default::default();

                            // The server replays a short history right after this
                            // event (libxmpp flushes RoomJoined before the buffered
                            // backlog). Mark the boundary so a divider separates the
                            // old chat from that replayed history, which can repeat
                            // messages we already have. Skip if we already have a
                            // marker at this exact spot (e.g. a reconnect with no
                            // new messages since the last one).
                            let boundary = self.rooms[pos].messages.len();
                            if self.rooms[pos].history_markers.last() != Some(&boundary)
                            {
                                self.rooms[pos].history_markers.push(boundary);
                            }
                            pos
                        }
                        else
                        {
                            let title = jid.split('@').next().unwrap_or(&jid).to_string();
                            let users = members.into_iter().map(|m| room::user::User
                            {
                                jid: m.jid,
                                name: m.nick,
                                show: m.show,
                                status: m.status,
                            }).collect();
                            self.rooms.push(room::Room
                            {
                                jid,
                                title,
                                topic: String::new(),
                                privacy: Default::default(),
                                own_nick,
                                users,
                                messages: Vec::new(),
                                parked_messages: Vec::new(),
                                unread: false,
                                read_marker: None,
                                read_boundary: None,
                                history_markers: Vec::new(),
                                oldest_loaded_id: None, local_cursor: None,
                                has_older: false,
                                at_live_tail: true,
                                anchored_bottom: true,
                                mam_in_flight: false,
                                mam_exhausted: false,
                                mam_cursor: None,
                            });

                            let i = self.rooms.len() - 1;
                            // Seed the freshly-opened room with its persisted tail
                            // so last session's history is visible immediately
                            // (before any server replay, which then dedupes
                            // against the DB).
                            let conv = self.rooms[i].jid.clone();
                            seed_tail_for(&self.history, &conv, &mut self.rooms[i]);
                            i
                        };

                        let changed = activate && self.active != Some(Selection::Room(index));
                        if changed {
                            self.stash_active_draft();
                            self.stamp_active_read_marker(Some(Selection::Room(index)));
                            self.selected_msgs.clear();
                            self.select_anchor = None;
                            self.body_editors.clear();
                            self.body_drag = None;
                            self.find = None;
                            self.active = Some(Selection::Room(index));
                            self.load_draft(Selection::Room(index));
                        }

                        // Saving the departing draft is independent of whether
                        // this room is added to the auto-join list.
                        let remember = requested && self.save_room && !self.saved_config.rooms.iter().any(|r| r == &conv_jid || ::xmpp::jid_matches(r, &conv_jid));
                        if remember { self.saved_config.rooms.push(conv_jid.clone()); }
                        if changed || remember { self.persist_settings(); }

                        // Fill any history missed while we were away (offline gap)
                        // by catching up the server archive forward from our last
                        // stored message.
                        self.start_catchup(&conv_jid);

                        if requested { self.show_join_panel = false; }
                        if activate { return Task::batch([snap_to_bottom(), focus_input()]); }
                        return Task::none();
                    }
                    xmpp::XmppEvent::RoomJoinFailed { room, reason } =>
                    {
                        return self.room_join_failed(room, reason);
                    }
                    xmpp::XmppEvent::PresenceError { from, condition, text } =>
                    {
                        let room = from.split('/').next().unwrap_or(&from).to_string();
                        let is_join_error = matches!(self.room_connections.get(&room), Some(RoomConnection::Joining)) || self.joining_room.as_ref().map_or(false, |room|
                        {
                            from == *room || from.starts_with(&format!("{}/", room))
                        });

                        if is_join_error
                        {
                            let message = match condition.as_str()
                            {
                                "item-not-found" => "Room does not exist.".to_string(),
                                "not-allowed" => "Not allowed to join this room.".to_string(),
                                "forbidden" => "You are banned from this room.".to_string(),
                                "conflict" => "Nickname is already in use.".to_string(),
                                "service-unavailable" => "Room service is unavailable.".to_string(),
                                "registration-required" => "Registration is required to join this room.".to_string(),
                                "not-authorized" => "Not authorized to join this room.".to_string(),
                                _ => text.unwrap_or_else(|| format!("Could not join room: {}.", condition)),
                            };

                            return self.room_join_failed(room, message);
                        }
                    }
                    xmpp::XmppEvent::RoomLeft(jid) =>
                    {
                        if let Some(operation) = self.room_operations.remove(&jid) { operation.cancel(); }
                        self.room_connections.remove(&jid);
                        self.outbox_replay_pending.remove(&Some(jid.clone()));
                        if let Some(pos) = self.rooms.iter().position(|r| r.jid == jid)
                        {
                            // A server departure (kick, shutdown, lost membership)
                            // is not a request to discard unfinished text. Manual
                            // LeaveRoom already removes its sidebar entry below.
                            if self.active == Some(Selection::Room(pos)) {
                                self.stash_active_draft();
                                self.persist_settings();
                            }
                            self.rooms.remove(pos);
                            if let Some(Selection::Room(active)) = self.active
                            {
                                if active == pos
                                {
                                    self.active = None;
                                    self.message_input = text_editor::Content::new();
                                    self.input_history = Default::default();
                                }
                                else if active > pos
                                {
                                    self.active = Some(Selection::Room(active - 1));
                                }
                            }
                        }
                    }
                    xmpp::XmppEvent::MemberJoined { room, member } =>
                    {
                        if let Some(r) = self.rooms.iter_mut().find(|r| r.jid == room)
                        {
                            let jid = member.jid.clone();
                            let nick = member.nick.clone();
                            let existing_idx = r.users.iter().position(|u| u.name == nick);

                            if let Some(idx) = existing_idx
                            {
                                if jid.is_some() { r.users[idx].jid = jid; }
                                let old_show = r.users[idx].show.clone();
                                r.users[idx].show = member.show.clone();
                                r.users[idx].status = member.status;

                                if old_show != r.users[idx].show
                                {
                                    let _ = r.append_live(room::message::Message::Event
                                    {
                                        kind: room::message::EventKind::StatusChanged(r.users[idx].show.clone()),
                                        nick,
                                        received: chrono::Utc::now(),
                                    });
                                }
                            }
                            else
                            {
                                r.users.push(room::user::User
                                {
                                    jid: jid.clone(),
                                    name: nick.clone(),
                                    show: member.show,
                                    status: member.status,
                                });
                                let _ = r.append_live(room::message::Message::Event
                                {
                                    kind: room::message::EventKind::Joined,
                                    nick,
                                    received: chrono::Utc::now(),
                                });
                            }
                        }
                    }
                    xmpp::XmppEvent::MemberNicknameChanged { room, old_nick, new_nick, is_self } =>
                    {
                        if let Some(r) = self.rooms.iter_mut().find(|r| r.jid == room) {
                            if is_self { r.own_nick = new_nick.clone(); }
                            let old = r.users.iter().position(|user| user.name == old_nick);
                            let new = r.users.iter().position(|user| user.name == new_nick);
                            match (old, new) {
                                (Some(old), Some(new)) if old != new => {
                                    if r.users[new].jid.is_none() { r.users[new].jid = r.users[old].jid.clone(); }
                                    r.users.remove(old);
                                }
                                (Some(old), _) => r.users[old].name = new_nick,
                                _ => {},
                            }
                        }
                    }
                    xmpp::XmppEvent::MemberLeft { room, nick } =>
                    {
                        if let Some(r) = self.rooms.iter_mut().find(|r| r.jid == room)
                        {
                            r.users.retain(|u| u.name != nick);
                            let _ = r.append_live(room::message::Message::Event
                            {
                                kind: room::message::EventKind::Left,
                                nick,
                                received: chrono::Utc::now(),
                            });
                        }
                    }
                    xmpp::XmppEvent::RoomMessage { room, nick, body, timestamp, delayed, stanza_id, origin_id } =>
                    {
                        let Some(idx) = self.rooms.iter().position(|r| r.jid == room) else
                        {
                            // The native reader only forwards requested-room
                            // traffic, including buffers from an unconfirmed entry
                            // or a final message after leaving. Save without reopening.
                            self.persist_message_with_policy(&room, &nick, &body, timestamp, delayed && !unstamped_replay, stanza_id.as_deref(), false);
                            return Task::none();
                        };

                        // Persist + dedup first. A None result means this was a
                        // duplicate (e.g. MUC history replayed on rejoin, or the
                        // archive copy of a message we already hold) and is dropped
                        // without ever reaching the UI.
                        let mut reconciled_id = None;
                        let mut reconciled_status = None;
                        // A delayed copy (join history) is not proof of authorship by
                        // nickname alone, but our unguessable origin ID still queued
                        // for this room proves the server already delivered that send.
                        let own_queued_copy = delayed && origin_id.as_ref().is_some_and(|origin|
                            self.history.as_ref().is_some_and(|history| matches!(history.has_queued_room_origin(&room, &body, origin), Ok(true))));
                        if (!delayed || own_queued_copy) && !nick.is_empty() && nick == self.rooms[idx].own_nick {
                            if let Some(origin) = &origin_id {
                                let result = self.history.as_ref().map(|history|
                                    store_own_room_copy(history, &room, &nick, &body, origin, timestamp, stanza_id.as_deref()));
                                match result {
                                    Some(Ok(Some((stored, merged, created)))) => {
                                        let confirmed = self.confirm_pending(idx, &nick, &body, stored.id, stored.received, Some(origin), false);
                                        self.rooms[idx].reconcile_stored(&stored, &merged);
                                        self.remap_message_ui(&room, stored.id, &stored.body, stored.received, merged.iter().copied());
                                        let visible = self.rooms[idx].messages.iter().any(|message| matches!(message, room::message::Message::Chat { id, .. } if *id == stored.id));
                                        if confirmed || visible || !created { return Task::none(); }
                                        reconciled_id = Some(stored.id);
                                        reconciled_status = Some(stored.delivery.into());
                                    }
                                    Some(Err(_)) | None => {
                                        // Preserve the authority of this live own echo
                                        // across a history outage; do not infer it again
                                        // from a historical nickname during retry.
                                        let known_temp = self.rooms[idx].messages.iter().chain(&self.rooms[idx].parked_messages).find_map(|message| match message {
                                            room::message::Message::Chat { id, body: text, .. } if text == &body && self.room_send_origins.get(id) == Some(origin) => Some(*id), _ => None,
                                        });
                                        let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid).to_string());
                                        let pending_id = self.pending_history_writes.iter().find(|pending| pending.account == account && pending.conversation == room
                                            && pending.own_room_origin && pending.body == body && pending.own_origin.as_ref() == Some(origin)
                                            && pending.stanza_id == stanza_id).map(|pending| pending.temp_id);
                                        let temp_id = pending_id.or(known_temp).unwrap_or_else(|| self.next_pending_id());
                                        if pending_id.is_none() {
                                            self.pending_history_writes.push(crate::app::PendingHistoryWrite { receipt: uuid::Uuid::new_v4(),
                                                account, conversation: room.clone(), sender: nick.clone(), body: body.clone(), received: timestamp,
                                                delayed: false, stanza_id: stanza_id.clone(), temp_id, outbox_id: None,
                                                own_origin: Some(origin.clone()), own_room_origin: true, allow_outgoing_fuzzy: false,
                                            });
                                        }
                                        if self.confirm_pending(idx, &nick, &body, temp_id, timestamp, Some(origin), false)
                                            || self.rooms[idx].messages.iter().any(|message| matches!(message, room::message::Message::Chat { id, .. } if *id == temp_id)) { return Task::none(); }
                                        reconciled_id = Some(temp_id);
                                    }
                                    Some(Ok(None)) => {},
                                }
                            }
                        }
                        let conv = self.rooms[idx].jid.clone();
                        let Some(db_id) = reconciled_id.or_else(|| self.persist_message_with_policy(&conv, &nick, &body, timestamp, delayed && !unstamped_replay, stanza_id.as_deref(), false)) else
                        {
                            if !delayed && !nick.is_empty() && nick == self.rooms[idx].own_nick && origin_id.as_ref().is_some_and(|origin| self.room_send_origins.values().any(|expected| expected == origin)) {
                                // MAM may have persisted this exact copy before
                                // the live echo reaches us. Still retire its pending UI entry.
                                let canonical = self.history.as_ref().and_then(|history|
                                    history.store_with_policy(&conv, &nick, &body, timestamp, delayed, stanza_id.as_deref(), false).ok());
                                if let Some(stored) = canonical {
                                    self.confirm_pending(idx, &nick, &body, stored.id, stored.received, origin_id.as_deref(), delayed);
                                }
                            }
                            return Task::none();
                        };

                        let is_active = self.active == Some(Selection::Room(idx));
                        let own_nick = self.rooms[idx].own_nick.clone();
                        let user_is_watching = is_active && self.window_focused && !self.join_panel_visible()
                            && self.rooms[idx].at_live_tail && self.rooms[idx].anchored_bottom;
                        let is_own = !nick.is_empty() && nick == own_nick;

                        // Our own message echoed back: confirm the optimistic entry
                        // in place (adopting the real id/timestamp) instead of
                        // appending a duplicate.
                        if is_own && self.confirm_pending(idx, &nick, &body, db_id, timestamp, origin_id.as_deref(), delayed)
                        {
                            // Already on screen since we sent it; nothing to add.
                        }
                        // Otherwise extend the in-memory window, but only when it's
                        // showing the live tail AND the user is parked at the bottom.
                        // If they've scrolled/jumped into history, the message is
                        // safely in the DB and reappears on return — never yanking
                        // their view. Unsaved messages must stay in memory even
                        // while scrolled up because no database copy can restore them.
                        else if db_id < 0 || (self.rooms[idx].at_live_tail && self.rooms[idx].anchored_bottom)
                        {
                            let new_index = self.rooms[idx].append_live(room::message::Message::Chat
                            {
                                id: db_id,
                                from: nick.clone(),
                                body: body.clone(),
                                received: timestamp,
                                status: reconciled_status.unwrap_or(ChatStatus::Confirmed),
                            });

                            if !user_is_watching && !is_own
                            {
                                self.rooms[idx].unread = true;
                                self.rooms[idx].note_unread(db_id, timestamp);
                            }
                            else if let Some(new_index) = new_index.filter(|_| is_own && is_active && !self.rooms[idx].unread)
                            {
                                // Own echo in active room: advance both markers past
                                // our message so it never shows under the "new
                                // messages" or replayed-history dividers.
                                if let Some(marker) = self.rooms[idx].read_marker
                                {
                                    if marker <= new_index
                                    {
                                        self.rooms[idx].stamp_read_boundary();
                                    }
                                }
                                for marker in self.rooms[idx].history_markers.iter_mut()
                                {
                                    if *marker == new_index
                                    {
                                        *marker = new_index + 1;
                                    }
                                }
                            }
                        }
                        else
                        {
                            self.rooms[idx].at_live_tail = false;
                            if !is_own {
                                self.rooms[idx].unread = true;
                                self.rooms[idx].note_unread(db_id, timestamp);
                            }
                        }

                        // Notify on a mention while the window isn't being watched.
                        // Check the event body directly so it works even when the
                        // message wasn't appended (user scrolled into history).
                        if !is_own && room::message::mentions(&body, &own_nick) {
                            log::debug!("Mention in {room}: {}", if self.window_focused { "window focused, no alert" } else { "sending alert" });
                        }
                        if !self.window_focused && !is_own
                            && room::message::mentions(&body, &own_nick)
                        {
                            let room_name = room.split('@').next().unwrap_or(&room);
                            crate::notifications::send(&format!("{} in {}", room::message::sender_label(&nick), room_name), &body);
                        }

                        // Keep the live tail pinned to the bottom for the active
                        // room; never yank a user who has scrolled into history.
                        if is_active && !self.join_panel_visible() && self.rooms[idx].at_live_tail && self.rooms[idx].anchored_bottom
                        {
                            return snap_to_bottom();
                        }
                        return Task::none();
                    }
                    xmpp::XmppEvent::RoomPrivacy { room, privacy } => {
                        if let Some(room) = self.rooms.iter_mut().find(|known| known.jid == room) {
                            room.privacy = privacy;
                        }
                    }
                    xmpp::XmppEvent::RoomSubject { room, subject } =>
                    {
                        if let Some(r) = self.rooms.iter_mut().find(|r| r.jid == room)
                        {
                            r.topic = subject;
                        }
                        // The subject ends the join history (XEP-0045 7.2.15).
                        if self.room_history_pending.remove(&room).is_some() { self.retry_outbox_replay(); }
                    }
                    xmpp::XmppEvent::DirectMessage { from, body, timestamp, delayed, stanza_id, origin_id }
                        | xmpp::XmppEvent::RoomPrivateMessage { from, body, timestamp, delayed, stanza_id, origin_id } =>
                    {
                        if ::xmpp::validate_message(&from, "").is_err() { return Task::none(); }
                        let own_sender = !private_room && self.own_jid().is_some_and(|own|
                            ::xmpp::jid_matches(from.split('/').next().unwrap_or(&from), own.split('/').next().unwrap_or(own)));
                        // An offline service notice can precede the native join
                        // request on a new connection. Saved/tracked room identity
                        // still distinguishes it from an ordinary bare contact.
                        if !private_room && !own_sender && !from.contains('/') {
                            if let Some(room) = self.tracked_room_key(&from).map(str::to_string) {
                                return self.update_inner(Message::XmppEvent(xmpp::XmppEvent::RoomMessage {
                                    room, nick: String::new(), body, timestamp, delayed, stanza_id, origin_id,
                                }.with_unstamped_replay(unstamped_replay)));
                            }
                        }
                        // Ordinary own-account traffic is self-chat even if an
                        // earlier stanza claimed this address was a private room.
                        let peer = if own_sender { from.split('/').next().unwrap_or(&from).to_string() }
                            else { let Some(peer) = self.direct_peer_key(&from, private_room) else { return Task::none(); }; peer };
                        let idx = match self.direct_chat_index(&peer) { Some(i) => i, None => self.push_direct_chat(peer) };

                        let conv = self.chats[idx].jid.clone();
                        let self_chat = own_sender && self.own_jid().is_some_and(|own|
                            ::xmpp::jid_matches(&conv, own.split('/').next().unwrap_or(own)));
                        let nick = if self_chat { self.connected_jid.as_deref().and_then(|jid| jid.split('@').next()).unwrap_or("me").to_string() }
                            else { self.chats[idx].title.clone() };
                        let mut pending_copy = None;
                        if self_chat {
                            if let Some(origin) = origin_id.as_deref() {
                                match self.persist_own_direct_copy(&conv, &nick, &body, timestamp, stanza_id.as_deref(), origin, delayed && !unstamped_replay) {
                                    OwnDirectCopy::Stored => return Task::none(),
                                    OwnDirectCopy::Unmatched => {},
                                    OwnDirectCopy::Pending(id) => {
                                        if self.chats[idx].messages.iter().chain(&self.chats[idx].parked_messages)
                                            .any(|message| matches!(message, room::message::Message::Chat { id: existing, .. } if *existing == id)) { return Task::none(); }
                                        pending_copy = Some(id);
                                    }
                                }
                            }
                        }
                        let Some(db_id) = pending_copy.or_else(|| self.persist_message_with_policy(&conv, &nick, &body, timestamp, delayed && !unstamped_replay, stanza_id.as_deref(), false)) else
                        {
                            return Task::none();
                        };

                        let is_active = self.active == Some(Selection::Chat(idx));
                        let user_is_watching = is_active && self.window_focused && !self.join_panel_visible()
                            && self.chats[idx].at_live_tail && self.chats[idx].anchored_bottom;

                        if db_id < 0 || (self.chats[idx].at_live_tail && self.chats[idx].anchored_bottom)
                        {
                            let _ = self.chats[idx].append_live(room::message::Message::Chat
                            {
                                id: db_id,
                                from: nick,
                                body: body.clone(),
                                received: timestamp,
                                status: ChatStatus::Confirmed,
                            });

                            if !user_is_watching
                            {
                                self.chats[idx].unread = true;
                                self.chats[idx].note_unread(db_id, timestamp);
                            }
                        }
                        else
                        {
                            self.chats[idx].at_live_tail = false;
                            self.chats[idx].unread = true;
                            self.chats[idx].note_unread(db_id, timestamp);
                        }

                        // A 1:1 message always warrants a notification when the
                        // window isn't being watched. Use the event body directly
                        // so it works even if the message wasn't appended (user
                        // scrolled into history).
                        log::debug!("Direct message from {}: {}", self.chats[idx].jid,
                            if self.window_focused { "window focused, no alert" } else { "sending alert" });
                        if !self.window_focused
                        {
                            crate::notifications::send(&self.chats[idx].title, &body);
                        }

                        if is_active && !self.join_panel_visible() && self.chats[idx].at_live_tail && self.chats[idx].anchored_bottom
                        {
                            return snap_to_bottom();
                        }
                        return Task::none();
                    }
                    xmpp::XmppEvent::ArchiveMessage { query_id, from, to, body, timestamp, stanza_id, origin_id } =>
                    {
                        let Some(ArchiveContext { conversation: conv, is_muc }) = self.mam_pending.get(&query_id).cloned() else
                        {
                            return Task::none();
                        };

                        // Correlating the archive envelope does not validate
                        // the forwarded message's routing identity.
                        if ::xmpp::validate_message(&from, "").is_err()
                            || to.as_deref().is_some_and(|to| ::xmpp::validate_message(to, "").is_err()) { return Task::none(); }
                        if is_muc && !::xmpp::jid_matches(from.split('/').next().unwrap_or(&from), &conv) {
                            return Task::none();
                        }

                        let own_direct = !is_muc && self.own_jid().is_some_and(|own| ::xmpp::jid_matches(own.split('/').next().unwrap_or(own), from.split('/').next().unwrap_or(&from)));
                        if !is_muc {
                            // Ordinary contacts share a conversation across
                            // resources; room PMs require the exact occupant.
                            let private_room = conv.contains('/');
                            let matches_peer = |jid: &str| ::xmpp::jid_matches(
                                if private_room { jid } else { jid.split('/').next().unwrap_or(jid) }, &conv);
                            let incoming = matches_peer(&from)
                                && to.as_deref().is_none_or(|to| self.own_jid().is_some_and(|own|
                                    ::xmpp::jid_matches(own.split('/').next().unwrap_or(own), to.split('/').next().unwrap_or(to))));
                            // Legacy personal archive results can omit `to`:
                            // retain the server's contact filter in that case.
                            // An explicit recipient must always match the peer.
                            let outgoing = own_direct && to.as_deref().map_or(!private_room, matches_peer);
                            if !incoming && !outgoing { return Task::none(); }
                        }
                        // Derive the display sender only after validating its
                        // relationship to the conversation being queried.
                        let sender = if is_muc
                        {
                            from.split_once('/').map(|(_, nick)| nick).unwrap_or("").to_string()
                        }
                        else
                        {
                            let from_bare = from.split('/').next().unwrap_or(&from);
                            let own_bare = self.own_jid()
                                .map(|j| j.split('/').next().unwrap_or(j))
                                .unwrap_or("");
                            if ::xmpp::jid_matches(from_bare, own_bare)
                            {
                                // Keep the same local display name as existing
                                // history; only wire identity uses the bound JID.
                                self.connected_jid.as_deref()
                                    .and_then(|j| j.split('@').next())
                                    .unwrap_or("me")
                                    .to_string()
                            }
                            else
                            {
                                self.chats.iter().find(|c| c.jid == conv)
                                    .map(|c| c.title.clone())
                                    .unwrap_or_else(|| from_bare.split('@').next().unwrap_or(from_bare).to_string())
                            }
                        };

                        // Persist (deduplicating); the window is updated in bulk
                        // when the query completes.
                        if query_id.starts_with("mam-c-") {
                            if let Some(progress) = self.mam_catchup.get_mut(&conv) {
                                if progress.sync.received.is_some_and(|start| timestamp < start) {
                                    warn!("Archive for {} returned messages before the sync boundary", conv);
                                    self.mam_pending.remove(&query_id);
                                    self.release_failed_archive_query(&query_id, &conv);
                                    return Task::none();
                                }
                                progress.sync.newest = progress.sync.newest.max(Some(timestamp));
                            }
                        }
                        if is_muc && let Some(origin) = &origin_id
                            && self.retire_queued_room_copy(&conv, &sender, &body, origin, timestamp, stanza_id.as_deref()) { return Task::none(); }
                        if own_direct {
                            if let Some(origin) = &origin_id {
                                if !matches!(self.persist_own_direct_copy(&conv, &sender, &body, timestamp, stanza_id.as_deref(), origin, true), OwnDirectCopy::Unmatched) { return Task::none(); }
                            }
                        }
                        let allow_outgoing_fuzzy = own_direct && origin_id.is_none();
                        let _ = self.persist_message_with_policy(&conv, &sender, &body, timestamp, true, stanza_id.as_deref(), allow_outgoing_fuzzy);

                    }
                    xmpp::XmppEvent::ArchiveFailed { query_id, reason } =>
                    {
                        if query_id.starts_with("mam-d-") {
                            if let Some(ArchiveContext { conversation: conv, .. }) = self.mam_pending.remove(&query_id) {
                                warn!("Delivery check for {} failed ({}); sending queued messages again", conv, reason);
                                self.finish_delivery_check(&conv);
                            }
                            return Task::none();
                        }
                        if let Some(ArchiveContext { conversation: conv, .. }) = self.mam_pending.remove(&query_id) {
                            warn!("Archive query for {} failed: {}", conv, reason);
                            // Expired RSM IDs must fall back to the same time
                            // range, rather than retrying an unusable ID forever.
                            if query_id.starts_with("mam-c-") && reason == "archive query rejected by server" {
                                if let Some(history) = &self.history {
                                    if let Ok(mut sync) = history.archive_sync(&conv) {
                                        sync.before = None;
                                        sync.newest = None;
                                        self.save_catchup_progress(&conv, &sync);
                                    }
                                }
                            }
                            self.release_failed_archive_query(&query_id, &conv);
                        }
                    }
                    xmpp::XmppEvent::ArchiveComplete { query_id, complete, first, last } =>
                    {
                        let Some(ArchiveContext { conversation: conv, is_muc }) = self.mam_pending.remove(&query_id) else
                        {
                            return Task::none();
                        };

                        if query_id.starts_with("mam-d-") {
                            let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid).to_string());
                            if self.pending_history_writes.iter().any(|pending| pending.account == account && pending.conversation == conv) {
                                // An archived copy may still be waiting to reconcile.
                                // Check again once history writes recover.
                                self.delivery_checks.remove(&conv);
                                self.request_outbox_replay(if is_muc { Some(conv) } else { None });
                                return Task::none();
                            }
                            let pages = self.delivery_checks.get(&conv).map_or(DELIVERY_CHECK_PAGES, |check| check.pages);
                            match last.filter(|cursor| !complete && !cursor.is_empty() && pages < DELIVERY_CHECK_PAGES) {
                                Some(cursor) => self.send_delivery_check_page(&conv, Some(cursor)),
                                None => self.finish_delivery_check(&conv),
                            }
                            return Task::none();
                        }

                        // Keep the archive cursor unchanged until every received
                        // message for this conversation has a durable copy.
                        let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid).to_string());
                        if self.pending_history_writes.iter().any(|pending| pending.account == account && pending.conversation == conv) {
                            self.release_failed_archive_query(&query_id, &conv);
                            return Task::none();
                        }

                        if query_id.starts_with("mam-c-")
                        {
                            // Finish the entire missing range. Duplicate pages
                            // do not establish coverage of an offline gap.
                            let mut progress = self.mam_catchup.remove(&conv).unwrap_or_default();
                            progress.pages += 1;
                            if !complete {
                                let Some(before) = first.filter(|cursor| !cursor.is_empty() && progress.remember_cursor(cursor)) else {
                                    warn!("Archive catch-up for {} did not advance its cursor; leaving it retryable", conv);
                                    progress.sync.before = None;
                                    progress.sync.newest = None;
                                    self.save_catchup_progress(&conv, &progress.sync);
                                    self.release_failed_archive_query(&query_id, &conv);
                                    // Keep independent backward requests and the
                                    // visible window intact while leaving retryable.
                                    return Task::none();
                                };
                                progress.sync.before = Some(before.clone());
                                if !self.save_catchup_progress(&conv, &progress.sync) {
                                    self.release_failed_archive_query(&query_id, &conv);
                                    return self.pause_catchup(&conv, progress);
                                }
                                if progress.pages >= CATCHUP_MAX_PAGES { return self.pause_catchup(&conv, progress); }
                                self.mam_catchup.insert(conv.clone(), progress);
                                self.send_catchup_page(&conv, is_muc, Some(before));
                                return Task::none();
                            }

                            progress.sync.received = progress.sync.received.max(progress.sync.newest);
                            progress.sync.before = None;
                            progress.sync.newest = None;
                            if !self.save_catchup_progress(&conv, &progress.sync) {
                                self.release_failed_archive_query(&query_id, &conv);
                                return self.pause_catchup(&conv, progress);
                            }
                            // A resumed backwards sweep excluded newer arrivals
                            // while the client was away. Check that small range next.
                            if progress.resumed {
                                progress.resumed = false;
                                return self.pause_catchup(&conv, progress);
                            }

                            self.mam_caught_up.insert(conv.clone());

                            // If parked at the bottom of this conversation, reseed
                            // so the freshly back-filled history shows right away;
                            // otherwise it appears on the next scroll/reselect.
                            if self.active_conv_jid().as_deref() == Some(conv.as_str())
                                && self.active_backlog().is_some_and(|b| b.anchored_bottom())
                            {
                                match self.active
                                {
                                    Some(Selection::Room(i)) => seed_tail_for(&self.history, &conv, &mut self.rooms[i]),
                                    Some(Selection::Chat(i)) => seed_tail_for(&self.history, &conv, &mut self.chats[i]),
                                    None => {}
                                }
                                self.mark_active_read_if_visible();
                                return snap_to_bottom();
                            }
                            return Task::none();
                        }

                        // Backward (scroll-up) paging: pull what was stored into
                        // the window and advance the older-cursor.
                        match self.load_older_into(&conv) {
                            Ok(_) => {},
                            Err(e) => {
                                warn!("Cannot load archive page for {}: {}", conv, e);
                                self.release_failed_archive_query(&query_id, &conv);
                                return Task::none();
                            }
                        }
                        if let Some(b) = self.backlog_by_jid_mut(&conv)
                        {
                            b.set_mam_in_flight(false);
                            let first = first.filter(|cursor| !cursor.is_empty());
                            // Duplicate-only pages can precede older messages.
                            // Exhaustion comes from the archive, not the number
                            // of newly inserted or locally rendered messages.
                            if complete || first.as_ref().is_some_and(|cursor| Some(cursor) != b.mam_cursor().as_ref()) {
                                b.set_mam_cursor(first);
                            } else {
                                warn!("Archive page for {} did not advance its cursor", conv);
                            }
                            b.set_mam_exhausted(complete);
                        }
                    }
                }
            }
            Message::Disconnect =>
            {
                if let Some(channel) = self.xmpp_cmd_rx.as_ref().filter(|_| matches!(self.state, AppState::Connected | AppState::Connecting)).cloned() {
                    self.snapshot_active_draft();
                    self.disconnecting = true;
                    channel.request_stop();
                    return Task::none();
                }
                self.login_form_id = uuid::Uuid::new_v4();
                self.composer_id = uuid::Uuid::new_v4();
                self.finish_login_attempt();
                // Preserve the active conversation's draft across logout.
                self.stash_active_draft();
                self.persist_settings();

                self.state = AppState::Login;
                self.reconnecting = false;
                self.connected_jid = None;
                self.bound_jid = None;
                self.rooms.clear();
                self.end_unqueued_attempts();
                self.cancel_room_operations();
                self.room_connections.clear();
                self.chats.clear();
                self.earlier_chats.clear();
                self.mam_pending.clear();
                self.mam_catchup.clear();
                self.mam_caught_up.clear();
                self.active = None;
                self.message_input = text_editor::Content::new();
                self.input_history = Default::default();
                self.show_join_panel = false;
                self.joining_room = None;
                self.join_error = None;
                self.join_input.clear();
                self.xmpp_cmd_tx = None;
                self.xmpp_pending_commands.clear();
                self.outbox_in_flight.clear();
                self.outbox_replay_pending.clear();
                self.send_error = None;
                self.xmpp_cmd_rx = None;

                return focus_jid_input();
            }
            Message::SetDockIcon =>
            {
                // Runs on the main thread, after launch — see Snack::new.
                #[cfg(target_os = "macos")]
                crate::set_macos_dock_icon();
            }
            Message::ForgetAutoLogin =>
            {
                self.remember_me = false;
                self.saved_login_error = None;
                self.credential_error = storage::remove_auto_login(&mut self.saved_config).err();
            }
            Message::RetryConfigLoad | Message::RecoverConfiguration =>
            {
                if self.state != AppState::Login || self.saved_config.load_error.is_none() { return Task::none(); }
                let (cfg, notice) = if matches!(message, Message::RecoverConfiguration) {
                    match storage::recover_configuration(&self.saved_config) {
                        Ok((cfg, backup)) => (cfg, backup.map(|path| format!("Original settings backed up to {}", path.display()))),
                        Err(error) => { self.config_notice = Some(error); return Task::none(); }
                    }
                } else { (storage::load(), None) };
                self.login_form_id = uuid::Uuid::new_v4();
                self.saved_config = cfg;
                self.config_notice = notice;
                self.settings_save_error = None;
                self.pending_close = None;
                self.jid_input = self.saved_config.jid.clone().unwrap_or_default();
                self.password_input.clear();
                self.remember_me = false;
                self.auto_login_attempt = false;
                self.connect_error = None;
                self.credential_error = None;
                self.saved_login_error = None;
                if !self.saved_config.pending_credential_removals.is_empty() {
                    self.credential_error = storage::retry_credential_removals(&mut self.saved_config).err();
                }
                return self.update_inner(Message::RetrySavedLogin);
            }
            Message::RetrySavedLogin =>
            {
                if self.state != AppState::Login || self.saved_config.load_error.is_some() { return Task::none(); }
                self.saved_login_error = None;
                let Some(jid) = self.saved_config.jid.clone() else { return focus_jid_input(); };
                if self.saved_config.pending_credential_removals.contains(&jid) {
                    self.saved_login_error = Some("Password cleanup is pending for this account. Finish cleanup before enabling auto-login.".into());
                    return Task::none();
                }
                self.remember_me = true;
                match storage::load_password(&jid) {
                    Ok(Some(password)) => {
                        self.login_form_id = uuid::Uuid::new_v4();
                        self.jid_input = jid;
                        self.password_input = password;
                        self.auto_login_attempt = true;
                        // Enter Connecting before another input event can change
                        // the account associated with the restored password.
                        return self.update_inner(Message::Connect);
                    }
                    Ok(None) => { self.auto_login_attempt = false; }
                    Err(error) => {
                        self.auto_login_attempt = false;
                        self.saved_login_error = Some(format!("Could not read the saved password for {jid}. Unlock your keyring and retry saved login, or enter your password. {error}"));
                    }
                }
            }
            Message::RetryCredentialRemoval =>
            {
                self.credential_error = storage::retry_credential_removals(&mut self.saved_config).err();
            }
            Message::ScopedSidebar { channel, profile, action } => {
                if self.state != AppState::Connected || channel != self.xmpp_cmd_rx || profile != self.connected_jid {
                    return Task::none();
                }
                use crate::message::SidebarAction;
                return self.update_inner(match action {
                    SidebarAction::SelectRoom(jid) => Message::SelectRoom(jid),
                    SidebarAction::SelectChat(jid) => Message::SelectChat(jid),
                    SidebarAction::ReopenChat(jid) => Message::ReopenChat(jid),
                    SidebarAction::ToggleEarlierChats => Message::ToggleEarlierChats,
                    SidebarAction::Disconnect => Message::Disconnect,
                    SidebarAction::ForgetAutoLogin => Message::ForgetAutoLogin,
                    SidebarAction::ForceReconnect => Message::ForceReconnect,
                    SidebarAction::ShowJoinPanel => Message::ShowJoinPanel,
                    SidebarAction::RetryCredentialRemoval => Message::RetryCredentialRemoval,
                });
            }
            Message::SelectRoom(jid) =>
            {
                let Some(index) = self.rooms.iter().position(|room| room.jid == jid) else { return Task::none(); };
                let next = Selection::Room(index);
                let changed = self.active != Some(next);
                if changed
                {
                    self.stash_active_draft();
                }

                self.stamp_active_read_marker(Some(next));

                self.active = Some(next);
                self.show_join_panel = false;
                self.find = None;
                // Selection is per-conversation; drop it when switching away.
                if changed
                {
                    self.selected_msgs.clear();
                    self.select_anchor = None;
                    self.body_editors.clear();
                    self.body_drag = None;
                }
                // Re-attach to the live tail and re-bound the window on switch.
                if changed || !self.rooms[index].at_live_tail
                {
                    let conv = self.rooms[index].jid.clone();
                    seed_tail_for(&self.history, &conv, &mut self.rooms[index]);
                    // Backfill any offline gap for this room the first time it is
                    // opened this session.
                    self.start_catchup(&conv);
                }
                self.mark_active_read_if_visible();

                if changed
                {
                    self.load_draft(next);
                    self.persist_settings();
                }

                return Task::batch([snap_to_bottom(), focus_input()]);
            }
            Message::SelectChat(jid) =>
            {
                let Some(index) = self.chats.iter().position(|chat| chat.jid == jid) else { return Task::none(); };
                let next = Selection::Chat(index);
                let changed = self.active != Some(next);
                if changed
                {
                    self.stash_active_draft();
                }

                self.stamp_active_read_marker(Some(next));

                self.active = Some(next);
                self.show_join_panel = false;
                self.find = None;
                // Selection is per-conversation; drop it when switching away.
                if changed
                {
                    self.selected_msgs.clear();
                    self.select_anchor = None;
                    self.body_editors.clear();
                    self.body_drag = None;
                }
                if changed || !self.chats[index].at_live_tail
                {
                    let conv = self.chats[index].jid.clone();
                    seed_tail_for(&self.history, &conv, &mut self.chats[index]);
                    self.start_catchup(&conv);
                }
                self.mark_active_read_if_visible();

                if changed
                {
                    self.load_draft(next);
                    self.persist_settings();
                }

                return Task::batch([snap_to_bottom(), focus_input()]);
            }
            Message::OpenMemberChat { context, nick, jid } =>
            {
                if context != self.composer_id || self.state != AppState::Connected { return Task::none(); }
                let Some(Selection::Room(index)) = self.active else { return Task::none(); };
                let Some(room) = self.rooms.get(index) else { return Task::none(); };
                if !room.users.iter().any(|user| user.name == nick && user.jid.as_deref() == Some(jid.as_str())) {
                    return Task::none();
                }
                return self.update_inner(Message::StartChat(jid));
            }
            Message::ToggleEarlierChats => { self.show_earlier_chats = !self.show_earlier_chats; }
            Message::ReopenChat(jid) =>
            {
                // Reopen the stored conversation key as is: a private room
                // chat must not become the bare room after leaving the room.
                if !self.earlier_chats.iter().any(|chat| chat.conversation == jid)
                    || ::xmpp::validate_message(&jid, "").is_err() { return Task::none(); }
                let idx = match self.direct_chat_index(&jid) { Some(i) => i, None => self.push_direct_chat(jid) };
                return self.update_inner(Message::SelectChat(self.chats[idx].jid.clone()));
            }
            Message::StartChat(jid) =>
            {
                if let Err(error) = ::xmpp::validate_message(&jid, "") {
                    self.send_error = Some(error.to_string());
                    return Task::none();
                }
                let Some(peer) = self.direct_peer_key(&jid, false) else { return Task::none(); };
                let idx = match self.direct_chat_index(&peer) { Some(i) => i, None => self.push_direct_chat(peer) };

                // Activate during this accepted action; a deferred SelectChat
                // could otherwise switch a later account or conversation.
                return self.update_inner(Message::SelectChat(self.chats[idx].jid.clone()));
            }
            Message::ScopedComposer { context, action } => {
                if context != self.composer_id || self.state != AppState::Connected
                    || self.active.is_none() || self.join_panel_visible() {
                    return Task::none();
                }
                // UI callbacks may be queued before a conversation change.
                // Resolve them only in the editor instance that produced them.
                use crate::message::ComposerAction;
                return self.update_inner(match action {
                    ComposerAction::Input(action) => Message::InputAction(action),
                    ComposerAction::Edit(command) => Message::ComposerEdit(command),
                    ComposerAction::Undo => Message::UndoInput,
                    ComposerAction::Redo => Message::RedoInput,
                    ComposerAction::Send => Message::SendMessage,
                    ComposerAction::Complete(backward) => if backward { Message::ShiftTabPressed } else { Message::TabPressed },
                    ComposerAction::Clean(original) => {
                        // Cleanup is consent to edit this exact draft. Normal
                        // typing keeps its composer identity, so compare text too.
                        if self.send_error.is_none() || self.message_input.text().as_str() != original.as_ref() {
                            return Task::none();
                        }
                        Message::CleanMessageInput
                    }
                });
            }
            Message::InputAction(action) =>
            {
                // Shift-click extends the selection from the caret (or the
                // existing selection's anchor) to the click, as in native
                // text fields. Iced's editor treats it as a plain click.
                let action = match action {
                    text_editor::Action::Click(point) if self.shift_down => text_editor::Action::Drag(point),
                    action => action,
                };
                self.clear_body_selection();
                self.selected_msgs.clear();
                self.select_anchor = None;
                let is_edit = action.is_edit();
                self.input_history.perform(&mut self.message_input, action);
                if is_edit
                {
                    self.send_error = None;
                    let text = self.message_input.text();
                    if self.nick_complete.as_ref().is_some_and(|s| s.last_output != text)
                    {
                        self.nick_complete = None;
                    }
                }
            }
            Message::ComposerEdit(command) => { self.composer_edit(command); }
            Message::UndoInput => {
                self.send_error = None;
                self.input_history.undo(&mut self.message_input);
                self.nick_complete = None;
            }
            Message::RedoInput => {
                self.send_error = None;
                self.input_history.redo(&mut self.message_input);
                self.nick_complete = None;
            }
            Message::CleanMessageInput => {
                let clean = ::xmpp::clean_message_text(&self.message_input.text());
                if clean != self.message_input.text() {
                    self.input_history.record(&self.message_input, &text_editor::Action::Edit(text_editor::Edit::Paste(std::sync::Arc::new(clean.clone()))));
                    self.message_input = text_editor::Content::with_text(&clean);
                    self.message_input.perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
                    self.nick_complete = None;
                }
                self.send_error = None;
                return focus_input();
            }
            Message::CopyRejectedSend(token) => {
                let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid).to_string());
                if let Some(send) = self.rejected_sends.iter().find(|send| send.token == token && send.account == account) {
                    return iced::clipboard::write(send.body.clone());
                }
            }
            Message::DiscardRejectedSend(token) => {
                let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid).to_string());
                if let Some(index) = self.rejected_sends.iter().position(|send| send.token == token && send.account == account) {
                    let send = self.rejected_sends.remove(index);
                    self.prune_rejection_retry_cursors();
                    if send.outbox_id > 0 {
                        self.discarded_outbox.insert((account.clone(), send.outbox_id));
                        self.pending_outbox_deletes.insert((account, send.outbox_id));
                        self.retry_outbox_deletes();
                    }
                }
            }
            Message::SendMessage =>
            {
                let raw = self.message_input.text();
                let result = ::xmpp::validate_message_text(&raw).and_then(|_| {
                    if let Some(to) = self.active_conv_jid() { ::xmpp::validate_message(&to, &raw) } else { Ok(()) }
                });
                if let Err(e) = result {
                    self.send_error = Some(e.to_string());
                    return Task::none();
                }
                self.send_error = None;
                let body = raw.trim().to_string();

                if body.is_empty()
                {
                    return Task::none();
                }

                // Whether to attempt an immediate send. When not connected (or
                // mid-reconnect) the message still goes into the persistent outbox
                // and is flushed on the next (re)join/reconnect, so it is never lost.
                let can_send_now = self.xmpp_cmd_tx.is_some()
                    && !self.reconnecting
                    && self.state == AppState::Connected;

                match self.active
                {
                    Some(Selection::Room(index)) =>
                    {
                        let room_jid = self.rooms[index].jid.clone();
                        let can_send_now = can_send_now && self.room_is_joined(&room_jid);

                        // Persist for delivery retries first (flushed on
                        // (re)join). The outbox id correlates the SendResult so the
                        // entry is dropped after server confirmation.
                        let queued = self.enqueue_outbox(&room_jid, true, &body);
                        let outbox_id = queued.as_ref().map(|item| item.id);
                        if outbox_id.is_none() && !can_send_now {
                            warn!("Cannot queue message: persistent outbox unavailable; retaining input");
                            return Task::none();
                        }
                        let origin_id = queued.as_ref().map(|item| item.origin_id.clone()).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                        if can_send_now
                        {
                            if self.xmpp_cmd_tx.is_some()
                            {
                                let sent = self.send_xmpp_command(xmpp::XmppCommand::SendRoomMessage
                                {
                                    id: outbox_id.unwrap_or(0),
                                    origin_id: Some(origin_id.clone()),
                                    room: room_jid.clone(),
                                    body: body.clone(),
                                });
                                if !sent && outbox_id.is_none() { return Task::none(); }
                            }
                        }

                        // Reattach to the live tail so the optimistic message (and
                        // the eventual echo) land in view.
                        if !self.rooms[index].at_live_tail
                        {
                            seed_tail_for(&self.history, &room_jid, &mut self.rooms[index]);
                        }
                        self.rooms[index].set_anchored_bottom(true);

                        // Show it right away as Sending. The server echo confirms it
                        // in place (confirm_pending); the grace timer reveals a
                        // "sending…" badge only if it is slow. It stays queued and is
                        // retried until actually delivered.
                        let own_nick = self.rooms[index].own_nick.clone();
                        let temp_id = self.next_pending_id();
                        self.room_send_origins.insert(temp_id, origin_id);
                        let inserted = self.rooms[index].append_live(room::message::Message::Chat
                        {
                            id: temp_id,
                            from: own_nick,
                            body: body.clone(),
                            received: chrono::Utc::now(),
                            status: ChatStatus::Sending(temp_id),
                        });

                        // Clear the unread marker only after the live tail was
                        // actually loaded. Advance the reconnect divider past
                        // our own message as before.
                        if self.window_focused && !self.join_panel_visible() && self.rooms[index].at_live_tail {
                            self.rooms[index].clear_read_boundary();
                        }
                        self.mark_active_read_if_visible();
                        for marker in self.rooms[index].history_markers.iter_mut()
                        {
                            if inserted == Some(*marker)
                            {
                                *marker += 1;
                            }
                        }

                        self.message_input = text_editor::Content::new();
                        self.input_history = Default::default();
                        self.nick_complete = None;
                        if self.clear_active_draft()
                        {
                            self.persist_settings();
                        }

                        return Task::batch([
                            snap_to_bottom(),
                            focus_input(),
                            delay_then(SEND_PENDING_GRACE, Message::MarkSendPending
                            {
                                conversation: room_jid,
                                temp_id,
                            }),
                        ]);
                    }
                    Some(Selection::Chat(index)) =>
                    {
                        let chat_jid = self.chats[index].jid.clone();

                        let own_nick = self.connected_jid.as_deref()
                            .and_then(|jid| jid.split('@').next()).unwrap_or("me").to_string();
                        let (queued, stored) = self.history.as_ref().and_then(|history| {
                            match history.enqueue_direct_message(&chat_jid, &own_nick, &body) {
                                Ok(queued) => Some(queued),
                                Err(error) => { warn!("Cannot queue outgoing direct message: {}", error); None },
                            }
                        }).unzip();
                        // A history-table failure must not remove durable retry
                        // protection when the outbox itself is still writable.
                        let queued = queued.or_else(|| self.history.as_ref().and_then(|history|
                            history.enqueue_direct_retry(&chat_jid, &own_nick, &body).ok()));
                        let outbox_id = queued.as_ref().map(|item| item.id);
                        if outbox_id.is_none() && !can_send_now {
                            warn!("Cannot queue message: persistent outbox unavailable; retaining input");
                            return Task::none();
                        }
                        let unqueued_attempt = if outbox_id.is_none() { Some((self.next_pending_id(), uuid::Uuid::new_v4().to_string())) } else { None };
                        if can_send_now
                        {
                            if self.xmpp_cmd_tx.is_some()
                            {
                                let sent = self.send_xmpp_command(xmpp::XmppCommand::SendDirectMessage
                                {
                                    id: outbox_id.unwrap_or_else(|| unqueued_attempt.as_ref().unwrap().0),
                                    origin_id: queued.as_ref().map(|item| item.origin_id.clone()).or_else(|| unqueued_attempt.as_ref().map(|(_, origin)| origin.clone())),
                                    to: chat_jid.clone(),
                                    body: body.clone(),
                                });
                                if !sent && outbox_id.is_none() { return Task::none(); }
                            }
                        }

                        let conv = chat_jid.clone();
                        let (db_id, received, status) = if let Some(stored) = stored {
                            (stored.id, stored.received, ChatStatus::from(stored.delivery))
                        } else if let Some(outbox_id) = outbox_id {
                            let received = chrono::Utc::now();
                            let stored = self.history.as_ref().and_then(|history|
                                history.store_direct_message(&conv, &own_nick, &body, received, outbox_id).ok());
                            if let Some(stored) = stored { (stored.id, stored.received, ChatStatus::from(stored.delivery)) }
                            else {
                                let temp_id = self.next_pending_id();
                                let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid).to_string());
                                self.pending_history_writes.push(crate::app::PendingHistoryWrite { receipt: uuid::Uuid::new_v4(),
                                    account, conversation: conv.clone(), sender: own_nick.clone(), body: body.clone(),
                                    received, delayed: false, stanza_id: None, temp_id, outbox_id: Some(outbox_id), own_origin: None, own_room_origin: false, allow_outgoing_fuzzy: true,
                                });
                                (temp_id, received, ChatStatus::Queued(outbox_id))
                            }
                        } else {
                            let received = chrono::Utc::now();
                            let (command_id, origin_id) = unqueued_attempt.unwrap();
                            let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid).to_string());
                            self.unqueued_dm_sends.push(crate::app::UnqueuedDmSend {
                                account, command_id, origin_id, conversation: conv.clone(), sender: own_nick.clone(), body: body.clone(),
                                received, message_id: command_id, state: UnqueuedDelivery::Unconfirmed, dirty: true, awaiting_result: true,
                            });
                            (command_id, received, ChatStatus::Unconfirmed)
                        };

                        // Show our message before trying to reload the tail. A
                        // failed read must not hide text that already left the
                        // composer; seed_tail retains unpersisted copies and
                        // replaces durable ones from the database on success.
                        let was_detached = !self.chats[index].at_live_tail;
                        let inserted = self.chats[index].append_live(room::message::Message::Chat
                        {
                            id: db_id,
                            from: own_nick,
                            body,
                            received,
                            status,
                        });

                        if let (Some(marker), Some(new_index)) = (self.chats[index].read_marker, inserted.filter(|_| !self.chats[index].unread))
                        {
                            if marker <= new_index
                            {
                                self.chats[index].stamp_read_boundary();
                            }
                        }
                        if was_detached
                        {
                            seed_tail_for(&self.history, &conv, &mut self.chats[index]);
                        }
                        self.chats[index].set_anchored_bottom(true);
                        self.mark_active_read_if_visible();

                        self.message_input = text_editor::Content::new();
                        self.input_history = Default::default();
                        self.nick_complete = None;
                        if self.clear_active_draft()
                        {
                            self.persist_settings();
                        }

                        return Task::batch([snap_to_bottom(), focus_input()]);
                    }
                    None => {}
                }
            }
            Message::MarkSendPending { conversation, temp_id } =>
            {
                // Grace period elapsed without an echo: reveal the "sending…" badge.
                // The message stays queued in the outbox and is retried until it is
                // actually delivered, so there is no dead-end "failed" state.
                self.transition_pending(&conversation, temp_id, ChatStatus::Pending(temp_id));
            }
            Message::ShowJoinPanel =>
            {
                self.composer_id = uuid::Uuid::new_v4();
                self.stamp_active_read_marker(None);
                self.show_join_panel = true;
                self.join_input.clear();
                self.join_error = None;
                self.joining_room = None;

                return focus_join_input();
            }
            Message::HideJoinPanel =>
            {
                self.composer_id = uuid::Uuid::new_v4();
                self.show_join_panel = false;
                self.join_error = None;
                // The transcript widget was absent while the panel was open;
                // restore a pending/current search against its new layout.
                if self.find.is_some() { return Task::batch([self.jump_to_current_match(), focus_input()]); }
                self.mark_active_read_if_visible();
                return focus_input();
            }
            Message::ScopedJoinForm { context, action } =>
            {
                if context != self.composer_id || self.state != AppState::Connected
                    || self.joining_room.is_some() || !(self.join_panel_visible() || self.active.is_none()) {
                    return Task::none();
                }
                use crate::message::JoinFormAction;
                return self.update_inner(match action {
                    JoinFormAction::Input(value) => Message::JoinInputChanged(value),
                    JoinFormAction::Save(value) => Message::SaveRoomToggled(value),
                    JoinFormAction::Submit => Message::JoinRoom,
                    JoinFormAction::Cancel => Message::HideJoinPanel,
                });
            }
            Message::JoinInputChanged(value) =>
            {
                self.join_input = value;
            }
            Message::JoinRoom =>
            {
                if self.state != AppState::Connected || self.joining_room.is_some() { return Task::none(); }
                let jid = self.join_input.trim().to_string();

                if !jid.is_empty()
                {
                    // Consume this form even when the attempt fails immediately.
                    // Queued input/submit events must not become a new retry.
                    self.composer_id = uuid::Uuid::new_v4();
                    if let Err(error) = ::xmpp::validate_room_jid(&jid) {
                        self.join_error = Some(error.to_string());
                        return focus_join_input();
                    }
                    let jid = self.tracked_room_key(&jid).unwrap_or(&jid).to_string();
                    // If already in this room, just switch to it.
                    if self.rooms.iter().any(|r| r.jid == jid) && self.room_is_joined(&jid)
                    {
                        self.show_join_panel = false;
                        self.join_input.clear();
                        self.join_error = None;

                        return self.update_inner(Message::SelectRoom(jid));
                    }

                    if self.xmpp_cmd_tx.is_some()
                    {
                        if !self.send_xmpp_command(xmpp::XmppCommand::JoinRoom(jid.clone())) {
                            self.join_error = Some("Connection closed before joining the room".into());
                            return focus_join_input();
                        }
                    } else {
                        self.join_error = Some("Connect before joining a room".into());
                        return focus_join_input();
                    }

                    self.joining_room = Some(jid);
                    self.join_error = None;
                }
            }
            Message::DismissJoinError =>
            {
                self.join_error = None;
                return focus_join_input();
            }
            Message::ScopedRoomRetry(context) => {
                if context != self.room_retry_context(&context.room) { return Task::none(); }
                return self.update_inner(Message::RetryRoomJoin(context.room));
            }
            Message::RetryRoomJoin(jid) => {
                let jid = self.tracked_room_key(&jid).unwrap_or(&jid).to_string();
                if self.state == AppState::Connected && !self.reconnecting
                    && matches!(self.room_connections.get(&jid), Some(RoomConnection::Failed(_))) {
                    if !self.send_xmpp_command(xmpp::XmppCommand::JoinRoom(jid.clone())) {
                        return self.room_join_failed(jid, "Connection closed before joining the room.".into());
                    }
                }
            }
            Message::LeaveRoom =>
            {
                if let Some(Selection::Room(index)) = self.active
                {
                    let room_jid = self.rooms[index].jid.clone();
                    self.room_connections.remove(&room_jid);
                    self.outbox_replay_pending.remove(&Some(room_jid.clone()));

                    if self.xmpp_cmd_tx.is_some()
                    {
                        let nick = self.rooms[index].own_nick.clone();

                        self.send_xmpp_command(xmpp::XmppCommand::LeaveRoom
                        {
                            room: room_jid.clone(),
                            nick,
                        });
                    }

                    // Drop any stashed draft for the room we're leaving.
                    self.saved_config.drafts.remove(&room_jid);

                    // Stop auto-joining this room next time.
                    self.saved_config.rooms.retain(|r| r != &room_jid && !::xmpp::jid_matches(r, &room_jid));
                    self.draft_histories.remove(&room_jid);
                    // Draft removal must be durable even for rooms that were
                    // never saved in the auto-join list.
                    self.persist_settings();

                    // Remove the room immediately rather than waiting for the
                    // server to confirm via XmppEvent::RoomLeft — if the
                    // connection is dropped or the command never reaches the
                    // server, the user is otherwise stuck with a phantom room.
                    // The RoomLeft handler becomes a no-op if the room is
                    // already gone.
                    self.rooms.remove(index);
                    self.active = None;
                    self.message_input = text_editor::Content::new();
                    self.input_history = Default::default();
                }
            }
            Message::CloseChat =>
            {
                if let Some(Selection::Chat(index)) = self.active
                {
                    let chat_jid = self.chats[index].jid.clone();
                    let title = self.chats[index].title.clone();
                    self.chats.remove(index);
                    // Closing hides the conversation; it stays under Earlier.
                    if let Some(history) = &self.history {
                        if let Err(error) = history.record_direct_chat(&chat_jid, &title, false) {
                            warn!("Could not record closed direct conversation: {error}");
                        }
                    }
                    self.refresh_earlier_chats();
                    self.saved_config.drafts.remove(&chat_jid);
                    self.draft_histories.remove(&chat_jid);
                    // The removed index may now name another chat. Clear it
                    // before selecting so we cannot stash the closed draft
                    // under that chat or skip its normal activation logic.
                    self.active = None;
                    self.input_history = Default::default();
                    self.nick_complete = None;
                    if !self.chats.is_empty() {
                        let jid = self.chats[index.saturating_sub(1)].jid.clone();
                        return self.update_inner(Message::SelectChat(jid));
                    }
                    if let Some(room) = self.rooms.first() {
                        return self.update_inner(Message::SelectRoom(room.jid.clone()));
                    }
                    self.message_input = text_editor::Content::new();
                    self.find = None;
                    self.send_error = None;
                    self.persist_settings();
                }
            }
            Message::CloseConversation { context } => {
                if context != self.composer_id || self.state != AppState::Connected || self.join_panel_visible() {
                    return Task::none();
                }
                return self.update_inner(Message::LeaveSelection);
            }
            Message::CloseShortcut { context } => {
                // Cmd/Ctrl-W closes a direct conversation. Leaving a room is
                // deliberate and needs its Leave button.
                if context != self.composer_id || self.state != AppState::Connected || self.join_panel_visible()
                    || !matches!(self.active, Some(Selection::Chat(_))) { return Task::none(); }
                return self.update_inner(Message::CloseChat);
            }
            Message::FocusComposer { context } => {
                if context != self.composer_id || self.active.is_none() || self.join_panel_visible() { return Task::none(); }
                return focus_input();
            }
            Message::LeaveSelection =>
            {
                match self.active
                {
                    Some(Selection::Room(_)) => return self.update(Message::LeaveRoom),
                    Some(Selection::Chat(_)) => return self.update(Message::CloseChat),
                    _ => {}
                }
            }
            Message::OpenUrl(url) =>
            {
                // A desktop launcher may wait for the browser to exit. Detach
                // it so a link click cannot stall the UI and message handling.
                if let Err(e) = open::that_detached(&url)
                {
                    error!("Failed to open URL {}: {}", url, e);
                }
                // Clicking the link unfocuses the message input; restore it so
                // the user can keep typing without a manual click after the
                // browser steals window focus.
                if self.is_message_input_context()
                {
                    return focus_input();
                }
            }
            Message::ModifiersUpdate { shift, primary } =>
            {
                self.shift_down = shift;
                self.cmd_down = primary;
            }
            Message::ScopedTranscript { context, action } => {
                if context != self.composer_id || self.state != AppState::Connected
                    || self.active.is_none() || self.join_panel_visible() {
                    return Task::none();
                }
                use crate::message::TranscriptAction;
                return self.update_inner(match action {
                    TranscriptAction::Select(id) => Message::SelectMessage { id },
                    TranscriptAction::Quote(id) => Message::QuoteMessage { id },
                    TranscriptAction::Editor { id, action } => Message::BodyEditorAction { id, action },
                    TranscriptAction::Drag { id, point } => Message::BodyDrag { id, point },
                    TranscriptAction::Copy(selection) => {
                        if selection != self.selection_id { return Task::none(); }
                        Message::CopySelection
                    }
                    TranscriptAction::Clear(selection) => {
                        if selection != self.selection_id { return Task::none(); }
                        Message::ClearSelection
                    }
                    TranscriptAction::Scrolled { search, viewport } => {
                        if search != self.find.as_ref().map(|find| find.scroll_id) { return Task::none(); }
                        Message::MessagesScrolled(viewport)
                    }
                });
            }
            Message::SelectMessage { id } =>
            {
                self.clear_body_selection();
                self.toggle_message_selection(id);
            }
            Message::QuoteMessage { id } =>
            {
                let body = self.active_backlog().and_then(|b| b.messages().iter().find_map(|m| match m {
                    room::message::Message::Chat { id: mid, body, .. } if *mid == id => Some(body.clone()),
                    _ => None,
                }));
                let Some(body) = body else { return Task::none(); };
                let current = self.message_input.text();
                let separator = if current.is_empty() || current.ends_with('\n') { "" } else { "\n" };
                let quote = body.split('\n')
                    .map(|line| format!("> {}", line.trim_end_matches('\r')))
                    .collect::<Vec<_>>().join("\n");
                let action = text_editor::Action::Edit(text_editor::Edit::Paste(
                    std::sync::Arc::new(format!("{separator}{quote}\n")),
                ));
                self.input_history.break_group();
                self.input_history.record(&self.message_input, &action);
                self.message_input.perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
                self.message_input.perform(action);
                self.input_history.break_group();
                self.nick_complete = None;
                self.clear_body_selection();
                self.selected_msgs.clear();
                self.select_anchor = None;
                return focus_input();
            }
            Message::EndBodyDrag =>
            {
                self.body_drag = None;
                if let Some(url) = self.take_link_click() {
                    return self.update_inner(Message::OpenUrl(url));
                }
            }
            Message::BodyDrag { id, point } => { self.extend_body_selection(id, point); }
            Message::BodyEditorAction { id, action } =>
            {
                if matches!(action, text_editor::Action::Click(_)) { self.link_press = None; }
                if self.cmd_down && matches!(action, text_editor::Action::Click(_)) {
                    self.clear_body_selection();
                    self.toggle_message_selection(id);
                    return Task::none();
                }
                if matches!(action, text_editor::Action::SelectWord) && !self.cmd_down {
                    return self.update_inner(Message::QuoteMessage { id });
                }
                if action.is_edit() || matches!(action, text_editor::Action::Drag(_)) {
                    return Task::none();
                }
                if matches!(action, text_editor::Action::Click(_)) {
                    self.selected_msgs.clear();
                    self.select_anchor = None;
                    self.clear_body_selection();
                    if let Some(content) = self.body_editors.get_mut(&id) {
                        content.perform(action);
                        self.body_drag = Some((id, content.cursor()));
                        let position = content.cursor().position;
                        self.link_press = url_at(&content.text(), position).map(|url| (id, url));
                        log::debug!("Body click in message {id} at {position:?}; link: {:?}",
                            self.link_press.as_ref().map(|(_, url)| url));
                    }
                } else if let Some(content) = self.body_editors.get_mut(&id) {
                    content.perform(action);
                }
            }
            Message::ClearSelection =>
            {
                self.clear_body_selection();
                self.selected_msgs.clear();
                self.select_anchor = None;
            }
            Message::CopySelection =>
            {
                if let Some(text) = self.selected_message_text()
                {
                    self.clear_body_selection();
                    self.selected_msgs.clear();
                    self.select_anchor = None;
                    return Task::batch([iced::clipboard::write(text), focus_input()]);
                }
                // Free text copying keeps its normal substring behavior.
                if let Some(b) = self.active_backlog() {
                    let parts: Vec<String> = b.messages().iter().filter_map(|m| match m {
                        room::message::Message::Chat { id, .. } => self.body_editors.get(id).and_then(|c| c.selection()),
                        _ => None,
                    }).collect();
                    if !parts.is_empty() { return iced::clipboard::write(parts.join("\n")); }
                }
                return Task::none();
            }
            Message::WindowUnfocused =>
            {
                self.window_focused = false;
                self.body_drag = None;
                self.cmd_down = false;
                self.shift_down = false;

                // Stamp the read marker on the active room/chat so messages
                // arriving while the window is unfocused are flagged as new.
                // Non-active rooms/chats already have their marker set from
                // when the user switched away — don't clobber it.
                match self.active
                {
                    Some(Selection::Room(idx)) =>
                    {
                        if let Some(r) = self.rooms.get_mut(idx)
                        {
                            if !r.unread && r.at_live_tail && r.anchored_bottom { r.stamp_read_boundary(); }
                        }
                    }
                    Some(Selection::Chat(idx)) =>
                    {
                        if let Some(c) = self.chats.get_mut(idx)
                        {
                            if !c.unread && c.at_live_tail && c.anchored_bottom { c.stamp_read_boundary(); }
                        }
                    }
                    None => {}
                }
            }
            Message::ActivateWindow =>
            {
                return iced::window::oldest().then(|window| match window {
                    Some(id) => Task::batch([iced::window::minimize(id, false), iced::window::gain_focus(id)]),
                    None => Task::none(),
                });
            }
            Message::WindowFocused =>
            {
                self.window_focused = true;

                // Focusing an old history window does not display new messages.
                // Keep the divider, even when the visible tail clears the badge.
                self.mark_active_read_if_visible();
            }
            Message::WindowCloseRequested(id) =>
            {
                if self.pending_close.is_some_and(|close| close.draining) { return Task::none(); }
                // An unreadable startup file has not admitted any login or
                // editing. Closing its recovery screen must remain possible.
                if self.saved_config.load_error.is_some() && self.active.is_none() && !self.has_unsaved_message_updates() {
                    self.pending_close = None;
                    return iced::window::close(id);
                }
                self.snapshot_active_draft();
                if self.has_unsaved_message_updates() {
                    self.retry_history_writes();
                    self.retry_send_rejections();
                    self.retry_outbox_deletes();
                }
                let saved = self.persist_settings();
                if saved && !self.has_unsaved_message_updates() {
                    return self.begin_window_shutdown(id, false);
                }
                self.pending_close = Some(crate::app::PendingClose { window: id, token: uuid::Uuid::new_v4(), draining: false, discard: false });
            }
            Message::SaveNotice { prompt, action } =>
            {
                // A queued callback must still belong to the displayed close
                // attempt. In particular, ordinary Retry saving cannot become
                // Retry save and close when a close request arrives later.
                if prompt != self.pending_close.map(|close| close.token) { return Task::none(); }
                use crate::message::SaveNoticeAction;
                return self.update_inner(match action {
                    SaveNoticeAction::Retry => Message::RetrySettingsSave,
                    SaveNoticeAction::KeepOpen => Message::KeepWindowOpen,
                    SaveNoticeAction::Discard => Message::CloseWithoutSaving,
                });
            }
            Message::RetrySettingsSave =>
            {
                if let Some(close) = self.pending_close { return self.update_inner(Message::WindowCloseRequested(close.window)); }
                self.snapshot_active_draft();
                self.persist_settings();
            }
            Message::KeepWindowOpen => { self.pending_close = None; }
            Message::CloseWithoutSaving =>
            {
                // Only an explicit action following a failed close can bypass
                // persistence. A stale button event cannot close a kept window.
                if let Some(close) = self.pending_close.take() { return self.begin_window_shutdown(close.window, true); }
            }
            Message::SearchViewport { token, at_bottom } => {
                let conversation = self.active_conv_jid();
                let Some(find) = self.find.as_mut() else { return Task::none(); };
                if find.pending_scroll != Some(token)
                    || find.matches.get(find.index).map(|hit| hit.conversation.as_str()) != conversation.as_deref() {
                    return Task::none();
                }
                find.pending_scroll = None;
                // Fitting an old slice is not an instruction to abandon the
                // hit and load today's tail. Only a slice that contains the
                // newest row can establish that the live bottom is visible.
                let contains_tail = match self.active {
                    Some(Selection::Room(i)) => self.rooms[i].at_live_tail,
                    Some(Selection::Chat(i)) => self.chats[i].at_live_tail,
                    None => false,
                };
                return self.update_scroll_position(at_bottom && contains_tail, false);
            }
            Message::MessagesScrolled(viewport) =>
            {
                if let Some(find) = self.find.as_mut() { find.pending_scroll = None; }
                // Absolute geometry also handles an undefined relative offset
                // when content and viewport heights are equal.
                let overflow = viewport.content_bounds().height - viewport.bounds().height;
                let offset = viewport.absolute_offset().y;
                let at_bottom = overflow <= 0.0 || offset <= overflow * 0.02;
                let near_top = overflow > 0.0 && offset >= overflow * 0.92;
                return self.update_scroll_position(at_bottom, near_top);
            }
            Message::ScopedFind { context, action } => {
                if self.state != AppState::Connected || self.active.is_none() || self.join_panel_visible() {
                    return Task::none();
                }
                let Some(find) = self.find.as_ref().filter(|find| find.id == context) else { return Task::none(); };
                use crate::message::FindAction;
                let message = match action {
                    FindAction::Input(value) => Message::FindInputChanged(value),
                    FindAction::Scope(value) => Message::FindScopeToggled(value),
                    FindAction::Next => Message::FindNext,
                    FindAction::Prev => Message::FindPrev,
                    FindAction::Close => Message::CloseFind,
                    FindAction::Retry => Message::FindInputChanged(find.query.clone()),
                    FindAction::Submit => if find.error.is_some() {
                        Message::FindInputChanged(find.query.clone())
                    } else { Message::FindNext },
                };
                return self.update_inner(message);
            }
            Message::ToggleFind =>
            {
                if self.active.is_none()
                {
                    return Task::none();
                }
                if self.find.is_some() { return focus_find_input(); }
                self.find = Some(crate::app::FindState::default());
                return focus_find_input();
            }
            Message::CloseFind =>
            {
                if self.find.take().is_some()
                {
                    return focus_input();
                }
                self.clear_body_selection();
                // Esc maps here too: with the find bar already closed, fall back
                // to clearing any message selection.
                if !self.selected_msgs.is_empty()
                {
                    self.selected_msgs.clear();
                    self.select_anchor = None;
                    self.body_editors.clear();
                    self.body_drag = None;
                    return focus_input();
                }
            }
            Message::FindInputChanged(value) =>
            {
                if let Some(find) = self.find.as_mut()
                {
                    find.query = value;
                }
                self.run_find_search();
                let has = self.find.as_ref().is_some_and(|f| !f.matches.is_empty());
                if has
                {
                    return self.jump_to_current_match();
                }
            }
            Message::FindScopeToggled(all) =>
            {
                if let Some(find) = self.find.as_mut()
                {
                    find.all_scope = all;
                }
                self.run_find_search();
                let has = self.find.as_ref().is_some_and(|f| !f.matches.is_empty());
                if has
                {
                    return self.jump_to_current_match();
                }
                return focus_find_input();
            }
            Message::FindNext =>
            {
                let jump = match self.find.as_mut()
                {
                    Some(find) if !find.matches.is_empty() =>
                    {
                        find.index = (find.index + 1) % find.matches.len();
                        true
                    }
                    _ => false,
                };
                if jump
                {
                    return self.jump_to_current_match();
                }
            }
            Message::FindPrev =>
            {
                let jump = match self.find.as_mut()
                {
                    Some(find) if !find.matches.is_empty() =>
                    {
                        find.index = if find.index == 0 { find.matches.len() - 1 } else { find.index - 1 };
                        true
                    }
                    _ => false,
                };
                if jump
                {
                    return self.jump_to_current_match();
                }
            }
        }

        return Task::none();
    }
}

#[cfg(test)]
#[path = "update_tests.rs"]
mod tests;
