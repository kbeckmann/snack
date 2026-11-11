use std::collections::{ HashMap, HashSet };

use iced::{ Color, Element, Fill, Length };
use iced::keyboard;
use iced::widget::{ button, checkbox, column, container, row, scrollable, text, text_editor, text_input, Id };

use crate::app::FindState;
use crate::{ Message, Selection, Snack, FIND_INPUT_ID, MESSAGE_SCROLL_ID, MESSAGE_INPUT_ID };
use crate::room::backlog::Backlog;
use crate::room::message::{ ChatStatus, Message as RoomMessage, EventKind };
use crate::ui::{ join, style };

fn privacy_notices(privacy: ::xmpp::RoomPrivacy) -> Vec<&'static str>
{
    let mut notices = Vec::new();
    if privacy.publicly_logged == Some(true) { notices.push("This room is publicly logged. Messages may be published outside this chat."); }
    if privacy.non_anonymous == Some(true) { notices.push("This room exposes your real XMPP address to all participants."); }
    notices
}

// Messages whose ids land in the active find result set are tinted; the focused
// hit is tinted more strongly.
#[derive(Clone)]
struct FindHighlight
{
    match_ids: HashSet<i64>,
    current_id: Option<i64>,
    pending_scroll: Option<uuid::Uuid>,
    scroll_id: uuid::Uuid,
}

// Split text into alternating (plain, url) fragments.
pub(crate) fn parse_urls(body: &str) -> Vec<(&str, bool)>
{
    let mut parts = Vec::new();
    let mut remaining = body;

    // Check both schemes in one forward scan. Searching the entire remainder
    // separately for an absent scheme makes dense link messages quadratic.
    while let Some(start) = remaining.as_bytes().iter().enumerate().find_map(|(index, byte)| {
        if !matches!(byte, b'h' | b'H') { return None; }
        let tail = &remaining.as_bytes()[index..];
        (tail.get(..7).is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"http://"))
            || tail.get(..8).is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"https://"))).then_some(index)
    })
    {
        if start > 0
        {
            parts.push((&remaining[..start], false));
        }

        let url_text = &remaining[start..];
        // Angle brackets and double quotes delimit URLs in prose; they are
        // not part of a raw URI. Keep valid URL punctuation and percent escapes.
        let end = url_text.find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"')).unwrap_or(url_text.len());

        parts.push((&remaining[start..start + end], true));
        remaining = &remaining[start + end..];
    }

    if !remaining.is_empty()
    {
        parts.push((remaining, false));
    }

    return parts;
}

// A full-width horizontal rule with a centered caption, used to separate
// sections of the message list (new messages, replayed reconnect history).
fn divider<'a>(label: &str, color: Color) -> Element<'a, Message>
{
    let caption = text(format!("  {}  ", label)).size(11).color(color);
    let line = move || container(text(""))
        .height(1)
        .width(Fill)
        .style(move |_: &_| container::Style
        {
            background: Some(iced::Background::Color(color)),
            ..Default::default()
        });
    return row![line(), caption, line()]
        .align_y(iced::Alignment::Center)
        .width(Fill)
        .into();
}

fn render_messages<'a>(
    msgs: &'a [RoomMessage],
    read_marker: Option<usize>,
    history_markers: &[usize],
    my_nick: Option<&str>,
    has_older: bool,
    find: Option<&FindHighlight>,
    selected: &HashSet<i64>,
    body_editors: &'a HashMap<i64, text_editor::Content>,
    dragging: bool,
    primary_down: bool,
    context: uuid::Uuid,
    selection: uuid::Uuid,
) -> Element<'a, Message>
{
    let history_markers = history_markers.to_vec();
    let my_nick = my_nick.map(str::to_owned);
    let find = find.cloned();
    let selected = selected.clone();
    iced::widget::responsive(move |size| render_messages_at_width(
        msgs, read_marker, &history_markers, my_nick.as_deref(), has_older,
        find.as_ref(), &selected, body_editors, dragging, primary_down,
        context, selection, size.width,
    )).into()
}

