#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use iced::{ Application, Program, Theme };

mod app;
mod message;
mod room;
mod storage;
mod subscription;
mod ui;
mod update;
mod view;
mod xmpp;

pub use app::{
    AppState, NickCompleteState, Selection, Snack,
    ACCOUNT_JID_INPUT_ID, ACCOUNT_PASSWORD_INPUT_ID, JOIN_INPUT_ID,
    MESSAGE_INPUT_ID, MESSAGE_SCROLL_ID,
};
pub use message::Message;

fn main() -> iced::Result
{
    init_logging();
    return application().run();
}

/// Default to `debug` for snack + libxmpp when built without
/// optimisations (i.e. `cargo run`). `RUST_LOG`, when set, always wins.
fn init_logging()
{
    let default_filter = if cfg!(debug_assertions)
    {
        "snack=debug,xmpp=debug,info"
    }
    else
    {
        "info"
    };

    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(default_filter),
    )
    .format_timestamp_millis()
    .init();
}

fn application() -> Application<impl Program<Message = Message, Theme = Theme>>
{
    return iced::application(Snack::new, Snack::update, Snack::view)
                .subscription(Snack::subscription)
                .title(Snack::title)
                .theme(Snack::theme);
}
