use iced::{ Color, Element, Fill, Length };
use iced::keyboard;
use iced::widget::{ button, column, container, rich_text, row, scrollable, span, text, text_editor, Id };

use crate::{ Message, Selection, Snack, MESSAGE_SCROLL_ID, MESSAGE_INPUT_ID };
use crate::room::message::{ Message as RoomMessage, EventKind };
use crate::ui::{ join, style };

// Split text into alternating (plain, url) fragments.
fn parse_urls(body: &str) -> Vec<(&str, bool)>
{
    let mut parts = Vec::new();
    let mut remaining = body;

    while let Some(start) = remaining.find("https://").or_else(|| remaining.find("http://"))
    {
        if start > 0
        {
            parts.push((&remaining[..start], false));
        }

        let url_text = &remaining[start..];
        let end = url_text.find(|c: char| c.is_whitespace()).unwrap_or(url_text.len());

        parts.push((&remaining[start..start + end], true));
        remaining = &remaining[start + end..];
    }

    if !remaining.is_empty()
    {
        parts.push((remaining, false));
    }

    return parts;
}

fn render_messages<'a>(
    msgs: &'a [RoomMessage],
    read_marker: Option<usize>,
    my_nick: Option<&str>,
) -> Element<'a, Message>
{
    let today = chrono::Local::now().date_naive();

    // Estimate nick column width from the longest nick in chat messages.
    let max_nick_len = msgs.iter()
        .filter_map(|m| match m
        {
            RoomMessage::Chat { from, .. } => Some(from.len()),
            _ => None,
        })
        .max()
        .unwrap_or(4);
    // ~8px per character at size 14 + 2 chars for ": "
    let nick_width = ((max_nick_len + 2) as f32) * 8.0;

    let mut messages: Vec<Element<'a, Message>> = Vec::with_capacity(msgs.len() + 1);

    for (i, m) in msgs.iter().enumerate()
    {
        // Insert a "new messages" divider before the first new message.
        if read_marker == Some(i) && i < msgs.len()
        {
            let accent = Color::from_rgb(0.60, 0.40, 0.40);
            let new_label = text("  new messages  ").size(11).color(accent);
            let line = || container(text(""))
                .height(1)
                .width(Fill)
                .style(|_: &_| container::Style
                {
                    background: Some(iced::Background::Color(Color::from_rgb(0.60, 0.40, 0.40))),
                    ..Default::default()
                });
            let divider: Element<'a, Message> = row![line(), new_label, line()]
                .align_y(iced::Alignment::Center)
                .width(Fill)
                .into();
            messages.push(divider);
        }

        match m
        {
            RoomMessage::Chat { from, body, received } =>
            {
                let local_time = received.with_timezone(&chrono::Local);
                let timestamp = if local_time.date_naive() == today
                {
                    local_time.format("%H:%M:%S").to_string()
                }
                else
                {
                    local_time.format("%Y-%m-%d %H:%M:%S").to_string()
                };

                let time_color = Color::from_rgb(0.40, 0.44, 0.50);
                let nick_color = Color::from_rgb(0.60, 0.64, 0.70);

                let time_width = if local_time.date_naive() == today { 65.0 } else { 145.0 };
                let time_label = text(timestamp).size(14).color(time_color)
                    .width(Length::Fixed(time_width));
                let nick_label = text(format!("{}: ", from)).size(14).color(nick_color)
                    .width(Length::Fixed(nick_width));

                let link_color = Color::from_rgb(0.53, 0.75, 0.82);
                let body_spans: Vec<_> = parse_urls(body).into_iter().map(|(s, is_url)|
                {
                    if is_url
                    {
                        span(s.to_string()).color(link_color).underline(true).link(s.to_string())
                    }
                    else
                    {
                        span(s.to_string())
                    }
                }).collect();

                let body_label = rich_text(body_spans)
                    .on_link_click(Message::OpenUrl)
                    .size(14)
                    .width(Fill);

                let msg_row = row![time_label, nick_label, body_label]
                    .spacing(4).width(Fill);

                let is_mention = my_nick
                    .is_some_and(|nick|
                    {
                        let body_lower = body.to_lowercase();
                        body_lower.match_indices(nick).any(|(start, matched)|
                        {
                            let end = start + matched.len();
                            let before_ok = start == 0 || !body_lower.as_bytes()[start - 1].is_ascii_alphanumeric();
                            let after_ok = end == body_lower.len() || !body_lower.as_bytes()[end].is_ascii_alphanumeric();
                            before_ok && after_ok
                        })
                    });

                let msg_container = container(msg_row).padding(4).width(Fill);

                let msg_element: Element<'a, Message> = if is_mention
                {
                    msg_container.style(style::mention_highlight).into()
                }
                else
                {
                    msg_container.into()
                };

                messages.push(msg_element);
            }
            RoomMessage::Event { kind, nick, received } =>
            {
                let local_time = received.with_timezone(&chrono::Local);
                let timestamp = if local_time.date_naive() == today
                {
                    local_time.format("%H:%M:%S").to_string()
                }
                else
                {
                    local_time.format("%Y-%m-%d %H:%M:%S").to_string()
                };

                let time_color = Color::from_rgb(0.40, 0.44, 0.50);
                let event_color = Color::from_rgb(0.50, 0.54, 0.60);

                let time_width = if local_time.date_naive() == today { 65.0 } else { 145.0 };
                let time_label = text(timestamp).size(14).color(time_color)
                    .width(Length::Fixed(time_width));

                let event_text = match kind
                {
                    EventKind::Joined => format!("* {} has joined the room", nick),
                    EventKind::Left => format!("* {} has left the room", nick),
                    EventKind::StatusChanged(show) => match show.as_deref()
                    {
                        None => format!("* {} is now online", nick),
                        Some("away") => format!("* {} is now away", nick),
                        Some("xa") => format!("* {} is now extended away", nick),
                        Some("dnd") => format!("* {} is do not disturb", nick),
                        Some("chat") => format!("* {} is free for chat", nick),
                        Some(other) => format!("* {} status: {}", nick, other),
                    },
                };

                let event_label = text(event_text).size(14).color(event_color).width(Fill);

                let event_row = row![time_label, event_label].spacing(4).width(Fill);
                let event_element: Element<'a, Message> = container(event_row)
                    .padding(4)
                    .width(Fill)
                    .into();
                messages.push(event_element);
            }
        }
    }

    return scrollable(
        column(messages).spacing(2).width(Fill)
    )
    .id(Id::new(MESSAGE_SCROLL_ID))
    .height(Fill)
    .width(Fill)
    .into();
}

