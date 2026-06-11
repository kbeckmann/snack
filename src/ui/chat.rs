use std::collections::HashSet;

use iced::{ Color, Element, Fill, Length };
use iced::keyboard;
use iced::widget::{ button, checkbox, column, container, rich_text, row, scrollable, span, text, text_editor, text_input, Id };

use crate::app::FindState;
use crate::{ Message, Selection, Snack, FIND_INPUT_ID, MESSAGE_SCROLL_ID, MESSAGE_INPUT_ID };
use crate::room::backlog::Backlog;
use crate::room::message::{ ChatStatus, Message as RoomMessage, EventKind };
use crate::ui::{ join, style };

// Messages whose ids land in the active find result set are tinted; the focused
// hit is tinted more strongly.
struct FindHighlight
{
    match_ids: HashSet<i64>,
    current_id: Option<i64>,
}

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
    has_older: bool,
    find: Option<&FindHighlight>,
) -> Element<'a, Message>
{
    let today = chrono::Local::now().date_naive();

    // `msgs` is already the bounded in-memory window (see room::backlog), so we
    // turn the whole slice into widgets. The read marker indexes directly into it.
    let visible = msgs;
    let rel_marker = read_marker;

    // Estimate nick column width from the longest nick in rendered chat messages.
    let max_nick_len = visible.iter()
        .filter_map(|m| match m
        {
            RoomMessage::Chat { from, .. } => Some(from.len()),
            _ => None,
        })
        .max()
        .unwrap_or(4);
    // ~8px per character at size 14 + 2 chars for ": "
    let nick_width = ((max_nick_len + 2) as f32) * 8.0;

    let mut messages: Vec<Element<'a, Message>> = Vec::with_capacity(visible.len() + 2);

    // Hint that scrolling further up will page in older history.
    if has_older
    {
        let hint_color = Color::from_rgb(0.40, 0.44, 0.50);
        messages.push(
            container(text("↑ earlier messages").size(11).color(hint_color))
                .padding(4)
                .width(Fill)
                .center_x(Fill)
                .into(),
        );
    }

    for (i, m) in visible.iter().enumerate()
    {
        // Insert a "new messages" divider before the first new message.
        if rel_marker == Some(i)
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
            RoomMessage::Chat { id, from, body, received, status } =>
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

                let mut msg_row = row![time_label, nick_label, body_label]
                    .spacing(4).width(Fill);

                // Delivery badge for our own slow/failed sends, right-aligned.
                // Confirmed messages (and ones still within the grace period) get
                // none, so a normal send never flickers an indicator.
                match status
                {
                    ChatStatus::Pending => msg_row = msg_row.push(
                        text("sending…").size(11).color(Color::from_rgb(0.45, 0.48, 0.54)),
                    ),
                    ChatStatus::Failed => msg_row = msg_row.push(
                        text("failed").size(11).color(Color::from_rgb(0.85, 0.45, 0.45)),
                    ),
                    ChatStatus::Sending | ChatStatus::Confirmed => {}
                }

                let is_mention = my_nick
                    .is_some_and(|nick| crate::room::message::mentions(body, nick));

                let msg_container = container(msg_row).padding(4).width(Fill);

                let is_current_hit = find.is_some_and(|f| f.current_id == Some(*id));
                let is_match_hit = !is_current_hit && find.is_some_and(|f| f.match_ids.contains(id));

                let msg_element: Element<'a, Message> = if is_current_hit
                {
                    msg_container.style(style::find_current_highlight).into()
                }
                else if is_match_hit
                {
                    msg_container.style(style::find_match_highlight).into()
                }
                else if is_mention
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
    // Bottom-anchored: pins to the newest message, and — crucially — keeps the
    // viewport visually fixed when older history is prepended on scroll-up.
    .anchor_bottom()
    .on_scroll(Message::MessagesScrolled)
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
            // Alt+Up/Down navigates between selections. The editor would
            // otherwise capture the arrow keys to move the cursor between
            // lines, so the global keyboard subscription never sees them.
            if press.modifiers.alt() && !press.modifiers.shift()
                && !press.modifiers.control() && !press.modifiers.command()
            {
                if matches!(press.key, keyboard::Key::Named(keyboard::key::Named::ArrowUp))
                {
                    return Some(text_editor::Binding::Custom(Message::PrevSelection));
                }
                if matches!(press.key, keyboard::Key::Named(keyboard::key::Named::ArrowDown))
                {
                    return Some(text_editor::Binding::Custom(Message::NextSelection));
                }
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

// The Ctrl+F find bar: query box, hit counter, prev/next, scope toggle, close.
fn find_bar(find: &FindState) -> Element<'_, Message>
{
    let input = text_input("Search messages…", &find.query)
        .id(Id::new(FIND_INPUT_ID))
        .on_input(Message::FindInputChanged)
        .on_submit(Message::FindNext)
        .padding(6)
        .width(Fill);

    let counter = if find.query.trim().is_empty()
    {
        String::new()
    }
    else if find.matches.is_empty()
    {
        "no results".to_string()
    }
    else
    {
        format!("{}/{}", find.index + 1, find.matches.len())
    };

    let counter_color = Color::from_rgb(0.55, 0.59, 0.65);
    let prev = button(text("˄").size(16)).on_press(Message::FindPrev).padding(4).style(button::text);
    let next = button(text("˅").size(16)).on_press(Message::FindNext).padding(4).style(button::text);
    let close = button(text("✕").size(14)).on_press(Message::CloseFind).padding(4).style(button::text);
    let scope = checkbox(find.all_scope)
        .label("All rooms")
        .on_toggle(Message::FindScopeToggled)
        .size(14)
        .text_size(13);

    return container(
        row![
            input,
            text(counter).size(12).color(counter_color).width(Length::Fixed(70.0)),
            prev,
            next,
            scope,
            close,
        ]
        .align_y(iced::Alignment::Center)
        .spacing(8)
        .width(Fill)
    )
    .padding(6)
    .width(Fill)
    .style(container::bordered_box)
    .into();
}

// Find-result highlighting for the conversation currently being rendered.
fn find_highlight(state: &Snack, conv: &str) -> Option<FindHighlight>
{
    let find = state.find.as_ref()?;
    if find.matches.is_empty()
    {
        return None;
    }

    let match_ids: HashSet<i64> = find.matches.iter()
        .filter(|m| m.conversation == conv)
        .map(|m| m.id)
        .collect();
    let current_id = find.matches.get(find.index)
        .filter(|m| m.conversation == conv)
        .map(|m| m.id);

    return Some(FindHighlight { match_ids, current_id });
}

pub fn view(state: &Snack) -> Element<'_, Message>
{
    if state.show_join_panel || state.joining_room.is_some() || state.join_error.is_some()
    {
        return join::view(state);
    }

    let my_nick: Option<&str> = state.connected_jid
        .as_deref()
        .and_then(|j| j.split('@').next());

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

            let highlight = find_highlight(state, &room.jid);
            let messages = render_messages(
                &room.messages, room.read_marker, my_nick,
                room.more_history(), highlight.as_ref(),
            );

            let mut col = column![topic_label, messages].spacing(8);
            if let Some(find) = &state.find
            {
                col = col.push(find_bar(find));
            }
            col = col.push(input_row(state));

            return col
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

            let highlight = find_highlight(state, &chat.jid);
            let messages = render_messages(
                &chat.messages, chat.read_marker, my_nick,
                chat.more_history(), highlight.as_ref(),
            );

            let mut col = column![header, messages].spacing(8);
            if let Some(find) = &state.find
            {
                col = col.push(find_bar(find));
            }
            col = col.push(input_row(state));

            return col
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
