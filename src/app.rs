use iced::{ Task, Theme };
use iced::widget::{ text_editor, Id };

use crate::message::Message;
use crate::{ room, storage, xmpp };

pub const MESSAGE_SCROLL_ID: &str = "message_scroll";
pub const MESSAGE_INPUT_ID: &str = "message_input";
pub const JOIN_INPUT_ID: &str = "join_input";
pub const ACCOUNT_JID_INPUT_ID: &str = "account_jid_input";
pub const ACCOUNT_PASSWORD_INPUT_ID: &str = "account_password_input";
pub const FIND_INPUT_ID: &str = "find_input";

pub(crate) fn focus_find_input() -> Task<Message>
{
    iced::widget::operation::focus(Id::new(FIND_INPUT_ID))
}

pub(crate) fn focus_jid_input() -> Task<Message>
{
    iced::widget::operation::focus(Id::new(ACCOUNT_JID_INPUT_ID))
}

pub(crate) fn focus_join_input() -> Task<Message>
{
    iced::widget::operation::focus(Id::new(JOIN_INPUT_ID))
}

pub(crate) fn focus_input() -> Task<Message>
{
    iced::widget::operation::focus(Id::new(MESSAGE_INPUT_ID))
}

pub(crate) fn snap_to_bottom() -> Task<Message>
{
    // The message list is bottom-anchored (see ui::chat), which inverts relative
    // offsets: y = 0.0 renders as the bottom (newest). So snapping to the newest
    // message targets the start offset, not the end.
    iced::widget::operation::snap_to(
        Id::new(MESSAGE_SCROLL_ID),
        iced::widget::scrollable::RelativeOffset { x: 0.0, y: 0.0 },
    )
}

