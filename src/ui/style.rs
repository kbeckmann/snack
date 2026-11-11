use iced::{ Color, Theme };
use iced::widget::{ button, container, text_editor };

pub fn mention_highlight(_theme: &Theme) -> container::Style
{
    return container::Style
    {
        background: Some(iced::Background::Color(
            Color::from_rgba(1.0, 0.85, 0.35, 0.12)
        )),
        ..Default::default()
    };
}

// A message that matches the current find query.
pub fn find_match_highlight(_theme: &Theme) -> container::Style
{
    return container::Style
    {
        background: Some(iced::Background::Color(
            Color::from_rgba(0.95, 0.80, 0.30, 0.18)
        )),
        ..Default::default()
    };
}

// The single find hit currently focused (Enter/Shift+Enter target).
pub fn find_current_highlight(_theme: &Theme) -> container::Style
{
    return container::Style
    {
        background: Some(iced::Background::Color(
            Color::from_rgba(0.95, 0.65, 0.20, 0.42)
        )),
        ..Default::default()
    };
}

// A chat message currently selected for copy-to-clipboard. A clearly visible
// blue wash so a multi-message selection reads as a contiguous block.
pub fn selected_message(_theme: &Theme) -> container::Style
{
    return container::Style
    {
        background: Some(iced::Background::Color(
            Color::from_rgba(0.40, 0.55, 0.85, 0.30)
        )),
        ..Default::default()
    };
}

// A per-message read-only body editor. It must read as plain inline text, so
// the box chrome (background/border) is stripped and the value colour matches
// the theme's body text; only the drag-selection wash is kept visible.
pub fn body_editor(theme: &Theme, _status: text_editor::Status) -> text_editor::Style
{
    let palette = theme.extended_palette();

    return text_editor::Style
    {
        background: iced::Background::Color(Color::TRANSPARENT),
        border: iced::Border::default(),
        placeholder: palette.secondary.base.color,
        value: palette.background.base.text,
        selection: palette.primary.weak.color,
    };
}

pub fn room_button_active(theme: &Theme, status: button::Status) -> button::Style
{
    let palette = theme.extended_palette();
    let base = button::text(theme, status);

    return button::Style
    {
        background: Some(iced::Background::Color(
            match status
            {
                button::Status::Hovered => palette.primary.weak.color,
                _ => palette.primary.weak.color.scale_alpha(0.5),
            }
        )),
        text_color: palette.primary.strong.text,
        ..base
    };
}
