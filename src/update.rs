use iced::Task;
use iced::widget::{ text_editor, Id };
use log::{ error, warn };

use crate::app::{
    focus_input, focus_jid_input, focus_join_input, snap_to_bottom,
    AppState, NickCompleteState, Selection, Snack,
    ACCOUNT_PASSWORD_INPUT_ID, MESSAGE_INPUT_ID,
};
use crate::message::Message;
use crate::{ room, storage, xmpp };

fn send_notification(summary: &str, body: &str)
{
    if let Err(e) = notify_rust::Notification::new()
        .summary(summary)
        .body(body)
        .show()
    {
        warn!("Failed to send notification: {}", e);
    }
}

impl Snack
{
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

    // Step through the flat list of sidebar entries (rooms first, then chats),
    // wrapping at either end. Returns the Task that performs the selection, or
    // None when there is nothing to select.
    fn step_selection(&self, forward: bool) -> Option<Task<Message>>
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
            return Some(Task::done(Message::SelectRoom(next_idx)));
        }

        return Some(Task::done(Message::SelectChat(next_idx - self.rooms.len())));
    }

    // Tab nick completion is only active when the message text input is the
    // expected focus target — i.e. a Room is selected, we're fully connected,
    // and no other panel is overlaying the chat view.
    fn is_message_input_context(&self) -> bool
    {
        return self.state == AppState::Connected
            && !self.show_join_panel
            && self.joining_room.is_none()
            && self.join_error.is_none()
            && matches!(self.active, Some(Selection::Room(_)));
    }

    fn cycle_nick_completion(&mut self, backward: bool) -> Task<Message>
    {
        let Some(Selection::Room(idx)) = self.active else
        {
            return Task::none();
        };

        // Resume an in-progress cycle only if the input wasn't edited in between.
        let current_input = self.message_input.text();
        let mut state = self.nick_complete
            .take()
            .filter(|s| s.last_output == current_input);

        if let Some(ref mut s) = state
        {
            if backward
            {
                s.index = if s.index == 0 { s.matches.len() - 1 } else { s.index - 1 };
            }
            else
            {
                s.index = (s.index + 1) % s.matches.len();
            }
        }
        else
        {
            // Start a fresh cycle: find the partial word at the end of the input
            // (whitespace-delimited) and gather all matching nicks alphabetically.
            let input = &current_input;
            let prefix_start = input
                .char_indices()
                .rev()
                .find(|(_, c)| c.is_whitespace())
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(0);
            let prefix = &input[prefix_start..];

            if prefix.is_empty()
            {
                return Task::none();
            }

            let prefix_lower = prefix.to_lowercase();
            let mut matches: Vec<String> = self.rooms[idx].users.iter()
                .map(|u| u.name.clone())
                .filter(|n| n.to_lowercase().starts_with(&prefix_lower))
                .collect();
            matches.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
            matches.dedup();

            if matches.is_empty()
            {
                return Task::none();
            }

            let index = if backward { matches.len() - 1 } else { 0 };
            state = Some(NickCompleteState
            {
                prefix_start,
                matches,
                index,
                last_output: String::new(),
            });
        }

        let mut state = state.expect("state populated above");
        let nick = &state.matches[state.index];
        let suffix = if state.prefix_start == 0 { ": " } else { " " };
        let new_input = format!("{}{}{}", &current_input[..state.prefix_start], nick, suffix);

        self.message_input = text_editor::Content::with_text(&new_input);
        self.message_input.perform(text_editor::Action::Move(
            iced::widget::text_editor::Motion::DocumentEnd,
        ));
        state.last_output = new_input;
        self.nick_complete = Some(state);

        return iced::widget::operation::focus(Id::new(MESSAGE_INPUT_ID));
    }

    pub(crate) fn update(&mut self, message: Message) -> Task<Message>
    {
        match message
        {
            Message::Ignore => {}
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
                        self.message_input = text_editor::Content::new();
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
                            let user_is_watching = is_active && self.window_focused;

                            let own_nick = self.connected_jid
                                .as_deref()
                                .and_then(|j| j.split('@').next())
                                .unwrap_or("");
                            let is_own = nick == own_nick;

                            if !user_is_watching && !is_own
                            {
                                self.rooms[idx].unread = true;
                                if self.rooms[idx].read_marker.is_none()
                                {
                                    self.rooms[idx].read_marker = Some(msg_index);
                                }
                            }
                            else if is_own && is_active
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

                            if !self.window_focused && !is_own
                            {
                                if let Some(room::message::Message::Chat { body, .. }) =
                                    self.rooms[idx].messages.last()
                                {
                                    if room::message::mentions(body, own_nick)
                                    {
                                        let room_name =
                                            room.split('@').next().unwrap_or(&room);
                                        send_notification(
                                            &format!("{} in {}", nick, room_name),
                                            body,
                                        );
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

                        let is_active_chat = self.active == Some(Selection::Chat(idx));
                        let user_is_watching = is_active_chat && self.window_focused;

                        if !user_is_watching
                        {
                            self.chats[idx].unread = true;
                            if self.chats[idx].read_marker.is_none()
                            {
                                self.chats[idx].read_marker = Some(msg_index);
                            }
                        }

                        if !self.window_focused
                        {
                            if let Some(room::message::Message::Chat { from, body, .. }) =
                                self.chats[idx].messages.last()
                            {
                                send_notification(from, body);
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
                self.message_input = text_editor::Content::new();
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
            Message::InputAction(action) =>
            {
                let is_edit = action.is_edit();
                self.message_input.perform(action);
                if is_edit
                {
                    let text = self.message_input.text();
                    if self.nick_complete.as_ref().is_some_and(|s| s.last_output != text)
                    {
                        self.nick_complete = None;
                    }
                }
            }
            Message::SendMessage =>
            {
                let body = self.message_input.text().trim().to_string();

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

                        self.message_input = text_editor::Content::new();
                        self.nick_complete = None;

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

                        self.message_input = text_editor::Content::new();
                        self.nick_complete = None;

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

                    // Remove the room immediately rather than waiting for the
                    // server to confirm via XmppEvent::RoomLeft — if the
                    // connection is dropped or the command never reaches the
                    // server, the user is otherwise stuck with a phantom room.
                    // The RoomLeft handler becomes a no-op if the room is
                    // already gone.
                    self.rooms.remove(index);
                    self.active = None;
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
                if let Err(e) = open::that(&url)
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
            Message::WindowUnfocused =>
            {
                self.window_focused = false;

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
                            r.read_marker = Some(r.messages.len());
                        }
                    }
                    Some(Selection::Chat(idx)) =>
                    {
                        if let Some(c) = self.chats.get_mut(idx)
                        {
                            c.read_marker = Some(c.messages.len());
                        }
                    }
                    None => {}
                }
            }
            Message::WindowFocused =>
            {
                self.window_focused = true;

                // Clear the unread badge on the active room/chat — the user is
                // now looking at it. Leave read_marker so the "new messages"
                // divider remains visible until they navigate away.
                match self.active
                {
                    Some(Selection::Room(idx)) =>
                    {
                        if let Some(r) = self.rooms.get_mut(idx)
                        {
                            r.unread = false;
                        }
                    }
                    Some(Selection::Chat(idx)) =>
                    {
                        if let Some(c) = self.chats.get_mut(idx)
                        {
                            c.unread = false;
                        }
                    }
                    None => {}
                }
            }
        }

        return Task::none();
    }
}
