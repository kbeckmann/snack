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

#[derive(Debug, Clone, PartialEq)]
pub enum AppState
{
    Login,
    Connecting,
    Connected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection
{
    Room(usize),
    Chat(usize),
}

#[derive(Debug, Clone)]
pub struct NickCompleteState
{
    pub prefix_start: usize,
    pub matches: Vec<String>,
    pub index: usize,
    pub last_output: String,
}

// One search hit: which conversation it belongs to and its history DB row id.
#[derive(Debug, Clone)]
pub struct FindMatch
{
    pub conversation: String,
    pub id: i64,
}

// State of the in-room find bar (present only while the bar is open).
#[derive(Debug, Clone, Default)]
pub struct FindState
{
    pub query: String,
    // false = search only the active conversation, true = all open conversations.
    pub all_scope: bool,
    // Hits ordered oldest-first; `index` is the currently-focused one.
    pub matches: Vec<FindMatch>,
    pub index: usize,
}

pub struct Snack
{
    pub(crate) state: AppState,
    pub(crate) jid_input: String,
    pub(crate) password_input: String,
    pub(crate) connected_jid: Option<String>,
    pub(crate) connect_error: Option<String>,
    pub(crate) rooms: Vec<room::Room>,
    pub(crate) chats: Vec<room::chat::Chat>,
    pub(crate) active: Option<Selection>,
    pub(crate) message_input: text_editor::Content,
    pub(crate) show_join_panel: bool,
    pub(crate) joining_room: Option<String>,
    pub(crate) join_error: Option<String>,
    pub(crate) join_input: String,
    pub(crate) xmpp_cmd_tx: Option<tokio::sync::mpsc::Sender<xmpp::XmppCommand>>,
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
    // Find bar; Some only while open over the active conversation.
    pub(crate) find: Option<FindState>,
    // MAM (server archive) paging: a monotonic counter for query ids and a map
    // from in-flight query id to the conversation JID it is paging.
    pub(crate) mam_seq: u64,
    pub(crate) mam_pending: std::collections::HashMap<String, String>,
    // Catch-up sync state for conversations with a sweep in progress: (pages
    // fetched, new messages stored in the current page). The sweep walks the
    // recent archive backwards from the present and stops once a page yields no
    // new messages (i.e. it has reached history we already hold). `mam_caught_up`
    // marks conversations swept this session so re-selecting doesn't re-sync.
    pub(crate) mam_catchup: std::collections::HashMap<String, (u32, u32)>,
    pub(crate) mam_caught_up: std::collections::HashSet<String>,
    // Auto-reconnect: when an established session drops (e.g. after the laptop
    // wakes from sleep) we transparently retry with exponential backoff instead
    // of dumping the user back to the login screen. Credentials are stashed so
    // the retry can rebuild the command channel without re-prompting.
    pub(crate) reconnecting: bool,
    pub(crate) reconnect_attempts: u32,
    pub(crate) reconnect_jid: Option<String>,
    pub(crate) reconnect_password: Option<String>,
}

impl Snack
{
    pub(crate) fn new() -> (Self, Task<Message>)
    {
        storage::init_keyring();
        let saved_config = storage::load();

        let mut snack = Self
        {
            state: AppState::Login,
            jid_input: saved_config.jid.clone().unwrap_or_default(),
            password_input: String::new(),
            connected_jid: None,
            connect_error: None,
            rooms: Vec::new(),
            chats: Vec::new(),
            active: None,
            message_input: text_editor::Content::new(),
            show_join_panel: false,
            joining_room: None,
            join_error: None,
            join_input: String::new(),
            xmpp_cmd_tx: None,
            xmpp_cmd_rx: None,
            remember_me: false,
            save_room: false,
            saved_config,
            pending_save_password: None,
            auto_login_attempt: false,
            nick_complete: None,
            window_focused: true,
            history: storage::open_history(),
            find: None,
            mam_seq: 0,
            mam_pending: std::collections::HashMap::new(),
            mam_catchup: std::collections::HashMap::new(),
            mam_caught_up: std::collections::HashSet::new(),
            reconnecting: false,
            reconnect_attempts: 0,
            reconnect_jid: None,
            reconnect_password: None,
        };

        // Auto-login: if a keyring entry exists for the saved JID, connect silently.
        if let Some(jid) = snack.saved_config.jid.clone()
        {
            if let Some(password) = storage::load_password(&jid)
            {
                snack.password_input = password;
                snack.remember_me = true;
                snack.auto_login_attempt = true;
                return (snack, Task::done(Message::Connect));
            }
        }

        return (snack, focus_jid_input());
    }

    pub(crate) fn title(&self) -> String
    {
        if let Some(ref jid) = self.connected_jid
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
