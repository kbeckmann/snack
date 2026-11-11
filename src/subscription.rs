use crate::app::{ AppState, Snack };
use crate::message::Message;
use crate::xmpp;

impl Snack
{
    pub(crate) fn subscription(&self) -> iced::Subscription<Message>
    {
        let keyboard = iced::keyboard::listen().with((self.shortcut_context(), self.navigation_context(), self.focus_context()))
            .map(|((view, session, focus), event)| session_keyboard_event(view, session, focus, event));

        // See captured events too: focused editors consume keyboard events.
        let window_focus = iced::event::listen_with(|event, _status, _id| global_event(event))
            .with(self.shortcut_context()).map(|(context, message)| scope_shortcut(context, message));

        // We disable exit-on-close so we can flush in-progress drafts to disk
        // before the window actually closes.
        let window_close = iced::window::close_requests().map(Message::WindowCloseRequested);
        let activation = iced::Subscription::run(crate::instance::activation_events);

        let command_retry = if self.xmpp_pending_commands.is_empty() {
            iced::Subscription::none()
        } else {
            iced::time::every(std::time::Duration::from_millis(25))
                .map(|_| Message::FlushXmppCommands)
        };

        let history_retry = if self.pending_history_writes.is_empty() && self.pending_outbox_deletes.is_empty() && self.outbox_replay_pending.is_empty()
            && !self.unqueued_dm_sends.iter().any(|send| send.dirty) && self.pending_message_errors.is_empty() && !self.rejected_sends.iter().any(|send| !send.saved) {
            iced::Subscription::none()
        } else {
            iced::time::every(std::time::Duration::from_secs(5))
                .map(|_| Message::RetryHistoryWrites)
        };

        match (&self.state, &self.xmpp_cmd_rx)
        {
            (AppState::Connecting | AppState::Connected, Some(cmd_rx)) =>
            {
                let xmpp_sub = iced::Subscription::run_with(
                    cmd_rx.clone(),
                    |cmd: &xmpp::CommandChannel|
                    {
                        let channel = cmd.clone();
                        iced::futures::StreamExt::map(xmpp::connect(cmd.clone()), move |event| {
                            Message::ScopedXmppEvent { channel: channel.clone(), event }
                        })
                    },
                );

                return iced::Subscription::batch([keyboard, window_focus, window_close, activation, command_retry, history_retry, xmpp_sub]);
            }
            _ => return iced::Subscription::batch([keyboard, window_focus, window_close, activation, command_retry, history_retry]),
        }
    }
}

pub(crate) fn session_keyboard_event(view: crate::message::ShortcutContext, session: crate::message::NavigationContext,
    focus: crate::message::FocusContext, event: iced::keyboard::Event) -> Message {
    let message = contextual_keyboard_event(view, event);
    let message = match message {
        Message::TraverseFocus { backward } => Message::ScopedFocus { context: focus, backward },
        other => other,
    };
    scope_navigation(session, message)
}

pub(crate) fn scope_navigation(context: crate::message::NavigationContext, message: Message) -> Message {
    let forward = match message {
        Message::NextSelection => true,
        Message::PrevSelection => false,
        other => return other,
    };
    Message::Navigate { context, forward }
}

pub(crate) fn contextual_keyboard_event(context: crate::message::ShortcutContext, event: iced::keyboard::Event) -> Message {
    scope_shortcut(context, keyboard_event(context.view, event))
}

pub(crate) fn scope_shortcut(context: crate::message::ShortcutContext, message: Message) -> Message {
    use crate::message::ShortcutAction;
    let action = match message {
        Message::CloseFind => ShortcutAction::Escape,
        Message::CopySelection => ShortcutAction::Copy,
        Message::ShowJoinPanel => ShortcutAction::Join,
        Message::ToggleFind => ShortcutAction::Find,
        other => return other,
    };
    Message::ScopedShortcut { context, action }
}

// Keyboard subscription identity follows the conversation view, so a queued
// close shortcut cannot be retargeted after a selection/account transition.
pub(crate) fn keyboard_event(context: uuid::Uuid, event: iced::keyboard::Event) -> Message {
    if let iced::keyboard::Event::KeyPressed { key, modifiers, .. } = event
    {
        if key == iced::keyboard::Key::Named(iced::keyboard::key::Named::Tab)
        {
            // Focused room composers consume nickname-completion Tab events.
            // Every unhandled Tab belongs to ordinary focus traversal.
            return Message::TraverseFocus { backward: modifiers.shift() };
        }

        // Escape closes the find bar (a no-op when it isn't open).
        if key == iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)
        {
            return Message::CloseFind;
        }

        if modifiers.alt() && !modifiers.shift() && !modifiers.control() && !modifiers.command()
        {
            if key == iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowUp)
            {
                return Message::PrevSelection;
            }
            if key == iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowDown)
            {
                return Message::NextSelection;
            }
        }

        // Cmd on macOS, Ctrl elsewhere. Control letters on macOS are editing
        // keys (Ctrl-N moves down, Ctrl-W is unbound), not app shortcuts.
        let primary = modifiers.command() && !modifiers.shift() && !modifiers.alt();
        if primary
        {
            if matches!(&key, iced::keyboard::Key::Character(k) if k.as_str().eq_ignore_ascii_case("n"))
            {
                return Message::ShowJoinPanel;
            }
            if matches!(&key, iced::keyboard::Key::Character(k) if k.as_str().eq_ignore_ascii_case("w"))
            {
                return Message::CloseConversation { context };
            }
            // Copy selected chat messages to the clipboard. The handler
            // no-ops when nothing is selected, so an ordinary Cmd/Ctrl+C
            // inside the message editor still copies the editor's text.
            if matches!(&key, iced::keyboard::Key::Character(k) if k.as_str().eq_ignore_ascii_case("c"))
            {
                return Message::CopySelection;
            }
        }
    }

    return Message::Ignore;
}

pub(crate) fn global_event(event: iced::Event) -> Option<Message> {
    match event {
        iced::Event::Keyboard(iced::keyboard::Event::ModifiersChanged(modifiers)) => {
            Some(Message::ModifiersUpdate {
                shift: modifiers.shift(),
                primary: modifiers.command(),
            })
        }
        iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. })
            if modifiers.command()
                && !modifiers.shift()
                && !modifiers.alt()
                && matches!(&key, iced::keyboard::Key::Character(k) if k.as_str().eq_ignore_ascii_case("f")) =>
        {
            Some(Message::ToggleFind)
        }
        iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)) => {
            Some(Message::EndBodyDrag)
        }
        iced::Event::Window(iced::window::Event::Focused) => Some(Message::WindowFocused),
        iced::Event::Window(iced::window::Event::Unfocused) => Some(Message::WindowUnfocused),
        _ => None,
    }
}