// Iced uses its Tokio executor (enabled in Cargo.toml), so timers can be
// awaited directly without spawning a sleeping thread for every outgoing message.
pub(crate) fn delay_then(delay: std::time::Duration, message: Message) -> Task<Message>
{
    Task::perform(async move { tokio::time::sleep(delay).await; }, move |_| message.clone())
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingClose {
    pub window: iced::window::Id,
    pub token: uuid::Uuid,
    pub draining: bool,
    pub discard: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AppState
{
    Login,
    Connecting,
    Connected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RoomConnection { Joining, Joined, Failed(String) }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection
{
    Room(usize),
    Chat(usize),
}

#[derive(Debug, Clone)]
pub struct NickCompleteState
{
    pub prefix_start: text_editor::Position,
    pub prefix: String,
    pub nick: String,
    pub last_output: String,
    pub last_cursor: text_editor::Cursor,
}

// One search hit: which conversation it belongs to and its history DB row id.
#[derive(Debug, Clone)]
pub struct FindMatch
{
    pub conversation: String,
    pub id: i64,
    pub received: chrono::DateTime<chrono::Utc>,
}

// State of the in-room find bar (present only while the bar is open).
#[derive(Debug, Clone)]
pub struct FindState
{
    pub id: uuid::Uuid,
    pub query: String,
    pub error: Option<String>,
    // false = search only the active conversation, true = all open conversations.
    pub all_scope: bool,
    // Hits ordered oldest-first; `index` is the currently-focused one.
    pub matches: Vec<FindMatch>,
    pub index: usize,
    // Only the latest pending search scroll may report its layout result.
    pub pending_scroll: Option<uuid::Uuid>,
    // Retained after completion so old geometry cannot affect a later jump.
    pub scroll_id: uuid::Uuid,
}

impl Default for FindState {
    fn default() -> Self {
        Self { id: uuid::Uuid::new_v4(), query: String::new(), error: None,
            all_scope: false, matches: Vec::new(), index: 0, pending_scroll: None, scroll_id: uuid::Uuid::new_v4() }
    }
}

pub(crate) struct PendingHistoryWrite
{
    pub receipt: uuid::Uuid,
    pub account: Option<String>,
    pub conversation: String,
    pub sender: String,
    pub body: String,
    pub received: chrono::DateTime<chrono::Utc>,
    // Timestamp provenance for storage; a replay marker alone is insufficient.
    pub delayed: bool,
    pub stanza_id: Option<String>,
    pub temp_id: i64,
    pub outbox_id: Option<i64>,
    pub own_origin: Option<String>,
    pub own_room_origin: bool,
    pub allow_outgoing_fuzzy: bool,
}

pub(crate) struct UnqueuedDmSend
{
    pub account: Option<String>,
    pub command_id: i64,
    pub origin_id: String,
    pub conversation: String,
    pub sender: String,
    pub body: String,
    pub received: chrono::DateTime<chrono::Utc>,
    pub message_id: i64,
    pub state: crate::storage::history::UnqueuedDelivery,
    pub dirty: bool,
    pub awaiting_result: bool,
}

pub(crate) struct RejectedSend
{
    pub token: uuid::Uuid,
    pub account: Option<String>,
    pub outbox_id: i64,
    pub conversation: String,
    pub body: String,
    pub reason: String,
    pub saved: bool,
}

pub(crate) struct PendingMessageError
{
    pub account: Option<String>,
    // Bare authenticated identity when the report arrived, before any rebind.
    pub own_jid: String,
    pub origin_id: String,
    pub from: Option<String>,
    pub reason: String,
}

#[derive(Debug)]
pub(crate) struct DeliveryCheck
{
    pub is_room: bool,
    pub start: String,
    pub pages: u32,
}

// Preserve query routing independently of sidebar membership while results
// and subsequent pages are still in flight.
#[derive(Clone)]
pub(crate) struct ArchiveContext
{
    pub conversation: String,
    pub is_muc: bool,
}

#[derive(Default)]
pub(crate) struct CatchupProgress {
    pub pages: u32,
    pub sync: crate::storage::history::ArchiveSync,
    pub resumed: bool,
    pub continuation: Option<uuid::Uuid>,
    // Retain fixed-size fingerprints rather than a sweep's worth of arbitrary
    // server strings. The batch limit bounds the number retained.
    cursors: std::collections::HashSet<[u8; 32]>,
}

impl CatchupProgress {
    pub fn next_batch(&mut self) {
        self.pages = 0;
        self.cursors.clear();
        if let Some(before) = self.sync.before.clone() { self.remember_cursor(&before); }
    }
    pub fn remember_cursor(&mut self, cursor: &str) -> bool {
        use sha2::Digest;
        self.cursors.insert(sha2::Sha256::digest(cursor.as_bytes()).into())
    }
}

pub struct Snack
{
    pub(crate) state: AppState,
    pub(crate) reconnecting: bool,
    pub(crate) jid_input: String,
    pub(crate) login_form_id: uuid::Uuid,
    pub(crate) password_input: String,
    // Login profile owns persisted history, drafts and credentials.
    pub(crate) connected_jid: Option<String>,
    // Server-authoritative identity for wire data and own-message labels.
    pub(crate) bound_jid: Option<String>,
    pub(crate) connect_error: Option<String>,
    pub(crate) credential_error: Option<String>,
    pub(crate) saved_login_error: Option<String>,
    pub(crate) config_notice: Option<String>,
    pub(crate) settings_save_error: Option<String>,
    pub(crate) pending_close: Option<PendingClose>,
    // Retain the account and incoming subscription through native close.
    pub(crate) disconnecting: bool,
    pub(crate) send_error: Option<String>,
    pub(crate) rejected_sends: Vec<RejectedSend>,
    // Retry positions follow stable notice identities within each account.
    pub(crate) rejected_retry_cursors: std::collections::HashMap<Option<String>, uuid::Uuid>,
    pub(crate) detached_retry_after: Option<String>,
    pub(crate) pending_message_errors: Vec<PendingMessageError>,
    pub(crate) rooms: Vec<room::Room>,
    pub(crate) chats: Vec<room::chat::Chat>,
    pub(crate) active: Option<Selection>,
    pub(crate) message_input: text_editor::Content,
    // Stable across typing/sends; renewed when the conversation editor changes.
    pub(crate) composer_id: uuid::Uuid,
    pub(crate) input_history: crate::edit_history::EditHistory,
    pub(crate) draft_histories: std::collections::HashMap<String, crate::edit_history::EditHistory>,
    pub(crate) body_drag: Option<(i64, text_editor::Cursor)>,
    // A press on a URL in a message body (row id, URL); it opens on release
    // unless the press turned into a text selection.
    pub(crate) link_press: Option<(i64, String)>,
    // Closed direct conversations, most recent first, and whether the
    // sidebar's Earlier list is expanded.
    pub(crate) earlier_chats: Vec<crate::storage::history::DirectChat>,
    pub(crate) show_earlier_chats: bool,
    pub(crate) show_join_panel: bool,
    pub(crate) joining_room: Option<String>,
    pub(crate) join_error: Option<String>,
    pub(crate) room_connections: std::collections::BTreeMap<String, RoomConnection>,
    pub(crate) room_operations: std::collections::HashMap<String, xmpp::RoomOperation>,
    pub(crate) join_input: String,
    pub(crate) xmpp_cmd_tx: Option<tokio::sync::mpsc::Sender<xmpp::XmppCommand>>,
    pub(crate) outbox_in_flight: std::collections::HashSet<i64>,
    // Confirmed or explicitly discarded rows whose removal needs retrying.
    // Account scope keeps equal row IDs in different databases independent.
    pub(crate) pending_outbox_deletes: std::collections::HashSet<(Option<String>, i64)>,
    pub(crate) outbox_delete_retry_after: std::collections::HashMap<Option<String>, i64>,
    pub(crate) discarded_outbox: std::collections::HashSet<(Option<String>, i64)>,
    // Replay triggers for this connection: None for DMs, Some(jid) for a
    // confirmed room join. Retain them while the database cannot be read.
    pub(crate) outbox_replay_pending: std::collections::HashSet<Option<String>>,
    // Rooms whose join history (presence, history, then subject) is still
    // arriving. Their replay waits for the subject so that history can prove
    // an unacknowledged send was already delivered; the deadline covers
    // services that never send a subject.
    pub(crate) room_history_pending: std::collections::HashMap<String, std::time::Instant>,
    // Archive lookups that run before queued sends are replayed: an attempt
    // whose acknowledgement was lost may already be delivered. Keyed by
    // conversation; in flight while a `mam-d-` query for it is pending.
    pub(crate) delivery_checks: std::collections::HashMap<String, DeliveryCheck>,
    // Sends handed to the connection whose durable attempt mark failed.
    pub(crate) attempted_outbox: std::collections::HashSet<i64>,
    pub(crate) xmpp_pending_commands: std::collections::VecDeque<xmpp::XmppCommand>,
    pub(crate) xmpp_cmd_rx: Option<xmpp::CommandChannel>,
    pub(crate) remember_me: bool,
    pub(crate) save_room: bool,
    pub(crate) saved_config: storage::SavedConfig,
    pub(crate) pending_save_password: Option<String>,
    pub(crate) auto_login_attempt: bool,
    pub(crate) nick_complete: Option<NickCompleteState>,
    pub(crate) window_focused: bool,
    // Persistent, searchable chat-log store. None if the DB could not be opened,
    // in which case the app degrades to its old in-memory-only behaviour.
    pub(crate) history: Option<storage::history::History>,
    pub(crate) pending_history_writes: Vec<PendingHistoryWrite>,
    pub(crate) room_send_origins: std::collections::HashMap<i64, String>,
    pub(crate) unqueued_dm_sends: Vec<UnqueuedDmSend>,
    // Find bar; Some only while open over the active conversation.
    pub(crate) find: Option<FindState>,
    // Decreasing counter handing out negative temporary ids for optimistically-
    // shown outgoing room messages, until the server echo gives them their place.
    pub(crate) pending_seq: i64,
    // Message selection for copy-to-clipboard. Holds the history row ids of the
    // chat messages selected in the *active* conversation; cleared whenever the
    // active conversation changes. `select_anchor` is the pivot for shift-click
    // range selection (the last message toggled on).
    pub(crate) selected_msgs: std::collections::HashSet<i64>,
    pub(crate) selection_id: uuid::Uuid,
    pub(crate) select_anchor: Option<i64>,
    // Whether Shift is currently held, tracked from keyboard modifier events.
    // `mouse_area::on_press` carries no modifier info, so a Shift+click range
    // selection consults this instead.
    pub(crate) shift_down: bool,
    // Whether the primary modifier (Cmd on macOS / Ctrl elsewhere) is currently
    // held. A plain click on a message body now drags a free-text selection, so
    // the old whole-message selection mode is gated behind this modifier.
    pub(crate) cmd_down: bool,
    // Per-message read-only `text_editor` content, keyed by the chat message's
    // history row id. Each body is rendered through its own editor so the user
    // can drag-select arbitrary text within it; the selection state lives here
    // (mutated in `update`, never `view`) so it survives across renders.
    // Reconciled against the active conversation by `sync_body_editors`.
    pub(crate) body_editors: std::collections::HashMap<i64, text_editor::Content>,
    // MAM (server archive) paging: a monotonic counter for query ids and a map
    // from in-flight query id to its conversation and original archive type.
    pub(crate) mam_seq: u64,
    pub(crate) mam_pending: std::collections::HashMap<String, ArchiveContext>,
    // Catch-up sync state: completed page count for each active sweep.
    // The sweep walks the
    // recent archive backwards from the present to completion or a bounded page
    // limit, including all-duplicate pages preceding an offline gap. `mam_caught_up`
    // marks conversations swept this session so re-selecting doesn't re-sync.
    pub(crate) mam_catchup: std::collections::HashMap<String, CatchupProgress>,
    pub(crate) mam_caught_up: std::collections::HashSet<String>,
}

impl Snack
{
    pub(crate) fn navigation_context(&self) -> crate::message::NavigationContext {
        crate::message::NavigationContext { channel: self.xmpp_cmd_rx.clone(), profile: self.connected_jid.clone() }
    }

    pub(crate) fn shortcut_context(&self) -> crate::message::ShortcutContext {
        crate::message::ShortcutContext { view: self.composer_id, search: self.find.as_ref().map(|find| find.id), selection: self.selection_id }
    }

    pub(crate) fn focus_context(&self) -> crate::message::FocusContext {
        crate::message::FocusContext {
            login: self.login_form_id, view: self.composer_id, phase: self.state.clone(),
            search: self.find.as_ref().map(|find| find.id), join: self.join_panel_visible(),
            selected: !self.selected_msgs.is_empty(),
        }
    }

    pub(crate) fn room_retry_context(&self, room: &str) -> crate::message::RoomRetryContext {
        crate::message::RoomRetryContext {
            channel: self.xmpp_cmd_rx.clone(),
            profile: self.connected_jid.clone(),
            room: room.to_string(),
            operation: self.room_operations.get(room).map(|operation| operation.id.clone()),
        }
    }

    pub(crate) fn join_panel_visible(&self) -> bool {
        self.show_join_panel || self.joining_room.is_some() || self.join_error.is_some()
    }

    // Saved and requested rooms remain visible before entry confirmation.
    // Prefer the registered key and avoid duplicate rows for legacy aliases.
    pub(crate) fn unopened_room_jids(&self) -> Vec<&str> {
        let mut rooms: Vec<&str> = Vec::new();
        for jid in self.room_connections.keys().chain(self.saved_config.rooms.iter()) {
            if !self.rooms.iter().any(|room| ::xmpp::jid_matches(&room.jid, jid))
                && !rooms.iter().any(|existing| ::xmpp::jid_matches(existing, jid)) {
                rooms.push(jid);
            }
        }
        rooms
    }

    #[cfg(test)]
    pub(crate) fn test_state() -> Self
    {
        Self
        {
            state: AppState::Login,
            reconnecting: false,
            jid_input: String::new(),
            login_form_id: uuid::Uuid::new_v4(),
            password_input: String::new(),
            connected_jid: None,
            bound_jid: None,
            connect_error: None,
            credential_error: None,
            saved_login_error: None,
            config_notice: None,
            settings_save_error: None,
            pending_close: None,
            disconnecting: false,
            send_error: None,
            rejected_sends: Vec::new(),
            rejected_retry_cursors: Default::default(),
            detached_retry_after: None,
            pending_message_errors: Vec::new(),
            rooms: Vec::new(),
            chats: Vec::new(),
            active: None,
            message_input: text_editor::Content::new(),
            composer_id: uuid::Uuid::new_v4(),
            input_history: Default::default(),
            draft_histories: Default::default(),
            body_drag: None,
            link_press: None,
            earlier_chats: Vec::new(),
            show_earlier_chats: false,
            show_join_panel: false,
            joining_room: None,
            join_error: None,
            room_connections: Default::default(),
            room_operations: Default::default(),
            join_input: String::new(),
            xmpp_cmd_tx: None,
            xmpp_pending_commands: Default::default(),
            outbox_in_flight: Default::default(),
            pending_outbox_deletes: Default::default(),
            outbox_delete_retry_after: Default::default(),
            discarded_outbox: Default::default(),
            outbox_replay_pending: Default::default(),
            room_history_pending: Default::default(),
            delivery_checks: Default::default(),
            attempted_outbox: Default::default(),
            xmpp_cmd_rx: None,
            remember_me: false,
            save_room: false,
            saved_config: Default::default(),
            pending_save_password: None,
            auto_login_attempt: false,
            nick_complete: None,
            window_focused: true,
            history: None,
            pending_history_writes: Vec::new(),
            room_send_origins: std::collections::HashMap::new(),
            unqueued_dm_sends: Vec::new(),
            find: None,
            pending_seq: -1,
            selected_msgs: std::collections::HashSet::new(),
            selection_id: uuid::Uuid::new_v4(),
            select_anchor: None,
            shift_down: false,
            cmd_down: false,
            body_editors: std::collections::HashMap::new(),
            mam_seq: 0,
            mam_pending: std::collections::HashMap::new(),
            mam_catchup: std::collections::HashMap::new(),
            mam_caught_up: std::collections::HashSet::new(),
        }
    }

    pub(crate) fn new() -> (Self, Task<Message>)
    {
        let mut saved_config = storage::load();
        let credential_error = if saved_config.pending_credential_removals.is_empty() { None }
            else { storage::retry_credential_removals(&mut saved_config).err() };

        let mut snack = Self
        {
            state: AppState::Login,
            reconnecting: false,
            jid_input: saved_config.jid.clone().unwrap_or_default(),
            login_form_id: uuid::Uuid::new_v4(),
            password_input: String::new(),
            connected_jid: None,
            bound_jid: None,
            connect_error: None,
            credential_error,
            saved_login_error: None,
            config_notice: None,
            settings_save_error: None,
            pending_close: None,
            disconnecting: false,
            send_error: None,
            rejected_sends: Vec::new(),
            rejected_retry_cursors: Default::default(),
            detached_retry_after: None,
            pending_message_errors: Vec::new(),
            rooms: Vec::new(),
            chats: Vec::new(),
            active: None,
            message_input: text_editor::Content::new(),
            composer_id: uuid::Uuid::new_v4(),
            input_history: Default::default(),
            draft_histories: Default::default(),
            body_drag: None,
            link_press: None,
            earlier_chats: Vec::new(),
            show_earlier_chats: false,
            show_join_panel: false,
            joining_room: None,
            join_error: None,
            room_connections: Default::default(),
            room_operations: Default::default(),
            join_input: String::new(),
            xmpp_cmd_tx: None,
            xmpp_pending_commands: Default::default(),
            outbox_in_flight: Default::default(),
            pending_outbox_deletes: Default::default(),
            outbox_delete_retry_after: Default::default(),
            discarded_outbox: Default::default(),
            outbox_replay_pending: Default::default(),
            room_history_pending: Default::default(),
            delivery_checks: Default::default(),
            attempted_outbox: Default::default(),
            xmpp_cmd_rx: None,
            remember_me: false,
            save_room: false,
            saved_config,
            pending_save_password: None,
            auto_login_attempt: false,
            nick_complete: None,
            window_focused: true,
            history: None,
            pending_history_writes: Vec::new(),
            room_send_origins: std::collections::HashMap::new(),
            unqueued_dm_sends: Vec::new(),
            find: None,
            pending_seq: -1,
            selected_msgs: std::collections::HashSet::new(),
            selection_id: uuid::Uuid::new_v4(),
            select_anchor: None,
            shift_down: false,
            cmd_down: false,
            body_editors: std::collections::HashMap::new(),
            mam_seq: 0,
            mam_pending: std::collections::HashMap::new(),
            mam_catchup: std::collections::HashMap::new(),
            mam_caught_up: std::collections::HashSet::new(),
        };

        // Use the same guarded path at startup and after the keyring unlocks.
        let startup = snack.update(Message::RetrySavedLogin);

        // On macOS, set the dock icon a moment after launch — setting it before
        // the event loop is running doesn't stick (see main::set_macos_dock_icon).
        #[cfg(target_os = "macos")]
        let startup = Task::batch([
            startup,
            delay_then(std::time::Duration::from_millis(250), Message::SetDockIcon),
        ]);

        return (snack, startup);
    }

    pub(crate) fn has_unsaved_message_updates(&self) -> bool
    {
        !self.pending_history_writes.is_empty() || !self.pending_outbox_deletes.is_empty()
            || !self.pending_message_errors.is_empty() || self.rejected_sends.iter().any(|send| !send.saved)
            || self.unqueued_dm_sends.iter().any(|send| send.dirty)
    }

    pub(crate) fn own_jid(&self) -> Option<&str>
    {
        self.bound_jid.as_deref().or(self.connected_jid.as_deref())
    }

    pub(crate) fn title(&self) -> String
    {
        if let Some(jid) = self.own_jid()
        {
            return format!("Snack — {}", jid);
        }

        return "Snack".to_string();
    }

    pub(crate) fn theme(&self) -> Theme
    {
        return Theme::Nord;
    }
}