fn input_row(state: &Snack) -> Element<'_, Message>
{
    let input = text_editor(&state.message_input)
        .id(Id::new(MESSAGE_INPUT_ID))
        .placeholder("Type a message...")
        .on_action(Message::InputAction)
        .key_binding(|press|
        {
            // Plain Enter sends the message. Alt+Enter (and Shift+Enter as a
            // common alternative) inserts a newline instead.
            if matches!(press.key, keyboard::Key::Named(keyboard::key::Named::Enter))
            {
                if press.modifiers.alt() || press.modifiers.shift()
                {
                    return Some(text_editor::Binding::Enter);
                }
                return Some(text_editor::Binding::Custom(Message::SendMessage));
            }
            return text_editor::Binding::from_key_press(press);
        })
        .padding(10)
        .height(Length::Shrink)
        .max_height(160.0);

    let send_btn = button(text("Send").size(14))
        .on_press(Message::SendMessage)
        .padding(10);

    return row![input, send_btn]
        .align_y(iced::Alignment::End)
        .spacing(8)
        .width(Fill)
        .into();
}

pub fn view(state: &Snack) -> Element<'_, Message>
{
    if state.show_join_panel || state.joining_room.is_some() || state.join_error.is_some()
    {
        return join::view(state);
    }

    let my_nick: Option<String> = state.connected_jid
        .as_deref()
        .and_then(|j| j.split('@').next())
        .map(|n| n.to_lowercase());

    match state.active
    {
        Some(Selection::Room(index)) =>
        {
            let room = &state.rooms[index];

            let leave_btn = button(text("Leave").size(12))
                .on_press(Message::LeaveRoom)
                .padding(4)
                .style(button::text);

            let topic_label = container(
                row![
                    text(&room.topic).size(14),
                    text("").width(Fill),
                    leave_btn,
                ].align_y(iced::Alignment::Center).width(Fill)
            )
                .padding(8)
                .width(Fill)
                .style(container::bordered_box);

            let messages = render_messages(&room.messages, room.read_marker, my_nick.as_deref());

            return column![topic_label, messages, input_row(state)]
                .spacing(8)
                .width(Fill)
                .height(Fill)
                .padding(8)
                .into();
        }
        Some(Selection::Chat(index)) =>
        {
            let chat = &state.chats[index];

            let close_btn = button(text("Close").size(12))
                .on_press(Message::CloseChat)
                .padding(4)
                .style(button::text);

            let header = container(
                row![
                    text(&chat.jid).size(14),
                    text("").width(Fill),
                    close_btn,
                ].align_y(iced::Alignment::Center).width(Fill)
            )
                .padding(8)
                .width(Fill)
                .style(container::bordered_box);

            let messages = render_messages(&chat.messages, chat.read_marker, my_nick.as_deref());

            return column![header, messages, input_row(state)]
                .spacing(8)
                .width(Fill)
                .height(Fill)
                .padding(8)
                .into();
        }
        None =>
        {
            return join::view(state);
        }
    }
}
