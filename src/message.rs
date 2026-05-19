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