fn render_messages_at_width<'a>(
    msgs: &'a [RoomMessage],
    read_marker: Option<usize>,
    history_markers: &[usize],
    my_nick: Option<&str>,
    has_older: bool,
    find: Option<&FindHighlight>,
    selected: &HashSet<i64>,
    body_editors: &'a HashMap<i64, text_editor::Content>,
    dragging: bool,
    primary_down: bool,
    context: uuid::Uuid,
    selection: uuid::Uuid,
    width: f32,
) -> Element<'a, Message>
{
    let today = chrono::Local::now().date_naive();

    // `msgs` is already the bounded in-memory window (see room::backlog), so we
    // turn the whole slice into widgets. The markers index directly into it.
    let visible = msgs;

    // Estimate nick column width from the longest nick in rendered chat messages.
    let max_nick_len = visible.iter()
        .filter_map(|m| match m
        {
            RoomMessage::Chat { from, .. } => Some(crate::room::message::sender_label(from).chars().take(16).count()),
            _ => None,
        })
        .max()
        .unwrap_or(4);
    // Keep names aligned without letting one long resource consume the entire
    // transcript. Full names wrap by glyph inside this bounded column.
    let nick_width = (((max_nick_len + 2) as f32) * 8.0).min(width * 0.25);

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
        // Insert a "reconnected — history below" divider before the first message
        // of the backlog the server replayed after a reconnect, so it's clear
        // where the already-seen chat ends and the (possibly overlapping) new
        // history begins.
        if history_markers.contains(&i) && i < msgs.len()
        {
            messages.push(divider("reconnected — history below", Color::from_rgb(0.42, 0.52, 0.62)));
        }

        // Insert a "new messages" divider before the first new message — unless it
        // would land at the same spot as the reconnected-history divider, which
        // already marks where the unseen messages begin. Stacking both is noise.
        if read_marker == Some(i) && !history_markers.contains(&i) && i < msgs.len()
        {
            messages.push(divider("new messages", Color::from_rgb(0.60, 0.40, 0.40)));
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
                let nick_label = text(format!("{}: ", crate::room::message::sender_label(from))).size(14)
                    .color(if from.is_empty() { Color::from_rgb(0.85, 0.56, 0.24) } else { nick_color })
                    .wrapping(iced::widget::text::Wrapping::Glyph)
                    .width(Length::Fixed(nick_width));

                // Read-only editors share a transcript-wide drag selection.
                let body_label: Element<'a, Message> = match body_editors.get(id)
                {
                    Some(content) =>
                    {
                        let id = *id;
                        super::selection_area::selection_area(text_editor(content)
                            .on_action(move |action| Message::BodyEditorAction { id, action })
                            .key_binding(move |press|
                            {
                                if !matches!(press.status, text_editor::Status::Focused { .. }) { return None; }
                                // Preserve Alt+Up/Down room navigation while a body
                                // editor is focused; the editor would otherwise
                                // capture the arrow keys to move the cursor.
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
                                if press.modifiers.command() && matches!(&press.key, keyboard::Key::Character(k) if k.as_str().eq_ignore_ascii_case("c")) {
                                    return Some(text_editor::Binding::Custom(Message::CopySelection));
                                }
                                // Tab goes back to typing; the body is read-only.
                                if matches!(press.key, keyboard::Key::Named(keyboard::key::Named::Tab))
                                    && !press.modifiers.command() && !press.modifiers.control() && !press.modifiers.alt() {
                                    return Some(text_editor::Binding::Custom(Message::FocusComposer { context }));
                                }
                                if is_close_shortcut(&press) {
                                    return Some(text_editor::Binding::Custom(Message::CloseShortcut { context }));
                                }
                                return text_editor::Binding::from_key_press(press);
                            })
                            .style(style::body_editor)
                            .size(14)
                            // Break long URLs and hashes instead of overflowing.
                            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                            .padding(0)
                            .into(), id, dragging, content.selection().is_some())
                    }
                    None => text(body).size(14).wrapping(iced::widget::text::Wrapping::WordOrGlyph).width(Fill).into(),
                };

                let message_id = *id;
                let mut msg_row = row![].spacing(4).width(Fill);
                if !selected.is_empty()
                {
                    let select = checkbox(selected.contains(id))
                        .on_toggle(move |_| Message::SelectMessage { id: message_id })
                        .size(14);
                    msg_row = msg_row.push(iced::widget::tooltip(select,
                        text(if selected.contains(id) { "Deselect message" } else { "Select message" }),
                        iced::widget::tooltip::Position::Top));
                }
                msg_row = msg_row.push(time_label).push(nick_label);

                // Keep badges out of the body when metadata leaves too little
                // reading space. A narrow header can itself wrap without hiding
                // timestamps, names, selection controls or delivery state.
                let badge = match status {
                    ChatStatus::Pending(_) | ChatStatus::Queued(_) => Some(
                        text("sending…").size(11).color(Color::from_rgb(0.45, 0.48, 0.54)),
                    ),
                    ChatStatus::Unconfirmed => Some(
                        text("not confirmed").size(11).color(Color::from_rgb(0.45, 0.48, 0.54)),
                    ),
                    ChatStatus::Failed(_) => Some(
                        text("not sent").size(11).color(Color::from_rgb(0.75, 0.35, 0.35)),
                    ),
                    ChatStatus::Sending(_) | ChatStatus::Confirmed | ChatStatus::Submitted(_) => None,
                };
                let metadata_width = time_width + nick_width + 16.0
                    + if selected.is_empty() { 0.0 } else { 18.0 }
                    + if badge.is_some() { 104.0 } else { 0.0 };
                let header_width = time_width + nick_width + 4.0
                    + if selected.is_empty() { 0.0 } else { 18.0 };
                let message_content = super::message_layout::message_layout(
                    msg_row.wrap().into(), body_label, badge.map(Into::into),
                    width - metadata_width >= width * 0.55, header_width,
                );

                let is_mention = my_nick
                    .is_some_and(|nick| crate::room::message::mentions(body, nick));

                let msg_container = container(message_content).padding(4).width(Fill);
                let msg_container = if let Some(token) = find.filter(|find| find.current_id == Some(*id)).and_then(|find| find.pending_scroll) {
                    msg_container.id(super::search_scroll::marker_id(token))
                } else { msg_container };

                let is_selected = selected.contains(id);
                let is_current_hit = find.is_some_and(|f| f.current_id == Some(*id));
                let is_match_hit = !is_current_hit && find.is_some_and(|f| f.match_ids.contains(id));

                // Selection is an explicit user gesture, so its highlight takes
                // precedence over the find and mention tints.
                let styled: Element<'a, Message> = if is_selected
                {
                    msg_container.style(style::selected_message).into()
                }
                else if is_current_hit
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

                // Editors handle body clicks; expose the same gestures on the
                // timestamp and nickname without intercepting text drags.
                let actions = iced::widget::mouse_area(styled);
                let actions = if primary_down {
                    actions.on_press(Message::SelectMessage { id: message_id })
                } else {
                    actions.on_double_click(Message::QuoteMessage { id: message_id })
                };
                messages.push(super::identity::instance(actions.into(), message_id));
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

                let event_label = text(event_text).size(14).color(event_color).wrapping(iced::widget::text::Wrapping::WordOrGlyph).width(Fill);

                let event_row = row![time_label, event_label].spacing(4).width(Fill);
                let event_element: Element<'a, Message> = container(event_row)
                    .padding(4)
                    .width(Fill)
                    .into();
                messages.push(event_element);
            }
        }
    }

    let content = container(column(messages).spacing(2).width(Fill));
    let scroll = scrollable(content)
    .id(Id::new(MESSAGE_SCROLL_ID))
    // Bottom-anchored: pins to the newest message, and — crucially — keeps the
    // viewport visually fixed when older history is prepended on scroll-up.
    .anchor_bottom()
    .on_scroll(Message::MessagesScrolled)
    .height(Fill)
    .width(Fill)
    .into();
    let scroll = super::search_scroll::guard_layout_scroll(scroll, find.is_some_and(|find| find.pending_scroll.is_some()));
    let search = find.map(|find| find.scroll_id);
    let scroll = scroll.map(move |message| transcript_event(context, selection, search, message));
    super::identity::instance(scroll, ("transcript", context))
}

