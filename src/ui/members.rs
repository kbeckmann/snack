use iced::{ Element, Fill, Length };
use iced::widget::{ button, column, container, scrollable, text, tooltip };

use crate::{ Message, Selection, Snack };

pub fn view(state: &Snack) -> Element<'_, Message>
{
    if let Some(Selection::Room(index)) = state.active
    {
        let room = &state.rooms[index];
        let member_count = room.users.len();
        let members: Vec<Element<'_, Message>> = room.users.iter().map(|u|
        {
            let show_indicator = match u.show.as_deref()
            {
                Some("away") | Some("xa") => " (away)",
                Some("dnd") => " (busy)",
                _ => "",
            };

            let label = if show_indicator.is_empty()
            {
                u.name.clone()
            }
            else
            {
                format!("{}{}", u.name, show_indicator)
            };

            let (tooltip_text, on_press) = match u.jid.as_deref()
            {
                Some(jid) => (jid.to_string(), Some(Message::OpenMemberChat { context: state.composer_id, nick: u.name.clone(), jid: jid.to_string() })),
                None => ("JID not available in this room".to_string(), None),
            };

            let mut name_btn = button(text(label).size(14))
                .padding(0)
                .style(button::text);

            if let Some(msg) = on_press
            {
                name_btn = name_btn.on_press(msg);
            }

            crate::ui::identity::instance(tooltip(
                name_btn,
                container(text(tooltip_text).size(12)).padding(4).style(container::bordered_box),
                tooltip::Position::Right,
            ).into(), ("member", u.name.clone(), u.jid.clone()))
        }).collect();

        let panel: Element<'_, Message> = container(
            column![
                text(format!("Members ({})", member_count)).size(12),
                scrollable(
                    column(members).spacing(6).width(Fill)
                )
            ].spacing(8).width(Fill)
        )
        .width(Length::Fixed(160.0))
        .height(Fill)
        .padding(8)
        .style(container::bordered_box)
        .into();
        return crate::ui::identity::instance(panel, ("members", state.composer_id));
    }
    else
    {
        return container(text(""))
                .width(Length::Fixed(160.0))
                .height(Fill)
                .into();
    }
}
