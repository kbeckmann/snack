use iced::{ Element, Fill, Length };
use iced::widget::{ button, column, container, row, scrollable, text };

use crate::{ Message, Selection, Snack };
use crate::ui::style;

pub fn view(state: &Snack) -> Element<'_, Message>
{
    let account_label = if let Some(ref jid) = state.connected_jid
    {
        text(jid.clone()).size(12)
    }
    else
    {
        text("Not connected").size(12)
    };

    let disconnect_btn = button(text("Disconnect").size(11))
        .on_press(Message::Disconnect)
        .padding(4)
        .style(button::text);

    let mut action_row = row![disconnect_btn, text("").width(Fill)];

    if state.saved_config.jid.is_some()
    {
        let forget_btn = button(text("Remove auto-login").size(11))
            .on_press(Message::ForgetAutoLogin)
            .padding(4)
            .style(button::text);
        action_row = action_row.push(forget_btn);
    }

    let mut account_col = column![account_label].spacing(4).width(Fill);

    if state.reconnecting
    {
        let retry_btn = button(text("Reconnect now").size(11))
            .on_press(Message::ForceReconnect)
            .padding(4)
            .style(button::text);

        account_col = account_col.push(
            row![
                text("Reconnecting…").size(11),
                text("").width(Fill),
                retry_btn,
            ].align_y(iced::Alignment::Center).width(Fill)
        );
    }

    let account_row = account_col.push(action_row);

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
                .on_press(Message::SelectRoom(i))
                .width(Fill)
                .padding(6)
                .style(btn_style);

            items.push(item.into());
        }
    }

    let list = scrollable(
        column(items).spacing(2).width(Fill)
    );

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
            .on_press(Message::SelectChat(i))
            .width(Fill)
            .padding(6)
            .style(btn_style);

        chat_items.push(item.into());
    }

    let chats_list = scrollable(
        column(chat_items).spacing(2).width(Fill)
    );

    return container(
        column![account_row, sidebar_header, list, chats_header, chats_list].spacing(8).width(Fill)
    )
    .width(Length::Fixed(200.0))
    .height(Fill)
    .padding(8)
    .style(container::bordered_box)
    .into();
}
