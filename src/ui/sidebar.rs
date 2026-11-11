use iced::{ Element, Fill, Length };
use iced::widget::{ button, column, container, row, scrollable, text };

use crate::{ Message, Selection, Snack };
use crate::ui::style;

pub fn view(state: &Snack) -> Element<'_, Message>
{
    let account_label = if let Some(jid) = state.own_jid()
    {
        text(jid).size(12)
    }
    else
    {
        text("Not connected").size(12)
    };

    let account_label = container(scrollable(account_label.wrapping(iced::widget::text::Wrapping::Glyph))
        .height(Length::Shrink)).max_height(48.0).width(Fill);

    let disconnect_btn = button(text("Disconnect").size(11))
        .on_press(Message::Disconnect)
        .padding(4)
        .style(button::text);

    let disconnect_btn = super::identity::instance(disconnect_btn.into(), "account-disconnect");
    let mut action_row = row![disconnect_btn, text("").width(Fill)];

    if state.saved_config.jid.is_some()
    {
        let forget_btn = button(text("Remove auto-login").size(11))
            .on_press(Message::ForgetAutoLogin)
            .padding(4)
            .style(button::text);
        let forget_btn = super::identity::instance(forget_btn.into(), "account-forget");
        action_row = action_row.push(forget_btn);
    }

    let mut account_row = column![
        account_label,
    ].spacing(4).width(Fill);

    if state.reconnecting
    {
        let retry_btn = button(text("Reconnect now").size(11))
            .on_press(Message::ForceReconnect)
            .padding(4)
            .style(button::text);

        let retry_btn = super::identity::instance(retry_btn.into(), "account-reconnect");
        account_row = account_row.push(
            row![
                text("Reconnecting…").size(11),
                text("").width(Fill),
                retry_btn,
            ].align_y(iced::Alignment::Center).width(Fill)
        );
    }

    account_row = account_row.push(action_row);
    if let Some(notice) = super::credential_notice(state) {
        account_row = account_row.push(container(scrollable(notice)
            .id("sidebar-credential-notice").height(Length::Shrink)).max_height(96.0).width(Fill));
    }

    let sidebar_header = row![
        text("Rooms").size(14),
        text("").width(Fill),
        button(text("+").size(14))
            .on_press(Message::ShowJoinPanel)
            .padding(4)
            .style(button::text),
    ].align_y(iced::Alignment::Center).width(Fill);

    // Group rooms by server.
    let mut servers: Vec<String> = Vec::new();
    let mut grouped: Vec<(String, Vec<usize>)> = Vec::new();

    for (i, r) in state.rooms.iter().enumerate()
    {
        let server = r.jid.split('@').nth(1).unwrap_or(&r.jid).to_string();

        if let Some(pos) = servers.iter().position(|s| *s == server)
        {
            grouped[pos].1.push(i);
        }
        else
        {
            servers.push(server.clone());
            grouped.push((server, vec![i]));
        }
    }

    let mut items: Vec<Element<'_, Message>> = Vec::new();

    for (server, indices) in &grouped
    {
        // Server header.
        items.push(
            text(server.clone()).size(12).into()
        );

        // Room entries with unread dot or empty space, aligned with server names.
        for &i in indices
        {
            let r = &state.rooms[i];
            let is_active = state.active == Some(Selection::Room(i));
            let icon = if r.unread { "\u{2022}" } else { " " };
            let title_text = if r.unread
            {
                text(&r.title).size(14).font(iced::Font { weight: iced::font::Weight::Bold, ..Default::default() })
            }
            else
            {
                text(&r.title).size(14)
            };

            let label = row![
                text(icon).size(14),
                title_text,
            ].spacing(6).align_y(iced::Alignment::Center);

            let btn_style = if is_active { style::room_button_active } else { button::text };

            let item = button(label)
                .on_press(Message::SelectRoom(r.jid.clone()))
                .width(Fill)
                .padding(6)
                .style(btn_style);

            items.push(super::identity::instance(item.into(), ("room-entry", r.jid.clone())));
        }
    }

    let unopened = state.unopened_room_jids();
    for jid in &unopened {
        let status = match state.room_connections.get(*jid) {
            Some(crate::app::RoomConnection::Failed(_)) => "Could not join",
            _ if state.reconnecting => "Reconnecting…",
            Some(crate::app::RoomConnection::Joining) => "Joining…",
            _ => "Waiting for connection…",
        };
        let mut entry = column![text(*jid).size(13).wrapping(iced::widget::text::Wrapping::Glyph), text(status).size(11)].spacing(4).width(Fill);
        if matches!(state.room_connections.get(*jid), Some(crate::app::RoomConnection::Failed(_))) {
            let mut retry = button(text("Retry join").size(11)).padding(4);
            if !state.reconnecting && state.xmpp_cmd_tx.is_some() {
                retry = retry.on_press(Message::ScopedRoomRetry(state.room_retry_context(jid)));
            }
            entry = entry.push(retry);
        }
        items.push(super::identity::instance(entry.into(), ("pending-room-entry", state.room_retry_context(jid))));
    }

    let list = scrollable(
        column(items).spacing(2).width(Fill)
    ).id("sidebar-rooms")
        .height(if state.rooms.is_empty() && unopened.is_empty() { Length::Shrink } else { Fill });

    // Chats section.
    let chats_header = text("Chats").size(14);
    let mut chat_items: Vec<Element<'_, Message>> = Vec::new();

    for (i, c) in state.chats.iter().enumerate()
    {
        let is_active = state.active == Some(Selection::Chat(i));
        let icon = if c.unread { "\u{2022}" } else { " " };
        let title_text = if c.unread
        {
            text(&c.title).size(14).font(iced::Font { weight: iced::font::Weight::Bold, ..Default::default() })
        }
        else
        {
            text(&c.title).size(14)
        };

        let label = row![
            text(icon).size(14),
            title_text,
        ].spacing(6).align_y(iced::Alignment::Center);

        let btn_style = if is_active { style::room_button_active } else { button::text };

        let item = button(label)
            .on_press(Message::SelectChat(c.jid.clone()))
            .width(Fill)
            .padding(6)
            .style(btn_style);

        chat_items.push(super::identity::instance(item.into(), ("chat-entry", c.jid.clone())));
    }

    let chats_list = scrollable(
        column(chat_items).spacing(2).width(Fill)
    ).id("sidebar-chats")
        .height(if state.chats.is_empty() { Length::Shrink } else { Fill });

    let content: Element<'_, Message> = container(
        column![account_row, sidebar_header, list, chats_header, chats_list].spacing(8).width(Fill)
    )
    .width(Length::Fixed(200.0))
    .height(Fill)
    .padding(8)
    .style(container::bordered_box)
    .into();
    let channel = state.xmpp_cmd_rx.clone();
    let profile = state.connected_jid.clone();
    let identity = ("sidebar", channel.clone(), profile.clone());
    let content = content.map(move |message| {
        use crate::message::SidebarAction;
        let action = match message {
            Message::SelectRoom(jid) => SidebarAction::SelectRoom(jid),
            Message::SelectChat(jid) => SidebarAction::SelectChat(jid),
            Message::Disconnect => SidebarAction::Disconnect,
            Message::ForgetAutoLogin => SidebarAction::ForgetAutoLogin,
            Message::ForceReconnect => SidebarAction::ForceReconnect,
            Message::ShowJoinPanel => SidebarAction::ShowJoinPanel,
            Message::RetryCredentialRemoval => SidebarAction::RetryCredentialRemoval,
            other => return other,
        };
        Message::ScopedSidebar { channel: channel.clone(), profile: profile.clone(), action }
    });
    super::identity::instance(content, identity)
}
