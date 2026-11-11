use iced::{ Element, Fill, Length };
use iced::widget::{ button, checkbox, column, row, text, text_input, Id };

use crate::{ Message, Snack, JOIN_INPUT_ID };

pub fn view(state: &Snack) -> Element<'_, Message>
{
    let joining = state.joining_room.is_some();

    let heading = if joining
    {
        text("Joining...").size(18)
    }
    else
    {
        text("Join a room").size(18)
    };

    let mut join_input = text_input("room@conference.example.org", &state.join_input)
        .id(Id::new(JOIN_INPUT_ID))
        .padding(10)
        .width(Length::Fixed(400.0));

    if !joining
    {
        join_input = join_input
            .on_input(Message::JoinInputChanged)
            .on_submit(Message::JoinRoom);
    }

    let mut save_checkbox = checkbox(state.save_room).label("Save this room");

    if !joining
    {
        save_checkbox = save_checkbox.on_toggle(Message::SaveRoomToggled);
    }

    let mut join_btn = button(text("Join").size(14)).padding(10);
    if !joining
    {
        join_btn = join_btn.on_press(Message::JoinRoom);
    }

    let mut buttons = row![].spacing(8);
    if (state.active.is_some() || !state.unopened_room_jids().is_empty()) && !joining
    {
        let cancel_btn = button(text("Cancel").size(14))
            .on_press(Message::HideJoinPanel)
            .padding(10)
            .style(button::text);
        buttons = buttons.push(cancel_btn);
    }
    buttons = buttons.push(join_btn);

    let mut form = column![
        heading,
        join_input,
        save_checkbox,
        buttons,
    ].spacing(12).align_x(iced::Alignment::Center).width(Fill);

    if let Some(ref err) = state.join_error
    {
        form = form.push(text(err.clone()).size(14));
    }

    let form = super::centered_form(form, "join-form-scroll");
    let context = state.composer_id;
    let form = form.map(move |message| {
        use crate::message::JoinFormAction;
        let action = match message {
            Message::JoinInputChanged(value) => JoinFormAction::Input(value),
            Message::SaveRoomToggled(value) => JoinFormAction::Save(value),
            Message::JoinRoom => JoinFormAction::Submit,
            Message::HideJoinPanel => JoinFormAction::Cancel,
            other => return other,
        };
        Message::ScopedJoinForm { context, action }
    });
    crate::ui::identity::instance(form, ("join-form", context, joining))
}
