pub mod style;
pub mod account;
pub mod sidebar;
pub mod chat;
pub(crate) mod message_layout;
pub(crate) mod identity;
pub(crate) mod bounded_editor;
pub(crate) mod composer_keys;
pub mod join;
pub mod members;

pub mod selection_area;
pub(crate) mod search_scroll;

// Keep compact forms centered while letting long errors and recovery controls
// remain reachable in constrained windows. Scrolling retains the full text.
pub(crate) fn centered_form<'a>(content: impl Into<iced::Element<'a, crate::Message>>, scroll_id: &'static str)
    -> iced::Element<'a, crate::Message>
{
    use iced::{Fill, Length};
    use iced::widget::{container, scrollable, Id};
    container(container(scrollable(content).id(Id::new(scroll_id)).height(Length::Shrink).width(Fill))
        .max_width(440.0).width(Fill))
        .center(Fill).into()
}

// Credential warnings survive conversation and connection state changes.
pub(crate) fn credential_notice(state: &crate::Snack) -> Option<iced::Element<'_, crate::Message>>
{
    use iced::widget::{button, column, text};
    let pending = !state.saved_config.pending_credential_removals.is_empty();
    if state.credential_error.is_none() && !pending && state.saved_login_error.is_none() { return None; }
    let mut notice = column![].spacing(4);
    if state.credential_error.is_some() || pending {
        let message = state.credential_error.as_deref().unwrap_or("Saved password cleanup is pending.");
        notice = notice.push(text(message).size(12));
    }
    if let Some(error) = &state.saved_login_error {
        notice = notice.push(text(error).size(12));
        if state.state == crate::AppState::Login && state.saved_config.jid.is_some()
            && !state.saved_config.jid.as_ref().is_some_and(|jid| state.saved_config.pending_credential_removals.contains(jid)) {
            notice = notice.push(button(text("Retry saved login").size(12)).on_press(crate::Message::RetrySavedLogin));
        }
    }
    if pending {
        notice = notice.push(button(text("Retry password cleanup").size(12))
            .on_press(crate::Message::RetryCredentialRemoval));
    }
    Some(notice.into())
}
