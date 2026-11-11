use iced::{ Element, Fill };
use iced::widget::row;

use crate::app::{ AppState, RoomConnection, Selection, Snack };
use crate::message::Message;
use crate::ui;

impl Snack
{
    pub(crate) fn view(&self) -> Element<'_, Message>
    {
        iced::widget::responsive(move |size| self.view_at_height(size.height)).into()
    }

    fn view_at_height(&self, available_height: f32) -> Element<'_, Message>
    {
        if self.session_is_draining() {
            return iced::widget::container(iced::widget::column![
                iced::widget::text(if self.pending_close.is_some_and(|close| close.draining) { "Closing…" } else { "Disconnecting…" }).size(20),
                iced::widget::text("Finishing your connection and saving messages.").size(14),
            ].spacing(8)).center(Fill).into();
        }
        let content = self.view_content();
        let account = self.connected_jid.as_deref().map(|jid| jid.split('/').next().unwrap_or(jid).to_string());
        let rejected = self.rejected_sends.iter().find(|send| send.account == account);
        let storage_pending = self.has_unsaved_message_updates() || !self.outbox_replay_pending.is_empty();
        let room_failures = self.state == AppState::Connected && self.room_connections.values().any(|state| matches!(state, RoomConnection::Failed(_)));
        let joining_active = match self.active {
            Some(Selection::Room(index)) if self.state == AppState::Connected && !self.reconnecting => self.rooms.get(index)
                .filter(|room| matches!(self.room_connections.get(&room.jid), Some(RoomConnection::Joining))),
            _ => None,
        };
        let mut stack = iced::widget::column![];
        if self.settings_save_error.is_some() || self.pending_close.is_some() {
            let message = self.settings_save_error.as_deref().unwrap_or("Closing is paused until pending changes are saved.");
            let mut controls = row![
                iced::widget::button(if self.pending_close.is_some() { "Retry save and close" } else { "Retry saving" })
                    .on_press(Message::RetrySettingsSave),
            ].spacing(8);
            if self.pending_close.is_some() {
                controls = controls.push(iced::widget::button("Keep open").on_press(Message::KeepWindowOpen))
                    .push(iced::widget::button("Close without saving").on_press(Message::CloseWithoutSaving));
            }
            let prompt = self.pending_close.map(|close| close.token);
            let notice: Element<'_, Message> = iced::widget::container(iced::widget::column![
                iced::widget::text(message).size(13).style(iced::widget::text::danger).width(Fill),
                controls.wrap(),
            ].spacing(8)).padding(8).width(Fill).into();
            let notice = notice.map(move |message| {
                use crate::message::SaveNoticeAction;
                let action = match message {
                    Message::RetrySettingsSave => SaveNoticeAction::Retry,
                    Message::KeepWindowOpen => SaveNoticeAction::KeepOpen,
                    Message::CloseWithoutSaving => SaveNoticeAction::Discard,
                    other => return other,
                };
                Message::SaveNotice { prompt, action }
            });
            stack = stack.push(ui::identity::instance(notice, ("save-notice", prompt)));
        }

        if let Some(room) = joining_active {
            stack = stack.push(iced::widget::container(iced::widget::text(format!("Joining {}… Messages will remain queued until entry is confirmed.", room.title)).size(13)).padding(8).width(Fill));
        }
        if room_failures {
            let active_failure = match self.active {
                Some(Selection::Room(index)) => self.rooms.get(index).and_then(|room| self.room_connections.get_key_value(&room.jid))
                    .filter(|(_, state)| matches!(state, RoomConnection::Failed(_))),
                _ => None,
            };
            let failure = active_failure.or_else(|| self.room_connections.iter().find(|(_, state)| matches!(state, RoomConnection::Failed(_))));
            if let Some((jid, RoomConnection::Failed(reason))) = failure {
                let context = self.room_retry_context(jid);
                let mut retry = iced::widget::button("Retry join");
                if !self.reconnecting && self.xmpp_cmd_tx.is_some() { retry = retry.on_press(Message::ScopedRoomRetry(context.clone())); }
                let notice = iced::widget::container(row![
                    iced::widget::text(format!("Could not join {jid}: {reason} Messages will remain queued.")).size(13).style(iced::widget::text::danger).width(Fill),
                    retry,
                ].spacing(8)).padding(8).width(Fill).into();
                stack = stack.push(ui::identity::instance(notice, ("room-retry", context)));
                let others = self.room_connections.values().filter(|state| matches!(state, RoomConnection::Failed(_))).count() - 1;
                if others > 0 {
                    stack = stack.push(iced::widget::container(iced::widget::text(format!("{others} other room joins also failed.")).size(13)).padding(8).width(Fill));
                }
            }
        }
        if let Some(error) = &self.send_error {
            let original: std::sync::Arc<str> = self.message_input.text().into();
            let mut controls = row![iced::widget::text(error).size(13).style(iced::widget::text::danger).width(Fill)];
            if ::xmpp::validate_message_text(&original).is_err() {
                controls = controls.push(iced::widget::button("Remove control characters").on_press(Message::ScopedComposer {
                    context: self.composer_id, action: crate::message::ComposerAction::Clean(original.clone()),
                }));
            }
            let notice = iced::widget::container(controls.spacing(8)).padding(8).width(Fill).into();
            stack = stack.push(ui::identity::instance(notice, ("draft-error", self.composer_id, original)));
        }
        if let Some(send) = rejected {
            let notice = iced::widget::container(row![
                iced::widget::text(format!("Could not send to {}: {}", send.conversation, send.reason)).size(13).style(iced::widget::text::danger).width(Fill),
                iced::widget::button("Copy message").on_press(Message::CopyRejectedSend(send.token)),
                iced::widget::button("Discard").on_press(Message::DiscardRejectedSend(send.token)),
            ].spacing(8)).padding(8).width(Fill).into();
            stack = stack.push(ui::identity::instance(notice, ("rejected-send", send.token)));
        }
        let warning = if !self.has_unsaved_message_updates() {
            "Queued messages cannot be loaded yet. Snack will retry automatically."
        } else { "Some message updates have not been saved. Keep Snack open while it retries." };
        if storage_pending {
            stack = stack.push(iced::widget::container(iced::widget::text(warning).size(13).style(iced::widget::text::danger)).padding(8).width(Fill));
        }
        // Bound the aggregate, since several independent notices can coexist.
        // Keep this slot even when empty so showing a notice does not replace
        // the conversation's widget tree and discard composer focus/selection.
        let notices = iced::widget::container(iced::widget::scrollable(stack.width(Fill))
            .id(iced::widget::Id::new("global-notices")).height(iced::Length::Shrink).width(Fill))
            .max_height((available_height * 0.4).min(200.0)).width(Fill);
        return iced::widget::column![notices, content].height(Fill).width(Fill).into();
    }

    fn view_content(&self) -> Element<'_, Message>
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
}