fn transcript_event(context: uuid::Uuid, selection: uuid::Uuid, search: Option<uuid::Uuid>, message: Message) -> Message {
    use crate::message::TranscriptAction;
    let action = match message {
        Message::SelectMessage { id } => TranscriptAction::Select(id),
        Message::QuoteMessage { id } => TranscriptAction::Quote(id),
        Message::BodyEditorAction { id, action } => TranscriptAction::Editor { id, action },
        Message::BodyDrag { id, point } => TranscriptAction::Drag { id, point },
        Message::CopySelection => TranscriptAction::Copy(selection),
        Message::ClearSelection => TranscriptAction::Clear(selection),
        Message::MessagesScrolled(viewport) => TranscriptAction::Scrolled { search, viewport },
        other => return other,
    };
    Message::ScopedTranscript { context, action }
}

// Cmd-W on macOS, Ctrl-W elsewhere.
fn is_close_shortcut(press: &text_editor::KeyPress) -> bool
{
    press.modifiers.command() && !press.modifiers.shift() && !press.modifiers.alt()
        && press.key.to_latin(press.physical_key) == Some('w')
}

fn input_row(state: &Snack) -> Element<'_, Message>
{
    let composer = state.composer_id;
    let complete_nicks = state.state == crate::app::AppState::Connected
        && matches!(state.active, Some(Selection::Room(_))) && !state.join_panel_visible();
    let input = text_editor(&state.message_input)
        .id(Id::new(MESSAGE_INPUT_ID))
        .placeholder("Type a message...")
        .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
        .on_action(Message::InputAction)
        .key_binding(move |press|
        {
            // Iced calls custom bindings for unfocused editors too. Only the
            // focused composer may send or change its draft via shortcuts.
            if !matches!(press.status, text_editor::Status::Focused { .. }) { return None; }
            if complete_nicks && matches!(press.key, keyboard::Key::Named(keyboard::key::Named::Tab))
                && !press.modifiers.command() && !press.modifiers.control() && !press.modifiers.alt() {
                return Some(text_editor::Binding::Custom(if press.modifiers.shift() {
                    Message::ShiftTabPressed
                } else { Message::TabPressed }));
            }
            if press.modifiers.command() && !press.modifiers.alt()
                && matches!(&press.key, keyboard::Key::Character(k) if k.as_str().eq_ignore_ascii_case("z"))
            {
                return Some(text_editor::Binding::Custom(if press.modifiers.shift() {
                    Message::RedoInput
                } else { Message::UndoInput }));
            }
            // Plain Enter sends the message. Alt+Enter (and Shift+Enter as a
            // common alternative) inserts a newline instead, as does
            // Ctrl+Enter on macOS (Cocoa's insertLineBreak).
            if matches!(press.key, keyboard::Key::Named(keyboard::key::Named::Enter))
            {
                let mac_line_break = super::composer_keys::CURRENT == super::composer_keys::Platform::MacOs
                    && press.modifiers.control();
                if press.modifiers.alt() || press.modifiers.shift() || mac_line_break
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
            // The focused composer captures keys, so handle Cmd/Ctrl-W here too.
            if is_close_shortcut(&press) {
                return Some(text_editor::Binding::Custom(Message::CloseShortcut { context: composer }));
            }
            if let Some(binding) = super::composer_keys::binding(super::composer_keys::CURRENT, &press.key, press.modifiers) {
                return Some(binding);
            }
            return text_editor::Binding::from_key_press(press);
        })
        .padding(10)
        .height(Length::Shrink)
        .max_height(160.0);
    let input = super::bounded_editor::bounded_editor(input.into(), 160.0);

    let send_btn = button(text("Send").size(14))
        .on_press(Message::SendMessage)
        .padding(10);

    let content: Element<'_, Message> = row![input, send_btn]
        .align_y(iced::Alignment::End)
        .spacing(8)
        .width(Fill)
        .into();
    let context = state.composer_id;
    let content = content.map(move |message| {
        use crate::message::ComposerAction;
        let action = match message {
            Message::InputAction(action) => ComposerAction::Input(action),
            Message::ComposerEdit(command) => ComposerAction::Edit(command),
            Message::UndoInput => ComposerAction::Undo,
            Message::RedoInput => ComposerAction::Redo,
            Message::SendMessage => ComposerAction::Send,
            Message::TabPressed => ComposerAction::Complete(false),
            Message::ShiftTabPressed => ComposerAction::Complete(true),
            other => return other,
        };
        Message::ScopedComposer { context, action }
    });
    // A pressed button/IME interaction also belongs to this editor instance,
    // even if the gesture finishes after another conversation is loaded.
    super::identity::instance(content, ("composer", context))
}

// A small bar shown while one or more messages are selected: a count plus Copy
// and Clear affordances, so the feature is usable without the keyboard shortcut.
fn selection_bar(count: usize, context: uuid::Uuid, selection: uuid::Uuid) -> Element<'static, Message>
{
    let label_color = Color::from_rgb(0.55, 0.59, 0.65);
    let label = if count == 1
    {
        "1 message selected".to_string()
    }
    else
    {
        format!("{} messages selected", count)
    };

    let copy = button(text("Copy").size(13)).on_press(Message::CopySelection).padding(4);
    let clear = button(text("Clear").size(13)).on_press(Message::ClearSelection).padding(4).style(button::text);

    let content: Element<'static, Message> = container(
        row![
            text(label).size(13).color(label_color),
            text("").width(Fill),
            copy,
            clear,
        ]
        .align_y(iced::Alignment::Center)
        .spacing(8)
        .width(Fill)
    )
    .padding(6)
    .width(Fill)
    .style(container::bordered_box)
    .into();
    let content = content.map(move |message| transcript_event(context, selection, None, message));
    super::identity::instance(content, ("selection", context, selection))
}

