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
}
