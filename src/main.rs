#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use iced::{ Application, Program, Theme };

mod app;
mod edit_history;
mod instance;
mod notifications;
mod message;
mod room;
mod storage;
mod subscription;
mod ui;
mod update;
mod view;
mod xmpp;

pub use app::{
    AppState, FindMatch, FindState, NickCompleteState, Selection, Snack,
    ACCOUNT_JID_INPUT_ID, ACCOUNT_PASSWORD_INPUT_ID, FIND_INPUT_ID, JOIN_INPUT_ID,
    MESSAGE_INPUT_ID, MESSAGE_SCROLL_ID,
};
pub use message::Message;

fn main() -> Result<(), Box<dyn std::error::Error>>
{
    init_logging();
    // Keep the OS lock until the UI and its worker subscriptions have stopped.
    // A secondary launch only requests activation; it never loads user state.
    let Some(_instance) = instance::acquire()? else { return Ok(()); };
    application().run()?;
    Ok(())
}

// Give the running process a real dock icon. macOS takes the dock/app icon from
// an .app bundle's .icns, not from winit's per-window icon (which it ignores), so
// a binary launched straight from the terminal would otherwise show a generic
// icon. Setting NSApplication's icon image fixes that — but it only sticks once
// the app has finished launching, so it's invoked from a short delayed message
// after startup (see Snack::new / Message::SetDockIcon), not before run().
#[cfg(target_os = "macos")]
pub(crate) fn set_macos_dock_icon()
{
    use objc2::ClassType;
    use objc2_app_kit::{ NSApplication, NSApplicationActivationPolicy, NSImage };
    use objc2_foundation::{ MainThreadMarker, NSData };

    let Some(mtm) = MainThreadMarker::new() else { return; };
    let app = NSApplication::sharedApplication(mtm);

    // A binary launched straight from the terminal is not a bundled .app, so it
    // can end up without regular-app status — which drops it out of the Cmd-Tab
    // switcher (and a Dock restart can surface this). Pin it to Regular so it is
    // always a focusable, switchable app.
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    let data = NSData::with_bytes(include_bytes!("../icon/snack-transparent-256.png"));
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else { return; };
    unsafe { app.setApplicationIconImage(Some(&image)); }
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
                .theme(Snack::theme)
                // Handle window-close ourselves so drafts are flushed to disk
                // before the app exits (see Message::WindowCloseRequested).
                .exit_on_close_request(false);
}