// The Ctrl+F find bar: query box, hit counter, prev/next, scope toggle, close.
fn find_bar<'a>(find: &'a FindState) -> Element<'a, Message>
{
    use crate::message::FindAction;
    let context = find.id;
    let event = move |action| Message::ScopedFind { context, action };
    let owned = |control: Element<'a, Message>, role: &'static str| super::identity::instance(control, (role, context));
    let input = text_input("Search messages…", &find.query)
        .id(Id::new(FIND_INPUT_ID))
        .on_input(move |value| event(FindAction::Input(value)))
        .on_submit(event(FindAction::Submit))
        .padding(6)
        .width(Fill);

    let counter = if find.query.trim().is_empty() || find.error.is_some()
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
    let prev = owned(button(text("˄").size(16)).on_press(event(FindAction::Prev)).padding(4).style(button::text).into(), "find-prev");
    let next = owned(button(text("˅").size(16)).on_press(event(FindAction::Next)).padding(4).style(button::text).into(), "find-next");
    let close = owned(button(text("✕").size(14)).on_press(event(FindAction::Close)).padding(4).style(button::text).into(), "find-close");
    let scope = owned(checkbox(find.all_scope)
        .label("All rooms")
        .on_toggle(move |value| event(FindAction::Scope(value)))
        .size(14)
        .text_size(13).into(), "find-scope");

    // Keep a useful query width even in a narrow conversation pane. The
    // optional retry control must not squeeze the input out of its row.
    let query = row![input, close].spacing(8).align_y(iced::Alignment::Center).width(Fill);
    let mut controls = row![text(counter).size(12).color(counter_color).width(Fill), prev, next, scope];
    if find.error.is_some() {
        controls = controls.push(owned(button(text("Retry").size(12))
            .on_press(event(FindAction::Retry)).padding(4).style(button::text).into(), "find-retry"));
    }
    let mut content = column![query, controls.align_y(iced::Alignment::Center).spacing(8).width(Fill)].spacing(6);
    if let Some(error) = &find.error {
        content = content.push(container(scrollable(
            text(error).size(12).color(counter_color).wrapping(iced::widget::text::Wrapping::Glyph)
        ).height(Length::Shrink)).max_height(48.0).width(Fill));
    }
    let bar = container(content)
    .padding(6)
    .width(Fill)
    .style(container::bordered_box)
    .into();
    owned(bar, "find-bar")
}

// Find-result highlighting for the conversation currently being rendered.
fn find_highlight(state: &Snack, conv: &str) -> Option<FindHighlight>
{
    let find = state.find.as_ref()?;

    let match_ids: HashSet<i64> = find.matches.iter()
        .filter(|m| m.conversation == conv)
        .map(|m| m.id)
        .collect();
    let current_id = find.matches.get(find.index)
        .filter(|m| m.conversation == conv)
        .map(|m| m.id);

    return Some(FindHighlight { match_ids, current_id, pending_scroll: find.pending_scroll, scroll_id: find.scroll_id });
}

// Keep optional controls in fixed slots: removing the selection bar must not
// replace the search input's widget state or steal its keyboard/IME focus.
fn bottom_controls(state: &Snack) -> Element<'_, Message>
{
    let selected = !state.selected_msgs.is_empty();
    let selection = container(column(selected.then(|| selection_bar(state.selected_msgs.len(), state.composer_id, state.selection_id))))
        .padding(iced::Padding { bottom: if selected { 8.0 } else { 0.0 }, ..Default::default() });
    let find = container(column(state.find.as_ref().map(find_bar)))
        .padding(iced::Padding { bottom: if state.find.is_some() { 8.0 } else { 0.0 }, ..Default::default() });
    column![selection, find, input_row(state)].into()
}

pub fn view(state: &Snack) -> Element<'_, Message>
{
    let context = state.navigation_context();
    view_content(state).map(move |message| crate::subscription::scope_navigation(context.clone(), message))
}

