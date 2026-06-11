use iced::widget::text_editor;

use crate::xmpp;

#[derive(Debug, Clone)]
pub enum Message
{
    Ignore,
    TabPressed,
    ShiftTabPressed,
    NextSelection,
    PrevSelection,
    JidInputChanged(String),
    PasswordInputChanged(String),
    RememberMeToggled(bool),
    SaveRoomToggled(bool),
    FocusPassword,
    Connect,
    Reconnect,
    // User-triggered immediate reconnect: bypasses the backoff and abandons a
    // stuck in-flight attempt by starting a fresh connection from scratch.
    ForceReconnect,
    CancelConnect,
    XmppEvent(xmpp::XmppEvent),
    Disconnect,
    SelectRoom(usize),
    SelectChat(usize),
    StartChat(String),
    InputAction(text_editor::Action),
    SendMessage,
    // Delayed checks on an optimistically-shown outgoing message (keyed by its
    // negative temp id): show the "sending…" badge once the grace period passes,
    // and mark it failed if no server echo confirms it before the timeout.
    MarkSendPending { conversation: String, temp_id: i64 },
    MarkSendFailed { conversation: String, temp_id: i64 },
    ShowJoinPanel,
    HideJoinPanel,
    JoinInputChanged(String),
    JoinRoom,
    DismissJoinError,
    LeaveRoom,
    CloseChat,
    LeaveSelection,
    OpenUrl(String),
    ForgetAutoLogin,
    WindowFocused,
    WindowUnfocused,
    WindowCloseRequested(iced::window::Id),
    // Message-list scroll position changed; drives infinite-scroll paging and
    // live-tail re-bounding.
    MessagesScrolled(iced::widget::scrollable::Viewport),
    // Find bar.
    ToggleFind,
    CloseFind,
    FindInputChanged(String),
    FindScopeToggled(bool),
    FindNext,
    FindPrev,
}
