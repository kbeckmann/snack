use iced::widget::text_editor;

use crate::xmpp;

#[derive(Debug, Clone)]
pub enum ComposerAction {
    Input(text_editor::Action),
    Undo,
    Redo,
    Send,
    Clean(std::sync::Arc<str>),
    Complete(bool),
}

#[derive(Debug, Clone)]
pub enum TranscriptAction {
    Select(i64),
    Quote(i64),
    Editor { id: i64, action: text_editor::Action },
    Drag { id: i64, point: iced::Point },
    Copy(uuid::Uuid),
    Clear(uuid::Uuid),
    Scrolled { search: Option<uuid::Uuid>, viewport: iced::widget::scrollable::Viewport },
}

#[derive(Debug, Clone)]
pub enum SidebarAction {
    SelectRoom(String),
    SelectChat(String),
    Disconnect,
    ForgetAutoLogin,
    ForceReconnect,
    ShowJoinPanel,
    RetryCredentialRemoval,
}

#[derive(Debug, Clone)]
pub enum LoginAction {
    Jid(String),
    Password(String),
    Remember(bool),
    FocusPassword,
    Connect,
    Cancel,
    RetrySavedLogin,
    RetryCredentialRemoval,
    RetryConfigLoad,
    RecoverConfiguration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoomRetryContext {
    pub channel: Option<xmpp::CommandChannel>,
    pub profile: Option<String>,
    pub room: String,
    pub operation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NavigationContext {
    pub channel: Option<xmpp::CommandChannel>,
    pub profile: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShortcutContext {
    pub view: uuid::Uuid,
    pub search: Option<uuid::Uuid>,
    pub selection: uuid::Uuid,
}

#[derive(Debug, Clone)]
pub enum ShortcutAction { Escape, Copy, Join, Find }

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FocusContext {
    pub login: uuid::Uuid,
    pub view: uuid::Uuid,
    pub phase: crate::app::AppState,
    pub search: Option<uuid::Uuid>,
    pub join: bool,
    pub selected: bool,
}

#[derive(Debug, Clone)]
pub enum FindAction { Input(String), Scope(bool), Next, Prev, Close, Retry, Submit }

#[derive(Debug, Clone)]
pub enum JoinFormAction { Input(String), Save(bool), Submit, Cancel }

#[derive(Debug, Clone)]
pub enum SaveNoticeAction { Retry, KeepOpen, Discard }

#[derive(Debug, Clone)]
pub enum Message
{
    Ignore,
    ScopedShortcut { context: ShortcutContext, action: ShortcutAction },
    FlushXmppCommands,
    RetryHistoryWrites,
    ContinueArchiveSync { context: RoomRetryContext, token: uuid::Uuid },
    ScopedFocus { context: FocusContext, backward: bool },
    TraverseFocus { backward: bool },
    TabPressed,
    ShiftTabPressed,
    Navigate { context: NavigationContext, forward: bool },
    NextSelection,
    PrevSelection,
    ScopedLogin { context: uuid::Uuid, phase: crate::app::AppState, action: LoginAction },
    JidInputChanged(String),
    PasswordInputChanged(String),
    RememberMeToggled(bool),
    SaveRoomToggled(bool),
    FocusPassword,
    Connect,
    // User-triggered immediate reconnect: skips the remaining backoff delay
    // while the worker is retrying after a dropped connection.
    ForceReconnect,
    CancelConnect,
    ScopedXmppEvent { channel: xmpp::CommandChannel, event: xmpp::XmppEvent },
    XmppEvent(xmpp::XmppEvent),
    Disconnect,
    ScopedSidebar { channel: Option<xmpp::CommandChannel>, profile: Option<String>, action: SidebarAction },
    SelectRoom(String),
    SelectChat(String),
    StartChat(String),
    OpenMemberChat { context: uuid::Uuid, nick: String, jid: String },
    ScopedComposer { context: uuid::Uuid, action: ComposerAction },
    // Internal editing operations; composer widgets emit ScopedComposer and
    // the update handler validates its identity before dispatching these.
    InputAction(text_editor::Action),
    UndoInput,
    RedoInput,
    CleanMessageInput,
    CopyRejectedSend(uuid::Uuid),
    DiscardRejectedSend(uuid::Uuid),
    BodyDrag { id: i64, point: iced::Point },
    EndBodyDrag,
    SendMessage,
    // Grace check on an optimistically-shown outgoing room message (keyed by its
    // negative temp id): reveal the "sending…" badge once the grace period passes.
    // Delivery retries are backed by the persistent outbox, not this timer.
    MarkSendPending { conversation: String, temp_id: i64 },
    ShowJoinPanel,
    HideJoinPanel,
    ScopedJoinForm { context: uuid::Uuid, action: JoinFormAction },
    JoinInputChanged(String),
    JoinRoom,
    RetryRoomJoin(String),
    ScopedRoomRetry(RoomRetryContext),
    DismissJoinError,
    LeaveRoom,
    CloseChat,
    LeaveSelection,
    // Return keyboard focus to the composer, e.g. Tab from the transcript.
    FocusComposer { context: uuid::Uuid },
    CloseConversation { context: uuid::Uuid },
    OpenUrl(String),
    // Message selection for copy-to-clipboard. `SelectMessage` toggles the chat
    // message with the given history row id (Shift+click, detected via the
    // tracked modifier state, range-extends instead); `CopySelection` writes the
    // selected message(s) to the system clipboard and `ClearSelection` deselects
    // everything. `ModifiersUpdate` keeps the tracked Shift/primary-modifier state
    // in sync.
    ScopedTranscript { context: uuid::Uuid, action: TranscriptAction },
    SelectMessage { id: i64 },
    // Append a quoted reply to the current draft and focus the composer.
    QuoteMessage { id: i64 },
    CopySelection,
    ClearSelection,
    ModifiersUpdate { shift: bool, primary: bool },
    // An action from a per-message read-only body `text_editor` (keyed by the
    // chat message's history row id). With the primary modifier held this routes
    // to whole-message selection; otherwise it drives the body editor's own text
    // selection (edits are dropped to keep it read-only).
    BodyEditorAction { id: i64, action: text_editor::Action },
    // Set the macOS dock icon (a no-op elsewhere). Fired shortly after launch,
    // once the app has finished launching so the icon actually sticks.
    SetDockIcon,
    ForgetAutoLogin,
    RetryCredentialRemoval,
    RetrySavedLogin,
    RetryConfigLoad,
    RecoverConfiguration,
    ActivateWindow,
    WindowFocused,
    WindowUnfocused,
    WindowCloseRequested(iced::window::Id),
    SaveNotice { prompt: Option<uuid::Uuid>, action: SaveNoticeAction },
    RetrySettingsSave,
    KeepWindowOpen,
    CloseWithoutSaving,
    // Message-list scroll position changed; drives infinite-scroll paging and
    // live-tail re-bounding.
    MessagesScrolled(iced::widget::scrollable::Viewport),
    SearchViewport { token: uuid::Uuid, at_bottom: bool },
    // Find bar.
    ScopedFind { context: uuid::Uuid, action: FindAction },
    ToggleFind,
    CloseFind,
    FindInputChanged(String),
    FindScopeToggled(bool),
    FindNext,
    FindPrev,
}