fn view_content(state: &Snack) -> Element<'_, Message>
{
    if state.join_panel_visible()
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
                .on_press(Message::CloseConversation { context: state.composer_id })
                .padding(4)
                .style(button::text);

            // A server-controlled topic must not consume the composer or
            // hide privacy notices. Keep its complete text in its own viewport.
            let topic = container(scrollable(
                text(&room.topic).size(14).wrapping(iced::widget::text::Wrapping::Glyph)
            ).height(Length::Shrink))
                .max_height(64.0)
                .width(Fill);
            let mut header = column![row![topic, leave_btn]
                .spacing(8)
                .align_y(iced::Alignment::Center).width(Fill)].spacing(6);
            for notice in privacy_notices(room.privacy) {
                header = header.push(text(notice).size(13).color(Color::from_rgb(0.85, 0.56, 0.24)));
            }
            let topic_label = container(header)
                .padding(8)
                .width(Fill)
                .style(container::bordered_box);

            let topic_label = super::identity::instance(topic_label.into(), ("conversation-header", state.composer_id));

            let highlight = find_highlight(state, &room.jid);
            let messages = render_messages(
                &room.messages, room.read_marker, &room.history_markers, Some(&room.own_nick),
                room.more_history(), highlight.as_ref(), &state.selected_msgs,
                &state.body_editors, state.body_drag.is_some(), state.cmd_down, state.composer_id, state.selection_id,
            );

            let col = column![topic_label, messages, bottom_controls(state)].spacing(8);

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
                .on_press(Message::CloseConversation { context: state.composer_id })
                .padding(4)
                .style(button::text);

            let header = container(
                row![
                    container(scrollable(
                        text(&chat.jid).size(14).wrapping(iced::widget::text::Wrapping::Glyph)
                    ).height(Length::Shrink)).max_height(64.0).width(Fill),
                    close_btn,
                ].spacing(8).align_y(iced::Alignment::Center).width(Fill)
            )
                .padding(8)
                .width(Fill)
                .style(container::bordered_box);

            let header = super::identity::instance(header.into(), ("conversation-header", state.composer_id));

            // Direct-message chats get no reconnect-history replay, so no divider.
            let highlight = find_highlight(state, &chat.jid);
            let messages = render_messages(
                &chat.messages, chat.read_marker, &[], my_nick,
                chat.more_history(), highlight.as_ref(), &state.selected_msgs,
                &state.body_editors, state.body_drag.is_some(), state.cmd_down, state.composer_id, state.selection_id,
            );

            let col = column![header, messages, bottom_controls(state)].spacing(8);

            return col
                .width(Fill)
                .height(Fill)
                .padding(8)
                .into();
        }
        None =>
        {
            if !state.unopened_room_jids().is_empty() {
                let joining = state.room_connections.values().any(|connection| matches!(connection, crate::app::RoomConnection::Joining));
                let heading = if state.reconnecting { "Reconnecting to your rooms…" }
                    else if joining { "Joining your rooms…" }
                    else { "Your rooms are in the sidebar" };
                let detail = if joining || state.reconnecting { "Your rooms will open when entry is confirmed." }
                    else if state.room_connections.values().any(|connection| matches!(connection, crate::app::RoomConnection::Failed(_))) { "Use Retry join to reconnect to a room." }
                    else { "Your rooms will open when the connection is ready." };
                return super::centered_form(column![text(heading).size(20), text(detail).size(14)].spacing(12), "room-restoration-scroll");
            }
            return join::view(state);
        }
    }
}

#[cfg(test)]
mod tests
{
    use super::*;
    use iced::advanced::{widget::Tree, layout, Layout, Shell};

    #[test]
    fn url_detection_preserves_case_and_excludes_angle_and_quote_wrappers() {
        let body = "🙂 see <HTTPS://example.org/A?x=1&y=2> and \"hTtP://example.org/O'Reilly\" then https://example.org/%3E";
        let parts = parse_urls(body);
        let urls: Vec<_> = parts.iter().filter_map(|(text, url)| url.then_some(*text)).collect();
        assert_eq!(urls, ["HTTPS://example.org/A?x=1&y=2", "hTtP://example.org/O'Reilly", "https://example.org/%3E"]);
        assert_eq!(parts.iter().map(|(text, _)| *text).collect::<String>(), body);
    }

    #[test]
    fn url_detection_keeps_scheme_order_and_valid_url_punctuation() {
        let body = "http://first.invalid https://second.invalid/wiki/Name_(topic) https://[::1]/path?q=a,b!#fragment";
        let urls: Vec<_> = parse_urls(body).into_iter().filter_map(|(text, url)| url.then_some(text)).collect();
        assert_eq!(urls, ["http://first.invalid", "https://second.invalid/wiki/Name_(topic)", "https://[::1]/path?q=a,b!#fragment"]);
    }

    #[test]
    fn dense_link_messages_keep_all_links_and_plain_text() {
        let body = "é <https://example.invalid/x>\n".repeat(2048);
        let parts = parse_urls(&body);
        assert_eq!(parts.iter().filter(|(_, url)| *url).count(), 2048);
        assert!(parts.iter().filter(|(_, url)| *url).all(|(url, _)| *url == "https://example.invalid/x"));
        assert_eq!(parts.iter().map(|(text, _)| *text).collect::<String>(), body);
    }

