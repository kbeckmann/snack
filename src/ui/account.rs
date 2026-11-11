use iced::{ Element, Fill, Length };
use iced::widget::{ button, checkbox, column, text, text_input, Id };

use crate::{ AppState, Message, Snack, ACCOUNT_JID_INPUT_ID, ACCOUNT_PASSWORD_INPUT_ID };

pub fn view(state: &Snack) -> Element<'_, Message>
{
    if let Some(error) = &state.saved_config.load_error {
        let mut recovery = column![
            text("Saved settings need recovery").size(18),
            text(error).size(14),
            text("Your existing settings have been kept. Repair the file or its permissions and retry, or back it up and start with new settings.").size(14),
            button("Retry loading settings").on_press(Message::RetryConfigLoad),
            button("Back up settings and start fresh").on_press(Message::RecoverConfiguration),
        ].spacing(12).width(Fill);
        if let Some(notice) = &state.config_notice { recovery = recovery.push(text(notice).size(14)); }
        return owned_form(state, super::centered_form(recovery, "account-form-scroll"));
    }
    let connecting = state.state == AppState::Connecting;

    let mut jid_input = text_input("user@example.org", &state.jid_input)
        .id(Id::new(ACCOUNT_JID_INPUT_ID))
        .padding(10)
        .width(Length::Fixed(400.0));

    let mut password_input = text_input("Password", &state.password_input)
        .id(Id::new(ACCOUNT_PASSWORD_INPUT_ID))
        .secure(true)
        .padding(10)
        .width(Length::Fixed(400.0));

    if !connecting
    {
        jid_input = jid_input
            .on_input(Message::JidInputChanged)
            .on_submit(Message::FocusPassword);
        password_input = password_input
            .on_input(Message::PasswordInputChanged)
            .on_submit(Message::Connect);
    }

    let heading = if connecting
    {
        text("Connecting...").size(18)
    }
    else
    {
        text("Connect to account").size(18)
    };

    let mut remember_checkbox = checkbox(state.remember_me).label("Auto-login");

    if !connecting
    {
        remember_checkbox = remember_checkbox.on_toggle(Message::RememberMeToggled);
    }

    let action_btn = if connecting
    {
        button(text("Cancel").size(14))
            .on_press(Message::CancelConnect)
            .padding(10)
    }
    else
    {
        button(text("Connect").size(14))
            .on_press(Message::Connect)
            .padding(10)
    };

    let mut form = column![
        heading,
        jid_input,
        password_input,
        remember_checkbox,
        action_btn,
    ].spacing(12).align_x(iced::Alignment::Center).width(Fill);

    if !connecting && state.saved_login_error.is_none() && state.password_input.is_empty()
        && state.saved_config.jid.as_deref().is_some_and(|jid| jid == state.jid_input.trim()
            && !state.saved_config.pending_credential_removals.iter().any(|pending| pending == jid)) {
        form = form.push(button("Use saved login").on_press(Message::RetrySavedLogin));
    }

    if let Some(notice) = &state.config_notice { form = form.push(text(notice).size(12)); }

    if let Some(notice) = super::credential_notice(state) { form = form.push(notice); }

    if let Some(ref err) = state.connect_error
    {
        form = form.push(text(err.clone()).size(14));
    }

    return owned_form(state, super::centered_form(form, "account-form-scroll"));
}

fn owned_form<'a>(state: &Snack, content: Element<'a, Message>) -> Element<'a, Message> {
    let context = state.login_form_id;
    let phase = state.state.clone();
    let identity = ("login-form", context, phase.clone(), state.saved_config.load_error.is_some());
    let content = content.map(move |message| {
        use crate::message::LoginAction;
        let action = match message {
            Message::JidInputChanged(value) => LoginAction::Jid(value),
            Message::PasswordInputChanged(value) => LoginAction::Password(value),
            Message::RememberMeToggled(value) => LoginAction::Remember(value),
            Message::FocusPassword => LoginAction::FocusPassword,
            Message::Connect => LoginAction::Connect,
            Message::CancelConnect => LoginAction::Cancel,
            Message::RetrySavedLogin => LoginAction::RetrySavedLogin,
            Message::RetryCredentialRemoval => LoginAction::RetryCredentialRemoval,
            Message::RetryConfigLoad => LoginAction::RetryConfigLoad,
            Message::RecoverConfiguration => LoginAction::RecoverConfiguration,
            other => return other,
        };
        Message::ScopedLogin { context, phase: phase.clone(), action }
    });
    super::identity::instance(content, identity)
}
