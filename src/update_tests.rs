use super::*;
use iced::advanced::{Layout, layout, widget::Tree};

fn chat(jid: &str, bodies: &[&str]) -> room::chat::Chat {
    room::chat::Chat {
        jid: jid.into(),
        title: jid.into(),
        unread: false,
        read_marker: None, read_boundary: None,
        parked_messages: Vec::new(),
        messages: bodies
            .iter()
            .enumerate()
            .map(|(i, body)| room::message::Message::Chat {
                id: i as i64 + 1,
                from: "alice".into(),
                body: (*body).into(),
                received: chrono::Utc::now(),
                status: ChatStatus::Confirmed,
            })
            .collect(),
        oldest_loaded_id: None, local_cursor: None,
        has_older: false,
        at_live_tail: true,
        anchored_bottom: true,
        mam_in_flight: false,
        mam_exhausted: false,
        mam_cursor: None,
    }
}
fn state() -> Snack {
    let mut s = Snack::test_state();
    s.chats
        .push(chat("alice@example", &["alpha", "bravo", "charlie"]));
    s.active = Some(Selection::Chat(0));
    s.sync_body_editors();
    s
}

fn complete_worker_stop(s: &mut Snack) {
    assert!(s.disconnecting);
    let channel = s.xmpp_cmd_rx.clone().expect("stopping worker subscription");
    let _ = s.update(Message::ScopedXmppEvent { channel, event: xmpp::XmppEvent::Stopped });
    assert!(!s.disconnecting);
}

fn composer_key(key: iced::keyboard::Key, text: Option<&str>, modifiers: iced::keyboard::Modifiers) -> iced::Event {
    iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
        modified_key: key.clone(), key,
        physical_key: iced::keyboard::key::Physical::Code(iced::keyboard::key::Code::Enter),
        location: iced::keyboard::Location::Standard, modifiers, text: text.map(Into::into), repeat: false,
    })
}

fn composer_events(s: &Snack, events: Vec<iced::Event>) -> Vec<Message> {
    let mut widget = crate::ui::chat::view(s);
    let mut tree = Tree::new(&widget);
    let r = renderer();
    let size = iced::Size::new(600.0, 400.0);
    let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
    let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
    let mut focus = iced::advanced::widget::operation::focusable::focus::<()>(iced::widget::Id::new(crate::app::MESSAGE_INPUT_ID));
    widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut focus);
    let mut messages = Vec::new();
    for event in events {
        widget.as_widget_mut().update(&mut tree, &event, Layout::new(&node), iced::mouse::Cursor::Unavailable, &r,
            &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut messages), &bounds);
    }
    messages
}

#[test]
fn delayed_composer_send_does_not_send_another_or_reopened_draft() {
    for transition in ["other chat", "same chat reopened", "hidden", "panel reopened"] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        s.chats[0].messages.clear();
        s.body_editors.clear();
        s.history = Some(History::open_in_memory());
        s.chats.push(chat("bob@example", &[]));
        s.saved_config.drafts.insert("bob@example".into(), "bob private draft".into());
        s.message_input = text_editor::Content::with_text("alice private draft");
        let queued = composer_events(&s, vec![composer_key(iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter), None, iced::keyboard::Modifiers::NONE)]);
        assert_eq!(queued.len(), 1, "Enter must produce exactly one composer event");
        match transition {
            "other chat" => { let _ = s.update(Message::SelectChat("bob@example".into())); }
            "same chat reopened" => {
                let _ = s.update(Message::SelectChat("bob@example".into()));
                let _ = s.update(Message::SelectChat("alice@example".into()));
            }
            "hidden" => { let _ = s.update(Message::ShowJoinPanel); }
            _ => { let _ = s.update(Message::ShowJoinPanel); let _ = s.update(Message::HideJoinPanel); }
        }
        let draft = s.message_input.text();
        let _ = s.update(queued.into_iter().next().unwrap());
        assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty(), "{transition}");
        assert_eq!(s.message_input.text(), draft);
    }
}

#[test]
fn delayed_composer_edits_and_undo_do_not_modify_the_new_conversation() {
    for undo in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.history = Some(History::open_in_memory());
        s.chats.push(chat("bob@example", &[]));
        let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Insert('a'))));
        let key = if undo { composer_key(iced::keyboard::Key::Character("z".into()), None, iced::keyboard::Modifiers::COMMAND) }
            else { composer_key(iced::keyboard::Key::Character("x".into()), Some("x"), iced::keyboard::Modifiers::NONE) };
        let queued = composer_events(&s, vec![key]);
        assert!(!queued.is_empty());
        let _ = s.update(Message::SelectChat("bob@example".into()));
        let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Paste(std::sync::Arc::new("bob private draft".into())))));
        for event in queued { let _ = s.update(event); }
        assert_eq!(s.message_input.text(), "bob private draft");
    }
}

#[test]
fn batched_current_composer_actions_keep_typing_undo_redo_and_reconnect_queuing() {
    for reconnecting in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        s.chats[0].messages.clear();
        s.body_editors.clear();
        s.history = Some(History::open_in_memory());
        s.reconnecting = reconnecting;
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        let events = composer_events(&s, vec![
            composer_key(iced::keyboard::Key::Character("a".into()), Some("a"), iced::keyboard::Modifiers::NONE),
            composer_key(iced::keyboard::Key::Character("b".into()), Some("b"), iced::keyboard::Modifiers::NONE),
            composer_key(iced::keyboard::Key::Character("z".into()), None, iced::keyboard::Modifiers::COMMAND),
            composer_key(iced::keyboard::Key::Character("z".into()), None, iced::keyboard::Modifiers::COMMAND | iced::keyboard::Modifiers::SHIFT),
            composer_key(iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter), None, iced::keyboard::Modifiers::NONE),
            composer_key(iced::keyboard::Key::Character("c".into()), Some("c"), iced::keyboard::Modifiers::NONE),
            composer_key(iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter), None, iced::keyboard::Modifiers::NONE),
        ]);
        assert_eq!(events.len(), 7);
        for event in events { let _ = s.update(event); }
        let queued = s.history.as_ref().unwrap().load_outbox().unwrap();
        assert_eq!(queued.iter().map(|item| item.body.as_str()).collect::<Vec<_>>(), ["ab", "c"]);
        assert!(queued.iter().all(|item| item.conversation == "alice@example"));
        assert_eq!(s.message_input.text(), "");
        let mut sent = 0;
        while let Ok(command) = rx.try_recv() { if matches!(command.into_inner(), xmpp::XmppCommand::SendDirectMessage { .. }) { sent += 1; } }
        assert_eq!(sent, if reconnecting { 0 } else { 2 });
    }
}

#[test]
fn composer_events_from_a_previous_account_cannot_send_the_new_accounts_draft() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("old@example/device".into());
    s.saved_config.draft_account = Some("old@example".into());
    s.history = Some(History::open_in_memory());
    s.message_input = text_editor::Content::with_text("old account draft");
    let queued = composer_events(&s, vec![composer_key(iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter), None, iced::keyboard::Modifiers::NONE)]);
    assert_eq!(queued.len(), 1);
    let _ = s.update(Message::Disconnect);
    let _ = s.update(Message::JidInputChanged("new@example/device".into()));
    let _ = s.update(Message::PasswordInputChanged("test secret".into()));
    let _ = s.update(Message::Connect);
    // The subscription is never run: this is an isolated state transition,
    // with no real connection and the test keyring implementation.
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Connected));
    assert!(s.state == AppState::Connected);
    s.chats.push(chat("alice@example", &[]));
    let _ = s.update(Message::SelectChat("alice@example".into()));
    let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Paste(std::sync::Arc::new("new account private draft".into())))));
    for event in queued { let _ = s.update(event); }
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
    assert_eq!(s.message_input.text(), "new account private draft");
    assert_eq!(s.saved_config.account_drafts["old@example"]["alice@example"], "old account draft");
}

fn transcript_clicks(s: &Snack, label: Option<&str>, count: usize) -> Vec<Message> {
    use iced::advanced::widget::operation::{Operation, Focusable};
    struct Target<'a> { label: Option<&'a str>, bounds: Option<iced::Rectangle> }
    impl Operation for Target<'_> {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) {
            if self.label == Some(text) { self.bounds = Some(bounds); }
        }
        fn focusable(&mut self, id: Option<&iced::widget::Id>, bounds: iced::Rectangle, _state: &mut dyn Focusable) {
            if self.label.is_none() && id.is_none() { self.bounds = Some(bounds); }
        }
    }
    let mut widget = crate::ui::chat::view(s);
    let mut tree = Tree::new(&widget);
    let r = renderer();
    let size = iced::Size::new(600.0, 400.0);
    let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
    let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
    let mut target = Target { label, bounds: None };
    widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut target);
    let point = target.bounds.expect("transcript click target").center();
    let mut messages = Vec::new();
    for _ in 0..count {
        for event in [iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left), iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)] {
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(event), Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut messages), &bounds);
        }
    }
    messages
}

#[test]
fn current_transcript_callbacks_still_quote_select_and_copy_normally() {
    let mut s = state();
    s.state = AppState::Connected;
    s.chats[0].messages.clear();
    s.body_editors.clear();
    let h = History::open_in_memory();
    h.insert("alice@example", "alice", "quote this message", chrono::Utc::now(), false, None).unwrap();
    let id = h.load_tail("alice@example", 1).unwrap()[0].id;
    s.history = Some(h);
    seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
    s.sync_body_editors();
    s.message_input = text_editor::Content::with_text("my draft");
    let quote = transcript_clicks(&s, Some("alice: "), 2);
    assert!(!quote.is_empty());
    for message in quote { let _ = s.update(message); }
    assert_eq!(s.message_input.text(), "my draft\n> quote this message\n");
    let _ = s.update(Message::UndoInput);
    assert_eq!(s.message_input.text(), "my draft");
    s.cmd_down = true;
    let select = transcript_clicks(&s, Some("alice: "), 1);
    assert!(!select.is_empty());
    for message in select { let _ = s.update(message); }
    assert_eq!(s.selected_msgs, [id].into_iter().collect());
    s.cmd_down = false;
    let copy = transcript_clicks(&s, Some("Copy"), 1);
    assert!(!copy.is_empty());
    let mut units = 0;
    for message in copy { units += s.update(message).units(); }
    assert_eq!(units, 2, "copy produces clipboard and focus tasks without executing them in the test");
    assert!(s.selected_msgs.is_empty());
    assert!(s.select_anchor.is_none());
}

#[test]
fn delayed_transcript_clicks_cannot_clear_or_copy_a_new_chats_selection() {
    for copy in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.chats[0].messages.clear();
        s.body_editors.clear();
        let h = History::open_in_memory();
        h.insert("alice@example", "alice", "old chat message", chrono::Utc::now(), false, None).unwrap();
        h.insert("bob@example", "bob", "new chat private message", chrono::Utc::now(), false, None).unwrap();
        let alice = h.load_tail("alice@example", 1).unwrap()[0].id;
        let bob = h.load_tail("bob@example", 1).unwrap()[0].id;
        s.history = Some(h);
        seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
        s.sync_body_editors();
        if copy { let _ = s.update(Message::SelectMessage { id: alice }); }
        let queued = transcript_clicks(&s, if copy { Some("Copy") } else { None }, 1);
        assert!(!queued.is_empty());
        s.chats.push(chat("bob@example", &[]));
        let _ = s.update(Message::SelectChat("bob@example".into()));
        let _ = s.update(Message::SelectMessage { id: bob });
        for message in queued {
            let task = s.update(message);
            assert_eq!(task.units(), 0, "old callbacks must not issue clipboard/focus tasks");
        }
        assert_eq!(s.selected_msgs, [bob].into_iter().collect());
        assert_eq!(s.select_anchor, Some(bob));
    }
}

#[test]
fn delayed_quote_cannot_use_a_reused_history_id_in_another_account() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("old@example/device".into());
    s.saved_config.draft_account = Some("old@example".into());
    s.chats[0].messages.clear();
    s.body_editors.clear();
    let h = History::open_in_memory();
    h.insert("alice@example", "alice", "old account message", chrono::Utc::now(), false, None).unwrap();
    let old_id = h.load_tail("alice@example", 1).unwrap()[0].id;
    s.history = Some(h);
    seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
    s.sync_body_editors();
    let queued = transcript_clicks(&s, Some("alice: "), 2);
    assert!(!queued.is_empty());
    let _ = s.update(Message::Disconnect);
    let _ = s.update(Message::JidInputChanged("new@example/device".into()));
    let _ = s.update(Message::PasswordInputChanged("test secret".into()));
    let _ = s.update(Message::Connect);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Connected)); // No subscription/network is run.
    s.history.as_ref().unwrap().insert("alice@example", "alice", "new account private message", chrono::Utc::now(), false, None).unwrap();
    let new_id = s.history.as_ref().unwrap().load_tail("alice@example", 1).unwrap()[0].id;
    assert_eq!(old_id, new_id, "different account stores can reuse numeric IDs");
    s.chats.push(chat("alice@example", &[]));
    let _ = s.update(Message::SelectChat("alice@example".into()));
    s.message_input = text_editor::Content::with_text("new account draft");
    for event in queued { let _ = s.update(event); }
    assert_eq!(s.message_input.text(), "new account draft");
}

#[test]
fn delayed_conversation_close_cannot_discard_a_replacement_conversation() {
    for rooms in [false, true] {
        for return_to_original in [false, true] {
            let mut s = state();
            s.state = AppState::Connected;
            s.history = Some(History::open_in_memory());
            let (first, second) = if rooms { ("first@example", "second@example") } else { ("alice@example", "bob@example") };
            if rooms {
                for jid in [first, second] { let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: jid.into(), own_nick: "me".into(), members: Vec::new() }); }
                s.saved_config.rooms = vec![first.into(), second.into()];
                let _ = s.update(Message::SelectRoom(first.into()));
            } else { s.chats.push(chat(second, &[])); }
            s.message_input = text_editor::Content::with_text("first private draft");
            s.saved_config.drafts.insert(second.into(), "second private draft".into());
            let queued = transcript_clicks(&s, Some(if rooms { "Leave" } else { "Close" }), 1);
            assert!(!queued.is_empty());
            let _ = s.update(if rooms { Message::SelectRoom(second.into()) } else { Message::SelectChat(second.into()) });
            if return_to_original { let _ = s.update(if rooms { Message::SelectRoom(first.into()) } else { Message::SelectChat(first.into()) }); }
            let target = s.active_conv_jid();
            let draft = s.message_input.text();
            let (tx, mut rx) = tokio::sync::mpsc::channel(10);
            s.xmpp_cmd_tx = Some(tx);
            for message in queued { let _ = s.update(message); }
            assert_eq!(if rooms { s.rooms.len() } else { s.chats.len() }, 2);
            assert_eq!(s.active_conv_jid(), target);
            assert_eq!(s.message_input.text(), draft);
            if rooms {
                assert_eq!(s.saved_config.rooms, [first, second]);
                assert!(rx.try_recv().is_err(), "a stale button cannot send a leave command");
            }
        }
    }
}

#[test]
fn conversation_close_mouse_press_cannot_finish_in_another_conversation() {
    use iced::advanced::widget::Operation;
    struct Target { label: &'static str, bounds: Option<iced::Rectangle> }
    impl Operation for Target {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) {
            if text == self.label { self.bounds = Some(bounds); }
        }
    }
    for rooms in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.history = Some(History::open_in_memory());
        let (first, second) = if rooms { ("first@example", "second@example") } else { ("alice@example", "bob@example") };
        if rooms {
            for jid in [first, second] { let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: jid.into(), own_nick: "me".into(), members: Vec::new() }); }
            let _ = s.update(Message::SelectRoom(first.into()));
        } else { s.chats.push(chat(second, &[])); }
        let r = renderer();
        let size = iced::Size::new(600.0, 400.0);
        let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
        let mut tree;
        let point;
        {
            let mut widget = crate::ui::chat::view(&s);
            tree = Tree::new(&widget);
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut target = Target { label: if rooms { "Leave" } else { "Close" }, bounds: None };
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut target);
            point = target.bounds.unwrap().center();
            let mut messages = Vec::new();
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut messages), &bounds);
            assert!(messages.is_empty());
        }
        let _ = s.update(if rooms { Message::SelectRoom(second.into()) } else { Message::SelectChat(second.into()) });
        let messages = {
            let mut widget = crate::ui::chat::view(&s);
            tree.diff(widget.as_widget());
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut messages = Vec::new();
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)),
                Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut messages), &bounds);
            messages
        };
        for message in messages { let _ = s.update(message); }
        assert_eq!(if rooms { s.rooms.len() } else { s.chats.len() }, 2);
        assert_eq!(s.active_conv_jid().as_deref(), Some(second));
    }
}

#[test]
fn current_conversation_close_buttons_and_shortcuts_target_only_the_visible_owner() {
    for rooms in [false, true] {
        for button in [false, true] {
            let mut s = state();
            s.state = AppState::Connected;
            s.history = Some(History::open_in_memory());
            let (first, second) = if rooms { ("first@example", "second@example") } else { ("alice@example", "bob@example") };
            if rooms {
                for jid in [first, second] { let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: jid.into(), own_nick: format!("nick-{jid}"), members: Vec::new() }); }
                s.saved_config.rooms = vec![first.into(), second.into()];
                let _ = s.update(Message::SelectRoom(first.into()));
            } else { s.chats.push(chat(second, &[])); }
            s.message_input = text_editor::Content::with_text("first preserved draft");
            let shortcut = |context| {
                let iced::Event::Keyboard(event) = composer_key(iced::keyboard::Key::Character("w".into()), None,
                    iced::keyboard::Modifiers::COMMAND) else { unreachable!() };
                crate::subscription::keyboard_event(context, event)
            };
            let stale = shortcut(s.composer_id);
            let _ = s.update(if rooms { Message::SelectRoom(second.into()) } else { Message::SelectChat(second.into()) });
            let _ = s.update(stale);
            assert_eq!(if rooms { s.rooms.len() } else { s.chats.len() }, 2);
            let (tx, mut rx) = tokio::sync::mpsc::channel(10);
            s.xmpp_cmd_tx = Some(tx);
            let events = if button { transcript_clicks(&s, Some(if rooms { "Leave" } else { "Close" }), 1) } else { vec![shortcut(s.composer_id)] };
            assert!(!events.is_empty());
            for event in events { let _ = s.update(event); }
            if rooms && !button {
                // Cmd/Ctrl-W never leaves a room; that needs the Leave button.
                assert_eq!(s.rooms.len(), 2);
                assert_eq!(s.saved_config.rooms, [first, second]);
                assert!(rx.try_recv().is_err());
                continue;
            }
            assert_eq!(if rooms { s.rooms.len() } else { s.chats.len() }, 1);
            assert_eq!(s.saved_config.drafts.get(first).map(String::as_str), Some("first preserved draft"));
            if rooms {
                assert_eq!(s.rooms[0].jid, first);
                assert_eq!(s.saved_config.rooms, [first]);
                assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::LeaveRoom { room, nick }
                    if room == second && nick == format!("nick-{second}")));
                assert!(rx.try_recv().is_err());
            } else {
                assert_eq!(s.chats[0].jid, first);
                assert_eq!(s.message_input.text(), "first preserved draft");
            }
        }
    }
}

#[test]
fn close_shortcut_does_not_leave_a_conversation_hidden_by_the_join_panel() {
    let mut s = state();
    s.state = AppState::Connected;
    s.history = Some(History::open_in_memory());
    s.message_input = text_editor::Content::with_text("preserved while joining");
    let _ = s.update(Message::ShowJoinPanel);
    let iced::Event::Keyboard(event) = composer_key(iced::keyboard::Key::Character("w".into()), None, iced::keyboard::Modifiers::COMMAND) else { unreachable!() };
    let close = crate::subscription::keyboard_event(s.composer_id, event);
    let _ = s.update(close);
    assert_eq!(s.chats.len(), 1);
    assert!(s.show_join_panel);
    assert_eq!(s.message_input.text(), "preserved while joining");
}

fn sidebar_clicks(s: &Snack, label: &str) -> Vec<Message> {
    use iced::advanced::widget::Operation;
    struct Target<'a> { label: &'a str, bounds: Option<iced::Rectangle> }
    impl Operation for Target<'_> {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) {
            if text == self.label { self.bounds = Some(bounds); }
        }
    }
    let mut widget = crate::ui::sidebar::view(s);
    let mut tree = Tree::new(&widget);
    let r = renderer();
    let size = iced::Size::new(200.0, 600.0);
    let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
    let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
    let mut target = Target { label, bounds: None };
    widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut target);
    let point = target.bounds.unwrap_or_else(|| panic!("sidebar target {label}")).center();
    let mut messages = Vec::new();
    for event in [iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left), iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)] {
        widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(event), Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
            &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut messages), &bounds);
    }
    messages
}

#[test]
fn stale_sidebar_callbacks_cannot_act_on_a_new_account() {
    for label in ["Disconnect", "Remove auto-login", "alice@example"] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("old@example/device".into());
        s.saved_config.jid = Some("old@example/device".into());
        s.saved_config.draft_account = Some("old@example".into());
        let (tx, channel) = xmpp::new_command_channel("old@example/device".into(), "test old secret".into());
        s.xmpp_cmd_tx = Some(tx);
        s.xmpp_cmd_rx = Some(channel);
        let queued = sidebar_clicks(&s, label);
        assert!(!queued.is_empty());
        let _ = s.update(Message::Disconnect);
        let channel = s.xmpp_cmd_rx.clone().unwrap();
        let _ = s.update(Message::ScopedXmppEvent { channel, event: xmpp::XmppEvent::Stopped });
        let _ = s.update(Message::JidInputChanged("new@example/device".into()));
        let _ = s.update(Message::PasswordInputChanged("test new secret".into()));
        let _ = s.update(Message::RememberMeToggled(true));
        let _ = s.update(Message::Connect);
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Connected)); // Never run the subscription/network.
        s.chats.push(chat("alice@example", &[]));
        s.chats.push(chat("bob@example", &[]));
        let _ = s.update(Message::SelectChat("bob@example".into()));
        s.message_input = text_editor::Content::with_text("new account draft");
        let current = s.xmpp_cmd_rx.clone();
        for message in queued { let _ = s.update(message); }
        assert!(s.state == AppState::Connected, "{label}");
        assert_eq!(s.xmpp_cmd_rx, current);
        assert_eq!(s.active_conv_jid().as_deref(), Some("bob@example"));
        assert_eq!(s.message_input.text(), "new account draft");
        assert_eq!(s.saved_config.jid.as_deref(), Some("new@example/device"));
        assert_eq!(storage::load_password("new@example/device").unwrap().as_deref(), Some("test new secret"));
    }
}

#[test]
fn sidebar_mouse_press_stays_with_its_room_or_chat_when_entries_shift() {
    use iced::advanced::widget::Operation;
    struct Target<'a> { label: &'a str, bounds: Option<iced::Rectangle> }
    impl Operation for Target<'_> {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) {
            if text == self.label { self.bounds = Some(bounds); }
        }
    }
    for rooms in [false, true] {
        for remove_first in [false, true] {
            let mut s = state();
            s.state = AppState::Connected;
            s.connected_jid = Some("me@example/device".into());
            s.history = Some(History::open_in_memory());
            s.chats.clear();
            s.body_editors.clear();
            s.active = None;
            if rooms {
                for jid in ["first@example", "second@example", "third@example"] { let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: jid.into(), own_nick: "me".into(), members: Vec::new() }); }
            } else {
                for jid in ["first@example", "second@example", "third@example"] { s.chats.push(chat(jid, &[])); }
                let _ = s.update(Message::SelectChat("first@example".into()));
            }
            let r = renderer();
            let size = iced::Size::new(200.0, 600.0);
            let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
            let mut tree;
            let point;
            {
                let mut widget = crate::ui::sidebar::view(&s);
                tree = Tree::new(&widget);
                let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
                let mut target = Target { label: if rooms { "second" } else { "second@example" }, bounds: None };
                widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut target);
                point = target.bounds.unwrap().center();
                let mut messages = Vec::new();
                widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                    Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                    &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut messages), &bounds);
                assert!(messages.is_empty());
            }
            if remove_first {
                let _ = s.update(if rooms { Message::XmppEvent(xmpp::XmppEvent::RoomLeft("first@example".into())) } else { Message::CloseChat });
            } else if rooms { s.rooms[1].unread = true; } else { s.chats[1].unread = true; }
            let messages = {
                let mut widget = crate::ui::sidebar::view(&s);
                tree.diff(widget.as_widget());
                let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
                let mut messages = Vec::new();
                widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)),
                    Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                    &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut messages), &bounds);
                messages
            };
            for message in messages { let _ = s.update(message); }
            let expected = if rooms && remove_first { None } else { Some("second@example") };
            assert_eq!(s.active_conv_jid().as_deref(), expected);
        }
    }
}

#[test]
fn sidebar_disconnect_press_cannot_finish_in_a_new_login_session() {
    use iced::advanced::widget::Operation;
    struct Target(Option<iced::Rectangle>);
    impl Operation for Target {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) {
            if text == "Disconnect" { self.0 = Some(bounds); }
        }
    }
    for new_session in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        let (tx, channel) = xmpp::new_command_channel("me@example/device".into(), "test secret".into());
        s.xmpp_cmd_tx = Some(tx);
        s.xmpp_cmd_rx = Some(channel);
        let r = renderer();
        let size = iced::Size::new(200.0, 600.0);
        let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
        let mut tree;
        let point;
        {
            let mut widget = crate::ui::sidebar::view(&s);
            tree = Tree::new(&widget);
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut target = Target(None);
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut target);
            point = target.0.unwrap().center();
            let mut messages = Vec::new();
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut messages), &bounds);
            assert!(messages.is_empty());
        }
        if new_session {
            // Same profile, but a fresh authenticated command channel.
            let (tx, channel) = xmpp::new_command_channel("me@example/device".into(), "test secret".into());
            s.xmpp_cmd_tx = Some(tx);
            s.xmpp_cmd_rx = Some(channel);
        }
        let messages = {
            let mut widget = crate::ui::sidebar::view(&s);
            tree.diff(widget.as_widget());
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut messages = Vec::new();
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)),
                Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut messages), &bounds);
            messages
        };
        for message in messages { let _ = s.update(message); }
        assert_eq!(s.disconnecting, !new_session);
        if !new_session {
            let channel = s.xmpp_cmd_rx.clone().unwrap();
            let _ = s.update(Message::ScopedXmppEvent { channel, event: xmpp::XmppEvent::Stopped });
        }
        assert_eq!(s.state == AppState::Connected, new_session);
    }
}

#[test]
fn current_sidebar_account_actions_keep_their_normal_effects() {
    for forget in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        s.saved_config.jid = Some("me@example/device".into());
        s.remember_me = true;
        storage::save_password("me@example/device", "test stored secret").unwrap();
        let (tx, channel) = xmpp::new_command_channel("me@example/device".into(), "test stored secret".into());
        s.xmpp_cmd_tx = Some(tx);
        s.xmpp_cmd_rx = Some(channel);
        let events = sidebar_clicks(&s, if forget { "Remove auto-login" } else { "Disconnect" });
        assert!(!events.is_empty());
        for event in events { let _ = s.update(event); }
        if forget {
            assert!(s.state == AppState::Connected);
            assert!(s.saved_config.jid.is_none());
            assert!(storage::load_password("me@example/device").unwrap().is_none());
        } else {
            assert!(s.disconnecting);
            let channel = s.xmpp_cmd_rx.clone().unwrap();
            let _ = s.update(Message::ScopedXmppEvent { channel, event: xmpp::XmppEvent::Stopped });
            assert!(s.state == AppState::Login);
            assert_eq!(s.saved_config.jid.as_deref(), Some("me@example/device"));
            assert_eq!(storage::load_password("me@example/device").unwrap().as_deref(), Some("test stored secret"));
        }
    }
}

fn login_clicks(s: &Snack, label: &str) -> Vec<Message> {
    use iced::advanced::widget::Operation;
    struct Target<'a> { label: &'a str, bounds: Option<iced::Rectangle> }
    impl Operation for Target<'_> {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) {
            if text == self.label { self.bounds = Some(bounds); }
        }
    }
    let mut widget = crate::ui::account::view(s);
    let mut tree = Tree::new(&widget);
    let r = renderer();
    let size = iced::Size::new(600.0, 600.0);
    let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
    let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
    let mut target = Target { label, bounds: None };
    widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut target);
    let point = target.bounds.unwrap_or_else(|| panic!("login target {label}")).center();
    let mut messages = Vec::new();
    for event in [iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left), iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)] {
        widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(event), Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
            &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut messages), &bounds);
    }
    messages
}

fn login_input(s: &Snack, field: &'static str, event: iced::Event) -> Vec<Message> {
    let mut widget = crate::ui::account::view(s);
    let mut tree = Tree::new(&widget);
    let r = renderer();
    let size = iced::Size::new(600.0, 600.0);
    let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
    let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
    let mut focus = iced::advanced::widget::operation::focusable::focus::<()>(iced::widget::Id::new(field));
    widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut focus);
    let mut messages = Vec::new();
    widget.as_widget_mut().update(&mut tree, &event, Layout::new(&node), iced::mouse::Cursor::Unavailable, &r,
        &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut messages), &bounds);
    messages
}

#[test]
fn stale_login_checkbox_cannot_delete_a_later_logins_saved_credentials() {
    for logged_out_again in [false, true] {
        let mut s = Snack::test_state();
        s.remember_me = true;
        s.jid_input = "old@example".into();
        let queued = login_clicks(&s, "Auto-login");
        assert!(!queued.is_empty());
        let _ = s.update(Message::JidInputChanged("new@example".into()));
        let _ = s.update(Message::PasswordInputChanged("test new secret".into()));
        let _ = s.update(Message::RememberMeToggled(true));
        let _ = s.update(Message::Connect);
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Connected)); // No subscription/network is run.
        if logged_out_again { let _ = s.update(Message::Disconnect); complete_worker_stop(&mut s); }
        for message in queued { let _ = s.update(message); }
        assert_eq!(s.saved_config.jid.as_deref(), Some("new@example"));
        assert_eq!(storage::load_password("new@example").unwrap().as_deref(), Some("test new secret"));
        assert!(s.remember_me);
    }
}

#[test]
fn stale_login_cancel_cannot_stop_success_or_a_new_attempt() {
    for successful in [false, true] {
        let mut s = Snack::test_state();
        let _ = s.update(Message::JidInputChanged("old@example".into()));
        let _ = s.update(Message::PasswordInputChanged("old test secret".into()));
        let _ = s.update(Message::Connect);
        let queued = login_clicks(&s, "Cancel");
        assert!(!queued.is_empty());
        if successful { let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Connected)); }
        else {
            let _ = s.update(Message::CancelConnect);
            complete_worker_stop(&mut s);
            let _ = s.update(Message::JidInputChanged("new@example".into()));
            let _ = s.update(Message::PasswordInputChanged("new test secret".into()));
            let _ = s.update(Message::Connect);
        }
        let channel = s.xmpp_cmd_rx.clone();
        for message in queued { let _ = s.update(message); }
        assert_eq!(s.xmpp_cmd_rx, channel);
        assert!(s.state == if successful { AppState::Connected } else { AppState::Connecting });
    }
}

#[test]
fn stale_login_edits_cannot_overwrite_the_form_after_cancellation() {
    for field in [crate::app::ACCOUNT_JID_INPUT_ID, crate::app::ACCOUNT_PASSWORD_INPUT_ID] {
        let mut s = Snack::test_state();
        let _ = s.update(Message::JidInputChanged("old@example".into()));
        let _ = s.update(Message::PasswordInputChanged("old test secret".into()));
        let queued = login_input(&s, field, composer_key(iced::keyboard::Key::Character("x".into()), Some("x"), iced::keyboard::Modifiers::NONE));
        assert!(!queued.is_empty());
        let _ = s.update(Message::Connect);
        let _ = s.update(Message::CancelConnect);
        complete_worker_stop(&mut s);
        let _ = s.update(Message::JidInputChanged("new@example".into()));
        let _ = s.update(Message::PasswordInputChanged("new test secret".into()));
        for message in queued { let _ = s.update(message); }
        assert_eq!(s.jid_input, "new@example");
        assert_eq!(s.password_input, "new test secret");
    }
}

#[test]
fn current_login_form_supports_batched_edits_submit_cancel_and_saved_credentials() {
    let mut s = Snack::test_state();
    let mut events = login_input(&s, crate::app::ACCOUNT_JID_INPUT_ID,
        iced::Event::InputMethod(iced::advanced::input_method::Event::Commit("user@example".into())));
    events.extend(login_input(&s, crate::app::ACCOUNT_PASSWORD_INPUT_ID,
        iced::Event::InputMethod(iced::advanced::input_method::Event::Commit("test secret".into()))));
    events.extend(login_clicks(&s, "Auto-login"));
    events.extend(login_input(&s, crate::app::ACCOUNT_PASSWORD_INPUT_ID,
        composer_key(iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter), None, iced::keyboard::Modifiers::NONE)));
    assert_eq!(events.len(), 4);
    for event in events { let _ = s.update(event); }
    assert!(s.state == AppState::Connecting);
    assert_eq!(s.connected_jid.as_deref(), Some("user@example"));
    assert_eq!(s.pending_save_password.as_deref(), Some("test secret"));
    assert!(s.remember_me);
    let first = s.xmpp_cmd_rx.clone();
    let cancel = login_clicks(&s, "Cancel");
    assert!(!cancel.is_empty());
    for event in cancel { let _ = s.update(event); }
    complete_worker_stop(&mut s);
    assert!(s.state == AppState::Login);
    assert_eq!(s.password_input, "test secret");
    let connect = login_clicks(&s, "Connect");
    for event in connect { let _ = s.update(event); }
    assert!(s.state == AppState::Connecting);
    assert_ne!(s.xmpp_cmd_rx, first);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Connected)); // No real subscription or login.
    let channel = s.xmpp_cmd_rx.clone();
    let _ = s.update(Message::RememberMeToggled(false));
    let _ = s.update(Message::CancelConnect);
    assert!(s.state == AppState::Connected);
    assert_eq!(s.xmpp_cmd_rx, channel);
    assert_eq!(s.saved_config.jid.as_deref(), Some("user@example"));
    assert_eq!(storage::load_password("user@example").unwrap().as_deref(), Some("test secret"));
}

#[test]
fn login_button_mouse_press_cannot_finish_as_a_different_form_action() {
    use iced::advanced::widget::Operation;
    struct Target<'a> { label: &'a str, bounds: Option<iced::Rectangle> }
    impl Operation for Target<'_> {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) {
            if text == self.label { self.bounds = Some(bounds); }
        }
    }
    for change in ["start", "cancel", "redraw"] {
        let mut s = Snack::test_state();
        let _ = s.update(Message::JidInputChanged("user@example".into()));
        let _ = s.update(Message::PasswordInputChanged("test secret".into()));
        if change == "cancel" { let _ = s.update(Message::Connect); }
        let r = renderer();
        let size = iced::Size::new(600.0, 600.0);
        let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
        let mut tree;
        let point;
        {
            let mut widget = crate::ui::account::view(&s);
            tree = Tree::new(&widget);
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut target = Target { label: if change == "cancel" { "Cancel" } else { "Connect" }, bounds: None };
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut target);
            point = target.bounds.unwrap().center();
            let mut output = Vec::new();
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
            assert!(output.is_empty());
        }
        let _ = s.update(match change {
            "start" => Message::Connect,
            "cancel" => Message::CancelConnect,
            _ => Message::JidInputChanged("edited@example".into()),
        });
        if change == "cancel" { complete_worker_stop(&mut s); }
        let expected_channel = s.xmpp_cmd_rx.clone();
        let output = {
            let mut widget = crate::ui::account::view(&s);
            tree.diff(widget.as_widget());
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut output = Vec::new();
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)),
                Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
            output
        };
        for event in output { let _ = s.update(event); }
        if change == "redraw" {
            assert!(s.state == AppState::Connecting);
            assert_eq!(s.connected_jid.as_deref(), Some("edited@example"));
        } else {
            assert_eq!(s.xmpp_cmd_rx, expected_channel);
            assert!(s.state == if change == "start" { AppState::Connecting } else { AppState::Login });
        }
    }
}

fn app_clicks(s: &Snack, label: &str) -> Vec<Message> {
    use iced::advanced::widget::Operation;
    struct Target<'a> { label: &'a str, bounds: Option<iced::Rectangle> }
    impl Operation for Target<'_> {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) {
            if text == self.label { self.bounds = Some(bounds); }
        }
    }
    let mut widget = s.view();
    let mut tree = Tree::new(&widget);
    let r = renderer();
    let size = iced::Size::new(800.0, 600.0);
    let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
    let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
    let mut target = Target { label, bounds: None };
    widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut target);
    let point = target.bounds.unwrap_or_else(|| panic!("save notice target {label}")).center();
    let mut messages = Vec::new();
    for event in [iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left), iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)] {
        widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(event), Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
            &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut messages), &bounds);
    }
    messages
}

#[test]
fn stale_save_notice_buttons_cannot_act_on_a_later_close_attempt() {
    for label in ["Close without saving", "Keep open", "Retry save and close"] {
        let mut s = state();
        s.state = AppState::Connected;
        s.message_input = text_editor::Content::with_text("first unsaved draft");
        let path = storage::config_path().unwrap();
        if path.is_file() { std::fs::remove_file(&path).unwrap(); }
        std::fs::create_dir_all(&path).unwrap();
        let window = iced::window::Id::unique();
        assert_eq!(s.update(Message::WindowCloseRequested(window)).units(), 0);
        let queued = app_clicks(&s, label);
        assert!(!queued.is_empty());
        let _ = s.update(Message::KeepWindowOpen);
        s.message_input = text_editor::Content::with_text("later private draft");
        assert_eq!(s.update(Message::WindowCloseRequested(window)).units(), 0);
        if label == "Retry save and close" { std::fs::remove_dir(&path).unwrap(); }
        for message in queued { assert_eq!(s.update(message).units(), 0, "{label}: stale notice must not close the window"); }
        assert!(s.pending_close.is_some(), "{label}: old Keep open cannot cancel a newer close request");
        assert_eq!(s.message_input.text(), "later private draft");
        let current = app_clicks(&s, label);
        let mut units = 0;
        for message in current { units += s.update(message).units(); }
        assert_eq!(units, if label == "Keep open" { 0 } else { 1 });
        assert!(s.pending_close.is_none());
        if label == "Retry save and close" { assert_eq!(storage::load().drafts["alice@example"], "later private draft"); }
    }
}

#[test]
fn a_nonclosing_retry_cannot_close_a_later_save_prompt() {
    let mut s = state();
    s.state = AppState::Connected;
    s.message_input = text_editor::Content::with_text("unsaved draft");
    let path = storage::config_path().unwrap();
    if path.is_file() { std::fs::remove_file(&path).unwrap(); }
    std::fs::create_dir_all(&path).unwrap();
    let _ = s.update(Message::RetrySettingsSave);
    assert!(s.settings_save_error.is_some());
    assert!(s.pending_close.is_none());
    let queued = app_clicks(&s, "Retry saving");
    assert!(!queued.is_empty());
    let window = iced::window::Id::unique();
    let _ = s.update(Message::WindowCloseRequested(window));
    std::fs::remove_dir(&path).unwrap();
    for message in queued { assert_eq!(s.update(message).units(), 0); }
    assert!(s.pending_close.is_some());
    for message in app_clicks(&s, "Retry save and close") {
        assert_eq!(s.update(message).units(), 1);
    }
    assert!(s.pending_close.is_none());
    assert_eq!(storage::load().drafts["alice@example"], "unsaved draft");
}

#[test]
fn failed_save_retry_replaces_the_close_prompt_even_for_the_same_window() {
    let mut s = state();
    s.state = AppState::Connected;
    let path = storage::config_path().unwrap();
    if path.is_file() { std::fs::remove_file(&path).unwrap(); }
    std::fs::create_dir_all(&path).unwrap();
    let window = iced::window::Id::unique();
    let _ = s.update(Message::WindowCloseRequested(window));
    let discard = app_clicks(&s, "Close without saving");
    let first = s.pending_close.unwrap().token;
    for message in app_clicks(&s, "Retry save and close") {
        assert_eq!(s.update(message).units(), 0);
    }
    assert_ne!(s.pending_close.unwrap().token, first);
    for message in discard { assert_eq!(s.update(message).units(), 0); }
    assert!(s.pending_close.is_some());
    for message in app_clicks(&s, "Close without saving") {
        assert_eq!(s.update(message).units(), 1);
    }
    assert!(s.pending_close.is_none());
}

#[test]
fn a_discard_mouse_press_cannot_finish_in_a_replacement_close_prompt() {
    use iced::advanced::widget::Operation;
    struct Target(Option<iced::Rectangle>);
    impl Operation for Target {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) {
            if text == "Close without saving" { self.0 = Some(bounds); }
        }
    }
    for renew in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.message_input = text_editor::Content::with_text("unsaved draft");
        let path = storage::config_path().unwrap();
        if path.is_file() { std::fs::remove_file(&path).unwrap(); }
        std::fs::create_dir_all(&path).unwrap();
        let window = iced::window::Id::unique();
        let _ = s.update(Message::WindowCloseRequested(window));
        let r = renderer();
        let size = iced::Size::new(800.0, 600.0);
        let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
        let mut tree;
        let point;
        {
            let mut widget = s.view();
            tree = Tree::new(&widget);
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut target = Target(None);
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut target);
            point = target.0.unwrap().center();
            let mut output = Vec::new();
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
            assert!(output.is_empty());
        }
        if renew {
            let _ = s.update(Message::KeepWindowOpen);
            let _ = s.update(Message::WindowCloseRequested(window));
        }
        let output = {
            let mut widget = s.view();
            tree.diff(widget.as_widget());
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut output = Vec::new();
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)),
                Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
            output
        };
        let mut units = 0;
        for message in output { units += s.update(message).units(); }
        assert_eq!(units, if renew { 0 } else { 1 });
        assert_eq!(s.pending_close.is_some(), renew);
    }
}

#[test]
fn canonical_bound_identity_labels_own_messages_and_authorizes_server_reports_without_changing_profile() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("Login@alias.example/requested".into());
    s.saved_config.draft_account = Some("Login@alias.example".into());
    s.history = Some(History::open_in_memory());
    let (tx, _rx) = tokio::sync::mpsc::channel(20);
    s.xmpp_cmd_tx = Some(tx);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::BoundJid("canonical@server.example/assigned".into())));
    assert_eq!(s.own_jid(), Some("canonical@server.example/assigned"));
    assert_eq!(s.title(), "Snack — canonical@server.example/assigned");
    assert_eq!(s.connected_jid.as_deref(), Some("Login@alias.example/requested"));
    assert_eq!(s.saved_config.draft_account.as_deref(), Some("Login@alias.example"));

    s.message_input = text_editor::Content::with_text("local outgoing");
    let _ = s.update(Message::SendMessage);
    assert!(s.chats[0].messages.iter().any(|message| matches!(message, room::message::Message::Chat { from, body, .. } if from == "Login" && body == "local outgoing")));
    s.mam_pending.insert("archive".into(), crate::app::ArchiveContext { conversation: "alice@example".into(), is_muc: false });
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage { to: None,
        query_id: "archive".into(), from: "canonical@server.example/other-device".into(), body: "own archived".into(),
        timestamp: chrono::Utc::now(), stanza_id: Some("own-archive-id".into()),
        origin_id: None,
    }));
    let stored = s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap();
    assert!(stored.iter().any(|message| message.sender == "Login" && message.body == "own archived"));

    let outgoing = s.history.as_ref().unwrap().load_outbox().unwrap().remove(0);
    let _ = s.update(delivery_error(&outgoing.origin_id, "alias.example", "cancel"));
    assert!(s.rejected_sends.is_empty());
    let _ = s.update(delivery_error(&outgoing.origin_id, "server.example", "cancel"));
    assert_eq!(s.rejected_sends.len(), 1);
    assert_eq!(s.rejected_sends[0].account.as_deref(), Some("Login@alias.example"));
    assert_eq!(s.rejected_sends[0].body, "local outgoing");
    let _ = s.update(Message::Disconnect);
    assert!(s.bound_jid.is_none());
    assert!(s.own_jid().is_none());
}

// Most tests begin after a requested room entry. Stale/unsolicited-event tests
// deliberately call update directly, without registering this intent.
// Replays of possibly delivered sends first look them up in the archive.
// Answer that lookup with an empty archive and return the next command.
fn next_send_after_delivery_check(s: &mut Snack, rx: &mut tokio::sync::mpsc::Receiver<xmpp::XmppCommand>) -> xmpp::XmppCommand {
    let command = rx.try_recv().unwrap().into_inner();
    let xmpp::XmppCommand::QueryArchive { query_id, forward: true, .. } = &command else { return command; };
    assert!(query_id.starts_with("mam-d-"), "expected a delivery check, got {command:?}");
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: query_id.clone(), complete: true, first: None, last: None }));
    rx.try_recv().unwrap().into_inner()
}

// Self presence, then (empty) join history, then the subject that ends it.
fn confirm_room_entry(s: &mut Snack, event: xmpp::XmppEvent) -> iced::Task<Message> {
    let xmpp::XmppEvent::RoomJoined { room, .. } = &event else { panic!("expected room entry fixture"); };
    let room = room.clone();
    s.room_connections.insert(room.clone(), RoomConnection::Joining);
    let joined = s.update(Message::XmppEvent(event));
    let topic = s.rooms.iter().find(|known| known.jid == room).map(|known| known.topic.clone()).unwrap_or_default();
    let subject = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomSubject { room, subject: topic }));
    iced::Task::batch([joined, subject])
}

fn completion_state(names: &[&str]) -> Snack {
    let mut s = state();
    s.state = AppState::Connected;
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined {
        room: "room@example".into(), own_nick: "me".into(), members: Vec::new(),
    });
    let _ = s.update(Message::SelectRoom("room@example".into()));
    s.rooms[0].users = names.iter().map(|name| room::user::User {
        jid: None, name: (*name).into(), show: None, status: None,
    }).collect();
    s
}

fn search_input_events(s: &Snack, events: Vec<iced::Event>) -> Vec<Message> {
    let mut widget = crate::ui::chat::view(s);
    let mut tree = Tree::new(&widget);
    let r = renderer();
    let size = iced::Size::new(800.0, 600.0);
    let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
    let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
    let mut focus = iced::advanced::widget::operation::focusable::focus::<()>(iced::widget::Id::new(crate::app::FIND_INPUT_ID));
    widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut focus);
    let mut output = Vec::new();
    for event in events {
        widget.as_widget_mut().update(&mut tree, &event, Layout::new(&node), iced::mouse::Cursor::Unavailable, &r,
            &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
    }
    output
}

fn contextual_shortcut(s: &Snack, key: iced::keyboard::Key, modifiers: iced::keyboard::Modifiers) -> Message {
    let event = composer_key(key.clone(), None, modifiers);
    if key == iced::keyboard::Key::Character("f".into()) {
        crate::subscription::scope_shortcut(s.shortcut_context(), crate::subscription::global_event(event).unwrap())
    } else {
        let iced::Event::Keyboard(event) = event else { unreachable!() };
        crate::subscription::session_keyboard_event(s.shortcut_context(), s.navigation_context(), s.focus_context(), event)
    }
}

#[test]
fn queued_copy_cannot_copy_a_replacement_selection_in_the_same_conversation() {
    let mut s = state();
    s.state = AppState::Connected;
    let _ = s.update(Message::SelectMessage { id: 1 });
    let queued = contextual_shortcut(&s, iced::keyboard::Key::Character("c".into()), iced::keyboard::Modifiers::COMMAND);
    let _ = s.update(Message::ClearSelection);
    let _ = s.update(Message::SelectMessage { id: 2 });
    assert_eq!(s.update(queued).units(), 0, "old copy must not write a newly selected message to the clipboard");
    assert!(s.selected_msgs.contains(&2));
    let current = contextual_shortcut(&s, iced::keyboard::Key::Character("c".into()), iced::keyboard::Modifiers::COMMAND);
    assert_eq!(s.update(current).units(), 2);
    assert!(s.selected_msgs.is_empty());
}

#[test]
fn queued_selection_toolbar_actions_cannot_target_a_replacement_selection() {
    for label in ["Copy", "Clear"] {
        let mut s = state();
        s.state = AppState::Connected;
        let _ = s.update(Message::SelectMessage { id: 1 });
        let queued = transcript_clicks(&s, Some(label), 1);
        assert!(!queued.is_empty());
        let _ = s.update(Message::ClearSelection);
        let _ = s.update(Message::SelectMessage { id: 2 });
        for event in queued { assert_eq!(s.update(event).units(), 0, "{label}"); }
        assert_eq!(s.selected_msgs, [2].into_iter().collect());
        let current = transcript_clicks(&s, Some(label), 1);
        let units: usize = current.into_iter().map(|event| s.update(event).units()).sum();
        assert_eq!(units, if label == "Copy" { 2 } else { 0 });
        assert!(s.selected_msgs.is_empty());
    }
}

#[test]
fn free_text_copy_tracks_the_range_and_survives_unrelated_changes() {
    let mut s = state();
    s.state = AppState::Connected;
    let _ = s.update(Message::BodyEditorAction { id: 1, action: text_editor::Action::SelectAll });
    let old = contextual_shortcut(&s, iced::keyboard::Key::Character("c".into()), iced::keyboard::Modifiers::COMMAND);
    let selection = s.selection_id;
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::BoundJid("me@example/device".into())));
    let _ = s.update(Message::ModifiersUpdate { shift: false, primary: false });
    assert_eq!(s.selection_id, selection);
    assert_eq!(s.update(old).units(), 1);
    let old = contextual_shortcut(&s, iced::keyboard::Key::Character("c".into()), iced::keyboard::Modifiers::COMMAND);
    let _ = s.update(Message::BodyEditorAction { id: 1, action: text_editor::Action::Move(text_editor::Motion::DocumentStart) });
    let _ = s.update(Message::BodyEditorAction { id: 2, action: text_editor::Action::SelectAll });
    assert_ne!(s.selection_id, selection);
    assert_eq!(s.update(old).units(), 0);
    assert_eq!(s.body_editors[&2].selection().as_deref(), Some("bravo"));
    let current = contextual_shortcut(&s, iced::keyboard::Key::Character("c".into()), iced::keyboard::Modifiers::COMMAND);
    assert_eq!(s.update(current).units(), 1);
    assert_eq!(s.body_editors[&2].selection().as_deref(), Some("bravo"));
    let old = contextual_shortcut(&s, iced::keyboard::Key::Character("c".into()), iced::keyboard::Modifiers::COMMAND);
    let _ = s.update(Message::BodyEditorAction { id: 2, action: text_editor::Action::Select(text_editor::Motion::Left) });
    assert_eq!(s.update(old).units(), 0, "changing only the range in the same message invalidates old Copy");
    assert_eq!(s.body_editors[&2].selection().as_deref(), Some("brav"));
    let old = contextual_shortcut(&s, iced::keyboard::Key::Character("c".into()), iced::keyboard::Modifiers::COMMAND);
    let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Insert('x'))));
    assert_eq!(s.update(old).units(), 0, "typing in the composer deliberately clears transcript selection");
}

#[test]
fn focused_body_copy_keeps_the_selection_that_produced_its_callback() {
    use iced::advanced::widget::operation::{Operation, Focusable};
    struct FocusBody(bool);
    impl Operation for FocusBody {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn focusable(&mut self, id: Option<&iced::widget::Id>, _bounds: iced::Rectangle, state: &mut dyn Focusable) {
            if id.is_none() && !self.0 { state.focus(); self.0 = true; }
        }
    }
    let mut s = state();
    s.state = AppState::Connected;
    let _ = s.update(Message::BodyEditorAction { id: 1, action: text_editor::Action::SelectAll });
    let output = {
        let mut widget = crate::ui::chat::view(&s);
        let mut tree = Tree::new(&widget);
        let r = renderer();
        let size = iced::Size::new(800.0, 600.0);
        let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
        let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
        let mut focus = FocusBody(false);
        widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut focus);
        assert!(focus.0);
        let mut output = Vec::new();
        let event = composer_key(iced::keyboard::Key::Character("c".into()), None, iced::keyboard::Modifiers::COMMAND);
        widget.as_widget_mut().update(&mut tree, &event, Layout::new(&node), iced::mouse::Cursor::Unavailable, &r,
            &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
        output
    };
    assert_eq!(output.len(), 1);
    assert!(matches!(&output[0], Message::ScopedTranscript { action: crate::message::TranscriptAction::Copy(selection), .. }
        if *selection == s.selection_id));
    let _ = s.update(Message::ClearSelection);
    let _ = s.update(Message::BodyEditorAction { id: 2, action: text_editor::Action::SelectAll });
    for event in output { assert_eq!(s.update(event).units(), 0); }
    assert_eq!(s.body_editors[&2].selection().as_deref(), Some("bravo"));
}

#[test]
fn selection_changes_do_not_cancel_other_contextual_shortcuts() {
    let mut s = state();
    s.state = AppState::Connected;
    let find = contextual_shortcut(&s, iced::keyboard::Key::Character("f".into()), iced::keyboard::Modifiers::COMMAND);
    let _ = s.update(Message::SelectMessage { id: 1 });
    let _ = s.update(find);
    assert!(s.find.is_some());
    let escape = contextual_shortcut(&s, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape), iced::keyboard::Modifiers::NONE);
    let _ = s.update(Message::SelectMessage { id: 2 });
    let _ = s.update(escape);
    assert!(s.find.is_none());
}

#[test]
fn queued_escape_cannot_clear_a_replacement_selection() {
    let mut s = state();
    s.state = AppState::Connected;
    let _ = s.update(Message::SelectMessage { id: 1 });
    let escape = contextual_shortcut(&s, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape), iced::keyboard::Modifiers::NONE);
    let _ = s.update(Message::ClearSelection);
    let _ = s.update(Message::SelectMessage { id: 2 });
    assert_eq!(s.update(escape).units(), 0);
    assert_eq!(s.selected_msgs, [2].into_iter().collect());
}

#[test]
fn held_selection_buttons_cannot_finish_on_a_replacement_selection() {
    use iced::advanced::widget::Operation;
    struct Target<'a>(&'a str, Option<iced::Rectangle>);
    impl Operation for Target<'_> {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) {
            if text == self.0 { self.1 = Some(bounds); }
        }
    }
    for label in ["Copy", "Clear"] {
        for replace in [false, true] {
            let mut s = state();
            s.state = AppState::Connected;
            let _ = s.update(Message::SelectMessage { id: 1 });
            let r = renderer();
            let size = iced::Size::new(800.0, 600.0);
            let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
            let mut tree;
            {
                let mut widget = crate::ui::chat::view(&s);
                tree = Tree::new(&widget);
                let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
                let mut target = Target(label, None);
                widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut target);
                let point = target.1.unwrap().center();
                let mut output = Vec::new();
                widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                    Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                    &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
                assert!(output.is_empty());
            }
            if replace {
                let _ = s.update(Message::ClearSelection);
                let _ = s.update(Message::SelectMessage { id: 2 });
            } else { let _ = s.update(Message::ModifiersUpdate { shift: false, primary: false }); }
            let output = {
                let mut widget = crate::ui::chat::view(&s);
                tree.diff(widget.as_widget());
                let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
                let mut target = Target(label, None);
                widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut target);
                let point = target.1.unwrap().center();
                let mut output = Vec::new();
                widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)),
                    Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                    &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
                output
            };
            let units: usize = output.into_iter().map(|event| s.update(event).units()).sum();
            assert_eq!(units, if label == "Copy" && !replace { 2 } else { 0 }, "{label}, replace={replace}");
            assert_eq!(s.selected_msgs, if replace { [2].into_iter().collect() } else { Default::default() });
        }
    }
}

#[test]
fn old_navigation_events_cannot_move_a_new_accounts_active_conversation() {
    for composer in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("old@example/device".into());
        s.saved_config.draft_account = Some("old@example".into());
        let key = iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowDown);
        let queued = if composer { composer_events(&s, vec![composer_key(key, None, iced::keyboard::Modifiers::ALT)]) }
            else { vec![contextual_shortcut(&s, key, iced::keyboard::Modifiers::ALT)] };
        assert_eq!(queued.len(), 1);
        let _ = s.update(Message::Disconnect);
        let _ = s.update(Message::JidInputChanged("new@example/device".into()));
        let _ = s.update(Message::PasswordInputChanged("test secret".into()));
        let _ = s.update(Message::Connect);
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Connected)); // Subscription never runs.
        for jid in ["alice@example", "bob@example"] { s.chats.push(chat(jid, &[])); }
        let _ = s.update(Message::SelectChat("alice@example".into()));
        s.message_input = text_editor::Content::with_text("new account private draft");
        for message in queued { assert_eq!(s.update(message).units(), 0, "composer={composer}"); }
        assert_eq!(s.active_conv_jid().as_deref(), Some("alice@example"));
        assert_eq!(s.message_input.text(), "new account private draft");
    }
}

#[test]
fn navigation_requires_the_current_connection_even_for_the_same_profile() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("same@example/device".into());
    s.chats.push(chat("bob@example", &[]));
    let (tx, channel) = xmpp::new_command_channel("same@example/device".into(), "test secret".into());
    s.xmpp_cmd_tx = Some(tx);
    s.xmpp_cmd_rx = Some(channel);
    let key = iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowDown);
    let queued = contextual_shortcut(&s, key.clone(), iced::keyboard::Modifiers::ALT);
    let (tx, channel) = xmpp::new_command_channel("same@example/device".into(), "test secret".into());
    s.xmpp_cmd_tx = Some(tx);
    s.xmpp_cmd_rx = Some(channel);
    assert_eq!(s.update(queued).units(), 0);
    assert_eq!(s.active_conv_jid().as_deref(), Some("alice@example"));
    let current = contextual_shortcut(&s, key, iced::keyboard::Modifiers::ALT);
    let _ = s.update(current);
    assert_eq!(s.active_conv_jid().as_deref(), Some("bob@example"));
}

fn composer_with(text: &str, line: usize, column: usize) -> Snack {
    let mut s = state();
    s.message_input = text_editor::Content::with_text(text);
    s.message_input.move_to(text_editor::Cursor { position: text_editor::Position { line, column }, selection: None });
    s
}

fn caret(s: &Snack) -> (usize, usize) {
    let position = s.message_input.cursor().position;
    (position.line, position.column)
}

#[test]
fn native_word_and_line_deletion_removes_only_to_the_boundary_and_is_one_undo_step() {
    use crate::ui::composer_keys::EditCommand;
    let mut s = composer_with("hello brave world", 0, 17);
    let _ = s.update(Message::ComposerEdit(EditCommand::DeleteWordBackward));
    assert_eq!(s.message_input.text(), "hello brave ");
    let _ = s.update(Message::UndoInput);
    assert_eq!(s.message_input.text(), "hello brave world", "a word deletion undoes in one step");

    let mut s = composer_with("hello brave world", 0, 6);
    let _ = s.update(Message::ComposerEdit(EditCommand::DeleteWordForward));
    assert_eq!(s.message_input.text(), "hello  world");

    // An existing selection is deleted instead of extending it.
    let mut s = composer_with("hello brave world", 0, 9);
    s.message_input.move_to(text_editor::Cursor { position: text_editor::Position { line: 0, column: 9 },
        selection: Some(text_editor::Position { line: 0, column: 7 }) });
    let _ = s.update(Message::ComposerEdit(EditCommand::DeleteWordBackward));
    assert_eq!(s.message_input.text(), "hello bve world");

    // Nothing to delete at the start: no character is removed by accident.
    let mut s = composer_with("hello", 0, 0);
    let _ = s.update(Message::ComposerEdit(EditCommand::DeleteWordBackward));
    let _ = s.update(Message::ComposerEdit(EditCommand::DeleteToLineStart));
    assert_eq!(s.message_input.text(), "hello");

    let mut s = composer_with("first line\nsecond line", 1, 7);
    let _ = s.update(Message::ComposerEdit(EditCommand::DeleteToLineStart));
    assert_eq!(s.message_input.text(), "first line\nline");
}

#[test]
fn caret_commands_reach_document_and_paragraph_boundaries() {
    use crate::ui::composer_keys::EditCommand;
    // macOS: moving up from the first line goes to the start (and selects).
    let mut s = composer_with("one two", 0, 4);
    let _ = s.update(Message::ComposerEdit(EditCommand::Vertical { down: false, select: true }));
    assert_eq!(s.message_input.selection().as_deref(), Some("one "));
    let mut s = composer_with("one two", 0, 4);
    let _ = s.update(Message::ComposerEdit(EditCommand::Vertical { down: true, select: false }));
    assert_eq!(caret(&s), (0, 7));
    assert!(s.message_input.selection().is_none());

    // Paragraph steps: backward to the start (then the previous paragraph),
    // forward to the end (GTK, macOS) or the next paragraph's start (Windows).
    let step = |s: &mut Snack, forward, next_start| {
        let _ = s.update(Message::ComposerEdit(EditCommand::ParagraphStep { forward, select: false, next_start }));
        caret(s)
    };
    let mut s = composer_with("ab\ncd\nef", 1, 1);
    assert_eq!(step(&mut s, false, false), (1, 0));
    assert_eq!(step(&mut s, false, false), (0, 0));
    assert_eq!(step(&mut s, false, false), (0, 0));
    assert_eq!(step(&mut s, true, false), (0, 2));
    assert_eq!(step(&mut s, true, false), (1, 2));
    assert_eq!(step(&mut s, true, true), (2, 0));
    assert_eq!(step(&mut s, true, true), (2, 2));
}

#[test]
fn focused_composer_routes_native_edit_keys_and_keeps_alt_arrows_for_conversations() {
    use crate::message::ComposerAction;
    use crate::ui::composer_keys::EditCommand;
    use iced::keyboard::{Key, Modifiers, key::Named};
    let s = state();
    let word_delete = if cfg!(target_os = "macos") { Modifiers::ALT } else { Modifiers::CTRL };
    let messages = composer_events(&s, vec![composer_key(Key::Named(Named::Backspace), None, word_delete)]);
    assert!(messages.iter().any(|m| matches!(m, Message::ScopedComposer { context, action: ComposerAction::Edit(EditCommand::DeleteWordBackward) } if *context == s.composer_id)),
        "{messages:?}");
    let messages = composer_events(&s, vec![composer_key(Key::Named(Named::ArrowUp), None, Modifiers::ALT)]);
    assert!(messages.iter().any(|m| matches!(m, Message::PrevSelection | Message::Navigate { forward: false, .. })), "{messages:?}");
    assert!(!messages.iter().any(|m| matches!(m, Message::ScopedComposer { .. })));
}

#[test]
fn macos_control_letters_are_editing_keys_not_app_shortcuts() {
    if !cfg!(target_os = "macos") { return; }
    for key in ["n", "w"] {
        let mut s = state();
        s.state = AppState::Connected;
        s.chats.push(chat("bob@example", &[]));
        let event = contextual_shortcut(&s, iced::keyboard::Key::Character(key.into()), iced::keyboard::Modifiers::CTRL);
        let _ = s.update(event);
        assert!(!s.show_join_panel, "Ctrl-{key} opened the join panel");
        assert_eq!(s.chats.len(), 2, "Ctrl-{key} closed a conversation");
    }
}

fn direct_chat_state() -> Snack {
    let mut s = Snack::test_state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.history = Some(History::open_in_memory());
    s
}

fn chat_bodies(s: &Snack, jid: &str) -> Vec<String> {
    let chat = s.chats.iter().find(|chat| chat.jid == jid).expect("open chat");
    chat.messages.iter().filter_map(|message| match message {
        room::message::Message::Chat { body, .. } => Some(body.clone()), _ => None,
    }).collect()
}

#[test]
fn closed_direct_chats_stay_listed_and_reopen_with_their_history() {
    let mut s = direct_chat_state();
    let _ = s.update(Message::StartChat("bob@example".into()));
    assert!(s.earlier_chats.is_empty());
    s.history.as_ref().unwrap().store("bob@example", "bob", "old message", chrono::Utc::now(), false, None).unwrap();

    let _ = s.update(Message::CloseChat);
    assert!(s.chats.is_empty());
    assert_eq!(s.earlier_chats.iter().map(|chat| chat.conversation.as_str()).collect::<Vec<_>>(), ["bob@example"]);
    assert!(!s.show_earlier_chats, "the Earlier list starts collapsed");
    let _ = s.update(Message::ToggleEarlierChats);
    assert!(s.show_earlier_chats);

    let _ = s.update(Message::ReopenChat("bob@example".into()));
    assert_eq!(s.active, Some(Selection::Chat(0)));
    assert!(s.earlier_chats.is_empty());
    assert_eq!(chat_bodies(&s, "bob@example"), ["old message"]);

    // A message from a closed contact reopens the conversation.
    let _ = s.update(Message::CloseChat);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage {
        from: "bob@example/phone".into(), body: "are you there".into(), timestamp: chrono::Utc::now(),
        delayed: false, stanza_id: None, origin_id: None }));
    assert!(s.chats.iter().any(|chat| chat.jid == "bob@example"));
    assert!(s.earlier_chats.is_empty());
    assert!(s.history.as_ref().unwrap().direct_chats().unwrap()[0].open);

    // Only listed conversations can be reopened from the sidebar.
    let _ = s.update(Message::ReopenChat("stranger@example".into()));
    assert!(!s.chats.iter().any(|chat| chat.jid == "stranger@example"));
}

#[test]
fn reopened_private_room_chats_keep_their_occupant_key() {
    let mut s = direct_chat_state();
    let key = "room@conference.example/nick";
    s.history.as_ref().unwrap().record_direct_chat(key, "nick", false).unwrap();
    s.refresh_earlier_chats();
    let _ = s.update(Message::ReopenChat(key.into()));
    assert_eq!(s.chats.iter().map(|chat| chat.jid.as_str()).collect::<Vec<_>>(), [key]);
    assert_eq!(s.chats[0].title, "nick");
}

#[test]
fn login_restores_open_direct_chats_and_lists_closed_ones() {
    let mut s = direct_chat_state();
    {
        let h = s.history.as_ref().unwrap();
        h.record_direct_chat("bob@example", "bob", true).unwrap();
        h.record_direct_chat("carol@example", "carol", false).unwrap();
        h.store("dave@example", "dave", "from before the list existed", chrono::Utc::now(), false, None).unwrap();
    }
    s.restore_direct_chats("me@example");
    assert_eq!(s.chats.iter().map(|chat| chat.jid.as_str()).collect::<Vec<_>>(), ["bob@example"]);
    let mut earlier: Vec<_> = s.earlier_chats.iter().map(|chat| chat.conversation.as_str()).collect();
    earlier.sort_unstable();
    assert_eq!(earlier, ["carol@example", "dave@example"]);
    assert!(s.active.is_none(), "restoring does not switch conversations");
}

#[test]
fn sidebar_shows_closed_chats_only_under_the_expanded_earlier_list() {
    use iced::advanced::widget::Operation;
    struct Texts(Vec<String>);
    impl Operation for Texts {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, _bounds: iced::Rectangle, text: &str) { self.0.push(text.to_string()); }
    }
    let texts = |s: &Snack| {
        let mut widget = crate::ui::sidebar::view(s);
        let mut tree = Tree::new(&widget);
        let r = renderer();
        let size = iced::Size::new(200.0, 600.0);
        let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
        let mut texts = Texts(Vec::new());
        widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut texts);
        texts.0
    };
    let mut s = direct_chat_state();
    let _ = s.update(Message::StartChat("bob@example".into()));
    let _ = s.update(Message::CloseChat);
    let collapsed = texts(&s);
    assert!(collapsed.iter().any(|text| text.ends_with("Earlier (1)")), "{collapsed:?}");
    assert!(!collapsed.iter().any(|text| text == "bob"));
    let _ = s.update(Message::ToggleEarlierChats);
    assert!(texts(&s).iter().any(|text| text == "bob"));
}

// Click a message body to focus it, then send keys to the focused body.
fn transcript_body_keys(s: &Snack, keys: Vec<iced::Event>) -> Vec<Message> {
    use iced::advanced::widget::operation::{Operation, Focusable};
    struct Body(Option<iced::Rectangle>);
    impl Operation for Body {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn focusable(&mut self, id: Option<&iced::widget::Id>, bounds: iced::Rectangle, _state: &mut dyn Focusable) {
            if id.is_none() && self.0.is_none() { self.0 = Some(bounds); }
        }
    }
    let mut widget = crate::ui::chat::view(s);
    let mut tree = Tree::new(&widget);
    let r = renderer();
    let size = iced::Size::new(600.0, 400.0);
    let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
    let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
    let mut body = Body(None);
    widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut body);
    let point = body.0.expect("message body").center();
    let mut messages = Vec::new();
    let click = [iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left), iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)]
        .map(iced::Event::Mouse);
    for event in click.into_iter().chain(keys) {
        widget.as_widget_mut().update(&mut tree, &event, Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
            &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut messages), &bounds);
    }
    messages
}

fn named_key(key: iced::keyboard::key::Named, modifiers: iced::keyboard::Modifiers) -> iced::Event {
    iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
        key: iced::keyboard::Key::Named(key), modified_key: iced::keyboard::Key::Named(key),
        physical_key: iced::keyboard::key::Physical::Code(iced::keyboard::key::Code::Tab),
        location: iced::keyboard::Location::Standard, modifiers, text: None, repeat: false,
    })
}

fn close_key() -> iced::Event {
    iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
        key: iced::keyboard::Key::Character("w".into()), modified_key: iced::keyboard::Key::Character("w".into()),
        physical_key: iced::keyboard::key::Physical::Code(iced::keyboard::key::Code::KeyW),
        location: iced::keyboard::Location::Standard, modifiers: iced::keyboard::Modifiers::COMMAND, text: None, repeat: false,
    })
}

#[test]
fn tab_from_a_focused_message_body_returns_to_the_composer() {
    let mut s = state();
    s.state = AppState::Connected;
    let messages = transcript_body_keys(&s, vec![named_key(iced::keyboard::key::Named::Tab, iced::keyboard::Modifiers::NONE)]);
    let focus: Vec<_> = messages.into_iter().filter(|m| matches!(m, Message::FocusComposer { .. })).collect();
    assert!(matches!(focus.as_slice(), [Message::FocusComposer { context }] if *context == s.composer_id), "{focus:?}");
    assert!(s.update(focus[0].clone()).units() > 0, "the composer is focused");
    // A request from a previous conversation's view does nothing.
    let stale = Message::FocusComposer { context: uuid::Uuid::new_v4() };
    assert_eq!(s.update(stale).units(), 0);
}

#[test]
fn close_shortcut_closes_direct_chats_from_composer_or_transcript_but_never_leaves_rooms() {
    for from_body in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        let messages = if from_body { transcript_body_keys(&s, vec![close_key()]) } else { composer_events(&s, vec![close_key()]) };
        let close: Vec<_> = messages.into_iter().filter(|m| matches!(m, Message::CloseShortcut { .. })).collect();
        assert!(matches!(close.as_slice(), [Message::CloseShortcut { context }] if *context == s.composer_id),
            "from_body={from_body}: {close:?}");
        let _ = s.update(close[0].clone());
        assert!(s.chats.is_empty(), "from_body={from_body}");
    }

    let mut s = state();
    s.state = AppState::Connected;
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() });
    let _ = s.update(Message::SelectRoom("room@example".into()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    for message in composer_events(&s, vec![close_key()]) { let _ = s.update(message); }
    assert_eq!(s.rooms.len(), 1, "Cmd/Ctrl-W must not leave a room");
    assert!(rx.try_recv().is_err());
}

// Press and release the mouse in the composer, `x` pixels into its text.
fn composer_click(s: &Snack, x: f32) -> Vec<Message> {
    use iced::advanced::widget::operation::{Operation, Focusable};
    struct Composer(Option<iced::Rectangle>);
    impl Operation for Composer {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn focusable(&mut self, id: Option<&iced::widget::Id>, bounds: iced::Rectangle, _state: &mut dyn Focusable) {
            if id == Some(&iced::widget::Id::new(crate::app::MESSAGE_INPUT_ID)) { self.0 = Some(bounds); }
        }
    }
    let mut widget = crate::ui::chat::view(s);
    let mut tree = Tree::new(&widget);
    let r = renderer();
    let size = iced::Size::new(600.0, 400.0);
    let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
    let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
    let mut composer = Composer(None);
    widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut composer);
    let composer = composer.0.expect("composer");
    let point = iced::Point::new(composer.x + 10.0 + x, composer.center_y());
    let mut messages = Vec::new();
    for event in [iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left), iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)] {
        widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(event), Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
            &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut messages), &bounds);
    }
    messages
}

#[test]
fn shift_click_in_the_composer_selects_from_the_caret_to_the_click() {
    let mut s = state();
    s.state = AppState::Connected;
    s.message_input = text_editor::Content::with_text("hello brave new world");
    for message in composer_click(&s, 1.0) { let _ = s.update(message); }
    assert_eq!(s.message_input.cursor().position.column, 0);
    assert!(s.message_input.selection().is_none());

    let _ = s.update(Message::ModifiersUpdate { shift: true, primary: false });
    for message in composer_click(&s, 80.0) { let _ = s.update(message); }
    let first = s.message_input.selection().expect("shift-click selects");
    assert!(first.len() > 3 && "hello brave new world".starts_with(&first), "{first:?}");

    // A further shift-click keeps the original anchor and extends the range.
    for message in composer_click(&s, 140.0) { let _ = s.update(message); }
    let extended = s.message_input.selection().expect("still selected");
    assert!(extended.len() > first.len() && extended.starts_with(&first), "{first:?} -> {extended:?}");

    // Without Shift, a click just places the caret.
    let _ = s.update(Message::ModifiersUpdate { shift: false, primary: false });
    for message in composer_click(&s, 1.0) { let _ = s.update(message); }
    assert!(s.message_input.selection().is_none());
}

#[test]
fn uppercase_primary_shortcuts_work_without_shift() {
    for modifiers in [iced::keyboard::Modifiers::COMMAND] {
        for key in ["N", "W", "C"] {
            let mut s = state();
            s.state = AppState::Connected;
            s.chats.push(chat("bob@example", &[]));
            let _ = s.update(Message::SelectMessage { id: 1 });
            let event = contextual_shortcut(&s, iced::keyboard::Key::Character(key.into()), modifiers);
            let units = s.update(event).units();
            match key {
                "N" => assert!(s.show_join_panel, "uppercase Join shortcut was ignored"),
                "W" => assert_eq!(s.chats.len(), 1, "uppercase Close shortcut was ignored"),
                _ => { assert_eq!(units, 2); assert!(s.selected_msgs.is_empty()); }
            }
        }
    }
    for modifiers in [iced::keyboard::Modifiers::COMMAND | iced::keyboard::Modifiers::SHIFT,
        iced::keyboard::Modifiers::COMMAND | iced::keyboard::Modifiers::ALT] {
        let mut s = state();
        s.state = AppState::Connected;
        let event = contextual_shortcut(&s, iced::keyboard::Key::Character("N".into()), modifiers);
        assert_eq!(s.update(event).units(), 0);
        assert!(!s.show_join_panel);
    }
}

#[test]
fn queued_escape_cannot_close_a_new_search_in_the_same_conversation() {
    for initially_open in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        if initially_open { let _ = s.update(Message::ToggleFind); }
        let queued = contextual_shortcut(&s, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape), iced::keyboard::Modifiers::NONE);
        if initially_open { let _ = s.update(Message::CloseFind); }
        let _ = s.update(Message::ToggleFind);
        let current = s.find.as_ref().unwrap().id;
        assert_eq!(s.update(queued).units(), 0);
        assert_eq!(s.find.as_ref().unwrap().id, current);
        let current_escape = contextual_shortcut(&s, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape), iced::keyboard::Modifiers::NONE);
        assert_eq!(s.update(current_escape).units(), 1);
        assert!(s.find.is_none());
    }
}

#[test]
fn queued_copy_join_and_find_shortcuts_cannot_target_another_conversation() {
    for key in ["c", "n", "f"] {
        let mut s = state();
        s.state = AppState::Connected;
        let _ = s.update(Message::SelectMessage { id: 1 });
        let queued = contextual_shortcut(&s, iced::keyboard::Key::Character(key.into()), iced::keyboard::Modifiers::COMMAND);
        s.chats.push(chat("bob@example", &["bob private message"]));
        let _ = s.update(Message::SelectChat("bob@example".into()));
        let _ = s.update(Message::SelectMessage { id: 1 });
        assert_eq!(s.update(queued).units(), 0, "{key}: stale shortcut issued a task");
        assert_eq!(s.selected_msgs, [1].into_iter().collect());
        assert!(s.find.is_none());
        assert!(!s.show_join_panel);
        let current = contextual_shortcut(&s, iced::keyboard::Key::Character(key.into()), iced::keyboard::Modifiers::COMMAND);
        assert_eq!(s.update(current).units(), if key == "c" { 2 } else { 1 });
        if key == "c" { assert!(s.selected_msgs.is_empty()); }
        if key == "n" { assert!(s.show_join_panel); }
        if key == "f" { assert!(s.find.is_some()); }
    }
}

#[test]
fn global_shortcuts_from_an_old_account_cannot_act_after_relogin() {
    for key in ["c", "n", "f", "Escape"] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("old@example/device".into());
        s.saved_config.draft_account = Some("old@example".into());
        let _ = s.update(Message::SelectMessage { id: 1 });
        if key == "Escape" { let _ = s.update(Message::ToggleFind); }
        let queued = contextual_shortcut(&s, if key == "Escape" {
            iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)
        } else { iced::keyboard::Key::Character(key.into()) }, iced::keyboard::Modifiers::COMMAND);
        let _ = s.update(Message::Disconnect);
        let _ = s.update(Message::JidInputChanged("new@example/device".into()));
        let _ = s.update(Message::PasswordInputChanged("test secret".into()));
        let _ = s.update(Message::Connect);
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Connected)); // No subscription/network runs.
        let history = s.history.as_ref().unwrap();
        history.insert("alice@example", "alice", "new account private message", chrono::Utc::now(), false, None).unwrap();
        let id = history.load_tail("alice@example", 1).unwrap()[0].id;
        s.chats.push(chat("alice@example", &[]));
        let _ = s.update(Message::SelectChat("alice@example".into()));
        let _ = s.update(Message::SelectMessage { id });
        if key == "Escape" { let _ = s.update(Message::ToggleFind); }
        let current_search = s.find.as_ref().map(|find| find.id);
        assert_eq!(s.update(queued).units(), 0, "{key}");
        assert_eq!(s.selected_msgs, [id].into_iter().collect());
        assert_eq!(s.find.as_ref().map(|find| find.id), current_search);
        assert!(!s.show_join_panel);
    }
}

#[test]
fn global_window_and_modifier_events_still_apply_after_a_view_change() {
    let mut s = state();
    s.state = AppState::Connected;
    let old = s.shortcut_context();
    let _ = s.update(Message::ToggleFind);
    assert_ne!(s.shortcut_context(), old);
    s.window_focused = false;
    let _ = s.update(crate::subscription::scope_shortcut(old, Message::WindowFocused));
    assert!(s.window_focused);
    let _ = s.update(crate::subscription::scope_shortcut(old, Message::ModifiersUpdate { shift: true, primary: true }));
    assert!(s.cmd_down && s.shift_down);
    let _ = s.update(crate::subscription::scope_shortcut(old, Message::WindowUnfocused));
    assert!(!s.window_focused);
    assert!(!s.cmd_down && !s.shift_down);
}

#[test]
fn tab_in_search_traverses_focus_without_completing_the_private_room_draft() {
    for backward in [false, true] {
        let mut s = completion_state(&["alice", "anna"]);
        s.message_input = text_editor::Content::with_text("a");
        s.message_input.perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
        let _ = s.update(Message::ToggleFind);
        let event = composer_key(iced::keyboard::Key::Named(iced::keyboard::key::Named::Tab), None,
            if backward { iced::keyboard::Modifiers::SHIFT } else { iced::keyboard::Modifiers::NONE });
        assert!(search_input_events(&s, vec![event.clone()]).is_empty(), "Tab should reach normal focus traversal");
        let iced::Event::Keyboard(key) = event else { unreachable!() };
        let message = crate::subscription::session_keyboard_event(s.shortcut_context(), s.navigation_context(), s.focus_context(), key);
        assert_eq!(s.update(message).units(), 1, "expected a focus traversal operation");
        assert_eq!(s.message_input.text(), "a");
        assert!(s.nick_complete.is_none());
        assert_eq!(s.find.as_ref().unwrap().query, "");
    }
}

#[test]
fn queued_tab_cannot_move_focus_in_a_replacement_form_or_conversation() {
    for transition in ["conversation", "search", "search reopened", "join", "selected controls", "logout", "login attempt", "login connected", "cancel"] {
        for backward in [false, true] {
            let mut s = state();
            s.state = match transition {
                "login attempt" => AppState::Login,
                "login connected" | "cancel" => AppState::Connecting,
                _ => AppState::Connected,
            };
            if transition == "search reopened" { let _ = s.update(Message::ToggleFind); }
            let modifiers = if backward { iced::keyboard::Modifiers::SHIFT } else { iced::keyboard::Modifiers::NONE };
            let tab = contextual_shortcut(&s, iced::keyboard::Key::Named(iced::keyboard::key::Named::Tab), modifiers);
            match transition {
                "conversation" => { s.chats.push(chat("bob@example", &[])); let _ = s.update(Message::SelectChat("bob@example".into())); }
                "search" => { let _ = s.update(Message::ToggleFind); }
                "search reopened" => { let _ = s.update(Message::CloseFind); let _ = s.update(Message::ToggleFind); }
                "join" => { let _ = s.update(Message::ShowJoinPanel); }
                "selected controls" => { let _ = s.update(Message::SelectMessage { id: 1 }); }
                "logout" => { let _ = s.update(Message::Disconnect); }
                "login connected" => { let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Connected)); }
                "cancel" => { let _ = s.update(Message::CancelConnect); }
                _ => {
                    let _ = s.update(Message::JidInputChanged("me@example/device".into()));
                    let _ = s.update(Message::PasswordInputChanged("test secret".into()));
                    let _ = s.update(Message::Connect); // Subscription is never run.
                }
            }
            assert_eq!(s.update(tab).units(), 0, "{transition}, backward={backward}: old Tab moved focus in the new view");
            let current = contextual_shortcut(&s, iced::keyboard::Key::Named(iced::keyboard::key::Named::Tab), modifiers);
            assert_eq!(s.update(current).units(), 1, "current Tab should still traverse focus");
        }
    }
}

#[test]
fn queued_tabs_keep_traversing_within_the_same_form() {
    for connected in [false, true] {
        for backward in [false, true] {
            let mut s = state();
            if connected { s.state = AppState::Connected; }
            let modifiers = if backward { iced::keyboard::Modifiers::SHIFT } else { iced::keyboard::Modifiers::NONE };
            let queued: Vec<_> = (0..3).map(|_| contextual_shortcut(&s,
                iced::keyboard::Key::Named(iced::keyboard::key::Named::Tab), modifiers)).collect();
            if connected {
                let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Insert('x'))));
            } else { let _ = s.update(Message::JidInputChanged("edited@example".into())); }
            for event in queued { assert_eq!(s.update(event).units(), 1); }
            assert_eq!(s.message_input.text(), if connected { "x" } else { "" });
            assert!(s.nick_complete.is_none());
        }
    }
}

#[test]
fn focused_composer_owns_forward_and_reverse_nickname_completion() {
    for backward in [false, true] {
        let mut s = completion_state(&["alice", "anna"]);
        s.message_input = text_editor::Content::with_text("a");
        s.message_input.perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
        let key = composer_key(iced::keyboard::Key::Named(iced::keyboard::key::Named::Tab), None,
            if backward { iced::keyboard::Modifiers::SHIFT } else { iced::keyboard::Modifiers::NONE });
        let queued = composer_events(&s, vec![key.clone(), key]);
        assert_eq!(queued.len(), 2, "focused composer must consume both Tab presses: {queued:?}");
        for (index, message) in queued.into_iter().enumerate() {
            let _ = s.update(message);
            let expected = if backward == (index == 0) { "anna: " } else { "alice: " };
            assert_eq!(s.message_input.text(), expected);
        }
        let _ = s.update(Message::UndoInput);
        assert_eq!(s.message_input.text(), if backward { "anna: " } else { "alice: " });
    }
}

#[test]
fn delayed_completion_cannot_change_or_move_focus_in_another_composer() {
    let mut s = completion_state(&["alice", "anna"]);
    s.message_input = text_editor::Content::with_text("a");
    s.message_input.perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
    let queued = composer_events(&s, vec![composer_key(iced::keyboard::Key::Named(iced::keyboard::key::Named::Tab), None, iced::keyboard::Modifiers::NONE)]);
    assert_eq!(queued.len(), 1);
    s.saved_config.drafts.insert("alice@example".into(), "private replacement draft".into());
    let _ = s.update(Message::SelectChat("alice@example".into()));
    for message in queued { assert_eq!(s.update(message).units(), 0); }
    assert_eq!(s.message_input.text(), "private replacement draft");
}

#[test]
fn consecutive_navigation_shortcuts_apply_each_step_before_the_next_event() {
    for forward in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.chats.push(chat("bob@example", &[]));
        s.chats.push(chat("charlie@example", &[]));
        for jid in ["first-room@example", "second-room@example"] {
            let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: jid.into(), own_nick: "me".into(), members: Vec::new() });
        }
        assert_eq!(s.active_conv_jid().as_deref(), Some("alice@example"));
        for jid in ["first-room@example", "second-room@example", "alice@example", "bob@example", "charlie@example"] {
            s.saved_config.drafts.insert(jid.into(), format!("draft for {jid}"));
        }
        s.message_input = text_editor::Content::with_text("draft for alice@example");
        let iced::Event::Keyboard(event) = composer_key(
            iced::keyboard::Key::Named(if forward { iced::keyboard::key::Named::ArrowDown } else { iced::keyboard::key::Named::ArrowUp }),
            None, iced::keyboard::Modifiers::ALT) else { unreachable!() };
        // Several key events may already be queued from the same rendered view.
        let shortcut = crate::subscription::session_keyboard_event(s.shortcut_context(), s.navigation_context(), s.focus_context(), event);
        let expected = if forward {
            ["bob@example", "charlie@example", "first-room@example", "second-room@example", "alice@example"]
        } else {
            ["second-room@example", "first-room@example", "charlie@example", "bob@example", "alice@example"]
        };
        for jid in expected {
            let _ = s.update(shortcut.clone()); // No deferred tasks are executed.
            assert_eq!(s.active_conv_jid().as_deref(), Some(jid), "forward={forward}");
            assert_eq!(s.message_input.text(), format!("draft for {jid}"));
        }
        assert_eq!(storage::load().drafts.get("alice@example").map(String::as_str), Some("draft for alice@example"));
    }
}

#[test]
fn navigation_handles_no_selection_and_empty_or_logged_out_sidebars() {
    for forward in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.chats.push(chat("bob@example", &[]));
        s.active = None;
        s.message_input = text_editor::Content::new();
        let action = if forward { Message::NextSelection } else { Message::PrevSelection };
        let _ = s.update(action.clone());
        assert_eq!(s.active_conv_jid().as_deref(), Some(if forward { "alice@example" } else { "bob@example" }));
        s.state = AppState::Login;
        assert_eq!(s.update(action.clone()).units(), 0);
        assert_eq!(s.active_conv_jid().as_deref(), Some(if forward { "alice@example" } else { "bob@example" }));
        s.state = AppState::Connected;
        s.chats.clear();
        s.active = None;
        assert_eq!(s.update(action).units(), 0);
        assert!(s.active.is_none());
    }
}

#[test]
fn composer_navigation_shortcut_is_not_repeated_by_unfocused_message_bodies() {
    let mut s = state();
    s.state = AppState::Connected;
    let events = composer_events(&s, vec![composer_key(
        iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowDown), None, iced::keyboard::Modifiers::ALT)]);
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(matches!(events[0], Message::Navigate { forward: true, .. }));
}

#[test]
fn focused_search_input_cannot_send_or_undo_the_message_draft() {
    for (key, modifiers) in [
        (iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter), iced::keyboard::Modifiers::NONE),
        (iced::keyboard::Key::Character("z".into()), iced::keyboard::Modifiers::COMMAND),
        (iced::keyboard::Key::Character("z".into()), iced::keyboard::Modifiers::COMMAND | iced::keyboard::Modifiers::SHIFT),
        (iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowDown), iced::keyboard::Modifiers::ALT),
    ] {
        let mut s = state();
        s.state = AppState::Connected;
        s.chats.push(chat("bob@example", &[]));
        s.history = Some(History::open_in_memory());
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Paste(std::sync::Arc::new("unfinished private draft".into())))));
        let _ = s.update(Message::ToggleFind);
        let queued = search_input_events(&s, vec![composer_key(key.clone(), None, modifiers)]);
        assert!(!queued.iter().any(|message| matches!(message,
            Message::ScopedComposer { .. } | Message::Navigate { .. } | Message::NextSelection | Message::PrevSelection)), "{key:?}: {queued:?}");
        for message in queued { let _ = s.update(message); }
        assert_eq!(s.message_input.text(), "unfinished private draft");
        assert_eq!(s.active_conv_jid().as_deref(), Some("alice@example"));
        assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn old_search_controls_cannot_change_a_reopened_search() {
    for action in ["✕", "All rooms", "˄", "˅", "input"] {
        let mut s = state();
        s.state = AppState::Connected;
        let _ = s.update(Message::ToggleFind);
        let queued = if action == "input" {
            search_input_events(&s, vec![iced::Event::InputMethod(iced::advanced::input_method::Event::Commit("old query".into()))])
        } else { app_clicks(&s, action) };
        assert!(!queued.is_empty(), "{action}");
        let _ = s.update(Message::CloseFind);
        let _ = s.update(Message::ToggleFind);
        let find = s.find.as_mut().unwrap();
        find.query = "new query".into();
        find.matches = (1..=3).map(|id| crate::app::FindMatch { conversation: "alice@example".into(), id, received: chrono::DateTime::UNIX_EPOCH }).collect();
        find.index = 1;
        for message in queued { assert_eq!(s.update(message).units(), 0, "{action}"); }
        let find = s.find.as_ref().expect("old close must not close new search");
        assert_eq!(find.query, "new query", "{action}");
        assert!(!find.all_scope, "{action}");
        assert_eq!(find.index, 1, "{action}");
        assert_eq!(find.matches.len(), 3, "{action}");
    }
}

#[test]
fn old_search_close_cannot_clear_selection_after_the_search_is_closed() {
    let mut s = state();
    s.state = AppState::Connected;
    let _ = s.update(Message::ToggleFind);
    let queued = app_clicks(&s, "✕");
    assert!(!queued.is_empty());
    let _ = s.update(Message::CloseFind);
    let _ = s.update(Message::SelectMessage { id: 2 });
    for message in queued { assert_eq!(s.update(message).units(), 0); }
    assert_eq!(s.selected_msgs, [2].into_iter().collect());
}

#[test]
fn search_submit_and_retry_keep_the_latest_typed_query_across_global_result_switches() {
    for retry in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.chats[0].messages.clear();
        s.body_editors.clear();
        s.chats.push(chat("bob@example", &[]));
        let h = History::open_in_memory();
        let now = chrono::Utc::now();
        h.insert("alice@example", "alice", "needle first", now, false, None).unwrap();
        h.insert("bob@example", "bob", "needle second", now + chrono::Duration::seconds(1), false, None).unwrap();
        s.history = Some(h);
        let _ = s.update(Message::ToggleFind);
        let find = s.find.as_mut().unwrap();
        find.all_scope = true;
        find.error = Some("unavailable".into());
        let old_retry = if retry { app_clicks(&s, "Retry") } else { Vec::new() };
        let mut input = vec![iced::Event::InputMethod(iced::advanced::input_method::Event::Commit("needle".into()))];
        if !retry { input.push(composer_key(iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter), None, iced::keyboard::Modifiers::NONE)); }
        let events = search_input_events(&s, input);
        assert_eq!(events.len(), if retry { 1 } else { 2 }, "{events:?}");
        for message in events.into_iter().chain(old_retry) { let _ = s.update(message); }
        let find = s.find.as_ref().unwrap();
        assert_eq!(find.query, "needle", "retry={retry}: old displayed query replaced latest input");
        assert!(find.error.is_none());
        assert_eq!(find.matches.len(), 2);
        assert_eq!(s.active_conv_jid().as_deref(), Some(if retry { "bob@example" } else { "alice@example" }));
    }
}

#[test]
fn search_button_press_keeps_its_form_and_control_role() {
    use iced::advanced::widget::Operation;
    struct Target<'a>(&'a str, Option<iced::Rectangle>);
    impl Operation for Target<'_> {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) {
            if text == self.0 { self.1 = Some(bounds); }
        }
    }
    for change in ["redraw", "reopen", "retry removed"] {
        let mut s = state();
        s.state = AppState::Connected;
        let _ = s.update(Message::ToggleFind);
        let find = s.find.as_mut().unwrap();
        find.query = "query".into();
        find.matches = (1..=3).map(|id| crate::app::FindMatch { conversation: "alice@example".into(), id, received: chrono::DateTime::UNIX_EPOCH }).collect();
        find.index = 1;
        if change == "retry removed" { find.error = Some("unavailable".into()); }
        let r = renderer();
        let size = iced::Size::new(800.0, 600.0);
        let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
        let mut tree;
        {
            let mut widget = crate::ui::chat::view(&s);
            tree = Tree::new(&widget);
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut target = Target("˄", None);
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut target);
            let point = target.1.unwrap().center();
            let mut output = Vec::new();
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
            assert!(output.is_empty());
        }
        if change == "reopen" {
            let old = s.find.take().unwrap();
            let _ = s.update(Message::ToggleFind);
            let find = s.find.as_mut().unwrap();
            find.query = old.query;
            find.matches = old.matches;
            find.index = 1;
        } else { s.find.as_mut().unwrap().error = None; }
        let output = {
            let mut widget = crate::ui::chat::view(&s);
            tree.diff(widget.as_widget());
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut target = Target(if change == "retry removed" { "˅" } else { "˄" }, None);
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut target);
            let point = target.1.unwrap().center();
            let mut output = Vec::new();
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)),
                Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
            output
        };
        for message in output { let _ = s.update(message); }
        assert_eq!(s.find.as_ref().unwrap().index, if change == "redraw" { 0 } else { 1 }, "{change}");
    }
}

#[test]
fn global_search_keeps_keyboard_focus_when_a_result_removes_the_selection_menu() {
    for room_target in [false, true] {
        for selected in [false, true] {
            let mut s = state();
            s.state = AppState::Connected;
            let target = if room_target { "room@example" } else { "bob@example" };
            if room_target {
                let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: target.into(), own_nick: "me".into(), members: Vec::new() });
            } else { s.chats.push(chat(target, &[])); }
            let h = History::open_in_memory();
            let now = chrono::Utc::now();
            h.insert("alice@example", "alice", "unrelated", now, false, None).unwrap();
            h.insert(target, "bob", "needle more", now + chrono::Duration::seconds(1), false, None).unwrap();
            s.history = Some(h);
            seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
            s.sync_body_editors();
            if selected {
                let id = s.history.as_ref().unwrap().load_tail("alice@example", 1).unwrap()[0].id;
                let _ = s.update(Message::SelectMessage { id });
            }
            s.message_input = text_editor::Content::with_text("alice private draft");
            s.saved_config.drafts.insert(target.into(), "destination private draft".into());
            let _ = s.update(Message::ToggleFind);
            s.find.as_mut().unwrap().all_scope = true;
            let find_id = s.find.as_ref().unwrap().id;
            let r = renderer();
            let size = iced::Size::new(800.0, 600.0);
            let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
            let mut tree;
            let first = {
                let mut widget = crate::ui::chat::view(&s);
                tree = Tree::new(&widget);
                let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
                let mut focus = iced::advanced::widget::operation::focusable::focus::<()>(iced::widget::Id::new(crate::app::FIND_INPUT_ID));
                widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut focus);
                let mut output = Vec::new();
                widget.as_widget_mut().update(&mut tree, &iced::Event::InputMethod(iced::advanced::input_method::Event::Commit("needle".into())),
                    Layout::new(&node), iced::mouse::Cursor::Unavailable, &r,
                    &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
                output
            };
            for message in first { let _ = s.update(message); }
            assert_eq!(s.active_conv_jid().as_deref(), Some(target));
            assert!(s.selected_msgs.is_empty());
            assert_eq!(s.find.as_ref().unwrap().id, find_id);
            let second = {
                let mut widget = crate::ui::chat::view(&s);
                tree.diff(widget.as_widget());
                let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
                let mut output = Vec::new();
                // Retain the widget tree, without issuing another focus task.
                widget.as_widget_mut().update(&mut tree, &iced::Event::InputMethod(iced::advanced::input_method::Event::Commit(" more".into())),
                    Layout::new(&node), iced::mouse::Cursor::Unavailable, &r,
                    &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
                output
            };
            assert!(second.iter().any(|message| matches!(message, Message::ScopedFind {
                context, action: crate::message::FindAction::Input(query)
            } if *context == find_id && query == "needle more")), "room={room_target}, selected={selected}: search lost focus: {second:?}");
            for message in second { let _ = s.update(message); }
            assert_eq!(s.find.as_ref().unwrap().query, "needle more");
            assert_eq!(s.message_input.text(), "destination private draft");
        }
    }
}

#[test]
fn search_enter_retries_when_the_current_query_has_an_error() {
    let mut s = state();
    s.state = AppState::Connected;
    let h = History::open_in_memory();
    h.insert("alice@example", "alice", "needle", chrono::Utc::now(), false, None).unwrap();
    s.history = Some(h);
    let _ = s.update(Message::ToggleFind);
    let find = s.find.as_mut().unwrap();
    find.query = "needle".into();
    find.error = Some("unavailable".into());
    let id = find.id;
    for message in search_input_events(&s, vec![composer_key(iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter), None, iced::keyboard::Modifiers::NONE)]) {
        let _ = s.update(message);
    }
    let find = s.find.as_ref().unwrap();
    assert_eq!(find.id, id);
    assert_eq!(find.query, "needle");
    assert!(find.error.is_none());
    assert_eq!(find.matches.len(), 1);
}

#[test]
fn old_room_retry_callbacks_cannot_restart_another_attempt_or_account() {
    for change in ["attempt", "account"] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("old@example/device".into());
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        assert!(s.send_xmpp_command(xmpp::XmppCommand::JoinRoom("room@example".into())));
        let _ = rx.try_recv().unwrap();
        let _ = s.room_join_failed("room@example".into(), "first failure".into());
        let queued = app_clicks(&s, "Retry join");
        assert!(!queued.is_empty());
        if change == "account" {
            let _ = s.update(Message::Disconnect);
            let _ = s.update(Message::JidInputChanged("new@example/device".into()));
            let _ = s.update(Message::PasswordInputChanged("test secret".into()));
            let _ = s.update(Message::Connect);
            let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Connected)); // No subscription runs.
            let (tx, new_rx) = tokio::sync::mpsc::channel(10);
            s.xmpp_cmd_tx = Some(tx);
            rx = new_rx;
        }
        assert!(s.send_xmpp_command(xmpp::XmppCommand::JoinRoom("room@example".into())));
        let _ = rx.try_recv().unwrap();
        let _ = s.room_join_failed("room@example".into(), "later failure".into());
        let current = s.room_operations["room@example"].clone();
        for message in queued { let _ = s.update(message); }
        assert!(rx.try_recv().is_err(), "{change}: old Retry started a join");
        assert!(current.is_current());
        assert_eq!(s.room_operations["room@example"].id, current.id);
        assert!(matches!(s.room_connections.get("room@example"), Some(RoomConnection::Failed(reason)) if reason == "later failure"));
        for message in app_clicks(&s, "Retry join") { let _ = s.update(message); }
        assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::JoinRoom(jid) if jid == "room@example"));
        assert!(!current.is_current());
        assert!(matches!(s.room_connections.get("room@example"), Some(RoomConnection::Joining)));
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn room_retry_requires_the_same_connection_even_for_the_same_profile() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("same@example/device".into());
    let (tx, channel) = xmpp::new_command_channel("same@example/device".into(), "test secret".into());
    s.xmpp_cmd_tx = Some(tx);
    s.xmpp_cmd_rx = Some(channel);
    s.room_connections.insert("room@example".into(), RoomConnection::Failed("test failure".into()));
    let queued = app_clicks(&s, "Retry join");
    assert!(!queued.is_empty());
    let (tx, channel) = xmpp::new_command_channel("same@example/device".into(), "test secret".into());
    s.xmpp_cmd_tx = Some(tx.clone());
    s.xmpp_cmd_rx = Some(channel);
    let capacity = tx.capacity();
    for message in queued { let _ = s.update(message); }
    assert_eq!(tx.capacity(), capacity, "old connection callback queued a join");
    assert!(s.room_operations.is_empty());
    assert!(matches!(s.room_connections.get("room@example"), Some(RoomConnection::Failed(_))));
    for message in app_clicks(&s, "Retry join") { let _ = s.update(message); }
    assert_eq!(tx.capacity(), capacity - 1);
    assert!(matches!(s.room_connections.get("room@example"), Some(RoomConnection::Joining)));
}

#[test]
fn held_room_retry_cannot_move_to_another_failed_room_or_attempt() {
    use iced::advanced::widget::Operation;
    struct Target(Option<iced::Rectangle>);
    impl Operation for Target {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) {
            if text == "Retry join" { self.0 = Some(bounds); }
        }
    }
    for change in ["redraw", "room", "attempt"] {
        let mut s = state();
        s.state = AppState::Connected;
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        s.room_connections.insert("first@example".into(), RoomConnection::Failed("test failure".into()));
        s.room_operations.insert("first@example".into(), xmpp::RoomOperation::new());
        let r = renderer();
        let size = iced::Size::new(800.0, 600.0);
        let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
        let mut tree;
        let point;
        {
            let mut widget = s.view();
            tree = Tree::new(&widget);
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut target = Target(None);
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut target);
            point = target.0.unwrap().center();
            let mut output = Vec::new();
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
            assert!(output.is_empty());
        }
        match change {
            "room" => {
                s.room_connections.remove("first@example");
                s.room_connections.insert("next@example".into(), RoomConnection::Failed("test failure".into()));
            }
            "attempt" => {
                s.room_operations.remove("first@example").unwrap().cancel();
                s.room_operations.insert("first@example".into(), xmpp::RoomOperation::new());
            }
            _ => {},
        }
        let output = {
            let mut widget = s.view();
            tree.diff(widget.as_widget());
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut output = Vec::new();
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)),
                Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
            output
        };
        for message in output { let _ = s.update(message); }
        if change == "redraw" {
            assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::JoinRoom(jid) if jid == "first@example"));
        }
        assert!(rx.try_recv().is_err(), "{change}: held Retry started another join");
    }
}

#[test]
fn invalid_join_addresses_do_not_enter_room_state_or_command_queues() {
    for jid in ["room@example/nick".to_string(), "@example".into(), "room@".into(), "room name@example".into(),
        "example".into(), "room@example/".into(), "bad\0room@example".into(), format!("{}@example", "a".repeat(1024))] {
        let mut s = state();
        s.state = AppState::Connected;
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        s.message_input = text_editor::Content::with_text("private draft");
        let _ = s.update(Message::ShowJoinPanel);
        s.join_input = jid.clone();
        for message in app_clicks(&s, "Join") { let _ = s.update(message); }
        assert!(s.joining_room.is_none(), "invalid input became an active join: {jid:?}");
        assert!(s.join_error.is_some(), "invalid input needs actionable feedback");
        assert!(s.room_connections.is_empty());
        assert!(s.room_operations.is_empty());
        assert!(s.xmpp_pending_commands.is_empty());
        assert!(rx.try_recv().is_err());
        assert_eq!(s.join_input, jid);
        assert_eq!(s.message_input.text(), "private draft");
        assert!(s.show_join_panel);
    }
}

#[test]
fn invalid_background_room_join_cannot_replace_an_existing_operation() {
    let mut s = state();
    let jid = "room@example/invalid";
    let operation = xmpp::RoomOperation::new();
    s.room_operations.insert(jid.into(), operation.clone());
    s.room_connections.insert(jid.into(), RoomConnection::Failed("original failure".into()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    assert!(!s.send_xmpp_command(xmpp::XmppCommand::JoinRoom(jid.into())));
    assert_eq!(s.room_operations[jid].id, operation.id);
    assert!(operation.is_current());
    assert!(matches!(s.room_connections.get(jid), Some(RoomConnection::Failed(reason)) if reason == "original failure"));
    assert!(s.xmpp_pending_commands.is_empty());
    assert!(rx.try_recv().is_err());
}

#[test]
fn valid_unicode_and_boundary_room_addresses_still_enter_the_join_queue() {
    for input in ["  rüm@conference.example  ".to_string(), format!("{}@{}", "a".repeat(1023), "b".repeat(1023))] {
        let mut s = state();
        s.state = AppState::Connected;
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        let _ = s.update(Message::ShowJoinPanel);
        s.join_input = input.clone();
        for message in app_clicks(&s, "Join") { let _ = s.update(message); }
        let expected = input.trim();
        assert_eq!(s.joining_room.as_deref(), Some(expected));
        assert!(s.join_error.is_none());
        assert!(matches!(s.room_connections.get(expected), Some(RoomConnection::Joining)));
        assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::JoinRoom(jid) if jid == expected));
        assert!(rx.try_recv().is_err());
    }
}

fn join_input_events(s: &Snack, events: Vec<iced::Event>) -> Vec<Message> {
    let mut widget = crate::ui::join::view(s);
    let mut tree = Tree::new(&widget);
    let r = renderer();
    let size = iced::Size::new(800.0, 600.0);
    let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
    let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
    let mut focus = iced::advanced::widget::operation::focusable::focus::<()>(iced::widget::Id::new(crate::app::JOIN_INPUT_ID));
    widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut focus);
    let mut output = Vec::new();
    for event in events {
        widget.as_widget_mut().update(&mut tree, &event, Layout::new(&node), iced::mouse::Cursor::Unavailable, &r,
            &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
    }
    output
}

#[test]
fn old_join_form_actions_cannot_change_or_submit_a_replacement_form() {
    for action in ["Join", "Cancel", "Save this room", "input"] {
        let mut s = state();
        s.state = AppState::Connected;
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        let _ = s.update(Message::ShowJoinPanel);
        s.join_input = "old-room@example".into();
        s.save_room = true;
        let queued = if action == "input" {
            join_input_events(&s, vec![iced::Event::InputMethod(iced::advanced::input_method::Event::Commit("old edit".into()))])
        } else { app_clicks(&s, action) };
        assert!(!queued.is_empty(), "{action}");
        let _ = s.update(Message::HideJoinPanel);
        let _ = s.update(Message::ShowJoinPanel);
        s.join_input = "new-room@example".into();
        for message in queued { assert_eq!(s.update(message).units(), 0, "{action}"); }
        assert_eq!(s.join_input, "new-room@example", "{action}");
        assert!(s.save_room, "{action}");
        assert!(s.show_join_panel, "{action}");
        assert!(s.joining_room.is_none(), "{action}");
        assert!(rx.try_recv().is_err(), "{action}: stale form sent a command");
    }
}

#[test]
fn join_form_batched_typing_and_submit_runs_once_and_cannot_replay_after_failure() {
    let mut s = state();
    s.state = AppState::Connected;
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    let _ = s.update(Message::ShowJoinPanel);
    s.save_room = false;
    let checkbox = app_clicks(&s, "Save this room");
    let events = join_input_events(&s, vec![
        iced::Event::InputMethod(iced::advanced::input_method::Event::Commit("room@example".into())),
        composer_key(iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter), None, iced::keyboard::Modifiers::NONE),
    ]);
    assert_eq!(events.len(), 2);
    let old_submit = events[1].clone();
    for message in checkbox.into_iter().chain(events) { let _ = s.update(message); }
    assert!(s.save_room);
    assert_eq!(s.joining_room.as_deref(), Some("room@example"));
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::JoinRoom(jid) if jid == "room@example"));
    let _ = s.update(old_submit.clone());
    assert!(rx.try_recv().is_err(), "duplicate Enter must not restart the active join");
    let _ = s.room_join_failed("room@example".into(), "test failure".into());
    let _ = s.update(old_submit);
    assert!(rx.try_recv().is_err(), "a failed attempt must not reenable its old submit");
    assert!(s.join_error.is_some());
    for message in app_clicks(&s, "Join") { let _ = s.update(message); }
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::JoinRoom(jid) if jid == "room@example"));
    assert!(rx.try_recv().is_err());
}

#[test]
fn join_form_press_cannot_finish_in_a_reopened_or_retried_form() {
    use iced::advanced::widget::Operation;
    struct Target(Option<iced::Rectangle>);
    impl Operation for Target {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) {
            if text == "Join" { self.0 = Some(bounds); }
        }
    }
    for change in ["redraw", "reopen", "failure"] {
        let mut s = state();
        s.state = AppState::Connected;
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        let _ = s.update(Message::ShowJoinPanel);
        s.join_input = "room@example".into();
        let r = renderer();
        let size = iced::Size::new(800.0, 600.0);
        let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
        let mut tree;
        let point;
        {
            let mut widget = crate::ui::join::view(&s);
            tree = Tree::new(&widget);
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut target = Target(None);
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut target);
            point = target.0.unwrap().center();
            let mut output = Vec::new();
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
            assert!(output.is_empty());
        }
        match change {
            "reopen" => {
                let _ = s.update(Message::HideJoinPanel);
                let _ = s.update(Message::ShowJoinPanel);
                s.join_input = "new-room@example".into();
            }
            "failure" => {
                let _ = s.update(Message::JoinRoom);
                let _ = rx.try_recv().unwrap();
                let _ = s.room_join_failed("room@example".into(), "test failure".into());
            }
            _ => {},
        }
        let output = {
            let mut widget = crate::ui::join::view(&s);
            tree.diff(widget.as_widget());
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut output = Vec::new();
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)),
                Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
            output
        };
        for message in output { let _ = s.update(message); }
        if change == "redraw" {
            assert_eq!(s.joining_room.as_deref(), Some("room@example"));
            assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::JoinRoom(jid) if jid == "room@example"));
        } else {
            assert!(s.joining_room.is_none(), "{change}");
        }
        assert!(rx.try_recv().is_err(), "{change}: stale gesture submitted a new join");
    }
}

#[test]
fn join_form_activates_an_existing_room_in_the_accepted_action() {
    for address in ["room@example", "room@EXAMPLE"] {
        let mut s = completion_state(&[]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(20);
        s.xmpp_cmd_tx = Some(tx);
        s.saved_config.rooms.push(address.into());
        let _ = s.update(Message::SelectChat("alice@example".into()));
        s.message_input = text_editor::Content::with_text("alice private draft");
        s.saved_config.drafts.insert("room@example".into(), "room draft".into());
        let _ = s.update(Message::ShowJoinPanel);
        s.join_input = address.into();
        let queued = app_clicks(&s, "Join");
        assert!(!queued.is_empty());
        for message in queued { let _ = s.update(message); }
        assert_eq!(s.active_conv_jid().as_deref(), Some("room@example"));
        assert_eq!(s.message_input.text(), "room draft");
        assert_eq!(s.saved_config.drafts.get("alice@example").map(String::as_str), Some("alice private draft"));
        assert!(!s.show_join_panel);
        assert!(s.joining_room.is_none());
        assert_eq!(s.rooms.len(), 1);
        assert!(rx.try_recv().is_err());
    }
}

fn member_click_events(s: &Snack, name: &str) -> Vec<Message> {
    use iced::advanced::widget::Operation;
    struct Target<'a>(&'a str, Option<iced::Rectangle>);
    impl Operation for Target<'_> {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) {
            if text == self.0 { self.1 = Some(bounds); }
        }
    }
    let mut widget = crate::ui::members::view(s);
    let mut tree = Tree::new(&widget);
    let r = renderer();
    let size = iced::Size::new(160.0, 400.0);
    let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
    let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
    let mut target = Target(name, None);
    widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut target);
    let point = target.1.expect("member button").center();
    let mut output = Vec::new();
    for event in [iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left), iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)] {
        widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(event), Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
            &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
    }
    output
}

#[test]
fn stale_member_click_cannot_open_an_outdated_or_different_room_contact() {
    for change in ["departed", "reassigned", "anonymous", "reopened", "disconnect", "account"] {
        let mut s = completion_state(&["bob"]);
        s.rooms[0].users[0].jid = Some("bob@example/device".into());
        let queued = member_click_events(&s, "bob");
        assert!(!queued.is_empty());
        match change {
            "departed" => s.rooms[0].users.clear(),
            "reassigned" => s.rooms[0].users[0].jid = Some("someone-else@example/device".into()),
            "anonymous" => s.rooms[0].users[0].jid = None,
            "account" => {
                let _ = s.update(Message::Disconnect);
                let _ = s.update(Message::JidInputChanged("new@example/device".into()));
                let _ = s.update(Message::PasswordInputChanged("test secret".into()));
                let _ = s.update(Message::Connect);
                let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Connected)); // No subscription runs.
                let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined {
                    room: "room@example".into(), own_nick: "me".into(), members: Vec::new(),
                });
                let _ = s.update(Message::SelectRoom("room@example".into()));
                s.rooms[0].users.push(room::user::User {
                    name: "bob".into(), jid: Some("bob@example/device".into()), show: None, status: None,
                });
            }
            "reopened" => {
                let _ = s.update(Message::SelectChat("alice@example".into()));
                let _ = s.update(Message::SelectRoom("room@example".into()));
            }
            _ => { let _ = s.update(Message::Disconnect); }
        }
        for message in queued { let _ = s.update(message); }
        assert!(!s.chats.iter().any(|chat| chat.jid == "bob@example"), "{change}: old member click opened a contact");
    }
    let mut s = completion_state(&["bob"]);
    s.rooms[0].users[0].jid = Some("bob@example/device".into());
    s.message_input = text_editor::Content::with_text("room draft");
    s.saved_config.drafts.insert("bob@example".into(), "bob draft".into());
    let queued = member_click_events(&s, "bob");
    s.rooms[0].users[0].show = Some("away".into());
    for message in queued { let _ = s.update(message); }
    assert!(s.chats.iter().any(|chat| chat.jid == "bob@example"));
    assert_eq!(s.active_conv_jid().as_deref(), Some("bob@example"));
    assert_eq!(s.message_input.text(), "bob draft");
    assert_eq!(s.saved_config.drafts.get("room@example").map(String::as_str), Some("room draft"));
}

#[test]
fn held_member_press_cannot_retarget_after_roster_or_room_changes() {
    use iced::advanced::widget::Operation;
    struct Target(Option<iced::Rectangle>);
    impl Operation for Target {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) {
            if text == "bob" { self.0 = Some(bounds); }
        }
    }
    for change in ["redraw", "remove", "reassigned", "anonymous", "reopened"] {
        let mut s = completion_state(&["anna", "bob", "charlie"]);
        for user in &mut s.rooms[0].users { user.jid = Some(format!("{}@example/device", user.name)); }
        let r = renderer();
        let size = iced::Size::new(160.0, 400.0);
        let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
        let mut tree;
        let point;
        {
            let mut widget = crate::ui::members::view(&s);
            tree = Tree::new(&widget);
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut target = Target(None);
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut target);
            point = target.0.unwrap().center();
            let mut output = Vec::new();
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
            assert!(output.is_empty());
        }
        match change {
            "remove" => { s.rooms[0].users.remove(0); }
            "reassigned" => s.rooms[0].users[1].jid = Some("someone-else@example/device".into()),
            "anonymous" => s.rooms[0].users[1].jid = None,
            "reopened" => {
                let _ = s.update(Message::SelectChat("alice@example".into()));
                let _ = s.update(Message::SelectRoom("room@example".into()));
            }
            _ => s.rooms[0].users[0].status = Some("still present".into()),
        }
        let output = {
            let mut widget = crate::ui::members::view(&s);
            tree.diff(widget.as_widget());
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut output = Vec::new();
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)),
                Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
            output
        };
        for message in output { let _ = s.update(message); }
        let new_chats: Vec<_> = s.chats.iter().filter(|chat| chat.jid != "alice@example").map(|chat| chat.jid.as_str()).collect();
        assert_eq!(new_chats, if change == "redraw" { vec!["bob@example"] } else { vec![] }, "{change}");
        assert_eq!(s.active_conv_jid().as_deref(), Some(if change == "redraw" { "bob@example" } else { "room@example" }));
    }
}

#[test]
fn nickname_completion_replaces_the_word_at_the_caret_and_preserves_the_rest() {
    for (input, line, column, expected, next) in [
        ("ask a about this", 0, 5, "ask alice about this", "ask anna about this"),
        ("a rest", 0, 1, "alice: rest", "anna: rest"),
        ("é🙂\r\na remainder", 1, 1, "é🙂\r\nalice remainder", "é🙂\r\nanna remainder"),
        ("ask azzz rest", 0, 5, "ask alice rest", "ask anna rest"),
        ("é a rest", 0, 4, "é alice rest", "é anna rest"),
    ] {
        let mut s = completion_state(&["alice", "anna"]);
        s.message_input = text_editor::Content::with_text(input);
        let cursor = text_editor::Cursor { position: text_editor::Position { line, column }, selection: None };
        s.message_input.move_to(cursor);
        let _ = s.update(Message::TabPressed);
        assert_eq!(s.message_input.text(), expected);
        let completed_cursor = s.message_input.cursor();
        let _ = s.update(Message::TabPressed);
        assert_eq!(s.message_input.text(), next);
        let _ = s.update(Message::UndoInput);
        assert_eq!(s.message_input.text(), expected);
        assert_eq!(s.message_input.cursor(), completed_cursor);
        let _ = s.update(Message::UndoInput);
        assert_eq!(s.message_input.text(), input);
        assert_eq!(s.message_input.cursor(), cursor);
    }
}

#[test]
fn repeated_completion_of_one_candidate_does_not_evict_real_undo_history() {
    let mut s = completion_state(&["alice"]);
    let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Insert('a'))));
    for _ in 0..110 { let _ = s.update(Message::TabPressed); }
    assert_eq!(s.message_input.text(), "alice: ");
    let _ = s.update(Message::UndoInput);
    assert_eq!(s.message_input.text(), "a");
    let _ = s.update(Message::UndoInput);
    assert_eq!(s.message_input.text(), "");
}

#[test]
fn nickname_completion_handles_unicode_names_without_moving_the_trailing_text() {
    let mut s = completion_state(&["Åke", "Åsa"]);
    s.message_input = text_editor::Content::with_text("hi å rest");
    s.message_input.move_to(text_editor::Cursor { position: position(5), selection: None });
    let _ = s.update(Message::TabPressed);
    assert_eq!(s.message_input.text(), "hi Åke rest");
    assert_eq!(s.message_input.cursor().position, position(7));
    let _ = s.update(Message::TabPressed);
    assert_eq!(s.message_input.text(), "hi Åsa rest");
    let _ = s.update(Message::ShiftTabPressed);
    assert_eq!(s.message_input.text(), "hi Åke rest");
}

#[test]
fn nickname_completion_stops_cycling_after_cursor_moves_or_text_is_selected() {
    let mut s = completion_state(&["alice", "anna"]);
    s.message_input = text_editor::Content::with_text("al");
    s.message_input.perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
    let _ = s.update(Message::TabPressed);
    assert_eq!(s.message_input.text(), "alice: ");
    let _ = s.update(Message::InputAction(text_editor::Action::Move(text_editor::Motion::DocumentStart)));
    let _ = s.update(Message::TabPressed);
    assert_eq!(s.message_input.text(), "alice: ");
    s.message_input.move_to(text_editor::Cursor { position: position(2), selection: Some(position(0)) });
    let _ = s.update(Message::TabPressed);
    assert_eq!(s.message_input.text(), "alice: ");
    assert_eq!(s.message_input.selection().as_deref(), Some("al"));
}

#[test]
fn nickname_completion_uses_the_current_room_roster_when_cycling() {
    let mut s = completion_state(&["alice", "anna", "arthur"]);
    s.message_input = text_editor::Content::with_text("a");
    s.message_input.perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
    let _ = s.update(Message::TabPressed);
    assert_eq!(s.message_input.text(), "alice: ");
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::MemberLeft { room: "room@example".into(), nick: "anna".into() }));
    let _ = s.update(Message::TabPressed);
    assert_eq!(s.message_input.text(), "arthur: ");
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::MemberJoined {
        room: "room@example".into(), member: ::xmpp::RoomMember { nick: "amber".into(), jid: None, affiliation: String::new(), role: String::new(), show: None, status: None },
    }));
    let _ = s.update(Message::ShiftTabPressed);
    assert_eq!(s.message_input.text(), "amber: ");
}

#[test]
fn room_switch_does_not_resume_the_previous_rooms_nickname_completion() {
    let mut s = state();
    s.state = AppState::Connected;
    for jid in ["first@example", "second@example"] {
        let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined {
            room: jid.into(), own_nick: "me".into(), members: Vec::new(),
        });
    }
    s.rooms[0].users = ["alice", "anna"].into_iter().map(|nick| room::user::User {
        jid: None, name: nick.into(), show: None, status: None,
    }).collect();
    s.rooms[1].users = vec![room::user::User { jid: None, name: "bob".into(), show: None, status: None }];
    s.saved_config.drafts.insert("second@example".into(), "alice: ".into());
    let _ = s.update(Message::SelectRoom("first@example".into()));
    let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Insert('a'))));
    let _ = s.update(Message::TabPressed);
    assert_eq!(s.message_input.text(), "alice: ");
    let _ = s.update(Message::SelectRoom("second@example".into()));
    let _ = s.update(Message::TabPressed);
    assert_eq!(s.message_input.text(), "alice: ", "completion must not insert a name from the previous room");
    let _ = s.update(Message::SelectRoom("first@example".into()));
    let _ = s.update(Message::UndoInput);
    assert_eq!(s.message_input.text(), "a", "completion remains undoable in its original room");
}

#[test]
fn requested_room_entry_saves_the_departing_draft_without_changing_autojoin_preferences() {
    for remembered in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.chats[0].messages.clear();
        s.body_editors.clear();
        let history = History::open_in_memory();
        history.insert("alice@example", "alice", "read message", chrono::Utc::now(), false, None).unwrap();
        s.history = Some(history);
        seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
        s.sync_body_editors();
        s.save_room = remembered;
        if remembered { s.saved_config.rooms.push("room@example".into()); }
        let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Insert('x'))));
        s.find = Some(Default::default());
        anchor(&mut s, 1, 2);
        s.joining_room = Some("room@example".into());
        let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined {
            room: "room@example".into(), own_nick: "me".into(), members: Vec::new(),
        });
        assert_eq!(s.active_conv_jid().as_deref(), Some("room@example"));
        assert!(s.find.is_none());
        assert!(s.body_drag.is_none());
        assert_eq!(storage::load().drafts.get("alice@example").map(String::as_str), Some("x"));
        assert_eq!(s.saved_config.rooms.contains(&"room@example".to_string()), remembered);
        assert!(s.chats[0].read_boundary.is_some());
        let _ = s.update(Message::SelectChat("alice@example".into()));
        assert_eq!(s.message_input.text(), "x");
        let _ = s.update(Message::UndoInput);
        assert_eq!(s.message_input.text(), "");
    }
}

#[test]
fn room_archive_preserves_the_entire_nickname_after_the_first_jid_separator() {
    let mut s = state();
    s.connected_jid = Some("me@example/device".into());
    s.history = Some(History::open_in_memory());
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() });
    s.mam_pending.insert("room-archive".into(), crate::app::ArchiveContext { conversation: "room@example".into(), is_muc: true });
    s.rooms[0].mam_in_flight = true;
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage { to: None,
        query_id: "room-archive".into(), from: "room@example/team/alice".into(), body: "archived room message".into(),
        timestamp: chrono::Utc::now(), stanza_id: Some("room-archive-id".into()),
        origin_id: None,
    }));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete {
        query_id: "room-archive".into(), complete: true, first: Some("room-archive-id".into()), last: Some("room-archive-id".into()),
    }));
    let stored = s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].sender, "team/alice");
    assert!(s.rooms[0].messages.iter().any(|message| matches!(message, room::message::Message::Chat { from, body, .. } if from == "team/alice" && body == "archived room message")));
}

#[test]
fn room_echoes_match_exact_origins_and_never_confirm_new_text_from_old_history() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.history = Some(History::open_in_memory());
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() });
    let _ = s.update(Message::SelectRoom("room@example".into()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    let mut sends = Vec::new();
    for _ in 0..2 {
        s.message_input = text_editor::Content::with_text("same text");
        let _ = s.update(Message::SendMessage);
        let xmpp::XmppCommand::SendRoomMessage { origin_id: Some(origin), .. } = rx.try_recv().unwrap().into_inner() else { panic!("identified room send"); };
        let temp = s.rooms[0].messages.iter().filter_map(|message| match message { room::message::Message::Chat { id, .. } if *id < 0 => Some(*id), _ => None }).min().unwrap();
        sends.push((temp, origin));
    }
    let status = |s: &Snack, temp| s.rooms[0].messages.iter().find_map(|message| match message { room::message::Message::Chat { id, status, .. } if *id == temp => Some(*status), _ => None });
    let echo = |origin_id, delayed, sid: &str| Message::XmppEvent(xmpp::XmppEvent::RoomMessage {
        room: "room@example".into(), nick: "me".into(), body: "same text".into(), timestamp: chrono::Utc::now(),
        delayed, stanza_id: Some(sid.into()), origin_id,
    });
    let _ = s.update(echo(Some("old-origin".into()), false, "old"));
    let _ = s.update(echo(None, true, "old-without-origin"));
    // A historical occupant may have used today's own nickname, so history
    // never confirms by nickname or with different text. Our queued origin ID
    // is unguessable until the room delivers that send, so history carrying it
    // with the same text proves delivery even if the acknowledgement was lost.
    let other_text = Message::XmppEvent(xmpp::XmppEvent::RoomMessage {
        room: "room@example".into(), nick: "me".into(), body: "different text".into(), timestamp: chrono::Utc::now(),
        delayed: true, stanza_id: Some("old-with-other-text".into()), origin_id: Some(sends[0].1.clone()),
    });
    let _ = s.update(other_text);
    assert!(matches!(status(&s, sends[0].0), Some(ChatStatus::Sending(_))));
    assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 2);
    for _ in 0..2 {
        let _ = s.update(echo(Some(sends[0].1.clone()), true, "history-copy-of-first"));
    }
    assert!(status(&s, sends[0].0).is_none());
    assert!(matches!(status(&s, sends[1].0), Some(ChatStatus::Sending(_))));
    assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 1);
    assert!(s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap().iter().any(|message| message.body == "same text"));
    // If the archive stored this copy first, dedup must still retire its pending UI entry.
    s.history.as_ref().unwrap().insert("room@example", "me", "same text", chrono::Utc::now(), true, Some("second")).unwrap();
    let _ = s.update(echo(Some(sends[1].1.clone()), false, "second"));
    assert!(status(&s, sends[1].0).is_none());
    assert!(s.room_send_origins.is_empty());
}

#[test]
fn own_dm_archives_reconcile_exact_origins_across_clock_skew_and_preserve_distinct_sends() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.history = Some(History::open_in_memory());
    let (tx, _rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    for _ in 0..2 {
        s.message_input = text_editor::Content::with_text("identical outgoing body");
        let _ = s.update(Message::SendMessage);
    }
    let queued = s.history.as_ref().unwrap().load_outbox().unwrap();
    s.mam_pending.insert("archive".into(), crate::app::ArchiveContext { conversation: "alice@example".into(), is_muc: false });
    let archive = |origin: &str, sid: &str, timestamp| Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage { to: None,
        query_id: "archive".into(), from: "me@example/other-resource".into(), body: "identical outgoing body".into(),
        timestamp, stanza_id: Some(sid.into()), origin_id: Some(origin.into()),
    });
    let old = chrono::Utc::now() - chrono::Duration::days(2);
    s.history.as_ref().unwrap().insert("alice@example", "me", "identical outgoing body", old, true, Some("copy-2")).unwrap();
    seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
    assert_eq!(s.chats[0].messages.len(), 3);
    let _ = s.update(archive(&queued[1].origin_id, "copy-2", old));
    assert_eq!(s.chats[0].messages.len(), 2);
    let _ = s.update(archive(&queued[1].origin_id, "copy-2-retransmitted", old + chrono::Duration::hours(1)));
    let stored = s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap();
    assert_eq!(stored.len(), 2);
    assert!(stored.iter().any(|message| message.delivery == crate::storage::history::DeliveryState::Queued(queued[0].id)));
    assert!(stored.iter().any(|message| message.delivery == crate::storage::history::DeliveryState::Submitted(queued[1].id) && message.received.timestamp_millis() == old.timestamp_millis()));
    assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 1);
    let _ = s.update(archive(&queued[0].origin_id, "copy-1", old));
    assert_eq!(s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap().len(), 2);
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
}

#[test]
fn own_archive_origin_supports_equivalent_domains_without_changing_profile() {
    for (bound, from) in [("me@EXAMPLE/device", "me@example/other"),
        ("me@[2001:db8::1]/device", "me@[2001:0db8:0:0:0:0:0:1]/other")] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("profile@alias/device".into());
        s.bound_jid = Some(bound.into());
        s.history = Some(History::open_in_memory());
        let (tx, _rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        s.message_input = text_editor::Content::with_text("outgoing");
        let _ = s.update(Message::SendMessage);
        let queued = s.history.as_ref().unwrap().load_outbox().unwrap().remove(0);
        s.mam_pending.insert("archive".into(), crate::app::ArchiveContext { conversation: "alice@example".into(), is_muc: false });
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage { to: None,
            query_id: "archive".into(), from: from.into(), body: "outgoing".into(),
            timestamp: chrono::Utc::now() - chrono::Duration::days(2), stanza_id: Some("server-copy".into()),
            origin_id: Some(queued.origin_id),
        }));
        let history = s.history.as_ref().unwrap();
        assert!(history.load_outbox().unwrap().is_empty());
        let stored = history.load_tail("alice@example", 10).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].sender, "profile");
    }
}

#[test]
fn remote_sender_cannot_reconcile_an_own_dm_by_copying_origin_and_display_name() {
    for archived in [false, true] {
        for server_id in [None, Some("remote-copy".to_string())] {
            let mut s = state();
            s.state = AppState::Connected;
            s.connected_jid = Some("me@local/device".into());
            s.chats = vec![chat("me@remote", &[])];
            s.chats[0].title = "me".into();
            s.history = Some(History::open_in_memory());
            let (tx, _rx) = tokio::sync::mpsc::channel(10);
            s.xmpp_cmd_tx = Some(tx);
            s.message_input = text_editor::Content::with_text("same body");
            let _ = s.update(Message::SendMessage);
            let queued = s.history.as_ref().unwrap().load_outbox().unwrap().remove(0);
            s.mam_pending.insert("archive".into(), crate::app::ArchiveContext { conversation: "me@remote".into(), is_muc: false });
            let incoming = if archived {
                xmpp::XmppEvent::ArchiveMessage { to: None,
                    query_id: "archive".into(), from: "me@remote/phone".into(), body: "same body".into(), timestamp: chrono::Utc::now(),
                    stanza_id: server_id, origin_id: Some(queued.origin_id),
                }
            } else {
                xmpp::XmppEvent::DirectMessage {
                    from: "me@remote/phone".into(), body: "same body".into(), timestamp: chrono::Utc::now(), delayed: true,
                    stanza_id: server_id, origin_id: Some(queued.origin_id),
                }
            };
            let _ = s.update(Message::XmppEvent(incoming));
            let stored = s.history.as_ref().unwrap().load_tail("me@remote", 10).unwrap();
            assert_eq!(stored.len(), 2);
            assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 1);
            assert!(stored.iter().any(|message| message.delivery == crate::storage::history::DeliveryState::Queued(queued.id)));
        }
    }
}

#[test]
fn own_origin_archive_retry_preserves_cursor_and_reconciles_after_storage_recovers() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.history = Some(History::open_in_memory());
    let (tx, _rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.message_input = text_editor::Content::with_text("own archived message");
    let _ = s.update(Message::SendMessage);
    let queued = s.history.as_ref().unwrap().load_outbox().unwrap().remove(0);
    s.history.as_ref().unwrap().disable_writes();
    s.mam_pending.insert("archive".into(), crate::app::ArchiveContext { conversation: "alice@example".into(), is_muc: false });
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage { to: None,
        query_id: "archive".into(), from: "me@example/phone".into(), body: "own archived message".into(),
        timestamp: chrono::Utc::now() - chrono::Duration::days(1), stanza_id: Some("own-copy".into()), origin_id: Some(queued.origin_id.clone()),
    }));
    assert_eq!(s.pending_history_writes.len(), 1);
    assert_eq!(s.pending_history_writes[0].own_origin.as_deref(), Some(queued.origin_id.as_str()));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: "archive".into(), complete: true, first: Some("own-copy".into()), last: None }));
    assert!(s.chats[0].mam_cursor.is_none());
    s.history.as_ref().unwrap().enable_writes();
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.pending_history_writes.is_empty());
    let stored = s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].delivery, crate::storage::history::DeliveryState::Submitted(queued.id));
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
}

#[test]
fn best_effort_archive_acceptance_survives_late_transport_failure_and_storage_recovery() {
    for outage in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        let h = History::open_in_memory();
        if outage { h.disable_writes(); } else { h.disable_outbox_reads(); }
        s.history = Some(h);
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        s.message_input = text_editor::Content::with_text("best effort archived");
        let _ = s.update(Message::SendMessage);
        let xmpp::XmppCommand::SendDirectMessage { id, origin_id: Some(origin), .. } = rx.try_recv().unwrap() else { panic!("identified best effort DM"); };
        if !outage { s.history.as_ref().unwrap().enable_outbox_reads(); }
        s.mam_pending.insert("archive".into(), crate::app::ArchiveContext { conversation: "alice@example".into(), is_muc: false });
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage { to: None,
            query_id: "archive".into(), from: "me@example/phone".into(), body: "best effort archived".into(),
            timestamp: chrono::Utc::now() - chrono::Duration::days(1), stanza_id: Some("best-effort-copy".into()), origin_id: Some(origin),
        }));
        if outage {
            assert_eq!(s.pending_history_writes.len(), 1);
            s.history.as_ref().unwrap().enable_writes();
            let _ = s.update(Message::RetryHistoryWrites);
        }
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id, ok: false }));
        let messages = s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].delivery, crate::storage::history::DeliveryState::UnqueuedSubmitted);
        assert!(s.chats[0].messages.iter().any(|message| matches!(message, room::message::Message::Chat { body, status: ChatStatus::Confirmed, .. } if body == "best effort archived")));
        assert!(s.pending_history_writes.is_empty());
    }
}

#[test]
fn room_retransmission_origins_are_persistent_but_other_and_old_nicknames_are_not_authority() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.history = Some(History::open_in_memory());
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() });
    let _ = s.update(Message::SelectRoom("room@example".into()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.message_input = text_editor::Content::with_text("room message");
    let _ = s.update(Message::SendMessage);
    let xmpp::XmppCommand::SendRoomMessage { id, origin_id: Some(origin), .. } = rx.try_recv().unwrap().into_inner() else { panic!("room send"); };
    let echo = |nick: &str, sid: &str| Message::XmppEvent(xmpp::XmppEvent::RoomMessage {
        room: "room@example".into(), nick: nick.into(), body: "room message".into(), timestamp: chrono::Utc::now(),
        delayed: false, stanza_id: Some(sid.into()), origin_id: Some(origin.clone()),
    });
    let _ = s.update(echo("someone else", "foreign"));
    assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 1);
    let _ = s.update(echo("me", "own-first"));
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
    assert!(s.room_send_origins.is_empty());
    let count = s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap().len();
    let _ = s.update(echo("me", "own-retry"));
    assert_eq!(s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap().len(), count);
    // No temporary correlation survives here; the same server copies still dedup.
    s.room_send_origins.clear();
    let _ = s.update(echo("me", "own-retry"));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::MemberNicknameChanged { room: "room@example".into(), old_nick: "me".into(), new_nick: "new me".into(), is_self: true }));
    let _ = s.update(echo("me", "reused-old-nick"));
    let stored = s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap();
    assert_eq!(stored.len(), count + 1);
    assert_eq!(stored.iter().filter(|message| message.delivery == crate::storage::history::DeliveryState::Submitted(id)).count(), 1);
}

#[test]
fn linked_room_messages_keep_late_delivery_failures_visible() {
    for optimistic in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        s.history = Some(History::open_in_memory());
        let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() });
        let _ = s.update(Message::SelectRoom("room@example".into()));
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        s.message_input = text_editor::Content::with_text("late rejection");
        let _ = s.update(Message::SendMessage);
        let xmpp::XmppCommand::SendRoomMessage { id, origin_id: Some(origin), .. } = rx.try_recv().unwrap().into_inner() else { panic!("room send"); };
        // A restored queue may have no optimistic row in the current window.
        if !optimistic { s.rooms[0].messages.clear(); }
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomMessage {
            room: "room@example".into(), nick: "me".into(), body: "late rejection".into(), timestamp: chrono::Utc::now(),
            delayed: false, stanza_id: Some("room-copy".into()), origin_id: Some(origin.clone()),
        }));
        assert!(s.rooms[0].messages.iter().any(|message| matches!(message, room::message::Message::Chat { status: ChatStatus::Submitted(link), .. } if *link == id)));
        s.history.as_ref().unwrap().disable_writes();
        let _ = s.update(delivery_error(&origin, "room@example", "cancel"));
        assert!(s.rooms[0].messages.iter().any(|message| matches!(message, room::message::Message::Chat { status: ChatStatus::Failed(link), .. } if *link == id)));
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id, ok: true }));
        // Reloading older durable state must not erase a known unsaved failure.
        s.rooms[0].messages.clear();
        s.rooms[0].at_live_tail = false;
        let _ = s.update(Message::SelectRoom("room@example".into()));
        assert!(s.rooms[0].messages.iter().any(|message| matches!(message, room::message::Message::Chat { status: ChatStatus::Failed(link), .. } if *link == id)));
        s.history.as_ref().unwrap().enable_writes();
        let _ = s.update(Message::RetryHistoryWrites);
        assert_eq!(s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap()[0].delivery, crate::storage::history::DeliveryState::Failed(id));
    }
}

#[test]
fn room_echo_storage_retry_retains_the_identity_verified_before_a_nickname_change() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.history = Some(History::open_in_memory());
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() });
    let _ = s.update(Message::SelectRoom("room@example".into()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.message_input = text_editor::Content::with_text("stored after outage");
    let _ = s.update(Message::SendMessage);
    let xmpp::XmppCommand::SendRoomMessage { origin_id: Some(origin), .. } = rx.try_recv().unwrap().into_inner() else { panic!("room send"); };
    s.history.as_ref().unwrap().disable_writes();
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomMessage {
        room: "room@example".into(), nick: "me".into(), body: "stored after outage".into(), timestamp: chrono::Utc::now(),
        delayed: false, stanza_id: Some("own-copy".into()), origin_id: Some(origin),
    }));
    assert!(s.pending_history_writes[0].own_room_origin);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::MemberNicknameChanged { room: "room@example".into(), old_nick: "me".into(), new_nick: "new me".into(), is_self: true }));
    s.history.as_ref().unwrap().enable_writes();
    let _ = s.update(Message::RetryHistoryWrites);
    let messages = s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].sender, "me");
    assert!(s.pending_history_writes.is_empty());
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
    assert_eq!(s.rooms[0].messages.iter().filter(|message| matches!(message, room::message::Message::Chat { body, .. } if body == "stored after outage")).count(), 1);
}

#[test]
fn a_queued_leave_survives_rejoin_cancellation_and_keeps_fifo_order() {
    let mut s = state();
    s.state = AppState::Connected;
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    s.xmpp_cmd_tx = Some(tx);
    s.send_xmpp_command(xmpp::XmppCommand::JoinRoom("room@example".into()));
    s.send_xmpp_command(xmpp::XmppCommand::LeaveRoom { room: "room@example".into(), nick: "me".into() });
    let departure = s.room_operations["room@example"].clone();
    s.send_xmpp_command(xmpp::XmppCommand::JoinRoom("room@example".into()));
    assert!(!departure.is_current());
    assert_eq!(s.xmpp_pending_commands.len(), 2);
    let first = rx.try_recv().unwrap();
    assert!(!first.should_execute());
    s.flush_xmpp_commands();
    let leave = rx.try_recv().unwrap();
    assert!(matches!(leave.inner(), xmpp::XmppCommand::LeaveRoom { .. }));
    assert!(leave.should_execute());
    assert!(!leave.is_current());
    s.flush_xmpp_commands();
    let join = rx.try_recv().unwrap();
    assert!(matches!(join.inner(), xmpp::XmppCommand::JoinRoom(_)));
    assert!(join.is_current());
    assert!(s.xmpp_pending_commands.is_empty());
}

#[test]
fn old_room_operations_cannot_change_a_replacement_entry_but_keep_message_history() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.history = Some(History::open_in_memory());
    let (tx, mut rx) = tokio::sync::mpsc::channel(30);
    s.xmpp_cmd_tx = Some(tx);
    let room_event = |context: &str, event| Message::XmppEvent(xmpp::XmppEvent::RoomEvent { room: "room@example".into(), context: context.into(), event: Box::new(event) });
    s.send_xmpp_command(xmpp::XmppCommand::JoinRoom("room@example".into()));
    let first = s.room_operations["room@example"].clone();
    let _ = s.update(room_event(&first.id, xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "old".into(), members: Vec::new() }));
    let _ = s.update(Message::SelectRoom("room@example".into()));
    let _ = s.update(Message::LeaveRoom);
    let departure = s.room_operations["room@example"].clone();
    s.send_xmpp_command(xmpp::XmppCommand::JoinRoom("room@example".into()));
    let current = s.room_operations["room@example"].clone();
    let _ = s.update(room_event(&current.id, xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "new".into(), members: Vec::new() }));
    while rx.try_recv().is_ok() {}
    assert!(!first.is_current());
    assert!(!departure.is_current());
    assert!(current.is_current());
    s.message_input = text_editor::Content::with_text("new draft");
    for (context, event) in [
        (&departure.id, xmpp::XmppEvent::RoomLeft("room@example".into())),
        (&first.id, xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "stale".into(), members: Vec::new() }),
        (&first.id, xmpp::XmppEvent::RoomJoinFailed { room: "room@example".into(), reason: "old failure".into() }),
        (&first.id, xmpp::XmppEvent::MemberNicknameChanged { room: "room@example".into(), old_nick: "new".into(), new_nick: "stale".into(), is_self: true }),
        (&first.id, xmpp::XmppEvent::RoomSubject { room: "room@example".into(), subject: "old topic".into() }),
    ] { let _ = s.update(room_event(context, event)); }
    assert_eq!(s.rooms.len(), 1);
    assert_eq!(s.rooms[0].own_nick, "new");
    assert!(s.rooms[0].topic.is_empty());
    assert_eq!(s.message_input.text(), "new draft");
    assert!(s.room_is_joined("room@example"));
    assert!(rx.try_recv().is_err());
    s.message_input = text_editor::Content::with_text("same body");
    let _ = s.update(Message::SendMessage);
    let old_time = chrono::Utc::now() - chrono::Duration::minutes(5);
    let _ = s.update(room_event(&first.id, xmpp::XmppEvent::RoomMessage { room: "room@example".into(), nick: "new".into(), body: "same body".into(), timestamp: old_time, delayed: false, stanza_id: Some("old-archive-id".into()), origin_id: None }));
    assert_eq!(s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap().len(), 1);
    assert!(s.rooms[0].messages.iter().any(|m| matches!(m, room::message::Message::Chat { body, status: ChatStatus::Sending(_), .. } if body == "same body")));
    let _ = s.update(room_event(&current.id, xmpp::XmppEvent::RoomMessage { room: "room@example".into(), nick: "new".into(), body: "same body".into(), timestamp: chrono::Utc::now(), delayed: false, stanza_id: Some("new-archive-id".into()), origin_id: None }));
    assert!(s.rooms[0].messages.iter().any(|m| matches!(m, room::message::Message::Chat { body, status: ChatStatus::Confirmed, .. } if body == "same body")));
    let _ = s.update(Message::Disconnect);
    assert!(!current.is_current());
    assert!(s.room_operations.is_empty());
}

#[test]
fn queued_confirmation_after_leave_cannot_reopen_the_room_and_final_messages_are_saved() {
    for fail_writes in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        s.history = Some(History::open_in_memory());
        let (tx, mut rx) = tokio::sync::mpsc::channel(30);
        s.xmpp_cmd_tx = Some(tx);
        let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() });
        let _ = s.update(Message::SelectRoom("room@example".into()));
        let _ = s.update(Message::LeaveRoom);
        while rx.try_recv().is_ok() {}
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() }));
        assert!(s.rooms.is_empty());
        assert!(s.active.is_none());
        assert!(!s.room_connections.contains_key("room@example"));
        assert!(s.outbox_replay_pending.is_empty());
        assert!(rx.try_recv().is_err());
        if fail_writes { s.history.as_ref().unwrap().disable_writes(); }
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomMessage {
            room: "room@example".into(), nick: "alice".into(), body: "last message before leaving".into(),
            timestamp: chrono::Utc::now(), delayed: false, stanza_id: Some("final-id".into()),
            origin_id: None,
        }));
        if fail_writes {
            assert_eq!(s.pending_history_writes.len(), 1);
            s.history.as_ref().unwrap().enable_writes();
            let _ = s.update(Message::RetryHistoryWrites);
            assert!(s.pending_history_writes.is_empty());
        }
        let saved = s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].body, "last message before leaving");
        assert!(s.rooms.is_empty());
        assert!(s.active.is_none());
    }
}

#[test]
fn late_confirmation_after_failure_requires_a_new_join_request_before_replay() {
    let mut s = state();
    s.state = AppState::Connected;
    s.history = Some(History::open_in_memory());
    s.history.as_ref().unwrap().enqueue_outbox("room@example", true, "wait for entry").unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(30);
    s.xmpp_cmd_tx = Some(tx);
    s.send_xmpp_command(xmpp::XmppCommand::JoinRoom("room@example".into()));
    rx.try_recv().unwrap().into_inner();
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomJoinFailed { room: "room@example".into(), reason: "Entry cancelled".into() }));
    let joined = || Message::XmppEvent(xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() });
    let _ = s.update(joined());
    assert!(s.rooms.is_empty());
    assert!(matches!(s.room_connections.get("room@example"), Some(RoomConnection::Failed(_))));
    assert!(rx.try_recv().is_err());
    assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 1);
    let _ = s.update(Message::RetryRoomJoin("room@example".into()));
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::JoinRoom(_)));
    let _ = s.update(joined());
    assert_eq!(s.rooms.len(), 1);
    assert!(s.room_is_joined("room@example"));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomSubject { room: "room@example".into(), subject: String::new() }));
    assert!(std::iter::from_fn(|| rx.try_recv().ok().map(xmpp::XmppCommand::into_inner)).any(|command| matches!(command, xmpp::XmppCommand::SendRoomMessage { body, .. } if body == "wait for entry")));
}

#[test]
fn automatic_rejoin_failures_are_retryable_and_new_messages_wait_for_membership() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.history = Some(History::open_in_memory());
    let (tx, mut rx) = tokio::sync::mpsc::channel(30);
    s.xmpp_cmd_tx = Some(tx);
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() });
    let _ = s.update(Message::SelectRoom("room@example".into()));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Reconnecting));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Reconnected { generation: 1 }));
    while rx.try_recv().is_ok() {}
    s.message_input = text_editor::Content::with_text("keep drafting");
    let active = s.active;
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::PresenceError { from: "room@example/me".into(), condition: "forbidden".into(), text: None }));
    assert!(matches!(s.room_connections.get("room@example"), Some(RoomConnection::Failed(reason)) if reason.contains("banned")));
    assert_eq!(s.active, active);
    assert_eq!(s.message_input.text(), "keep drafting");
    assert!(s.join_error.is_none());
    s.history.as_ref().unwrap().disable_writes();
    let _ = s.update(Message::SendMessage);
    assert_eq!(s.message_input.text(), "keep drafting");
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
    assert!(rx.try_recv().is_err());
    s.history.as_ref().unwrap().enable_writes();
    let _ = s.update(Message::SendMessage);
    assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 1);
    assert!(rx.try_recv().is_err());
    let _ = s.update(Message::RetryRoomJoin("room@example".into()));
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::JoinRoom(jid) if jid == "room@example"));
    let _ = s.update(Message::RetryRoomJoin("room@example".into()));
    assert!(rx.try_recv().is_err());
    assert!(matches!(s.room_connections.get("room@example"), Some(RoomConnection::Joining)));
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() });
    assert!(s.room_is_joined("room@example"));
    assert!(std::iter::from_fn(|| rx.try_recv().ok().map(xmpp::XmppCommand::into_inner)).any(|cmd| matches!(cmd, xmpp::XmppCommand::SendRoomMessage { body, .. } if body == "keep drafting")));
    let _ = s.update(Message::Disconnect);
    assert!(s.room_connections.is_empty());
}

#[test]
fn startup_keeps_saved_rooms_visible_while_joining_and_after_failure() {
    use iced::advanced::widget::Operation;
    fn labels(mut widget: iced::Element<'_, Message>) -> Vec<String> {
        #[derive(Default)]
        struct Labels(Vec<String>);
        impl Operation for Labels {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
            fn text(&mut self, _id: Option<&iced::widget::Id>, _bounds: iced::Rectangle, text: &str) { self.0.push(text.into()); }
        }
        let mut tree = Tree::new(&widget);
        let r = renderer();
        let size = iced::Size::new(600.0, 500.0);
        let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
        let mut labels = Labels::default();
        widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut labels);
        labels.0
    }
    let first = "alpha@conference.example";
    let second = "beta@conference.example";
    let saved = vec![first.to_string(), second.to_string(), "alpha@CONFERENCE.EXAMPLE".into()];
    let config = storage::SavedConfig { jid: Some("me@example".into()), room_account: Some("me@example".into()), rooms: saved.clone(), ..Default::default() };
    storage::save(&config).unwrap();
    let mut s = Snack::test_state();
    s.saved_config = storage::load();
    s.jid_input = "me@example".into();
    s.password_input = "isolated test password".into();
    s.remember_me = true;
    let _ = s.update(Message::Connect); // No worker subscription is run.
    let (tx, mut rx) = tokio::sync::mpsc::channel(20);
    s.xmpp_cmd_tx = Some(tx);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Connected));
    let commands: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok().map(xmpp::XmppCommand::into_inner)).collect();
    assert_eq!(commands.iter().filter(|command| matches!(command, xmpp::XmppCommand::JoinRoom(_))).count(), 2);
    assert_eq!(s.unopened_room_jids(), vec![first, second]);
    let sidebar = labels(crate::ui::sidebar::view(&s));
    assert!(sidebar.iter().any(|label| label == first));
    assert!(sidebar.iter().any(|label| label == second));
    assert_eq!(sidebar.iter().filter(|label| *label == "Joining…").count(), 2);
    assert!(labels(crate::ui::chat::view(&s)).iter().any(|label| label == "Joining your rooms…"));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomJoinFailed { room: second.into(), reason: "Entry timed out".into() }));
    let sidebar = labels(crate::ui::sidebar::view(&s));
    assert!(sidebar.iter().any(|label| label == second));
    assert!(sidebar.iter().any(|label| label == "Could not join"));
    assert!(sidebar.iter().any(|label| label == "Retry join"));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomJoinFailed { room: first.into(), reason: "Temporarily unavailable".into() }));
    assert!(labels(crate::ui::chat::view(&s)).iter().any(|label| label == "Your rooms are in the sidebar"));
    let _ = s.update(Message::ShowJoinPanel);
    assert!(labels(crate::ui::chat::view(&s)).iter().any(|label| label == "Cancel"));
    let _ = s.update(Message::HideJoinPanel);
    assert!(labels(crate::ui::chat::view(&s)).iter().any(|label| label == "Your rooms are in the sidebar"));
    let _ = s.update(Message::ScopedRoomRetry(s.room_retry_context(first)));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomJoined { room: first.into(), own_nick: "me".into(), members: Vec::new() }));
    assert_eq!(s.active_conv_jid().as_deref(), Some(first));
    assert_eq!(s.rooms.len(), 1);
    assert_eq!(s.unopened_room_jids(), vec![second]);
    assert_eq!(s.saved_config.rooms, saved);
    assert_eq!(storage::load().rooms, saved);
    let _ = s.update(Message::ScopedRoomRetry(s.room_retry_context(second)));
    assert!(std::iter::from_fn(|| rx.try_recv().ok().map(xmpp::XmppCommand::into_inner))
        .any(|command| matches!(command, xmpp::XmppCommand::JoinRoom(room) if room == second)));
    assert!(matches!(s.room_connections.get(second), Some(RoomConnection::Joining)));
}

#[test]
fn failed_saved_rooms_remain_visible_and_rejoin_even_without_an_open_conversation() {
    let mut s = state();
    s.state = AppState::Connected;
    let (tx, mut rx) = tokio::sync::mpsc::channel(20);
    s.xmpp_cmd_tx = Some(tx);
    s.joining_room = Some("manual@example".into());
    assert!(s.send_xmpp_command(xmpp::XmppCommand::JoinRoom("saved@example".into())));
    rx.try_recv().unwrap().into_inner();
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomJoinFailed { room: "saved@example".into(), reason: "Entry timed out".into() }));
    assert!(matches!(s.room_connections.get("saved@example"), Some(RoomConnection::Failed(reason)) if reason == "Entry timed out"));
    assert_eq!(s.joining_room.as_deref(), Some("manual@example"));
    assert!(s.join_error.is_none());
    assert!(s.rooms.is_empty());
    let _ = s.view();
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Reconnecting));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Reconnected { generation: 2 }));
    let commands: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok().map(xmpp::XmppCommand::into_inner)).collect();
    assert_eq!(commands.iter().filter(|cmd| matches!(cmd, xmpp::XmppCommand::JoinRoom(jid) if jid == "saved@example")).count(), 1);
    assert_eq!(commands.iter().filter(|cmd| matches!(cmd, xmpp::XmppCommand::JoinRoom(jid) if jid == "manual@example")).count(), 1);
}

#[test]
fn failed_membership_releases_overflow_attempts_and_preserves_unqueued_text() {
    let mut s = state();
    s.room_connections.insert("room@example".into(), RoomConnection::Joined);
    let h = History::open_in_memory();
    let id = h.enqueue_outbox("room@example", true, "durable text").unwrap();
    s.history = Some(h);
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tx.try_send(xmpp::XmppCommand::ForceReconnect).unwrap();
    s.xmpp_cmd_tx = Some(tx);
    s.resend_outbox(|_| true);
    s.send_xmpp_command(xmpp::XmppCommand::SendRoomMessage { id: 0, origin_id: None, room: "room@example".into(), body: "unsaved text".into() });
    assert!(s.outbox_in_flight.contains(&id));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomJoinFailed { room: "room@example".into(), reason: "Entry refused".into() }));
    assert!(s.xmpp_pending_commands.is_empty());
    assert!(s.outbox_in_flight.is_empty());
    assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 1);
    assert_eq!(s.rejected_sends[0].body, "unsaved text");
    rx.try_recv().unwrap().into_inner();
    s.flush_xmpp_commands();
    assert!(rx.try_recv().is_err());
    // A replacement join can also invalidate commands before a failure has
    // arrived. Flush must retain non-durable text and release durable attempts.
    s.room_connections.insert("room@example".into(), RoomConnection::Joined);
    s.xmpp_cmd_tx.as_ref().unwrap().try_send(xmpp::XmppCommand::ForceReconnect).unwrap();
    s.resend_outbox(|_| true);
    s.send_xmpp_command(xmpp::XmppCommand::SendRoomMessage { id: 0, origin_id: None, room: "room@example".into(), body: "second unsaved text".into() });
    s.send_xmpp_command(xmpp::XmppCommand::JoinRoom("room@example".into()));
    assert!(s.outbox_in_flight.is_empty());
    assert!(s.rejected_sends.iter().any(|send| send.body == "second unsaved text"));
    rx.try_recv().unwrap().into_inner();
    s.flush_xmpp_commands();
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::JoinRoom(_)));
}

#[test]
fn room_identity_tracks_assigned_nicknames_and_confirms_echoes_after_rename() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("account@example/resource".into());
    s.history = Some(History::open_in_memory());
    let (tx, _rx) = tokio::sync::mpsc::channel(20);
    s.xmpp_cmd_tx = Some(tx);
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined {
        room: "room@example".into(), own_nick: "assigned".into(), members: Vec::new(),
    });
    let _ = s.update(Message::SelectRoom("room@example".into()));
    s.window_focused = false;
    s.message_input = text_editor::Content::with_text("pending text");
    let _ = s.update(Message::SendMessage);
    assert!(s.rooms[0].messages.iter().any(|m| matches!(m, room::message::Message::Chat { from, status: ChatStatus::Sending(_), .. } if from == "assigned")));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::MemberNicknameChanged {
        room: "room@example".into(), old_nick: "assigned".into(), new_nick: "renamed".into(), is_self: true,
    }));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomMessage {
        room: "room@example".into(), nick: "renamed".into(), body: "pending text".into(),
        timestamp: chrono::Utc::now(), delayed: false, stanza_id: Some("echo".into()),
        origin_id: None,
    }));
    assert_eq!(s.rooms[0].own_nick, "renamed");
    assert!(!s.rooms[0].unread);
    let chats: Vec<_> = s.rooms[0].messages.iter().filter(|m| matches!(m, room::message::Message::Chat { body, .. } if body == "pending text")).collect();
    assert_eq!(chats.len(), 1);
    assert!(matches!(chats[0], room::message::Message::Chat { from, status: ChatStatus::Confirmed, .. } if from == "renamed"));
    s.message_input = text_editor::Content::with_text("next text");
    let _ = s.update(Message::SendMessage);
    assert!(s.rooms[0].messages.iter().any(|m| matches!(m, room::message::Message::Chat { from, body, .. } if body == "next text" && from == "renamed")));
}

#[test]
fn renamed_members_keep_identity_without_departure_events_or_duplicate_roster_rows() {
    let mut s = state();
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined {
        room: "room@example".into(), own_nick: "me".into(), members: Vec::new(),
    });
    s.rooms[0].users.push(room::user::User { name: "old".into(), jid: Some("alice@example/phone".into()), show: None, status: None });
    let before = s.rooms[0].messages.len();
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::MemberNicknameChanged {
        room: "room@example".into(), old_nick: "old".into(), new_nick: "new".into(), is_self: false,
    }));
    assert_eq!(s.rooms[0].own_nick, "me");
    assert_eq!(s.rooms[0].users[0].name, "new");
    assert_eq!(s.rooms[0].users[0].jid.as_deref(), Some("alice@example/phone"));
    assert_eq!(s.rooms[0].messages.len(), before);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::MemberJoined {
        room: "room@example".into(), member: ::xmpp::RoomMember { nick: "new".into(), jid: Some("alice@example/laptop".into()), affiliation: String::new(), role: String::new(), show: None, status: None },
    }));
    assert_eq!(s.rooms[0].users.len(), 1);
    assert_eq!(s.rooms[0].users[0].jid.as_deref(), Some("alice@example/laptop"));
    assert_eq!(s.rooms[0].messages.len(), before);
}

#[test]
fn events_from_replaced_workers_cannot_mutate_the_current_account() {
    // Include a new login to the same account: comparing JIDs alone cannot
    // distinguish a previous connection from its replacement.
    for old_jid in ["old@example/resource", "me@example/resource"] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/resource".into());
        let (_old_tx, old_channel) = xmpp::new_command_channel(old_jid.into(), "secret".into());
        let (tx, current_channel) = xmpp::new_command_channel("me@example/resource".into(), "secret".into());
        s.xmpp_cmd_tx = Some(tx);
        s.xmpp_cmd_rx = Some(current_channel.clone());
        let h = History::open_in_memory();
        let id = h.enqueue_outbox("alice@example", false, "current account message").unwrap();
        s.history = Some(h);
        for event in [
            xmpp::XmppEvent::SendResult { id, ok: true },
            xmpp::XmppEvent::BoundJid("old@example/old".into()),
            xmpp::XmppEvent::SendRejected { id, conversation: "private@example".into(), body: "old account secret".into(), reason: "rejected".into() },
            xmpp::XmppEvent::RoomJoined { own_nick: "me".into(), room: "old-room@example".into(), members: Vec::new() },
            xmpp::XmppEvent::Disconnected("old connection failed".into()),
        ] {
            let _ = s.update(Message::ScopedXmppEvent { channel: old_channel.clone(), event });
        }
        assert!(matches!(s.state, AppState::Connected));
        assert_eq!(s.xmpp_cmd_rx.as_ref(), Some(&current_channel));
        assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 1);
        assert!(s.pending_outbox_deletes.is_empty());
        assert!(s.rejected_sends.is_empty());
        assert!(s.rooms.is_empty());
        assert!(s.bound_jid.is_none());
        // The live worker's acknowledgement must still retire the row.
        let _ = s.update(Message::ScopedXmppEvent { channel: current_channel, event: xmpp::XmppEvent::SendResult { id, ok: true } });
        assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
    }
}

#[test]
fn completed_logout_ignores_obsolete_worker_events() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/resource".into());
    let (tx, channel) = xmpp::new_command_channel("me@example/resource".into(), "secret".into());
    s.xmpp_cmd_tx = Some(tx);
    s.xmpp_cmd_rx = Some(channel.clone());
    let _ = s.update(Message::Disconnect);
    let _ = s.update(Message::ScopedXmppEvent { channel: channel.clone(), event: xmpp::XmppEvent::Stopped });
    assert!(s.xmpp_cmd_rx.is_none());
    for event in [
        xmpp::XmppEvent::BoundJid("old@example/old".into()),
        xmpp::XmppEvent::Connected,
        xmpp::XmppEvent::SendRejected { id: 1, conversation: "private@example".into(), body: "private text".into(), reason: "rejected".into() },
        xmpp::XmppEvent::RoomJoined { own_nick: "me".into(), room: "old-room@example".into(), members: Vec::new() },
    ] {
        let _ = s.update(Message::ScopedXmppEvent { channel: channel.clone(), event });
    }
    assert!(matches!(s.state, AppState::Login));
    assert!(s.rooms.is_empty());
    assert!(s.rejected_sends.is_empty());
    assert!(s.connected_jid.is_none());
    assert!(s.bound_jid.is_none());
}

#[test]
fn failed_history_tail_read_preserves_the_window_and_recovers_on_retry() {
    let mut s = state();
    let h = History::open_in_memory();
    h.insert("alice@example", "alice", "durable", chrono::Utc::now(), false, None).unwrap();
    h.disable_reads();
    s.history = Some(h);
    s.chats[0].read_marker = Some(1);
    s.chats[0].at_live_tail = false;
    s.chats[0].has_older = true;
    seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
    assert_eq!(s.chats[0].messages.len(), 3);
    assert_eq!(s.chats[0].read_marker, Some(1));
    assert!(!s.chats[0].at_live_tail);
    assert!(s.chats[0].has_older);
    s.history.as_ref().unwrap().enable_reads();
    seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
    assert_eq!(s.chats[0].messages.len(), 1);
    assert!(matches!(&s.chats[0].messages[0], room::message::Message::Chat { body, .. } if body == "durable"));
    assert!(s.chats[0].at_live_tail);
}

#[test]
fn outgoing_dm_remains_visible_when_detached_history_reload_or_storage_fails() {
    for failure in ["none", "read", "write"] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        s.window_focused = true;
        let (tx, _rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        let h = History::open_in_memory();
        if failure == "read" { h.disable_reads(); }
        if failure == "write" { h.disable_writes(); }
        s.history = Some(h);
        s.chats[0].at_live_tail = false;
        s.chats[0].anchored_bottom = false;
        s.chats[0].unread = true;
        s.message_input = text_editor::Content::with_text("visible outgoing text");
        let _ = s.update(Message::SendMessage);
        let copies = |s: &Snack| s.chats[0].messages.iter().filter(|message| matches!(message, room::message::Message::Chat { body, .. } if body == "visible outgoing text")).count();
        assert_eq!(copies(&s), 1, "{failure}");
        assert!(s.message_input.text().is_empty());
        if failure == "read" {
            assert!(!s.chats[0].at_live_tail);
            assert!(s.chats[0].unread);
            s.history.as_ref().unwrap().enable_reads();
        }
        if failure == "write" { s.history.as_ref().unwrap().enable_writes(); }
        let _ = s.update(Message::RetryHistoryWrites);
        seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
        assert_eq!(copies(&s), 1, "recovery: {failure}");
        assert_eq!(s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap().len(), 1);
        assert!(s.pending_history_writes.is_empty());
        assert!(s.chats[0].parked_messages.is_empty());
    }
}

#[test]
fn queued_dm_status_tracks_exact_acknowledgements_and_late_rejections_after_reload() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.history = Some(History::open_in_memory());
    let (tx, _rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    for _ in 0..2 {
        s.message_input = text_editor::Content::with_text("identical outgoing body");
        let _ = s.update(Message::SendMessage);
    }
    let queued = s.history.as_ref().unwrap().load_outbox().unwrap();
    assert_eq!(queued.len(), 2);
    let first = queued[0].id;
    let second = queued[1].id;
    let statuses = |s: &Snack| s.chats[0].messages.iter().filter_map(|message| match message {
        room::message::Message::Chat { body, status, .. } if body == "identical outgoing body" => Some(*status), _ => None,
    }).collect::<Vec<_>>();
    assert_eq!(statuses(&s), [ChatStatus::Queued(first), ChatStatus::Queued(second)]);
    seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
    assert_eq!(statuses(&s), [ChatStatus::Queued(first), ChatStatus::Queued(second)]);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id: second, ok: true }));
    assert_eq!(statuses(&s), [ChatStatus::Queued(first), ChatStatus::Submitted(second)]);
    let _ = s.update(delivery_error(&queued[1].origin_id, "alice@example/phone", "cancel"));
    assert_eq!(statuses(&s), [ChatStatus::Queued(first), ChatStatus::Failed(second)]);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id: second, ok: true }));
    seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
    assert_eq!(statuses(&s), [ChatStatus::Queued(first), ChatStatus::Failed(second)]);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id: first, ok: false }));
    assert_eq!(statuses(&s), [ChatStatus::Queued(first), ChatStatus::Failed(second)]);
}

#[test]
fn dm_keeps_durable_retry_and_delivery_identity_while_history_is_unavailable() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    let h = History::open_in_memory();
    h.disable_reads();
    s.history = Some(h);
    let (tx, _rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.message_input = text_editor::Content::with_text("durable despite history failure");
    let _ = s.update(Message::SendMessage);
    let queued = s.history.as_ref().unwrap().load_outbox().unwrap();
    assert_eq!(queued.len(), 1);
    let id = queued[0].id;
    assert_eq!(s.pending_history_writes[0].outbox_id, Some(id));
    assert!(s.chats[0].messages.iter().any(|message| matches!(message, room::message::Message::Chat { status: ChatStatus::Queued(outbox), .. } if *outbox == id)));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id, ok: true }));
    s.history.as_ref().unwrap().enable_reads();
    let _ = s.update(Message::RetryHistoryWrites);
    let stored = s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].delivery, crate::storage::history::DeliveryState::Submitted(id));
    assert!(s.pending_history_writes.is_empty());
}

#[test]
fn unsaved_delivery_failure_survives_history_reload_and_stays_account_scoped() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.history = Some(History::open_in_memory());
    let (tx, _rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.message_input = text_editor::Content::with_text("rejected message");
    let _ = s.update(Message::SendMessage);
    let outgoing = s.history.as_ref().unwrap().load_outbox().unwrap().remove(0);
    s.history.as_ref().unwrap().disable_writes();
    let _ = s.update(delivery_error(&outgoing.origin_id, "alice@example/phone", "cancel"));
    assert!(!s.rejected_sends[0].saved);
    s.chats[0].at_live_tail = false;
    let _ = s.update(Message::SelectChat("alice@example".into()));
    assert!(s.chats[0].messages.iter().any(|message| matches!(message, room::message::Message::Chat { status: ChatStatus::Failed(id), .. } if *id == outgoing.id)));
    // A different account can reuse row IDs without inheriting the old failure.
    s.connected_jid = Some("other@example/device".into());
    s.history = Some(History::open_in_memory());
    s.chats = vec![chat("alice@example", &[])];
    s.message_input = text_editor::Content::with_text("different account");
    let _ = s.update(Message::SendMessage);
    assert!(s.chats[0].messages.iter().any(|message| matches!(message, room::message::Message::Chat { status: ChatStatus::Queued(id), .. } if *id == outgoing.id)));
}

#[test]
fn best_effort_dms_match_out_of_order_results_and_late_errors_after_history_reload() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    let h = History::open_in_memory();
    h.disable_outbox_reads();
    s.history = Some(h);
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    let mut attempts = Vec::new();
    for _ in 0..2 {
        s.message_input = text_editor::Content::with_text("same best effort body");
        let _ = s.update(Message::SendMessage);
        let xmpp::XmppCommand::SendDirectMessage { id, origin_id: Some(origin), .. } = rx.try_recv().unwrap() else { panic!("expected identified DM"); };
        assert!(id < 0);
        attempts.push((id, origin));
    }
    assert_ne!(attempts[0].0, attempts[1].0);
    assert_ne!(attempts[0].1, attempts[1].1);
    s.history.as_ref().unwrap().enable_outbox_reads();
    s.chats[0].at_live_tail = false;
    let _ = s.update(Message::SelectChat("alice@example".into()));
    let statuses = |s: &Snack| s.chats[0].messages.iter().filter_map(|message| match message {
        room::message::Message::Chat { body, status, .. } if body == "same best effort body" => Some(*status), _ => None,
    }).collect::<Vec<_>>();
    assert_eq!(statuses(&s), [ChatStatus::Unconfirmed, ChatStatus::Unconfirmed]);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id: attempts[1].0, ok: true }));
    assert_eq!(statuses(&s), [ChatStatus::Unconfirmed, ChatStatus::Confirmed]);
    let _ = s.update(delivery_error(&attempts[1].1, "mallory@example/phone", "cancel"));
    assert!(s.rejected_sends.is_empty());
    let _ = s.update(delivery_error(&attempts[1].1, "alice@example/phone", "cancel"));
    assert_eq!(statuses(&s), [ChatStatus::Unconfirmed, ChatStatus::Failed(0)]);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id: attempts[1].0, ok: true }));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id: attempts[0].0, ok: false }));
    s.chats[0].at_live_tail = false;
    let _ = s.update(Message::SelectChat("alice@example".into()));
    assert_eq!(statuses(&s), [ChatStatus::Unconfirmed, ChatStatus::Failed(0)]);
    assert!(s.unqueued_dm_sends.is_empty());
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
}

#[test]
fn best_effort_results_survive_total_write_failure_and_recover_without_collapsing_messages() {
    let mut s = state();
    s.chats[0].messages.clear();
    s.body_editors.clear();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    let h = History::open_in_memory();
    h.disable_writes();
    s.history = Some(h);
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    let mut ids = Vec::new();
    for _ in 0..2 {
        s.message_input = text_editor::Content::with_text("same body during outage");
        let _ = s.update(Message::SendMessage);
        let xmpp::XmppCommand::SendDirectMessage { id, .. } = rx.try_recv().unwrap() else { panic!("DM"); };
        ids.push(id);
    }
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id: ids[1], ok: true }));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id: ids[0], ok: false }));
    assert_eq!(s.unqueued_dm_sends.len(), 2);
    for id in &ids { let _ = s.update(Message::SelectMessage { id: *id }); }
    s.history.as_ref().unwrap().enable_writes();
    let _ = s.update(Message::RetryHistoryWrites);
    let messages = s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(s.selected_msgs, messages.iter().map(|message| message.id).collect());
    assert_eq!(s.selected_message_text().unwrap().matches("same body during outage").count(), 2);
    assert_eq!(messages[0].delivery, crate::storage::history::DeliveryState::Unconfirmed);
    assert_eq!(messages[1].delivery, crate::storage::history::DeliveryState::UnqueuedSubmitted);
    assert!(s.unqueued_dm_sends.is_empty());
    assert!(s.chats[0].parked_messages.is_empty());
}

#[test]
fn interrupted_best_effort_send_stays_unconfirmed_and_is_not_replayed_automatically() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    let h = History::open_in_memory();
    h.disable_outbox_reads();
    s.history = Some(h);
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.message_input = text_editor::Content::with_text("outcome unknown");
    let _ = s.update(Message::SendMessage);
    let xmpp::XmppCommand::SendDirectMessage { id, .. } = rx.try_recv().unwrap() else { panic!("DM"); };
    s.history.as_ref().unwrap().enable_outbox_reads();
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Reconnecting));
    assert!(s.unqueued_dm_sends.is_empty());
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id, ok: true }));
    let stored = s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap();
    assert_eq!(stored[0].delivery, crate::storage::history::DeliveryState::Unconfirmed);
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
}

#[test]
fn delayed_messages_keep_storage_order_and_unread_boundary_before_and_after_reload() {
    for rooms in [false, true] {
        let mut s = state();
        s.window_focused = true;
        s.connected_jid = Some("me@example/device".into());
        s.history = Some(History::open_in_memory());
        let conv = if rooms { "room@example" } else { "alice@example" };
        if rooms {
            let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: conv.into(), own_nick: "me".into(), members: Vec::new() });
        } else { s.chats[0].messages.clear(); }
        s.active = None; // Unwatched, without invoking desktop notifications.
        let now = chrono::Utc::now();
        for (body, offset) in [("newer", 20), ("older", 10), ("oldest", 0)] {
            let timestamp = now + chrono::Duration::seconds(offset);
            let event = if rooms {
                xmpp::XmppEvent::RoomMessage { room: conv.into(), nick: "alice".into(), body: body.into(), timestamp, delayed: true, stanza_id: Some(body.into()), origin_id: None }
            } else {
                xmpp::XmppEvent::DirectMessage { from: "alice@example/phone".into(), body: body.into(), timestamp, delayed: true, stanza_id: Some(body.into()), origin_id: None }
            };
            let _ = s.update(Message::XmppEvent(event));
        }
        let backlog: &mut dyn Backlog = if rooms { &mut s.rooms[0] } else { &mut s.chats[0] };
        let ids = |backlog: &dyn Backlog| backlog.messages().iter().filter_map(|message| match message { room::message::Message::Chat { id, .. } => Some(*id), _ => None }).collect::<Vec<_>>();
        let before = ids(backlog);
        assert_eq!(backlog.read_marker(), Some(0));
        let stored = s.history.as_ref().unwrap().load_tail(conv, 10).unwrap();
        assert_eq!(before, stored.iter().map(|message| message.id).collect::<Vec<_>>());
        assert_eq!(stored.iter().map(|message| message.body.as_str()).collect::<Vec<_>>(), vec!["oldest", "older", "newer"]);
        assert_eq!(backlog.oldest_loaded_id(), before.first().copied());
        backlog.seed_tail(stored, false);
        assert_eq!(ids(backlog), before);
        assert_eq!(backlog.read_marker(), Some(0));
    }
}

#[test]
fn chronological_reordering_preserves_boundaries_and_trims_the_oldest_rows() {
    let mut s = state();
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() });
    let b = &mut s.rooms[0];
    b.messages.clear();
    let now = chrono::Utc::now();
    let row = |id, offset| crate::storage::history::StoredMessage {
        id, sender: "alice".into(), body: id.to_string(), received: now + chrono::Duration::seconds(offset),
        delivery: crate::storage::history::DeliveryState::Received,
    };
    b.seed_tail(vec![row(1, 10), row(2, 20)], false);
    b.read_marker = Some(1);
    b.history_markers = vec![1];
    let inserted = b.append_live(room::message::Message::Chat { id: 3, from: "alice".into(), body: "3".into(), received: now, status: ChatStatus::Confirmed });
    assert_eq!(inserted, Some(0));
    assert_eq!(b.read_marker, Some(2));
    assert_eq!(b.history_markers, vec![2]);
    b.reconcile_stored(&row(2, -10), &[]);
    assert_eq!(b.read_marker, Some(0));
    assert_eq!(b.history_markers, vec![0]);
    assert_eq!(b.oldest_loaded_id, Some(2));
    // Same-millisecond timestamps use durable row IDs, as SQLite does.
    b.seed_tail(vec![row(4, 10), row(5, 10)], false);
    b.promote_message(5, &row(3, 10));
    assert!(matches!(b.messages[0], room::message::Message::Chat { id: 3, .. }));
    b.messages.clear();
    for id in 1..=backlog::LIVE_TAIL_CAP as i64 {
        let _ = b.append_live(room::message::Message::Chat { id, from: "alice".into(), body: id.to_string(), received: now + chrono::Duration::seconds(id), status: ChatStatus::Confirmed });
    }
    assert_eq!(b.append_live(room::message::Message::Chat { id: 999, from: "alice".into(), body: "too old".into(), received: now, status: ChatStatus::Confirmed }), None);
    assert_eq!(b.messages.len(), backlog::LIVE_TAIL_CAP);
    assert_eq!(b.oldest_loaded_id, Some(1));
    assert!(b.has_older);
}

#[test]
fn search_positions_the_actual_result_with_uneven_message_heights() {
    use iced::advanced::widget::operation::{Operation, Outcome, Scrollable, black_box};
    struct Geometry {
        token: uuid::Uuid,
        scroll: Option<(iced::Rectangle, iced::Rectangle, iced::Vector)>,
        hit: Option<iced::Rectangle>,
    }
    impl Operation for Geometry {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn container(&mut self, id: Option<&iced::widget::Id>, bounds: iced::Rectangle) {
            if id == Some(&crate::ui::search_scroll::marker_id(self.token)) { self.hit = Some(bounds); }
        }
        fn scrollable(&mut self, id: Option<&iced::widget::Id>, bounds: iced::Rectangle, content: iced::Rectangle,
            translation: iced::Vector, _state: &mut dyn Scrollable) {
            if id == Some(&iced::widget::Id::new(crate::app::MESSAGE_SCROLL_ID)) { self.scroll = Some((bounds, content, translation)); }
        }
    }
    for (before_lines, hit_lines, after_lines) in [(80, 1, 1), (1, 1, 80), (1, 80, 1), (80, 1, 0)] {
        let mut s = state();
        s.state = AppState::Connected;
        s.chats[0].messages.clear();
        s.body_editors.clear();
        let h = History::open_in_memory();
        let now = chrono::Utc::now();
        let hit = "needle\n".repeat(hit_lines);
        h.insert("alice@example", "alice", &"before\n".repeat(before_lines), now, false, None).unwrap();
        h.insert("alice@example", "alice", &hit, now + chrono::Duration::seconds(1), false, None).unwrap();
        if after_lines > 0 { h.insert("alice@example", "alice", &"after\n".repeat(after_lines), now + chrono::Duration::seconds(2), false, None).unwrap(); }
        let id = h.load_tail("alice@example", 10).unwrap().iter().find(|row| row.body == hit).unwrap().id;
        s.history = Some(h);
        s.find = Some(crate::app::FindState {
            matches: vec![crate::app::FindMatch { conversation: "alice@example".into(), id, received: chrono::DateTime::UNIX_EPOCH }], ..Default::default()
        });
        let _ = s.update(Message::FindNext);
        let token = s.find.as_ref().unwrap().pending_scroll.unwrap();
        let mut widget = crate::ui::chat::view(&s);
        let mut tree = Tree::new(&widget);
        let r = renderer();
        let size = iced::Size::new(600.0, 400.0);
        let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
        for initial_y in [0.0, 1.0] {
            let mut initial = iced::advanced::widget::operation::scrollable::snap_to::<()>(
                iced::widget::Id::new(crate::app::MESSAGE_SCROLL_ID),
                iced::widget::scrollable::RelativeOffset { x: Some(0.0), y: Some(initial_y) },
            );
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut initial);
            let mut operation: Box<dyn Operation<Message>> = Box::new(crate::ui::search_scroll::SearchScroll::new(token));
            let report = loop {
                widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut black_box(&mut *operation));
                match operation.finish() {
                    Outcome::Chain(next) => operation = next,
                    Outcome::Some(report) => break report,
                    Outcome::None => panic!("search operation must finish"),
                }
            };
            let mut geometry = Geometry { token, scroll: None, hit: None };
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut geometry);
            let (bounds, content, translation) = geometry.scroll.unwrap();
            let hit = geometry.hit.unwrap();
            let top = content.y + translation.y;
            if hit.height <= bounds.height {
                assert!(hit.y >= top - 1.0 && hit.y + hit.height <= top + bounds.height + 1.0,
                    "result must be visible: hit={hit:?}, visible top={top}, height={}", bounds.height);
            } else { assert!((hit.y - top).abs() <= 1.0, "a tall result must show its beginning"); }
            if after_lines == 0 { assert!(matches!(report, Message::SearchViewport { at_bottom: true, .. })); }
        }
    }
}

fn search_scroll_layout(content_height: f32, relative_y: f32, token: uuid::Uuid)
    -> (Option<iced::widget::scrollable::Viewport>, Message)
{
    use iced::advanced::widget::operation::{Operation, Outcome, black_box};
    let target_height = content_height.min(20.0);
    let target_top = ((content_height - 100.0).max(0.0) * (1.0 - relative_y) + 50.0 - target_height / 2.0)
        .clamp(0.0, content_height - target_height);
    let content = iced::widget::column![
        iced::widget::Space::new().height(target_top),
        iced::widget::container(iced::widget::Space::new().height(target_height).width(100))
            .id(crate::ui::search_scroll::marker_id(token)),
        iced::widget::Space::new().height(content_height - target_top - target_height),
    ];
    let mut widget: iced::Element<'_, iced::widget::scrollable::Viewport> = iced::widget::scrollable(content)
        .id(iced::widget::Id::new(crate::app::MESSAGE_SCROLL_ID)).anchor_bottom().height(100).width(100).on_scroll(|viewport| viewport).into();
    let mut tree = Tree::new(&widget);
    let r = renderer();
    let node = widget.as_widget_mut().layout(&mut tree, &r,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(100.0, 100.0)));
    let mut operation: Box<dyn Operation<Message>> = Box::new(crate::ui::search_scroll::SearchScroll::new(token));
    let report = loop {
        widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut black_box(&mut *operation));
        match operation.finish() {
            Outcome::Some(report) => break report,
            Outcome::Chain(next) => operation = next,
            Outcome::None => panic!("search operation must find the marked scrollable"),
        }
    };
    let mut viewports = Vec::new();
    let mut shell = iced::advanced::Shell::new(&mut viewports);
    widget.as_widget_mut().update(&mut tree,
        &iced::Event::Window(iced::window::Event::RedrawRequested(iced::time::Instant::now())),
        Layout::new(&node), iced::mouse::Cursor::Unavailable, &r,
        &mut iced::advanced::clipboard::Null, &mut shell,
        &iced::Rectangle::new(iced::Point::ORIGIN, iced::Size::new(100.0, 100.0)));
    (viewports.pop(), report)
}

fn scroll_viewport(content_height: f32, relative_y: f32) -> iced::widget::scrollable::Viewport {
    search_scroll_layout(content_height, relative_y, uuid::Uuid::new_v4()).0.expect("overflow redraw publishes geometry")
}

fn transcript_wheel_events(s: &Snack) -> Vec<Message> {
    let mut widget = crate::ui::chat::view(s);
    let mut tree = Tree::new(&widget);
    let r = renderer();
    let size = iced::Size::new(600.0, 400.0);
    let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
    let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
    let mut messages = Vec::new();
    widget.as_widget_mut().update(&mut tree,
        &iced::Event::Mouse(iced::mouse::Event::WheelScrolled { delta: iced::mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 } }),
        Layout::new(&node), iced::mouse::Cursor::Available(iced::Point::new(300.0, 150.0)), &r,
        &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut messages), &bounds);
    messages
}

#[test]
fn queued_transcript_scroll_cannot_cancel_a_new_search_jump_in_the_same_chat() {
    for prior_search in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.chats[0].messages.clear();
        s.body_editors.clear();
        let h = History::open_in_memory();
        for i in 0..40 {
            h.insert("alice@example", "alice", &format!("needle {i}\n{}", "long body\n".repeat(5)),
                chrono::Utc::now() + chrono::Duration::seconds(i), false, None).unwrap();
        }
        s.history = Some(h);
        seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
        s.sync_body_editors();
        if prior_search {
            let _ = s.update(Message::ToggleFind);
            let _ = s.update(Message::FindInputChanged("needle".into()));
        }
        let queued = transcript_wheel_events(&s);
        assert!(!queued.is_empty(), "real wheel must produce a scroll callback");
        if prior_search { let _ = s.update(Message::FindPrev); }
        else {
            let _ = s.update(Message::ToggleFind);
            let _ = s.update(Message::FindInputChanged("needle".into()));
        }
        let token = s.find.as_ref().unwrap().pending_scroll.unwrap();
        let late_after_completion = queued.clone();
        for event in queued { let _ = s.update(event); }
        assert_eq!(s.find.as_ref().unwrap().pending_scroll, Some(token), "prior_search={prior_search}");
        let current = transcript_wheel_events(&s);
        assert!(!current.is_empty());
        let _ = s.update(Message::SearchViewport { token, at_bottom: false });
        assert!(!s.chats[0].anchored_bottom);
        for event in late_after_completion { let _ = s.update(event); }
        assert!(!s.chats[0].anchored_bottom, "an obsolete viewport cannot mark the new result as the visible tail");
        assert_eq!(s.find.as_ref().unwrap().scroll_id, token);
        // Completion clears only pending_scroll, not the current view identity.
        for event in current { let _ = s.update(event); }
        assert!(s.find.as_ref().unwrap().pending_scroll.is_none(), "current real input must still override search positioning");
    }
}

#[test]
fn scroll_callbacks_follow_empty_failed_closed_and_reopened_search_views() {
    use crate::message::TranscriptAction;
    for transition in ["empty", "failed", "closed", "reopened"] {
        let mut s = state();
        s.state = AppState::Connected;
        let body = "long transcript\n".repeat(10);
        s.chats[0] = chat("alice@example", &[body.as_str(); 30]);
        s.sync_body_editors();
        let _ = s.update(Message::ToggleFind);
        let queued = transcript_wheel_events(&s);
        assert!(!queued.is_empty());
        match transition {
            "empty" => { let _ = s.update(Message::FindInputChanged(String::new())); }
            "failed" => { let _ = s.update(Message::FindInputChanged("needle".into())); assert!(s.find.as_ref().unwrap().error.is_some()); }
            _ => {
                let _ = s.update(Message::CloseFind);
                if transition == "reopened" { let _ = s.update(Message::ToggleFind); }
            }
        }
        let bottom = |messages: &[Message]| messages.iter().find_map(|message| match message {
            Message::ScopedTranscript { action: TranscriptAction::Scrolled { viewport, .. }, .. } => {
                let overflow = viewport.content_bounds().height - viewport.bounds().height;
                Some(overflow <= 0.0 || viewport.absolute_offset().y <= overflow * 0.02)
            }
            _ => None,
        }).expect("actual scoped viewport callback");
        let old_bottom = bottom(&queued);
        s.chats[0].anchored_bottom = !old_bottom;
        for event in queued { let _ = s.update(event); }
        assert_eq!(s.chats[0].anchored_bottom, !old_bottom, "{transition}: old geometry changed the new view");
        let current = transcript_wheel_events(&s);
        let current_bottom = bottom(&current);
        s.chats[0].anchored_bottom = !current_bottom;
        for event in current { let _ = s.update(event); }
        assert_eq!(s.chats[0].anchored_bottom, current_bottom, "{transition}: current input was ignored");
    }
}

#[test]
fn a_search_slice_containing_the_tail_is_not_proof_the_tail_is_visible() {
    for rooms in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        s.chats[0].messages.clear();
        s.body_editors.clear();
        let conv = if rooms { "room@example" } else { "alice@example" };
        let h = History::open_in_memory();
        let now = chrono::Utc::now();
        for i in 0..10 { h.insert(conv, "alice", &format!("row {i}"), now + chrono::Duration::seconds(i), false, None).unwrap(); }
        let first = h.load_tail(conv, 10).unwrap()[0].id;
        s.history = Some(h);
        if rooms {
            let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: conv.into(), own_nick: "me".into(), members: Vec::new() });
            let _ = s.update(Message::SelectRoom(conv.into()));
        }
        s.find = Some(crate::app::FindState {
            matches: vec![crate::app::FindMatch { conversation: conv.into(), id: first, received: chrono::DateTime::UNIX_EPOCH }], ..Default::default()
        });
        let _ = s.update(Message::FindNext);
        assert!(!s.active_backlog().unwrap().anchored_bottom());
        assert!(if rooms { s.rooms[0].at_live_tail } else { s.chats[0].at_live_tail });
        let event = if rooms { xmpp::XmppEvent::RoomMessage {
            room: conv.into(), nick: "alice".into(), body: "unseen latest".into(), timestamp: now + chrono::Duration::minutes(1), delayed: false, stanza_id: None, origin_id: None,
        } } else { xmpp::XmppEvent::DirectMessage {
            from: conv.into(), body: "unseen latest".into(), timestamp: now + chrono::Duration::minutes(1), delayed: false, stanza_id: None, origin_id: None,
        } };
        let _ = s.update(Message::XmppEvent(event));
        let _ = s.update(Message::MessagesScrolled(scroll_viewport(1000.0, 0.5)));
        let _ = s.update(Message::WindowFocused);
        assert!(if rooms { s.rooms[0].unread } else { s.chats[0].unread });
        assert_eq!(s.active_backlog().unwrap().messages().len(), 10);
        let _ = s.update(Message::MessagesScrolled(scroll_viewport(1000.0, 0.0)));
        assert!(!if rooms { s.rooms[0].unread } else { s.chats[0].unread });
        assert_eq!(s.active_backlog().unwrap().messages().len(), 11);
    }
}

#[test]
fn pending_search_scroll_uses_the_current_widget_and_yields_to_user_scrolling() {
    use iced::advanced::widget::operation::{Operation, Outcome, black_box};
    let mut s = state();
    s.state = AppState::Connected;
    s.chats[0].messages.clear();
    s.body_editors.clear();
    let h = History::open_in_memory();
    let now = chrono::Utc::now();
    for i in 0..20 { h.insert("alice@example", "alice", &"long message ".repeat(20), now + chrono::Duration::seconds(i), false, None).unwrap(); }
    let id = h.load_tail("alice@example", 20).unwrap()[0].id;
    s.history = Some(h);
    s.find = Some(crate::app::FindState {
        matches: vec![crate::app::FindMatch { conversation: "alice@example".into(), id, received: chrono::DateTime::UNIX_EPOCH }], ..Default::default()
    });
    let _ = s.update(Message::FindNext);
    let token = s.find.as_ref().unwrap().pending_scroll.unwrap();
    let (messages, report) = {
        let mut widget = crate::ui::chat::view(&s);
        let mut tree = Tree::new(&widget);
        let r = renderer();
        let size = iced::Size::new(600.0, 400.0);
        let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
        let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
        let mut messages = Vec::new();
        {
            let mut shell = iced::advanced::Shell::new(&mut messages);
            widget.as_widget_mut().update(&mut tree,
                &iced::Event::Window(iced::window::Event::RedrawRequested(iced::time::Instant::now())),
                Layout::new(&node), iced::mouse::Cursor::Unavailable, &r,
                &mut iced::advanced::clipboard::Null, &mut shell, &bounds);
        }
        assert!(!messages.iter().any(|message| matches!(message, Message::ScopedTranscript { action: crate::message::TranscriptAction::Scrolled { .. }, .. })), "initial layout must not cancel the jump");
        let mut stale = crate::ui::search_scroll::SearchScroll::new(uuid::Uuid::new_v4());
        widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut black_box(&mut stale));
        assert!(matches!(stale.finish(), Outcome::Some(Message::SearchViewport { at_bottom: false, .. })), "a missing marker must finish without scrolling or chaining");
        let mut operation: Box<dyn Operation<Message>> = Box::new(crate::ui::search_scroll::SearchScroll::new(token));
        let report = loop {
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut black_box(&mut *operation));
            match operation.finish() {
                Outcome::Chain(next) => operation = next,
                Outcome::Some(report) => break report,
                Outcome::None => panic!("current search marker must be present in the real chat view"),
            }
        };
        assert!(matches!(report, Message::SearchViewport { at_bottom: false, .. }));
        {
            let mut shell = iced::advanced::Shell::new(&mut messages);
            widget.as_widget_mut().update(&mut tree,
                &iced::Event::Mouse(iced::mouse::Event::WheelScrolled { delta: iced::mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 } }),
                Layout::new(&node), iced::mouse::Cursor::Available(iced::Point::new(300.0, 150.0)), &r,
                &mut iced::advanced::clipboard::Null, &mut shell, &bounds);
        }
        assert!(messages.iter().any(|message| matches!(message, Message::ScopedTranscript { action: crate::message::TranscriptAction::Scrolled { .. }, .. })), "real input must remain authoritative");
        (messages, report)
    };
    for message in messages { let _ = s.update(message); }
    assert!(s.find.as_ref().unwrap().pending_scroll.is_none());
    let _ = s.update(Message::SearchViewport { token, at_bottom: true });
    assert!(!s.active_backlog().unwrap().anchored_bottom(), "late reports must not override the user");
    let _ = s.update(Message::FindNext);
    let newer = s.find.as_ref().unwrap().pending_scroll.unwrap();
    assert_ne!(newer, token);
    let _ = s.update(report);
    assert_eq!(s.find.as_ref().unwrap().pending_scroll, Some(newer));
}

#[test]
fn hiding_the_transcript_during_a_search_does_not_leave_a_stuck_request() {
    use iced::advanced::widget::operation::{Operation, Outcome, black_box};
    let mut s = state();
    s.state = AppState::Connected;
    let h = History::open_in_memory();
    h.insert("alice@example", "alice", "hit", chrono::Utc::now(), false, None).unwrap();
    let id = h.load_tail("alice@example", 1).unwrap()[0].id;
    s.chats[0].messages.clear();
    s.body_editors.clear();
    s.history = Some(h);
    s.find = Some(crate::app::FindState {
        matches: vec![crate::app::FindMatch { conversation: "alice@example".into(), id, received: chrono::DateTime::UNIX_EPOCH }], ..Default::default()
    });
    let _ = s.update(Message::FindNext);
    let old = s.find.as_ref().unwrap().pending_scroll.unwrap();
    let _ = s.update(Message::ShowJoinPanel);
    let report = {
        let mut widget = crate::ui::chat::view(&s);
        let mut tree = Tree::new(&widget);
        let r = renderer();
        let size = iced::Size::new(600.0, 400.0);
        let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
        let mut operation = crate::ui::search_scroll::SearchScroll::new(old);
        widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut black_box(&mut operation));
        match operation.finish() { Outcome::Some(report) => report, _ => panic!("absent transcript must finish its request") }
    };
    let _ = s.update(report);
    assert!(s.find.as_ref().unwrap().pending_scroll.is_none());
    let _ = s.update(Message::HideJoinPanel);
    let new = s.find.as_ref().unwrap().pending_scroll.unwrap();
    assert_ne!(new, old);
    let (_, report) = search_scroll_layout(20.0, 1.0, new);
    let _ = s.update(report);
    assert!(s.active_backlog().unwrap().anchored_bottom());
}

#[test]
fn a_fitting_detached_search_window_does_not_jump_away_from_the_hit() {
    let mut s = state();
    s.chats[0].messages.clear();
    s.body_editors.clear();
    let h = History::open_in_memory();
    let now = chrono::Utc::now();
    for i in 0..(backlog::JUMP_CONTEXT + 2) {
        h.insert("alice@example", "alice", &format!("row {i}"), now + chrono::Duration::seconds(i as i64), false, None).unwrap();
    }
    let id = h.load_tail("alice@example", backlog::JUMP_CONTEXT + 2).unwrap()[0].id;
    s.history = Some(h);
    s.find = Some(crate::app::FindState {
        matches: vec![crate::app::FindMatch { conversation: "alice@example".into(), id, received: chrono::DateTime::UNIX_EPOCH }], ..Default::default()
    });
    let _ = s.update(Message::FindNext);
    assert!(!s.chats[0].at_live_tail);
    let before = s.chats[0].messages.len();
    let token = s.find.as_ref().unwrap().pending_scroll.unwrap();
    let (_, report) = search_scroll_layout(20.0, 1.0, token);
    let _ = s.update(report);
    assert!(!s.chats[0].at_live_tail);
    assert_eq!(s.chats[0].messages.len(), before);
    assert!(!s.chats[0].anchored_bottom);
}

#[test]
fn non_scrolling_search_viewport_reports_the_visible_bottom_without_a_callback() {
    for height in [20.0, 100.0] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        s.chats[0].messages.clear();
        s.body_editors.clear();
        let h = History::open_in_memory();
        h.insert("alice@example", "alice", "search hit", chrono::Utc::now(), false, None).unwrap();
        let id = h.load_tail("alice@example", 1).unwrap()[0].id;
        s.history = Some(h);
        s.find = Some(crate::app::FindState {
            matches: vec![crate::app::FindMatch { conversation: "alice@example".into(), id, received: chrono::DateTime::UNIX_EPOCH }], ..Default::default()
        });
        let _ = s.update(Message::FindNext);
        assert!(!s.active_backlog().unwrap().anchored_bottom());
        let token = s.find.as_ref().unwrap().pending_scroll.unwrap();
        let (callback, report) = search_scroll_layout(height, 1.0, token);
        assert!(callback.is_none(), "Iced omits the callback when content fits");
        let _ = s.update(report);
        assert!(s.active_backlog().unwrap().anchored_bottom());
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage {
            from: "alice@example".into(), body: "visible latest".into(), timestamp: chrono::Utc::now(), delayed: false, stanza_id: None, origin_id: None,
        }));
        assert!(!s.chats[0].unread);
        assert_eq!(s.chats[0].messages.len(), 2);
    }
}

#[test]
fn messages_hidden_by_join_screens_stay_unread_until_the_transcript_returns() {
    for rooms in [false, true] {
        for screen in ["form", "joining", "error"] {
            let mut s = state();
            s.state = AppState::Connected;
            s.connected_jid = Some("me@example/device".into());
            s.chats[0].messages.clear();
            s.body_editors.clear();
            let conv = if rooms { "room@example" } else { "alice@example" };
            let h = History::open_in_memory();
            let now = chrono::Utc::now();
            h.insert(conv, "alice", "already read", now, false, None).unwrap();
            s.history = Some(h);
            if rooms {
                let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: conv.into(), own_nick: "me".into(), members: Vec::new() });
                let _ = s.update(Message::SelectRoom(conv.into()));
            } else { seed_tail_for(&s.history, conv, &mut s.chats[0]); }
            // A previous unread divider can remain after the messages were read.
            let row = s.history.as_ref().unwrap().load_tail(conv, 1).unwrap().remove(0);
            s.active_backlog_mut().unwrap().note_unread(row.id, row.received);
            let _ = s.update(Message::ShowJoinPanel);
            if screen == "joining" { s.show_join_panel = false; s.joining_room = Some("other-room@example".into()); }
            if screen == "error" { s.show_join_panel = false; s.join_error = Some("join failed".into()); }
            let event = if rooms { xmpp::XmppEvent::RoomMessage {
                room: conv.into(), nick: "alice".into(), body: "hidden arrival".into(), timestamp: now + chrono::Duration::seconds(1), delayed: false, stanza_id: None, origin_id: None,
            } } else { xmpp::XmppEvent::DirectMessage {
                from: conv.into(), body: "hidden arrival".into(), timestamp: now + chrono::Duration::seconds(1), delayed: false, stanza_id: None, origin_id: None,
            } };
            let task = s.update(Message::XmppEvent(event));
            assert!(if rooms { s.rooms[0].unread } else { s.chats[0].unread }, "{screen}");
            assert_eq!(task.units(), 0, "hidden transcript must not issue scroll commands");
            let _ = s.update(Message::WindowFocused);
            let _ = s.update(Message::RetryHistoryWrites);
            assert!(if rooms { s.rooms[0].unread } else { s.chats[0].unread });
            let latest = s.history.as_ref().unwrap().load_tail(conv, 1).unwrap()[0].id;
            assert_eq!(s.active_backlog().unwrap().read_boundary().unwrap().id, Some(latest));
            if screen == "joining" {
                let _ = s.update(Message::HideJoinPanel);
                assert!(if rooms { s.rooms[0].unread } else { s.chats[0].unread }, "ongoing entry still hides transcript");
                s.joining_room = None;
            }
            let _ = s.update(Message::HideJoinPanel);
            assert!(!if rooms { s.rooms[0].unread } else { s.chats[0].unread });
            assert_eq!(s.active_backlog().unwrap().messages().len(), 2);
        }
    }
}

#[test]
fn closing_the_join_panel_only_reads_a_visible_live_tail() {
    for rooms in [false, true] {
        for detached in [false, true] {
            let mut s = state();
            s.state = AppState::Connected;
            s.connected_jid = Some("me@example/device".into());
            s.chats[0].messages.clear();
            s.body_editors.clear();
            s.history = Some(History::open_in_memory());
            let conv = if rooms { "room@example" } else { "alice@example" };
            if rooms {
                let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: conv.into(), own_nick: "me".into(), members: Vec::new() });
                let _ = s.update(Message::SelectRoom(conv.into()));
            }
            let _ = s.update(Message::ShowJoinPanel);
            if detached {
                s.active_backlog_mut().unwrap().set_at_live_tail(false);
                s.active_backlog_mut().unwrap().set_anchored_bottom(false);
            } else { let _ = s.update(Message::WindowUnfocused); }
            let event = if rooms { xmpp::XmppEvent::RoomMessage {
                room: conv.into(), nick: "alice".into(), body: "unseen arrival".into(), timestamp: chrono::Utc::now(), delayed: false, stanza_id: None, origin_id: None,
            } } else { xmpp::XmppEvent::DirectMessage {
                from: conv.into(), body: "unseen arrival".into(), timestamp: chrono::Utc::now(), delayed: false, stanza_id: None, origin_id: None,
            } };
            let _ = s.update(Message::XmppEvent(event));
            let _ = s.update(Message::HideJoinPanel);
            assert!(if rooms { s.rooms[0].unread } else { s.chats[0].unread });
            if detached {
                let _ = s.update(if rooms { Message::SelectRoom(conv.into()) } else { Message::SelectChat(conv.into()) });
            } else { let _ = s.update(Message::WindowFocused); }
            assert!(!if rooms { s.rooms[0].unread } else { s.chats[0].unread });
            assert_eq!(s.active_backlog().unwrap().messages().len(), 1);
        }
    }
}

#[test]
fn messages_outside_the_visible_tail_stay_unread_until_the_tail_is_loaded() {
    for rooms in [false, true] {
        for was_live in [false, true] {
            let mut s = state();
            s.window_focused = true;
            s.connected_jid = Some("me@example/resource".into());
            let conv = if rooms { "room@example" } else { "alice@example" };
            let h = History::open_in_memory();
            let now = chrono::Utc::now();
            h.insert(conv, "alice", "old", now - chrono::Duration::seconds(60), false, None).unwrap();
            s.history = Some(h);
            let select = || if rooms { Message::SelectRoom(conv.into()) } else { Message::SelectChat(conv.into()) };
            if rooms {
                let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { own_nick: "me".into(), room: conv.into(), members: Vec::new() });
                let _ = s.update(select());
                s.rooms[0].at_live_tail = was_live;
                s.rooms[0].anchored_bottom = false;
            } else {
                seed_tail_for(&s.history, conv, &mut s.chats[0]);
                s.chats[0].at_live_tail = was_live;
                s.chats[0].anchored_bottom = false;
            }
            let incoming = if rooms {
                xmpp::XmppEvent::RoomMessage { room: conv.into(), nick: "alice".into(), body: "new".into(), timestamp: now, delayed: false, stanza_id: Some("new-id".into()), origin_id: None }
            } else {
                xmpp::XmppEvent::DirectMessage { from: "alice@example/phone".into(), body: "new".into(), timestamp: now, delayed: false, stanza_id: Some("new-id".into()), origin_id: None }
            };
            let _ = s.update(Message::XmppEvent(incoming));
            assert_eq!(s.active_backlog().unwrap().messages().len(), 1);
            let _ = s.update(Message::WindowFocused);
            assert!(if rooms { s.rooms[0].unread } else { s.chats[0].unread });
            s.history.as_ref().unwrap().disable_reads();
            let _ = s.update(select());
            assert!(if rooms { s.rooms[0].unread } else { s.chats[0].unread });
            assert_eq!(s.active_backlog().unwrap().messages().len(), 1);
            s.history.as_ref().unwrap().enable_reads();
            let _ = s.update(select());
            assert_eq!(s.active_backlog().unwrap().messages().len(), 2);
            assert!(!if rooms { s.rooms[0].unread } else { s.chats[0].unread });
        }
    }
}

#[test]
fn an_own_room_echo_outside_the_window_does_not_create_unread_messages() {
    let mut s = state();
    s.connected_jid = Some("me@example/resource".into());
    s.history = Some(History::open_in_memory());
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { own_nick: "me".into(), room: "room@example".into(), members: Vec::new() });
    s.rooms[0].at_live_tail = false;
    s.rooms[0].anchored_bottom = false;
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomMessage {
        room: "room@example".into(), nick: "me".into(), body: "own echo".into(),
        timestamp: chrono::Utc::now(), delayed: false, stanza_id: Some("own-id".into()),
        origin_id: None,
    }));
    assert!(!s.rooms[0].unread);
}

#[test]
fn failed_history_paging_preserves_cursors_and_does_not_exhaust_the_archive() {
    let mut s = state();
    let h = History::open_in_memory();
    h.disable_reads();
    s.history = Some(h);
    s.chats[0].has_older = true;
    s.chats[0].oldest_loaded_id = Some(1);
    s.chats[0].mam_cursor = Some("old-cursor".into());
    assert!(s.page_older_active().is_err());
    assert!(s.chats[0].has_older);
    s.mam_pending.insert("mam-b-1".into(), crate::app::ArchiveContext { conversation: "alice@example".into(), is_muc: false });
    s.chats[0].mam_in_flight = true;
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete {
        query_id: "mam-b-1".into(), complete: true,
        first: Some("new-cursor".into()), last: None,
    }));
    assert_eq!(s.chats[0].messages.len(), 3);
    assert_eq!(s.chats[0].mam_cursor.as_deref(), Some("old-cursor"));
    assert!(!s.chats[0].mam_in_flight);
    assert!(!s.chats[0].mam_exhausted);
}

#[test]
fn failed_or_missing_search_hit_does_not_switch_chats_or_replace_drafts() {
    for failed_read in [false, true] {
        let mut s = state();
        let h = History::open_in_memory();
        if failed_read { h.disable_reads(); }
        s.history = Some(h);
        s.chats.push(chat("bob@example", &["bob's log"]));
        s.message_input = text_editor::Content::with_text("keep my draft");
        s.find = Some(crate::app::FindState {
            matches: vec![crate::app::FindMatch { conversation: "bob@example".into(), id: 999, received: chrono::DateTime::UNIX_EPOCH }],
            ..Default::default()
        });
        let _ = s.jump_to_current_match();
        assert!(matches!(s.active, Some(Selection::Chat(0))));
        assert_eq!(s.message_input.text(), "keep my draft");
        assert_eq!(s.chats[0].messages.len(), 3);
        assert_eq!(s.chats[1].messages.len(), 1);
    }
}

#[test]
fn closed_search_destination_preserves_selection_and_draft_undo() {
    let mut s = state();
    let history = History::open_in_memory();
    history.insert("closed@example", "closed", "search hit", chrono::Utc::now(), false, None).unwrap();
    let id = history.load_tail("closed@example", 1).unwrap()[0].id;
    s.history = Some(history);
    let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Insert('x'))));
    let _ = s.update(Message::SelectMessage { id: 2 });
    s.find = Some(crate::app::FindState {
        matches: vec![crate::app::FindMatch { conversation: "closed@example".into(), id, received: chrono::DateTime::UNIX_EPOCH }],
        ..Default::default()
    });
    let _ = s.update(Message::FindNext);
    assert!(matches!(s.active, Some(Selection::Chat(0))));
    assert_eq!(s.message_input.text(), "x");
    assert_eq!(s.selected_msgs, [2].into_iter().collect());
    assert_eq!(s.select_anchor, Some(2));
    let _ = s.update(Message::UndoInput);
    assert_eq!(s.message_input.text(), "");
}

#[test]
fn search_switch_persists_the_departing_draft_and_read_boundary() {
    let mut s = state();
    s.chats[0].messages.clear();
    s.body_editors.clear();
    s.chats.push(chat("bob@example", &[]));
    let history = History::open_in_memory();
    history.insert("alice@example", "alice", "read message", chrono::Utc::now(), false, None).unwrap();
    history.insert("bob@example", "bob", "search hit", chrono::Utc::now(), false, None).unwrap();
    let id = history.load_tail("bob@example", 1).unwrap()[0].id;
    s.history = Some(history);
    seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
    s.sync_body_editors();
    let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Insert('x'))));
    s.find = Some(crate::app::FindState {
        matches: vec![crate::app::FindMatch { conversation: "bob@example".into(), id, received: chrono::DateTime::UNIX_EPOCH }],
        ..Default::default()
    });
    let _ = s.update(Message::FindNext);
    assert!(matches!(s.active, Some(Selection::Chat(1))));
    assert_eq!(s.message_input.text(), "");
    assert_eq!(storage::load().drafts.get("alice@example").map(String::as_str), Some("x"));
    assert!(s.chats[0].read_boundary.is_some());
    let _ = s.update(Message::SelectChat("alice@example".into()));
    assert_eq!(s.message_input.text(), "x");
    let _ = s.update(Message::UndoInput);
    assert_eq!(s.message_input.text(), "");
}

#[test]
fn failed_find_search_reports_an_error_and_recovers_with_the_same_query() {
    let mut s = state();
    let h = History::open_in_memory();
    h.insert("alice@example", "alice", "needle", chrono::Utc::now(), false, None).unwrap();
    h.disable_reads();
    s.history = Some(h);
    s.find = Some(Default::default());
    let _ = s.update(Message::FindInputChanged("needle".into()));
    assert_eq!(s.find.as_ref().unwrap().query, "needle");
    assert!(s.find.as_ref().unwrap().error.is_some());
    assert!(s.find.as_ref().unwrap().matches.is_empty());
    assert_eq!(s.chats[0].messages.len(), 3);
    s.history.as_ref().unwrap().enable_reads();
    // Retry uses the same input message as editing or submitting the query.
    let _ = s.update(Message::FindInputChanged("needle".into()));
    assert!(s.find.as_ref().unwrap().error.is_none());
    assert_eq!(s.find.as_ref().unwrap().matches.len(), 1);
    assert_eq!(s.chats[0].messages.len(), 1);
    let _ = s.update(Message::FindInputChanged("absent".into()));
    assert!(s.find.as_ref().unwrap().error.is_none());
    assert!(s.find.as_ref().unwrap().matches.is_empty());
}

#[test]
fn find_without_local_history_does_not_claim_there_are_no_matches() {
    let mut s = state();
    s.find = Some(Default::default());
    let _ = s.update(Message::FindInputChanged("alpha".into()));
    assert!(s.find.as_ref().unwrap().error.is_some());
    let _ = s.update(Message::FindInputChanged("".into()));
    assert!(s.find.as_ref().unwrap().error.is_none());
}

#[test]
fn all_room_search_is_not_crowded_out_by_matches_in_closed_conversations() {
    let mut s = state();
    let h = History::open_in_memory();
    let timestamp = chrono::Utc::now();
    let open_id = h.insert("alice@example", "alice", "needle", timestamp, false, None).unwrap().unwrap();
    for i in 1..=2001 {
        h.insert("closed@example", "closed", "needle", timestamp + chrono::Duration::seconds(i), false, None).unwrap();
    }
    s.history = Some(h);
    s.find = Some(crate::app::FindState { query: "needle".into(), all_scope: true, ..Default::default() });
    s.run_find_search();
    let find = s.find.as_ref().unwrap();
    assert!(find.error.is_none());
    assert_eq!(find.matches.len(), 1);
    assert_eq!(find.matches[0].id, open_id);
    assert_eq!(find.matches[0].conversation, "alice@example");
}
fn renderer() -> iced::Renderer {
    iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(
        iced::Font::DEFAULT,
        iced::Pixels(14.0),
    ))
}
fn shape(s: &Snack) {
    let r = renderer();
    for c in s.body_editors.values() {
        let mut widget: iced::Element<'_, Message> =
            iced::widget::text_editor(c).size(14).padding(0).into();
        let mut tree = Tree::new(&widget);
        widget.as_widget_mut().layout(
            &mut tree,
            &r,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(400.0, 100.0)),
        );
    }
}
fn position(column: usize) -> text_editor::Position {
    text_editor::Position { line: 0, column }
}
fn anchor(s: &mut Snack, id: i64, column: usize) {
    let cursor = text_editor::Cursor {
        position: position(column),
        selection: None,
    };
    s.body_editors.get_mut(&id).unwrap().move_to(cursor);
    s.body_drag = Some((id, cursor));
}
#[test]
fn drag_selects_partial_endpoints_and_intermediate_messages_both_directions() {
    let mut s = state();
    shape(&s);
    anchor(&mut s, 1, 2);
    s.extend_body_selection(3, iced::Point::new(0.0, 0.0));
    assert_eq!(s.body_editors[&1].selection().as_deref(), Some("pha"));
    assert_eq!(s.body_editors[&2].selection().as_deref(), Some("bravo"));
    assert_eq!(s.body_editors[&3].selection().unwrap_or_default(), "");
    anchor(&mut s, 3, 3);
    s.extend_body_selection(1, iced::Point::new(399.0, 0.0));
    assert_eq!(s.body_editors[&1].selection().unwrap_or_default(), "");
    assert_eq!(s.body_editors[&2].selection().as_deref(), Some("bravo"));
    assert_eq!(s.body_editors[&3].selection().as_deref(), Some("cha"));
    // Moving back inside the anchor clears the previously selected other rows.
    s.extend_body_selection(3, iced::Point::new(0.0, 0.0));
    assert_eq!(s.body_editors[&2].selection(), None);
    assert_eq!(s.body_editors[&3].selection().as_deref(), Some("cha"));
}
#[test]
fn dragging_publishes_across_captured_events_and_selected_rows_paint() {
    let mut s = state();
    shape(&s);
    anchor(&mut s, 1, 2);
    s.extend_body_selection(2, iced::Point::new(399.0, 0.0));
    let mut widget = crate::ui::selection_area::selection_area(
        iced::widget::text_editor(&s.body_editors[&2])
            .on_action(|action| Message::BodyEditorAction { id: 2, action })
            .into(),
        2,
        true,
        true,
    );
    let mut tree = Tree::new(&widget);
    let mut r = renderer();
    let node = widget.as_widget_mut().layout(
        &mut tree,
        &r,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(400.0, 100.0)),
    );
    let bounds = iced::Rectangle::new(iced::Point::ORIGIN, iced::Size::new(400.0, 100.0));
    let mut messages = Vec::new();
    let mut shell = iced::advanced::Shell::new(&mut messages);
    shell.capture_event();
    let point = iced::Point::new(10.0, 5.0);
    widget.as_widget_mut().update(
        &mut tree,
        &iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: point }),
        Layout::new(&node),
        iced::mouse::Cursor::Available(point),
        &r,
        &mut iced::advanced::clipboard::Null,
        &mut shell,
        &bounds,
    );
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::BodyDrag { id: 2, .. }))
    );
    widget.as_widget().draw(
        &tree,
        &mut r,
        &iced::Theme::Dark,
        &iced::advanced::renderer::Style::default(),
        Layout::new(&node),
        iced::mouse::Cursor::Unavailable,
        &bounds,
    );
}
#[test]
fn undo_is_per_conversation_and_does_not_undo_sent_messages() {
    let mut s = state();
    s.history = Some(History::open_in_memory());
    s.chats.push(chat("bob@example", &[]));
    for ch in "hello".chars() {
        let _ = s.update(Message::InputAction(text_editor::Action::Edit(
            text_editor::Edit::Insert(ch),
        )));
    }
    let _ = s.update(Message::SelectChat("bob@example".into()));
    let _ = s.update(Message::UndoInput);
    assert_eq!(s.message_input.text(), "");
    let _ = s.update(Message::SelectChat("alice@example".into()));
    assert_eq!(s.message_input.text(), "hello");
    let _ = s.update(Message::UndoInput);
    assert_eq!(s.message_input.text(), "");
    let _ = s.update(Message::RedoInput);
    assert_eq!(s.message_input.text(), "hello");
    let _ = s.update(Message::SendMessage);
    let _ = s.update(Message::UndoInput);
    assert_eq!(s.message_input.text(), "");
}

#[test]
fn queued_conversation_selections_follow_identity_after_an_entry_is_removed() {
    for rooms in [false, true] {
        let mut s = state();
        let names = ["alice@example", "bob@example", "charlie@example"];
        let select = |jid: &str| if rooms { Message::SelectRoom(jid.into()) } else { Message::SelectChat(jid.into()) };
        if rooms {
            for jid in names {
                let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { own_nick: "me".into(), room: jid.into(), members: Vec::new() });
            }
            let _ = s.update(select(names[0]));
        } else {
            s.chats.push(chat(names[1], &[]));
            s.chats.push(chat(names[2], &[]));
        }
        // These actions were created while the entries still had indices 0, 1,
        // and 2. Removing the first entry shifts both surviving indices.
        let removed = select(names[0]);
        let second = select(names[1]);
        let third = select(names[2]);
        let _ = s.update(if rooms { Message::LeaveRoom } else { Message::CloseChat });
        let _ = s.update(third);
        assert_eq!(s.active_conv_jid().as_deref(), Some(names[2]));
        let _ = s.update(second);
        assert_eq!(s.active_conv_jid().as_deref(), Some(names[1]));
        s.message_input = text_editor::Content::with_text("keep current draft");
        let _ = s.update(removed);
        assert_eq!(s.active_conv_jid().as_deref(), Some(names[1]));
        assert_eq!(s.message_input.text(), "keep current draft");
    }
}
#[test]
fn missing_history_assigns_unique_message_ids() {
    let mut s = state();
    let a = s
        .persist_message("x", "a", "one", chrono::Utc::now(), false, None)
        .unwrap();
    let b = s
        .persist_message("x", "a", "two", chrono::Utc::now(), false, None)
        .unwrap();
    assert_ne!(a, b);
}
#[test]
fn failed_history_writes_keep_live_messages_while_scrolled_up() {
    let mut s = state();
    let h = History::open_in_memory();
    h.disable_writes();
    s.history = Some(h);
    s.window_focused = true;
    s.chats[0].anchored_bottom = false;
    for body in ["one", "two"] {
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage {
            from: "alice@example/resource".into(),
            body: body.into(),
            timestamp: chrono::Utc::now(),
            delayed: false,
            stanza_id: None,
            origin_id: None,
        }));
    }
    let messages = &s.chats[0].messages;
    assert_eq!(messages.len(), 5);
    let ids: Vec<_> = messages[3..].iter().map(|message| match message {
        room::message::Message::Chat { id, .. } => *id,
        _ => panic!("expected retained chat"),
    }).collect();
    assert!(ids.iter().all(|id| *id < 0));
    assert_ne!(ids[0], ids[1]);
    assert!(!s.chats[0].anchored_bottom);
}

#[test]
fn closing_a_chat_activates_the_fallback_tail_and_its_own_draft() {
    for (room_target, failed_read) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut s = state();
        s.state = AppState::Connected;
        s.chats[0].messages.clear();
        s.body_editors.clear();
        let target = if room_target { "room@example" } else { "bob@example" };
        let history = History::open_in_memory();
        history.insert(target, "friend", "older message", chrono::Utc::now() - chrono::Duration::hours(1), false, None).unwrap();
        s.history = Some(history);
        if room_target {
            let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined {
                room: target.into(), own_nick: "me".into(), members: Vec::new(),
            });
            let _ = s.update(Message::SelectRoom(target.into()));
        } else {
            s.chats.push(chat(target, &[]));
            let _ = s.update(Message::SelectChat(target.into()));
        }
        let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Insert('x'))));
        let _ = s.update(Message::SelectChat("alice@example".into()));
        if room_target {
            s.rooms[0].at_live_tail = false;
            s.rooms[0].anchored_bottom = false;
        } else {
            s.chats[1].at_live_tail = false;
            s.chats[1].anchored_bottom = false;
        }
        let event = if room_target { xmpp::XmppEvent::RoomMessage {
            room: target.into(), nick: "friend".into(), body: "latest message".into(), timestamp: chrono::Utc::now(),
            delayed: false, stanza_id: None, origin_id: None,
        } } else { xmpp::XmppEvent::DirectMessage {
            from: target.into(), body: "latest message".into(), timestamp: chrono::Utc::now(),
            delayed: false, stanza_id: None, origin_id: None,
        } };
        let _ = s.update(Message::XmppEvent(event));
        assert!(if room_target { s.rooms[0].unread } else { s.chats[1].unread });
        s.find = Some(Default::default());
        if failed_read { s.history.as_ref().unwrap().disable_reads(); }
        let _ = s.update(Message::CloseChat);
        assert_eq!(s.active_conv_jid().as_deref(), Some(target));
        assert!(s.find.is_none());
        let backlog = s.active_backlog().unwrap();
        assert_eq!(backlog.messages().iter().any(|message| matches!(message, room::message::Message::Chat { body, .. } if body == "latest message")), !failed_read);
        let (at_tail, unread) = if room_target { (s.rooms[0].at_live_tail, s.rooms[0].unread) } else { (s.chats[0].at_live_tail, s.chats[0].unread) };
        assert_eq!((at_tail, unread), (!failed_read, failed_read));
        assert_eq!(s.message_input.text(), "x");
        let _ = s.update(Message::UndoInput);
        assert_eq!(s.message_input.text(), "");
    }
}

#[test]
fn closing_the_last_conversation_discards_its_undo_history() {
    for undone in [false, true] {
        let mut s = state();
        let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Paste(std::sync::Arc::new("private draft".into())))));
        let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Insert('x'))));
        if undone { let _ = s.update(Message::UndoInput); }
        let _ = s.update(Message::CloseChat);
        assert!(s.active.is_none());
        assert_eq!(s.message_input.text(), "");
        let _ = s.update(Message::UndoInput);
        assert_eq!(s.message_input.text(), "");
        let _ = s.update(Message::RedoInput);
        assert_eq!(s.message_input.text(), "");
    }
}

#[test]
fn search_across_conversations_preserves_both_drafts() {
    let mut s = state();
    s.chats.push(chat("bob@example", &[]));
    s.saved_config
        .drafts
        .insert("bob@example".into(), "bob draft".into());
    s.message_input = text_editor::Content::with_text("alice draft");
    s.find = Some(crate::app::FindState {
        matches: vec![crate::app::FindMatch {
            conversation: "bob@example".into(),
            id: 1,
            received: chrono::DateTime::UNIX_EPOCH,
        }],
        ..Default::default()
    });
    let _ = s.jump_to_current_match();
    assert_eq!(s.saved_config.drafts["alice@example"], "alice draft");
    assert_eq!(s.message_input.text(), "bob draft");
}

#[test]
fn unavailable_outbox_keeps_unsent_input() {
    let mut s = state();
    s.message_input = text_editor::Content::with_text("keep me");
    let _ = s.update(Message::SendMessage);
    assert_eq!(s.message_input.text(), "keep me");
}
#[test]
fn catchup_checkpoint_limits_restarts_to_the_missing_server_time_range() {
    use chrono::TimeZone;
    let old = chrono::Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let newer = old + chrono::Duration::hours(1);
    let path = storage::history_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let h = History::open(&path).unwrap();
    h.insert("alice@example", "alice", "old server message", old, true, Some("old")).unwrap();
    h.insert("alice@example", "alice", "skewed local receipt", old + chrono::Duration::days(365), false, Some("live")).unwrap();
    // Model an existing installation before the checkpoint schema existed.
    rusqlite::Connection::open(&path).unwrap().execute_batch("DROP TABLE archive_sync").unwrap();
    drop(h);
    for restart in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        s.history = Some(History::open(&path).unwrap());
        let (tx, mut rx) = tokio::sync::mpsc::channel(20);
        s.xmpp_cmd_tx = Some(tx);
        s.start_catchup("alice@example");
        let expected = if restart { newer } else { old };
        let query = match rx.try_recv().unwrap().into_inner() {
            xmpp::XmppCommand::QueryArchive { query_id, start: Some(start), cursor: None, forward: false, .. }
                if start == expected.to_rfc3339() => query_id,
            other => panic!("catch-up ignored the durable server boundary: {other:?}"),
        };
        if !restart {
            let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage { query_id: query.clone(),
                from: "alice@example/phone".into(), to: None, body: "missed while offline".into(), timestamp: newer,
                stanza_id: Some("missed".into()), origin_id: None }));
        }
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: query, complete: true, first: None, last: None }));
        assert!(rx.try_recv().is_err(), "a complete incremental check must not walk old archive pages");
        assert!(s.mam_caught_up.contains("alice@example"));
        let sync = s.history.as_ref().unwrap().archive_sync("alice@example").unwrap();
        assert_eq!(sync.received, Some(newer));
        assert!(sync.before.is_none());
        assert_eq!(s.history.as_ref().unwrap().load_tail("alice@example", 20).unwrap().len(), 3);
    }
}

#[test]
fn unfinished_catchup_resumes_pages_and_then_checks_newer_arrivals() {
    use chrono::TimeZone;
    let old = chrono::Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let head = old + chrono::Duration::hours(2);
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    let h = History::open_in_memory();
    h.insert("alice@example", "alice", "old", old, true, Some("old")).unwrap();
    h.save_archive_sync("alice@example", &storage::history::ArchiveSync { received: Some(old), ..Default::default() }).unwrap();
    s.history = Some(h);
    let (tx, mut rx) = tokio::sync::mpsc::channel(20);
    s.xmpp_cmd_tx = Some(tx);
    s.start_catchup("alice@example");
    let query = match rx.try_recv().unwrap().into_inner() { xmpp::XmppCommand::QueryArchive { query_id, .. } => query_id, _ => unreachable!() };
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage { query_id: query.clone(),
        from: "alice@example".into(), to: None, body: "gap head".into(), timestamp: head, stanza_id: Some("head".into()), origin_id: None }));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: query, complete: false, first: Some("older-gap".into()), last: None }));
    let _ = rx.try_recv().unwrap();
    let sync = s.history.as_ref().unwrap().archive_sync("alice@example").unwrap();
    assert_eq!(sync.received, Some(old), "an incomplete page must not skip the rest of the gap");
    assert_eq!(sync.before.as_deref(), Some("older-gap"));
    // A session restart loses transient query state but keeps page progress.
    s.mam_pending.clear();
    s.mam_catchup.clear();
    s.start_catchup("alice@example");
    let query = match rx.try_recv().unwrap().into_inner() {
        xmpp::XmppCommand::QueryArchive { query_id, start: Some(start), cursor: Some(cursor), .. }
            if start == old.to_rfc3339() && cursor == "older-gap" => query_id,
        other => panic!("unfinished gap restarted from the top: {other:?}"),
    };
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage { query_id: query.clone(), from: "alice@example".into(), to: None,
        body: "gap middle".into(), timestamp: old + chrono::Duration::hours(1), stanza_id: Some("middle".into()), origin_id: None }));
    let task = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: query, complete: true, first: None, last: None }));
    assert!(task.units() > 0);
    let token = s.mam_catchup["alice@example"].continuation.unwrap();
    let _ = s.update(Message::ContinueArchiveSync { context: s.room_retry_context("alice@example"), token });
    let query = match rx.try_recv().unwrap().into_inner() {
        xmpp::XmppCommand::QueryArchive { query_id, start: Some(start), cursor: None, .. } if start == head.to_rfc3339() => query_id,
        other => panic!("resumed sweep missed newer arrivals: {other:?}"),
    };
    let newest = head + chrono::Duration::hours(1);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage { query_id: query.clone(), from: "alice@example".into(), to: None,
        body: "new while resuming".into(), timestamp: newest, stanza_id: Some("newest".into()), origin_id: None }));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: query, complete: true, first: None, last: None }));
    assert!(s.mam_caught_up.contains("alice@example"));
    assert_eq!(s.history.as_ref().unwrap().archive_sync("alice@example").unwrap().received, Some(newest));
    assert_eq!(s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap().len(), 4);
    assert!(rx.try_recv().is_err());
}

#[test]
fn catchup_checkpoint_write_failure_pauses_before_advancing() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    let h = History::open_in_memory();
    let now = chrono::Utc::now();
    h.insert("alice@example", "alice", "history", now, true, Some("old")).unwrap();
    s.history = Some(h);
    let (tx, mut rx) = tokio::sync::mpsc::channel(20);
    s.xmpp_cmd_tx = Some(tx);
    s.start_catchup("alice@example");
    let query = match rx.try_recv().unwrap().into_inner() { xmpp::XmppCommand::QueryArchive { query_id, .. } => query_id, _ => unreachable!() };
    s.history.as_ref().unwrap().disable_writes();
    let task = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: query, complete: false, first: Some("next".into()), last: None }));
    assert!(task.units() > 0);
    assert!(rx.try_recv().is_err());
    assert!(s.history.as_ref().unwrap().archive_sync("alice@example").unwrap().before.is_none());
    assert!(!s.mam_caught_up.contains("alice@example"));
    let token = s.mam_catchup["alice@example"].continuation.unwrap();
    let context = s.room_retry_context("alice@example");
    let still_blocked = s.update(Message::ContinueArchiveSync { context: context.clone(), token });
    assert!(still_blocked.units() > 0);
    assert!(rx.try_recv().is_err());
    s.history.as_ref().unwrap().enable_writes();
    let token = s.mam_catchup["alice@example"].continuation.unwrap();
    let _ = s.update(Message::ContinueArchiveSync { context, token });
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::QueryArchive { cursor: Some(cursor), .. } if cursor == "next"));
    assert_eq!(s.history.as_ref().unwrap().archive_sync("alice@example").unwrap().before.as_deref(), Some("next"));
}

#[test]
fn catchup_preserves_boundary_timestamp_messages_and_rejects_older_results() {
    use chrono::TimeZone;
    for older in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        let h = History::open_in_memory();
        let boundary = chrono::Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        h.insert("alice@example", "alice", "known", boundary, true, Some("known")).unwrap();
        h.save_archive_sync("alice@example", &storage::history::ArchiveSync { received: Some(boundary), ..Default::default() }).unwrap();
        s.history = Some(h);
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        s.start_catchup("alice@example");
        let query = match rx.try_recv().unwrap().into_inner() { xmpp::XmppCommand::QueryArchive { query_id, .. } => query_id, _ => unreachable!() };
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage { query_id: query.clone(), from: "alice@example".into(), to: None,
            body: "another identity".into(), timestamp: boundary - chrono::Duration::seconds(i64::from(older)), stanza_id: Some("another".into()), origin_id: None }));
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: query, complete: true, first: None, last: None }));
        assert_eq!(s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap().len(), if older { 1 } else { 2 });
        assert_eq!(s.mam_caught_up.contains("alice@example"), !older);
        assert_eq!(s.history.as_ref().unwrap().archive_sync("alice@example").unwrap().received, Some(boundary));
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn catchup_rejected_resume_cursor_falls_back_to_its_original_time_range() {
    use chrono::TimeZone;
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    let h = History::open_in_memory();
    let old = chrono::Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    h.insert("alice@example", "alice", "known", old, true, Some("known")).unwrap();
    h.save_archive_sync("alice@example", &storage::history::ArchiveSync { received: Some(old), before: Some("expired".into()), newest: Some(old + chrono::Duration::days(1)) }).unwrap();
    s.history = Some(h);
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.start_catchup("alice@example");
    let query = match rx.try_recv().unwrap().into_inner() { xmpp::XmppCommand::QueryArchive { query_id, cursor: Some(cursor), .. } if cursor == "expired" => query_id, _ => unreachable!() };
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveFailed { query_id: query, reason: "archive query rejected by server".into() }));
    s.start_catchup("alice@example");
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::QueryArchive { start: Some(start), cursor: None, .. } if start == old.to_rfc3339()));
    let sync = s.history.as_ref().unwrap().archive_sync("alice@example").unwrap();
    assert_eq!(sync.received, Some(old));
    assert!(sync.newest.is_none());
}

#[test]
fn catchup_paused_timer_cannot_request_from_a_replaced_room_operation() {
    let mut s = completion_state(&[]);
    s.connected_jid = Some("me@example/device".into());
    let h = History::open_in_memory();
    h.insert("room@example", "alice", "known", chrono::Utc::now(), true, Some("known")).unwrap();
    s.history = Some(h);
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    let _ = s.pause_catchup("room@example", Default::default());
    let token = s.mam_catchup["room@example"].continuation.unwrap();
    let context = s.room_retry_context("room@example");
    s.room_operations.insert("room@example".into(), xmpp::RoomOperation::new());
    let _ = s.update(Message::ContinueArchiveSync { context, token });
    assert!(rx.try_recv().is_err());
    assert!(!s.mam_catchup.contains_key("room@example"));
    s.start_catchup("room@example");
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::QueryArchive { .. }));
}

#[test]
fn duplicate_archive_page_does_not_end_catchup_before_a_gap() {
    let mut s = state();
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.mam_pending
        .insert("mam-c-0".into(), crate::app::ArchiveContext { conversation: "alice@example".into(), is_muc: false });
    s.mam_seq = 1;
    s.mam_catchup.insert("alice@example".into(), Default::default());
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete {
        query_id: "mam-c-0".into(),
        complete: false,
        first: Some("older".into()),
        last: None,
    }));
    assert!(
        matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::QueryArchive { cursor: Some(c), .. } if c == "older")
    );
    assert!(!s.mam_caught_up.contains("alice@example"));
}
#[test]
fn reconnect_resets_archive_requests_and_allows_fresh_sync() {
    let mut s = state();
    s.chats[0].mam_in_flight = true;
    s.chats[0].mam_exhausted = true;
    s.mam_caught_up.insert("alice@example".into());
    s.mam_pending
        .insert("old-query".into(), crate::app::ArchiveContext { conversation: "alice@example".into(), is_muc: false });
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Reconnecting));
    assert!(s.mam_pending.is_empty());
    assert!(s.mam_caught_up.is_empty());
    assert!(!s.chats[0].mam_in_flight);
    assert!(!s.chats[0].mam_exhausted);
}

#[test]
fn failed_archive_queries_preserve_cursors_and_allow_history_retry() {
    for query in ["mam-b-1", "mam-c-1"] {
        let mut s = state();
        s.mam_pending.insert(query.into(), crate::app::ArchiveContext { conversation: "alice@example".into(), is_muc: false });
        s.mam_pending.insert("other".into(), crate::app::ArchiveContext { conversation: "bob@example".into(), is_muc: false });
        let mut progress = crate::app::CatchupProgress::default();
        progress.pages = 3;
        s.mam_catchup.insert("alice@example".into(), progress);
        s.mam_caught_up.insert("alice@example".into());
        s.chats[0].mam_in_flight = true;
        s.chats[0].mam_cursor = Some("existing-cursor".into());
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveFailed {
            query_id: query.into(), reason: "forbidden".into(),
        }));
        assert!(!s.mam_pending.contains_key(query));
        assert!(s.mam_pending.contains_key("other"));
        let catchup_failed = query.starts_with("mam-c-");
        assert_eq!(s.mam_catchup.contains_key("alice@example"), !catchup_failed);
        assert_eq!(s.mam_caught_up.contains("alice@example"), !catchup_failed);
        assert!(!s.chats[0].mam_in_flight);
        assert!(!s.chats[0].mam_exhausted);
        assert_eq!(s.chats[0].mam_cursor.as_deref(), Some("existing-cursor"));
        // A fin already queued behind an invalid archive result cannot turn a
        // failed page into success or advance past the unprocessed message.
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete {
            query_id: query.into(), complete: true, first: Some("skip-bad-message".into()), last: None,
        }));
        assert_eq!(s.chats[0].mam_cursor.as_deref(), Some("existing-cursor"));
        assert!(!s.chats[0].mam_exhausted);
        assert_eq!(s.chats[0].messages.len(), 3);
    }
}

#[test]
fn the_first_archive_page_is_visible_in_an_empty_conversation() {
    let mut s = state();
    s.history = Some(History::open_in_memory());
    s.chats[0].messages.clear();
    s.chats[0].oldest_loaded_id = None;
    s.chats[0].mam_in_flight = true;
    s.mam_pending.insert("mam-b-first".into(), crate::app::ArchiveContext { conversation: "alice@example".into(), is_muc: false });
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage { to: None,
        query_id: "mam-b-first".into(), from: "alice@example/phone".into(),
        body: "first archived message".into(), timestamp: chrono::Utc::now(), stanza_id: Some("first".into()),
        origin_id: None,
    }));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete {
        query_id: "mam-b-first".into(), complete: false, first: Some("first".into()), last: None,
    }));
    assert_eq!(s.chats[0].messages.len(), 1);
    assert!(matches!(&s.chats[0].messages[0], room::message::Message::Chat { body, .. } if body == "first archived message"));
    assert!(s.chats[0].oldest_loaded_id.is_some());
    assert_eq!(s.chats[0].mam_cursor.as_deref(), Some("first"));
    assert!(!s.chats[0].mam_in_flight);
    assert!(!s.chats[0].mam_exhausted);
}

#[test]
fn duplicate_only_backward_pages_do_not_hide_older_archive_messages() {
    let mut s = state();
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    let h = History::open_in_memory();
    h.insert("alice@example", "alice", "already loaded", chrono::Utc::now(), false, Some("existing")).unwrap();
    s.history = Some(h);
    seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
    s.chats[0].mam_in_flight = true;
    s.chats[0].mam_cursor = Some("old-cursor".into());
    s.mam_pending.insert("mam-b-1".into(), crate::app::ArchiveContext { conversation: "alice@example".into(), is_muc: false });
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete {
        query_id: "mam-b-1".into(), complete: false, first: Some("next-cursor".into()), last: None,
    }));
    assert_eq!(s.chats[0].messages.len(), 1);
    assert!(!s.chats[0].mam_exhausted);
    s.fetch_archive_active();
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::QueryArchive { cursor: Some(cursor), .. } if cursor == "next-cursor"));
}

#[test]
fn an_incomplete_archive_page_without_cursor_progress_remains_retryable() {
    for first in [None, Some(""), Some("old-cursor")] {
        let mut s = state();
        s.history = Some(History::open_in_memory());
        s.chats[0].oldest_loaded_id = Some(1);
        s.chats[0].mam_cursor = Some("old-cursor".into());
        s.chats[0].mam_in_flight = true;
        s.mam_pending.insert("mam-b-1".into(), crate::app::ArchiveContext { conversation: "alice@example".into(), is_muc: false });
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete {
            query_id: "mam-b-1".into(), complete: false,
            first: first.map(str::to_string), last: None,
        }));
        assert!(!s.chats[0].mam_in_flight);
        assert!(!s.chats[0].mam_exhausted);
        assert_eq!(s.chats[0].mam_cursor.as_deref(), Some("old-cursor"));
    }
}
#[test]
fn paging_cursor_ignores_temporary_messages_and_tracks_first_persisted_message() {
    let mut c = chat("x", &[]);
    for id in [-2, 10, 11] {
        c.append_live(room::message::Message::Chat {
            id,
            from: "x".into(),
            body: "x".into(),
            received: chrono::Utc::now(),
            status: ChatStatus::Confirmed,
        });
    }
    assert_eq!(c.oldest_loaded_id, Some(10));
}

#[test]
fn explicit_message_selection_replaces_text_selection_and_adjusts_ranges() {
    let mut s = state();
    s.body_editors
        .get_mut(&1)
        .unwrap()
        .perform(text_editor::Action::SelectAll);
    let _ = s.update(Message::SelectMessage { id: 1 });
    assert_eq!(s.body_editors[&1].selection(), None);
    assert_eq!(s.selected_msgs, [1].into_iter().collect());
    let _ = s.update(Message::ModifiersUpdate {
        shift: true,
        primary: false,
    });
    let _ = s.update(Message::SelectMessage { id: 3 });
    assert_eq!(s.selected_msgs, [1, 2, 3].into_iter().collect());
    let _ = s.update(Message::SelectMessage { id: 2 });
    assert_eq!(s.selected_msgs, [1, 2].into_iter().collect());
    let _ = s.update(Message::ModifiersUpdate {
        shift: false,
        primary: false,
    });
    let _ = s.update(Message::SelectMessage { id: 2 });
    assert_eq!(s.selected_msgs, [1].into_iter().collect());
    let _ = s.update(Message::SelectMessage { id: 999 });
    assert_eq!(s.selected_msgs, [1].into_iter().collect());
}

#[test]
fn selection_count_drops_messages_removed_from_the_window() {
    let mut s = state();
    let _ = s.update(Message::SelectMessage { id: 1 });
    s.chats[0].messages.remove(0);
    s.sync_body_editors();
    assert!(s.selected_msgs.is_empty());
    assert!(s.select_anchor.is_none());
}

#[test]
fn double_click_quotes_the_message_and_preserves_the_draft_and_undo() {
    let mut s = state();
    if let room::message::Message::Chat { body, .. } = &mut s.chats[0].messages[0] {
        *body = "first line\r\nsecond line".into();
    }
    s.message_input = text_editor::Content::with_text("my draft");
    s.message_input.move_to(text_editor::Cursor {
        position: position(2),
        selection: None,
    });
    let original_cursor = s.message_input.cursor();
    let _ = s.update(Message::BodyEditorAction {
        id: 1,
        action: text_editor::Action::SelectWord,
    });
    assert_eq!(
        s.message_input.text(),
        "my draft\n> first line\n> second line\n"
    );
    assert_eq!(
        s.message_input.cursor().position,
        text_editor::Position { line: 3, column: 0 }
    );
    assert!(s.body_drag.is_none());
    let _ = s.update(Message::UndoInput);
    assert_eq!(s.message_input.text(), "my draft");
    assert_eq!(s.message_input.cursor(), original_cursor);
    let _ = s.update(Message::RedoInput);
    assert_eq!(
        s.message_input.text(),
        "my draft\n> first line\n> second line\n"
    );
}

#[test]
fn quote_into_empty_input_ends_on_a_new_line_and_ignores_missing_messages() {
    let mut s = state();
    let _ = s.update(Message::QuoteMessage { id: 1 });
    assert_eq!(s.message_input.text(), "> alpha\n");
    let _ = s.update(Message::QuoteMessage { id: 999 });
    assert_eq!(s.message_input.text(), "> alpha\n");
}

#[test]
fn find_shortcut_refocuses_existing_search_without_losing_the_query() {
    let mut s = state();
    let _ = s.update(Message::ToggleFind);
    s.find.as_mut().unwrap().query = "keep this query".into();
    let _ = s.update(Message::ToggleFind);
    assert_eq!(s.find.as_ref().unwrap().query, "keep this query");
}

#[test]
fn real_body_double_click_dispatches_a_quoted_reply() {
    let mut s = state();
    let r = renderer();
    let mut widget = crate::ui::selection_area::selection_area(
        iced::widget::text_editor(&s.body_editors[&1])
            .size(14)
            .padding(0)
            .on_action(|action| Message::BodyEditorAction { id: 1, action })
            .into(),
        1,
        false,
        false,
    );
    let mut tree = Tree::new(&widget);
    let node = widget.as_widget_mut().layout(
        &mut tree,
        &r,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(400.0, 100.0)),
    );
    let bounds = iced::Rectangle::new(iced::Point::ORIGIN, iced::Size::new(400.0, 100.0));
    let cursor = iced::mouse::Cursor::Available(iced::Point::new(10.0, 5.0));
    let mut messages = Vec::new();
    for _ in 0..2 {
        for event in [
            iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left),
            iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left),
        ] {
            widget.as_widget_mut().update(
                &mut tree,
                &iced::Event::Mouse(event),
                Layout::new(&node),
                cursor,
                &r,
                &mut iced::advanced::clipboard::Null,
                &mut iced::advanced::Shell::new(&mut messages),
                &bounds,
            );
        }
    }
    drop(widget);
    assert!(messages.iter().any(|m| matches!(
        m,
        Message::BodyEditorAction {
            action: text_editor::Action::SelectWord,
            ..
        }
    )));
    for message in messages {
        let _ = s.update(message);
    }
    assert_eq!(s.message_input.text(), "> alpha\n");
}

#[test]
fn message_copy_includes_timestamp_and_nick_even_for_one_message() {
    let mut s = state();
    let stamp = chrono::DateTime::parse_from_rfc3339("2026-10-29T12:34:56Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    if let room::message::Message::Chat { received, from, .. } = &mut s.chats[0].messages[0] {
        *received = stamp;
        *from = "Alice".into();
    }
    let _ = s.update(Message::SelectMessage { id: 1 });
    let expected = format!(
        "[{}] Alice: alpha",
        stamp
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
    );
    assert_eq!(
        s.selected_message_text().as_deref(),
        Some(expected.as_str())
    );
    let _ = s.update(Message::SelectMessage { id: 3 });
    let _ = s.update(Message::SelectMessage { id: 2 });
    let copied = s.selected_message_text().unwrap();
    assert!(copied.starts_with(&expected));
    assert!(copied.find("bravo").unwrap() < copied.find("charlie").unwrap());
    assert_eq!(copied.lines().count(), 3);
}

#[test]
fn url_positions_map_to_the_link_under_the_caret() {
    use crate::update::url_at;
    let at = |line, column| text_editor::Position { line, column };
    let text = "see https://a.invalid/x, ok\nhttps://b.invalid é";
    assert_eq!(url_at(text, at(0, 0)), None);
    assert_eq!(url_at(text, at(0, 4)).as_deref(), Some("https://a.invalid/x,"));
    assert_eq!(url_at(text, at(0, 24)).as_deref(), Some("https://a.invalid/x,"), "the end is inclusive");
    assert_eq!(url_at(text, at(0, 26)), None);
    assert_eq!(url_at(text, at(1, 5)).as_deref(), Some("https://b.invalid"));
    assert_eq!(url_at(text, at(1, 18)), None);
    assert_eq!(url_at(text, at(2, 0)), None);
}

#[test]
fn clicking_url_text_opens_it_but_selecting_or_clicking_plain_text_does_not() {
    let click = |s: &mut Snack, id: i64, point: iced::Point| {
        let _ = s.update(Message::BodyEditorAction { id, action: text_editor::Action::Click(point) });
    };
    let setup = || {
        let mut s = Snack::test_state();
        s.chats.push(chat("alice@example", &["https://example.invalid/page", "https://example.invalid/x then plain text"]));
        s.active = Some(Selection::Chat(0));
        s.sync_body_editors();
        s
    };
    let at_start = iced::Point::new(2.0, 2.0);

    let mut s = setup();
    click(&mut s, 1, at_start);
    assert_eq!(s.take_link_click().as_deref(), Some("https://example.invalid/page"));
    assert!(s.take_link_click().is_none(), "a click opens its link once");

    // Real clicks usually move the pointer slightly, which reports a drag
    // that leaves an empty selection at the press position.
    let mut s = setup();
    click(&mut s, 1, at_start);
    let _ = s.update(Message::BodyDrag { id: 1, point: at_start });
    assert_eq!(s.take_link_click().as_deref(), Some("https://example.invalid/page"));

    let mut s = setup();
    click(&mut s, 2, at_start);
    assert!(s.take_link_click().is_none(), "plain text is not a link");

    let mut s = setup();
    click(&mut s, 1, at_start);
    let _ = s.update(Message::BodyDrag { id: 2, point: iced::Point::new(40.0, 2.0) });
    assert!(s.body_editors.values().any(|body| body.selection().is_some()));
    assert!(s.take_link_click().is_none(), "dragging a selection out of a link does not open it");

    let mut s = setup();
    let _ = s.update(Message::ModifiersUpdate { shift: false, primary: true });
    click(&mut s, 1, at_start);
    assert!(s.take_link_click().is_none(), "Cmd/Ctrl-click selects the message instead");
}

#[test]
fn cmd_click_enters_message_selection_and_copy_restores_normal_state() {
    let mut s = state();
    assert!(s.selected_msgs.is_empty());
    let _ = s.update(Message::ModifiersUpdate {
        shift: false,
        primary: true,
    });
    let _ = s.update(Message::BodyEditorAction {
        id: 2,
        action: text_editor::Action::Click(iced::Point::ORIGIN),
    });
    assert_eq!(s.selected_msgs, [2].into_iter().collect());
    assert_eq!(s.select_anchor, Some(2));
    s.message_input = text_editor::Content::with_text("unsent draft");
    let _ = s.update(Message::CopySelection);
    assert!(s.selected_msgs.is_empty());
    assert!(s.select_anchor.is_none());
    assert!(s.body_drag.is_none());
    assert!(
        s.body_editors
            .values()
            .all(|body| body.selection().is_none())
    );
    assert_eq!(s.message_input.text(), "unsent draft");
    assert!(s.selected_message_text().is_none());
}

#[test]
fn outbox_replay_survives_worker_queue_capacity_and_keeps_fifo_order() {
    let mut s = state();
    let h = History::open_in_memory();
    let mut expected = Vec::new();
    for i in 0..250 {
        expected.push(h.enqueue_outbox("alice@example", false, &i.to_string()).unwrap());
    }
    s.history = Some(h);
    let (tx, mut rx) = tokio::sync::mpsc::channel(100);
    s.xmpp_cmd_tx = Some(tx);
    s.resend_outbox(|_| true);
    assert_eq!(s.xmpp_pending_commands.len(), 150);
    for (i, id) in expected.into_iter().enumerate() {
        let command = rx.try_recv().expect("queued send must eventually reach worker").into_inner();
        assert!(matches!(command, xmpp::XmppCommand::SendDirectMessage { id: actual, body, .. }
            if actual == id && body == i.to_string()));
        let _ = s.update(Message::FlushXmppCommands);
    }
    assert!(s.xmpp_pending_commands.is_empty());
    assert!(rx.try_recv().is_err());
    // Queueing a command alone must not delete the durable retry entry.
    assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 250);
}

#[test]
fn delivered_outbox_rows_are_not_replayed_while_deletion_is_failing() {
    let mut s = state();
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.connected_jid = Some("me@example/resource".into());
    let h = History::open_in_memory();
    let id = h.enqueue_outbox("alice@example", false, "send once").unwrap();
    h.disable_writes();
    s.history = Some(h);
    s.resend_outbox(|_| true);
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::SendDirectMessage { id: actual, .. } if actual == id));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id, ok: true }));
    assert_eq!(s.pending_outbox_deletes.len(), 1);
    assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 1);
    assert!(!s.outbox_in_flight.contains(&id));
    // Reconnecting normally clears all attempts; cleanup protection must live
    // independently and remain effective for both replay and direct commands.
    s.outbox_in_flight.clear();
    s.resend_outbox(|_| true);
    s.send_xmpp_command(xmpp::XmppCommand::SendDirectMessage { id, origin_id: None, to: "alice@example".into(), body: "send once".into() });
    assert!(rx.try_recv().is_err());
    let _ = s.update(Message::RetryHistoryWrites);
    assert_eq!(s.pending_outbox_deletes.len(), 1);
    s.history.as_ref().unwrap().enable_writes();
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.pending_outbox_deletes.is_empty());
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
    s.resend_outbox(|_| true);
    assert!(rx.try_recv().is_err());
}

#[test]
fn queued_messages_reuse_their_origin_id_on_initial_send_and_retry() {
    for rooms in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        let (tx, mut rx) = tokio::sync::mpsc::channel(20);
        s.xmpp_cmd_tx = Some(tx);
        s.history = Some(History::open_in_memory());
        if rooms {
            let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { own_nick: "me".into(), room: "room@example".into(), members: Vec::new() });
            let _ = s.update(Message::SelectRoom("room@example".into()));
        }
        s.message_input = text_editor::Content::with_text("retry this message");
        let _ = s.update(Message::SendMessage);
        let (id, origin) = match rx.try_recv().unwrap().into_inner() {
            xmpp::XmppCommand::SendRoomMessage { id, origin_id: Some(origin), .. }
                | xmpp::XmppCommand::SendDirectMessage { id, origin_id: Some(origin), .. } => (id, origin),
            other => panic!("expected queued send, got {other:?}"),
        };
        assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap()[0].origin_id, origin);
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id, ok: false }));
        s.resend_outbox(|_| true);
        match next_send_after_delivery_check(&mut s, &mut rx) {
            xmpp::XmppCommand::SendRoomMessage { id: retried, origin_id: Some(retried_origin), .. }
                | xmpp::XmppCommand::SendDirectMessage { id: retried, origin_id: Some(retried_origin), .. } => {
                assert_eq!(retried, id);
                assert_eq!(retried_origin, origin);
            }
            other => panic!("expected queued retry, got {other:?}"),
        }
    }
}

#[test]
fn stale_clean_draft_buttons_cannot_modify_a_replacement_draft() {
    for change in ["chat", "edit", "hidden"] {
        let mut s = state();
        s.state = AppState::Connected;
        s.message_input = text_editor::Content::with_text("original\0 draft");
        let _ = s.update(Message::SendMessage);
        let queued = app_clicks(&s, "Remove control characters");
        assert!(!queued.is_empty());
        match change {
            "chat" => {
                s.chats.push(chat("bob@example", &[]));
                s.saved_config.drafts.insert("bob@example".into(), "bob\0 private draft".into());
                let _ = s.update(Message::SelectChat("bob@example".into()));
                let _ = s.update(Message::SendMessage);
            }
            "edit" => {
                s.message_input.perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
                let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Paste(std::sync::Arc::new(" plus new\0 text".into())))));
                let _ = s.update(Message::SendMessage);
            }
            _ => { let _ = s.update(Message::ShowJoinPanel); }
        }
        let original = s.message_input.text();
        for message in queued { assert_eq!(s.update(message).units(), 0, "{change}: stale clean button ran"); }
        assert_eq!(s.message_input.text(), original, "{change}");
        assert!(s.send_error.is_some());
        if change != "hidden" {
            let fresh = app_clicks(&s, "Remove control characters");
            assert!(!fresh.is_empty());
            for message in fresh { let _ = s.update(message); }
            assert_eq!(s.message_input.text(), ::xmpp::clean_message_text(&original));
            assert!(s.send_error.is_none());
            let _ = s.update(Message::UndoInput);
            assert_eq!(s.message_input.text(), original);
        }
    }
}

#[test]
fn held_clean_draft_press_cannot_finish_on_an_edited_draft() {
    use iced::advanced::widget::Operation;
    struct Target(Option<iced::Rectangle>);
    impl Operation for Target {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) {
            if text == "Remove control characters" { self.0 = Some(bounds); }
        }
    }
    for edited in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.message_input = text_editor::Content::with_text("original\0 draft");
        let _ = s.update(Message::SendMessage);
        let r = renderer();
        let size = iced::Size::new(800.0, 600.0);
        let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
        let mut tree;
        let point;
        {
            let mut widget = s.view();
            tree = Tree::new(&widget);
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut target = Target(None);
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut target);
            point = target.0.unwrap().center();
            let mut output = Vec::new();
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
            assert!(output.is_empty());
        }
        if edited {
            s.message_input.perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
            let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Paste(std::sync::Arc::new(" newer\0 text".into())))));
            let _ = s.update(Message::SendMessage);
        }
        let original = s.message_input.text();
        let output = {
            let mut widget = s.view();
            tree.diff(widget.as_widget());
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut output = Vec::new();
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)),
                Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
            output
        };
        for message in output { let _ = s.update(message); }
        assert_eq!(s.message_input.text(), if edited { original.clone() } else { ::xmpp::clean_message_text(&original) });
        assert_eq!(s.send_error.is_some(), edited);
        if !edited { let _ = s.update(Message::UndoInput); assert_eq!(s.message_input.text(), original); }
    }
}

#[test]
fn rejected_send_buttons_cannot_retarget_a_held_press_to_the_next_message() {
    use iced::advanced::widget::Operation;
    struct Target<'a>(&'a str, Option<iced::Rectangle>);
    impl Operation for Target<'_> {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) {
            if text == self.0 { self.1 = Some(bounds); }
        }
    }
    for label in ["Discard", "Copy message"] {
        for replace in [false, true] {
            let mut s = state();
            s.state = AppState::Connected;
            s.record_rejected_send(0, "alice@example".into(), "first unsent message".into(), "test failure".into(), true);
            s.record_rejected_send(0, "alice@example".into(), "second private unsent message".into(), "test failure".into(), true);
            let first = s.rejected_sends[0].token;
            let second = s.rejected_sends[1].token;
            let r = renderer();
            let size = iced::Size::new(800.0, 600.0);
            let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
            let mut tree;
            let point;
            {
                let mut widget = s.view();
                tree = Tree::new(&widget);
                let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
                let mut target = Target(label, None);
                widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut target);
                point = target.1.unwrap().center();
                let mut output = Vec::new();
                widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                    Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                    &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
                assert!(output.is_empty());
            }
            if replace { let _ = s.update(Message::DiscardRejectedSend(first)); }
            let output = {
                let mut widget = s.view();
                tree.diff(widget.as_widget());
                let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
                let mut output = Vec::new();
                widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)),
                    Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                    &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
                output
            };
            let mut units = 0;
            for message in output { units += s.update(message).units(); }
            assert!(s.rejected_sends.iter().any(|send| send.token == second), "{label}: held press discarded the next message");
            assert_eq!(units, if label == "Copy message" && !replace { 1 } else { 0 }, "{label}: held press copied the next message");
            assert_eq!(s.rejected_sends.iter().any(|send| send.token == first), !replace && label == "Copy message");
            if replace {
                let current = app_clicks(&s, label);
                assert!(!current.is_empty());
                let mut current_units = 0;
                for message in current { current_units += s.update(message).units(); }
                assert_eq!(current_units, if label == "Copy message" { 1 } else { 0 });
                assert_eq!(s.rejected_sends.is_empty(), label == "Discard");
            }
        }
    }
}

#[test]
fn unsupported_message_characters_preserve_the_draft_and_cleaning_is_undoable() {
    let mut s = state();
    s.state = AppState::Connected;
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.history = Some(History::open_in_memory());
    let original = "keep\0 this\u{000b} text 😀\nnext line";
    s.message_input = text_editor::Content::with_text(original);
    s.message_input.perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
    let original_cursor = s.message_input.cursor();
    let _ = s.update(Message::SendMessage);
    assert_eq!(s.message_input.text(), original);
    assert!(s.send_error.is_some());
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
    assert!(rx.try_recv().is_err());
    let _ = s.update(Message::CleanMessageInput);
    assert_eq!(s.message_input.text(), "keep this text 😀\nnext line");
    assert!(s.send_error.is_none());
    let _ = s.update(Message::UndoInput);
    assert_eq!(s.message_input.text(), original);
    assert_eq!(s.message_input.cursor(), original_cursor);
    let _ = s.update(Message::RedoInput);
    let _ = s.update(Message::SendMessage);
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::SendDirectMessage { body, .. } if body == "keep this text 😀\nnext line"));
    assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 1);
}

#[test]
fn invalid_queued_messages_remain_recoverable_without_replay_or_reconnect() {
    let mut s = state();
    s.connected_jid = Some("me@example/resource".into());
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    let h = History::open_in_memory();
    let id = h.enqueue_outbox("alice@example", false, "old\0message").unwrap();
    s.history = Some(h);
    s.resend_outbox(|_| true);
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::SendDirectMessage { id: actual, .. } if actual == id));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendRejected {
        id, conversation: "alice@example".into(), body: "old\0message".into(), reason: "Unsupported characters".into(),
    }));
    assert!(!s.reconnecting);
    assert!(!s.outbox_in_flight.contains(&id));
    assert_eq!(s.rejected_sends[0].body, "old\0message");
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
    assert_eq!(s.history.as_ref().unwrap().load_rejected_outbox().unwrap()[0].0.body, "old\0message");
    s.resend_outbox(|_| true);
    assert!(rx.try_recv().is_err());
    let token = s.rejected_sends[0].token;
    s.history.as_ref().unwrap().disable_writes();
    let _ = s.update(Message::DiscardRejectedSend(token));
    assert!(s.rejected_sends.is_empty());
    assert_eq!(s.pending_outbox_deletes.len(), 1);
    s.resend_outbox(|_| true);
    assert!(rx.try_recv().is_err());
    s.history.as_ref().unwrap().enable_writes();
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
}

#[test]
fn rejected_send_actions_do_not_affect_another_accounts_equal_row_id() {
    let mut s = state();
    s.connected_jid = Some("me@one/resource".into());
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendRejected {
        id: 1, conversation: "alice@example".into(), body: "first account".into(), reason: "invalid".into(),
    }));
    let token = s.rejected_sends[0].token;
    s.connected_jid = Some("me@two/resource".into());
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    let h = History::open_in_memory();
    assert_eq!(h.enqueue_outbox("alice@example", false, "second account").unwrap(), 1);
    s.history = Some(h);
    let _ = s.update(Message::DiscardRejectedSend(token));
    assert_eq!(s.rejected_sends.len(), 1);
    assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 1);
    s.resend_outbox(|_| true);
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::SendDirectMessage { body, .. } if body == "second account"));
}

fn delivery_error(origin: &str, from: &str, kind: &str) -> Message {
    Message::XmppEvent(xmpp::XmppEvent::MessageError { origin_id: origin.into(), from: Some(from.into()),
        error_type: kind.into(), condition: "forbidden".into(), text: Some("Delivery refused".into()) })
}

#[test]
fn authenticated_delivery_failures_remain_visible_after_ack_and_reload() {
    for ack_first in [false, true] {
        let mut s = state();
        s.connected_jid = Some("me@local/device".into());
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        let h = History::open_in_memory();
        let message = h.enqueue_outbox_message("alice@remote", false, "recover this body").unwrap();
        s.history = Some(h);
        s.resend_outbox(|_| true);
        rx.try_recv().unwrap().into_inner();
        if ack_first { let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id: message.id, ok: true })); }
        let _ = s.update(delivery_error(&message.origin_id, "mallory@remote/phone", "cancel"));
        assert!(s.rejected_sends.is_empty());
        let _ = s.update(delivery_error(&message.origin_id, "alice@remote/phone", "continue"));
        assert!(s.rejected_sends.is_empty());
        let _ = s.update(delivery_error(&message.origin_id, "alice@remote/phone", "cancel"));
        assert_eq!(s.rejected_sends.len(), 1);
        assert!(s.rejected_sends[0].saved);
        if !ack_first { let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id: message.id, ok: true })); }
        s.rejected_sends.clear(); // Simulate rebuilding UI state from durable data.
        s.resend_outbox(|_| true);
        assert!(rx.try_recv().is_err());
        assert_eq!(s.rejected_sends[0].body, "recover this body");
        let token = s.rejected_sends[0].token;
        let _ = s.update(Message::DiscardRejectedSend(token));
        let _ = s.update(delivery_error(&message.origin_id, "alice@remote/phone", "cancel"));
        assert!(s.rejected_sends.is_empty());
        assert!(s.history.as_ref().unwrap().load_rejected_outbox().unwrap().is_empty());
    }
}

#[test]
fn a_room_occupant_cannot_forge_a_delivery_failure_for_the_room() {
    let mut s = state();
    s.connected_jid = Some("me@local/device".into());
    let h = History::open_in_memory();
    let message = h.enqueue_outbox_message("room@conference", true, "room message").unwrap();
    s.history = Some(h);
    s.history.as_ref().unwrap().disable_reads();
    let _ = s.update(delivery_error(&message.origin_id, "room@conference/mallory", "cancel"));
    assert!(s.rejected_sends.is_empty());
    let _ = s.update(delivery_error(&uuid::Uuid::new_v4().to_string(), "room@conference", "cancel"));
    assert!(s.rejected_sends.is_empty());
    let _ = s.update(delivery_error(&message.origin_id, "room@conference", "cancel"));
    assert_eq!(s.rejected_sends.len(), 1);
}

#[test]
fn delivery_failures_retry_storage_reads_and_writes_without_losing_the_report() {
    let mut s = state();
    s.connected_jid = Some("me@local/device".into());
    let h = History::open_in_memory();
    let message = h.enqueue_outbox_message("alice@remote", false, "preserve me").unwrap();
    h.disable_outgoing_reads();
    s.history = Some(h);
    let _ = s.update(delivery_error(&message.origin_id, "alice@remote/phone", "cancel"));
    assert_eq!(s.pending_message_errors.len(), 1);
    assert!(s.rejected_sends.is_empty());
    s.history.as_ref().unwrap().enable_outgoing_reads();
    s.history.as_ref().unwrap().disable_writes();
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.pending_message_errors.is_empty());
    assert_eq!(s.rejected_sends[0].body, "preserve me");
    assert!(!s.rejected_sends[0].saved);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id: message.id, ok: true }));
    assert_eq!(s.pending_outbox_deletes.len(), 1);
    s.history.as_ref().unwrap().enable_writes();
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.rejected_sends[0].saved);
    assert_eq!(s.history.as_ref().unwrap().load_rejected_outbox().unwrap().len(), 1);
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
}

#[test]
fn delayed_outbox_cleanup_is_scoped_to_its_original_account() {
    let mut s = state();
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.connected_jid = Some("me@one/resource".into());
    let first = History::open_in_memory();
    let id = first.enqueue_outbox("alice@example", false, "account one").unwrap();
    first.disable_writes();
    s.history = Some(first);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id, ok: true }));
    let first = s.history.take().unwrap();
    s.connected_jid = Some("me@two/resource".into());
    let second = History::open_in_memory();
    assert_eq!(second.enqueue_outbox("alice@example", false, "account two").unwrap(), id);
    s.history = Some(second);
    let _ = s.update(Message::RetryHistoryWrites);
    assert_eq!(s.pending_outbox_deletes.len(), 1);
    assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 1);
    s.resend_outbox(|_| true);
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::SendDirectMessage { body, .. } if body == "account two"));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id, ok: true }));
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
    assert_eq!(s.pending_outbox_deletes.len(), 1);
    s.connected_jid = Some("me@one/new-resource".into());
    first.enable_writes();
    s.history = Some(first);
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.pending_outbox_deletes.is_empty());
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
}

#[test]
fn one_blocked_outbox_cleanup_does_not_starve_other_delivered_messages() {
    for (detached, discarded) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut s = state();
        let owner = Some("cleanup@example".to_string());
        s.connected_jid = Some("cleanup@example/device".into());
        s.saved_config.jid = s.connected_jid.clone();
        let h = storage::open_history("cleanup@example", Some("cleanup@example")).unwrap();
        for index in 0..HISTORY_RETRY_BATCH + 7 {
            let id = h.enqueue_outbox("alice@example", false, &format!("delivered {index}")).unwrap();
            s.pending_outbox_deletes.insert((owner.clone(), id));
            if discarded { s.discarded_outbox.insert((owner.clone(), id)); }
        }
        // The existing HashSet traversal always encounters this retained row
        // first, reproducing starvation independently of randomized hash order.
        let blocked_id = s.pending_outbox_deletes.iter().next().unwrap().1;
        let db = rusqlite::Connection::open(storage::history_path().unwrap()).unwrap();
        db.execute_batch(&format!("CREATE TRIGGER snack_test_block_cleanup BEFORE DELETE ON outbox
            WHEN OLD.id = {blocked_id} BEGIN SELECT RAISE(FAIL, 'one cleanup unavailable'); END;")).unwrap();
        if detached { s.connected_jid = None; }
        else { s.history = Some(h); }
        for _ in 0..4 {
            let before = s.pending_outbox_deletes.len();
            if detached { s.retry_detached_history(&None); }
            else { s.retry_outbox_deletes(); }
            assert!(before - s.pending_outbox_deletes.len() <= HISTORY_RETRY_BATCH);
        }
        assert_eq!(s.pending_outbox_deletes.len(), 1, "detached={detached}: writable acknowledgements must get a turn");
        assert!(s.pending_outbox_deletes.contains(&(owner.clone(), blocked_id)));
        let history = storage::open_history("cleanup@example", Some("cleanup@example")).unwrap();
        assert_eq!(history.load_outbox().unwrap().iter().map(|item| item.id).collect::<Vec<_>>(), vec![blocked_id]);
        let more = if detached { s.retry_detached_history(&None) } else { s.retry_outbox_deletes() };
        assert!(!more, "a blocked-only remainder must use the normal retry timer");
        db.execute_batch("DROP TRIGGER snack_test_block_cleanup").unwrap();
        if detached { s.retry_detached_history(&None); }
        else { s.retry_outbox_deletes(); }
        assert!(s.pending_outbox_deletes.is_empty());
        assert!(s.discarded_outbox.is_empty());
        assert!(s.outbox_delete_retry_after.is_empty());
        assert!(history.load_outbox().unwrap().is_empty());
    }
}

#[test]
fn delivered_outbox_cleanup_retries_in_bounded_batches() {
    let mut s = state();
    let h = History::open_in_memory();
    let ids: Vec<_> = (0..HISTORY_RETRY_BATCH + 7)
        .map(|i| h.enqueue_outbox("alice@example", false, &i.to_string()).unwrap()).collect();
    h.disable_writes();
    s.history = Some(h);
    for id in ids {
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id, ok: true }));
    }
    assert_eq!(s.pending_outbox_deletes.len(), HISTORY_RETRY_BATCH + 7);
    s.history.as_ref().unwrap().enable_writes();
    assert!(s.retry_outbox_deletes());
    assert_eq!(s.pending_outbox_deletes.len(), 7);
    assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 7);
    assert!(!s.retry_outbox_deletes());
    assert!(s.pending_outbox_deletes.is_empty());
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
}

#[test]
fn recovered_outbox_reads_replay_only_dms_and_confirmed_rooms() {
    let mut s = state();
    let (tx, mut rx) = tokio::sync::mpsc::channel(20);
    s.xmpp_cmd_tx = Some(tx);
    let h = History::open_in_memory();
    let dm = h.enqueue_outbox("alice@example", false, "dm").unwrap();
    let joined = h.enqueue_outbox("joined@example", true, "joined room").unwrap();
    let waiting = h.enqueue_outbox("waiting@example", true, "waiting room").unwrap();
    h.disable_outbox_reads();
    s.history = Some(h);
    s.request_outbox_replay(None);
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { own_nick: "me".into(), room: "joined@example".into(), members: Vec::new() });
    assert_eq!(s.outbox_replay_pending.len(), 2);
    assert!(rx.try_recv().is_err());
    let _ = s.update(Message::RetryHistoryWrites);
    assert_eq!(s.outbox_replay_pending.len(), 2);
    s.history.as_ref().unwrap().enable_outbox_reads();
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::SendDirectMessage { id, .. } if id == dm));
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::SendRoomMessage { id, .. } if id == joined));
    assert!(rx.try_recv().is_err());
    assert!(s.outbox_replay_pending.is_empty());
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { own_nick: "me".into(), room: "waiting@example".into(), members: Vec::new() });
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::SendRoomMessage { id, .. } if id == waiting));
}

#[test]
fn reconnect_discards_failed_room_replay_until_the_new_join_is_confirmed() {
    let mut s = state();
    let (tx, mut rx) = tokio::sync::mpsc::channel(20);
    s.xmpp_cmd_tx = Some(tx);
    let h = History::open_in_memory();
    let id = h.enqueue_outbox("room@example", true, "room").unwrap();
    h.disable_outbox_reads();
    s.history = Some(h);
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { own_nick: "me".into(), room: "room@example".into(), members: Vec::new() });
    assert!(!s.outbox_replay_pending.is_empty());
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Reconnecting));
    assert!(s.outbox_replay_pending.is_empty());
    s.history.as_ref().unwrap().enable_outbox_reads();
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Reconnected { generation: 1 }));
    let _ = s.update(Message::RetryHistoryWrites);
    while let Ok(command) = rx.try_recv() {
        let command = command.into_inner();
        assert!(!matches!(command, xmpp::XmppCommand::SendRoomMessage { .. }));
    }
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { own_nick: "me".into(), room: "room@example".into(), members: Vec::new() });
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::SendRoomMessage { id: actual, .. } if actual == id));
}

#[test]
fn server_room_departure_preserves_active_and_background_drafts() {
    for active in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.history = Some(History::open_in_memory());
        let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined {
            room: "room@example".into(), own_nick: "me".into(), members: Vec::new(),
        });
        let _ = s.update(Message::SelectRoom("room@example".into()));
        let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Insert('x'))));
        if !active { let _ = s.update(Message::SelectChat("alice@example".into())); }
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomLeft("room@example".into())));
        assert!(s.rooms.is_empty());
        assert_eq!(s.saved_config.drafts.get("room@example").map(String::as_str), Some("x"));
        assert_eq!(storage::load().drafts.get("room@example").map(String::as_str), Some("x"));
        if active { assert_eq!(s.message_input.text(), ""); }
        let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined {
            room: "room@example".into(), own_nick: "me".into(), members: Vec::new(),
        });
        let _ = s.update(Message::SelectRoom("room@example".into()));
        assert_eq!(s.message_input.text(), "x");
        let _ = s.update(Message::UndoInput);
        assert_eq!(s.message_input.text(), "");
    }
}

#[test]
fn server_room_departure_keeps_the_draft_available_after_a_failed_save() {
    let mut s = state();
    s.history = Some(History::open_in_memory());
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined {
        room: "room@example".into(), own_nick: "me".into(), members: Vec::new(),
    });
    let _ = s.update(Message::SelectRoom("room@example".into()));
    let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Insert('x'))));
    let path = storage::config_path().unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomLeft("room@example".into())));
    assert!(s.active.is_none());
    assert!(s.settings_save_error.is_some());
    assert_eq!(s.saved_config.drafts.get("room@example").map(String::as_str), Some("x"));
    std::fs::remove_dir(&path).unwrap();
    let _ = s.update(Message::RetrySettingsSave);
    assert!(s.settings_save_error.is_none());
    assert_eq!(storage::load().drafts.get("room@example").map(String::as_str), Some("x"));
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined {
        room: "room@example".into(), own_nick: "me".into(), members: Vec::new(),
    });
    assert_eq!(s.message_input.text(), "x");
    let _ = s.update(Message::UndoInput);
    assert_eq!(s.message_input.text(), "");
}

#[test]
fn manually_leaving_an_unremembered_room_durably_discards_its_draft() {
    let mut s = state();
    s.history = Some(History::open_in_memory());
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined {
        room: "room@example".into(), own_nick: "me".into(), members: Vec::new(),
    });
    let _ = s.update(Message::SelectRoom("room@example".into()));
    let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Insert('x'))));
    let _ = s.update(Message::SelectChat("alice@example".into()));
    assert_eq!(storage::load().drafts.get("room@example").map(String::as_str), Some("x"));
    let _ = s.update(Message::SelectRoom("room@example".into()));
    assert!(!s.saved_config.rooms.contains(&"room@example".to_string()));
    let _ = s.update(Message::LeaveRoom);
    assert!(!s.saved_config.drafts.contains_key("room@example"));
    assert!(!storage::load().drafts.contains_key("room@example"));
}

#[test]
fn leaving_a_room_cancels_pending_outbox_read_retries_for_it() {
    for manual in [false, true] {
        let mut s = state();
        let (tx, mut rx) = tokio::sync::mpsc::channel(20);
        s.xmpp_cmd_tx = Some(tx);
        let h = History::open_in_memory();
        h.enqueue_outbox("room@example", true, "room").unwrap();
        h.disable_outbox_reads();
        s.history = Some(h);
        let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { own_nick: "me".into(), room: "room@example".into(), members: Vec::new() });
        let _ = s.update(Message::SelectRoom("room@example".into()));
        let _ = s.update(if manual { Message::LeaveRoom } else { Message::XmppEvent(xmpp::XmppEvent::RoomLeft("room@example".into())) });
        assert!(s.outbox_replay_pending.is_empty());
        s.history.as_ref().unwrap().enable_outbox_reads();
        let _ = s.update(Message::RetryHistoryWrites);
        while let Ok(command) = rx.try_recv() {
            let command = command.into_inner();
            assert!(!matches!(command, xmpp::XmppCommand::SendRoomMessage { .. }));
        }
    }
}

#[test]
fn full_command_channel_preserves_archive_request_until_retry() {
    let mut s = state();
    s.history = Some(History::open_in_memory());
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tx.try_send(xmpp::XmppCommand::ForceReconnect).unwrap();
    s.xmpp_cmd_tx = Some(tx);
    s.fetch_archive_active();
    assert!(s.chats[0].mam_in_flight);
    assert_eq!(s.xmpp_pending_commands.len(), 1);
    rx.try_recv().unwrap().into_inner();
    let _ = s.update(Message::FlushXmppCommands);
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::QueryArchive { query_id, .. }
        if s.mam_pending.contains_key(&query_id)));
    assert!(s.xmpp_pending_commands.is_empty());
}

#[test]
fn closed_command_channel_releases_unsent_archive_request() {
    let mut s = state();
    s.history = Some(History::open_in_memory());
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    drop(rx);
    s.xmpp_cmd_tx = Some(tx);
    s.fetch_archive_active();
    assert!(!s.chats[0].mam_in_flight);
    assert!(s.mam_pending.is_empty());
    assert!(s.xmpp_pending_commands.is_empty());
    assert!(s.xmpp_cmd_tx.is_none());
}

#[test]
fn cancelling_connection_discards_queued_commands_for_that_session() {
    let mut s = state();
    s.state = AppState::Connecting;
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    tx.try_send(xmpp::XmppCommand::ForceReconnect).unwrap();
    s.xmpp_cmd_tx = Some(tx);
    assert!(s.send_xmpp_command(xmpp::XmppCommand::JoinRoom("room@example".into())));
    assert_eq!(s.xmpp_pending_commands.len(), 1);
    let _ = s.update(Message::CancelConnect);
    assert!(s.xmpp_pending_commands.is_empty());
    assert!(s.xmpp_cmd_tx.is_none());
}

#[test]
fn forgetting_login_and_retrying_cleanup_remain_visible_across_restart() {
    for connected in [false, true] {
        let jid = if connected { "connected-cleanup@example" } else { "login-cleanup@example" };
        let mut s = state();
        if connected { s.state = AppState::Connected; }
        s.saved_config.jid = Some(jid.into());
        s.remember_me = true;
        storage::save_password(jid, "test secret").unwrap();
        storage::fail_next_password_operation(jid, keyring_core::Error::NoStorageAccess(Box::new(std::io::Error::other("locked"))));
        let _ = s.update(if connected { Message::ForgetAutoLogin } else { Message::RememberMeToggled(false) });
        assert!(!s.remember_me);
        assert!(s.saved_config.jid.is_none());
        assert!(s.credential_error.is_some());
        assert!(crate::ui::credential_notice(&s).is_some());
        assert!(storage::load().jid.is_none());
        storage::fail_next_password_operation(jid, keyring_core::Error::NoStorageAccess(Box::new(std::io::Error::other("still locked"))));
        let (mut reopened, _) = Snack::new();
        assert!(!reopened.auto_login_attempt);
        assert!(reopened.password_input.is_empty());
        assert!(reopened.credential_error.is_some());
        assert_eq!(reopened.saved_config.pending_credential_removals, [jid]);
        let _ = reopened.update(Message::RetryCredentialRemoval);
        assert!(reopened.credential_error.is_none());
        assert!(reopened.saved_config.pending_credential_removals.is_empty());
        assert!(storage::load_password(jid).unwrap().is_none());
        assert!(crate::ui::credential_notice(&reopened).is_none());
    }
}

#[test]
fn locked_saved_login_can_be_retried_without_losing_credentials_or_using_an_edited_account() {
    let jid = "saved-login@example/device";
    let mut cfg = storage::SavedConfig::default();
    cfg.jid = Some(jid.into());
    storage::save(&cfg).unwrap();
    storage::save_password(jid, "saved secret").unwrap();
    storage::fail_next_password_operation(jid, keyring_core::Error::NoStorageAccess(Box::new(std::io::Error::other("locked"))));
    let (mut s, _) = Snack::new();
    assert!(s.state == AppState::Login);
    assert!(!s.auto_login_attempt);
    assert!(s.password_input.is_empty());
    assert!(s.remember_me);
    assert!(s.saved_login_error.as_deref().unwrap().contains("locked"));
    assert_eq!(storage::load().jid.as_deref(), Some(jid));
    assert!(crate::ui::credential_notice(&s).is_some());
    s.jid_input = "edited@other.example".into();
    s.credential_error = Some("unrelated cleanup warning".into());
    let _ = s.update(Message::RetrySavedLogin);
    assert!(s.state == AppState::Connecting);
    assert!(s.auto_login_attempt);
    assert_eq!(s.connected_jid.as_deref(), Some(jid));
    assert_eq!(s.pending_save_password.as_deref(), Some("saved secret"));
    assert!(s.saved_login_error.is_none());
    assert_eq!(s.credential_error.as_deref(), Some("unrelated cleanup warning"));
    // No subscription is run, so no real connection or login is attempted.
    assert!(s.xmpp_cmd_rx.is_some());
    storage::fail_next_password_operation(jid, keyring_core::Error::NoStorageAccess(Box::new(std::io::Error::other("must not be read while connecting"))));
    let _ = s.update(Message::RetrySavedLogin);
    assert!(s.saved_login_error.is_none());
    assert!(storage::load_password(jid).is_err());
    let (restarted, _) = Snack::new();
    assert!(restarted.state == AppState::Connecting);
    assert_eq!(restarted.connected_jid.as_deref(), Some(jid));
    assert_eq!(restarted.pending_save_password.as_deref(), Some("saved secret"));
}

#[test]
fn missing_saved_password_is_distinct_from_a_keyring_failure() {
    let mut cfg = storage::SavedConfig::default();
    cfg.jid = Some("missing-password@example".into());
    storage::save(&cfg).unwrap();
    let (s, _) = Snack::new();
    assert!(s.state == AppState::Login);
    assert!(!s.auto_login_attempt);
    assert!(s.saved_login_error.is_none());
    assert!(s.password_input.is_empty());
    assert!(s.remember_me);
}

#[test]
fn opting_out_after_a_keyring_read_failure_cannot_be_reversed_by_a_stale_retry() {
    let jid = "opt-out@example";
    let mut cfg = storage::SavedConfig::default();
    cfg.jid = Some(jid.into());
    storage::save(&cfg).unwrap();
    storage::save_password(jid, "saved secret").unwrap();
    storage::fail_next_password_operation(jid, keyring_core::Error::NoStorageAccess(Box::new(std::io::Error::other("locked"))));
    let (mut s, _) = Snack::new();
    assert!(s.saved_login_error.is_some());
    let _ = s.update(Message::RememberMeToggled(false));
    assert!(s.saved_login_error.is_none());
    assert!(storage::load_password(jid).unwrap().is_none());
    let _ = s.update(Message::RetrySavedLogin);
    assert!(s.state == AppState::Login);
    assert!(s.saved_config.jid.is_none());
    assert!(!s.remember_me);
    assert!(!s.auto_login_attempt);
    assert!(s.xmpp_cmd_rx.is_none());
}

#[test]
fn damaged_settings_block_login_and_only_explicit_recovery_creates_a_backup() {
    let path = storage::config_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = b"[drafts]\nroom = 'important unfinished draft";
    std::fs::write(&path, original).unwrap();
    let (mut s, _) = Snack::new();
    assert!(s.saved_config.load_error.is_some());
    assert!(s.state == AppState::Login);
    s.jid_input = "me@example".into();
    s.password_input = "test secret".into();
    let _ = s.update(Message::Connect);
    assert!(s.state == AppState::Login);
    assert!(s.xmpp_cmd_rx.is_none());
    let _ = s.update(Message::RememberMeToggled(false));
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    let _ = s.update(Message::RecoverConfiguration);
    assert!(s.saved_config.load_error.is_none());
    assert!(s.config_notice.as_deref().unwrap().contains("config-recovery-"));
    let backup = std::fs::read_dir(path.parent().unwrap()).unwrap().filter_map(Result::ok)
        .find(|entry| entry.file_name().to_string_lossy().starts_with("config-recovery-")).unwrap();
    assert_eq!(std::fs::read(backup.path()).unwrap(), original);
    assert!(storage::load().load_error.is_none());
    s.saved_config.drafts.insert("room@example".into(), "new draft".into());
    let _ = s.update(Message::RecoverConfiguration); // stale UI event cannot reset healthy settings.
    assert_eq!(s.saved_config.drafts["room@example"], "new draft");
}

#[test]
fn repaired_settings_can_be_reloaded_without_resetting_drafts_or_credentials() {
    let path = storage::config_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"not TOML").unwrap();
    let (mut s, _) = Snack::new();
    assert!(s.saved_config.load_error.is_some());
    // Model the user repairing their file outside the application.
    let mut repaired = storage::SavedConfig::default();
    repaired.jid = Some("repair@example".into());
    repaired.drafts.insert("room@example".into(), "recovered draft".into());
    storage::save_password("repair@example", "recovered secret").unwrap();
    std::fs::write(&path, basic_toml::to_string(&repaired).unwrap()).unwrap();
    let _ = s.update(Message::RetryConfigLoad);
    assert!(s.saved_config.load_error.is_none());
    assert_eq!(s.saved_config.drafts["room@example"], "recovered draft");
    assert!(s.state == AppState::Connecting);
    assert_eq!(s.pending_save_password.as_deref(), Some("recovered secret"));
    assert!(s.config_notice.is_none());
    assert!(!std::fs::read_dir(path.parent().unwrap()).unwrap().filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().starts_with("config-recovery-")));
}

#[test]
fn closing_a_window_keeps_unsaved_drafts_when_configuration_write_fails() {
    let mut s = state();
    s.message_input = text_editor::Content::with_text("draft that must survive a failed close");
    let path = storage::config_path().unwrap();
    std::fs::create_dir_all(&path).unwrap(); // Force atomic config replacement to fail.
    let task = s.update(Message::WindowCloseRequested(iced::window::Id::unique()));
    assert_eq!(task.units(), 0, "failed save must not issue a window-close task");
    assert_eq!(s.message_input.text(), "draft that must survive a failed close");
    assert_eq!(s.saved_config.drafts["alice@example"], "draft that must survive a failed close");
    assert!(s.settings_save_error.is_some());
    assert!(s.pending_close.is_some());
    std::fs::remove_dir(&path).unwrap();
    s.message_input = text_editor::Content::with_text("newest edited draft");
    let task = s.update(Message::RetrySettingsSave);
    assert_eq!(task.units(), 1);
    assert!(s.pending_close.is_none());
    assert!(s.settings_save_error.is_none());
    assert_eq!(storage::load().drafts["alice@example"], "newest edited draft");
}

#[test]
fn failed_close_keeps_undo_and_stale_discard_cannot_close_a_kept_window() {
    let mut s = state();
    s.message_input = text_editor::Content::with_text("original");
    s.message_input.perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
    let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Paste(std::sync::Arc::new(" edit".into())))));
    let path = storage::config_path().unwrap();
    std::fs::create_dir_all(&path).unwrap();
    let window = iced::window::Id::unique();
    assert_eq!(s.update(Message::WindowCloseRequested(window)).units(), 0);
    let _ = s.update(Message::UndoInput);
    assert_eq!(s.message_input.text(), "original");
    let _ = s.update(Message::KeepWindowOpen);
    assert!(s.pending_close.is_none());
    assert_eq!(s.update(Message::CloseWithoutSaving).units(), 0);
    assert_eq!(s.update(Message::WindowCloseRequested(window)).units(), 0);
    assert_eq!(s.update(Message::CloseWithoutSaving).units(), 1);
    assert!(path.is_dir());
    assert!(s.pending_close.is_none());
}

#[test]
fn close_waits_for_unpersisted_messages_but_not_the_durable_outbox() {
    let mut s = state();
    s.pending_history_writes.push(crate::app::PendingHistoryWrite { receipt: uuid::Uuid::new_v4(),
        account: None, conversation: "alice@example".into(), sender: "alice".into(), body: "unsaved incoming".into(),
        received: chrono::Utc::now(), delayed: false, stanza_id: None, temp_id: -500,
        outbox_id: None, own_origin: None, own_room_origin: false, allow_outgoing_fuzzy: false,
    });
    assert_eq!(s.update(Message::WindowCloseRequested(iced::window::Id::unique())).units(), 0);
    assert!(s.pending_close.is_some());
    assert!(s.settings_save_error.is_none());
    let database = storage::config_path().unwrap().with_file_name("close-history.sqlite");
    s.history = Some(History::open(&database).unwrap());
    s.history.as_ref().unwrap().enqueue_outbox("alice@example", false, "durably queued").unwrap();
    s.outbox_replay_pending.insert(None);
    assert_eq!(s.update(Message::RetrySettingsSave).units(), 1);
    assert!(!s.has_unsaved_message_updates());
    assert_eq!(s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap()[0].body, "unsaved incoming");
    assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 1);
    drop(s);
    let reopened = History::open(&database).unwrap();
    assert_eq!(reopened.load_tail("alice@example", 10).unwrap()[0].body, "unsaved incoming");
    assert_eq!(reopened.load_outbox().unwrap()[0].body, "durably queued");
}

#[test]
fn window_close_waits_for_the_active_worker_before_issuing_an_os_close() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.saved_config.draft_account = Some("me@example".into());
    s.message_input = text_editor::Content::with_text("private draft");
    s.history = Some(History::open_in_memory());
    let (tx, channel) = xmpp::new_command_channel("me@example/device".into(), "test secret".into());
    s.xmpp_cmd_tx = Some(tx);
    s.xmpp_cmd_rx = Some(channel);
    let window = iced::window::Id::unique();
    assert_eq!(s.update(Message::WindowCloseRequested(window)).units(), 0,
        "saving the current UI snapshot cannot close the window before the worker drains");
    assert!(s.pending_close.is_some());
    assert!(s.settings_save_error.is_none());
}

fn shutdown_state() -> (Snack, xmpp::CommandChannel) {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.saved_config.jid = s.connected_jid.clone();
    s.saved_config.draft_account = Some("me@example".into());
    s.message_input = text_editor::Content::with_text("private draft");
    s.chats[0].messages.clear();
    s.body_editors.clear();
    s.history = Some(storage::open_history("me@example", Some("me@example")).unwrap());
    let (tx, channel) = xmpp::new_command_channel("me@example/device".into(), "test secret".into());
    s.xmpp_cmd_tx = Some(tx);
    s.xmpp_cmd_rx = Some(channel.clone());
    (s, channel)
}

fn late_shutdown_message(channel: &xmpp::CommandChannel) -> Message {
    Message::ScopedXmppEvent { channel: channel.clone(), event: xmpp::XmppEvent::DirectMessage {
        from: "alice@example/phone".into(), body: "arrived during close".into(), timestamp: chrono::Utc::now(),
        delayed: false, stanza_id: Some("final-message".into()), origin_id: None,
    } }
}

#[test]
fn window_shutdown_drains_messages_and_blocks_edits_until_os_close() {
    let (mut s, channel) = shutdown_state();
    let window = iced::window::Id::unique();
    assert_eq!(s.update(Message::WindowCloseRequested(window)).units(), 0);
    let token = s.pending_close.unwrap().token;
    assert_eq!(s.update(Message::WindowCloseRequested(window)).units(), 0);
    assert_eq!(s.pending_close.unwrap().token, token);
    for event in [xmpp::XmppEvent::Connected, xmpp::XmppEvent::Reconnecting,
        xmpp::XmppEvent::Disconnected("closing".into())] {
        let _ = s.update(Message::ScopedXmppEvent { channel: channel.clone(), event });
    }
    assert_eq!(s.xmpp_cmd_rx.as_ref(), Some(&channel));
    let _ = s.update(Message::Disconnect);
    let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Insert('x'))));
    assert_eq!(s.message_input.text(), "private draft");
    assert_eq!(s.state, AppState::Connected);
    let (_, foreign) = xmpp::new_command_channel("other@example".into(), "secret".into());
    assert_eq!(s.update(Message::ScopedXmppEvent { channel: foreign, event: xmpp::XmppEvent::Stopped }).units(), 0);
    let _ = s.update(late_shutdown_message(&channel));
    assert_eq!(s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap()[0].body, "arrived during close");
    assert_eq!(s.update(Message::ScopedXmppEvent { channel, event: xmpp::XmppEvent::Stopped }).units(), 1);
    assert!(s.xmpp_cmd_rx.is_none());
    assert!(s.pending_close.unwrap().draining);
    let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Insert('y'))));
    assert_eq!(s.message_input.text(), "private draft");
    assert_eq!(storage::load().drafts["alice@example"], "private draft");
}

#[test]
fn window_shutdown_save_failure_preserves_data_and_exposes_recovery() {
    for decision in ["retry", "keep", "discard"] {
        let (mut s, channel) = shutdown_state();
        assert_eq!(s.update(Message::WindowCloseRequested(iced::window::Id::unique())).units(), 0);
        let path = storage::config_path().unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let _ = s.update(late_shutdown_message(&channel));
        assert_eq!(s.update(Message::ScopedXmppEvent { channel, event: xmpp::XmppEvent::Stopped }).units(), 0);
        assert_eq!(s.state, AppState::Login);
        assert!(!s.pending_close.unwrap().draining);
        assert!(s.settings_save_error.is_some());
        assert_eq!(s.saved_config.drafts["alice@example"], "private draft");
        assert_eq!(s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap()[0].body, "arrived during close");
        match decision {
            "retry" => {
                std::fs::remove_dir(&path).unwrap();
                assert_eq!(s.update(Message::RetrySettingsSave).units(), 1);
                assert_eq!(storage::load().drafts["alice@example"], "private draft");
            }
            "keep" => { assert_eq!(s.update(Message::KeepWindowOpen).units(), 0); }
            _ => { assert_eq!(s.update(Message::CloseWithoutSaving).units(), 1); }
        }
        assert!(s.pending_close.is_none());
        if path.is_dir() { std::fs::remove_dir(&path).unwrap(); }
    }
}

#[test]
fn window_shutdown_explicit_discard_still_waits_for_incoming_messages() {
    let (mut s, channel) = shutdown_state();
    let path = storage::config_path().unwrap();
    std::fs::create_dir_all(&path).unwrap();
    assert_eq!(s.update(Message::WindowCloseRequested(iced::window::Id::unique())).units(), 0);
    assert!(!s.pending_close.unwrap().draining);
    assert_eq!(s.update(Message::CloseWithoutSaving).units(), 0);
    assert!(s.pending_close.unwrap().draining && s.pending_close.unwrap().discard);
    let _ = s.update(late_shutdown_message(&channel));
    assert_eq!(s.update(Message::ScopedXmppEvent { channel, event: xmpp::XmppEvent::Stopped }).units(), 1);
    assert_eq!(s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap()[0].body, "arrived during close");
    assert!(path.is_dir());
}

#[test]
fn window_shutdown_interrupts_connecting_without_activating_the_session() {
    let (mut s, channel) = shutdown_state();
    s.state = AppState::Connecting;
    assert_eq!(s.update(Message::WindowCloseRequested(iced::window::Id::unique())).units(), 0);
    let _ = s.update(Message::ScopedXmppEvent { channel: channel.clone(), event: xmpp::XmppEvent::Connected });
    assert_eq!(s.state, AppState::Connecting);
    assert_eq!(s.update(Message::ScopedXmppEvent { channel, event: xmpp::XmppEvent::Stopped }).units(), 1);
}

#[test]
fn window_shutdown_retains_failed_final_history_writes_for_retry() {
    let (mut s, channel) = shutdown_state();
    assert_eq!(s.update(Message::WindowCloseRequested(iced::window::Id::unique())).units(), 0);
    s.history.as_ref().unwrap().disable_writes();
    let _ = s.update(late_shutdown_message(&channel));
    assert_eq!(s.pending_history_writes.len(), 1);
    assert_eq!(s.update(Message::ScopedXmppEvent { channel, event: xmpp::XmppEvent::Stopped }).units(), 0);
    assert_eq!(s.state, AppState::Login);
    assert!(!s.pending_close.unwrap().draining);
    assert_eq!(s.pending_history_writes.len(), 1);
    assert_eq!(s.pending_history_writes[0].account.as_deref(), Some("me@example"));
    assert_eq!(s.update(Message::RetrySettingsSave).units(), 1);
    assert!(s.pending_history_writes.is_empty());
    let reopened = storage::open_history("me@example", Some("me@example")).unwrap();
    assert_eq!(reopened.load_tail("alice@example", 10).unwrap()[0].body, "arrived during close");
    assert_eq!(storage::load().drafts["alice@example"], "private draft");
}

#[test]
fn logout_keeps_its_session_until_final_messages_have_been_saved() {
    let (mut s, channel) = shutdown_state();
    assert_eq!(s.update(Message::Disconnect).units(), 0);
    assert_eq!(s.xmpp_cmd_rx.as_ref(), Some(&channel), "logout must retain the incoming subscription until it drains");
    assert!(s.disconnecting);
    let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Insert('x'))));
    let _ = s.update(Message::JidInputChanged("other@example".into()));
    let _ = s.update(Message::Connect);
    assert_eq!(s.message_input.text(), "private draft");
    assert_eq!(s.connected_jid.as_deref(), Some("me@example/device"));
    for event in [xmpp::XmppEvent::Connected, xmpp::XmppEvent::Reconnecting, xmpp::XmppEvent::Disconnected("closing".into())] {
        let _ = s.update(Message::ScopedXmppEvent { channel: channel.clone(), event });
    }
    assert_eq!(s.xmpp_cmd_rx.as_ref(), Some(&channel));
    let _ = s.update(late_shutdown_message(&channel));
    let _ = s.update(Message::ScopedXmppEvent { channel, event: xmpp::XmppEvent::Stopped });
    assert_eq!(s.state, AppState::Login);
    assert!(!s.disconnecting);
    assert!(s.xmpp_cmd_rx.is_none());
    let reopened = storage::open_history("me@example", Some("me@example")).unwrap();
    assert_eq!(reopened.load_tail("alice@example", 10).unwrap()[0].body, "arrived during close");
    assert_eq!(storage::load().drafts["alice@example"], "private draft");
}

#[test]
fn window_close_during_logout_waits_for_the_same_completion_barrier() {
    for fail_settings in [false, true] {
        let (mut s, channel) = shutdown_state();
        assert_eq!(s.update(Message::Disconnect).units(), 0);
        let path = storage::config_path().unwrap();
        if fail_settings {
            if path.is_file() { std::fs::remove_file(&path).unwrap(); }
            std::fs::create_dir_all(&path).unwrap();
        }
        assert_eq!(s.update(Message::WindowCloseRequested(iced::window::Id::unique())).units(), 0);
        let _ = s.update(late_shutdown_message(&channel));
        // Failed pre-close save leaves login focus; a successful save closes.
        assert_eq!(s.update(Message::ScopedXmppEvent { channel, event: xmpp::XmppEvent::Stopped }).units(), 1);
        if fail_settings {
            assert_eq!(s.state, AppState::Login);
            assert!(s.settings_save_error.is_some());
            assert!(!s.pending_close.unwrap().draining);
            std::fs::remove_dir(&path).unwrap();
            assert_eq!(s.update(Message::RetrySettingsSave).units(), 1);
        } else {
            assert!(s.pending_close.unwrap().draining);
        }
        assert!(!s.disconnecting);
        assert_eq!(s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap()[0].body, "arrived during close");
    }
}

#[test]
fn logout_retains_failed_final_message_writes_for_disconnected_retry() {
    let (mut s, channel) = shutdown_state();
    assert_eq!(s.update(Message::Disconnect).units(), 0);
    s.history.as_ref().unwrap().disable_writes();
    let _ = s.update(late_shutdown_message(&channel));
    let _ = s.update(Message::ScopedXmppEvent { channel, event: xmpp::XmppEvent::Stopped });
    assert_eq!(s.state, AppState::Login);
    assert_eq!(s.pending_history_writes.len(), 1);
    assert_eq!(s.pending_history_writes[0].account.as_deref(), Some("me@example"));
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.pending_history_writes.is_empty());
    let reopened = storage::open_history("me@example", Some("me@example")).unwrap();
    assert_eq!(reopened.load_tail("alice@example", 10).unwrap()[0].body, "arrived during close");
}

#[test]
fn cancelling_connection_waits_for_stop_and_ignores_a_queued_connected_event() {
    let (mut s, channel) = shutdown_state();
    s.state = AppState::Connecting;
    assert_eq!(s.update(Message::Disconnect).units(), 0);
    assert!(s.disconnecting);
    let _ = s.update(Message::ScopedXmppEvent { channel: channel.clone(), event: xmpp::XmppEvent::Connected });
    assert_eq!(s.state, AppState::Connecting);
    let _ = s.update(Message::ScopedXmppEvent { channel, event: xmpp::XmppEvent::Stopped });
    assert_eq!(s.state, AppState::Login);
    assert!(!s.disconnecting);
    assert!(s.xmpp_cmd_rx.is_none());
}

#[test]
fn login_cancel_button_drains_incoming_data_without_accepting_the_login() {
    let (mut s, channel) = shutdown_state();
    s.state = AppState::Connecting;
    s.auto_login_attempt = true;
    s.password_input = "saved test secret".into();
    s.pending_save_password = Some("saved test secret".into());
    let events = login_clicks(&s, "Cancel");
    assert!(!events.is_empty());
    for event in events { assert_eq!(s.update(event).units(), 0); }
    assert_eq!(s.xmpp_cmd_rx.as_ref(), Some(&channel));
    assert!(s.disconnecting);
    assert!(s.password_input.is_empty());
    assert!(s.pending_save_password.is_none());
    let _ = s.update(Message::ScopedXmppEvent { channel: channel.clone(), event: xmpp::XmppEvent::Connected });
    assert_eq!(s.state, AppState::Connecting);
    let _ = s.update(late_shutdown_message(&channel));
    let _ = s.update(Message::ScopedXmppEvent { channel, event: xmpp::XmppEvent::Stopped });
    assert_eq!(s.state, AppState::Login);
    assert!(s.xmpp_cmd_rx.is_none());
    assert!(storage::load_password("me@example/device").unwrap().is_none());
    let reopened = storage::open_history("me@example", Some("me@example")).unwrap();
    assert_eq!(reopened.load_tail("alice@example", 10).unwrap()[0].body, "arrived during close");
}

#[test]
fn logout_preserves_the_final_draft_when_configuration_saving_fails() {
    let (mut s, channel) = shutdown_state();
    let path = storage::config_path().unwrap();
    std::fs::create_dir_all(&path).unwrap();
    assert_eq!(s.update(Message::Disconnect).units(), 0);
    let _ = s.update(Message::ScopedXmppEvent { channel, event: xmpp::XmppEvent::Stopped });
    assert_eq!(s.state, AppState::Login);
    assert!(s.settings_save_error.is_some());
    assert_eq!(s.saved_config.drafts["alice@example"], "private draft");
    std::fs::remove_dir(&path).unwrap();
    assert_eq!(s.update(Message::RetrySettingsSave).units(), 0);
    assert!(s.settings_save_error.is_none());
    assert_eq!(storage::load().drafts["alice@example"], "private draft");
}

#[test]
fn failed_configuration_recovery_screen_can_still_be_closed() {
    let path = storage::config_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"invalid settings").unwrap();
    let (mut s, _) = Snack::new();
    assert!(s.saved_config.load_error.is_some());
    assert_eq!(s.update(Message::WindowCloseRequested(iced::window::Id::unique())).units(), 1);
    assert_eq!(std::fs::read(&path).unwrap(), b"invalid settings");
}

#[test]
fn disconnected_account_history_can_be_saved_before_closing_without_another_login() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("previous@example/device".into());
    s.saved_config.jid = Some("previous@example/device".into());
    let history = storage::open_history("previous@example", Some("previous@example")).unwrap();
    history.disable_writes();
    s.history = Some(history);
    s.persist_message("alice@example", "alice", "received before disconnect", chrono::Utc::now(), false, None);
    assert_eq!(s.pending_history_writes.len(), 1);
    let _ = s.update(Message::Disconnect);
    assert!(s.connected_jid.is_none());
    let close = s.update(Message::WindowCloseRequested(iced::window::Id::unique()));
    assert_eq!(close.units(), 1, "known-account history should flush without reconnecting");
    assert!(s.pending_history_writes.is_empty());
    let reopened = storage::open_history("previous@example", Some("previous@example")).unwrap();
    assert_eq!(reopened.load_tail("alice@example", 10).unwrap()[0].body, "received before disconnect");
}

#[test]
fn deferred_delivery_error_uses_the_authenticated_identity_from_its_original_session() {
    let mut s = state();
    s.connected_jid = Some("profile@alias/device".into());
    s.bound_jid = Some("canonical@old-server/device".into());
    let history = History::open_in_memory();
    let outgoing = history.enqueue_outbox_message("alice@remote", false, "rejected before rebind").unwrap();
    history.disable_outgoing_reads();
    s.history = Some(history);
    let _ = s.update(delivery_error(&outgoing.origin_id, "old-server", "cancel"));
    assert_eq!(s.pending_message_errors.len(), 1);
    s.bound_jid = Some("canonical@new-server/device".into());
    s.history.as_ref().unwrap().enable_outgoing_reads();
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.pending_message_errors.is_empty());
    assert_eq!(s.history.as_ref().unwrap().load_rejected_outbox().unwrap().len(), 1);
}

#[test]
fn detached_delivery_errors_keep_original_authority_and_never_touch_equal_current_ids() {
    for (index, from, rejected) in [(0, "old-server", true), (1, "new-server", false)] {
        let profile = format!("profile{index}@alias");
        let current_profile = format!("current{index}@alias");
        let mut s = state();
        s.connected_jid = Some(format!("{profile}/device"));
        s.bound_jid = Some("canonical@old-server/device".into());
        s.saved_config.jid = Some(profile.clone());
        let old = storage::open_history(&profile, Some(&profile)).unwrap();
        let outgoing = old.enqueue_outbox_message("alice@remote", false, "old account send").unwrap();
        old.disable_outgoing_reads();
        s.history = Some(old);
        let _ = s.update(delivery_error(&outgoing.origin_id, from, "cancel"));
        assert_eq!(s.pending_message_errors.len(), 1);
        let old = s.history.take().unwrap();
        old.enable_outgoing_reads();
        let current = storage::open_history(&current_profile, Some(&profile)).unwrap();
        let current_send = current.enqueue_direct_message("alice@remote", "current", "current account send").unwrap();
        assert_eq!(outgoing.id, current_send.0.id);
        s.connected_jid = Some(format!("{current_profile}/device"));
        s.bound_jid = Some("canonical@new-server/device".into());
        s.saved_config.jid = Some(current_profile.clone());
        s.history = Some(current);
        s.chats[0].messages = vec![room::message::Message::Chat {
            id: current_send.1.id, from: "current".into(), body: "current account send".into(),
            received: current_send.1.received, status: ChatStatus::Queued(current_send.0.id),
        }];
        let _ = s.update(Message::RetryHistoryWrites);
        assert!(s.pending_message_errors.is_empty());
        assert_eq!(old.load_rejected_outbox().unwrap().len(), usize::from(rejected));
        assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 1);
        assert!(matches!(s.chats[0].messages[0], room::message::Message::Chat { status: ChatStatus::Queued(_), .. }));
        assert!(s.rejected_sends.iter().all(|send| send.account.as_deref() == Some(profile.as_str()) && send.saved));
        assert_eq!(s.rejected_sends.len(), usize::from(rejected));
    }
}

#[test]
fn detached_best_effort_failure_is_persisted_after_history_reads_recover() {
    let profile = "previous@alias";
    let mut s = state();
    s.connected_jid = Some(format!("{profile}/device"));
    s.bound_jid = Some("canonical@old-server/device".into());
    s.saved_config.jid = Some(profile.into());
    let old = storage::open_history(profile, Some(profile)).unwrap();
    let origin = uuid::Uuid::new_v4().to_string();
    old.store_unqueued_direct(&origin, "alice@remote", "previous", "best effort send", chrono::Utc::now(), storage::history::UnqueuedDelivery::Submitted).unwrap();
    old.disable_reads();
    s.history = Some(old);
    let _ = s.update(delivery_error(&origin, "old-server", "cancel"));
    assert_eq!(s.pending_message_errors.len(), 1);
    let old = s.history.take().unwrap();
    old.enable_reads();
    s.connected_jid = None;
    s.bound_jid = None;
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.pending_message_errors.is_empty());
    assert_eq!(old.unqueued_direct(&origin).unwrap().unwrap().1.delivery, storage::history::DeliveryState::UnqueuedFailed);
    assert_eq!(s.rejected_sends.len(), 1);
    assert_eq!(s.rejected_sends[0].account.as_deref(), Some(profile));
    assert!(s.rejected_sends[0].saved);
}

#[test]
fn previously_validated_rejection_can_be_saved_after_account_switch() {
    let profile = "previous@alias";
    let mut s = state();
    s.connected_jid = Some(format!("{profile}/device"));
    s.bound_jid = Some("canonical@old-server/device".into());
    let old = storage::open_history(profile, Some(profile)).unwrap();
    let outgoing = old.enqueue_outbox_message("alice@remote", false, "failed save of rejection").unwrap();
    old.disable_writes();
    s.history = Some(old);
    let _ = s.update(delivery_error(&outgoing.origin_id, "old-server", "cancel"));
    assert_eq!(s.rejected_sends.len(), 1);
    assert!(!s.rejected_sends[0].saved);
    let old = s.history.take().unwrap();
    s.connected_jid = Some("current@alias/device".into());
    s.bound_jid = Some("canonical@new-server/device".into());
    s.history = Some(storage::open_history("current@alias", Some(profile)).unwrap());
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.rejected_sends[0].saved);
    assert_eq!(old.load_rejected_outbox().unwrap().len(), 1);
    assert!(s.history.as_ref().unwrap().load_rejected_outbox().unwrap().is_empty());
}

#[test]
fn manual_edit_after_rejected_saved_login_updates_the_saved_password() {
    for (saved_jid, saved_password, manual_jid) in [("invalid login", "old secret", "correct@example"),
        ("empty@example", "", "empty@example")] {
        let mut cfg = storage::SavedConfig::default();
        cfg.jid = Some(saved_jid.into());
        storage::save(&cfg).unwrap();
        storage::save_password(saved_jid, saved_password).unwrap();
        let (mut s, _) = Snack::new();
        assert!(s.state == AppState::Login);
        assert!(s.connect_error.is_some());
        assert!(!s.auto_login_attempt);
        assert!(s.password_input.is_empty());
        let _ = s.update(Message::JidInputChanged(manual_jid.into()));
        let _ = s.update(Message::PasswordInputChanged("edited secret".into()));
        let _ = s.update(Message::Connect);
        assert!(s.state == AppState::Connecting);
        // No subscription runs; model a successful result without a network login.
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Connected));
        assert_eq!(storage::load_password(manual_jid).unwrap().as_deref(), Some("edited secret"));
    }
}

#[test]
fn cancelled_or_failed_auto_login_cannot_carry_a_saved_secret_to_another_account() {
    for failed in [false, true] {
        let jid = if failed { "saved-failure@example" } else { "saved-cancel@example" };
        let mut cfg = storage::SavedConfig::default();
        cfg.jid = Some(jid.into());
        storage::save(&cfg).unwrap();
        storage::save_password(jid, "saved secret").unwrap();
        let (mut s, _) = Snack::new();
        assert!(s.state == AppState::Connecting);
        assert!(s.auto_login_attempt);
        let _ = s.update(if failed { Message::XmppEvent(xmpp::XmppEvent::Disconnected("unavailable".into())) } else { Message::CancelConnect });
        if !failed { complete_worker_stop(&mut s); }
        assert!(s.state == AppState::Login);
        assert!(s.password_input.is_empty());
        assert!(s.pending_save_password.is_none());
        assert!(!s.auto_login_attempt);
        assert_eq!(storage::load_password(jid).unwrap().as_deref(), Some("saved secret"));
        let _ = s.update(Message::JidInputChanged("different@other.example".into()));
        let _ = s.update(Message::Connect);
        assert!(s.state == AppState::Login);
        assert!(s.xmpp_cmd_rx.is_none());
        // Explicitly restoring saved login binds the secret to its original JID.
        let _ = s.update(Message::RetrySavedLogin);
        assert!(s.state == AppState::Connecting);
        assert_eq!(s.connected_jid.as_deref(), Some(jid));
        assert_eq!(s.pending_save_password.as_deref(), Some("saved secret"));
    }
}

#[test]
fn stale_form_events_and_duplicate_submits_do_not_replace_an_inflight_login() {
    let jid = "inflight@example";
    let mut cfg = storage::SavedConfig::default();
    cfg.jid = Some(jid.into());
    storage::save(&cfg).unwrap();
    storage::save_password(jid, "saved secret").unwrap();
    let (mut s, _) = Snack::new();
    let channel = s.xmpp_cmd_rx.clone().unwrap();
    let _ = s.update(Message::JidInputChanged("edited@other.example".into()));
    let _ = s.update(Message::PasswordInputChanged("wrong secret".into()));
    let _ = s.update(Message::Connect);
    assert_eq!(s.xmpp_cmd_rx.as_ref(), Some(&channel));
    assert_eq!(s.connected_jid.as_deref(), Some(jid));
    assert_eq!(s.pending_save_password.as_deref(), Some("saved secret"));
    assert!(s.auto_login_attempt);
    storage::fail_next_password_operation(jid, keyring_core::Error::NoStorageAccess(Box::new(std::io::Error::other("no keyring rewrite expected"))));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Connected));
    assert!(s.state == AppState::Connected);
    assert!(storage::load_password(jid).is_err()); // The injected error was not consumed by a save.
    assert_eq!(storage::load_password(jid).unwrap().as_deref(), Some("saved secret"));
    let _ = s.update(Message::Connect);
    assert!(s.state == AppState::Connected);
    assert_eq!(s.xmpp_cmd_rx.as_ref(), Some(&channel));
}

#[test]
fn cancelling_a_manual_login_retains_the_password_for_correction() {
    let mut s = Snack::test_state();
    let _ = s.update(Message::JidInputChanged("manual@example".into()));
    let _ = s.update(Message::PasswordInputChanged("typed secret".into()));
    let _ = s.update(Message::Connect);
    let _ = s.update(Message::CancelConnect);
    complete_worker_stop(&mut s);
    assert!(s.state == AppState::Login);
    assert_eq!(s.password_input, "typed secret");
    assert!(s.pending_save_password.is_none());
    assert!(!s.auto_login_attempt);
    let _ = s.update(Message::Connect);
    assert_eq!(s.pending_save_password.as_deref(), Some("typed secret"));
}

#[test]
fn selected_message_survives_recovery_from_temporary_to_history_identity() {
    for text_selection in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        s.chats[0].messages.clear();
        let history = History::open_in_memory();
        history.disable_writes();
        s.history = Some(history);
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage {
            from: "alice@example/phone".into(), body: "selected recovery text".into(), timestamp: chrono::Utc::now(),
            delayed: false, stanza_id: Some("selected-recovery".into()), origin_id: None,
        }));
        let room::message::Message::Chat { id: temporary, .. } = s.chats[0].messages[0] else { panic!("chat"); };
        assert!(temporary < 0);
        let cursor = text_editor::Cursor { position: position(8), selection: Some(position(0)) };
        if text_selection {
            s.body_editors.get_mut(&temporary).unwrap().move_to(cursor);
            s.body_drag = Some((temporary, cursor));
        } else {
            let _ = s.update(Message::SelectMessage { id: temporary });
        }
        s.history.as_ref().unwrap().enable_writes();
        let _ = s.update(Message::RetryHistoryWrites);
        let room::message::Message::Chat { id: permanent, .. } = s.chats[0].messages[0] else { panic!("chat"); };
        assert!(permanent > 0);
        assert!(!s.body_editors.contains_key(&temporary));
        if text_selection {
            assert_eq!(s.body_editors[&permanent].selection().as_deref(), Some("selected"));
            assert_eq!(s.body_drag, Some((permanent, cursor)));
        } else {
            assert!(s.selected_msgs.contains(&permanent));
            assert_eq!(s.select_anchor, Some(permanent));
            assert!(s.selected_message_text().unwrap().contains("selected recovery text"));
        }
    }
}

#[test]
fn room_echo_keeps_selection_on_the_confirmed_message() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.history = Some(History::open_in_memory());
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() });
    let _ = s.update(Message::SelectRoom("room@example".into()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.message_input = text_editor::Content::with_text("selected room message");
    let _ = s.update(Message::SendMessage);
    let xmpp::XmppCommand::SendRoomMessage { origin_id: Some(origin), .. } = rx.try_recv().unwrap().into_inner() else { panic!("room send"); };
    let temporary = s.rooms[0].messages.iter().find_map(|m| match m {
        room::message::Message::Chat { id, .. } if *id < 0 => Some(*id), _ => None,
    }).unwrap();
    let _ = s.update(Message::SelectMessage { id: temporary });
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomMessage {
        room: "room@example".into(), nick: "me".into(), body: "selected room message".into(), timestamp: chrono::Utc::now(),
        delayed: false, stanza_id: Some("confirmed-selection".into()), origin_id: Some(origin),
    }));
    let permanent = s.rooms[0].messages.iter().find_map(|m| match m {
        room::message::Message::Chat { id, .. } => Some(*id), _ => None,
    }).unwrap();
    assert!(permanent > 0);
    assert_eq!(s.selected_msgs, [permanent].into_iter().collect());
    assert_eq!(s.select_anchor, Some(permanent));
    assert!(s.selected_message_text().unwrap().contains("selected room message"));
}

#[test]
fn archive_dedup_keeps_selection_from_the_merged_copy() {
    for text_selection in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        s.chats[0].messages.clear();
        s.body_editors.clear();
        s.history = Some(History::open_in_memory());
        let (tx, _rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        s.message_input = text_editor::Content::with_text("selected archive copy");
        let _ = s.update(Message::SendMessage);
        let queued = s.history.as_ref().unwrap().load_outbox().unwrap().remove(0);
        let old = chrono::Utc::now() - chrono::Duration::days(2);
        s.history.as_ref().unwrap().insert("alice@example", "me", "selected archive copy", old, true, Some("merged-copy")).unwrap();
        seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
        s.sync_body_editors();
        let rows = s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap();
        assert_eq!(rows.len(), 2);
        let duplicate = rows.iter().find(|row| row.received.timestamp_millis() == old.timestamp_millis()).unwrap().id;
        let canonical = rows.iter().find(|row| row.id != duplicate).unwrap().id;
        let cursor = text_editor::Cursor { position: position(8), selection: Some(position(0)) };
        if text_selection {
            s.body_editors.get_mut(&duplicate).unwrap().move_to(cursor);
            s.body_drag = Some((duplicate, cursor));
        } else {
            let _ = s.update(Message::SelectMessage { id: duplicate });
        }
        s.mam_pending.insert("archive".into(), crate::app::ArchiveContext { conversation: "alice@example".into(), is_muc: false });
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage { to: None,
            query_id: "archive".into(), from: "me@example/other-device".into(), body: "selected archive copy".into(),
            timestamp: old, stanza_id: Some("merged-copy".into()), origin_id: Some(queued.origin_id),
        }));
        assert_eq!(s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap().len(), 1);
        assert!(!s.body_editors.contains_key(&duplicate));
        if text_selection {
            assert_eq!(s.body_editors[&canonical].selection().as_deref(), Some("selected"));
            assert_eq!(s.body_drag, Some((canonical, cursor)));
        } else {
            assert_eq!(s.selected_msgs, [canonical].into_iter().collect());
            assert_eq!(s.select_anchor, Some(canonical));
            assert!(s.selected_message_text().unwrap().contains("selected archive copy"));
        }
    }
}

#[test]
fn failed_auto_login_preserves_remembered_credentials() {
    let mut s = state();
    s.auto_login_attempt = true;
    s.remember_me = true;
    s.saved_config.jid = Some("alice@example.invalid".into());
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Disconnected("network unavailable".into())));
    assert!(s.remember_me);
    assert_eq!(s.saved_config.jid.as_deref(), Some("alice@example.invalid"));
    assert_eq!(s.connect_error.as_deref(), Some("network unavailable"));
    assert!(!s.auto_login_attempt);
    assert!(s.state == AppState::Login);
}

#[test]
fn terminal_disconnect_preserves_active_unsent_draft() {
    let mut s = state();
    s.message_input = text_editor::Content::with_text("unsent message during reconnect");
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Disconnected("authentication rejected".into())));
    assert!(s.active.is_none());
    assert!(s.chats.is_empty());
    assert_eq!(s.saved_config.drafts["alice@example"], "unsent message during reconnect");
    assert_eq!(storage::load().drafts["alice@example"], "unsent message during reconnect");
}

#[test]
fn outbox_replay_does_not_duplicate_an_in_flight_attempt() {
    let mut s = state();
    s.room_connections.insert("room@example".into(), RoomConnection::Joined);
    let h = History::open_in_memory();
    let id = h.enqueue_outbox("room@example", true, "once per attempt").unwrap();
    s.history = Some(h);
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tx.try_send(xmpp::XmppCommand::ForceReconnect).unwrap();
    s.xmpp_cmd_tx = Some(tx);
    s.resend_outbox(|_| true);
    s.resend_outbox(|_| true);
    assert_eq!(s.xmpp_pending_commands.len(), 1);
    assert_eq!(s.outbox_in_flight.len(), 1);
    rx.try_recv().unwrap().into_inner();
    s.flush_xmpp_commands();
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::SendRoomMessage { id: actual, .. } if actual == id));
    s.resend_outbox(|_| true);
    assert!(rx.try_recv().is_err());
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id, ok: false }));
    assert!(s.outbox_in_flight.is_empty());
    s.resend_outbox(|_| true);
    assert!(matches!(next_send_after_delivery_check(&mut s, &mut rx), xmpp::XmppCommand::SendRoomMessage { id: actual, .. } if actual == id));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id, ok: true }));
    s.resend_outbox(|_| true);
    assert!(rx.try_recv().is_err());
    assert!(s.outbox_in_flight.is_empty());
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
}

#[test]
fn reconnect_allows_durable_messages_to_be_attempted_again() {
    let mut s = state();
    let h = History::open_in_memory();
    let id = h.enqueue_outbox("alice@example", false, "retry after reconnect").unwrap();
    s.history = Some(h);
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    s.xmpp_cmd_tx = Some(tx);
    s.resend_outbox(|_| true);
    rx.try_recv().unwrap().into_inner();
    assert!(s.outbox_in_flight.contains(&id));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Reconnecting));
    assert!(s.outbox_in_flight.is_empty());
    s.resend_outbox(|_| true);
    assert!(matches!(next_send_after_delivery_check(&mut s, &mut rx), xmpp::XmppCommand::SendDirectMessage { id: actual, .. } if actual == id));
}

#[test]
fn reconnect_sends_barrier_before_fresh_room_and_outbox_commands() {
    let mut s = state();
    let h = History::open_in_memory();
    let id = h.enqueue_outbox("alice@example", false, "retry").unwrap();
    s.history = Some(h);
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.joining_room = Some("pending-room@example".into());
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Reconnecting));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Reconnected { generation: 7 }));
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::SessionReady(7)));
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::JoinRoom(room) if room == "pending-room@example"));
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::SendDirectMessage { id: actual, .. } if actual == id));
    assert!(rx.try_recv().is_err());
}

#[test]
fn archive_requests_wait_until_reconnect_finishes() {
    let mut s = state();
    let h = History::open_in_memory();
    h.insert("alice@example", "alice", "old", chrono::Utc::now(), false, None).unwrap();
    s.history = Some(h);
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.reconnecting = true;
    s.fetch_archive_active();
    s.start_catchup("alice@example");
    assert!(rx.try_recv().is_err());
    assert!(s.mam_pending.is_empty());
    assert!(s.mam_catchup.is_empty());
    assert!(!s.chats[0].mam_in_flight);
}

#[test]
fn background_room_joins_preserve_pending_join_and_active_composer() {
    let mut s = state();
    s.joining_room = Some("manual@example".into());
    s.show_join_panel = true;
    s.save_room = true;
    s.message_input = text_editor::Content::with_text("keep typing here");
    s.message_input.move_to(text_editor::Cursor { position: position(3), selection: None });
    let cursor = s.message_input.cursor();
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { own_nick: "me".into(),
        room: "background@example".into(), members: Vec::new(),
    });
    assert_eq!(s.joining_room.as_deref(), Some("manual@example"));
    assert!(s.show_join_panel);
    assert!(s.active == Some(Selection::Chat(0)));
    assert_eq!(s.message_input.text(), "keep typing here");
    assert_eq!(s.message_input.cursor(), cursor);
    assert!(s.saved_config.rooms.is_empty());
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomJoinFailed {
        room: "background@example".into(), reason: "unrelated failure".into(),
    }));
    assert_eq!(s.joining_room.as_deref(), Some("manual@example"));
    assert!(s.join_error.is_none());
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { own_nick: "me".into(),
        room: "manual@example".into(), members: Vec::new(),
    });
    assert!(s.joining_room.is_none());
    assert!(!s.show_join_panel);
    assert!(s.active == Some(Selection::Room(1)));
    assert_eq!(s.saved_config.rooms, vec!["manual@example"]);
    assert_eq!(s.saved_config.drafts["alice@example"], "keep typing here");
    s.message_input = text_editor::Content::with_text("active room composition");
    s.message_input.move_to(text_editor::Cursor { position: position(4), selection: None });
    let cursor = s.message_input.cursor();
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { own_nick: "me".into(),
        room: "manual@example".into(), members: Vec::new(),
    });
    assert_eq!(s.message_input.text(), "active room composition");
    assert_eq!(s.message_input.cursor(), cursor);
}

#[test]
fn history_windows_preserve_pending_and_unsaved_messages() {
    let mut b = chat("room@example", &[]);
    let now = chrono::Utc::now();
    for (id, status) in [(-1, ChatStatus::Sending(-1)), (-2, ChatStatus::Confirmed)] {
        b.append_live(room::message::Message::Chat {
            id, from: "me".into(), body: format!("temporary {id}"), received: now, status,
        });
    }
    let stored = || crate::storage::history::StoredMessage {
        delivery: crate::storage::history::DeliveryState::Received,
        id: 1, sender: "alice".into(), body: "stored".into(),
        received: now - chrono::Duration::seconds(1),
    };
    b.set_window(vec![stored()], false, false);
    assert_eq!(b.messages.len(), 1);
    assert_eq!(b.parked_messages.len(), 2);
    b.seed_tail(vec![stored()], false);
    assert_eq!(b.messages.len(), 3);
    assert!(b.parked_messages.is_empty());
    assert!(matches!(b.messages[1], room::message::Message::Chat { id: -1, status: ChatStatus::Sending(-1), .. }));
    assert!(matches!(b.messages[2], room::message::Message::Chat { id: -2, status: ChatStatus::Confirmed, .. }));
    // Repeated reloads must neither lose nor duplicate the transient entries.
    b.seed_tail(vec![stored()], false);
    assert_eq!(b.messages.len(), 3);
}

#[test]
fn trimming_keeps_unpersisted_messages_without_expanding_rendered_window() {
    let mut b = chat("room@example", &[]);
    let now = chrono::Utc::now();
    for i in 1..=400 {
        b.append_live(room::message::Message::Chat {
            id: -i, from: "me".into(), body: format!("pending {i}"),
            received: now + chrono::Duration::milliseconds(i), status: ChatStatus::Pending(-i),
        });
    }
    assert_eq!(b.messages.len(), backlog::LIVE_TAIL_CAP);
    assert_eq!(b.parked_messages.len(), 100);
    assert!(!b.has_older);
    b.seed_tail(Vec::new(), false);
    assert_eq!(b.messages.len(), backlog::LIVE_TAIL_CAP);
    assert_eq!(b.parked_messages.len(), 100);
}

#[test]
fn server_echo_confirms_oldest_pending_twin_even_when_it_is_parked() {
    let mut s = state();
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { own_nick: "me".into(),
        room: "room@example".into(), members: Vec::new(),
    });
    let now = chrono::Utc::now();
    for i in 1..=2 {
        s.rooms[0].append_live(room::message::Message::Chat {
            id: -i, from: "me".into(), body: "same text".into(),
            received: now + chrono::Duration::milliseconds(i), status: ChatStatus::Pending(-i),
        });
    }
    s.rooms[0].trim_front_to(1);
    assert_eq!(s.rooms[0].parked_messages.len(), 1);
    assert!(s.confirm_pending(0, "me", "same text", 77, now, None, false));
    assert!(s.rooms[0].parked_messages.is_empty());
    assert!(matches!(s.rooms[0].messages[0], room::message::Message::Chat { id: -2, status: ChatStatus::Pending(-2), .. }));
}

#[test]
fn read_boundary_survives_search_windows_missing_rows_and_empty_sentinels() {
    let now = chrono::Utc::now();
    let row = |id: i64| crate::storage::history::StoredMessage {
        id, sender: "alice".into(), body: id.to_string(), received: now + chrono::Duration::seconds(id),
        delivery: crate::storage::history::DeliveryState::Received,
    };
    let mut b = chat("alice@example", &[]);
    b.seed_tail(vec![row(1), row(2)], false);
    b.stamp_read_boundary(); // The next arrival has no visible row yet.
    b.set_window(vec![row(1)], false, false);
    assert_eq!(b.read_marker, Some(1));
    b.seed_tail(vec![row(1), row(2), row(3)], false);
    assert_eq!(b.read_marker, Some(2));
    b.note_unread(3, row(3).received);
    b.set_window(vec![row(1)], false, false);
    b.seed_tail(vec![row(4), row(5)], true); // First unread row is outside this tail.
    assert_eq!(b.read_marker, Some(0));
    b.prepend_older(vec![row(2), row(3)], true);
    assert_eq!(b.read_marker, Some(1)); // Original target is restored, not row 4.
    b.clear_read_boundary();
    b.seed_tail(Vec::new(), false);
    b.stamp_read_boundary();
    b.set_window(Vec::new(), false, false);
    b.note_unread(6, row(6).received); // Received while no visible copy exists.
    b.seed_tail(vec![row(6)], false);
    assert_eq!(b.read_marker, Some(0));
    b.clear_read_boundary();
    b.seed_tail(vec![row(7)], false);
    assert_eq!(b.read_marker, None);
}

#[test]
fn promoting_into_an_existing_saved_copy_refreshes_its_contents_and_read_boundary() {
    for read_id in [-1, 3] {
        let mut b = chat("alice@example", &[]);
        let now = chrono::Utc::now();
        let mut stored = crate::storage::history::StoredMessage {
            id: 3, sender: "me".into(), body: "saved copy".into(), received: now,
            delivery: crate::storage::history::DeliveryState::UnqueuedSubmitted,
        };
        b.messages = vec![
            room::message::Message::Chat { id: 1, from: "alice".into(), body: "neighbour".into(), received: now, status: ChatStatus::Confirmed },
            room::message::Message::Chat { id: -1, from: "me".into(), body: "temporary copy".into(), received: now, status: ChatStatus::Sending(-1) },
            room::message::Message::Chat { id: 3, from: "stale sender".into(), body: "stale copy".into(), received: now, status: ChatStatus::Unconfirmed },
        ];
        b.note_unread(read_id, now);
        stored.received = now - chrono::Duration::days(1);
        b.promote_message(-1, &stored);
        assert_eq!(b.messages.len(), 2);
        assert!(matches!(&b.messages[0], room::message::Message::Chat { id: 3, from, body, received, status: ChatStatus::Confirmed }
            if from == &stored.sender && body == &stored.body && *received == stored.received), "the retained canonical row must be refreshed, read_id={read_id}");
        assert_eq!(b.read_boundary.unwrap().id, Some(3));
        assert_eq!(b.read_boundary.unwrap().received, Some(stored.received));
        assert_eq!(b.read_marker, Some(0));
        assert_eq!(b.oldest_loaded_id, Some(3));
        // When the target leaves the window, its fallback uses the saved stamp.
        b.set_window(vec![crate::storage::history::StoredMessage {
            id: 2, sender: "alice".into(), body: "between stamps".into(), received: now - chrono::Duration::hours(1),
            delivery: crate::storage::history::DeliveryState::Received,
        }], true, false);
        assert_eq!(b.read_marker, Some(0));
    }
}

#[test]
fn unread_identity_follows_promotion_and_duplicate_merges_across_windows() {
    let now = chrono::Utc::now();
    let mut b = chat("alice@example", &[]);
    let _ = b.append_live(room::message::Message::Chat { id: -1, from: "alice".into(), body: "unsaved".into(), received: now, status: ChatStatus::Confirmed });
    b.note_unread(-1, now);
    let stored = crate::storage::history::StoredMessage { id: 10, sender: "alice".into(), body: "unsaved".into(), received: now - chrono::Duration::days(1), delivery: crate::storage::history::DeliveryState::Received };
    b.promote_message(-1, &stored);
    assert_eq!(b.read_boundary.unwrap().id, Some(10));
    b.set_window(Vec::new(), false, false);
    let mut canonical = stored.clone();
    canonical.id = 20;
    b.reconcile_stored(&canonical, &[10]);
    b.seed_tail(vec![canonical], false);
    assert_eq!(b.read_boundary.unwrap().id, Some(20));
    assert_eq!(b.read_marker, Some(0));
}

#[test]
fn leaving_a_search_window_does_not_move_the_last_read_boundary_backward() {
    let mut s = state();
    s.chats[0].stamp_read_boundary();
    let target = s.chats[0].read_boundary.unwrap().id;
    let row = crate::storage::history::StoredMessage {
        id: 1, sender: "alice".into(), body: "alpha".into(), received: chrono::Utc::now(),
        delivery: crate::storage::history::DeliveryState::Received,
    };
    s.chats[0].set_window(vec![row], false, false);
    let _ = s.update(Message::WindowUnfocused);
    assert_eq!(s.chats[0].read_boundary.unwrap().id, target);
    s.chats.push(chat("bob@example", &[]));
    let _ = s.update(Message::SelectChat("bob@example".into()));
    assert_eq!(s.chats[0].read_boundary.unwrap().id, target);
}

#[test]
fn switching_or_unfocusing_an_unread_detached_conversation_preserves_its_boundary() {
    for rooms in [false, true] {
        let mut s = state();
        s.window_focused = true;
        s.connected_jid = Some("me@example/device".into());
        s.history = Some(History::open_in_memory());
        let conv = if rooms { "room@example" } else { "alice@example" };
        if rooms {
            let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: conv.into(), own_nick: "me".into(), members: Vec::new() });
            let _ = s.update(Message::SelectRoom(conv.into()));
            s.rooms[0].at_live_tail = false;
        } else { s.chats[0].messages.clear(); s.chats[0].at_live_tail = false; }
        let now = chrono::Utc::now();
        let event = if rooms {
            xmpp::XmppEvent::RoomMessage { room: conv.into(), nick: "alice".into(), body: "unseen".into(), timestamp: now, delayed: false, stanza_id: Some("unseen".into()), origin_id: None }
        } else {
            xmpp::XmppEvent::DirectMessage { from: "alice@example/phone".into(), body: "unseen".into(), timestamp: now, delayed: false, stanza_id: Some("unseen".into()), origin_id: None }
        };
        let _ = s.update(Message::XmppEvent(event));
        let target = s.active_backlog().unwrap().read_boundary().unwrap().id;
        let _ = s.update(Message::WindowUnfocused);
        assert_eq!(s.active_backlog().unwrap().read_boundary().unwrap().id, target);
        s.chats.push(chat("bob@example", &[]));
        let _ = s.update(Message::SelectChat("bob@example".into()));
        let _ = s.update(if rooms { Message::SelectRoom(conv.into()) } else { Message::SelectChat(conv.into()) });
        assert_eq!(s.active_backlog().unwrap().read_boundary().unwrap().id, target);
        assert_eq!(s.active_backlog().unwrap().read_marker(), Some(0));
    }
}

#[test]
fn unread_marker_follows_its_message_after_tail_reload() {
    let mut b = chat("alice@example", &["read", "unread"]);
    let now = chrono::Utc::now();
    b.read_marker = Some(1);
    let stored = |id, received| crate::storage::history::StoredMessage {
        delivery: crate::storage::history::DeliveryState::Received,
        id, sender: "alice".into(), body: id.to_string(), received,
    };
    b.seed_tail(vec![stored(3, now - chrono::Duration::seconds(1)), stored(1, now), stored(2, now)], false);
    assert_eq!(b.read_marker, Some(2));
}

#[test]
fn failed_archive_storage_preserves_cursor_until_messages_can_be_saved() {
    let mut s = state();
    s.connected_jid = Some("me@example/resource".into());
    let h = History::open_in_memory();
    h.disable_writes();
    s.history = Some(h);
    let timestamp = chrono::Utc::now();
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage {
        from: "alice@example".into(), body: "recover me".into(), timestamp,
        delayed: false, stanza_id: Some("stable-id".into()),
        origin_id: None,
    }));
    assert_eq!(s.pending_history_writes.len(), 1);
    let canonical_time = timestamp - chrono::Duration::seconds(30);
    s.mam_pending.insert("mam-b-0".into(), crate::app::ArchiveContext { conversation: "alice@example".into(), is_muc: false });
    s.chats[0].mam_in_flight = true;
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage { to: None,
        query_id: "mam-b-0".into(), from: "alice@example".into(), body: "recover me".into(),
        timestamp: canonical_time, stanza_id: Some("stable-id".into()),
        origin_id: None,
    }));
    assert_eq!(s.pending_history_writes.len(), 1);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete {
        query_id: "mam-b-0".into(), complete: true, first: Some("older-cursor".into()), last: None,
    }));
    assert!(!s.chats[0].mam_exhausted);
    assert!(!s.chats[0].mam_in_flight);
    assert!(s.chats[0].mam_cursor.is_none());
    s.history.as_ref().unwrap().enable_writes();
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.pending_history_writes.is_empty());
    let saved = s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].body, "recover me");
    assert_eq!(saved[0].received.timestamp_millis(), canonical_time.timestamp_millis());
    assert_eq!(s.chats[0].messages.len(), 1);
    assert!(matches!(s.chats[0].messages[0], room::message::Message::Chat { id, .. } if id == saved[0].id));
}

#[test]
fn failed_history_writes_never_replay_into_another_accounts_database() {
    let mut s = state();
    s.connected_jid = Some("me@one/resource".into());
    let history = storage::open_history("me@one", Some("me@one")).unwrap();
    history.disable_writes();
    s.history = Some(history);
    s.persist_message("alice@example", "alice", "account one only", chrono::Utc::now(), false, None);
    s.connected_jid = Some("me@two/resource".into());
    s.history = Some(storage::open_history("me@two", Some("me@one")).unwrap());
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap().is_empty());
    assert!(s.pending_history_writes.is_empty());
    assert!(matches!(&s.chats[0].messages[0], room::message::Message::Chat { body, .. } if body == "alpha"));
    let original = storage::open_history("me@one", Some("me@one")).unwrap();
    assert_eq!(original.load_tail("alice@example", 10).unwrap()[0].body, "account one only");
}

#[test]
fn detached_confirmation_cannot_change_another_accounts_equal_outbox_id() {
    let old = storage::open_history("old@example", Some("old@example")).unwrap();
    let outgoing = old.enqueue_outbox_message("alice@example", false, "old body").unwrap();
    old.store_direct_message("alice@example", "old", "old body", chrono::Utc::now(), outgoing.id).unwrap();
    let current = storage::open_history("current@example", Some("old@example")).unwrap();
    let new_send = current.enqueue_outbox_message("alice@example", false, "new body").unwrap();
    let new_message = current.store_direct_message("alice@example", "current", "new body", chrono::Utc::now(), new_send.id).unwrap();
    assert_eq!(outgoing.id, new_send.id);
    let mut s = state();
    s.connected_jid = Some("current@example/device".into());
    s.saved_config.jid = Some("current@example/device".into());
    s.history = Some(current);
    s.chats[0].messages = vec![room::message::Message::Chat {
        id: new_message.id, from: "current".into(), body: "new body".into(), received: new_message.received, status: ChatStatus::Queued(new_send.id),
    }];
    s.pending_outbox_deletes.insert((Some("old@example".into()), outgoing.id));
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.pending_outbox_deletes.is_empty());
    assert!(old.load_outbox().unwrap().is_empty());
    assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 1);
    assert!(matches!(s.chats[0].messages[0], room::message::Message::Chat { status: ChatStatus::Queued(id), .. } if id == new_send.id));
}

#[test]
fn recovered_unqueued_direct_history_continues_until_all_batches_are_saved() {
    let mut s = state();
    s.connected_jid = Some("me@example/device".into());
    s.history = Some(History::open_in_memory());
    s.chats[0].messages.clear();
    for index in 0..2 * HISTORY_RETRY_BATCH + 7 {
        let id = -(index as i64 + 1);
        let body = format!("recover direct {index}");
        let received = chrono::Utc::now();
        s.chats[0].append_live(room::message::Message::Chat {
            id, from: "me".into(), body: body.clone(), received, status: ChatStatus::Sending(id),
        });
        s.unqueued_dm_sends.push(crate::app::UnqueuedDmSend {
            account: Some("me@example".into()), command_id: id, origin_id: uuid::Uuid::new_v4().to_string(),
            conversation: "alice@example".into(), sender: "me".into(), body, received, message_id: id,
            state: storage::history::UnqueuedDelivery::Unconfirmed, dirty: true, awaiting_result: true,
        });
    }
    s.history.as_ref().unwrap().disable_writes();
    let blocked = s.update(Message::RetryHistoryWrites);
    assert_eq!(blocked.units(), 0, "a blocked store must not cause rapid retry spinning");
    assert!(s.unqueued_dm_sends.iter().all(|send| send.dirty));
    s.history.as_ref().unwrap().enable_writes();
    let continuation = s.update(Message::RetryHistoryWrites);
    assert_eq!(s.unqueued_dm_sends.iter().filter(|send| send.dirty).count(), 7);
    assert_eq!(s.history.as_ref().unwrap().load_tail("alice@example", 300).unwrap().len(), 2 * HISTORY_RETRY_BATCH);
    assert!(continuation.units() > 0, "successful direct-history batch must schedule its remaining writes");
    let finished = s.update(Message::RetryHistoryWrites);
    assert_eq!(finished.units(), 0);
    assert!(s.unqueued_dm_sends.iter().all(|send| !send.dirty && send.awaiting_result));
    assert_eq!(s.history.as_ref().unwrap().load_tail("alice@example", 300).unwrap().len(), 2 * HISTORY_RETRY_BATCH + 7);
    assert!(s.chats[0].messages.iter().all(|message| matches!(message, room::message::Message::Chat { id, .. } if *id > 0)));
}

#[test]
fn detached_origins_are_stored_before_their_archive_copies_are_reconciled() {
    let mut s = state();
    s.connected_jid = Some("current@example/device".into());
    s.history = Some(storage::open_history("current@example", Some("current@example")).unwrap());
    let origin = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    s.unqueued_dm_sends.push(crate::app::UnqueuedDmSend {
        account: Some("old@example".into()), command_id: -100, origin_id: origin.clone(), conversation: "alice@example".into(),
        sender: "old".into(), body: "same logical send".into(), received: now, message_id: -101,
        state: storage::history::UnqueuedDelivery::Unconfirmed, dirty: true, awaiting_result: false,
    });
    s.pending_history_writes.push(crate::app::PendingHistoryWrite { receipt: uuid::Uuid::new_v4(),
        account: Some("old@example".into()), conversation: "alice@example".into(), sender: "old".into(), body: "same logical send".into(),
        received: now - chrono::Duration::days(1), delayed: true, stanza_id: Some("archive-copy".into()), temp_id: -102,
        outbox_id: None, own_origin: Some(origin), own_room_origin: false, allow_outgoing_fuzzy: false,
    });
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.pending_history_writes.is_empty());
    assert!(s.unqueued_dm_sends.is_empty());
    assert!(s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap().is_empty());
    let old = storage::open_history("old@example", Some("current@example")).unwrap();
    let stored = old.load_tail("alice@example", 10).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].delivery, storage::history::DeliveryState::UnqueuedSubmitted);
    assert_eq!(stored[0].received.timestamp_millis(), (now - chrono::Duration::days(1)).timestamp_millis());
}

#[test]
fn detached_history_recovery_is_batched_and_keeps_writes_when_store_is_unavailable() {
    let mut s = state();
    let legacy = storage::history_path().unwrap();
    std::fs::create_dir_all(&legacy).unwrap(); // Prevent opening the history store.
    for index in 0..HISTORY_RETRY_BATCH + 7 {
        s.pending_history_writes.push(crate::app::PendingHistoryWrite { receipt: uuid::Uuid::new_v4(),
            account: Some("old@example".into()), conversation: "alice@example".into(), sender: "alice".into(), body: format!("old {index}"),
            received: chrono::Utc::now(), delayed: false, stanza_id: Some(format!("old-{index}")), temp_id: -(index as i64 + 1),
            outbox_id: None, own_origin: None, own_room_origin: false, allow_outgoing_fuzzy: false,
        });
    }
    let _ = s.update(Message::RetryHistoryWrites);
    assert_eq!(s.pending_history_writes.len(), HISTORY_RETRY_BATCH + 7);
    std::fs::remove_dir(&legacy).unwrap();
    let task = s.update(Message::RetryHistoryWrites);
    assert!(task.units() > 0, "successful full batch schedules continued recovery");
    assert_eq!(s.pending_history_writes.len(), 7);
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.pending_history_writes.is_empty());
    let old = storage::open_history("old@example", Some("old@example")).unwrap();
    assert_eq!(old.load_tail("alice@example", 1000).unwrap().len(), HISTORY_RETRY_BATCH + 7);
}

#[test]
fn unavailable_history_is_reopened_and_failed_messages_are_saved() {
    let mut s = state();
    s.connected_jid = Some("me@example/resource".into());
    s.persist_message("alice@example", "alice", "saved after reopening", chrono::Utc::now(), false, None);
    assert!(s.history.is_none());
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.pending_history_writes.is_empty());
    assert_eq!(s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap()[0].body, "saved after reopening");
}

#[test]
fn recovery_adopts_canonical_row_when_another_copy_was_already_saved() {
    let mut s = state();
    let h = History::open_in_memory();
    h.disable_writes();
    s.history = Some(h);
    let timestamp = chrono::Utc::now();
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage {
        from: "alice@example".into(), body: "same message".into(), timestamp,
        delayed: false, stanza_id: Some("sid".into()),
        origin_id: None,
    }));
    s.history.as_ref().unwrap().enable_writes();
    let saved_id = s.history.as_ref().unwrap().insert("alice@example", "alice@example", "same message",
        timestamp, true, Some("sid")).unwrap().unwrap();
    s.chats[0].trim_front_to(0);
    assert_eq!(s.chats[0].parked_messages.len(), 1);
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.pending_history_writes.is_empty());
    assert!(s.chats[0].parked_messages.is_empty());
    assert_eq!(s.chats[0].messages.len(), 1);
    assert!(matches!(s.chats[0].messages[0], room::message::Message::Chat { id, .. } if id == saved_id));
    assert_eq!(s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap().len(), 1);
}

#[test]
fn history_recovery_yields_between_bounded_batches() {
    let mut s = state();
    let h = History::open_in_memory();
    h.disable_writes();
    s.history = Some(h);
    let now = chrono::Utc::now();
    for i in 0..HISTORY_RETRY_BATCH + 7 {
        s.persist_message("alice@example", "alice", &format!("message {i}"), now, false, None);
    }
    s.history.as_ref().unwrap().enable_writes();
    let _ = s.update(Message::RetryHistoryWrites);
    assert_eq!(s.pending_history_writes.len(), 7);
    assert_eq!(s.history.as_ref().unwrap().load_tail("alice@example", 1000).unwrap().len(), HISTORY_RETRY_BATCH);
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.pending_history_writes.is_empty());
    assert_eq!(s.history.as_ref().unwrap().load_tail("alice@example", 1000).unwrap().len(), HISTORY_RETRY_BATCH + 7);
}

#[test]
fn archive_only_messages_survive_failed_storage_and_become_visible_on_recovery() {
    let mut s = state();
    let h = History::open_in_memory();
    h.disable_writes();
    s.history = Some(h);
    s.mam_pending.insert("mam-b-0".into(), crate::app::ArchiveContext { conversation: "alice@example".into(), is_muc: false });
    s.chats[0].mam_in_flight = true;
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage { to: None,
        query_id: "mam-b-0".into(), from: "alice@example".into(), body: "archive recovery".into(),
        timestamp: chrono::Utc::now(), stanza_id: Some("archive-only".into()),
        origin_id: None,
    }));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete {
        query_id: "mam-b-0".into(), complete: true, first: Some("cursor".into()), last: None,
    }));
    let _ = s.update(Message::RetryHistoryWrites);
    assert_eq!(s.pending_history_writes.len(), 1);
    assert!(!s.chats[0].mam_exhausted);
    assert!(s.chats[0].mam_cursor.is_none());
    assert!(s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap().is_empty());
    s.history.as_ref().unwrap().enable_writes();
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.pending_history_writes.is_empty());
    assert_eq!(s.chats[0].messages.len(), 1);
    assert!(matches!(&s.chats[0].messages[0], room::message::Message::Chat { body, id, .. }
        if body == "archive recovery" && *id > 0));
}

#[test]
fn failed_writes_keep_distinct_stanza_ids_even_with_identical_content() {
    let mut s = state();
    let h = History::open_in_memory();
    h.disable_writes();
    s.history = Some(h);
    let timestamp = chrono::Utc::now();
    for sid in ["one", "two"] {
        assert!(s.persist_message("alice@example", "alice", "identical", timestamp, false, Some(sid)).is_some());
    }
    assert_eq!(s.pending_history_writes.len(), 2);
    s.history.as_ref().unwrap().enable_writes();
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.pending_history_writes.is_empty());
    assert_eq!(s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap().len(), 2);
}

#[test]
fn login_activates_only_its_own_drafts_and_autojoin_rooms() {
    let mut s = Snack::test_state();
    s.saved_config.draft_account = Some("first@example".into());
    s.saved_config.room_account = Some("first@example".into());
    s.saved_config.drafts.insert("shared@example".into(), "first draft".into());
    s.saved_config.rooms.push("first-room@example".into());
    s.saved_config.account_rooms.insert("second@example".into(), vec!["second-room@example".into()]);
    s.saved_config.account_drafts.insert("second@example".into(), [("shared@example".into(), "second draft".into())].into());
    s.draft_histories.insert("shared@example".into(), Default::default());
    s.jid_input = "second@example/device".into();
    s.password_input = "test password".into();
    let _ = s.update(Message::Connect);
    assert_eq!(s.saved_config.drafts["shared@example"], "second draft");
    assert_eq!(s.saved_config.rooms, vec!["second-room@example"]);
    assert!(s.draft_histories.is_empty());
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Connected));
    let joins = std::iter::from_fn(|| rx.try_recv().ok()).filter_map(|command| match command.into_inner() {
        xmpp::XmppCommand::JoinRoom(room) => Some(room), _ => None,
    }).collect::<Vec<_>>();
    assert_eq!(joins, vec!["second-room@example"]);
    assert_eq!(s.saved_config.account_drafts["first@example"]["shared@example"], "first draft");
    assert_eq!(s.saved_config.account_rooms["first@example"], vec!["first-room@example"]);
}

#[test]
fn invalid_login_address_does_not_change_account_or_open_history() {
    let mut s = state();
    s.saved_config.draft_account = Some("previous@example".into());
    s.saved_config.drafts.insert("alice@example".into(), "previous draft".into());
    let mut invalid: Vec<String> = ["@example", "user@", "user@@example", "user@example/", "user@bad domain", "user@example:5222", "user@[::1]:5222", "user@[invalid]", "user\u{ffff}@example", "user@example/\u{fffe}"].into_iter().map(str::to_string).collect();
    invalid.extend([format!("{}@example", "é".repeat(512)), format!("user@{}", "d".repeat(1024)), format!("user@example/{}", "🙂".repeat(256))]);
    for jid in invalid {
        s.jid_input = jid.clone();
        s.password_input = "test password".into();
        let _ = s.update(Message::Connect);
        assert!(s.connect_error.is_some(), "{jid}");
        assert!(s.state == AppState::Login);
        assert!(s.connected_jid.is_none());
        assert!(s.history.is_none());
        assert!(s.xmpp_cmd_tx.is_none());
        assert_eq!(s.saved_config.draft_account.as_deref(), Some("previous@example"));
        assert_eq!(s.saved_config.drafts["alice@example"], "previous draft");
    }
}

#[test]
fn oversized_recipient_does_not_queue_send_or_clear_the_composer() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.chats[0].jid = format!("alice@example/{}", "🙂".repeat(256));
    s.history = Some(History::open_in_memory());
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.message_input = text_editor::Content::with_text("retain this draft");
    let _ = s.update(Message::SendMessage);
    assert!(s.send_error.is_some());
    assert_eq!(s.message_input.text(), "retain this draft");
    assert_eq!(s.chats[0].messages.len(), 3);
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
    assert!(rx.try_recv().is_err());
}

#[test]
fn malformed_recipients_leave_drafts_conversations_and_outgoing_queue_unchanged() {
    for recipient in ["user@@example", "user@bad domain", "user@example:5222", "user@example/bad\tresource"] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        s.history = Some(History::open_in_memory());
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        s.message_input = text_editor::Content::with_text("keep this draft");
        let _ = s.update(Message::StartChat(recipient.into()));
        assert_eq!(s.chats.len(), 1);
        assert_eq!(s.active, Some(Selection::Chat(0)));
        assert!(s.send_error.is_some());
        s.chats[0].jid = recipient.into();
        let _ = s.update(Message::SendMessage);
        assert_eq!(s.message_input.text(), "keep this draft");
        assert_eq!(s.chats[0].messages.len(), 3);
        assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn equivalent_direct_addresses_reuse_the_existing_chat_and_history_key() {
    for (stored, incoming) in [
        ("alice@example", "alice@EXAMPLE/phone"),
        ("alice@EXAMPLE", "alice@example/laptop"),
        ("alice@[2001:db8::1]", "alice@[2001:0db8:0:0:0:0:0:1]/phone"),
    ] {
        let mut s = state();
        s.chats[0] = chat(stored, &[]);
        s.body_editors.clear();
        s.history = Some(History::open_in_memory());
        s.window_focused = true;
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage {
            from: incoming.into(), body: "same conversation".into(), timestamp: chrono::Utc::now(),
            delayed: false, stanza_id: Some("incoming-id".into()), origin_id: None,
        }));
        assert_eq!(s.chats.len(), 1, "incoming {incoming} split {stored}");
        assert_eq!(s.chats[0].jid, stored);
        assert_eq!(s.chats[0].messages.len(), 1);
        let history = s.history.as_ref().unwrap();
        assert_eq!(history.load_tail(stored, 10).unwrap()[0].body, "same conversation");
        let other_key = incoming.split('/').next().unwrap();
        assert!(history.load_tail(other_key, 10).unwrap().is_empty());
        let _ = s.update(Message::StartChat(incoming.into()));
        assert_eq!(s.chats.len(), 1);
        assert_eq!(s.active, Some(Selection::Chat(0)));
        assert_eq!(s.chats[0].jid, stored);
    }
}

#[test]
fn direct_address_matching_preserves_distinct_peers_and_existing_exact_keys() {
    let mut s = state();
    s.chats[0] = chat("alice@EXAMPLE", &[]);
    s.chats.push(chat("alice@example", &[]));
    s.body_editors.clear();
    let _ = s.update(Message::StartChat("alice@example/phone".into()));
    assert_eq!(s.active, Some(Selection::Chat(1)));
    for peer in ["bob@example", "alice@elsewhere", "example"] {
        let _ = s.update(Message::StartChat(peer.into()));
        assert_eq!(s.chats.last().unwrap().jid, peer);
    }
    assert_eq!(s.chats.len(), 5);
}

#[test]
fn equivalent_room_join_and_retry_actions_keep_the_registered_operation_key() {
    for source in ["failed", "departing", "saved"] {
        let mut s = state();
        s.state = AppState::Connected;
        let (tx, mut rx) = tokio::sync::mpsc::channel(20);
        s.xmpp_cmd_tx = Some(tx);
        match source {
            "failed" => { s.room_connections.insert("room@example".into(), RoomConnection::Failed("try again".into())); }
            "departing" => { s.room_operations.insert("room@example".into(), xmpp::RoomOperation::new()); }
            _ => { s.saved_config.rooms.push("room@example".into()); }
        }
        s.join_input = "room@EXAMPLE".into();
        let _ = s.update(Message::JoinRoom);
        assert_eq!(s.joining_room.as_deref(), Some("room@example"), "{source}");
        assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::JoinRoom(room) if room == "room@example"));
        assert_eq!(s.room_operations.len(), 1);
        assert_eq!(s.room_connections.len(), 1);
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomJoinFailed { room: "room@example".into(), reason: "retry".into() }));
        let _ = s.update(Message::RetryRoomJoin("room@EXAMPLE".into()));
        assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::JoinRoom(room) if room == "room@example"));
        assert_eq!(s.room_operations.len(), 1);
        assert_eq!(s.room_connections.len(), 1);
        let context = s.room_operations["room@example"].id.clone();
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomEvent {
            room: "room@example".into(), context,
            event: Box::new(xmpp::XmppEvent::RoomJoined { room: "room@example".into(), members: Vec::new(), own_nick: "me".into() }),
        }));
        assert!(s.room_is_joined("room@example"));
        assert_eq!(s.rooms.len(), 1);
        assert_eq!(s.rooms[0].jid, "room@example");
    }
}

#[test]
fn queued_room_commands_resolve_aliases_before_replacing_operation_ownership() {
    let mut s = state();
    let (tx, mut rx) = tokio::sync::mpsc::channel(20);
    s.xmpp_cmd_tx = Some(tx);
    assert!(s.send_xmpp_command(xmpp::XmppCommand::JoinRoom("room@example".into())));
    let first = rx.try_recv().unwrap();
    assert!(s.send_xmpp_command(xmpp::XmppCommand::JoinRoom("room@EXAMPLE".into())));
    let second = rx.try_recv().unwrap();
    assert!(!first.is_current());
    assert!(second.is_current());
    assert!(matches!(second.inner(), xmpp::XmppCommand::JoinRoom(room) if room == "room@example"));
    assert!(s.send_xmpp_command(xmpp::XmppCommand::LeaveRoom { room: "room@EXAMPLE".into(), nick: "me".into() }));
    let leave = rx.try_recv().unwrap();
    assert!(!second.is_current());
    assert!(leave.is_current());
    assert!(matches!(leave.inner(), xmpp::XmppCommand::LeaveRoom { room, .. } if room == "room@example"));
    assert_eq!(s.room_operations.len(), 1);
    assert!(s.send_xmpp_command(xmpp::XmppCommand::JoinRoom("other@example".into())));
    assert_eq!(s.room_operations.len(), 2);
    assert!(leave.is_current());
}

#[test]
fn leaving_a_room_removes_all_equivalent_autojoin_preferences() {
    let mut s = completion_state(&[]);
    s.saved_config.rooms = vec!["room@EXAMPLE".into(), "room@example".into(), "other@example".into(), "room@elsewhere".into()];
    let _ = s.update(Message::LeaveRoom);
    assert_eq!(s.saved_config.rooms, ["other@example", "room@elsewhere"]);
    assert_eq!(storage::load().rooms, ["other@example", "room@elsewhere"]);
    assert!(s.rooms.is_empty());
}

#[test]
fn saving_an_existing_membership_does_not_add_an_equivalent_preference() {
    let mut s = completion_state(&[]);
    s.saved_config.rooms = vec!["room@EXAMPLE".into()];
    s.save_room = true;
    s.joining_room = Some("room@example".into());
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() });
    assert_eq!(s.saved_config.rooms, ["room@EXAMPLE"]);
}

#[test]
fn login_and_reconnect_schedule_one_join_per_equivalent_room() {
    for reconnect in [false, true] {
        let mut s = if reconnect { completion_state(&[]) } else { Snack::test_state() };
        s.connected_jid = Some("me@example/device".into());
        s.saved_config.rooms = vec!["room@EXAMPLE".into(), "room@example".into(), "other@example".into()];
        let (tx, mut rx) = tokio::sync::mpsc::channel(20);
        s.xmpp_cmd_tx = Some(tx);
        if reconnect {
            let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Reconnecting));
            let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Reconnected { generation: 1 }));
        } else {
            s.state = AppState::Connecting;
            let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Connected));
        }
        let joins: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|command| matches!(command.inner(), xmpp::XmppCommand::JoinRoom(_))).collect();
        assert_eq!(joins.len(), 2, "reconnect={reconnect}");
        assert!(joins.iter().all(|command| command.is_current()));
        assert_eq!(s.room_connections.len(), 2);
        assert_eq!(s.room_operations.len(), 2);
        if reconnect { assert!(s.room_connections.contains_key("room@example")); }
    }
}

#[test]
fn messages_from_unconfirmed_entries_survive_failure_and_history_write_retries() {
    for fail_writes in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        s.chats[0].messages.clear();
        s.body_editors.clear();
        s.history = Some(History::open_in_memory());
        if fail_writes { s.history.as_ref().unwrap().disable_writes(); }
        let (tx, _rx) = tokio::sync::mpsc::channel(20);
        s.xmpp_cmd_tx = Some(tx);
        assert!(s.send_xmpp_command(xmpp::XmppCommand::JoinRoom("room@example".into())));
        s.joining_room = Some("room@example".into());
        let context = s.room_operations["room@example"].id.clone();
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomEvent {
            room: "room@example".into(), context: context.clone(),
            event: Box::new(xmpp::XmppEvent::RoomMessage { room: "room@example".into(), nick: "alice".into(), body: "received before confirmation".into(), timestamp: chrono::Utc::now(), delayed: false, stanza_id: Some("early-id".into()), origin_id: None }),
        }));
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomEvent {
            room: "room@example".into(), context,
            event: Box::new(xmpp::XmppEvent::RoomJoinFailed { room: "room@example".into(), reason: "entry failed".into() }),
        }));
        assert!(s.rooms.is_empty());
        assert_eq!(s.active, Some(Selection::Chat(0)));
        s.history.as_ref().unwrap().enable_writes();
        s.retry_history_writes();
        let stored = s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].body, "received before confirmation");
        let _ = s.update(Message::RetryRoomJoin("room@example".into()));
        let context = s.room_operations["room@example"].id.clone();
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomEvent {
            room: "room@example".into(), context,
            event: Box::new(xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() }),
        }));
        assert_eq!(s.rooms.len(), 1);
        assert!(s.rooms[0].messages.iter().any(|message| matches!(message, room::message::Message::Chat { body, .. } if body == "received before confirmation")));
    }
}

#[test]
fn room_private_messages_keep_occupants_and_public_history_separate() {
    let mut s = completion_state(&[]);
    s.connected_jid = Some("me@example/device".into());
    s.chats[0].messages.clear();
    s.body_editors.clear();
    s.history = Some(History::open_in_memory());
    let (tx, mut rx) = tokio::sync::mpsc::channel(30);
    s.xmpp_cmd_tx = Some(tx);
    for nick in ["Alice", "Bob", "Alice/phone"] {
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage {
            from: format!("room@example/{nick}"), body: format!("private from {nick}"), timestamp: chrono::Utc::now(), delayed: false, stanza_id: None, origin_id: None,
        }));
    }
    assert!(s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap().is_empty(), "private text entered the public-room history");
    for nick in ["Alice", "Bob", "Alice/phone"] {
        let peer = format!("room@example/{nick}");
        assert_eq!(s.history.as_ref().unwrap().load_tail(&peer, 10).unwrap()[0].body, format!("private from {nick}"));
        let _ = s.update(Message::StartChat(peer.clone()));
        assert_eq!(s.active_conv_jid().as_deref(), Some(peer.as_str()));
        s.message_input = text_editor::Content::with_text("private reply");
        let _ = s.update(Message::SendMessage);
        assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(|command|
            matches!(command.inner(), xmpp::XmppCommand::SendDirectMessage { to, body, .. } if to == &peer && body == "private reply")));
    }
    assert_eq!(s.chats.len(), 4);
    let _ = s.update(Message::SelectRoom("room@example".into()));
    let _ = s.update(Message::LeaveRoom);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomLeft("room@example".into())));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage {
        from: "room@EXAMPLE/Eve".into(), body: "private after leaving".into(), timestamp: chrono::Utc::now(), delayed: false, stanza_id: None, origin_id: None,
    }));
    assert_eq!(s.chats.last().unwrap().jid, "room@example/Eve");
    assert!(s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap().is_empty());
}

#[test]
fn marked_private_room_messages_need_no_open_room_but_account_resources_still_merge() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.chats[0].messages.clear();
    s.body_editors.clear();
    s.history = Some(History::open_in_memory());
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomPrivateMessage {
        from: "unknown@conference.example/Alice".into(), body: "private".into(), timestamp: chrono::Utc::now(), delayed: false, stanza_id: None, origin_id: None,
    }));
    assert_eq!(s.chats[1].jid, "unknown@conference.example/Alice");
    assert!(s.rooms.is_empty());
    assert!(s.room_connections.is_empty());
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage {
        from: "unknown@CONFERENCE.example/Bob".into(), body: "legacy private".into(), timestamp: chrono::Utc::now(), delayed: false, stanza_id: None, origin_id: None,
    }));
    assert_eq!(s.chats[2].jid, "unknown@conference.example/Bob");
    for resource in ["phone", "laptop"] {
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage {
            from: format!("alice@example/{resource}"), body: resource.into(), timestamp: chrono::Utc::now(), delayed: false, stanza_id: None, origin_id: None,
        }));
    }
    assert_eq!(s.chats.len(), 3);
    assert_eq!(s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap().len(), 2);
    assert!(s.history.as_ref().unwrap().load_tail("unknown@conference.example", 10).unwrap().is_empty());
}

#[test]
fn private_room_archive_pages_filter_both_sender_and_outgoing_recipient() {
    let mut s = state();
    s.connected_jid = Some("me@example/device".into());
    s.history = Some(History::open_in_memory());
    let peer = "room@example/Alice";
    s.chats.push(chat(peer, &[]));
    s.mam_pending.insert("private-page".into(), crate::app::ArchiveContext { conversation: peer.into(), is_muc: false });
    for (from, to, body, accepted) in [
        (peer, Some("me@example/phone"), "incoming", true),
        (peer, None, "incoming without recipient", true),
        ("me@example/laptop", Some(peer), "outgoing", true),
        ("room@example/Bob", Some("me@example/device"), "other occupant", false),
        ("me@example/laptop", Some("room@example/Bob"), "to other occupant", false),
        ("me@example/laptop", None, "unknown outgoing peer", false),
        (peer, Some("someoneelse@example/device"), "to another account", false),
    ] {
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage {
            query_id: "private-page".into(), from: from.into(), to: to.map(str::to_string), body: body.into(), timestamp: chrono::Utc::now(), stanza_id: Some(body.into()), origin_id: None,
        }));
        let stored = s.history.as_ref().unwrap().load_tail(peer, 20).unwrap();
        assert_eq!(stored.iter().any(|message| message.body == body), accepted, "{body}");
    }
    assert_eq!(s.history.as_ref().unwrap().load_tail(peer, 20).unwrap().len(), 3);
    assert!(s.history.as_ref().unwrap().load_tail("room@example", 20).unwrap().is_empty());
}

#[test]
fn room_archive_results_keep_their_sender_after_leaving() {
    for catchup in [false, true] {
        let mut s = completion_state(&[]);
        s.connected_jid = Some("me@example/device".into());
        s.chats[0].messages.clear();
        s.body_editors.clear();
        s.history = Some(History::open_in_memory());
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        if catchup {
            s.history.as_ref().unwrap().insert("room@example", "alice", "existing", chrono::Utc::now(), false, None).unwrap();
            s.start_catchup("room@example");
        } else { s.fetch_archive_active(); }
        let query = match rx.try_recv().unwrap().into_inner() {
            xmpp::XmppCommand::QueryArchive { query_id, to: Some(to), with: None, .. } if to == "room@example" => query_id,
            other => panic!("expected room query, got {other:?}"),
        };
        let _ = s.update(Message::LeaveRoom);
        assert!(s.rooms.is_empty());
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage {
            query_id: query, from: "room@example/team/alice".into(), to: None,
            body: "late archive result".into(), timestamp: chrono::Utc::now(), stanza_id: Some("late-room-result".into()), origin_id: None,
        }));
        let stored = s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap();
        let late = stored.iter().find(|message| message.body == "late archive result").unwrap();
        assert_eq!(late.sender, "team/alice", "catchup={catchup}");
        assert!(s.rooms.is_empty());
    }
}

#[test]
fn room_archive_pagination_keeps_the_original_archive_after_leaving() {
    let mut s = completion_state(&[]);
    s.connected_jid = Some("me@example/device".into());
    s.chats[0].messages.clear();
    s.body_editors.clear();
    s.history = Some(History::open_in_memory());
    s.history.as_ref().unwrap().insert("room@example", "alice", "existing", chrono::Utc::now(), false, None).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.start_catchup("room@example");
    let query = match rx.try_recv().unwrap().into_inner() {
        xmpp::XmppCommand::QueryArchive { query_id, .. } => query_id,
        other => panic!("expected query, got {other:?}"),
    };
    let _ = s.update(Message::LeaveRoom);
    while rx.try_recv().is_ok() {}
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete {
        query_id: query, complete: false, first: Some("older".into()), last: None,
    }));
    let next = match rx.try_recv().unwrap().into_inner() {
        xmpp::XmppCommand::QueryArchive { query_id, to: Some(to), with: None, cursor: Some(cursor), .. }
            if to == "room@example" && cursor == "older" => query_id,
        other => panic!("expected another room page, got {other:?}"),
    };
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage {
        query_id: next.clone(), from: "room@example/team/bob".into(), to: None,
        body: "second page".into(), timestamp: chrono::Utc::now(), stanza_id: Some("second-room-page".into()), origin_id: None,
    }));
    let stored = s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap();
    assert_eq!(stored.iter().find(|message| message.body == "second page").unwrap().sender, "team/bob");
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete {
        query_id: next, complete: true, first: Some("oldest".into()), last: None,
    }));
    assert!(s.mam_pending.is_empty());
    assert!(rx.try_recv().is_err());
    assert!(s.rooms.is_empty());
}

#[test]
fn closed_history_cannot_start_a_new_direct_archive_sweep() {
    let mut s = state();
    s.chats[0].messages.clear();
    s.body_editors.clear();
    s.history = Some(History::open_in_memory());
    s.history.as_ref().unwrap().insert("closed-room@example", "alice", "saved after departure", chrono::Utc::now(), false, None).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.start_catchup("closed-room@example");
    assert!(rx.try_recv().is_err());
    assert!(s.mam_pending.is_empty());
    assert!(!s.mam_catchup.contains_key("closed-room@example"));
}

#[test]
fn room_archive_rejects_foreign_and_malformed_forwarded_addresses() {
    for leave in [false, true] {
        let mut s = completion_state(&[]);
        s.connected_jid = Some("me@example/device".into());
        s.chats[0].messages.clear();
        s.body_editors.clear();
        s.history = Some(History::open_in_memory());
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        s.fetch_archive_active();
        let query = match rx.try_recv().unwrap().into_inner() {
            xmpp::XmppCommand::QueryArchive { query_id, .. } => query_id,
            other => panic!("expected room query, got {other:?}"),
        };
        if leave { let _ = s.update(Message::LeaveRoom); }
        let long = format!("room@example/{}", "a".repeat(1024));
        for (i, (from, to, accepted)) in [
            ("room@EXAMPLE/team/alice", None, true),
            ("other@example/alice", None, false),
            ("me@example/device", None, false),
            ("room@example/", None, false),
            ("room@@example/alice", None, false),
            (long.as_str(), None, false),
            ("room@example/alice", Some("me@example/"), false),
            ("room@example/alice", Some("me@example/device"), true),
            ("room@example", None, true),
        ].into_iter().enumerate() {
            let body = format!("result {i}");
            let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage {
                query_id: query.clone(), from: from.into(), to: to.map(str::to_string),
                body: body.clone(), timestamp: chrono::Utc::now(), stanza_id: Some(body.clone()), origin_id: None,
            }));
            let stored = s.history.as_ref().unwrap().load_tail("room@example", 20).unwrap();
            assert_eq!(stored.iter().any(|message| message.body == body), accepted, "leave={leave}, from={from}, to={to:?}");
        }
        assert_eq!(s.rooms.is_empty(), leave);
    }
}

#[test]
fn direct_archive_results_must_belong_to_the_requested_peer() {
    let mut s = state();
    s.connected_jid = Some("profile@alias/device".into());
    s.bound_jid = Some("me@EXAMPLE/device".into());
    s.chats[0].messages.clear();
    s.body_editors.clear();
    s.history = Some(History::open_in_memory());
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.fetch_archive_active();
    let query = match rx.try_recv().unwrap().into_inner() {
        xmpp::XmppCommand::QueryArchive { query_id, to: None, with: Some(peer), .. } if peer == "alice@example" => query_id,
        other => panic!("expected account archive query, got {other:?}"),
    };
    for (i, (from, to, accepted)) in [
        ("alice@EXAMPLE/phone", Some("me@example/old-device"), true),
        ("alice@example/laptop", None, true),
        ("mallory@example/phone", Some("me@example/device"), false),
        ("alice@example/phone", Some("other@example/device"), false),
        ("me@example/other-device", Some("bob@example/phone"), false),
        ("me@example/other-device", Some("alice@EXAMPLE/phone"), true),
        ("me@example/other-device", None, true),
        ("Alice@example/phone", Some("me@example/device"), false),
        ("profile@alias/device", Some("alice@example"), false),
    ].into_iter().enumerate() {
        let body = format!("direct result {i}");
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage {
            query_id: query.clone(), from: from.into(), to: to.map(str::to_string),
            body: body.clone(), timestamp: chrono::Utc::now(), stanza_id: Some(body.clone()), origin_id: None,
        }));
        let stored = s.history.as_ref().unwrap().load_tail("alice@example", 20).unwrap();
        assert_eq!(stored.iter().any(|message| message.body == body), accepted, "from={from}, to={to:?}");
    }
}

#[test]
fn direct_archive_to_another_peer_cannot_retire_an_outgoing_message() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.chats[0].messages.clear();
    s.body_editors.clear();
    s.history = Some(History::open_in_memory());
    let (tx, _rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.message_input = text_editor::Content::with_text("pending to alice");
    let _ = s.update(Message::SendMessage);
    let queued = s.history.as_ref().unwrap().load_outbox().unwrap().remove(0);
    s.mam_pending.insert("alice-archive".into(), crate::app::ArchiveContext { conversation: "alice@example".into(), is_muc: false });
    for peer in ["bob@example/phone", "alice@example/laptop"] {
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage {
            query_id: "alice-archive".into(), from: "me@example/another-device".into(), to: Some(peer.into()),
            body: "pending to alice".into(), timestamp: chrono::Utc::now(), stanza_id: Some(peer.into()), origin_id: Some(queued.origin_id.clone()),
        }));
        let history = s.history.as_ref().unwrap();
        assert_eq!(history.load_outbox().unwrap().is_empty(), peer.starts_with("alice@"));
        assert_eq!(history.load_tail("alice@example", 10).unwrap().len(), 1);
    }
}

#[test]
fn room_privacy_is_scoped_to_current_entry_and_reset_on_rejoin() {
    let mut s = completion_state(&[]);
    let operation = xmpp::RoomOperation::new();
    let context = operation.id.clone();
    s.room_operations.insert("room@example".into(), operation);
    let privacy = ::xmpp::RoomPrivacy { publicly_logged: Some(true), non_anonymous: Some(true) };
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomEvent {
        room: "room@example".into(), context, event: Box::new(xmpp::XmppEvent::RoomPrivacy { room: "room@example".into(), privacy }),
    }));
    assert_eq!(s.rooms[0].privacy, privacy);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomEvent {
        room: "room@example".into(), context: "obsolete".into(), event: Box::new(xmpp::XmppEvent::RoomPrivacy { room: "room@example".into(), privacy: Default::default() }),
    }));
    assert_eq!(s.rooms[0].privacy, privacy);
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() });
    assert_eq!(s.rooms[0].privacy, ::xmpp::RoomPrivacy::default());
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomPrivacy { room: "absent@example".into(), privacy }));
    assert_eq!(s.rooms.len(), 1);
}

#[test]
fn room_privacy_notices_stay_visible_in_the_header_at_narrow_widths() {
    use iced::advanced::widget::operation::Operation;
    #[derive(Default)]
    struct Notices(Vec<(String, iced::Rectangle)>);
    impl Operation for Notices {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) {
            if text.starts_with("This room ") { self.0.push((text.into(), bounds)); }
        }
    }
    let mut s = completion_state(&[]);
    for (logged, exposed, count) in [(None, None, 0), (Some(true), None, 1), (Some(true), Some(true), 2), (Some(false), Some(false), 0)] {
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomPrivacy { room: "room@example".into(), privacy: ::xmpp::RoomPrivacy { publicly_logged: logged, non_anonymous: exposed } }));
        for width in [280.0, 600.0] {
            let mut widget = crate::ui::chat::view(&s);
            let mut tree = Tree::new(&widget);
            let r = renderer();
            let size = iced::Size::new(width, 600.0);
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut notices = Notices::default();
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut notices);
            assert_eq!(notices.0.len(), count);
            for (_, bounds) in notices.0 { assert!(bounds.height > 0.0 && bounds.x >= 0.0 && bounds.x + bounds.width <= width); }
        }
    }
}

#[test]
fn long_room_topics_leave_the_composer_and_transcript_usable() {
    use iced::advanced::widget::operation::{Operation, Focusable, Scrollable};
    #[derive(Default)]
    struct Bounds { composer: Option<iced::Rectangle>, transcript: Option<iced::Rectangle>, topic: Option<(iced::Rectangle, iced::Rectangle)>, text: Vec<(String, iced::Rectangle)> }
    impl Operation for Bounds {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn focusable(&mut self, id: Option<&iced::widget::Id>, bounds: iced::Rectangle, _state: &mut dyn Focusable) {
            if id == Some(&iced::widget::Id::new(MESSAGE_INPUT_ID)) { self.composer = Some(bounds); }
        }
        fn scrollable(&mut self, id: Option<&iced::widget::Id>, bounds: iced::Rectangle, content: iced::Rectangle, _translation: iced::Vector, _state: &mut dyn Scrollable) {
            if id == Some(&iced::widget::Id::new(crate::app::MESSAGE_SCROLL_ID)) { self.transcript = Some(bounds); }
            if id.is_none() { self.topic = Some((bounds, content)); }
        }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) { self.text.push((text.into(), bounds)); }
    }
    let mut s = completion_state(&[]);
    for topic in ["word ".repeat(1500), "界".repeat(3000), "line\n".repeat(150)] {
        s.rooms[0].topic = topic;
        for warned in [false, true] {
            s.rooms[0].privacy = ::xmpp::RoomPrivacy { publicly_logged: warned.then_some(true), non_anonymous: warned.then_some(true) };
            for (width, height) in [(280.0, 300.0), (600.0, 300.0), (600.0, 600.0)] {
                let mut widget = crate::ui::chat::view(&s);
                let mut tree = Tree::new(&widget);
                let r = renderer();
                let size = iced::Size::new(width, height);
                let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
                let mut bounds = Bounds::default();
                widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut bounds);
                let input = bounds.composer.unwrap();
                let transcript = bounds.transcript.unwrap();
                assert!(input.height >= 35.0, "composer collapsed: {input:?}, size={size:?}, warned={warned}");
                assert!(input.y + input.height <= height, "composer outside window: {input:?}");
                assert!(transcript.height >= 30.0, "transcript collapsed: {transcript:?}");
                assert!(transcript.y + transcript.height <= input.y, "transcript overlaps composer");
                let (viewport, content) = bounds.topic.unwrap();
                assert!(viewport.height <= 64.0 && content.height > viewport.height);
                assert!(bounds.text.iter().any(|(text, _)| text == &s.rooms[0].topic), "full topic was lost");
                assert!(bounds.text.iter().any(|(text, rect)| text == "Leave" && rect.y + rect.height <= input.y));
                let notices: Vec<_> = bounds.text.iter().filter(|(text, _)| text.starts_with("This room ")).collect();
                assert_eq!(notices.len(), if warned { 2 } else { 0 });
                for (_, rect) in notices { assert!(rect.y + rect.height <= input.y && rect.x + rect.width <= width); }
            }
        }
    }
}

#[test]
fn drawn_room_mentions_follow_the_actual_nickname_and_renames() {
    fn highlights(s: &Snack) -> usize {
        let mut widget = crate::ui::chat::view(s);
        let mut tree = Tree::new(&widget);
        let mut r = renderer();
        let size = iced::Size::new(600.0, 400.0);
        let viewport = iced::Rectangle::new(iced::Point::ORIGIN, size);
        let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
        widget.as_widget().draw(&tree, &mut r, &iced::Theme::Dark,
            &iced::advanced::renderer::Style { text_color: iced::Color::WHITE },
            Layout::new(&node), iced::mouse::Cursor::Unavailable, &viewport);
        let expected = crate::ui::style::mention_highlight(&iced::Theme::Dark).background.unwrap();
        let iced::Renderer::Secondary(renderer) = &mut r else { panic!("test requires software renderer"); };
        renderer.layers().iter().flat_map(|layer| &layer.quads).filter(|(_, background)| *background == expected).count()
    }
    let mut s = completion_state(&[]);
    s.connected_jid = Some("account@example/device".into());
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined {
        room: "room@example".into(), own_nick: "AssignedNick".into(), members: Vec::new(),
    });
    for renamed in [false, true] {
        if renamed {
            let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::MemberNicknameChanged {
                room: "room@example".into(), old_nick: "AssignedNick".into(), new_nick: "NewNick".into(), is_self: true,
            }));
        }
        for (body, expected) in [("hello AssignedNick", !renamed), ("hello NewNick", renamed), ("hello account", false), ("ordinary text", false)] {
            s.rooms[0].messages = vec![room::message::Message::Chat { id: 11, from: "alice".into(), body: body.into(), received: chrono::Utc::now(), status: ChatStatus::Confirmed }];
            s.sync_body_editors();
            assert_eq!(highlights(&s), usize::from(expected), "renamed={renamed}, body={body}");
        }
    }
    // Direct chats keep their existing account-name mention behavior.
    s.active = Some(Selection::Chat(0));
    s.chats[0] = chat("alice@example", &["hello account"]);
    s.sync_body_editors();
    assert_eq!(highlights(&s), 1);
}

#[test]
fn room_service_history_deduplicates_without_confirming_occupant_sends() {
    let mut s = completion_state(&[]);
    s.connected_jid = Some("me@example/device".into());
    s.window_focused = true;
    s.chats[0].messages.clear();
    s.body_editors.clear();
    s.history = Some(History::open_in_memory());
    let (tx, _rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    // This is a valid nickname; using the bare room JID as a display/storage
    // sender would falsely make service text look like our own echo.
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "room@example".into(), members: Vec::new() });
    s.message_input = text_editor::Content::with_text("same body");
    let _ = s.update(Message::SendMessage);
    let queued = s.history.as_ref().unwrap().load_outbox().unwrap().remove(0);
    let timestamp = chrono::Utc::now();
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomMessage {
        room: "room@example".into(), nick: String::new(), body: "same body".into(), timestamp, delayed: false,
        stanza_id: Some("service-id".into()), origin_id: Some(queued.origin_id.clone()),
    }));
    s.mam_pending.insert("service-archive".into(), crate::app::ArchiveContext { conversation: "room@example".into(), is_muc: true });
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage {
        query_id: "service-archive".into(), from: "room@EXAMPLE".into(), to: None,
        body: "same body".into(), timestamp, stanza_id: Some("service-id".into()), origin_id: Some(queued.origin_id.clone()),
    }));
    let stored = s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 1);
    let service = stored.iter().find(|message| message.sender.is_empty()).unwrap();
    assert!(s.rooms[0].messages.iter().any(|message| matches!(message,
        room::message::Message::Chat { from, status: ChatStatus::Sending(_), .. } if from == "room@example")));
    s.selected_msgs.insert(service.id);
    assert!(s.selected_message_text().unwrap().contains("] Room service: same body"));
    {
        use iced::advanced::widget::operation::Operation;
        #[derive(Default)]
        struct Labels(Vec<String>);
        impl Operation for Labels {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
            fn text(&mut self, _id: Option<&iced::widget::Id>, _bounds: iced::Rectangle, text: &str) { self.0.push(text.into()); }
        }
        let mut widget = crate::ui::chat::view(&s);
        let mut tree = Tree::new(&widget);
        let r = renderer();
        let size = iced::Size::new(600.0, 400.0);
        let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
        let mut labels = Labels::default();
        widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut labels);
        assert!(labels.0.iter().any(|label| label == "Room service: "));
        assert!(labels.0.iter().any(|label| label == "room@example: "));
    }
    let _ = s.update(Message::QuoteMessage { id: service.id });
    assert_eq!(s.message_input.text(), "> same body\n");
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomMessage {
        room: "room@example".into(), nick: "room@example".into(), body: "same body".into(), timestamp, delayed: false,
        stanza_id: Some("own-real-echo".into()), origin_id: Some(queued.origin_id),
    }));
    let stored = s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap();
    assert_eq!(stored.len(), 2);
    assert!(stored.iter().any(|message| message.sender.is_empty()));
    assert!(stored.iter().any(|message| message.sender == "room@example"));
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
    let _ = s.update(Message::LeaveRoom);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomMessage {
        room: "room@example".into(), nick: String::new(), body: "late service".into(), timestamp, delayed: false, stanza_id: Some("late-service".into()), origin_id: None,
    }));
    assert!(s.rooms.is_empty());
    assert!(s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap().iter().any(|message| message.sender.is_empty() && message.body == "late service"));
}

#[test]
fn saved_room_service_notices_before_join_do_not_open_direct_chats() {
    let mut s = state();
    s.connected_jid = Some("me@example/device".into());
    s.chats[0].messages.clear();
    s.body_editors.clear();
    s.saved_config.rooms.push("room@example".into());
    s.history = Some(History::open_in_memory());
    for (from, body) in [("room@EXAMPLE", "offline service"), ("alice@example", "ordinary contact")] {
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage {
            from: from.into(), body: body.into(), timestamp: chrono::Utc::now(), delayed: true, stanza_id: Some(body.into()), origin_id: None,
        }));
    }
    assert!(s.rooms.is_empty());
    assert_eq!(s.chats.len(), 1);
    let service = s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap();
    assert_eq!(service.len(), 1);
    assert!(service[0].sender.is_empty());
    assert_eq!(service[0].body, "offline service");
    assert!(s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap().iter().all(|message| !message.sender.is_empty()));
}

#[test]
fn live_self_chat_echoes_reconcile_exact_origins_without_duplicate_messages() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("profile@example/device".into());
    s.bound_jid = Some("me@example/device".into());
    s.window_focused = true;
    s.chats[0].messages.clear();
    s.body_editors.clear();
    s.history = Some(History::open_in_memory());
    let (tx, _rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    let _ = s.update(Message::StartChat("me@example".into()));
    for _ in 0..2 {
        s.message_input = text_editor::Content::with_text("same self note");
        let _ = s.update(Message::SendMessage);
    }
    let queued = s.history.as_ref().unwrap().load_outbox().unwrap();
    assert_eq!(queued.len(), 2);
    let original = s.history.as_ref().unwrap().load_tail("me@example", 10).unwrap();
    for (index, origin, sid) in [(1, queued[1].origin_id.clone(), "second"), (1, queued[1].origin_id.clone(), "second-repeated"), (0, queued[0].origin_id.clone(), "first")] {
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage {
            from: "me@EXAMPLE/other-resource".into(), body: "same self note".into(), timestamp: chrono::Utc::now(), delayed: false,
            stanza_id: Some(sid.into()), origin_id: Some(origin),
        }));
        let history = s.history.as_ref().unwrap();
        let stored = history.load_tail("me@example", 10).unwrap();
        assert_eq!(stored.len(), 2);
        for message in &stored {
            assert_eq!(message.sender, "profile");
            assert_eq!(message.received, original.iter().find(|old| old.id == message.id).unwrap().received);
        }
        assert_eq!(history.load_outbox().unwrap().len(), index);
        let chat = s.chats.iter().find(|chat| chat.jid == "me@example").unwrap();
        assert_eq!(chat.messages.len(), 2);
        assert!(!chat.unread);
    }
}

#[test]
fn self_chat_copy_authority_does_not_cross_senders_or_conversations() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.window_focused = true;
    s.chats[0].messages.clear(); s.body_editors.clear();
    s.history = Some(History::open_in_memory());
    let (tx, _rx) = tokio::sync::mpsc::channel(10); s.xmpp_cmd_tx = Some(tx);
    let _ = s.update(Message::StartChat("me@example".into()));
    s.message_input = text_editor::Content::with_text("self note");
    let _ = s.update(Message::SendMessage);
    let self_origin = s.history.as_ref().unwrap().load_outbox().unwrap()[0].origin_id.clone();
    let _ = s.update(Message::StartChat("alice@example".into()));
    s.message_input = text_editor::Content::with_text("to alice");
    let _ = s.update(Message::SendMessage);
    let alice_origin = s.history.as_ref().unwrap().load_outbox().unwrap().iter().find(|outgoing| outgoing.conversation == "alice@example").unwrap().origin_id.clone();
    for (from, body, origin, private) in [
        ("mallory@example/phone", "self note", self_origin.as_str(), false),
        ("me@example/phone", "to alice", alice_origin.as_str(), false),
        ("me@example/phone", "changed body", self_origin.as_str(), false),
        ("me@example/phone", "self note", self_origin.as_str(), true),
    ] {
        let event = if private { xmpp::XmppEvent::RoomPrivateMessage { from: from.into(), body: body.into(), timestamp: chrono::Utc::now(), delayed: false, stanza_id: None, origin_id: Some(origin.into()) } }
            else { xmpp::XmppEvent::DirectMessage { from: from.into(), body: body.into(), timestamp: chrono::Utc::now(), delayed: false, stanza_id: None, origin_id: Some(origin.into()) } };
        let _ = s.update(Message::XmppEvent(event));
        assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 2);
    }
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage { from: "me@example/phone".into(), body: "self note".into(), timestamp: chrono::Utc::now(), delayed: false, stanza_id: Some("self-copy".into()), origin_id: Some(self_origin) }));
    let remaining = s.history.as_ref().unwrap().load_outbox().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].conversation, "alice@example");
}

#[test]
fn self_chat_copies_survive_history_outages_without_hiding_new_notes() {
    for recorded in [false, true] {
        for unreadable in [false, true] {
            let mut s = state(); s.state = AppState::Connected;
            s.connected_jid = Some("me@example/device".into()); s.window_focused = true;
            s.chats[0].messages.clear(); s.body_editors.clear();
            s.history = Some(History::open_in_memory());
            let (tx, _rx) = tokio::sync::mpsc::channel(10); s.xmpp_cmd_tx = Some(tx);
            let _ = s.update(Message::StartChat("me@example".into()));
            let origin = if recorded {
                s.message_input = text_editor::Content::with_text("self note");
                let _ = s.update(Message::SendMessage);
                s.history.as_ref().unwrap().load_outbox().unwrap()[0].origin_id.clone()
            } else { "remote-device-note".into() };
            if unreadable { s.history.as_ref().unwrap().disable_reads(); }
            s.history.as_ref().unwrap().disable_writes();
            for _ in 0..2 {
                let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage { from: "me@example/phone".into(), body: "self note".into(), timestamp: chrono::Utc::now(), delayed: false, stanza_id: Some("self-copy".into()), origin_id: Some(origin.clone()) }));
                let chat = s.chats.iter().find(|chat| chat.jid == "me@example").unwrap();
                assert_eq!(chat.messages.len(), 1, "recorded={recorded}, unreadable={unreadable}");
            }
            assert_eq!(s.pending_history_writes.len(), 1);
            s.history.as_ref().unwrap().enable_writes();
            if unreadable { s.history.as_ref().unwrap().enable_reads(); }
            let _ = s.retry_history_writes();
            assert!(s.pending_history_writes.is_empty());
            assert_eq!(s.history.as_ref().unwrap().load_tail("me@example", 10).unwrap().len(), 1);
            assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
            assert_eq!(s.chats.iter().find(|chat| chat.jid == "me@example").unwrap().messages.len(), 1);
        }
    }
}

#[test]
fn best_effort_self_chat_echo_is_submitted_without_a_duplicate_or_late_downgrade() {
    let mut s = state(); s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into()); s.window_focused = true;
    s.chats[0].messages.clear(); s.body_editors.clear();
    s.history = Some(History::open_in_memory());
    let (tx, mut rx) = tokio::sync::mpsc::channel(10); s.xmpp_cmd_tx = Some(tx);
    let _ = s.update(Message::StartChat("me@example".into()));
    s.history.as_ref().unwrap().disable_outbox_reads();
    s.message_input = text_editor::Content::with_text("best effort self note");
    let _ = s.update(Message::SendMessage);
    let (command, origin) = match rx.try_recv().unwrap().into_inner() {
        xmpp::XmppCommand::SendDirectMessage { id, origin_id: Some(origin), .. } => (id, origin),
        other => panic!("expected self send, got {other:?}"),
    };
    s.history.as_ref().unwrap().enable_outbox_reads();
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage { from: "me@example/phone".into(), body: "best effort self note".into(), timestamp: chrono::Utc::now(), delayed: false, stanza_id: Some("self-copy".into()), origin_id: Some(origin) }));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id: command, ok: false }));
    let stored = s.history.as_ref().unwrap().load_tail("me@example", 10).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].delivery, crate::storage::history::DeliveryState::UnqueuedSubmitted);
    assert_eq!(s.chats.iter().find(|chat| chat.jid == "me@example").unwrap().messages.len(), 1);
}

#[test]
fn self_chat_origin_confirmation_survives_a_conflicting_server_id() {
    for outage in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        s.window_focused = true;
        s.history = Some(History::open_in_memory());
        let (tx, _rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        let _ = s.update(Message::StartChat("me@example".into()));
        s.message_input = text_editor::Content::with_text("self note");
        let _ = s.update(Message::SendMessage);
        let origin = s.history.as_ref().unwrap().load_outbox().unwrap()[0].origin_id.clone();
        let unrelated = s.history.as_ref().unwrap().insert("me@example", "old sender", "unrelated note",
            chrono::Utc::now(), false, Some("conflicting-id")).unwrap().unwrap();
        if outage { s.history.as_ref().unwrap().disable_writes(); }
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage {
            from: "me@example/phone".into(), body: "self note".into(), timestamp: chrono::Utc::now(),
            delayed: false, stanza_id: Some("conflicting-id".into()), origin_id: Some(origin),
        }));
        if outage { s.history.as_ref().unwrap().enable_writes(); }
        let _ = s.retry_history_writes();
        assert!(s.pending_history_writes.is_empty(), "outage={outage}");
        let history = s.history.as_ref().unwrap();
        assert!(history.load_outbox().unwrap().is_empty());
        let rows = history.load_tail("me@example", 10).unwrap();
        assert_eq!(rows.len(), 2);
        let preserved = rows.iter().find(|row| row.id == unrelated).unwrap();
        assert_eq!(preserved.sender, "old sender");
        assert_eq!(preserved.body, "unrelated note");
    }
}

#[test]
fn room_origin_confirmation_preserves_conflicting_history_and_recovers_after_outage() {
    for outage in [false, true] {
        for queued_conflict in [false, true] {
            let mut s = state();
            s.state = AppState::Connected;
            s.connected_jid = Some("me@example/device".into());
            s.history = Some(History::open_in_memory());
            let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined {
                room: "room@example".into(), own_nick: "me".into(), members: Vec::new(),
            });
            let _ = s.update(Message::SelectRoom("room@example".into()));
            let (tx, mut rx) = tokio::sync::mpsc::channel(10);
            s.xmpp_cmd_tx = Some(tx);
            s.message_input = text_editor::Content::with_text("room note");
            let _ = s.update(Message::SendMessage);
            let xmpp::XmppCommand::SendRoomMessage { id, origin_id: Some(origin), .. } = rx.try_recv().unwrap().into_inner()
                else { panic!("room send"); };
            let history = s.history.as_ref().unwrap();
            let other = if queued_conflict {
                let other = history.enqueue_outbox_message("room@example", true, "room note").unwrap();
                history.reconcile_own_room_origin("room@example", "me", "room note", &other.origin_id,
                    chrono::Utc::now(), Some("conflicting-id")).unwrap().unwrap().0.id
            } else {
                history.insert("room@example", "someone else", "unrelated note", chrono::Utc::now(),
                    false, Some("conflicting-id")).unwrap().unwrap()
            };
            let before = history.load_tail("room@example", 10).unwrap().into_iter().find(|row| row.id == other).unwrap();
            if outage { history.disable_writes(); }
            let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomMessage {
                room: "room@example".into(), nick: "me".into(), body: "room note".into(),
                timestamp: chrono::Utc::now(), delayed: false, stanza_id: Some("conflicting-id".into()), origin_id: Some(origin),
            }));
            if outage {
                let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::MemberNicknameChanged {
                    room: "room@example".into(), old_nick: "me".into(), new_nick: "new me".into(), is_self: true,
                }));
                // A later ordinary write must not remain blocked behind this copy.
                let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomMessage {
                    room: "room@example".into(), nick: "someone else".into(), body: "later note".into(),
                    timestamp: chrono::Utc::now(), delayed: false, stanza_id: Some("later-id".into()), origin_id: None,
                }));
                s.history.as_ref().unwrap().enable_writes();
            }
            let _ = s.retry_history_writes();
            assert!(s.pending_history_writes.is_empty(), "outage={outage}, queued_conflict={queued_conflict}");
            let history = s.history.as_ref().unwrap();
            assert!(history.load_outbox().unwrap().is_empty());
            let rows = history.load_tail("room@example", 10).unwrap();
            assert_eq!(rows.len(), if outage { 3 } else { 2 });
            let preserved = rows.iter().find(|row| row.id == other).unwrap();
            assert_eq!(preserved.sender, before.sender);
            assert_eq!(preserved.body, before.body);
            assert_eq!(preserved.delivery, before.delivery);
            assert_eq!(preserved.received, before.received);
            assert_eq!(rows.iter().filter(|row| row.delivery == crate::storage::history::DeliveryState::Submitted(id)).count(), 1);
            assert_eq!(s.rooms[0].messages.iter().filter(|message| matches!(message,
                room::message::Message::Chat { status: ChatStatus::Submitted(link), .. } if *link == id)).count(), 1);
        }
    }
}

#[test]
fn unsaved_own_room_copies_keep_distinct_bodies_and_one_row_per_copy() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.history = Some(History::open_in_memory());
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined {
        room: "room@example".into(), own_nick: "me".into(), members: Vec::new(),
    });
    let _ = s.update(Message::SelectRoom("room@example".into()));
    s.history.as_ref().unwrap().disable_writes();
    // An own echo need not have a locally queued origin (e.g. another send
    // path or a restarted client); retain it until durable context is readable.
    for body in ["first note", "first note", "different note", "different note"] {
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomMessage {
            room: "room@example".into(), nick: "me".into(), body: body.into(),
            timestamp: chrono::Utc::now(), delayed: false, stanza_id: None, origin_id: Some("unknown-origin".into()),
        }));
    }
    assert_eq!(s.rooms[0].messages.len(), 2);
    assert_eq!(s.pending_history_writes.len(), 2);
    s.history.as_ref().unwrap().enable_writes();
    let _ = s.retry_history_writes();
    assert!(s.pending_history_writes.is_empty());
    let rows = s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| row.body == "first note"));
    assert!(rows.iter().any(|row| row.body == "different note"));
    assert_eq!(s.rooms[0].messages.len(), 2);
    assert!(s.rooms[0].messages.iter().all(|message| matches!(message,
        room::message::Message::Chat { id, .. } if *id > 0)));
}

#[test]
fn long_direct_chat_addresses_keep_header_and_composer_controls_visible() {
    use iced::advanced::widget::operation::{Operation, Focusable, Scrollable};
    #[derive(Default)]
    struct Bounds { composer: Option<iced::Rectangle>, address: Option<(iced::Rectangle, iced::Rectangle)>, texts: Vec<(String, iced::Rectangle)> }
    impl Operation for Bounds {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn focusable(&mut self, id: Option<&iced::widget::Id>, bounds: iced::Rectangle, _state: &mut dyn Focusable) {
            if id == Some(&iced::widget::Id::new(MESSAGE_INPUT_ID)) { self.composer = Some(bounds); }
        }
        fn scrollable(&mut self, id: Option<&iced::widget::Id>, bounds: iced::Rectangle, content: iced::Rectangle,
            _translation: iced::Vector, _state: &mut dyn Scrollable) {
            if id.is_none() { self.address = Some((bounds, content)); }
        }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) { self.texts.push((text.into(), bounds)); }
    }
    for address in [format!("{}@example", "longname".repeat(100)), format!("room@example/{}", "界".repeat(300))] {
        let mut s = state();
        s.chats[0].jid = address.clone();
        for width in [280.0, 600.0] {
            let size = iced::Size::new(width, 300.0);
            let mut widget = crate::ui::chat::view(&s);
            let mut tree = Tree::new(&widget);
            let r = renderer();
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut bounds = Bounds::default();
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut bounds);
            let (viewport, content) = bounds.address.expect("full address has a scroll viewport");
            assert!(viewport.height <= 64.0 && content.height > viewport.height);
            let composer = bounds.composer.unwrap();
            assert!(composer.height >= 35.0 && composer.y + composer.height <= size.height, "composer={composer:?}");
            for control in ["Close", "Send"] {
                let rect = bounds.texts.iter().find(|(text, _)| text == control).unwrap().1;
                assert!(rect.width >= 20.0 && rect.height >= 12.0 && rect.x + rect.width <= width && rect.y + rect.height <= size.height,
                    "hidden {control}: {rect:?}, size={size:?}");
            }
            assert!(bounds.texts.iter().any(|(text, _)| text == &address));
        }
    }
}

#[test]
fn narrow_find_bar_keeps_search_input_and_controls_usable() {
    use iced::advanced::widget::operation::{Operation, Focusable};
    #[derive(Default)]
    struct Bounds { search: Option<iced::Rectangle>, composer: Option<iced::Rectangle>, texts: Vec<(String, iced::Rectangle)> }
    impl Operation for Bounds {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn focusable(&mut self, id: Option<&iced::widget::Id>, bounds: iced::Rectangle, _state: &mut dyn Focusable) {
            if id == Some(&iced::widget::Id::new(crate::app::FIND_INPUT_ID)) { self.search = Some(bounds); }
            if id == Some(&iced::widget::Id::new(MESSAGE_INPUT_ID)) { self.composer = Some(bounds); }
        }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) { self.texts.push((text.into(), bounds)); }
    }
    for error in [None, Some("Cannot search history. ".repeat(20))] {
        for width in [280.0, 600.0] {
            let mut s = state();
            s.find = Some(crate::app::FindState { query: "needle".into(), error: error.clone(), ..Default::default() });
            let size = iced::Size::new(width, 400.0);
            let mut widget = crate::ui::chat::view(&s);
            let mut tree = Tree::new(&widget);
            let r = renderer();
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut bounds = Bounds::default();
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut bounds);
            let search = bounds.search.unwrap();
            assert!(search.width >= 120.0 && search.height >= 25.0, "search collapsed: {search:?}, width={width}");
            let composer = bounds.composer.unwrap();
            assert!(composer.height >= 35.0 && composer.y + composer.height <= size.height, "composer collapsed: {composer:?}");
            for control in ["˄", "˅", "All rooms", "✕"].into_iter().chain(error.is_some().then_some("Retry")) {
                let rect = bounds.texts.iter().find(|(text, _)| text == control).unwrap().1;
                assert!(rect.width > 5.0 && rect.height > 10.0 && rect.x >= 0.0 && rect.x + rect.width <= width && rect.y + rect.height <= composer.y,
                    "hidden {control}: {rect:?}, size={size:?}");
            }
        }
    }
}

#[test]
fn find_input_keeps_focus_as_search_errors_appear_and_clear() {
    let mut s = state();
    s.find = Some(crate::app::FindState { query: "needle".into(), ..Default::default() });
    let r = renderer();
    let size = iced::Size::new(280.0, 400.0);
    let bounds = iced::Rectangle::new(iced::Point::ORIGIN, size);
    let mut tree;
    {
        let mut widget = crate::ui::chat::view(&s);
        tree = Tree::new(&widget);
        let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
        let mut focus = iced::advanced::widget::operation::focusable::focus::<()>(iced::widget::Id::new(crate::app::FIND_INPUT_ID));
        widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut focus);
    }
    for error in [Some("Cannot search history. ".repeat(20)), None, Some("retry failed".into()), None] {
        s.find.as_mut().unwrap().error = error;
        let mut widget = crate::ui::chat::view(&s);
        tree.diff(widget.as_widget());
        let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
        let mut output = Vec::new();
        widget.as_widget_mut().update(&mut tree, &iced::Event::InputMethod(iced::advanced::input_method::Event::Commit(" more".into())),
            Layout::new(&node), iced::mouse::Cursor::Unavailable, &r,
            &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &bounds);
        assert!(output.iter().any(|message| matches!(message, Message::ScopedFind { context, action: crate::message::FindAction::Input(query) }
            if *context == s.find.as_ref().unwrap().id && query == "needle more")), "search lost focus: {output:?}");
    }
}

#[test]
fn short_chat_keeps_composer_usable_with_selection_and_search() {
    use iced::advanced::widget::operation::{Operation, Focusable};
    #[derive(Default)]
    struct Bounds(Option<iced::Rectangle>);
    impl Operation for Bounds {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn focusable(&mut self, id: Option<&iced::widget::Id>, bounds: iced::Rectangle, _state: &mut dyn Focusable) {
            if id == Some(&iced::widget::Id::new(MESSAGE_INPUT_ID)) { self.0 = Some(bounds); }
        }
    }
    for (multiline, height) in [(false, 300.0), (true, 300.0), (true, 600.0)] {
        let mut s = state();
        s.selected_msgs.insert(1);
        s.find = Some(crate::app::FindState { query: "needle".into(), error: Some("Cannot search history. ".repeat(20)), ..Default::default() });
        if multiline { s.message_input = text_editor::Content::with_text(&"draft line\n".repeat(20)); }
        let size = iced::Size::new(280.0, height);
        let mut widget = crate::ui::chat::view(&s);
        let mut tree = Tree::new(&widget);
        let r = renderer();
        let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
        let mut bounds = Bounds::default();
        widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut bounds);
        let input = bounds.0.unwrap();
        assert!(input.height >= 35.0 && input.height <= 160.0 && input.y + input.height <= size.height,
            "multiline={multiline}, composer={input:?}");
    }
}

#[test]
fn account_button_press_cannot_turn_into_a_reconnect_action() {
    use iced::advanced::widget::Operation;
    struct Target(Option<iced::Rectangle>);
    impl Operation for Target {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) {
            if text == "Remove auto-login" { self.0 = Some(bounds); }
        }
    }
    for reconnect in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        s.saved_config.jid = Some("me@example/device".into());
        let r = renderer();
        let size = iced::Size::new(200.0, 600.0);
        let viewport = iced::Rectangle::new(iced::Point::ORIGIN, size);
        let mut tree;
        let point;
        {
            let mut widget = crate::ui::sidebar::view(&s);
            tree = Tree::new(&widget);
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut target = Target(None);
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut target);
            point = target.0.unwrap().center();
            let mut output = Vec::new();
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &viewport);
            assert!(output.is_empty());
        }
        s.reconnecting = reconnect;
        let mut widget = crate::ui::sidebar::view(&s);
        tree.diff(widget.as_widget());
        let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
        let mut output = Vec::new();
        widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)),
            Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
            &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &viewport);
        assert_eq!(output.len(), usize::from(!reconnect), "reconnect={reconnect}: {output:?}");
        if !reconnect { assert!(matches!(output[0], Message::ScopedSidebar { action: crate::message::SidebarAction::ForgetAutoLogin, .. })); }
    }
}

#[test]
fn long_sidebar_account_details_leave_controls_and_conversations_visible() {
    use iced::advanced::widget::operation::{Operation, Scrollable};
    #[derive(Default)]
    struct Bounds { texts: Vec<(String, iced::Rectangle)>, notice: Option<(iced::Rectangle, iced::Vector)> }
    impl Operation for Bounds {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn scrollable(&mut self, id: Option<&iced::widget::Id>, bounds: iced::Rectangle, _content: iced::Rectangle,
            translation: iced::Vector, _state: &mut dyn Scrollable) {
            if id == Some(&iced::widget::Id::new("sidebar-credential-notice")) { self.notice = Some((bounds, translation)); }
        }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) { self.texts.push((text.into(), bounds)); }
    }
    for long_account in [false, true] {
        for warned in [false, true] {
            for reconnect in [false, true] {
                let mut s = completion_state(&[]);
                s.connected_jid = Some("me@example/device".into());
                s.bound_jid = Some(if long_account { format!("me@example/{}end", "device ".repeat(140)) } else { "me@example/device".into() });
                assert!(::xmpp::validate_message(s.own_jid().unwrap(), "").is_ok());
                s.saved_config.jid = s.connected_jid.clone();
                s.reconnecting = reconnect;
                if warned {
                    s.credential_error = Some("Password store unavailable. ".repeat(40));
                    s.saved_config.pending_credential_removals.push("old@example".into());
                }
                let size = iced::Size::new(800.0, 400.0);
                let mut widget = s.view();
                let mut tree = Tree::new(&widget);
                let r = renderer();
                let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
                let mut bounds = Bounds::default();
                widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut bounds);
                for control in ["Disconnect", "Remove auto-login", "Rooms", "Chats", "room", "alice@example"].into_iter()
                    .chain(reconnect.then_some("Reconnect now")) {
                    let rect = bounds.texts.iter().find(|(text, rect)| text == control && rect.x < 200.0).unwrap().1;
                    assert!(rect.width > 5.0 && rect.height > 10.0 && rect.x >= 0.0 && rect.x + rect.width <= 200.0 && rect.y + rect.height <= size.height,
                        "hidden {control}: {rect:?}, long={long_account}, warned={warned}, reconnect={reconnect}");
                }
                assert!(bounds.texts.iter().any(|(text, _)| Some(text.as_str()) == s.own_jid()));
                if warned {
                    assert!(bounds.texts.iter().any(|(text, _)| Some(text) == s.credential_error.as_ref()));
                    let mut bottom = iced::advanced::widget::operation::scrollable::snap_to::<()>(
                        iced::widget::Id::new("sidebar-credential-notice"),
                        iced::advanced::widget::operation::scrollable::RelativeOffset { x: None, y: Some(1.0) });
                    widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut bottom);
                    let mut scrolled = Bounds::default();
                    widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut scrolled);
                    let (viewport, translation) = scrolled.notice.unwrap();
                    assert!(viewport.height <= 96.0);
                    let retry = scrolled.texts.iter().find(|(text, _)| text == "Retry password cleanup").unwrap().1;
                    let point = retry.center() - translation;
                    assert!(viewport.contains(point), "cleanup control unreachable: {retry:?}, {viewport:?}, {translation:?}");
                    let mut output = Vec::new();
                    for event in [iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left), iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)] {
                        widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(event), Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                            &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &iced::Rectangle::new(iced::Point::ORIGIN, size));
                    }
                    assert!(output.iter().any(|message| matches!(message,
                        Message::ScopedSidebar { action: crate::message::SidebarAction::RetryCredentialRemoval, .. })));
                }
            }
        }
    }
}

#[test]
fn sidebar_room_and_chat_lists_stay_visible_and_reach_their_last_entries() {
    use iced::advanced::widget::operation::{Operation, Scrollable};
    #[derive(Default)]
    struct Bounds {
        texts: Vec<(String, iced::Rectangle)>,
        scrolls: std::collections::HashMap<iced::widget::Id, (iced::Rectangle, iced::Vector)>,
    }
    impl Operation for Bounds {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) { self.texts.push((text.into(), bounds)); }
        fn scrollable(&mut self, id: Option<&iced::widget::Id>, bounds: iced::Rectangle, _content: iced::Rectangle,
            translation: iced::Vector, _state: &mut dyn Scrollable) {
            if let Some(id) = id { self.scrolls.insert(id.clone(), (bounds, translation)); }
        }
    }
    for (rooms, chats) in [(0, 0), (0, 100), (100, 1), (1, 100), (100, 0), (100, 100)] {
        let mut s = state();
        s.state = AppState::Connected;
        for i in 0..rooms {
            let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined {
                room: format!("room{i}@example"), own_nick: "me".into(), members: Vec::new(),
            });
        }
        s.chats = (0..chats).map(|i| chat(&format!("chat{i}@example"), &[])).collect();
        s.active = None;
        let size = iced::Size::new(200.0, 400.0);
        let viewport = iced::Rectangle::new(iced::Point::ORIGIN, size);
        let mut widget = crate::ui::sidebar::view(&s);
        let mut tree = Tree::new(&widget);
        let r = renderer();
        let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
        let mut bounds = Bounds::default();
        widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut bounds);
        for header in ["Rooms", "Chats"] {
            let rect = bounds.texts.iter().find(|(text, _)| text == header).unwrap().1;
            assert!(rect.height > 10.0 && rect.width > 5.0 && rect.y + rect.height <= size.height,
                "hidden {header}: {rect:?}, rooms={rooms}, chats={chats}");
        }
        for (id, count, is_room) in [("sidebar-rooms", rooms, true), ("sidebar-chats", chats, false)] {
            let target_id = iced::widget::Id::new(id);
            let (rect, _) = bounds.scrolls[&target_id];
            assert!(rect.y + rect.height <= size.height, "list outside sidebar: {id}, {rect:?}");
            if count == 0 {
                assert_eq!(rect.height, 0.0, "empty list consumes space: {id}");
                continue;
            }
            assert!(rect.height >= 40.0, "list collapsed: {id}, {rect:?}, rooms={rooms}, chats={chats}");
            let mut bottom = iced::advanced::widget::operation::scrollable::snap_to::<()>(target_id.clone(),
                iced::advanced::widget::operation::scrollable::RelativeOffset { x: None, y: Some(1.0) });
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut bottom);
            let mut scrolled = Bounds::default();
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut scrolled);
            let target = if is_room { format!("room{}", count - 1) } else { format!("chat{}@example", count - 1) };
            let label = scrolled.texts.iter().find(|(text, _)| text == &target).unwrap().1;
            let (list_bounds, translation) = scrolled.scrolls[&target_id];
            let point = label.center() - translation;
            assert!(list_bounds.contains(point), "last entry unreachable: {target}");
            let mut output = Vec::new();
            for event in [iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left), iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)] {
                widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(event), Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                    &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &viewport);
            }
            let jid = if is_room { format!("{target}@example") } else { target };
            assert!(output.iter().any(|message| matches!(message, Message::ScopedSidebar { action, .. } if match action {
                crate::message::SidebarAction::SelectRoom(target) if is_room => target == &jid,
                crate::message::SidebarAction::SelectChat(target) if !is_room => target == &jid,
                _ => false,
            })), "last entry did not activate: {jid}, {output:?}");
        }
    }
}

#[test]
fn recovered_self_chat_copy_keeps_its_existing_row_in_unwatched_windows() {
    for inactive in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        s.history = Some(History::open_in_memory());
        let (tx, _rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        let _ = s.update(Message::StartChat("me@example".into()));
        s.message_input = text_editor::Content::with_text("self note");
        let _ = s.update(Message::SendMessage);
        let outgoing = s.history.as_ref().unwrap().load_outbox().unwrap()[0].clone();
        let stored = s.history.as_ref().unwrap().load_tail("me@example", 10).unwrap()[0].clone();
        let idx = s.chats.iter().position(|chat| chat.jid == "me@example").unwrap();
        s.chats[idx].anchored_bottom = false;
        s.chats[idx].stamp_read_boundary();
        if inactive { let _ = s.update(Message::SelectChat("alice@example".into())); }
        else { let _ = s.update(Message::SelectMessage { id: stored.id }); }
        let active = s.active;
        s.history.as_ref().unwrap().disable_writes();
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage {
            from: "me@example/phone".into(), body: "self note".into(), timestamp: chrono::Utc::now(),
            delayed: false, stanza_id: Some("self-copy".into()), origin_id: Some(outgoing.origin_id),
        }));
        assert_eq!(s.pending_history_writes.len(), 1);
        assert_eq!(s.pending_history_writes[0].temp_id, stored.id);
        s.history.as_ref().unwrap().enable_writes();
        let _ = s.update(Message::RetryHistoryWrites);
        assert!(s.pending_history_writes.is_empty());
        assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
        assert_eq!(s.active, active);
        assert_eq!(s.chats[idx].messages.len(), 1, "inactive={inactive}: recovery removed the existing row");
        assert!(matches!(&s.chats[idx].messages[0], room::message::Message::Chat { id, received, status: ChatStatus::Submitted(link), .. }
            if *id == stored.id && *received == stored.received && *link == outgoing.id));
        assert_eq!(s.chats[idx].read_boundary.unwrap().id, Some(stored.id));
        if !inactive { assert!(s.selected_msgs.contains(&stored.id)); }
    }
}

#[test]
fn paging_from_search_respects_the_history_window_limit() {
    let mut s = state();
    s.state = AppState::Connected;
    let history = History::open_in_memory();
    let now = chrono::Utc::now();
    for i in 0..2000 {
        history.insert("alice@example", "alice", &format!("history {i}"),
            now + chrono::Duration::seconds(i), false, None).unwrap();
    }
    s.history = Some(history);
    seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
    s.chats[0].stamp_read_boundary();
    let boundary = s.chats[0].read_boundary.unwrap().id;
    let _ = s.update(Message::ToggleFind);
    let _ = s.update(Message::FindInputChanged("history 1600".into()));
    assert_eq!(s.chats[0].messages.len(), backlog::JUMP_CONTEXT * 2 + 1);
    let hit = s.find.as_ref().unwrap().matches[0].id;
    let newest = match s.chats[0].messages.last().unwrap() {
        room::message::Message::Chat { id, .. } => *id,
        _ => panic!("expected stored search window"),
    };
    while s.chats[0].messages.len() < backlog::MAX_LOADED {
        assert!(s.page_older_active().unwrap());
        assert!(s.chats[0].messages.len() <= backlog::MAX_LOADED,
            "paging exceeded rendering limit: {}", s.chats[0].messages.len());
    }
    assert_eq!(s.chats[0].messages.len(), backlog::MAX_LOADED);
    assert!(s.chats[0].messages.iter().any(|message| matches!(message, room::message::Message::Chat { id, .. } if *id == hit)));
    assert!(matches!(s.chats[0].messages.last().unwrap(), room::message::Message::Chat { id, .. } if *id == newest));
    assert_eq!(s.chats[0].read_boundary.unwrap().id, boundary);
    assert!(!s.page_older_active().unwrap());
}

#[test]
fn distinct_live_messages_in_one_millisecond_survive_storage_recovery() {
    for room in [false, true] {
        for outage in [false, true] {
            let mut s = if room { completion_state(&[]) } else { state() };
            s.state = AppState::Connected;
            s.connected_jid = Some("me@example/device".into());
            s.window_focused = true;
            s.chats[0].messages.clear();
            s.body_editors.clear();
            s.history = Some(History::open_in_memory());
            if outage { s.history.as_ref().unwrap().disable_writes(); }
            let base = chrono::DateTime::from_timestamp_millis(1_700_000_000_000).unwrap();
            for (nanos, sid) in [(100, None), (200, None), (200, None), (300, Some("stable")), (300, Some("stable"))] {
                let timestamp = base + chrono::Duration::nanoseconds(nanos);
                let event = if room { xmpp::XmppEvent::RoomMessage {
                    room: "room@example".into(), nick: "alice".into(), body: "ok".into(), timestamp,
                    delayed: false, stanza_id: sid.map(str::to_string), origin_id: None,
                } } else { xmpp::XmppEvent::DirectMessage {
                    from: "alice@example/phone".into(), body: "ok".into(), timestamp,
                    delayed: false, stanza_id: sid.map(str::to_string), origin_id: None,
                } };
                let _ = s.update(Message::XmppEvent(event));
            }
            assert_eq!(s.active_backlog().unwrap().messages().len(), 4, "room={room}, outage={outage}: distinct arrivals collapsed");
            if outage {
                assert_eq!(s.pending_history_writes.len(), 4);
                s.history.as_ref().unwrap().enable_writes();
                let _ = s.update(Message::RetryHistoryWrites);
                let _ = s.update(Message::RetryHistoryWrites);
            }
            assert!(s.pending_history_writes.is_empty());
            let conversation = if room { "room@example" } else { "alice@example" };
            let rows = s.history.as_ref().unwrap().load_tail(conversation, 10).unwrap();
            assert_eq!(rows.len(), 4, "room={room}, outage={outage}: recovery collapsed distinct arrivals");
            assert_eq!(rows.iter().map(|row| row.id).collect::<std::collections::HashSet<_>>().len(), 4);
            assert_eq!(s.active_backlog().unwrap().messages().len(), 4);
        }
    }
}

#[test]
fn incoming_delayed_message_cannot_consume_a_queued_history_retry()
{
    for archived in [false, true] {
        for stanza_id in [None, Some("remote-copy".to_string())] {
            let mut s = state();
            s.state = AppState::Connected;
            s.connected_jid = Some("me@local/device".into());
            s.chats = vec![chat("me@remote", &[])];
            s.chats[0].title = "me".into();
            let h = History::open_in_memory();
            h.disable_reads();
            s.history = Some(h);
            let (tx, _rx) = tokio::sync::mpsc::channel(10);
            s.xmpp_cmd_tx = Some(tx);
            s.message_input = text_editor::Content::with_text("same body");
            let _ = s.update(Message::SendMessage);
            assert_eq!(s.pending_history_writes.len(), 1);
            let pending = &s.pending_history_writes[0];
            let timestamp = pending.received;
            let queued_id = pending.outbox_id.unwrap();
            let original_receipt = pending.receipt;
            s.mam_pending.insert("archive".into(), crate::app::ArchiveContext { conversation: "me@remote".into(), is_muc: false });
            let incoming = if archived {
                xmpp::XmppEvent::ArchiveMessage { to: None, query_id: "archive".into(), from: "me@remote/phone".into(),
                    body: "same body".into(), timestamp, stanza_id: stanza_id.clone(), origin_id: None }
            } else {
                xmpp::XmppEvent::DirectMessage { from: "me@remote/phone".into(), body: "same body".into(),
                    timestamp, delayed: true, stanza_id: stanza_id.clone(), origin_id: None }
            };
            let _ = s.update(Message::XmppEvent(incoming));
            assert_eq!(s.pending_history_writes.len(), 2, "archived={archived}, stanza_id={stanza_id:?}");
            assert_eq!(s.pending_history_writes[0].receipt, original_receipt);
            assert!(!s.pending_history_writes[0].delayed);
            assert!(s.pending_history_writes[0].stanza_id.is_none());
            assert!(s.pending_history_writes[1].outbox_id.is_none());
            s.history.as_ref().unwrap().enable_reads();
            let _ = s.update(Message::RetryHistoryWrites);
            let _ = s.update(Message::RetryHistoryWrites);
            assert!(s.pending_history_writes.is_empty());
            let history = s.history.as_ref().unwrap();
            let stored = history.load_tail("me@remote", 10).unwrap();
            assert_eq!(stored.len(), 2);
            assert_eq!(stored.iter().filter(|message| message.delivery == storage::history::DeliveryState::Queued(queued_id)).count(), 1);
            assert_eq!(stored.iter().filter(|message| message.delivery == storage::history::DeliveryState::Received).count(), 1);
            assert_eq!(history.load_outbox().unwrap().len(), 1);
        }
    }
}

#[test]
fn pending_own_copies_keep_server_time_independent_of_local_clock_skew()
{
    use chrono::TimeZone;
    let server_time = chrono::Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
    for outage in [false, true] {
        for archive_first in [false, true] {
            let mut s = state();
            s.state = AppState::Connected;
            s.connected_jid = Some("me@example/device".into());
            s.history = Some(History::open_in_memory());
            let (tx, _rx) = tokio::sync::mpsc::channel(10);
            s.xmpp_cmd_tx = Some(tx);
            let _ = s.update(Message::StartChat("me@example".into()));
            s.message_input = text_editor::Content::with_text("self note");
            let _ = s.update(Message::SendMessage);
            let queued = s.history.as_ref().unwrap().load_outbox().unwrap().remove(0);
            if outage { s.history.as_ref().unwrap().disable_writes(); }
            s.mam_pending.insert("archive".into(), crate::app::ArchiveContext { conversation: "me@example".into(), is_muc: false });
            let mut copies = vec![(false, server_time - chrono::Duration::hours(1)), (true, server_time)];
            if archive_first { copies.reverse(); }
            copies.extend([(true, server_time + chrono::Duration::minutes(1)), (false, server_time - chrono::Duration::hours(2))]);
            for (delayed, timestamp) in copies {
                let event = if delayed {
                    xmpp::XmppEvent::ArchiveMessage { to: Some("me@example".into()), query_id: "archive".into(),
                        from: "me@example/phone".into(), body: "self note".into(), timestamp,
                        stanza_id: Some("server-copy".into()), origin_id: Some(queued.origin_id.clone()) }
                } else {
                    xmpp::XmppEvent::DirectMessage { from: "me@example/phone".into(), body: "self note".into(), timestamp,
                        delayed: false, stanza_id: Some("server-copy".into()), origin_id: Some(queued.origin_id.clone()) }
                };
                let _ = s.update(Message::XmppEvent(event));
            }
            if outage {
                assert_eq!(s.pending_history_writes.len(), 1);
                assert!(s.pending_history_writes[0].delayed);
                assert_eq!(s.pending_history_writes[0].received, server_time, "archive_first={archive_first}");
                s.history.as_ref().unwrap().enable_writes();
                let _ = s.update(Message::RetryHistoryWrites);
            }
            assert!(s.pending_history_writes.is_empty());
            let h = s.history.as_ref().unwrap();
            let stored = h.load_tail("me@example", 10).unwrap();
            assert_eq!(stored.len(), 1);
            assert_eq!(stored[0].received, server_time, "outage={outage}, archive_first={archive_first}");
            assert_eq!(stored[0].delivery, storage::history::DeliveryState::Submitted(queued.id));
            assert!(h.load_outbox().unwrap().is_empty());
            let chat = s.chats.iter().find(|chat| chat.jid == "me@example").unwrap();
            assert_eq!(chat.messages.len(), 1);
            assert!(matches!(&chat.messages[0], room::message::Message::Chat { received, .. } if *received == server_time));
        }
    }
}

#[test]
fn ordinary_message_retries_keep_the_first_server_timestamp()
{
    use chrono::TimeZone;
    let server_time = chrono::Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
    for outage in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        s.chats[0].messages.clear();
        s.body_editors.clear();
        s.history = Some(History::open_in_memory());
        if outage { s.history.as_ref().unwrap().disable_writes(); }
        for (delayed, timestamp) in [
            (false, server_time - chrono::Duration::hours(1)),
            (true, server_time),
            (true, server_time + chrono::Duration::minutes(1)),
            (true, server_time - chrono::Duration::minutes(1)),
        ] {
            let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage {
                from: "alice@example/phone".into(), body: "one message".into(), timestamp, delayed,
                stanza_id: Some("same-server-copy".into()), origin_id: None,
            }));
        }
        if outage {
            assert_eq!(s.pending_history_writes.len(), 1);
            s.history.as_ref().unwrap().enable_writes();
            let _ = s.update(Message::RetryHistoryWrites);
        }
        let stored = s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].received, server_time, "outage={outage}");
    }
}

#[test]
fn archive_duplicates_refresh_visible_timestamps_without_replacing_the_window()
{
    use chrono::TimeZone;
    let base = chrono::Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
    for (from_archive, is_room) in [(false, false), (false, true), (true, false), (true, true)] {
        for detached in [false, true] {
            for with_id in [false, true] {
                let mut s = if is_room { completion_state(&[]) } else { state() };
                s.state = AppState::Connected;
                s.connected_jid = Some("me@example/device".into());
                s.window_focused = true;
                s.history = Some(History::open_in_memory());
                let conv = if is_room { "room@example" } else { "alice@example" };
                s.backlog_by_jid_mut(conv).unwrap().messages_mut().clear();
                s.body_editors.clear();
                s.mam_pending.insert("archive".into(), crate::app::ArchiveContext { conversation: conv.into(), is_muc: is_room });
                let event = |body: &str, seconds: i64, delayed: bool| {
                    let timestamp = base + chrono::Duration::seconds(seconds);
                    let stanza_id = with_id.then(|| body.to_string());
                    if delayed && from_archive {
                        xmpp::XmppEvent::ArchiveMessage { to: None, query_id: "archive".into(),
                            from: if is_room { "room@example/alice".into() } else { "alice@example/phone".into() },
                            body: body.into(), timestamp, stanza_id, origin_id: None }
                    } else if is_room {
                        xmpp::XmppEvent::RoomMessage { room: conv.into(), nick: "alice".into(), body: body.into(),
                            timestamp, delayed, stanza_id, origin_id: None }
                    } else {
                        xmpp::XmppEvent::DirectMessage { from: "alice@example/phone".into(), body: body.into(),
                            timestamp, delayed, stanza_id, origin_id: None }
                    }
                };
                let _ = s.update(Message::XmppEvent(event("corrected", 120, false)));
                let _ = s.update(Message::XmppEvent(event("neighbour", 60, false)));
                let target = s.history.as_ref().unwrap().load_tail(conv, 10).unwrap().into_iter().find(|m| m.body == "corrected").unwrap().id;
                s.selected_msgs.insert(target);
                s.select_anchor = Some(target);
                {
                    let b = s.backlog_by_jid_mut(conv).unwrap();
                    assert!(matches!(&b.messages()[1], room::message::Message::Chat { id, .. } if *id == target));
                    b.set_read_boundary(None);
                    b.set_read_marker(Some(1));
                    if detached {
                        b.set_at_live_tail(false);
                        b.set_anchored_bottom(false);
                    }
                }
                let _ = s.update(Message::XmppEvent(event("corrected", 0, true)));
                let persisted = s.history.as_ref().unwrap().load_tail(conv, 10).unwrap();
                assert_eq!(persisted.len(), 2);
                assert_eq!(persisted[0].id, target);
                assert_eq!(persisted[0].received, base);
                let b = s.backlog_by_jid_mut(conv).unwrap();
                assert_eq!(b.messages().len(), 2);
                assert!(matches!(&b.messages()[0], room::message::Message::Chat { id, received, .. } if *id == target && *received == base),
                    "room={is_room}, detached={detached}, with_id={with_id}");
                assert_eq!(b.read_marker(), Some(0));
                if detached { assert!(!b.anchored_bottom()); }
                assert!(s.selected_msgs.contains(&target));
                assert_eq!(s.select_anchor, Some(target));
            }
        }
    }
}

#[test]
fn no_op_composer_edits_preserve_redo_and_do_not_evict_undo_history()
{
    use text_editor::{Action, Edit, Motion};
    for no_op in [Edit::Backspace, Edit::Delete, Edit::Unindent, Edit::Paste(std::sync::Arc::new(String::new()))] {
        let mut s = state();
        let _ = s.update(Message::InputAction(Action::Edit(Edit::Paste(std::sync::Arc::new("recover my draft".into())))));
        let original_cursor = s.message_input.cursor();
        let _ = s.update(Message::UndoInput);
        assert!(s.message_input.text().is_empty());
        for _ in 0..110 {
            let _ = s.update(Message::InputAction(Action::Edit(no_op.clone())));
            assert!(s.message_input.text().is_empty());
        }
        let _ = s.update(Message::RedoInput);
        assert_eq!(s.message_input.text(), "recover my draft", "{no_op:?}");
        assert_eq!(s.message_input.cursor(), original_cursor);
    }
    let mut s = state();
    s.message_input = text_editor::Content::with_text("original");
    let _ = s.update(Message::InputAction(Action::Move(Motion::DocumentEnd)));
    let _ = s.update(Message::InputAction(Action::Edit(Edit::Insert('!'))));
    for _ in 0..110 { let _ = s.update(Message::InputAction(Action::Edit(Edit::Delete))); }
    assert_eq!(s.message_input.text(), "original!");
    let _ = s.update(Message::UndoInput);
    assert_eq!(s.message_input.text(), "original");
    let _ = s.update(Message::RedoInput);
    assert_eq!(s.message_input.text(), "original!");
}

#[cfg(unix)]
#[test]
fn configuration_directory_sync_failure_keeps_close_pending_until_retry()
{
    let mut s = state();
    assert!(s.persist_settings());
    s.message_input = text_editor::Content::with_text("draft with uncertain save durability");
    storage::fail_next_directory_sync();
    let task = s.update(Message::WindowCloseRequested(iced::window::Id::unique()));
    assert_eq!(task.units(), 0, "directory sync failure must not acknowledge a completed save and close");
    assert!(s.settings_save_error.as_deref().is_some_and(|error| error.contains("injected settings directory sync failure")));
    assert!(s.pending_close.is_some());
    assert_eq!(s.message_input.text(), "draft with uncertain save durability");
    // Replacement already happened; uncertainty is not a claim that the old
    // file remains. The complete replacement can be read and safely retried.
    assert_eq!(storage::load().drafts["alice@example"], "draft with uncertain save durability");
    let path = storage::config_path().unwrap();
    assert!(!std::fs::read_dir(path.parent().unwrap()).unwrap().filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().starts_with(".config-")));
    s.message_input = text_editor::Content::with_text("newest draft before retry");
    assert_eq!(s.update(Message::RetrySettingsSave).units(), 1);
    assert!(s.pending_close.is_none());
    assert!(s.settings_save_error.is_none());
    assert_eq!(storage::load().drafts["alice@example"], "newest draft before retry");
}

#[test]
fn long_form_errors_remain_readable_and_recovery_actions_reachable()
{
    use iced::advanced::widget::operation::{Operation, Scrollable};
    #[derive(Default)]
    struct Bounds {
        texts: Vec<(String, iced::Rectangle)>,
        scrolls: std::collections::HashMap<iced::widget::Id, (iced::Rectangle, iced::Rectangle, iced::Vector)>,
    }
    impl Operation for Bounds {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) { self.texts.push((text.into(), bounds)); }
        fn scrollable(&mut self, id: Option<&iced::widget::Id>, bounds: iced::Rectangle, content: iced::Rectangle,
            translation: iced::Vector, _state: &mut dyn Scrollable) {
            if let Some(id) = id { self.scrolls.insert(id.clone(), (bounds, content, translation)); }
        }
    }
    for kind in ["login", "join", "recovery"] {
        for size in [iced::Size::new(600.0, 400.0), iced::Size::new(300.0, 240.0)] {
            let mut s = state();
            s.state = if kind == "join" { AppState::Connected } else { AppState::Login };
            let error = "Server or settings error details\n".repeat(500);
            match kind {
                "login" => s.connect_error = Some(error.clone()),
                "join" => s.join_error = Some(error.clone()),
                _ => s.saved_config.load_error = Some(error.clone()),
            }
            let (id, label) = if kind == "join" { ("join-form-scroll", "Join") }
                else { ("account-form-scroll", if kind == "login" { "Connect" } else { "Back up settings and start fresh" }) };
            let id = iced::widget::Id::new(id);
            let mut widget = if kind == "join" { crate::ui::join::view(&s) } else { crate::ui::account::view(&s) };
            let mut tree = Tree::new(&widget);
            let r = renderer();
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut bounds = Bounds::default();
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut bounds);
            let details = bounds.texts.iter().find(|(text, _)| text == &error).unwrap().1;
            assert!(details.height > size.height, "long details were clipped without a way to read them: {kind}, {size:?}, {details:?}");
            let button = bounds.texts.iter().find(|(text, _)| text == label).unwrap().1;
            for (target, click) in [(iced::Point::new(details.x + 5.0, details.y + details.height - 5.0), false), (button.center(), true)] {
                let (viewport, content, _) = bounds.scrolls.get(&id).copied()
                    .unwrap_or((iced::Rectangle::new(iced::Point::ORIGIN, size), iced::Rectangle::new(iced::Point::ORIGIN, size), iced::Vector::ZERO));
                let overflow = content.height - viewport.height;
                let fraction = if overflow > 0.0 { ((target.y - viewport.y - viewport.height / 2.0) / overflow).clamp(0.0, 1.0) } else { 0.0 };
                let mut scroll = iced::advanced::widget::operation::scrollable::snap_to::<()>(id.clone(),
                    iced::advanced::widget::operation::scrollable::RelativeOffset { x: None, y: Some(fraction) });
                widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut scroll);
                let mut scrolled = Bounds::default();
                widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut scrolled);
                let (viewport, _, translation) = scrolled.scrolls.get(&id).copied().unwrap_or((viewport, content, iced::Vector::ZERO));
                let point = target - translation;
                assert!(viewport.contains(point), "unreachable form content: {kind}, {size:?}, click={click}, {point:?}, {viewport:?}");
                if click {
                    let mut output = Vec::new();
                    for event in [iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left), iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)] {
                        widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(event), Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                            &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &iced::Rectangle::new(iced::Point::ORIGIN, size));
                    }
                    assert!(output.iter().any(|message| match kind {
                        "join" => matches!(message, Message::ScopedJoinForm { action: crate::message::JoinFormAction::Submit, .. }),
                        "login" => matches!(message, Message::ScopedLogin { action: crate::message::LoginAction::Connect, .. }),
                        _ => matches!(message, Message::ScopedLogin { action: crate::message::LoginAction::RecoverConfiguration, .. }),
                    }), "form control could not be clicked: {kind}, {size:?}");
                }
            }
        }
    }
}

#[test]
fn global_notices_preserve_chat_space_and_recovery_actions_with_long_errors()
{
    use iced::advanced::widget::operation::{Operation, Focusable, Scrollable};
    #[derive(Default)]
    struct Bounds {
        texts: Vec<(String, iced::Rectangle)>,
        input: Option<iced::Rectangle>,
        notices: Option<(iced::Rectangle, iced::Rectangle, iced::Vector)>,
    }
    impl Operation for Bounds {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
        fn text(&mut self, _id: Option<&iced::widget::Id>, bounds: iced::Rectangle, text: &str) { self.texts.push((text.into(), bounds)); }
        fn focusable(&mut self, id: Option<&iced::widget::Id>, bounds: iced::Rectangle, _state: &mut dyn Focusable) {
            if id == Some(&iced::widget::Id::new(MESSAGE_INPUT_ID)) { self.input = Some(bounds); }
        }
        fn scrollable(&mut self, id: Option<&iced::widget::Id>, bounds: iced::Rectangle, content: iced::Rectangle,
            translation: iced::Vector, _state: &mut dyn Scrollable) {
            if id == Some(&iced::widget::Id::new("global-notices")) { self.notices = Some((bounds, content, translation)); }
        }
    }
    for all_notices in [false, true] {
        for size in [iced::Size::new(800.0, 600.0), iced::Size::new(500.0, 360.0)] {
            let mut s = state();
            s.state = AppState::Connected;
            s.connected_jid = Some("me@example/device".into());
            s.message_input = text_editor::Content::with_text("draft\u{7} needs cleanup");
            let error = "Long recovery error details\n".repeat(300);
            s.settings_save_error = Some(error.clone());
            let token = uuid::Uuid::new_v4();
            s.pending_close = Some(crate::app::PendingClose { window: iced::window::Id::unique(), token, draining: false, discard: false });
            let rejected = uuid::Uuid::new_v4();
            if all_notices {
                s.send_error = Some(error.clone());
                s.rejected_sends.push(crate::app::RejectedSend { token: rejected, account: Some("me@example".into()), outbox_id: 1,
                    conversation: "alice@example".into(), body: "failed message".into(), reason: error.clone(), saved: true });
                s.room_connections.insert("room@example".into(), RoomConnection::Failed(error.clone()));
                let (tx, _rx) = tokio::sync::mpsc::channel(10);
                s.xmpp_cmd_tx = Some(tx);
            }
            let mut widget = s.view();
            let mut tree = Tree::new(&widget);
            let r = renderer();
            let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
            let mut bounds = Bounds::default();
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut bounds);
            let input = bounds.input.unwrap();
            assert!(input.height >= 35.0 && input.y + input.height <= size.height,
                "notices hid the composer: all={all_notices}, {size:?}, {input:?}");
            let (viewport, content, _) = bounds.notices.expect("long notices need a scroll viewport");
            assert!(viewport.height <= size.height * 0.4 + 1.0);
            assert!(content.height > viewport.height);
            let labels = ["Retry save and close", "Keep open", "Close without saving"].into_iter()
                .chain(all_notices.then_some(["Retry join", "Remove control characters", "Copy message", "Discard"]).into_iter().flatten());
            for label in labels {
                let target = bounds.texts.iter().find(|(text, _)| text == label).unwrap().1.center();
                let fraction = ((target.y - viewport.y - viewport.height / 2.0) / (content.height - viewport.height)).clamp(0.0, 1.0);
                let mut scroll = iced::advanced::widget::operation::scrollable::snap_to::<()>(iced::widget::Id::new("global-notices"),
                    iced::advanced::widget::operation::scrollable::RelativeOffset { x: None, y: Some(fraction) });
                widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut scroll);
                let mut scrolled = Bounds::default();
                widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut scrolled);
                let (viewport, _, translation) = scrolled.notices.unwrap();
                let point = target - translation;
                assert!(viewport.contains(point), "unreachable {label}: {point:?}, {viewport:?}");
                let mut output = Vec::new();
                for event in [iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left), iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)] {
                    widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(event), Layout::new(&node), iced::mouse::Cursor::Available(point), &r,
                        &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut output), &iced::Rectangle::new(iced::Point::ORIGIN, size));
                }
                assert!(output.iter().any(|message| match label {
                    "Retry save and close" => matches!(message, Message::SaveNotice { prompt: Some(prompt), action: crate::message::SaveNoticeAction::Retry } if *prompt == token),
                    "Keep open" => matches!(message, Message::SaveNotice { prompt: Some(prompt), action: crate::message::SaveNoticeAction::KeepOpen } if *prompt == token),
                    "Close without saving" => matches!(message, Message::SaveNotice { prompt: Some(prompt), action: crate::message::SaveNoticeAction::Discard } if *prompt == token),
                    "Retry join" => matches!(message, Message::ScopedRoomRetry(_)),
                    "Remove control characters" => matches!(message, Message::ScopedComposer { context, action: crate::message::ComposerAction::Clean(_) } if *context == s.composer_id),
                    "Copy message" => matches!(message, Message::CopyRejectedSend(id) if *id == rejected),
                    _ => matches!(message, Message::DiscardRejectedSend(id) if *id == rejected),
                }), "could not click {label}: {output:?}");
            }
        }
    }
}

#[test]
fn global_notice_visibility_changes_preserve_composer_focus()
{
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.message_input = text_editor::Content::with_text("draft");
    let r = renderer();
    let size = iced::Size::new(800.0, 600.0);
    let viewport = iced::Rectangle::new(iced::Point::ORIGIN, size);
    let mut tree;
    {
        let mut widget = s.view();
        tree = Tree::new(&widget);
        let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
        let mut focus = iced::advanced::widget::operation::focusable::focus::<()>(iced::widget::Id::new(MESSAGE_INPUT_ID));
        widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &r, &mut focus);
    }
    for error in [Some("Save failed\n".repeat(300)), None, Some("Save failed again".into()), None] {
        s.settings_save_error = error;
        let mut widget = s.view();
        tree.diff(widget.as_widget());
        let node = widget.as_widget_mut().layout(&mut tree, &r, &layout::Limits::new(size, size));
        let mut output = Vec::new();
        widget.as_widget_mut().update(&mut tree, &iced::Event::InputMethod(iced::advanced::input_method::Event::Commit(" more".into())),
            Layout::new(&node), iced::mouse::Cursor::Unavailable, &r, &mut iced::advanced::clipboard::Null,
            &mut iced::advanced::Shell::new(&mut output), &viewport);
        assert!(output.iter().any(|message| matches!(message, Message::ScopedComposer { context, action: crate::message::ComposerAction::Input(_) }
            if *context == s.composer_id)), "composer lost focus when notice visibility changed");
    }
}

#[test]
fn backward_archive_bounds_use_stored_server_time_instead_of_receipt_or_cached_time()
{
    use chrono::TimeZone;
    let server_time = chrono::Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
    for is_room in [false, true] {
        for live_boundary in [true, false] {
            let mut s = if is_room { completion_state(&[]) } else { state() };
            s.state = AppState::Connected;
            s.connected_jid = Some("me@example/device".into());
            s.chats[0].title = "alice".into();
            let conv = if is_room { "room@example" } else { "alice@example" };
            s.backlog_by_jid_mut(conv).unwrap().messages_mut().clear();
            s.body_editors.clear();
            let h = History::open_in_memory();
            let received = if live_boundary { server_time - chrono::Duration::hours(1) } else { server_time };
            h.insert(conv, "alice", "boundary body", received, !live_boundary, Some("boundary-sid")).unwrap();
            s.history = Some(h);
            if is_room { seed_tail_for(&s.history, conv, &mut s.rooms[0]); }
            else { seed_tail_for(&s.history, conv, &mut s.chats[0]); }
            // A cached view can also lag behind an authoritative stored stamp.
            if !live_boundary {
                if let room::message::Message::Chat { received, .. } = &mut s.backlog_by_jid_mut(conv).unwrap().messages_mut()[0] {
                    *received = server_time - chrono::Duration::hours(2);
                }
            }
            let (tx, mut rx) = tokio::sync::mpsc::channel(10);
            s.xmpp_cmd_tx = Some(tx);
            s.fetch_archive_active();
            let (query, end) = match rx.try_recv().unwrap().into_inner() {
                xmpp::XmppCommand::QueryArchive { query_id, end, cursor: None, forward: false, .. } => (query_id, end),
                other => panic!("unexpected archive command: {other:?}"),
            };
            if live_boundary {
                assert!(end.is_none(), "a slow local receipt clock must not exclude server history: room={is_room}, end={end:?}");
            } else {
                assert_eq!(end.as_deref(), Some(server_time.to_rfc3339().as_str()), "archive bound must use stored server time");
            }
            for (body, timestamp, sid) in [("older server message", server_time - chrono::Duration::seconds(10), "older-sid"),
                ("boundary body", server_time, "boundary-sid")] {
                let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage { to: None, query_id: query.clone(),
                    from: if is_room { "room@example/alice".into() } else { "alice@example/phone".into() }, body: body.into(),
                    timestamp, stanza_id: Some(sid.into()), origin_id: None }));
            }
            let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: query, complete: false,
                first: Some("older-cursor".into()), last: Some("boundary-cursor".into()) }));
            let stored = s.history.as_ref().unwrap().load_tail(conv, 10).unwrap();
            assert_eq!(stored.len(), 2);
            assert_eq!(stored[0].body, "older server message");
            assert_eq!(stored[1].received, server_time);
            let b = s.backlog_by_jid(conv).unwrap();
            assert_eq!(b.messages().len(), 2);
            assert!(matches!(&b.messages()[0], room::message::Message::Chat { body, .. } if body == "older server message"));
            assert!(matches!(&b.messages()[1], room::message::Message::Chat { received, .. } if *received == server_time));
            s.fetch_archive_active();
            assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::QueryArchive { cursor: Some(cursor), end: None, .. }
                if cursor == "older-cursor"));
        }
    }
}

#[test]
fn failed_archive_boundary_read_preserves_window_and_stays_retryable()
{
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    let h = History::open_in_memory();
    let stamp = chrono::Utc::now();
    h.insert("alice@example", "alice", "retained history", stamp, true, Some("server-id")).unwrap();
    s.history = Some(h);
    seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.history.as_ref().unwrap().disable_reads();
    let sequence = s.mam_seq;
    s.fetch_archive_active();
    assert!(rx.try_recv().is_err());
    assert!(s.mam_pending.is_empty());
    assert_eq!(s.mam_seq, sequence);
    assert!(!s.chats[0].mam_in_flight);
    assert!(!s.chats[0].mam_exhausted);
    assert!(s.chats[0].mam_cursor.is_none());
    assert!(matches!(&s.chats[0].messages[0], room::message::Message::Chat { body, .. } if body == "retained history"));
    s.history.as_ref().unwrap().enable_reads();
    s.fetch_archive_active();
    assert!(s.chats[0].mam_in_flight);
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::QueryArchive { end: Some(end), .. }
        if chrono::DateTime::parse_from_rfc3339(&end).unwrap().timestamp_millis() == stamp.timestamp_millis()));
}

#[tokio::test]
async fn malformed_replay_stamps_do_not_become_server_time_and_valid_archives_repair_them()
{
    use chrono::TimeZone;
    let server_time = chrono::Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
    for kind in ["room", "service", "legacy service", "direct", "private"] {
        for stamp in [None, Some("invalid stamp")] {
            for outage in [false, true] {
                for retired in [false, true] {
                    let is_room = matches!(kind, "room" | "service" | "legacy service");
                    if retired && !matches!(kind, "room" | "service") { continue; }
                    let mut s = if is_room { completion_state(&[]) } else { state() };
                    s.state = AppState::Connected;
                    s.connected_jid = Some("me@example/device".into());
                    let conv = if is_room { "room@example" } else if kind == "private" { "room@example/Alice" } else { "alice@example" };
                    if is_room { s.rooms[0].messages.clear(); }
                    else { s.chats = vec![chat(conv, &[])]; }
                    s.body_editors.clear();
                    s.history = Some(History::open_in_memory());
                    if outage { s.history.as_ref().unwrap().disable_writes(); }
                    let delay = Some(::xmpp::Delay { stamp: stamp.map(str::to_string), from: None, reason: None });
                    let body = "replayed body".to_string();
                    let stanza_id = Some("replay-id".to_string());
                    let native = match kind {
                        "room" => ::xmpp::XmppEvent::RoomMessage { room: conv.into(), nick: "alice".into(), body, delay, stanza_id, origin_id: None },
                        "service" => ::xmpp::XmppEvent::RoomServiceMessage { room: conv.into(), body, delay, stanza_id, origin_id: None },
                        "legacy service" => ::xmpp::XmppEvent::DirectMessage { from: conv.into(), body, delay, stanza_id, origin_id: None },
                        "private" => ::xmpp::XmppEvent::RoomPrivateMessage { from: conv.into(), body, delay, stanza_id, origin_id: None },
                        _ => ::xmpp::XmppEvent::DirectMessage { from: "alice@example/phone".into(), body, delay, stanza_id, origin_id: None },
                    };
                    let native = if matches!(kind, "room" | "service") {
                        let operation = xmpp::RoomOperation::new();
                        let context = if retired { "retired".into() } else { operation.id.clone() };
                        s.room_operations.insert(conv.into(), operation);
                        ::xmpp::XmppEvent::RoomEvent { room: conv.into(), context, event: Box::new(native) }
                    } else { native };
                    let before = chrono::Utc::now();
                    let forwarded = xmpp::forward_message_for_test(native).await;
                    let after = chrono::Utc::now();
                    let _ = s.update(Message::XmppEvent(forwarded));
                    if outage {
                        assert_eq!(s.pending_history_writes.len(), 1);
                        assert!(!s.pending_history_writes[0].delayed, "receipt time was retained as server time: {kind}, retired={retired}");
                    } else {
                        let h = s.history.as_ref().unwrap();
                        let stored = h.load_tail(conv, 10).unwrap();
                        assert_eq!(stored.len(), 1);
                        assert_eq!(stored[0].body, "replayed body");
                        assert!(stored[0].received.timestamp_millis() >= before.timestamp_millis() && stored[0].received.timestamp_millis() <= after.timestamp_millis());
                        assert_eq!(h.archived_received(conv, stored[0].id).unwrap(), None, "receipt time became authoritative: {kind}, retired={retired}");
                    }
                    s.mam_pending.insert("valid-archive".into(), crate::app::ArchiveContext { conversation: conv.into(), is_muc: is_room });
                    let from = match kind { "room" => "room@example/alice", "service" | "legacy service" => "room@example", "private" => conv, _ => "alice@example/phone" };
                    let valid = xmpp::forward_message_for_test(::xmpp::XmppEvent::ArchiveMessage { query_id: "valid-archive".into(), from: from.into(), to: None,
                        body: "replayed body".into(), timestamp: Some(server_time.to_rfc3339()), stanza_id: Some("replay-id".into()), origin_id: None }).await;
                    let _ = s.update(Message::XmppEvent(valid));
                    if outage {
                        assert!(s.pending_history_writes[0].delayed);
                        s.history.as_ref().unwrap().enable_writes();
                        let _ = s.update(Message::RetryHistoryWrites);
                    }
                    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: "valid-archive".into(), complete: true, first: None, last: None }));
                    let h = s.history.as_ref().unwrap();
                    let stored = h.load_tail(conv, 10).unwrap();
                    assert_eq!(stored.len(), 1);
                    assert_eq!(stored[0].received, server_time);
                    assert_eq!(h.archived_received(conv, stored[0].id).unwrap(), Some(server_time));
                    assert!(s.pending_history_writes.is_empty());
                    assert!(s.backlog_by_jid(conv).unwrap().messages().iter().any(|message|
                        matches!(message, room::message::Message::Chat { body, received, .. } if body == "replayed body" && *received == server_time)));
                }
            }
        }
    }
}

#[tokio::test]
async fn room_replay_needs_the_queued_origin_to_confirm_a_send()
{
    let mut s = completion_state(&[]);
    s.connected_jid = Some("me@example/device".into());
    s.history = Some(History::open_in_memory());
    let (tx, _rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.message_input = text_editor::Content::with_text("same body");
    let _ = s.update(Message::SendMessage);
    let queued = s.history.as_ref().unwrap().load_outbox().unwrap().remove(0);
    let (pending_id, pending_status) = s.rooms[0].messages.iter().find_map(|message| match message {
        room::message::Message::Chat { id, status, body, .. } if body == "same body" => Some((*id, *status)), _ => None,
    }).unwrap();
    // Unstamped history with our nickname and text, but another origin, is
    // somebody else's message: it must not confirm or retire this send.
    let replay = |origin_id: String| xmpp::forward_message_for_test(::xmpp::XmppEvent::RoomMessage {
        room: "room@example".into(), nick: "me".into(), body: "same body".into(),
        delay: Some(::xmpp::Delay { stamp: Some("invalid stamp".into()), from: None, reason: None }),
        stanza_id: None, origin_id: Some(origin_id),
    });
    let _ = s.update(Message::XmppEvent(replay(uuid::Uuid::new_v4().to_string()).await));
    assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 1);
    assert!(s.rooms[0].messages.iter().any(|message| matches!(message,
        room::message::Message::Chat { id, status, .. } if *id == pending_id && *status == pending_status)));
    assert_eq!(s.room_send_origins.get(&pending_id), Some(&queued.origin_id));
    // Our queued origin in history means the room delivered this send even
    // though its acknowledgement was lost; a replay must not deliver it again.
    let _ = s.update(Message::XmppEvent(replay(queued.origin_id.clone()).await));
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
    assert!(!s.rooms[0].messages.iter().any(|message| matches!(message,
        room::message::Message::Chat { id, .. } if *id == pending_id)));
    assert_eq!(s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap().iter().filter(|message| message.body == "same body").count(), 2);
}

#[tokio::test]
async fn unstamped_own_dm_copy_confirms_origin_without_adopting_a_server_timestamp()
{
    use chrono::TimeZone;
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.history = Some(History::open_in_memory());
    let (tx, _rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    let _ = s.update(Message::StartChat("me@example".into()));
    s.message_input = text_editor::Content::with_text("self note");
    let _ = s.update(Message::SendMessage);
    let queued = s.history.as_ref().unwrap().load_outbox().unwrap().remove(0);
    let original = s.history.as_ref().unwrap().load_tail("me@example", 10).unwrap().remove(0);
    let invalid = xmpp::forward_message_for_test(::xmpp::XmppEvent::DirectMessage { from: "me@example/phone".into(), body: "self note".into(),
        delay: Some(::xmpp::Delay { stamp: None, from: None, reason: None }), stanza_id: Some("own-id".into()), origin_id: Some(queued.origin_id.clone()) }).await;
    let _ = s.update(Message::XmppEvent(invalid));
    let h = s.history.as_ref().unwrap();
    let stored = h.load_tail("me@example", 10).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].id, original.id);
    assert_eq!(stored[0].received, original.received);
    assert_eq!(stored[0].delivery, storage::history::DeliveryState::Submitted(queued.id));
    assert!(h.load_outbox().unwrap().is_empty());
    assert_eq!(h.archived_received("me@example", original.id).unwrap(), None);
    let server_time = chrono::Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
    s.mam_pending.insert("valid-own".into(), crate::app::ArchiveContext { conversation: "me@example".into(), is_muc: false });
    let valid = xmpp::forward_message_for_test(::xmpp::XmppEvent::ArchiveMessage { query_id: "valid-own".into(), from: "me@example/phone".into(), to: Some("me@example".into()),
        body: "self note".into(), timestamp: Some(server_time.to_rfc3339()), stanza_id: Some("own-id".into()), origin_id: Some(queued.origin_id) }).await;
    let _ = s.update(Message::XmppEvent(valid));
    let h = s.history.as_ref().unwrap();
    assert_eq!(h.archived_received("me@example", original.id).unwrap(), Some(server_time));
    assert_eq!(h.load_tail("me@example", 10).unwrap().len(), 1);
}

#[test]
fn incomplete_catchup_without_cursor_progress_stays_retryable()
{
    for replies in [vec![None], vec![Some("")], vec![Some("A"), Some("A")], vec![Some("A"), Some("B"), Some("A")]] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        let h = History::open_in_memory();
        h.insert("alice@example", "alice", "retained history", chrono::Utc::now(), false, Some("stored")).unwrap();
        s.history = Some(h);
        seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
        s.chats[0].mam_cursor = Some("backward-position".into());
        s.chats[0].mam_in_flight = true;
        s.chats[0].anchored_bottom = false;
        s.message_input = text_editor::Content::with_text("unsent draft");
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        s.mam_pending.insert("backward-query".into(), crate::app::ArchiveContext { conversation: "alice@example".into(), is_muc: false });
        s.mam_pending.insert("unrelated".into(), crate::app::ArchiveContext { conversation: "bob@example".into(), is_muc: false });
        s.start_catchup("alice@example");
        let mut query = match rx.try_recv().unwrap().into_inner() {
            xmpp::XmppCommand::QueryArchive { query_id, cursor: None, .. } => query_id,
            command => panic!("unexpected catch-up request: {command:?}"),
        };
        for (index, first) in replies.iter().enumerate() {
            let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: query.clone(), complete: false,
                first: first.map(str::to_string), last: None }));
            if index + 1 < replies.len() {
                query = match rx.try_recv().unwrap().into_inner() {
                    xmpp::XmppCommand::QueryArchive { query_id, cursor, .. } if cursor.as_deref() == *first => query_id,
                    command => panic!("unexpected cursor continuation: {command:?}"),
                };
            }
        }
        assert!(!s.mam_caught_up.contains("alice@example"), "incomplete catch-up was marked successful: {replies:?}");
        assert!(!s.mam_catchup.contains_key("alice@example"));
        assert!(!s.mam_pending.contains_key(&query));
        assert!(s.mam_pending.contains_key("unrelated"));
        assert!(s.mam_pending.contains_key("backward-query"));
        assert!(s.chats[0].mam_in_flight);
        assert!(rx.try_recv().is_err(), "non-progressing cursor must not trigger another request");
        assert_eq!(s.message_input.text(), "unsent draft");
        assert_eq!(s.chats[0].mam_cursor.as_deref(), Some("backward-position"));
        assert_eq!(s.chats[0].messages.len(), 1);
        assert!(matches!(&s.chats[0].messages[0], room::message::Message::Chat { body, .. } if body == "retained history"));
        s.start_catchup("alice@example");
        let retry = match rx.try_recv().unwrap().into_inner() {
            xmpp::XmppCommand::QueryArchive { query_id, cursor: None, .. } => query_id,
            command => panic!("catch-up was not retryable: {command:?}"),
        };
        assert_ne!(retry, query);
        // A fresh sweep may use a cursor seen by its failed predecessor, and
        // an explicit complete page does not require another advancing cursor.
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: retry, complete: false, first: Some("A".into()), last: None }));
        let final_query = match rx.try_recv().unwrap().into_inner() {
            xmpp::XmppCommand::QueryArchive { query_id, cursor: Some(cursor), .. } if cursor == "A" => query_id,
            command => panic!("fresh retry retained old cursor history: {command:?}"),
        };
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: final_query, complete: true, first: Some("A".into()), last: None }));
        assert!(s.mam_caught_up.contains("alice@example"));
        assert!(s.mam_pending.contains_key("unrelated"));
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn catchup_batches_pause_without_losing_the_remaining_gap()
{
    for valid_last_cursor in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        let h = History::open_in_memory();
        h.insert("alice@example", "alice", "history", chrono::Utc::now(), false, Some("stored")).unwrap();
        s.history = Some(h);
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        s.start_catchup("alice@example");
        for page in 0..CATCHUP_MAX_PAGES {
            let query = match rx.try_recv().unwrap().into_inner() {
                xmpp::XmppCommand::QueryArchive { query_id, .. } => query_id,
                command => panic!("unexpected request: {command:?}"),
            };
            let first = if page + 1 == CATCHUP_MAX_PAGES && !valid_last_cursor { None } else { Some(format!("cursor-{page}")) };
            let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: query, complete: false, first, last: None }));
        }
        assert!(!s.mam_caught_up.contains("alice@example"), "a batch limit must not mark a remaining gap complete");
        assert_eq!(s.mam_catchup.contains_key("alice@example"), valid_last_cursor);
        assert!(rx.try_recv().is_err(), "a full batch pauses before continuing");
        if valid_last_cursor {
            let token = s.mam_catchup["alice@example"].continuation.unwrap();
            let _ = s.update(Message::ContinueArchiveSync { context: s.room_retry_context("alice@example"), token });
            let query = match rx.try_recv().unwrap().into_inner() {
                xmpp::XmppCommand::QueryArchive { query_id, cursor: Some(cursor), .. } if cursor == format!("cursor-{}", CATCHUP_MAX_PAGES - 1) => query_id,
                other => panic!("lost the remaining gap: {other:?}"),
            };
            let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: query, complete: true, first: None, last: None }));
            assert!(s.mam_caught_up.contains("alice@example"));
        }
    }
}

#[test]
fn archive_failures_release_only_their_own_query_state()
{
    for catchup_fails in [false, true] {
        for storage_failure in [false, true] {
            let mut s = state();
            s.state = AppState::Connected;
            s.connected_jid = Some("me@example/device".into());
            let h = History::open_in_memory();
            h.insert("alice@example", "alice", "retained", chrono::Utc::now(), false, Some("stored")).unwrap();
            s.history = Some(h);
            seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
            s.chats[0].mam_cursor = Some("backward-position".into());
            s.chats[0].anchored_bottom = false;
            s.message_input = text_editor::Content::with_text("draft");
            let (tx, mut rx) = tokio::sync::mpsc::channel(10);
            s.xmpp_cmd_tx = Some(tx);
            s.fetch_archive_active();
            let backward = match rx.try_recv().unwrap().into_inner() { xmpp::XmppCommand::QueryArchive { query_id, .. } => query_id, _ => unreachable!() };
            s.start_catchup("alice@example");
            let first_catchup = match rx.try_recv().unwrap().into_inner() { xmpp::XmppCommand::QueryArchive { query_id, .. } => query_id, _ => unreachable!() };
            let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: first_catchup, complete: false, first: Some("A".into()), last: None }));
            let catchup = match rx.try_recv().unwrap().into_inner() { xmpp::XmppCommand::QueryArchive { query_id, .. } => query_id, _ => unreachable!() };
            assert_eq!(s.mam_catchup["alice@example"].pages, 1);
            let failed = if catchup_fails { &catchup } else { &backward };
            let remaining = if catchup_fails { &backward } else { &catchup };
            if storage_failure {
                s.history.as_ref().unwrap().disable_writes();
                let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage { query_id: failed.clone(), from: "alice@example/phone".into(), to: None,
                    body: "pending page".into(), timestamp: chrono::Utc::now(), stanza_id: Some("pending".into()), origin_id: None }));
                assert_eq!(s.pending_history_writes.len(), 1);
                let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: failed.clone(), complete: true, first: Some("must-not-advance".into()), last: None }));
            } else {
                let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveFailed { query_id: failed.clone(), reason: "failed page".into() }));
            }
            assert!(!s.mam_pending.contains_key(failed));
            assert!(s.mam_pending.contains_key(remaining));
            assert_eq!(s.chats[0].mam_in_flight, catchup_fails, "catchup_fails={catchup_fails}, storage_failure={storage_failure}");
            assert_eq!(s.mam_catchup.contains_key("alice@example"), !catchup_fails);
            if !catchup_fails { assert_eq!(s.mam_catchup["alice@example"].pages, 1); }
            assert_eq!(s.chats[0].mam_cursor.as_deref(), Some("backward-position"));
            assert_eq!(s.chats[0].messages.len(), 1);
            assert_eq!(s.message_input.text(), "draft");
            if catchup_fails { s.fetch_archive_active(); } else { s.start_catchup("alice@example"); }
            assert!(rx.try_recv().is_err(), "failure enabled a duplicate of the still-active request");
            if storage_failure {
                s.history.as_ref().unwrap().enable_writes();
                let _ = s.update(Message::RetryHistoryWrites);
                assert!(s.pending_history_writes.is_empty());
            }
            let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: remaining.clone(), complete: true,
                first: Some("remaining-finished".into()), last: None }));
            assert!(!s.mam_pending.contains_key(remaining));
            assert!(!s.chats[0].mam_in_flight);
            assert_eq!(s.mam_caught_up.contains("alice@example"), !catchup_fails);
            assert_eq!(s.chats[0].mam_cursor.as_deref(), Some(if catchup_fails { "remaining-finished" } else { "backward-position" }));
        }
    }
}

#[test]
fn closed_command_queue_keeps_already_accepted_archive_requests_active()
{
    for catchup_unsent in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        let h = History::open_in_memory();
        h.insert("alice@example", "alice", "history", chrono::Utc::now(), false, Some("stored")).unwrap();
        s.history = Some(h);
        seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        s.xmpp_cmd_tx = Some(tx.clone());
        if catchup_unsent { s.fetch_archive_active(); } else { s.start_catchup("alice@example"); }
        let accepted = match rx.try_recv().unwrap().into_inner() { xmpp::XmppCommand::QueryArchive { query_id, .. } => query_id, _ => unreachable!() };
        tx.try_send(xmpp::XmppCommand::ForceReconnect).unwrap();
        if catchup_unsent { s.start_catchup("alice@example"); } else { s.fetch_archive_active(); }
        assert_eq!(s.xmpp_pending_commands.len(), 1);
        let unsent = match s.xmpp_pending_commands.front().unwrap().inner() { xmpp::XmppCommand::QueryArchive { query_id, .. } => query_id.clone(), _ => unreachable!() };
        drop(rx);
        let _ = s.update(Message::FlushXmppCommands);
        assert!(s.xmpp_cmd_tx.is_none());
        assert!(s.xmpp_pending_commands.is_empty());
        assert!(!s.mam_pending.contains_key(&unsent));
        assert!(s.mam_pending.contains_key(&accepted));
        assert_eq!(s.chats[0].mam_in_flight, catchup_unsent);
        assert_eq!(s.mam_catchup.contains_key("alice@example"), !catchup_unsent);
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: accepted, complete: true, first: Some("finished".into()), last: None }));
        assert!(s.mam_pending.is_empty());
        assert!(!s.chats[0].mam_in_flight);
        assert_eq!(s.mam_caught_up.contains("alice@example"), !catchup_unsent);
    }
}

#[test]
fn rejected_archive_submissions_do_not_leave_phantom_requests()
{
    for draining in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        let h = History::open_in_memory();
        h.insert("alice@example", "alice", "history", chrono::Utc::now(), false, Some("stored")).unwrap();
        s.history = Some(h);
        seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        s.xmpp_cmd_tx = Some(tx);
        s.start_catchup("alice@example");
        let catchup = match rx.try_recv().unwrap().into_inner() { xmpp::XmppCommand::QueryArchive { query_id, .. } => query_id, _ => unreachable!() };
        s.fetch_archive_active();
        let backward = match rx.try_recv().unwrap().into_inner() { xmpp::XmppCommand::QueryArchive { query_id, .. } => query_id, _ => unreachable!() };
        if draining { s.disconnecting = true; } else { s.xmpp_cmd_tx = None; }
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: catchup, complete: false, first: Some("next".into()), last: None }));
        assert_eq!(s.mam_pending.len(), 1, "a rejected continuation became a phantom request: draining={draining}");
        assert!(s.mam_pending.contains_key(&backward));
        assert!(s.chats[0].mam_in_flight);
        assert!(!s.mam_catchup.contains_key("alice@example"));
        assert!(!s.mam_caught_up.contains("alice@example"));
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn older_archive_failure_preserves_a_completed_recent_catchup()
{
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    let h = History::open_in_memory();
    h.insert("alice@example", "alice", "history", chrono::Utc::now(), false, Some("stored")).unwrap();
    s.history = Some(h);
    seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.start_catchup("alice@example");
    let catchup = match rx.try_recv().unwrap().into_inner() { xmpp::XmppCommand::QueryArchive { query_id, .. } => query_id, _ => unreachable!() };
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id: catchup, complete: true, first: None, last: None }));
    assert!(s.mam_caught_up.contains("alice@example"));
    s.fetch_archive_active();
    let backward = match rx.try_recv().unwrap().into_inner() { xmpp::XmppCommand::QueryArchive { query_id, .. } => query_id, _ => unreachable!() };
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveFailed { query_id: backward, reason: "older page unavailable".into() }));
    assert!(s.mam_caught_up.contains("alice@example"));
    assert!(!s.chats[0].mam_in_flight);
    assert!(!s.chats[0].mam_exhausted);
    s.start_catchup("alice@example");
    assert!(rx.try_recv().is_err(), "an older-page error restarted an already completed recent sweep");
    s.fetch_archive_active();
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::QueryArchive { query_id, .. } if query_id.starts_with("mam-b-")));
}

#[test]
fn recovered_archive_writes_restart_catchup_while_older_page_is_still_pending()
{
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    let h = History::open_in_memory();
    h.insert("alice@example", "alice", "retained", chrono::Utc::now(), false, Some("stored")).unwrap();
    s.history = Some(h);
    seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
    s.chats[0].mam_cursor = Some("older-position".into());
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.fetch_archive_active();
    let backward = match rx.try_recv().unwrap().into_inner() { xmpp::XmppCommand::QueryArchive { query_id, .. } => query_id, _ => unreachable!() };
    s.start_catchup("alice@example");
    let failed = match rx.try_recv().unwrap().into_inner() { xmpp::XmppCommand::QueryArchive { query_id, .. } => query_id, _ => unreachable!() };
    s.history.as_ref().unwrap().disable_writes();
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage {
        query_id: failed.clone(), from: "alice@example/phone".into(), to: None,
        body: "offline gap".into(), timestamp: chrono::Utc::now(), stanza_id: Some("gap".into()), origin_id: None,
    }));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete {
        query_id: failed.clone(), complete: true, first: None, last: None,
    }));
    assert!(!s.mam_pending.contains_key(&failed));
    assert!(!s.mam_catchup.contains_key("alice@example"));
    assert_eq!(s.pending_history_writes.len(), 1);
    s.history.as_ref().unwrap().enable_writes();
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.pending_history_writes.is_empty());
    let restarted = match rx.try_recv().expect("saving the last pending message must restart the interrupted catch-up").into_inner() {
        xmpp::XmppCommand::QueryArchive { query_id, cursor: None, .. } if query_id.starts_with("mam-c-") => query_id,
        command => panic!("unexpected restart: {command:?}"),
    };
    assert_ne!(restarted, failed);
    assert!(s.mam_pending.contains_key(&backward));
    assert!(s.chats[0].mam_in_flight);
    assert_eq!(s.chats[0].mam_cursor.as_deref(), Some("older-position"));
    // Recovering another page write cannot duplicate the restarted sweep.
    s.history.as_ref().unwrap().disable_writes();
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage {
        query_id: restarted.clone(), from: "alice@example/phone".into(), to: None,
        body: "another recovered message".into(), timestamp: chrono::Utc::now(), stanza_id: Some("gap-2".into()), origin_id: None,
    }));
    assert_eq!(s.pending_history_writes.len(), 1);
    s.history.as_ref().unwrap().enable_writes();
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.pending_history_writes.is_empty());
    assert!(rx.try_recv().is_err());
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete {
        query_id: restarted, complete: true, first: None, last: None,
    }));
    assert!(s.mam_caught_up.contains("alice@example"));
    assert!(s.chats[0].mam_in_flight);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete {
        query_id: backward, complete: true, first: Some("older-done".into()), last: None,
    }));
    assert!(!s.chats[0].mam_in_flight);
    assert!(s.chats[0].mam_exhausted);
    assert!(s.mam_caught_up.contains("alice@example"));
    assert!(s.mam_pending.is_empty());
}

#[test]
fn archive_merges_remap_search_results_and_preserve_the_focused_message()
{
    for other_conversation_active in [false, true] {
        for focus in ["duplicate", "canonical", "neighbour"] {
            let mut s = state();
            s.state = AppState::Connected;
            s.connected_jid = Some("me@example/device".into());
            let h = History::open_in_memory();
            let (queued, canonical) = h.enqueue_direct_message("alice@example", "me", "needle merged").unwrap();
            let old = chrono::Utc::now() - chrono::Duration::days(2);
            let duplicate = h.insert("alice@example", "me", "needle merged", old, true, Some("merged-copy")).unwrap().unwrap();
            let neighbour = h.insert("alice@example", "alice", "needle neighbour", old + chrono::Duration::days(1), false, Some("neighbour")).unwrap().unwrap();
            s.history = Some(h);
            seed_tail_for(&s.history, "alice@example", &mut s.chats[0]);
            if other_conversation_active {
                s.chats.push(chat("bob@example", &["bob stays visible"]));
                s.active = Some(Selection::Chat(1));
            }
            s.find = Some(crate::app::FindState { query: "needle".into(), all_scope: other_conversation_active, ..Default::default() });
            s.run_find_search();
            let focused = match focus { "duplicate" => duplicate, "canonical" => canonical.id, _ => neighbour };
            let find = s.find.as_mut().unwrap();
            assert_eq!(find.matches.len(), 3);
            find.index = find.matches.iter().position(|hit| hit.id == focused).unwrap();
            let old_scroll = uuid::Uuid::new_v4();
            find.pending_scroll = Some(old_scroll);
            find.scroll_id = old_scroll;
            s.message_input = text_editor::Content::with_text("keep this draft");
            s.mam_pending.insert("archive".into(), crate::app::ArchiveContext { conversation: "alice@example".into(), is_muc: false });
            let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage {
                query_id: "archive".into(), to: None, from: "me@example/other-device".into(), body: "needle merged".into(),
                timestamp: old, stanza_id: Some("merged-copy".into()), origin_id: Some(queued.origin_id),
            }));
            let rows = s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap();
            assert_eq!(rows.len(), 2);
            let find = s.find.as_ref().unwrap();
            assert_eq!(find.matches.iter().map(|hit| hit.id).collect::<Vec<_>>(), vec![canonical.id, neighbour],
                "search must contain each surviving row once");
            assert_eq!(find.matches[find.index].id, if focus == "neighbour" { neighbour } else { canonical.id });
            assert!(find.pending_scroll.is_none());
            assert_ne!(find.scroll_id, old_scroll);
            assert_eq!(s.message_input.text(), "keep this draft");
            assert_eq!(s.active, Some(Selection::Chat(usize::from(other_conversation_active))));
            if other_conversation_active {
                assert!(matches!(&s.chats[1].messages[..], [room::message::Message::Chat { body, .. }] if body == "bob stays visible"));
            }
            // Navigation to the reconciled hit must still load a real row.
            s.find.as_mut().unwrap().index = 0;
            let _ = s.jump_to_current_match();
            assert_eq!(s.active, Some(Selection::Chat(0)));
            assert!(s.chats[0].messages.iter().any(|message| matches!(message, room::message::Message::Chat { id, .. } if *id == canonical.id)));
            assert!(s.find.as_ref().unwrap().pending_scroll.is_some());
        }
    }
}

#[test]
fn unchanged_nickname_completion_preserves_redo_and_still_finishes_the_word()
{
    for nick in ["alice", "Åke"] {
        for backward in [false, true] {
            for inside_word in [false, true] {
                let mut s = completion_state(&[nick]);
                let draft = format!("ask {nick} rest");
                s.message_input = text_editor::Content::with_text(&draft);
                s.message_input.perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
                let _ = s.update(Message::InputAction(text_editor::Action::Edit(text_editor::Edit::Insert('!'))));
                let _ = s.update(Message::UndoInput);
                assert_eq!(s.message_input.text(), draft);
                let word_end = 4 + nick.len();
                let column = if inside_word { 4 + nick.chars().next().unwrap().len_utf8() } else { word_end };
                s.message_input.move_to(text_editor::Cursor { position: position(column), selection: None });
                let _ = s.update(if backward { Message::ShiftTabPressed } else { Message::TabPressed });
                assert_eq!(s.message_input.text(), draft);
                assert_eq!(s.message_input.cursor().position, position(word_end));
                assert_eq!(s.nick_complete.as_ref().unwrap().nick, nick);
                let _ = s.update(Message::RedoInput);
                assert_eq!(s.message_input.text(), format!("{draft}!"),
                    "unchanged completion must preserve redo: nick={nick}, backward={backward}, inside_word={inside_word}");
                let _ = s.update(Message::UndoInput);
                assert_eq!(s.message_input.text(), draft);
                assert_eq!(s.message_input.cursor().position, position(word_end));
            }
        }
    }
}

#[test]
fn detached_discard_cleanup_accepts_an_already_removed_outbox_entry()
{
    for already_removed in [false, true] {
        for logged_out in [false, true] {
            let old_account = format!("old-{already_removed}-{logged_out}@example");
            let current_account = format!("current-{already_removed}-{logged_out}@example");
            let mut s = state();
            s.state = AppState::Connected;
            s.connected_jid = Some(format!("{old_account}/device"));
            let old = storage::open_history(&old_account, Some(&old_account)).unwrap();
            let outgoing = old.enqueue_outbox_message("alice@example", false, "discard old body").unwrap();
            s.history = Some(old);
            let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendRejected {
                id: outgoing.id, conversation: "alice@example".into(), body: "discard old body".into(), reason: "refused".into(),
            }));
            let token = s.rejected_sends[0].token;
            s.history.as_ref().unwrap().disable_writes();
            let _ = s.update(Message::DiscardRejectedSend(token));
            let key = (Some(old_account.clone()), outgoing.id);
            assert!(s.pending_outbox_deletes.contains(&key));
            assert!(s.discarded_outbox.contains(&key));
            assert!(s.rejected_sends.is_empty());
            let old = s.history.take().unwrap();
            old.enable_writes();
            // Model a completed deletion whose success was not observed by
            // the UI. Retrying this persisted state must be idempotent.
            if already_removed { old.remove_outbox(outgoing.id).unwrap(); }
            if logged_out {
                let _ = s.update(Message::Disconnect);
            } else {
                let current = storage::open_history(&current_account, Some(&old_account)).unwrap();
                let (queued, stored) = current.enqueue_direct_message("alice@example", "current", "keep current body").unwrap();
                assert_eq!(queued.id, outgoing.id);
                s.connected_jid = Some(format!("{current_account}/device"));
                s.saved_config.jid = Some(format!("{current_account}/device"));
                s.chats[0].messages = vec![room::message::Message::Chat {
                    id: stored.id, from: stored.sender, body: stored.body, received: stored.received, status: ChatStatus::Queued(queued.id),
                }];
                s.history = Some(current);
            }
            let _ = s.update(Message::RetryHistoryWrites);
            assert!(!s.pending_outbox_deletes.contains(&key), "already_removed={already_removed}, logged_out={logged_out}");
            assert!(!s.discarded_outbox.contains(&key));
            assert!(!s.has_unsaved_message_updates());
            assert!(old.load_outbox().unwrap().is_empty());
            assert!(old.outgoing_by_origin(&outgoing.origin_id).unwrap().is_none());
            if !logged_out {
                let current = s.history.as_ref().unwrap().load_outbox().unwrap();
                assert_eq!(current.len(), 1);
                assert_eq!(current[0].body, "keep current body");
                assert!(matches!(&s.chats[0].messages[..], [room::message::Message::Chat { body, status: ChatStatus::Queued(id), .. }]
                    if body == "keep current body" && *id == outgoing.id));
            }
        }
    }
}

#[test]
fn corrected_message_times_reorder_find_hits_without_changing_focus_or_conversation()
{
    use chrono::TimeZone;
    let base = chrono::Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
    for (is_room, corrected_seconds) in [(false, 0), (false, 60), (true, 0), (true, 60)] {
        for other_conversation_active in [false, true] {
            for focus_corrected in [false, true] {
                let mut s = if is_room { completion_state(&[]) } else { state() };
                s.state = AppState::Connected;
                s.connected_jid = Some("me@example/device".into());
                s.history = Some(History::open_in_memory());
                let conv = if is_room { "room@example" } else { "alice@example" };
                s.backlog_by_jid_mut(conv).unwrap().messages_mut().clear();
                let event = |body: &str, seconds: i64, delayed: bool| {
                    let timestamp = base + chrono::Duration::seconds(seconds);
                    if is_room {
                        xmpp::XmppEvent::RoomMessage { room: conv.into(), nick: "alice".into(), body: body.into(), timestamp,
                            delayed, stanza_id: Some(body.into()), origin_id: None }
                    } else {
                        xmpp::XmppEvent::DirectMessage { from: "alice@example/phone".into(), body: body.into(), timestamp,
                            delayed, stanza_id: Some(body.into()), origin_id: None }
                    }
                };
                let _ = s.update(Message::XmppEvent(event("needle corrected", 120, false)));
                let _ = s.update(Message::XmppEvent(event("needle neighbour", 60, false)));
                if other_conversation_active {
                    s.chats.push(chat("bob@example", &["keep this view"]));
                    s.active = Some(Selection::Chat(s.chats.len() - 1));
                }
                let active = s.active;
                s.message_input = text_editor::Content::with_text("keep this draft");
                s.find = Some(crate::app::FindState { query: "needle".into(), all_scope: other_conversation_active, ..Default::default() });
                s.run_find_search();
                let find = s.find.as_mut().unwrap();
                let neighbour = find.matches[0].id;
                let corrected = find.matches[1].id;
                find.index = usize::from(focus_corrected);
                let old_scroll = uuid::Uuid::new_v4();
                find.pending_scroll = Some(old_scroll);
                find.scroll_id = old_scroll;
                let _ = s.update(Message::XmppEvent(event("needle corrected", corrected_seconds, true)));
                let find = s.find.as_ref().unwrap();
                assert_eq!(find.matches.iter().map(|hit| hit.id).collect::<Vec<_>>(), vec![corrected, neighbour],
                    "room={is_room}, other_active={other_conversation_active}, focus_corrected={focus_corrected}");
                assert_eq!(find.matches[find.index].id, if focus_corrected { corrected } else { neighbour });
                assert!(find.pending_scroll.is_none());
                assert_ne!(find.scroll_id, old_scroll);
                assert_eq!(s.active, active);
                assert_eq!(s.message_input.text(), "keep this draft");
                s.find.as_mut().unwrap().index = 0;
                let _ = s.update(Message::FindNext);
                assert_eq!(s.find.as_ref().unwrap().matches[s.find.as_ref().unwrap().index].id, neighbour);
                assert_eq!(s.active_conv_jid().as_deref(), Some(conv));
            }
        }
    }
}

#[test]
fn blocked_detached_delivery_reports_do_not_starve_received_history()
{
    struct BlockReports(rusqlite::Connection);
    impl Drop for BlockReports {
        fn drop(&mut self) { let _ = self.0.execute_batch("DROP TRIGGER IF EXISTS snack_test_block_reports"); }
    }
    for pending_errors in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("old@example/device".into());
        let old = storage::open_history("old@example", Some("old@example")).unwrap();
        let outgoing: Vec<_> = (0..HISTORY_RETRY_BATCH).map(|i|
            old.enqueue_outbox_message("alice@example", false, &format!("queued-{pending_errors}-{i}")).unwrap()).collect();
        let db = rusqlite::Connection::open(storage::history_path().unwrap()).unwrap();
        db.execute_batch("CREATE TRIGGER snack_test_block_reports BEFORE UPDATE OF error ON outgoing_messages
            BEGIN SELECT RAISE(FAIL, 'delivery report storage unavailable'); END;").unwrap();
        let blocked = BlockReports(db);
        for outgoing in &outgoing {
            if pending_errors {
                s.pending_message_errors.push(crate::app::PendingMessageError {
                    account: Some("old@example".into()), own_jid: "old@example".into(),
                    origin_id: outgoing.origin_id.clone(), from: Some("alice@example/phone".into()), reason: "refused".into(),
                });
            } else {
                s.record_rejected_send(outgoing.id, outgoing.conversation.clone(), outgoing.body.clone(), "refused".into(), false);
            }
        }
        old.disable_writes();
        s.history = Some(old);
        let body = format!("received while reports were blocked: {pending_errors}");
        s.persist_message("alice@example", "alice", &body, chrono::Utc::now(), false, Some(&body));
        assert_eq!(s.pending_history_writes.len(), 1);
        let old = s.history.take().unwrap();
        old.enable_writes();
        let other = storage::open_history("z-good@example", Some("old@example")).unwrap();
        other.disable_writes();
        s.connected_jid = Some("z-good@example/device".into());
        s.history = Some(other);
        let other_body = format!("another account's received message: {pending_errors}");
        s.persist_message("alice@example", "alice", &other_body, chrono::Utc::now(), false, Some(&other_body));
        assert_eq!(s.pending_history_writes.len(), 2);
        let other = s.history.take().unwrap();
        other.enable_writes();
        let _ = s.update(Message::Disconnect);
        let task = s.update(Message::RetryHistoryWrites);
        assert!(s.pending_history_writes.is_empty(), "pending_errors={pending_errors}");
        assert!(old.load_tail("alice@example", 1000).unwrap().iter().any(|message| message.body == body));
        assert!(other.load_tail("alice@example", 1000).unwrap().iter().any(|message| message.body == other_body));
        assert!(task.units() > 0, "successful message recovery schedules continued work");
        if pending_errors { assert_eq!(s.pending_message_errors.len(), HISTORY_RETRY_BATCH); }
        else { assert_eq!(s.rejected_sends.iter().filter(|send| !send.saved).count(), HISTORY_RETRY_BATCH); }
        // Once only blocked reports remain, wait for the normal retry timer.
        assert_eq!(s.update(Message::RetryHistoryWrites).units(), 0);
        drop(blocked);
        let _ = s.update(Message::RetryHistoryWrites);
        assert!(s.pending_message_errors.is_empty());
        assert!(s.rejected_sends.iter().all(|send| send.saved));
        assert!(!s.has_unsaved_message_updates());
        assert!(old.load_outbox().unwrap().is_empty());
    }
}

#[test]
fn active_delivery_report_recovery_yields_between_batches()
{
    for pending_errors in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        let h = History::open_in_memory();
        let count = HISTORY_RETRY_BATCH + 7;
        for i in 0..count {
            let queued = h.enqueue_outbox_message("alice@example", false, &format!("report-{i}")).unwrap();
            if pending_errors {
                s.pending_message_errors.push(crate::app::PendingMessageError {
                    account: Some("me@example".into()), own_jid: "me@example".into(),
                    origin_id: queued.origin_id, from: Some("alice@example/phone".into()), reason: "refused".into(),
                });
            } else {
                s.record_rejected_send(queued.id, queued.conversation, queued.body, "refused".into(), false);
            }
        }
        s.history = Some(h);
        let task = s.update(Message::RetryHistoryWrites);
        assert_eq!(s.history.as_ref().unwrap().load_rejected_outbox().unwrap().len(), HISTORY_RETRY_BATCH,
            "pending_errors={pending_errors}: one update must not synchronously write the entire backlog");
        assert!(task.units() > 0, "successful recovery schedules the next bounded batch");
        if pending_errors { assert_eq!(s.pending_message_errors.len(), 7); }
        else { assert_eq!(s.rejected_sends.iter().filter(|send| !send.saved).count(), 7); }
        let task = s.update(Message::RetryHistoryWrites);
        assert_eq!(s.history.as_ref().unwrap().load_rejected_outbox().unwrap().len(), count);
        assert!(s.pending_message_errors.is_empty());
        assert!(s.rejected_sends.iter().all(|send| send.saved));
        assert!(!s.has_unsaved_message_updates());
        assert_eq!(task.units(), 0);
    }
}

#[test]
fn active_rejection_retry_reaches_writable_rows_without_reordering_notices()
{
    struct BlockReports(rusqlite::Connection);
    impl Drop for BlockReports {
        fn drop(&mut self) { let _ = self.0.execute_batch("DROP TRIGGER IF EXISTS snack_test_block_report_prefix"); }
    }
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    let h = storage::open_history("me@example", Some("me@example")).unwrap();
    for i in 0..HISTORY_RETRY_BATCH + 7 {
        let queued = h.enqueue_outbox_message("alice@example", false, &format!("report-{i}")).unwrap();
        s.record_rejected_send(queued.id, queued.conversation, queued.body, "refused".into(), false);
    }
    let tokens: Vec<_> = s.rejected_sends.iter().map(|send| send.token).collect();
    let db = rusqlite::Connection::open(storage::history_path().unwrap()).unwrap();
    db.execute_batch(&format!("CREATE TRIGGER snack_test_block_report_prefix BEFORE UPDATE OF error ON outgoing_messages
        WHEN OLD.outbox_id <= {HISTORY_RETRY_BATCH} BEGIN SELECT RAISE(FAIL, 'blocked report prefix'); END;")).unwrap();
    let blocked = BlockReports(db);
    s.history = Some(h);
    assert_eq!(s.update(Message::RetryHistoryWrites).units(), 0);
    assert_eq!(s.rejected_sends.iter().filter(|send| send.saved).count(), 0);
    assert!(s.update(Message::RetryHistoryWrites).units() > 0);
    assert_eq!(s.rejected_sends.iter().filter(|send| send.saved).count(), 7);
    assert_eq!(s.history.as_ref().unwrap().load_rejected_outbox().unwrap().len(), 7);
    assert_eq!(s.update(Message::RetryHistoryWrites).units(), 0, "only blocked reports remain");
    assert_eq!(s.rejected_sends.iter().map(|send| send.token).collect::<Vec<_>>(), tokens);
    drop(blocked);
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.rejected_sends.iter().all(|send| send.saved));
    assert!(!s.has_unsaved_message_updates());
}

#[test]
fn failed_report_lookups_do_not_starve_later_unqueued_reports()
{
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.chats[0].messages.clear();
    let h = History::open_in_memory();
    let error = |origin_id| crate::app::PendingMessageError {
        account: Some("me@example".into()), own_jid: "me@example".into(), origin_id,
        from: Some("alice@example/phone".into()), reason: "refused".into(),
    };
    for i in 0..HISTORY_RETRY_BATCH {
        let queued = h.enqueue_outbox_message("alice@example", false, &format!("queued-{i}")).unwrap();
        s.pending_message_errors.push(error(queued.origin_id));
    }
    let origins: Vec<_> = (0..7).map(|i| {
        let origin = uuid::Uuid::new_v4().to_string();
        h.store_unqueued_direct(&origin, "alice@example", "me", &format!("unqueued-{i}"), chrono::Utc::now(), UnqueuedDelivery::Unconfirmed).unwrap();
        s.pending_message_errors.push(error(origin.clone()));
        origin
    }).collect();
    // These reports need only messages-table context, so they can be resolved
    // even while outgoing-message lookups for the first full batch fail.
    h.disable_outgoing_reads();
    s.history = Some(h);
    assert_eq!(s.update(Message::RetryHistoryWrites).units(), 0);
    assert_eq!(s.pending_message_errors.len(), HISTORY_RETRY_BATCH + 7);
    assert!(s.update(Message::RetryHistoryWrites).units() > 0);
    assert_eq!(s.pending_message_errors.len(), HISTORY_RETRY_BATCH);
    for origin in origins {
        let (_, stored) = s.history.as_ref().unwrap().unqueued_direct(&origin).unwrap().unwrap();
        assert_eq!(stored.delivery, storage::history::DeliveryState::UnqueuedFailed);
    }
    assert_eq!(s.update(Message::RetryHistoryWrites).units(), 0);
    s.history.as_ref().unwrap().enable_outgoing_reads();
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(s.pending_message_errors.is_empty());
    assert!(s.rejected_sends.iter().all(|send| send.saved));
    assert!(!s.has_unsaved_message_updates());
}

#[test]
fn detached_retries_rotate_between_busy_accounts()
{
    let mut s = Snack::test_state();
    for (owner, count) in [("a-busy@example", 2 * HISTORY_RETRY_BATCH + 7), ("z-waiting@example", 1)] {
        for index in 0..count {
            s.pending_history_writes.push(crate::app::PendingHistoryWrite { receipt: uuid::Uuid::new_v4(),
                account: Some(owner.into()), conversation: "alice@example".into(), sender: "alice".into(), body: format!("{owner}-{index}"),
                received: chrono::Utc::now(), delayed: false, stanza_id: Some(format!("{owner}-{index}")), temp_id: -(index as i64 + 1),
                outbox_id: None, own_origin: None, own_room_origin: false, allow_outgoing_fuzzy: false,
            });
        }
    }
    assert!(s.update(Message::RetryHistoryWrites).units() > 0);
    let busy = storage::open_history("a-busy@example", Some("a-busy@example")).unwrap();
    let waiting = storage::open_history("z-waiting@example", Some("a-busy@example")).unwrap();
    assert_eq!(busy.load_tail("alice@example", 1000).unwrap().len(), HISTORY_RETRY_BATCH);
    assert!(waiting.load_tail("alice@example", 10).unwrap().is_empty());
    assert!(s.update(Message::RetryHistoryWrites).units() > 0);
    assert_eq!(waiting.load_tail("alice@example", 10).unwrap().len(), 1, "another account must get a turn before the busy account drains");
    assert_eq!(busy.load_tail("alice@example", 1000).unwrap().len(), 2 * HISTORY_RETRY_BATCH - 1);
    assert_eq!(s.pending_history_writes.len(), 8);
    assert_eq!(s.update(Message::RetryHistoryWrites).units(), 0);
    assert!(s.pending_history_writes.is_empty());
    assert!(s.detached_retry_after.is_none());
    assert_eq!(busy.load_tail("alice@example", 1000).unwrap().len(), 2 * HISTORY_RETRY_BATCH + 7);
}

#[test]
fn detached_retries_pass_blocked_reports_without_sharing_active_cursors()
{
    struct BlockReport(rusqlite::Connection);
    impl Drop for BlockReport {
        fn drop(&mut self) { let _ = self.0.execute_batch("DROP TRIGGER IF EXISTS snack_test_block_one_report"); }
    }
    for pending_errors in [false, true] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("current@example/device".into());
        s.record_rejected_send(-999, "bob@example".into(), "dismiss this notice".into(), "refused".into(), true);
        let dismissed = s.rejected_sends[0].token;
        s.connected_jid = Some("old@example/device".into());
        let old = storage::open_history("old@example", Some("old@example")).unwrap();
        let blocked_send = old.enqueue_outbox_message("alice@example", false, "blocked report").unwrap();
        let writable_send = old.enqueue_outbox_message("alice@example", false, "writable report").unwrap();
        for send in [&blocked_send, &writable_send] {
            if pending_errors {
                s.pending_message_errors.push(crate::app::PendingMessageError {
                    account: Some("old@example".into()), own_jid: "old@example".into(), origin_id: send.origin_id.clone(),
                    from: Some("alice@example/phone".into()), reason: "refused".into(),
                });
            } else {
                s.record_rejected_send(send.id, send.conversation.clone(), send.body.clone(), "refused".into(), false);
            }
        }
        let db = rusqlite::Connection::open(storage::history_path().unwrap()).unwrap();
        db.execute_batch(&format!("CREATE TRIGGER snack_test_block_one_report BEFORE UPDATE OF error ON outgoing_messages
            WHEN OLD.outbox_id = {} BEGIN SELECT RAISE(FAIL, 'one blocked report'); END;", blocked_send.id)).unwrap();
        let blocked = BlockReport(db);
        let current = History::open_in_memory();
        let current_send = current.enqueue_outbox_message("alice@example", false, "current account body").unwrap();
        s.connected_jid = Some("current@example/device".into());
        s.record_rejected_send(current_send.id, current_send.conversation, current_send.body, "current refusal".into(), false);
        current.disable_writes();
        s.history = Some(current);
        assert_eq!(s.update(Message::RetryHistoryWrites).units(), 0);
        assert!(!old.load_rejected_outbox().unwrap().iter().any(|(send, _)| send.id == writable_send.id));
        // Removing an earlier notice shifts vector indices. Retry positions
        // must follow stable notice identities rather than those indices.
        let _ = s.update(Message::DiscardRejectedSend(dismissed));
        let original_tokens: Vec<_> = s.rejected_sends.iter().map(|send| send.token).collect();
        assert!(s.update(Message::RetryHistoryWrites).units() > 0);
        let reports = old.load_rejected_outbox().unwrap();
        assert!(reports.iter().any(|(send, _)| send.id == writable_send.id), "pending_errors={pending_errors}");
        assert!(!reports.iter().any(|(send, _)| send.id == blocked_send.id));
        assert_eq!(s.update(Message::RetryHistoryWrites).units(), 0);
        assert!(s.history.as_ref().unwrap().load_rejected_outbox().unwrap().is_empty());
        assert_eq!(s.rejected_sends.iter().take(original_tokens.len()).map(|send| send.token).collect::<Vec<_>>(), original_tokens);
        drop(blocked);
        s.history.as_ref().unwrap().enable_writes();
        let _ = s.update(Message::RetryHistoryWrites);
        assert!(s.pending_message_errors.is_empty());
        assert!(s.rejected_sends.iter().all(|send| send.saved));
        assert!(!s.has_unsaved_message_updates());
        assert!(s.rejected_retry_cursors.is_empty());
        assert!(old.load_rejected_outbox().unwrap().iter().any(|(send, _)| send.id == blocked_send.id));
        assert_eq!(s.history.as_ref().unwrap().load_rejected_outbox().unwrap()[0].0.body, "current account body");
    }
}

#[test]
fn detached_retries_bound_failed_opens_and_resume_at_the_next_account()
{
    let mut s = Snack::test_state();
    let legacy = storage::history_path().unwrap();
    std::fs::create_dir_all(&legacy).unwrap(); // Every account open must fail.
    for index in 0..HISTORY_RETRY_BATCH + 7 {
        s.pending_history_writes.push(crate::app::PendingHistoryWrite { receipt: uuid::Uuid::new_v4(),
            account: Some(format!("blocked-{index:03}@example")), conversation: "alice@example".into(), sender: "alice".into(), body: "keep me".into(),
            received: chrono::Utc::now(), delayed: false, stanza_id: None, temp_id: -(index as i64 + 1),
            outbox_id: None, own_origin: None, own_room_origin: false, allow_outgoing_fuzzy: false,
        });
    }
    assert_eq!(s.update(Message::RetryHistoryWrites).units(), 0);
    assert_eq!(s.detached_retry_after.as_deref(), Some(format!("blocked-{:03}@example", HISTORY_RETRY_BATCH - 1).as_str()));
    assert_eq!(s.update(Message::RetryHistoryWrites).units(), 0);
    let last = (2 * HISTORY_RETRY_BATCH - 1) % (HISTORY_RETRY_BATCH + 7);
    assert_eq!(s.detached_retry_after.as_deref(), Some(format!("blocked-{last:03}@example").as_str()));
    assert_eq!(s.pending_history_writes.len(), HISTORY_RETRY_BATCH + 7);
    std::fs::remove_dir(legacy).unwrap();
}

#[test]
fn corrected_history_boundaries_do_not_skip_local_or_archive_page_rows()
{
    use chrono::TimeZone;
    let base = chrono::Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
    for is_room in [false, true] {
        for (archive_page, forward_correction) in [(false, false), (true, false), (true, true)] {
            let mut s = if is_room { completion_state(&[]) } else { state() };
            s.state = AppState::Connected;
            s.connected_jid = Some("me@example/device".into());
            s.window_focused = true;
            let conv = if is_room { "room@example" } else { "alice@example" };
            let h = History::open_in_memory();
            let gap_time = base + chrono::Duration::seconds(if forward_correction { 150 } else { 60 });
            if !archive_page { h.insert(conv, "alice", "gap", gap_time, true, Some("gap")).unwrap(); }
            let boundary = h.insert(conv, "alice", "boundary", base + chrono::Duration::seconds(120), false, Some("boundary")).unwrap().unwrap();
            let neighbour = h.insert(conv, "alice", "neighbour", base + chrono::Duration::seconds(180), false, Some("neighbour")).unwrap().unwrap();
            let window = h.load_tail(conv, 2).unwrap();
            s.history = Some(h);
            s.backlog_by_jid_mut(conv).unwrap().set_window(window, !archive_page, true);
            s.body_editors.clear();
            s.sync_body_editors();
            s.backlog_by_jid_mut(conv).unwrap().set_read_marker(Some(1));
            if is_room { s.rooms[0].history_markers = vec![1]; }
            s.selected_msgs.insert(boundary);
            s.select_anchor = Some(boundary);
            let query_id = if archive_page {
                let (tx, mut rx) = tokio::sync::mpsc::channel(10);
                s.xmpp_cmd_tx = Some(tx);
                s.fetch_archive_active();
                match rx.try_recv().unwrap().into_inner() { xmpp::XmppCommand::QueryArchive { query_id, .. } => Some(query_id), _ => unreachable!() }
            } else { None };
            let timestamp = base + chrono::Duration::seconds(if forward_correction { 240 } else { 0 });
            let from = if is_room { "room@example/alice" } else { "alice@example/phone" };
            let corrected = if let Some(query_id) = &query_id {
                xmpp::XmppEvent::ArchiveMessage { query_id: query_id.clone(), to: None, from: from.into(), body: "boundary".into(),
                    timestamp, stanza_id: Some("boundary".into()), origin_id: None }
            } else if is_room {
                xmpp::XmppEvent::RoomMessage { room: conv.into(), nick: "alice".into(), body: "boundary".into(), timestamp,
                    delayed: true, stanza_id: Some("boundary".into()), origin_id: None }
            } else {
                xmpp::XmppEvent::DirectMessage { from: from.into(), body: "boundary".into(), timestamp,
                    delayed: true, stanza_id: Some("boundary".into()), origin_id: None }
            };
            let _ = s.update(Message::XmppEvent(corrected));
            if let Some(query_id) = query_id {
                let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage { query_id: query_id.clone(), to: None, from: from.into(),
                    body: "gap".into(), timestamp: gap_time, stanza_id: Some("gap".into()), origin_id: None }));
                let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id, complete: true, first: Some("gap".into()), last: None }));
            } else {
                assert!(s.page_older_active().unwrap(), "local history must not skip the gap after correcting its boundary");
            }
            let stored = s.history.as_ref().unwrap().load_tail(conv, 10).unwrap();
            assert_eq!(stored.len(), 3);
            let expected: Vec<_> = stored.iter().map(|row| row.id).collect();
            let b = s.backlog_by_jid(conv).unwrap();
            let actual: Vec<_> = b.messages().iter().filter_map(|message| match message { room::message::Message::Chat { id, .. } => Some(*id), _ => None }).collect();
            assert_eq!(actual, expected, "room={is_room}, archive={archive_page}, forward={forward_correction}");
            assert_eq!(b.read_boundary().unwrap().id, Some(neighbour));
            assert_eq!(b.read_marker(), Some(if forward_correction { 1 } else { 2 }));
            assert!(!b.anchored_bottom());
            assert_eq!(s.selected_msgs, [boundary].into_iter().collect());
            assert_eq!(s.select_anchor, Some(boundary));
            if is_room { assert_eq!(s.rooms[0].history_markers, vec![if forward_correction { 1 } else { 2 }]); }
        }
    }
}

#[test]
fn archive_completion_respects_remaining_history_window_capacity()
{
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    let h = History::open_in_memory();
    let base = chrono::Utc::now();
    for i in 0..backlog::MAX_LOADED - 10 {
        h.insert("alice@example", "alice", &format!("retained-{i}"), base + chrono::Duration::seconds(i as i64), false, Some(&format!("retained-{i}"))).unwrap();
    }
    s.chats[0].set_window(h.load_tail("alice@example", backlog::MAX_LOADED).unwrap(), false, true);
    s.history = Some(h);
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.fetch_archive_active();
    let query_id = match rx.try_recv().unwrap().into_inner() { xmpp::XmppCommand::QueryArchive { query_id, .. } => query_id, _ => unreachable!() };
    for i in 1..=backlog::PAGE_SIZE {
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage { query_id: query_id.clone(), to: None, from: "alice@example/phone".into(),
            body: format!("archived-{i}"), timestamp: base - chrono::Duration::seconds(i as i64), stanza_id: Some(format!("archived-{i}")), origin_id: None }));
    }
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id, complete: true, first: Some("first".into()), last: None }));
    assert_eq!(s.chats[0].messages.len(), backlog::MAX_LOADED);
    assert!(s.chats[0].has_older);
    assert!(!s.chats[0].anchored_bottom);
    assert_eq!(s.history.as_ref().unwrap().load_tail("alice@example", 10000).unwrap().len(), backlog::MAX_LOADED - 10 + backlog::PAGE_SIZE);
}

#[test]
fn local_history_cursor_crosses_corrected_rows_and_resets_with_the_window()
{
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.window_focused = true;
    let h = History::open_in_memory();
    let base = chrono::Utc::now();
    for i in 1..=2 * backlog::PAGE_SIZE + 7 {
        h.insert("alice@example", "alice", &format!("gap-{i}"), base + chrono::Duration::seconds(i as i64), true, Some(&format!("gap-{i}"))).unwrap();
    }
    h.insert("alice@example", "alice", "boundary", base + chrono::Duration::seconds(1000), false, Some("boundary")).unwrap();
    h.insert("alice@example", "alice", "neighbour", base + chrono::Duration::seconds(2000), false, Some("neighbour")).unwrap();
    s.chats[0].set_window(h.load_tail("alice@example", 2).unwrap(), true, true);
    s.history = Some(h);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage { from: "alice@example/phone".into(), body: "boundary".into(),
        timestamp: base, delayed: true, stanza_id: Some("boundary".into()), origin_id: None }));
    let cursor = s.chats[0].local_cursor;
    s.history.as_ref().unwrap().disable_reads();
    assert!(s.page_older_active().is_err());
    assert_eq!(s.chats[0].local_cursor, cursor);
    assert_eq!(s.chats[0].messages.len(), 2);
    s.history.as_ref().unwrap().enable_reads();
    let expected: Vec<_> = s.history.as_ref().unwrap().load_tail("alice@example", 1000).unwrap().iter().map(|row| row.id).collect();
    for reset in ["initial", "trim", "tail", "search"] {
        match reset {
            "trim" => s.chats[0].trim_front_to(backlog::LIVE_TAIL_CAP),
            "tail" => seed_tail_for(&s.history, "alice@example", &mut s.chats[0]),
            "search" => s.chats[0].set_window(s.history.as_ref().unwrap().load_tail("alice@example", 2).unwrap(), true, true),
            _ => {},
        }
        s.chats[0].set_anchored_bottom(false);
        let mut pages = 0;
        while s.page_older_active().unwrap() {
            pages += 1;
            assert!(pages <= 3, "paging must advance across duplicate corrected rows: {reset}");
        }
        let actual: Vec<_> = s.chats[0].messages.iter().filter_map(|message| match message {
            room::message::Message::Chat { id, .. } => Some(*id), _ => None,
        }).collect();
        assert_eq!(actual, expected, "window reset: {reset}");
        assert!(!s.chats[0].has_older);
        assert!(!s.chats[0].anchored_bottom);
    }
}

#[test]
fn local_paging_skips_visible_rows_and_reaches_the_server_in_the_same_scroll()
{
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.window_focused = true;
    let h = History::open_in_memory();
    let now = chrono::Utc::now();
    h.insert("alice@example", "alice", "boundary", now, false, Some("boundary")).unwrap();
    h.insert("alice@example", "alice", "neighbour", now + chrono::Duration::seconds(60), false, Some("neighbour")).unwrap();
    s.chats[0].set_window(h.load_tail("alice@example", 10).unwrap(), false, true);
    s.history = Some(h);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage { from: "alice@example/phone".into(), body: "boundary".into(),
        timestamp: now - chrono::Duration::seconds(60), delayed: true, stanza_id: Some("boundary".into()), origin_id: None }));
    s.chats[0].has_older = true; // A conservative cached hint may include visible rows.
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    let _ = s.update_scroll_position(false, true);
    assert_eq!(s.chats[0].messages.len(), 2);
    assert!(!s.chats[0].has_older);
    assert!(matches!(rx.try_recv().unwrap().into_inner(), xmpp::XmppCommand::QueryArchive { .. }),
        "an already visible corrected row must not consume the scroll without requesting more history");
}

#[test]
fn history_cursor_keeps_inflight_rows_after_an_intermediate_local_page()
{
    use chrono::TimeZone;
    let base = chrono::Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.window_focused = true;
    let h = History::open_in_memory();
    h.insert("alice@example", "alice", "boundary", base + chrono::Duration::seconds(120), false, Some("boundary")).unwrap();
    h.insert("alice@example", "alice", "neighbour", base + chrono::Duration::seconds(180), false, Some("neighbour")).unwrap();
    s.chats[0].set_window(h.load_tail("alice@example", 10).unwrap(), false, true);
    s.history = Some(h);
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.fetch_archive_active();
    let query_id = match rx.try_recv().unwrap().into_inner() { xmpp::XmppCommand::QueryArchive { query_id, .. } => query_id, _ => unreachable!() };
    // A delayed copy can be delivered independently of the pending MAM page.
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::DirectMessage { from: "alice@example/phone".into(), body: "boundary".into(),
        timestamp: base + chrono::Duration::seconds(240), delayed: true, stanza_id: Some("boundary".into()), origin_id: None }));
    let archive = |body: &str, seconds: i64| Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage {
        query_id: query_id.clone(), to: None, from: "alice@example/phone".into(), body: body.into(),
        timestamp: base + chrono::Duration::seconds(seconds), stanza_id: Some(body.into()), origin_id: None,
    });
    let _ = s.update(archive("first gap", 150));
    assert!(s.page_older_active().unwrap());
    assert_eq!(s.chats[0].messages.len(), 3);
    let _ = s.update(archive("second gap", 160));
    let _ = s.update(archive("boundary", 240));
    let _ = s.update(archive("neighbour", 300));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id, complete: true, first: Some("first gap".into()), last: None }));
    let expected: Vec<_> = s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap().iter().map(|row| row.id).collect();
    let actual: Vec<_> = s.chats[0].messages.iter().filter_map(|message| match message { room::message::Message::Chat { id, .. } => Some(*id), _ => None }).collect();
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 4);
    assert!(!s.chats[0].mam_in_flight);
    assert!(!s.chats[0].anchored_bottom);
}

#[test]
fn archive_completion_keeps_a_window_that_filled_while_waiting_bounded()
{
    let mut s = completion_state(&[]);
    s.connected_jid = Some("me@example/device".into());
    let h = History::open_in_memory();
    let base = chrono::Utc::now();
    for i in 0..backlog::MAX_LOADED - 10 {
        h.insert("room@example", "alice", &format!("retained-{i}"), base + chrono::Duration::seconds(i as i64), false, Some(&format!("retained-{i}"))).unwrap();
    }
    s.rooms[0].set_window(h.load_tail("room@example", backlog::MAX_LOADED).unwrap(), false, true);
    s.history = Some(h);
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    s.xmpp_cmd_tx = Some(tx);
    s.fetch_archive_active();
    let query_id = match rx.try_recv().unwrap().into_inner() { xmpp::XmppCommand::QueryArchive { query_id, .. } => query_id, _ => unreachable!() };
    for i in 0..10 {
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::MemberJoined { room: "room@example".into(), member: ::xmpp::RoomMember {
            nick: format!("new-{i}"), jid: None, role: "participant".into(), affiliation: "member".into(), show: None, status: None,
        } }));
    }
    assert_eq!(s.rooms[0].messages.len(), backlog::MAX_LOADED);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage { query_id: query_id.clone(), to: None, from: "room@example/alice".into(),
        body: "older".into(), timestamp: base - chrono::Duration::seconds(1), stanza_id: Some("older".into()), origin_id: None }));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id, complete: true, first: Some("older".into()), last: None }));
    assert_eq!(s.rooms[0].messages.len(), backlog::MAX_LOADED);
    assert!(s.rooms[0].has_older, "the archive row remains available in local storage");
    assert!(!s.rooms[0].mam_in_flight);
    assert_eq!(s.history.as_ref().unwrap().load_tail("room@example", 10000).unwrap().len(), backlog::MAX_LOADED - 9);
}

// Regression: a room send reached the server but its stream-management ack
// was lost with the connection. The rejoin's history carried the delivered
// copy; replaying on self presence (before that history) sent it twice.
#[test]
fn rejoin_history_retires_an_unacknowledged_room_send_instead_of_resending_it() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.history = Some(History::open_in_memory());
    let (tx, mut rx) = tokio::sync::mpsc::channel(30);
    s.xmpp_cmd_tx = Some(tx);
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() });
    let _ = s.update(Message::SelectRoom("room@example".into()));
    s.message_input = text_editor::Content::with_text("only once");
    let _ = s.update(Message::SendMessage);
    let xmpp::XmppCommand::SendRoomMessage { id, origin_id: Some(origin), .. } = std::iter::from_fn(|| rx.try_recv().ok().map(xmpp::XmppCommand::into_inner))
        .find(|command| matches!(command, xmpp::XmppCommand::SendRoomMessage { .. })).unwrap() else { unreachable!() };
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id, ok: false }));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Reconnecting));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Reconnected { generation: 1 }));
    while rx.try_recv().is_ok() {}
    let sent_again = |rx: &mut tokio::sync::mpsc::Receiver<xmpp::XmppCommand>| std::iter::from_fn(|| rx.try_recv().ok().map(xmpp::XmppCommand::into_inner))
        .any(|command| matches!(command, xmpp::XmppCommand::SendRoomMessage { body, .. } if body == "only once"));
    s.room_connections.insert("room@example".into(), RoomConnection::Joining);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() }));
    assert!(!sent_again(&mut rx), "replay must wait for the join history");
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomMessage {
        room: "room@example".into(), nick: "me".into(), body: "only once".into(), timestamp: chrono::Utc::now(),
        delayed: true, stanza_id: Some("delivered".into()), origin_id: Some(origin),
    }));
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomSubject { room: "room@example".into(), subject: String::new() }));
    let _ = s.update(Message::RetryHistoryWrites);
    assert!(!sent_again(&mut rx));
    assert!(s.outbox_replay_pending.is_empty());
    let copies = s.history.as_ref().unwrap().load_tail("room@example", 10).unwrap();
    assert_eq!(copies.iter().filter(|message| message.body == "only once").count(), 1);
}

#[test]
fn rejoined_room_replays_queued_sends_after_the_subject_or_its_deadline() {
    for subject in [true, false] {
        let mut s = state();
        s.state = AppState::Connected;
        s.history = Some(History::open_in_memory());
        s.history.as_ref().unwrap().enqueue_outbox("room@example", true, "undelivered").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(30);
        s.xmpp_cmd_tx = Some(tx);
        s.room_connections.insert("room@example".into(), RoomConnection::Joining);
        let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() }));
        let replayed = |rx: &mut tokio::sync::mpsc::Receiver<xmpp::XmppCommand>| std::iter::from_fn(|| rx.try_recv().ok().map(xmpp::XmppCommand::into_inner))
            .any(|command| matches!(command, xmpp::XmppCommand::SendRoomMessage { body, .. } if body == "undelivered"));
        let _ = s.update(Message::RetryHistoryWrites);
        assert!(!replayed(&mut rx));
        if subject {
            let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomSubject { room: "room@example".into(), subject: "topic".into() }));
        } else {
            // A service that never sends a subject must not strand the queue.
            *s.room_history_pending.get_mut("room@example").unwrap() = std::time::Instant::now();
            let _ = s.update(Message::RetryHistoryWrites);
        }
        assert!(replayed(&mut rx));
        assert!(s.outbox_replay_pending.is_empty());
    }
}

fn drain_commands(rx: &mut tokio::sync::mpsc::Receiver<xmpp::XmppCommand>) -> Vec<xmpp::XmppCommand> {
    std::iter::from_fn(|| rx.try_recv().ok().map(xmpp::XmppCommand::into_inner)).collect()
}

// query ID, room (to), contact (with), RSM cursor
type DeliveryCheckQuery = (String, Option<String>, Option<String>, Option<String>);

fn delivery_check(commands: &[xmpp::XmppCommand]) -> Option<DeliveryCheckQuery> {
    commands.iter().find_map(|command| match command {
        xmpp::XmppCommand::QueryArchive { query_id, to, with, cursor, forward: true, start: Some(_), .. } if query_id.starts_with("mam-d-") =>
            Some((query_id.clone(), to.clone(), with.clone(), cursor.clone())),
        _ => None,
    })
}

// The DM variant of a lost acknowledgement: the server delivered and archived
// the message, so the reconnect must not send it again.
#[test]
fn reconnect_checks_the_archive_before_resending_an_unacknowledged_dm() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.history = Some(History::open_in_memory());
    let (tx, mut rx) = tokio::sync::mpsc::channel(30);
    s.xmpp_cmd_tx = Some(tx);
    s.message_input = text_editor::Content::with_text("dm only once");
    let _ = s.update(Message::SendMessage);
    let Some(xmpp::XmppCommand::SendDirectMessage { id, origin_id: Some(origin), .. }) = drain_commands(&mut rx).into_iter()
        .find(|command| matches!(command, xmpp::XmppCommand::SendDirectMessage { .. })) else { panic!("initial send"); };
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id, ok: false }));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Reconnecting));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Reconnected { generation: 1 }));
    let commands = drain_commands(&mut rx);
    assert!(!commands.iter().any(|command| matches!(command, xmpp::XmppCommand::SendDirectMessage { .. })), "resend must wait for the archive");
    let (query_id, to, with, cursor) = delivery_check(&commands).expect("delivery check");
    assert_eq!((to, with.as_deref(), cursor), (None, Some("alice@example"), None));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage {
        query_id: query_id.clone(), from: "me@example/device".into(), to: Some("alice@example".into()), body: "dm only once".into(),
        timestamp: chrono::Utc::now(), stanza_id: Some("archived".into()), origin_id: Some(origin),
    }));
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id, complete: true, first: None, last: None }));
    assert!(!drain_commands(&mut rx).iter().any(|command| matches!(command, xmpp::XmppCommand::SendDirectMessage { .. })));
    let copies = s.history.as_ref().unwrap().load_tail("alice@example", 10).unwrap();
    assert_eq!(copies.iter().filter(|message| message.body == "dm only once").count(), 1);
}

// Join history can be too short to include the delivered copy. The room's
// archive still proves delivery before anything is sent again.
#[test]
fn room_replay_checks_the_archive_when_join_history_lacks_the_delivered_copy() {
    let mut s = state();
    s.state = AppState::Connected;
    s.connected_jid = Some("me@example/device".into());
    s.history = Some(History::open_in_memory());
    let (tx, mut rx) = tokio::sync::mpsc::channel(30);
    s.xmpp_cmd_tx = Some(tx);
    let _ = confirm_room_entry(&mut s, xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() });
    let _ = s.update(Message::SelectRoom("room@example".into()));
    s.message_input = text_editor::Content::with_text("room only once");
    let _ = s.update(Message::SendMessage);
    let Some(xmpp::XmppCommand::SendRoomMessage { id, origin_id: Some(origin), .. }) = drain_commands(&mut rx).into_iter()
        .find(|command| matches!(command, xmpp::XmppCommand::SendRoomMessage { .. })) else { panic!("initial send"); };
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::SendResult { id, ok: false }));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Reconnecting));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::Reconnected { generation: 1 }));
    drain_commands(&mut rx);
    s.room_connections.insert("room@example".into(), RoomConnection::Joining);
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomJoined { room: "room@example".into(), own_nick: "me".into(), members: Vec::new() }));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::RoomSubject { room: "room@example".into(), subject: String::new() }));
    let commands = drain_commands(&mut rx);
    assert!(!commands.iter().any(|command| matches!(command, xmpp::XmppCommand::SendRoomMessage { .. })));
    let (query_id, to, with, _) = delivery_check(&commands).expect("delivery check");
    assert_eq!((to.as_deref(), with), (Some("room@example"), None));
    // A copy from another occupant with the same text proves nothing.
    let archived = |query_id: &str, nick: &str, origin: &str, sid: &str| Message::XmppEvent(xmpp::XmppEvent::ArchiveMessage {
        query_id: query_id.into(), from: format!("room@example/{nick}"), to: None, body: "room only once".into(),
        timestamp: chrono::Utc::now(), stanza_id: Some(sid.into()), origin_id: Some(origin.into()),
    });
    let _ = s.update(archived(&query_id, "alice", &uuid::Uuid::new_v4().to_string(), "someone-else"));
    assert_eq!(s.history.as_ref().unwrap().load_outbox().unwrap().len(), 1);
    let _ = s.update(archived(&query_id, "me", &origin, "delivered"));
    assert!(s.history.as_ref().unwrap().load_outbox().unwrap().is_empty());
    assert!(!s.rooms[0].messages.iter().any(|message| matches!(message, room::message::Message::Chat { id, .. } if *id < 0)));
    let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete { query_id, complete: true, first: None, last: None }));
    assert!(!drain_commands(&mut rx).iter().any(|command| matches!(command, xmpp::XmppCommand::SendRoomMessage { .. })));
}

#[test]
fn delivery_checks_page_forward_and_resend_after_failure_or_exhaustion() {
    for outcome in ["failed", "exhausted"] {
        let mut s = state();
        s.state = AppState::Connected;
        s.connected_jid = Some("me@example/device".into());
        s.history = Some(History::open_in_memory());
        let id = s.history.as_ref().unwrap().enqueue_outbox("alice@example", false, "maybe delivered").unwrap();
        s.history.as_ref().unwrap().mark_outbox_attempted(id).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(30);
        s.xmpp_cmd_tx = Some(tx);
        s.request_outbox_replay(None);
        let (mut query_id, _, _, _) = delivery_check(&drain_commands(&mut rx)).expect("delivery check");
        if outcome == "failed" {
            let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveFailed { query_id, reason: "feature-not-implemented".into() }));
        } else {
            for page in 1..DELIVERY_CHECK_PAGES {
                let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete {
                    query_id, complete: false, first: None, last: Some(format!("cursor-{page}")) }));
                let commands = drain_commands(&mut rx);
                assert!(!commands.iter().any(|command| matches!(command, xmpp::XmppCommand::SendDirectMessage { .. })));
                let (next, _, _, cursor) = delivery_check(&commands).expect("next page");
                assert_eq!(cursor, Some(format!("cursor-{page}")));
                query_id = next;
            }
            let _ = s.update(Message::XmppEvent(xmpp::XmppEvent::ArchiveComplete {
                query_id, complete: false, first: None, last: Some("beyond-the-limit".into()) }));
        }
        let commands = drain_commands(&mut rx);
        assert!(delivery_check(&commands).is_none());
        assert!(commands.iter().any(|command| matches!(command, xmpp::XmppCommand::SendDirectMessage { id: sent, .. } if *sent == id)), "{outcome}");
    }
}

#[test]
fn never_attempted_queued_sends_replay_without_an_archive_lookup() {
    let mut s = state();
    s.state = AppState::Connected;
    s.history = Some(History::open_in_memory());
    let id = s.history.as_ref().unwrap().enqueue_outbox("alice@example", false, "queued offline").unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(30);
    s.xmpp_cmd_tx = Some(tx);
    s.request_outbox_replay(None);
    let commands = drain_commands(&mut rx);
    assert!(delivery_check(&commands).is_none());
    assert!(commands.iter().any(|command| matches!(command, xmpp::XmppCommand::SendDirectMessage { id: sent, .. } if *sent == id)));
}