    #[test]
    fn submitting_find_emits_a_scoped_submit()
    {
        for failed in [false, true] {
            let find = FindState { query: "needle".into(), error: failed.then(|| "unavailable".into()), ..Default::default() };
            let mut widget = find_bar(&find);
            let mut tree = Tree::new(&widget);
            let renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(iced::Font::DEFAULT, iced::Pixels(14.0)));
            let bounds = iced::Rectangle::new(iced::Point::ORIGIN, iced::Size::new(600.0, 70.0));
            let node = widget.as_widget_mut().layout(&mut tree, &renderer, &layout::Limits::new(iced::Size::ZERO, bounds.size()));
            let cursor = iced::mouse::Cursor::Available(iced::Point::new(15.0, 15.0));
            let mut messages = Vec::new();
            for event in [
                iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Enter),
                    modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                    physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                    location: keyboard::Location::Standard, modifiers: keyboard::Modifiers::NONE, text: None, repeat: false,
                }),
            ] {
                widget.as_widget_mut().update(&mut tree, &event, Layout::new(&node), cursor,
                    &renderer, &mut iced::advanced::clipboard::Null, &mut Shell::new(&mut messages), &bounds);
            }
            assert!(messages.iter().any(|message| matches!(message,
                Message::ScopedFind { context, action: crate::message::FindAction::Submit } if *context == find.id)));
        }
    }
    #[test]
    fn send_button_press_cannot_carry_over_to_another_composer()
    {
        use iced::advanced::widget::Operation;
        struct ButtonBounds(Option<iced::Rectangle>);
        impl Operation for ButtonBounds {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
            fn text(&mut self, _id: Option<&Id>, bounds: iced::Rectangle, text: &str) {
                if text == "Send" { self.0 = Some(bounds); }
            }
        }
        for changed_context in [false, true] {
            let mut state = Snack::test_state();
            let renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(iced::Font::DEFAULT, iced::Pixels(14.0)));
            let bounds = iced::Rectangle::new(iced::Point::ORIGIN, iced::Size::new(600.0, 180.0));
            let mut tree;
            let target;
            {
                let mut widget = input_row(&state);
                tree = Tree::new(&widget);
                let node = widget.as_widget_mut().layout(&mut tree, &renderer, &layout::Limits::new(iced::Size::ZERO, bounds.size()));
                let mut button = ButtonBounds(None);
                widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &renderer, &mut button);
                target = button.0.unwrap().center();
                let mut messages = Vec::new();
                widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                    Layout::new(&node), iced::mouse::Cursor::Available(target), &renderer,
                    &mut iced::advanced::clipboard::Null, &mut Shell::new(&mut messages), &bounds);
                assert!(!messages.iter().any(|message| matches!(message, Message::ScopedComposer { action: crate::message::ComposerAction::Send, .. })));
            }
            // Loading another conversation gives its composer a new identity.
            if changed_context { state.composer_id = uuid::Uuid::new_v4(); }
            state.message_input = text_editor::Content::with_text("new private draft");
            let mut widget = input_row(&state);
            tree.diff(widget.as_widget());
            let node = widget.as_widget_mut().layout(&mut tree, &renderer, &layout::Limits::new(iced::Size::ZERO, bounds.size()));
            let mut messages = Vec::new();
            widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)),
                Layout::new(&node), iced::mouse::Cursor::Available(target), &renderer,
                &mut iced::advanced::clipboard::Null, &mut Shell::new(&mut messages), &bounds);
            assert_eq!(messages.iter().any(|message| matches!(message, Message::ScopedComposer { action: crate::message::ComposerAction::Send, .. })), !changed_context,
                "only a press from the same composer may finish sending");
        }
    }

    #[test]
    fn composer_captures_native_undo_and_redo_shortcuts()
    {
        let state = Snack::test_state();
        let mut widget = input_row(&state);
        let mut tree = Tree::new(&widget);
        let renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(iced::Font::DEFAULT, iced::Pixels(14.0)));
        let bounds = iced::Rectangle::new(iced::Point::ORIGIN, iced::Size::new(600.0, 180.0));
        let node = widget.as_widget_mut().layout(&mut tree, &renderer, &layout::Limits::new(iced::Size::ZERO, bounds.size()));
        let cursor = iced::mouse::Cursor::Available(iced::Point::new(15.0, 15.0));
        let mut messages = Vec::new();
        widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)), Layout::new(&node), cursor,
            &renderer, &mut iced::advanced::clipboard::Null, &mut Shell::new(&mut messages), &bounds);
        for redo in [false, true]
        {
            messages.clear();
            let event = iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character("z".into()), modified_key: keyboard::Key::Character("z".into()),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyZ),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::COMMAND | if redo { keyboard::Modifiers::SHIFT } else { keyboard::Modifiers::NONE },
                text: None, repeat: false,
            });
            widget.as_widget_mut().update(&mut tree, &event, Layout::new(&node), cursor,
                &renderer, &mut iced::advanced::clipboard::Null, &mut Shell::new(&mut messages), &bounds);
            assert!(messages.iter().any(|m| if redo { matches!(m, Message::ScopedComposer { context, action: crate::message::ComposerAction::Redo } if *context == state.composer_id) } else { matches!(m, Message::ScopedComposer { context, action: crate::message::ComposerAction::Undo } if *context == state.composer_id) }));
        }
    }
    #[test]
    fn search_shortcut_is_global_even_with_a_focused_composer()
    {
        let state = Snack::test_state();
        let mut widget = input_row(&state);
        let mut tree = Tree::new(&widget);
        let renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(iced::Font::DEFAULT, iced::Pixels(14.0)));
        let bounds = iced::Rectangle::new(iced::Point::ORIGIN, iced::Size::new(600.0, 180.0));
        let node = widget.as_widget_mut().layout(&mut tree, &renderer, &layout::Limits::new(iced::Size::ZERO, bounds.size()));
        let cursor = iced::mouse::Cursor::Available(iced::Point::new(15.0, 15.0));
        let mut messages = Vec::new();
        widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)), Layout::new(&node), cursor,
            &renderer, &mut iced::advanced::clipboard::Null, &mut Shell::new(&mut messages), &bounds);
        let event = iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Character("f".into()), modified_key: keyboard::Key::Character("f".into()),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyF),
            location: keyboard::Location::Standard, modifiers: keyboard::Modifiers::COMMAND,
            text: None, repeat: false,
        });
        messages.clear();
        widget.as_widget_mut().update(&mut tree, &event, Layout::new(&node), cursor,
            &renderer, &mut iced::advanced::clipboard::Null, &mut Shell::new(&mut messages), &bounds);
        assert!(!messages.iter().any(|m| matches!(m, Message::ToggleFind)));
        assert!(matches!(crate::subscription::global_event(event), Some(Message::ToggleFind)));
        assert!(matches!(crate::subscription::global_event(iced::Event::Keyboard(keyboard::Event::ModifiersChanged(
            keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT))), Some(Message::ModifiersUpdate { shift: true, primary: true })));
    }

    #[test]
    fn resizing_metadata_layout_preserves_body_editor_focus() {
        use iced::advanced::widget::operation::{Operation, Focusable};
        struct Focus { set: bool, found: bool }
        impl Operation for Focus {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
            fn focusable(&mut self, _id: Option<&Id>, _bounds: iced::Rectangle, state: &mut dyn Focusable) {
                if self.set { state.focus(); }
                self.found |= state.is_focused();
            }
        }
        let renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(iced::Font::DEFAULT, iced::Pixels(14.0)));
        let messages = vec![RoomMessage::Chat { id: 11, from: "a".repeat(128), body: "selected message".into(), received: chrono::Utc::now(), status: ChatStatus::Confirmed }];
        let editors = HashMap::from([(11, text_editor::Content::with_text("selected message"))]);
        let selected = HashSet::new();
        let mut widget = render_messages(&messages, None, &[], None, false, None, &selected, &editors, false, false, uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let mut tree = Tree::new(&widget);
        let node = widget.as_widget_mut().layout(&mut tree, &renderer, &layout::Limits::new(iced::Size::ZERO, iced::Size::new(1000.0, 300.0)));
        let mut focus = Focus { set: true, found: false };
        widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &renderer, &mut focus);
        assert!(focus.found);
        for width in [280.0, 1000.0] {
            let node = widget.as_widget_mut().layout(&mut tree, &renderer, &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 300.0)));
            let mut focus = Focus { set: false, found: false };
            widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &renderer, &mut focus);
            assert!(focus.found, "resizing to {width} lost body focus");
        }
    }

    #[test]
    fn long_nicknames_and_metadata_cannot_crowd_out_message_bodies() {
        use iced::advanced::widget::operation::{Operation, Focusable};
        #[derive(Default)]
        struct EditorBounds { editors: Vec<iced::Rectangle>, names: Vec<(String, iced::Rectangle)> }
        impl Operation for EditorBounds {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
            fn focusable(&mut self, _id: Option<&Id>, bounds: iced::Rectangle, _state: &mut dyn Focusable) { self.editors.push(bounds); }
            fn text(&mut self, _id: Option<&Id>, bounds: iced::Rectangle, text: &str) {
                if text.ends_with(": ") { self.names.push((text.into(), bounds)); }
            }
        }
        let renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(iced::Font::DEFAULT, iced::Pixels(14.0)));
        for width in [600.0, 280.0, 360.0, 1000.0] {
            for nick in ["a".repeat(128), "😀".repeat(128), "alice".to_string()] {
                for old in [false, true] {
                    for status in [ChatStatus::Confirmed, ChatStatus::Unconfirmed, ChatStatus::Queued(1), ChatStatus::Failed(1)] {
                        let received = chrono::Utc::now() - chrono::Duration::days(if old { 2 } else { 0 });
                        let messages = vec![
                            RoomMessage::Chat { id: 11, from: nick.clone(), body: "first complete message".into(), received, status },
                            RoomMessage::Chat { id: 12, from: "bob".into(), body: "second complete message".into(), received, status: ChatStatus::Confirmed },
                        ];
                        let editors = HashMap::from([(11, text_editor::Content::with_text("first complete message")), (12, text_editor::Content::with_text("second complete message"))]);
                        let selected = HashSet::from([11]);
                        let mut widget = render_messages(&messages, None, &[], None, false, None, &selected, &editors, false, false, uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
                        let mut tree = Tree::new(&widget);
                        let size = iced::Size::new(width, 300.0);
                        let node = widget.as_widget_mut().layout(&mut tree, &renderer, &layout::Limits::new(iced::Size::ZERO, size));
                        let mut bounds = EditorBounds::default();
                        widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &renderer, &mut bounds);
                        assert_eq!(bounds.editors.len(), 2);
                        for editor in bounds.editors {
                            assert!(editor.width >= width / 2.0, "nick_bytes={} width={width} old={old} status={status:?}: body width {}", nick.len(), editor.width);
                            assert!(editor.x >= 0.0 && editor.x + editor.width <= width);
                        }
                        assert_eq!(bounds.names.len(), 2);
                        assert_eq!(bounds.names[0].0, format!("{nick}: "));
                        assert_eq!(bounds.names[1].0, "bob: ");
                        for (_, name) in bounds.names { assert!(name.width > 0.0 && name.x >= 0.0 && name.x + name.width <= width); }
                        assert_eq!(editors[&11].text(), "first complete message");
                        assert_eq!(editors[&12].text(), "second complete message");
                    }
                }
            }
        }
    }

    #[test]
    fn a_double_click_cannot_span_different_message_rows()
    {
        use iced::advanced::widget::Operation;
        struct NickBounds(Option<iced::Rectangle>);
        impl Operation for NickBounds {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) { operate(self); }
            fn text(&mut self, _id: Option<&Id>, bounds: iced::Rectangle, text: &str) {
                if text == "alice: " { self.0 = Some(bounds); }
            }
        }
        let renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(iced::Font::DEFAULT, iced::Pixels(14.0)));
        for replace in [false, true] {
            let context = uuid::Uuid::new_v4();
            let selected = HashSet::new();
            let editors = HashMap::from([(11, text_editor::Content::with_text("first")), (12, text_editor::Content::with_text("other"))]);
            let received = chrono::Utc::now();
            let old = vec![RoomMessage::Chat { id: 11, from: "alice".into(), body: "first".into(), received, status: ChatStatus::Confirmed }];
            let new_id = if replace { 12 } else { 11 };
            let new = vec![RoomMessage::Chat { id: new_id, from: "alice".into(), body: if replace { "other" } else { "first" }.into(), received, status: ChatStatus::Confirmed }];
            let bounds = iced::Rectangle::new(iced::Point::ORIGIN, iced::Size::new(600.0, 80.0));
            let mut tree;
            let point;
            {
                let mut widget = render_messages(&old, None, &[], None, false, None, &selected, &editors, false, false, context, uuid::Uuid::new_v4());
                tree = Tree::new(&widget);
                let node = widget.as_widget_mut().layout(&mut tree, &renderer, &layout::Limits::new(iced::Size::ZERO, bounds.size()));
                let mut target = NickBounds(None);
                widget.as_widget_mut().operate(&mut tree, Layout::new(&node), &renderer, &mut target);
                point = target.0.unwrap().center();
                let mut output = Vec::new();
                for event in [iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left), iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)] {
                    widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(event), Layout::new(&node), iced::mouse::Cursor::Available(point), &renderer,
                        &mut iced::advanced::clipboard::Null, &mut Shell::new(&mut output), &bounds);
                }
                assert!(!output.iter().any(|message| matches!(message, Message::ScopedTranscript { action: crate::message::TranscriptAction::Quote(_), .. })));
            }
            let mut widget = render_messages(&new, None, &[], None, false, None, &selected, &editors, false, false, context, uuid::Uuid::new_v4());
            tree.diff(widget.as_widget());
            let node = widget.as_widget_mut().layout(&mut tree, &renderer, &layout::Limits::new(iced::Size::ZERO, bounds.size()));
            let mut output = Vec::new();
            for event in [iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left), iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)] {
                widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(event), Layout::new(&node), iced::mouse::Cursor::Available(point), &renderer,
                    &mut iced::advanced::clipboard::Null, &mut Shell::new(&mut output), &bounds);
            }
            assert_eq!(output.iter().any(|message| matches!(message, Message::ScopedTranscript { action: crate::message::TranscriptAction::Quote(id), .. } if *id == new_id)), !replace,
                "only two clicks on the same message may quote it");
        }
    }

    #[test]
    fn message_controls_are_hidden_until_selection_and_cmd_click_works_on_the_row()
    {
        let messages = vec![RoomMessage::Chat {
            id: 11, from: "alice".into(), body: "hello".into(), received: chrono::Utc::now(),
            status: ChatStatus::Confirmed,
        }];
        let editors = HashMap::from([(11, text_editor::Content::with_text("hello"))]);
        let renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(iced::Font::DEFAULT, iced::Pixels(14.0)));
        for selecting in [false, true]
        {
            let selected = if selecting { HashSet::from([11]) } else { HashSet::new() };
            let mut widget = render_messages(&messages, None, &[], None, false, None, &selected, &editors, false, !selecting, uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
            let mut tree = Tree::new(&widget);
            let bounds = iced::Rectangle::new(iced::Point::ORIGIN, iced::Size::new(600.0, 80.0));
            let node = widget.as_widget_mut().layout(&mut tree, &renderer, &layout::Limits::new(iced::Size::ZERO, bounds.size()));
            let transcript = Layout::new(&node).children().next().unwrap();
            let content_container = transcript.children().next().unwrap();
            let content = content_container.children().next().unwrap();
            let message = content.children().next().unwrap();
            let layout = message.children().next().unwrap();
            let header = layout.children().next().unwrap();
            // The leading checkbox occupies a slot only while selecting.
            assert_eq!(header.children().count(), if selecting { 3 } else { 2 });
            let target = header.children().next().unwrap().bounds().center();
            let mut output = Vec::new();
            for event in [iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left),
                iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)] {
                widget.as_widget_mut().update(&mut tree, &iced::Event::Mouse(event), Layout::new(&node),
                    iced::mouse::Cursor::Available(target), &renderer, &mut iced::advanced::clipboard::Null,
                    &mut Shell::new(&mut output), &bounds);
            }
            assert!(output.iter().any(|m| matches!(m, Message::ScopedTranscript { action: crate::message::TranscriptAction::Select(11), .. })));
            assert!(!output.iter().any(|m| matches!(m, Message::ScopedTranscript { action: crate::message::TranscriptAction::Quote(_), .. })));
        }
    }

}
