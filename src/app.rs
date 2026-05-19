use iced::{ Task, Theme };
use iced::widget::{ text_editor, Id };

use crate::message::Message;
use crate::{ room, storage, xmpp };

pub const MESSAGE_SCROLL_ID: &str = "message_scroll";
pub const MESSAGE_INPUT_ID: &str = "message_input";
pub const JOIN_INPUT_ID: &str = "join_input";
pub const ACCOUNT_JID_INPUT_ID: &str = "account_jid_input";
pub const ACCOUNT_PASSWORD_INPUT_ID: &str = "account_password_input";

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
    iced::widget::operation::snap_to_end(Id::new(MESSAGE_SCROLL_ID))
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
