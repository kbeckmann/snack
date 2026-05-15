#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use iced::{ Application, Element, Program, Task, Theme, Fill };
use iced::widget::{ row, Id };
use log::error;

mod omemo;
mod room;
mod storage;
mod ui;
mod xmpp;

pub const MESSAGE_SCROLL_ID: &str = "message_scroll";
pub const MESSAGE_INPUT_ID: &str = "message_input";
pub const JOIN_INPUT_ID: &str = "join_input";
pub const ACCOUNT_JID_INPUT_ID: &str = "account_jid_input";
pub const ACCOUNT_PASSWORD_INPUT_ID: &str = "account_password_input";

fn focus_jid_input() -> Task<Message>
{
    iced::widget::operation::focus(Id::new(ACCOUNT_JID_INPUT_ID))
}

fn focus_join_input() -> Task<Message>
{
    iced::widget::operation::focus(Id::new(JOIN_INPUT_ID))
}

fn focus_input() -> Task<Message>
{
    iced::widget::operation::focus(Id::new(MESSAGE_INPUT_ID))
}

fn snap_to_bottom() -> Task<Message>
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
    pub(crate) message_input: String,
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
}

#[derive(Debug, Clone)]
pub enum Message
{
    Ignore,
    TabPressed,
    ShiftTabPressed,
    JidInputChanged(String),
    PasswordInputChanged(String),
    RememberMeToggled(bool),
    SaveRoomToggled(bool),
    FocusPassword,
    Connect,
    CancelConnect,
    XmppEvent(xmpp::XmppEvent),
    Disconnect,
    SelectRoom(usize),
    SelectChat(usize),
    StartChat(String),
    InputChanged(String),
    SendMessage,
    ShowJoinPanel,
    HideJoinPanel,
    JoinInputChanged(String),
    JoinRoom,
    DismissJoinError,
    LeaveRoom,
    CloseChat,
    OpenUrl(String),
    ForgetAutoLogin,
    WindowFocused,
    WindowUnfocused,
}

fn main() -> iced::Result
{
    env_logger::init();
    return application().run();
}

fn application() -> Application<impl Program<Message = Message, Theme = Theme>>
{
    return iced::application(Snack::new, Snack::update, Snack::view)
                .subscription(Snack::subscription)
                .title(Snack::title)
                .theme(Snack::theme);
}

impl Snack
{
    fn new() -> (Self, Task<Message>)
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
            message_input: String::new(),
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

    fn title(&self) -> String
    {
        if let Some(ref jid) = self.connected_jid
        {
            return format!("Snack — {}", jid);
        }

        return "Snack".to_string();
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
                    r.read_marker = Some(r.messages.len());
                }
            }
            Some(Selection::Chat(old_idx)) if next != Some(Selection::Chat(old_idx)) =>
            {
                if let Some(c) = self.chats.get_mut(old_idx)
                {
                    c.read_marker = Some(c.messages.len());
                }
            }
            _ => {}
        }
    }

    fn update(&mut self, message: Message) -> Task<Message>
    {
        match message
        {
            Message::Ignore => {}
            Message::TabPressed =>
            {
                return iced::widget::operation::focus_next();
            }
            Message::ShiftTabPressed =>
            {
                return iced::widget::operation::focus_previous();
            }
            Message::JidInputChanged(value) =>
            {
                self.jid_input = value;
            }
            Message::PasswordInputChanged(value) =>
            {
                self.password_input = value;
            }
            Message::RememberMeToggled(value) =>
            {
                self.remember_me = value;

                // Unchecking "Remember me" on the login form immediately clears
                // any saved credentials so the user isn't locked into auto-login.
                if !value
                {
                    if let Some(jid) = self.saved_config.jid.take()
                    {
                        let _ = storage::delete_password(&jid);
                    }
                    if let Err(e) = storage::save(&self.saved_config)
                    {
                        log::warn!("Failed to save config after forgetting login: {}", e);
                    }
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
                let jid = self.jid_input.trim().to_string();
                let password = self.password_input.clone();

                if jid.is_empty() || password.is_empty()
                {
                    error!("Connection failed: JID and password are required");
                    self.connect_error = Some("JID and password are required.".to_string());

                    return Task::none();
                }

                if !jid.contains('@')
                {
                    error!("Connection failed: invalid JID format '{}'", jid);
                    self.connect_error = Some("JID must be in the format user@domain.".to_string());

                    return Task::none();
                }

                self.connected_jid = Some(jid.clone());
                self.connect_error = None;
                self.pending_save_password = Some(password.clone());

                let (cmd_tx, cmd_rx) = xmpp::new_command_channel(jid, password);
                self.xmpp_cmd_tx = Some(cmd_tx);
                self.xmpp_cmd_rx = Some(cmd_rx);

                self.state = AppState::Connecting;

                return Task::none();
            }
            Message::CancelConnect =>
            {
                self.state = AppState::Login;
                self.connected_jid = None;
                self.xmpp_cmd_tx = None;
                self.xmpp_cmd_rx = None;
                self.pending_save_password = None;
                self.auto_login_attempt = false;
                self.connect_error = None;

                return focus_jid_input();
            }
            Message::XmppEvent(event) =>
            {
                log::debug!("UI received XmppEvent: {:?}", event);
                match event
                {
                    xmpp::XmppEvent::Connected =>
                    {
                        let password = self.pending_save_password.take();
                        let was_auto_login = self.auto_login_attempt;
                        self.password_input.clear();
                        self.state = AppState::Connected;
                        self.auto_login_attempt = false;

                        let jid = self.connected_jid.clone().unwrap_or_default();

                        // Persist or clear saved login depending on the checkbox.
                        if self.remember_me
                        {
                            // Skip the write when the password came from the Keychain
                            // already as it hasn't changed.
                            if !was_auto_login
                            {
                                if let Some(pw) = password
                                {
                                    if !jid.is_empty()
                                    {
                                        if let Err(e) = storage::save_password(&jid, &pw)
                                        {
                                            log::warn!("Failed to save password to keyring: {}", e);
                                        }
                                    }
                                }
                            }
                            self.saved_config.jid = Some(jid.clone());
                        }
                        else
                        {
                            // User unchecked Remember me. Clear any prior saved login.
                            if let Some(prev) = self.saved_config.jid.clone()
                            {
                                let _ = storage::delete_password(&prev);
                            }

                            self.saved_config.jid = None;
                        }

                        if let Err(e) = storage::save(&self.saved_config)
                        {
                            log::warn!("Failed to save config: {}", e);
                        }

                        // Auto-join any saved rooms.
                        if let Some(ref tx) = self.xmpp_cmd_tx
                        {
                            for room_jid in &self.saved_config.rooms
                            {
                                let _ = tx.try_send(xmpp::XmppCommand::JoinRoom(room_jid.clone()));
                            }
                        }

                        return focus_join_input();
                    }
                    xmpp::XmppEvent::Disconnected(reason) =>
                    {
                        error!("Disconnected: {}", reason);

                        // If an auto-login attempt failed, the saved password is likely
                        // stale. Delete it so we don't loop on it next launch.
                        if self.auto_login_attempt
                        {
                            if let Some(jid) = self.saved_config.jid.clone()
                            {
                                let _ = storage::delete_password(&jid);
                            }

                            self.remember_me = false;
                        }

                        self.connect_error = Some(reason);
                        self.connected_jid = None;
                        self.state = AppState::Login;
                        self.rooms.clear();
                        self.chats.clear();
                        self.active = None;
                        self.message_input.clear();
                        self.show_join_panel = false;
                        self.joining_room = None;
                        self.join_error = None;
                        self.join_input.clear();
                        self.xmpp_cmd_tx = None;
                        self.xmpp_cmd_rx = None;
                        self.pending_save_password = None;
                        self.auto_login_attempt = false;

                        return focus_jid_input();
                    }
                    xmpp::XmppEvent::RoomJoined { room: jid, members } =>
                    {
                        self.joining_room = None;
                        self.join_error = None;

                        // Persist room if user opted in and it's not already saved.
                        if self.save_room && !self.saved_config.rooms.iter().any(|r| r == &jid)
                        {
                            self.saved_config.rooms.push(jid.clone());

                            if let Err(e) = storage::save(&self.saved_config)
                            {
                                log::warn!("Failed to save room to config: {}", e);
                            }
                        }

                        if let Some(pos) = self.rooms.iter().position(|r| r.jid == jid)
                        {
                            self.active = Some(Selection::Room(pos));
                            self.rooms[pos].users = members.into_iter().map(|m| room::user::User
                            {
                                jid: m.jid,
                                name: m.nick,
                                show: m.show,
                                status: m.status,
                            }).collect();
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
                                users,
                                messages: Vec::new(),
                                unread: false,
                                read_marker: None,
                            });

                            self.active = Some(Selection::Room(self.rooms.len() - 1));
                        }

                        self.show_join_panel = false;
                        return Task::batch([snap_to_bottom(), focus_input()]);
                    }
                    xmpp::XmppEvent::RoomJoinFailed { room: _, reason } =>
                    {
                        self.joining_room = None;
                        self.join_error = Some(reason);

                        return focus_join_input();
                    }
                    xmpp::XmppEvent::PresenceError { from, condition, text } =>
                    {
                        let is_join_error = self.joining_room.as_ref().map_or(false, |room|
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

                            self.joining_room = None;
                            self.join_error = Some(message);

                            return focus_join_input();
                        }
                    }
                    xmpp::XmppEvent::RoomLeft(jid) =>
                    {
                        if let Some(pos) = self.rooms.iter().position(|r| r.jid == jid)
                        {
                            self.rooms.remove(pos);
                            if let Some(Selection::Room(active)) = self.active
                            {
                                if active == pos
                                {
                                    self.active = None;
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
                                let old_show = r.users[idx].show.clone();
                                r.users[idx].show = member.show.clone();
                                r.users[idx].status = member.status;

                                if old_show != r.users[idx].show
                                {
                                    r.messages.push(room::message::Message::Event
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
                                r.messages.push(room::message::Message::Event
                                {
                                    kind: room::message::EventKind::Joined,
                                    nick,
                                    received: chrono::Utc::now(),
                                });
                            }
                        }
                    }
                    xmpp::XmppEvent::MemberLeft { room, nick } =>
                    {
                        if let Some(r) = self.rooms.iter_mut().find(|r| r.jid == room)
                        {
                            r.users.retain(|u| u.name != nick);
                            r.messages.push(room::message::Message::Event
                            {
                                kind: room::message::EventKind::Left,
                                nick,
                                received: chrono::Utc::now(),
                            });
                        }
                    }
                    xmpp::XmppEvent::RoomMessage { room, nick, body, timestamp } =>
                    {
                        let room_idx = self.rooms.iter().position(|r| r.jid == room);
                        if let Some(idx) = room_idx
                        {
                            let msg_index = self.rooms[idx].messages.len();
                            self.rooms[idx].messages.push(room::message::Message::Chat
                            {
                                from: nick.clone(),
                                body,
                                received: timestamp,
                            });

                            let is_active = self.active == Some(Selection::Room(idx));

                            let own_nick = self.connected_jid
                                .as_deref()
                                .and_then(|j| j.split('@').next())
                                .unwrap_or("");

                            if !is_active
                            {
                                self.rooms[idx].unread = true;
                                if self.rooms[idx].read_marker.is_none()
                                {
                                    self.rooms[idx].read_marker = Some(msg_index);
                                }
                            }
                            else if nick == own_nick
                            {
                                // Own echo in active room: advance marker past our message so it
                                // never appears in the "new messages" section.
                                if let Some(marker) = self.rooms[idx].read_marker
                                {
                                    if marker <= msg_index
                                    {
                                        self.rooms[idx].read_marker = Some(msg_index + 1);
                                    }
                                }
                            }

                            return snap_to_bottom();
                        }
                    }
                    xmpp::XmppEvent::RoomSubject { room, subject } =>
                    {
                        if let Some(r) = self.rooms.iter_mut().find(|r| r.jid == room)
                        {
                            r.topic = subject;
                        }
                    }
                    xmpp::XmppEvent::OmemoReady { device_id, fingerprint } =>
                    {
                        log::info!("OMEMO ready: device {} fingerprint {}", device_id, fingerprint);
                    }
                    xmpp::XmppEvent::OmemoError(e) =>
                    {
                        log::warn!("OMEMO: {}", e);
                    }
                    xmpp::XmppEvent::DirectMessage { from, body, timestamp } =>
                    {
                        let bare = from.split('/').next().unwrap_or(&from).to_string();
                        let idx = match self.chats.iter().position(|c| c.jid == bare)
                        {
                            Some(i) => i,
                            None =>
                            {
                                let title = bare.split('@').next().unwrap_or(&bare).to_string();
                                self.chats.push(room::chat::Chat
                                {
                                    jid: bare,
                                    title,
                                    messages: Vec::new(),
                                    unread: false,
                                    read_marker: None,
                                });
                                self.chats.len() - 1
                            }
                        };

                        let nick = self.chats[idx].title.clone();
                        let msg_index = self.chats[idx].messages.len();
                        self.chats[idx].messages.push(room::message::Message::Chat
                        {
                            from: nick,
                            body,
                            received: timestamp,
                        });

                        if self.active != Some(Selection::Chat(idx))
                        {
                            self.chats[idx].unread = true;
                            if self.chats[idx].read_marker.is_none()
                            {
                                self.chats[idx].read_marker = Some(msg_index);
                            }
                        }

                        return snap_to_bottom();
                    }
                }
            }
            Message::Disconnect =>
            {
                self.state = AppState::Login;
                self.connected_jid = None;
                self.rooms.clear();
                self.chats.clear();
                self.active = None;
                self.message_input.clear();
                self.show_join_panel = false;
                self.joining_room = None;
                self.join_error = None;
                self.join_input.clear();
                self.xmpp_cmd_tx = None;
                self.xmpp_cmd_rx = None;

                return focus_jid_input();
            }
            Message::ForgetAutoLogin =>
            {
                if let Some(jid) = self.saved_config.jid.take()
                {
                    let _ = storage::delete_password(&jid);
                }

                self.remember_me = false;

                if let Err(e) = storage::save(&self.saved_config)
                {
                    log::warn!("Failed to save config after removing auto-login: {}", e);
                }
            }
            Message::SelectRoom(index) =>
            {
                self.stamp_active_read_marker(Some(Selection::Room(index)));

                self.active = Some(Selection::Room(index));
                self.show_join_panel = false;
                if let Some(r) = self.rooms.get_mut(index)
                {
                    r.unread = false;
                    r.read_marker = None;
                }

                return Task::batch([snap_to_bottom(), focus_input()]);
            }
            Message::SelectChat(index) =>
            {
                self.stamp_active_read_marker(Some(Selection::Chat(index)));

                self.active = Some(Selection::Chat(index));
                self.show_join_panel = false;
                if let Some(c) = self.chats.get_mut(index)
                {
                    c.unread = false;
                    c.read_marker = None;
                }

                return Task::batch([snap_to_bottom(), focus_input()]);
            }
            Message::StartChat(jid) =>
            {
                let bare = jid.split('/').next().unwrap_or(&jid).to_string();
                let idx = match self.chats.iter().position(|c| c.jid == bare)
                {
                    Some(i) => i,
                    None =>
                    {
                        let title = bare.split('@').next().unwrap_or(&bare).to_string();
                        self.chats.push(room::chat::Chat
                        {
                            jid: bare,
                            title,
                            messages: Vec::new(),
                            unread: false,
                            read_marker: None,
                        });
                        self.chats.len() - 1
                    }
                };

                return Task::done(Message::SelectChat(idx));
            }
            Message::InputChanged(value) =>
            {
                self.message_input = value;
            }
            Message::SendMessage =>
            {
                let body = self.message_input.trim().to_string();

                if body.is_empty()
                {
                    return Task::none();
                }

                match self.active
                {
                    Some(Selection::Room(index)) =>
                    {
                        if let Some(ref tx) = self.xmpp_cmd_tx
                        {
                            let room_jid = self.rooms[index].jid.clone();
                            if tx.try_send(xmpp::XmppCommand::SendRoomMessage
                            {
                                room: room_jid,
                                body: body.clone(),
                            }).is_err()
                            {
                                return focus_input();
                            }
                        }

                        self.message_input.clear();

                        return Task::batch([snap_to_bottom(), focus_input()]);
                    }
                    Some(Selection::Chat(index)) =>
                    {
                        let chat_jid = self.chats[index].jid.clone();

                        if let Some(ref tx) = self.xmpp_cmd_tx
                        {
                            if tx.try_send(xmpp::XmppCommand::SendDirectMessage
                            {
                                to: chat_jid,
                                body: body.clone(),
                            }).is_err()
                            {
                                return focus_input();
                            }
                        }

                        // The server does not echo type='chat' messages back to us, so append locally.
                        let own_nick = self.connected_jid
                            .as_deref()
                            .and_then(|j| j.split('@').next())
                            .unwrap_or("me")
                            .to_string();

                        let msg_index = self.chats[index].messages.len();

                        self.chats[index].messages.push(room::message::Message::Chat
                        {
                            from: own_nick,
                            body,
                            received: chrono::Utc::now(),
                        });

                        if let Some(marker) = self.chats[index].read_marker
                        {
                            if marker <= msg_index
                            {
                                self.chats[index].read_marker = Some(msg_index + 1);
                            }
                        }

                        self.message_input.clear();

                        return Task::batch([snap_to_bottom(), focus_input()]);
                    }
                    None => {}
                }
            }
            Message::ShowJoinPanel =>
            {
                self.show_join_panel = true;
                self.join_input.clear();
                self.join_error = None;
                self.joining_room = None;

                return focus_join_input();
            }
            Message::HideJoinPanel =>
            {
                self.show_join_panel = false;
                self.join_error = None;

                return focus_input();
            }
            Message::JoinInputChanged(value) =>
            {
                self.join_input = value;
            }
            Message::JoinRoom =>
            {
                let jid = self.join_input.trim().to_string();

                if !jid.is_empty()
                {
                    // If already in this room, just switch to it.
                    if let Some(pos) = self.rooms.iter().position(|r| r.jid == jid)
                    {
                        self.active = Some(Selection::Room(pos));
                        self.show_join_panel = false;
                        self.join_input.clear();
                        self.join_error = None;

                        return Task::batch([snap_to_bottom(), focus_input()]);
                    }

                    if let Some(ref tx) = self.xmpp_cmd_tx
                    {
                        let _ = tx.try_send(xmpp::XmppCommand::JoinRoom(jid.clone()));
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
            Message::LeaveRoom =>
            {
                if let Some(Selection::Room(index)) = self.active
                {
                    let room_jid = self.rooms[index].jid.clone();

                    if let Some(ref tx) = self.xmpp_cmd_tx
                    {
                        // Derive nick the same way the XMPP thread does.
                        let nick = self.connected_jid
                            .as_deref()
                            .and_then(|j| j.split('@').next())
                            .unwrap_or("user")
                            .to_string();

                        let _ = tx.try_send(xmpp::XmppCommand::LeaveRoom
                        {
                            room: room_jid.clone(),
                            nick,
                        });
                    }

                    // Stop auto-joining this room next time.
                    let before = self.saved_config.rooms.len();
                    self.saved_config.rooms.retain(|r| r != &room_jid);

                    if self.saved_config.rooms.len() != before
                    {
                        if let Err(e) = storage::save(&self.saved_config)
                        {
                            log::warn!("Failed to save config after leaving room: {}", e);
                        }
                    }

                    // Room removal is driven by XmppEvent::RoomLeft.
                }
            }
            Message::CloseChat =>
            {
                if let Some(Selection::Chat(index)) = self.active
                {
                    self.chats.remove(index);
                    if self.chats.is_empty()
                    {
                        self.active = self.rooms.first().map(|_| Selection::Room(0));
                    }
                    else
                    {
                        self.active = Some(Selection::Chat(index.saturating_sub(1)));
                    }
                }
            }
            Message::OpenUrl(url) =>
            {
                if let Err(e) = open::that(&url)
                {
                    error!("Failed to open URL {}: {}", url, e);
                }
            }
            Message::WindowUnfocused =>
            {
                for room in self.rooms.iter_mut()
                {
                    room.read_marker = Some(room.messages.len());
                }

                for chat in self.chats.iter_mut()
                {
                    chat.read_marker = Some(chat.messages.len());
                }
            }
            Message::WindowFocused => {}
        }

        return Task::none();
    }

    fn view(&self) -> Element<'_, Message>
    {
        match self.state
        {
            AppState::Login | AppState::Connecting =>
            {
                return ui::account::view(self);
            }
            AppState::Connected =>
            {
                let room_list = ui::sidebar::view(self);
                let center = ui::chat::view(self);

                let show_members = matches!(self.active, Some(Selection::Room(_)));

                if show_members
                {
                    let member_list = ui::members::view(self);
                    return row![room_list, center, member_list]
                        .spacing(0)
                        .height(Fill)
                        .width(Fill)
                        .into();
                }

                return row![room_list, center]
                    .spacing(0)
                    .height(Fill)
                    .width(Fill)
                    .into();
            }
        }
    }

    fn subscription(&self) -> iced::Subscription<Message>
    {
        let keyboard = iced::keyboard::listen().map(|event|
        {
            if let iced::keyboard::Event::KeyPressed { key, modifiers, .. } = event
            {
                if key == iced::keyboard::Key::Named(iced::keyboard::key::Named::Tab)
                {
                    if modifiers.shift()
                    {
                        return Message::ShiftTabPressed;
                    }
                    return Message::TabPressed;
                }
            }

            return Message::Ignore;
        });

        let window_focus = iced::event::listen_with(|event, _status, _id|
        {
            match event
            {
                iced::Event::Window(iced::window::Event::Focused) => Some(Message::WindowFocused),
                iced::Event::Window(iced::window::Event::Unfocused) => Some(Message::WindowUnfocused),
                _ => None,
            }
        });

        match (&self.state, &self.xmpp_cmd_rx)
        {
            (AppState::Connecting | AppState::Connected, Some(cmd_rx)) =>
            {
                let xmpp_sub = iced::Subscription::run_with(
                    cmd_rx.clone(),
                    |cmd: &xmpp::CommandChannel|
                    {
                        xmpp::connect(cmd.clone())
                    },
                ).map(Message::XmppEvent);

                return iced::Subscription::batch([keyboard, window_focus, xmpp_sub]);
            }
            _ => return iced::Subscription::batch([keyboard, window_focus]),
        }
    }

    fn theme(&self) -> Theme
    {
        return Theme::Nord;
    }
}
