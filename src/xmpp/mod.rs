use iced::futures::SinkExt;
use iced::stream;
use log::error;
use std::sync::{ Arc, Mutex };
use std::time::Duration;

/// Delay before the first reconnect attempt after a session drops.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// Upper bound on the exponential backoff between reconnect attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// How often to send a keep-alive ping on an established session. This is what
/// surfaces a silently half-open socket, where reads would otherwise block
/// forever and never report the drop.
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// How long to wait for a ping reply before treating the connection as dead.
const PING_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound on a single connection-setup attempt (DNS, TCP, TLS, SASL,
/// resource bind). Without this a reconnect fired before the network is back
/// (e.g. right after wake-from-sleep) blocks forever on a dead socket.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);
/// A blocked write must trigger reconnect rather than stall the worker forever.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How often to compare wall-clock and monotonic time to notice a system sleep.
const SUSPEND_CHECK_INTERVAL: Duration = Duration::from_secs(2);
/// Wall-clock time beyond monotonic time that counts as a suspend (not jitter).
const SUSPEND_SLACK: Duration = Duration::from_secs(5);
/// After a suspend this long the session is replaced rather than probed. The
/// peer kept sending while we slept, so its TCP retransmissions are backed off
/// and its stream-management requests went unanswered: stanzas we send may be
/// delivered while every reply (echoes, acknowledgements) stalls for minutes.
const SUSPEND_RECONNECT: Duration = Duration::from_secs(60);

/// Notices system sleep. The monotonic clock (and every tokio timer) stops
/// while macOS sleeps, so keep-alive intervals cannot see the gap; the
/// wall clock does advance.
struct SuspendDetector
{
    monotonic: std::time::Instant,
    wall: std::time::SystemTime,
}

impl SuspendDetector
{
    fn new() -> Self { Self { monotonic: std::time::Instant::now(), wall: std::time::SystemTime::now() } }

    fn check(&mut self) -> Option<Duration> { self.check_at(std::time::Instant::now(), std::time::SystemTime::now()) }

    // How long the system was suspended since the previous check, if at all.
    // A wall clock set backwards only re-bases the detector.
    fn check_at(&mut self, monotonic: std::time::Instant, wall: std::time::SystemTime) -> Option<Duration>
    {
        let awake = monotonic.saturating_duration_since(self.monotonic);
        let elapsed = wall.duration_since(self.wall).unwrap_or_default();
        self.monotonic = monotonic;
        self.wall = wall;
        let suspended = elapsed.saturating_sub(awake);
        (suspended >= SUSPEND_SLACK).then_some(suspended)
    }
}
const CAPS_NODE: &str = "https://github.com/mikaelsahlstrom/snack";

#[derive(Debug, Clone)]
pub struct RoomOperation
{
    pub id: String,
    current: Arc<std::sync::atomic::AtomicBool>,
}

impl RoomOperation
{
    pub fn new() -> Self { Self { id: uuid::Uuid::new_v4().to_string(), current: Arc::new(std::sync::atomic::AtomicBool::new(true)) } }
    pub fn is_current(&self) -> bool { self.current.load(std::sync::atomic::Ordering::SeqCst) }
    pub fn cancel(&self) { self.current.store(false, std::sync::atomic::Ordering::SeqCst); }
}

#[derive(Debug)]
pub enum XmppCommand
{
    ScopedRoom { operation: RoomOperation, command: Box<XmppCommand> },
    // UI barrier: all commands before this marker belong to an older session.
    SessionReady(u64),
    JoinRoom(String),
    LeaveRoom { room: String, nick: String },
    // `id` is the outbox row id; echoed back in SendResult so the UI can mark
    // the queued message delivered (or keep it queued for retry).
    // `origin_id` is the persisted wire identity, reused by every retry.
    SendRoomMessage { id: i64, origin_id: Option<String>, room: String, body: String },
    SendDirectMessage { id: i64, origin_id: Option<String>, to: String, body: String },
    // Page the server-side archive (XEP-0313 MAM). `to` is the room JID for a
    // MUC archive, `with` the contact JID for a 1:1 archive. `forward` walks
    // towards the present from `start` (catch-up) or towards the past bounded by
    // `end` (scroll-up); `cursor` is the RSM cursor for subsequent pages.
    QueryArchive
    {
        query_id: String,
        to: Option<String>,
        with: Option<String>,
        start: Option<String>,
        end: Option<String>,
        forward: bool,
        cursor: Option<String>,
        max: u32,
    },
    /// User-triggered: skip the remaining reconnect backoff and retry now.
    ForceReconnect,
}

impl XmppCommand
{
    pub fn inner(&self) -> &Self { match self { Self::ScopedRoom { command, .. } => command.inner(), command => command } }
    pub fn into_inner(self) -> Self { match self { Self::ScopedRoom { command, .. } => command.into_inner(), command => command } }
    pub fn is_current(&self) -> bool { match self { Self::ScopedRoom { operation, command } => operation.is_current() && command.is_current(), _ => true } }
    // Membership cleanup must retain FIFO ordering even after a new UI entry
    // cancels its operation. Its old-context events remain fenced separately.
    pub fn should_execute(&self) -> bool { matches!(self.inner(), Self::LeaveRoom { .. }) || self.is_current() }
}

struct ChannelInner
{
    rx: Option<tokio::sync::mpsc::Receiver<XmppCommand>>,
    jid: String,
    password: String,
    stop: tokio::sync::watch::Sender<bool>,
}

#[derive(Clone)]
pub struct CommandChannel(Arc<Mutex<ChannelInner>>);

impl CommandChannel {
    pub fn request_stop(&self) { self.0.lock().unwrap().stop.send_replace(true); }
}

impl std::fmt::Debug for CommandChannel
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result
    {
        // ChannelInner contains credentials. Debug output must never inspect it.
        f.debug_struct("CommandChannel").finish_non_exhaustive()
    }
}

impl PartialEq for CommandChannel
{
    fn eq(&self, other: &Self) -> bool
    {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for CommandChannel {}

impl std::hash::Hash for CommandChannel
{
    fn hash<H: std::hash::Hasher>(&self, state: &mut H)
    {
        (Arc::as_ptr(&self.0) as usize).hash(state);
    }
}

pub fn new_command_channel(jid: String, password: String) -> (tokio::sync::mpsc::Sender<XmppCommand>, CommandChannel)
{
    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let (stop, _) = tokio::sync::watch::channel(false);
    (tx, CommandChannel(Arc::new(Mutex::new(ChannelInner { rx: Some(rx), jid, password, stop }))))
}

#[derive(Debug, Clone)]
pub enum XmppEvent
{
    Stopped,
    RoomEvent { room: String, context: String, event: Box<XmppEvent> },
    // A replay marker was present, but its timestamp could not be validated.
    // The inner message carries receipt time for display, not archive provenance.
    UnstampedReplay(Box<XmppEvent>),
    /// Delivered before the lifecycle event and incoming data of each session.
    BoundJid(String),
    Connected,
    /// An established session dropped; the worker is now retrying with backoff.
    Reconnecting,
    /// A dropped session was re-established. The UI re-joins its open rooms.
    Reconnected { generation: u64 },
    Disconnected(String),
    RoomJoined { room: String, members: Vec<::xmpp::RoomMember>, own_nick: String },
    RoomPrivacy { room: String, privacy: ::xmpp::RoomPrivacy },
    MemberNicknameChanged { room: String, old_nick: String, new_nick: String, is_self: bool },
    RoomJoinFailed { room: String, reason: String },
    RoomLeft(String),
    MemberJoined { room: String, member: ::xmpp::RoomMember },
    MemberLeft { room: String, nick: String },
    RoomMessage
    {
        room: String,
        // Empty is the reserved room-service source, not an occupant.
        nick: String,
        body: String,
        timestamp: chrono::DateTime<chrono::Utc>,
        // True when the stanza carried an XEP-0203 replay marker, even if its
        // timestamp is invalid. UnstampedReplay separately marks receipt time.
        delayed: bool,
        // XEP-0359 stable id; identical on the live, replayed and archived copies
        // of a message, so it is the primary dedup key. None if the server omits it.
        stanza_id: Option<String>,
        origin_id: Option<String>,
    },
    RoomSubject
    {
        room: String,
        subject: String,
    },
    PresenceError
    {
        from: String,
        condition: String,
        text: Option<String>,
    },
    DirectMessage
    {
        from: String,
        body: String,
        timestamp: chrono::DateTime<chrono::Utc>,
        delayed: bool,
        stanza_id: Option<String>,
        origin_id: Option<String>,
    },
    // A single message returned by a MAM archive query.
    ArchiveMessage
    {
        query_id: String,
        from: String,
        to: Option<String>,
        body: String,
        timestamp: chrono::DateTime<chrono::Utc>,
        stanza_id: Option<String>,
        origin_id: Option<String>,
    },
    // Result of attempting an outgoing send: `id` is the outbox row id, `ok` is
    // whether the write and server confirmation succeeded. This is not a
    // recipient receipt. Drives the delivery outbox.
    SendResult { id: i64, ok: bool },
    SendRejected { id: i64, conversation: String, body: String, reason: String },
    RoomPrivateMessage
    {
        from: String,
        body: String,
        timestamp: chrono::DateTime<chrono::Utc>,
        delayed: bool,
        stanza_id: Option<String>,
        origin_id: Option<String>,
    },
    MessageError { origin_id: String, from: Option<String>, error_type: String, condition: String, text: Option<String> },
    // A MAM archive query finished. `complete` means no further pages remain in
    // the query's direction; `first`/`last` are the RSM cursors for paging
    // further back (`first`) or continuing a forward catch-up (`last`).
    ArchiveComplete
    {
        query_id: String,
        complete: bool,
        first: Option<String>,
        last: Option<String>,
    },
    ArchiveFailed { query_id: String, reason: String },
}

impl XmppEvent {
    pub(crate) fn with_unstamped_replay(self, unstamped: bool) -> Self {
        if unstamped { Self::UnstampedReplay(Box::new(self)) } else { self }
    }

    pub(crate) fn split_unstamped_replay(self) -> (Self, bool) {
        let mut event = self;
        let mut unstamped = false;
        loop {
            event = match event {
                Self::UnstampedReplay(inner) => { unstamped = true; *inner },
                event => return (event, unstamped),
            };
        }
    }
}

fn message_with_delay(delay: Option<::xmpp::Delay>, message: impl FnOnce(chrono::DateTime<chrono::Utc>, bool) -> XmppEvent) -> XmppEvent {
    let delayed = delay.is_some();
    let timestamp = delay.and_then(|delay| delay.stamp)
        .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(&stamp).ok())
        .map(|stamp| stamp.with_timezone(&chrono::Utc));
    let unstamped = delayed && timestamp.is_none();
    message(timestamp.unwrap_or_else(chrono::Utc::now), delayed).with_unstamped_replay(unstamped)
}

/// Why a running session ended, telling the outer loop whether to retry.
enum SessionOutcome
{
    /// The connection dropped (read error, server close, or a failed
    /// keep-alive ping). The outer loop reconnects with backoff.
    Dropped,
    /// The UI logged out: the command sender was dropped. Stop for good.
    LoggedOut,
    /// The UI (and the event stream) went away. Stop for good.
    UiGone,
}

/// True for failures that must never be retried: bad credentials or no
/// usable SASL mechanism. The user has to re-authenticate.
fn is_auth_failure(e: &::xmpp::XmppError) -> bool
{
    matches!(e, ::xmpp::XmppError::Auth(_) | ::xmpp::XmppError::NoSaslMechanism)
}

/// Outcome of waiting out the reconnect backoff while draining commands that
/// can't be delivered.
enum BackoffWait
{
    /// The UI logged out: the command sender was dropped.
    Closed,
    /// The user asked to skip the rest of the backoff and retry now.
    ForceReconnect,
}

/// Drain commands issued while reconnecting (they can't be delivered, so most
/// are discarded). Completes when the channel closes (logout) or the UI asks
/// to skip the backoff.
async fn drain_until_signal(cmd_rx: &mut tokio::sync::mpsc::Receiver<XmppCommand>) -> BackoffWait
{
    while let Some(cmd) = cmd_rx.recv().await
    {
        match cmd
        {
            XmppCommand::ForceReconnect => return BackoffWait::ForceReconnect,
            other => log::debug!("Dropping command issued during reconnect backoff: {:?}", other),
        }
    }

    return BackoffWait::Closed;
}

async fn with_io_timeout<T>(
    timeout: Duration,
    operation: impl std::future::Future<Output = Result<T, ::xmpp::XmppError>>,
) -> Result<T, ::xmpp::XmppError>
{
    tokio::time::timeout(timeout, operation).await
        .map_err(|_| ::xmpp::XmppError::Timeout("XMPP write timed out".into()))?
}

// Return whether the session remains usable. Local input errors need user
// correction; reconnecting and replaying the same text cannot repair them.
async fn report_send_result(
    result: Result<::xmpp::SendConfirmation, ::xmpp::XmppError>, id: i64,
    conversation: String, body: String, bridge: &tokio::sync::mpsc::Sender<XmppEvent>,
) -> bool
{
    match result {
        Err(::xmpp::XmppError::InvalidInput(reason)) => {
            bridge.send(XmppEvent::SendRejected { id, conversation, body, reason }).await.is_ok()
        }
        result => {
            let ok = result.is_ok();
            if let Err(e) = result { error!("Failed to send message: {}", e); }
            bridge.send(XmppEvent::SendResult { id, ok }).await.is_ok() && ok
        }
    }
}

/// Advertise snack's real XEP-0030 service-discovery identity and features, then
/// re-broadcast presence so the XEP-0115 entity-capabilities hash peers cache
/// reflects snack rather than libxmpp's defaults. Called after every (re)connect,
/// since each fresh client starts from libxmpp's default disco.
async fn advertise_capabilities(client: &::xmpp::XmppClient) -> Result<(), ::xmpp::XmppError>
{
    client.set_disco_info(::xmpp::DiscoInfo
    {
        identities: vec![::xmpp::DiscoIdentity
        {
            category: "client".to_string(),
            kind: "pc".to_string(),
            name: Some("snack".to_string()),
        }],
        // The namespaces snack actually implements: the caps/disco/ping features
        // libxmpp answers on our behalf, MUC group chat, and XEP-0203 delayed
        // delivery, which we understand on incoming messages.
        features: vec![
            "http://jabber.org/protocol/caps".to_string(),
            "http://jabber.org/protocol/disco#info".to_string(),
            "http://jabber.org/protocol/disco#items".to_string(),
            "http://jabber.org/protocol/muc".to_string(),
            "urn:xmpp:ping".to_string(),
            "urn:xmpp:delay".to_string(),
        ],
    });
    client.set_caps_node(CAPS_NODE);

    // Re-advertise: changing the feature set changed the caps `ver` hash, so peers
    // need the fresh presence to refresh their cached view of our capabilities.
    return with_io_timeout(WRITE_TIMEOUT, client.send_presence()).await;
}

// Keep consuming library events while command writes and probes are awaiting
// I/O. Otherwise the bounded library queue can fill before its reader reaches
// the ping reply / stream acknowledgement, making a healthy session time out.
async fn forward_session_events(
    event_rx: tokio::sync::mpsc::Receiver<::xmpp::XmppEvent>,
    bridge_tx: &tokio::sync::mpsc::Sender<XmppEvent>,
    bound_jid: &str,
    established: bool,
    generation: u64,
) -> SessionOutcome
{
    forward_events(event_rx, bridge_tx, bound_jid, established, generation, true).await
}

async fn forward_events(
    mut event_rx: tokio::sync::mpsc::Receiver<::xmpp::XmppEvent>,
    bridge_tx: &tokio::sync::mpsc::Sender<XmppEvent>,
    bound_jid: &str,
    established: bool,
    generation: u64,
    emit_lifecycle: bool,
) -> SessionOutcome
{
    while let Some(ev) = event_rx.recv().await
    {
        let (context, ev) = match ev {
            ::xmpp::XmppEvent::RoomEvent { room, context, event } => (Some((room, context)), *event),
            event => (None, event),
        };
        let private_room = matches!(&ev, ::xmpp::XmppEvent::RoomPrivateMessage { .. });
        let mapped = match ev
        {
            ::xmpp::XmppEvent::Connected =>
            {
                if bridge_tx.send(XmppEvent::BoundJid(bound_jid.into())).await.is_err() { return SessionOutcome::UiGone; }
                // A re-established session reuses the server's
                // Connected event, but the UI handles it differently.
                emit_lifecycle.then(|| if established { XmppEvent::Reconnected { generation } } else { XmppEvent::Connected })
            }
            ::xmpp::XmppEvent::RoomJoined { room, members, own_nick } =>
            {
                Some(XmppEvent::RoomJoined { room, members, own_nick })
            }
            ::xmpp::XmppEvent::MemberNicknameChanged { room, old_nick, new_nick, is_self } => Some(XmppEvent::MemberNicknameChanged { room, old_nick, new_nick, is_self }),
            ::xmpp::XmppEvent::RoomJoinFailed { room, reason } => Some(XmppEvent::RoomJoinFailed { room, reason }),
            ::xmpp::XmppEvent::RoomLeft(room) =>
            {
                Some(XmppEvent::RoomLeft(room))
            }
            ::xmpp::XmppEvent::MemberJoined { room, member } =>
            {
                Some(XmppEvent::MemberJoined { room, member })
            }
            ::xmpp::XmppEvent::MemberLeft { room, nick } =>
            {
                Some(XmppEvent::MemberLeft { room, nick })
            }
            ::xmpp::XmppEvent::RoomMessage { room, nick, body, delay, stanza_id, origin_id } =>
            {
                Some(message_with_delay(delay, |timestamp, delayed|
                    XmppEvent::RoomMessage { room, nick, body, timestamp, delayed, stanza_id, origin_id }))
            }
            ::xmpp::XmppEvent::RoomServiceMessage { room, body, delay, stanza_id, origin_id } => {
                Some(message_with_delay(delay, |timestamp, delayed|
                    XmppEvent::RoomMessage { room, nick: String::new(), body, timestamp, delayed, stanza_id, origin_id }))
            }
            ::xmpp::XmppEvent::RoomPrivacy { room, privacy } => Some(XmppEvent::RoomPrivacy { room, privacy }),
            ::xmpp::XmppEvent::RoomSubject { room, subject } =>
            {
                Some(XmppEvent::RoomSubject { room, subject })
            }
            ::xmpp::XmppEvent::PresenceError { from, error_type: _, condition, text } =>
            {
                Some(XmppEvent::PresenceError { from, condition, text })
            }
            ::xmpp::XmppEvent::DirectMessage { from, body, delay, stanza_id, origin_id }
                | ::xmpp::XmppEvent::RoomPrivateMessage { from, body, delay, stanza_id, origin_id } =>
            {
                Some(message_with_delay(delay, |timestamp, delayed| if private_room {
                    XmppEvent::RoomPrivateMessage { from, body, timestamp, delayed, stanza_id, origin_id }
                } else { XmppEvent::DirectMessage { from, body, timestamp, delayed, stanza_id, origin_id } }))
            }
            ::xmpp::XmppEvent::MessageError { id, from, error_type, condition, text } =>
            {
                Some(XmppEvent::MessageError { origin_id: id, from, error_type, condition, text })
            }
            ::xmpp::XmppEvent::ArchiveMessage { query_id, from, to, body, timestamp, stanza_id, origin_id } =>
            {
                // Archive dates determine persistent history order and paging.
                // Substituting receipt time would permanently misdate old text
                // and let a successful page cursor skip an unrecoverable result.
                match timestamp.and_then(|stamp| chrono::DateTime::parse_from_rfc3339(&stamp).ok()) {
                    Some(timestamp) => Some(XmppEvent::ArchiveMessage {
                        query_id, from, to, body, timestamp: timestamp.with_timezone(&chrono::Utc), stanza_id, origin_id,
                    }),
                    None => Some(XmppEvent::ArchiveFailed {
                        query_id, reason: "An archived message has a missing or invalid timestamp.".into(),
                    }),
                }
            }
            ::xmpp::XmppEvent::ArchiveComplete { query_id, complete, first, last } =>
            {
                Some(XmppEvent::ArchiveComplete { query_id, complete, first, last })
            }
            ::xmpp::XmppEvent::ArchiveFailed { query_id, reason } =>
            {
                Some(XmppEvent::ArchiveFailed { query_id, reason })
            }
            _ => None,
        };

        if let Some(evt) = mapped
        {
            let evt = if let Some((room, context)) = context { XmppEvent::RoomEvent { room, context, event: Box::new(evt) } } else { evt };
            if bridge_tx.send(evt).await.is_err()
            {
                return SessionOutcome::UiGone;
            }
        }
    }
    return SessionOutcome::Dropped;
}

// The UI sends a marker after consuming Connected/Reconnected and before its
// fresh rejoin/outbox commands. FIFO ordering lets us reject every stale queued
// command without racing the UI or accidentally sending an old outbox attempt.
// An epoch on the marker also rejects markers left over from older sessions.
async fn next_session_command(
    cmd_rx: &mut tokio::sync::mpsc::Receiver<XmppCommand>,
    ready: &mut bool,
    generation: u64,
) -> Option<XmppCommand>
{
    while let Some(command) = cmd_rx.recv().await
    {
        match command {
            XmppCommand::SessionReady(epoch) => {
                if epoch == generation { *ready = true; }
            }
            command if *ready => return Some(command),
            _ => {},
        }
    }
    return None;
}

async fn run_session_commands(
    client: &mut ::xmpp::XmppClient,
    cmd_rx: &mut tokio::sync::mpsc::Receiver<XmppCommand>,
    bridge_tx: &tokio::sync::mpsc::Sender<XmppEvent>,
    nick: &str,
    generation: u64,
) -> SessionOutcome
{
    let mut ready = false;
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first tick fires immediately; skip it so we don't ping right away.
    ping.tick().await;
    let mut suspend_check = tokio::time::interval(SUSPEND_CHECK_INTERVAL);
    suspend_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    suspend_check.tick().await;
    let mut suspend = SuspendDetector::new();

    return loop
    {
        tokio::select!
        {
            cmd = next_session_command(cmd_rx, &mut ready, generation) =>
            {
                let (operation, cmd) = match cmd {
                    Some(command) => {
                        let Some((operation, command)) = current_room_command(command, bridge_tx).await else { continue; };
                        (operation, Some(command))
                    }
                    None => (None, None),
                };
                match cmd
                {
                    Some(XmppCommand::JoinRoom(room_jid)) =>
                    {
                        if let Err(e) = with_io_timeout(WRITE_TIMEOUT, async {
                            if let Some(operation) = &operation { client.join_room_with_context(&room_jid, nick, &operation.id).await }
                            else { client.join_room(&room_jid, nick).await }
                        }).await
                        {
                            let invalid_input = matches!(e, ::xmpp::XmppError::InvalidInput(_));
                            let event = XmppEvent::RoomJoinFailed { room: room_jid.clone(), reason: e.to_string() };
                            let event = if let Some(operation) = &operation { XmppEvent::RoomEvent { room: room_jid, context: operation.id.clone(), event: Box::new(event) } } else { event };
                            let _ = bridge_tx.send(event).await;
                            if !invalid_input { break SessionOutcome::Dropped; }
                        }
                    }
                    Some(XmppCommand::SendRoomMessage { id, origin_id, room, body }) =>
                    {
                        let result = client.send_room_message_confirmed(&room, &body, origin_id.as_deref(), WRITE_TIMEOUT).await;
                        if !report_send_result(result, id, room, body, bridge_tx).await { break SessionOutcome::Dropped; }
                    }
                    Some(XmppCommand::LeaveRoom { room, nick }) =>
                    {
                        match with_io_timeout(WRITE_TIMEOUT, client.request_room_leave(&room, &nick, operation.as_ref().map(|op| op.id.as_str()))).await {
                            Ok(::xmpp::RoomLeaveOutcome::AlreadyAbsent) => {
                                let event = XmppEvent::RoomLeft(room.clone());
                                let event = if let Some(operation) = &operation { XmppEvent::RoomEvent { room, context: operation.id.clone(), event: Box::new(event) } } else { event };
                                let _ = bridge_tx.send(event).await;
                            }
                            Ok(::xmpp::RoomLeaveOutcome::Requested) => {},
                            Err(e) => {
                                error!("Failed to leave room: {}", e);
                                if !matches!(e, ::xmpp::XmppError::InvalidInput(_)) { break SessionOutcome::Dropped; }
                            }
                        }
                    }
                    Some(XmppCommand::SendDirectMessage { id, origin_id, to, body }) =>
                    {
                        let result = if to.contains('/') {
                            client.send_room_private_message_confirmed(&to, &body, origin_id.as_deref(), WRITE_TIMEOUT).await
                        } else { client.send_message_confirmed(&to, &body, origin_id.as_deref(), WRITE_TIMEOUT).await };
                        if !report_send_result(result, id, to, body, bridge_tx).await { break SessionOutcome::Dropped; }
                    }
                    Some(XmppCommand::QueryArchive { query_id, to, with, start, end, forward, cursor, max }) =>
                    {
                        if let Err(e) = with_io_timeout(WRITE_TIMEOUT, client.query_archive(::xmpp::ArchiveQuery
                        {
                            query_id: &query_id,
                            to: to.as_deref(),
                            with: with.as_deref(),
                            start: start.as_deref(),
                            end: end.as_deref(),
                            forward,
                            cursor: cursor.as_deref(),
                            max,
                        })).await
                        {
                            error!("query_archive({}) failed: {}", query_id, e);
                            if matches!(e, ::xmpp::XmppError::InvalidInput(_)) {
                                let _ = bridge_tx.send(XmppEvent::ArchiveFailed { query_id, reason: e.to_string() }).await;
                                continue;
                            }
                            // A failed write is not an exhausted archive. The
                            // reconnect event releases this query for retry.
                            break SessionOutcome::Dropped;
                        }
                    }
                    Some(XmppCommand::SessionReady(_)) => unreachable!("session markers are consumed before dispatch"),
                    Some(XmppCommand::ScopedRoom { .. }) => unreachable!("room wrappers are removed before dispatch"),
                    Some(XmppCommand::ForceReconnect) =>
                    {
                        // Only meaningful while reconnecting; the session is
                        // already up, so there's nothing to do.
                    }
                    None => break SessionOutcome::LoggedOut,
                }
            }
            _ = suspend_check.tick() =>
            {
                let Some(suspended) = suspend.check() else { continue; };
                if suspended >= SUSPEND_RECONNECT {
                    error!("System was suspended for {}s; replacing the session before using it", suspended.as_secs());
                    break SessionOutcome::Dropped;
                }
                // A short sleep may have left the socket half-open: probe now
                // rather than after the rest of the ping interval.
                ping.reset();
                if let Err(e) = keep_alive_probe(client).await {
                    error!("Keep-alive probe after a {}s suspend failed, treating session as dropped: {}", suspended.as_secs(), e);
                    break SessionOutcome::Dropped;
                }
            }
            _ = ping.tick() =>
            {
                // A half-open socket never surfaces a read error, so probe it.
                // When XEP-0198 stream management is negotiated, request a stream
                // acknowledgement instead of a ping: the returned `<a/>` both proves
                // the link is alive and confirms the server has processed every
                // stanza we've sent — a stronger guarantee than a bare ping.
                if let Err(e) = keep_alive_probe(client).await
                {
                    error!("Keep-alive probe failed, treating session as dropped: {}", e);
                    break SessionOutcome::Dropped;
                }
            }
        }
    };

}

async fn keep_alive_probe(client: &mut ::xmpp::XmppClient) -> Result<(), ::xmpp::XmppError>
{
    if client.is_stream_management_enabled() { client.request_ack(PING_TIMEOUT).await }
    else { client.ping(None, PING_TIMEOUT).await.map(|_| ()) }
}

async fn current_room_command(command: XmppCommand, bridge: &tokio::sync::mpsc::Sender<XmppEvent>) -> Option<(Option<RoomOperation>, XmppCommand)>
{
    match command {
        XmppCommand::ScopedRoom { operation, command } => {
            if !matches!(command.inner(), XmppCommand::LeaveRoom { .. }) && (!operation.is_current() || !command.is_current()) {
                if let XmppCommand::SendRoomMessage { id, room, body, .. } = command.inner() {
                    let event = if *id > 0 { XmppEvent::SendResult { id: *id, ok: false } }
                    else { XmppEvent::SendRejected { id: *id, conversation: room.clone(), body: body.clone(), reason: "Room operation changed before this message was sent.".into() } };
                    let _ = bridge.send(event).await;
                }
                return None;
            }
            Some((Some(operation), command.into_inner()))
        }
        command => Some((None, command)),
    }
}

// Borrow the pinned forwarder so ending command work cannot discard an event
// already removed from the native queue but awaiting UI bridge capacity.
async fn drive_session_events(
    events: std::pin::Pin<&mut impl std::future::Future<Output = SessionOutcome>>,
    work: impl std::future::Future<Output = SessionOutcome>,
) -> (SessionOutcome, bool)
{
    tokio::select!
    {
        outcome = events => (outcome, true),
        outcome = work => (outcome, false),
    }
}

async fn finish_session_events(
    events: std::pin::Pin<&mut impl std::future::Future<Output = SessionOutcome>>,
    close: impl std::future::Future<Output = ()>,
)
{
    // Closing the native client can emit its final messages. Keep draining
    // concurrently, including a bridge send already pending when work ended.
    if tokio::time::timeout(WRITE_TIMEOUT, async { tokio::join!(close, events); }).await.is_err() {
        error!("XMPP close and incoming event drain timed out");
    }
}

async fn close_unstarted_session(client: ::xmpp::XmppClient, event_rx: tokio::sync::mpsc::Receiver<::xmpp::XmppEvent>,
    bridge_tx: &tokio::sync::mpsc::Sender<XmppEvent>, established: bool, generation: u64)
{
    let jid = client.get_jid().to_string();
    let events = forward_events(event_rx, bridge_tx, &jid, established, generation, false);
    tokio::pin!(events);
    finish_session_events(events.as_mut(), client.close()).await;
}

/// Run one established session until it ends. Multiplexes server events, UI
/// commands, and a keep-alive ping that detects a silently half-open socket.
/// `established` distinguishes a fresh login from a reconnect so the server's
/// `Connected` event maps to the right UI event.
async fn run_session(
    mut client: ::xmpp::XmppClient,
    event_rx: tokio::sync::mpsc::Receiver<::xmpp::XmppEvent>,
    cmd_rx: &mut tokio::sync::mpsc::Receiver<XmppCommand>,
    bridge_tx: &tokio::sync::mpsc::Sender<XmppEvent>,
    established: bool,
    generation: u64,
    stop: &mut tokio::sync::watch::Receiver<bool>,
) -> SessionOutcome
{
    let bound_jid = client.get_jid().to_string();
    let nick = bound_jid.split('@').next().unwrap_or("user");
    let events = forward_session_events(event_rx, bridge_tx, &bound_jid, established, generation);
    tokio::pin!(events);
    let result = run_while_ui_connected(bridge_tx, run_until_stopped(stop, drive_session_events(
        events.as_mut(), run_session_commands(&mut client, cmd_rx, bridge_tx, nick, generation),
    ))).await;
    let (outcome, events_finished) = match result {
        Some(Some(result)) => result,
        Some(None) => (SessionOutcome::LoggedOut, false),
        None => (SessionOutcome::UiGone, false),
    };

    // The command/probe future is gone before close consumes the client. An
    // unfinished forwarder must stay alive until close has released its sender.
    if events_finished {
        let _ = tokio::time::timeout(WRITE_TIMEOUT, client.close()).await;
    } else {
        finish_session_events(events.as_mut(), client.close()).await;
    }
    return outcome;
}

async fn run_until_stopped<T>(stop: &mut tokio::sync::watch::Receiver<bool>, work: impl std::future::Future<Output = T>) -> Option<T>
{
    tokio::select! {
        biased;
        _ = stop.wait_for(|stopped| *stopped) => None,
        result = work => Some(result),
    }
}

// Interrupt setup or in-flight work promptly when its subscription goes away.
// Callers keep established clients outside this future so cancellation can still
// run their bounded close handshake instead of merely dropping the TCP socket.
async fn run_while_ui_connected<T>(
    bridge_tx: &tokio::sync::mpsc::Sender<XmppEvent>,
    work: impl std::future::Future<Output = T>,
) -> Option<T>
{
    tokio::select! {
        biased;
        _ = bridge_tx.closed() => None,
        result = work => Some(result),
    }
}

pub fn connect(cmd: CommandChannel) -> impl iced::futures::Stream<Item = XmppEvent>
{
    stream::channel(100, async move |mut output|
    {
        let (cmd_rx, jid, password, mut stop) =
        {
            let mut inner = cmd.0.lock().unwrap();
            let rx = inner.rx.take();
            let jid = std::mem::take(&mut inner.jid);
            let password = std::mem::take(&mut inner.password);
            (rx, jid, password, inner.stop.subscribe())
        };

        if *stop.borrow() {
            let _ = output.send(XmppEvent::Stopped).await;
            return;
        }

        let mut cmd_rx = match cmd_rx
        {
            Some(rx) => rx,
            None =>
            {
                error!("Command channel receiver already consumed");
                let _ = output.send(XmppEvent::Disconnected("Internal error: command channel already consumed.".to_string())).await;
                let _ = output.send(XmppEvent::Stopped).await;
                return;
            }
        };

        // Bridge between iced's async executor and tokio: libxmpp requires a
        // tokio runtime for TCP, TLS, and spawned tasks.
        let (bridge_tx, bridge_rx) = tokio::sync::mpsc::channel::<XmppEvent>(100);

        std::thread::spawn(move ||
        {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");

            rt.block_on(async
            {
                // Outer reconnect loop. It owns cmd_rx and bridge_tx and keeps
                // re-establishing the underlying client without tearing down the
                // iced subscription, so the UI's rooms/chats/selection survive a
                // transient drop untouched.
                let mut backoff = INITIAL_BACKOFF;
                let mut established = false;
                let mut generation = 0u64;

                loop
                {
                    // Bound the setup path with a timeout. On expiry the in-flight
                    // future is dropped, cancelling the stalled connect, and we
                    // synthesize a transient error so the existing Err arm handles
                    // it (retry when reconnecting, drop to login on first connect).
                    let result = match run_while_ui_connected(&bridge_tx, run_until_stopped(&mut stop, tokio::time::timeout(
                        SETUP_TIMEOUT,
                        ::xmpp::XmppClient::new(&jid, &password),
                    ))).await.flatten()
                    {
                        None => return,
                        Some(Ok(result)) => result,
                        Some(Err(_)) => Err(::xmpp::XmppError::Timeout(
                            "connection setup timed out".to_string(),
                        )),
                    };

                    let result = match result {
                        // Setup's deadline ended when new() returned. This
                        // additional presence write needs its own deadline too.
                        Ok((client, events)) => match run_while_ui_connected(&bridge_tx, run_until_stopped(&mut stop,
                            with_io_timeout(WRITE_TIMEOUT, advertise_capabilities(&client)))).await.flatten() {
                            Some(Ok(())) => Ok((client, events)),
                            Some(Err(e)) => { close_unstarted_session(client, events, &bridge_tx, established, generation).await; Err(e) },
                            None => { close_unstarted_session(client, events, &bridge_tx, established, generation).await; return; },
                        },
                        Err(e) => Err(e),
                    };
                    match result
                    {
                        Ok((client, event_rx)) =>
                        {
                            backoff = INITIAL_BACKOFF;

                            if established { generation = generation.wrapping_add(1); }
                            let outcome = run_session(client, event_rx, &mut cmd_rx, &bridge_tx, established, generation, &mut stop).await;
                            established = true;

                            match outcome
                            {
                                // Logout or the UI going away: stop for good.
                                SessionOutcome::LoggedOut | SessionOutcome::UiGone => return,
                                // The session dropped: fall through to backoff and retry.
                                SessionOutcome::Dropped => {}
                            }
                        }
                        Err(e) =>
                        {
                            // The initial connect failing (auth or transient) keeps the
                            // old behavior of dropping to the login screen. Auth failures
                            // are never retried, even mid-session.
                            if !established || is_auth_failure(&e)
                            {
                                let _ = bridge_tx.send(XmppEvent::Disconnected(e.to_string())).await;
                                return;
                            }
                            // A transient connect failure while reconnecting: log the
                            // reason (so an endless retry loop isn't silent — e.g. an
                            // expired server TLS cert) and keep retrying with backoff.
                            error!("Reconnect attempt failed (retrying): {}", e);
                        }
                    }

                    if bridge_tx.send(XmppEvent::Reconnecting).await.is_err() { return; }

                    // Wait out the backoff, but retry immediately if the user
                    // hits "Reconnect now", or stop if the UI logs out (which
                    // drops the command sender and closes cmd_rx).
                    tokio::select!
                    {
                        _ = stop.wait_for(|stopped| *stopped) => return,
                        _ = bridge_tx.closed() => return,
                        _ = tokio::time::sleep(backoff) =>
                        {
                            backoff = (backoff * 2).min(MAX_BACKOFF);
                        }
                        wait = drain_until_signal(&mut cmd_rx) =>
                        {
                            match wait
                            {
                                BackoffWait::Closed => return,
                                BackoffWait::ForceReconnect => backoff = INITIAL_BACKOFF,
                            }
                        }
                    }
                }
            });
        });

        relay_worker_events(bridge_rx, &mut output).await;
    })
}

async fn relay_worker_events(mut events: tokio::sync::mpsc::Receiver<XmppEvent>,
    output: &mut iced::futures::channel::mpsc::Sender<XmppEvent>)
{
    while let Some(event) = events.recv().await {
        if output.send(event).await.is_err() { return; }
    }
    // FIFO: the UI sees this only after native close and every bridged event.
    let _ = output.send(XmppEvent::Stopped).await;
}

#[cfg(test)]
pub(crate) async fn forward_message_for_test(event: ::xmpp::XmppEvent) -> XmppEvent {
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    sender.try_send(event).unwrap();
    drop(sender);
    let (bridge, mut events) = tokio::sync::mpsc::channel(2);
    let _ = forward_events(receiver, &bridge, "me@example/device", false, 0, false).await;
    let event = events.try_recv().expect("one forwarded message");
    assert!(events.try_recv().is_err());
    event
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn suspend_detector_reports_sleep_but_not_awake_time_or_clock_changes()
    {
        let mono = std::time::Instant::now();
        let wall = std::time::SystemTime::now();
        let mut detector = SuspendDetector { monotonic: mono, wall };
        // Awake: both clocks advance together, with small jitter.
        assert_eq!(detector.check_at(mono + Duration::from_secs(2), wall + Duration::from_millis(2_300)), None);
        // The incident: monotonic time stood still for a 190 s sleep.
        let slept = detector.check_at(mono + Duration::from_secs(4), wall + Duration::from_millis(2_300) + Duration::from_secs(192));
        assert_eq!(slept, Some(Duration::from_secs(190)));
        assert!(slept.unwrap() >= SUSPEND_RECONNECT);
        // A short nap is reported for probing, not reconnecting.
        let wall = wall + Duration::from_millis(2_300) + Duration::from_secs(192);
        let nap = detector.check_at(mono + Duration::from_secs(6), wall + Duration::from_secs(12)).unwrap();
        assert!(nap >= SUSPEND_SLACK && nap < SUSPEND_RECONNECT);
        // A wall clock set backwards re-bases instead of reporting a suspend.
        let wall = wall + Duration::from_secs(12);
        assert_eq!(detector.check_at(mono + Duration::from_secs(8), wall - Duration::from_secs(3600)), None);
        assert_eq!(detector.check_at(mono + Duration::from_secs(10), wall - Duration::from_secs(3598)), None);
    }

    #[tokio::test]
    async fn stop_signal_cancels_work_even_when_requested_before_subscription() {
        for before in [false, true] {
            let (_tx, channel) = new_command_channel("me@example/device".into(), "test secret".into());
            if before { channel.request_stop(); }
            let mut stopped = channel.0.lock().unwrap().stop.subscribe();
            let work = run_until_stopped(&mut stopped, std::future::pending::<()>());
            let request = async { if !before { tokio::task::yield_now().await; channel.request_stop(); } };
            let (result, ()) = tokio::time::timeout(Duration::from_secs(1), async { tokio::join!(work, request) }).await.unwrap();
            assert!(result.is_none());
        }
    }

    #[tokio::test]
    async fn stopped_subscription_finishes_without_starting_connection_setup() {
        use iced::futures::StreamExt;
        let (_tx, channel) = new_command_channel("invalid jid that must never connect".into(), "test secret".into());
        channel.request_stop();
        let events = connect(channel);
        tokio::pin!(events);
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.next()).await.unwrap(), Some(XmppEvent::Stopped)));
        assert!(tokio::time::timeout(Duration::from_secs(1), events.next()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn completion_marker_follows_every_buffered_bridge_event() {
        use iced::futures::StreamExt;
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        for body in ["first", "last"] {
            tx.send(XmppEvent::DirectMessage { from: "alice@example".into(), body: body.into(), timestamp: chrono::Utc::now(),
                delayed: false, stanza_id: None, origin_id: None }).await.unwrap();
            if body == "first" { tx.send(XmppEvent::Disconnected("closing".into())).await.unwrap(); }
        }
        drop(tx);
        let (mut output, receiver) = iced::futures::channel::mpsc::channel(10);
        relay_worker_events(rx, &mut output).await;
        drop(output);
        let events = receiver.collect::<Vec<_>>().await;
        assert_eq!(events.len(), 4);
        assert!(matches!(&events[0], XmppEvent::DirectMessage { body, .. } if body == "first"));
        assert!(matches!(&events[1], XmppEvent::Disconnected(_)));
        assert!(matches!(&events[2], XmppEvent::DirectMessage { body, .. } if body == "last"));
        assert!(matches!(&events[3], XmppEvent::Stopped));
    }

    #[tokio::test]
    async fn stopping_before_activation_preserves_identity_and_messages_without_announcing_connection() {
        for established in [false, true] {
            let (sender, receiver) = tokio::sync::mpsc::channel(2);
            let (bridge, mut events) = tokio::sync::mpsc::channel(3);
            sender.send(::xmpp::XmppEvent::Connected).await.unwrap();
            sender.send(::xmpp::XmppEvent::DirectMessage { from: "alice@example".into(), body: "final hello".into(),
                delay: None, stanza_id: None, origin_id: None }).await.unwrap();
            drop(sender);
            let _ = forward_events(receiver, &bridge, "canonical@example/assigned", established, 7, false).await;
            assert!(matches!(events.try_recv(), Ok(XmppEvent::BoundJid(jid)) if jid == "canonical@example/assigned"));
            assert!(matches!(events.try_recv(), Ok(XmppEvent::DirectMessage { body, .. }) if body == "final hello"));
            assert!(events.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn room_service_forwarding_uses_a_nonoccupant_history_source() {
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        let (bridge, mut events) = tokio::sync::mpsc::channel(2);
        sender.send(::xmpp::XmppEvent::RoomEvent { room: "room@example".into(), context: "service-entry".into(), event: Box::new(::xmpp::XmppEvent::RoomServiceMessage {
            room: "room@example".into(), body: "service".into(), delay: Some(::xmpp::Delay { stamp: Some("2025-01-01T00:00:00Z".into()), from: None, reason: None }), stanza_id: Some("service-id".into()), origin_id: Some("origin".into()),
        }) }).await.unwrap();
        drop(sender);
        let _ = forward_session_events(receiver, &bridge, "me@example/device", false, 0).await;
        assert!(matches!(events.try_recv(), Ok(XmppEvent::RoomEvent { context, event, .. }) if context == "service-entry" && matches!(event.as_ref(),
            XmppEvent::RoomMessage { nick, body, delayed: true, stanza_id: Some(id), origin_id: Some(origin), timestamp, .. }
                if nick.is_empty() && body == "service" && id == "service-id" && origin == "origin" && timestamp.to_rfc3339() == "2025-01-01T00:00:00+00:00")));
    }

    #[tokio::test]
    async fn room_privacy_forwarding_preserves_operation_context() {
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        let (bridge, mut events) = tokio::sync::mpsc::channel(2);
        let privacy = ::xmpp::RoomPrivacy { publicly_logged: Some(true), non_anonymous: Some(true) };
        sender.send(::xmpp::XmppEvent::RoomEvent { room: "room@example".into(), context: "entry".into(), event: Box::new(::xmpp::XmppEvent::RoomPrivacy { room: "room@example".into(), privacy }) }).await.unwrap();
        drop(sender);
        let _ = forward_session_events(receiver, &bridge, "me@example/device", false, 0).await;
        assert!(matches!(events.try_recv(), Ok(XmppEvent::RoomEvent { room, context, event }) if room == "room@example" && context == "entry"
            && matches!(*event, XmppEvent::RoomPrivacy { privacy: actual, .. } if actual == privacy)));
    }

    #[tokio::test]
    async fn private_room_identity_survives_event_forwarding() {
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        let (bridge, mut events) = tokio::sync::mpsc::channel(2);
        sender.send(::xmpp::XmppEvent::RoomPrivateMessage { from: "room@example/Alice/phone".into(), body: "private".into(), delay: None, stanza_id: Some("server".into()), origin_id: Some("origin".into()) }).await.unwrap();
        drop(sender);
        let _ = forward_session_events(receiver, &bridge, "me@example/device", false, 0).await;
        assert!(matches!(events.try_recv(), Ok(XmppEvent::RoomPrivateMessage { from, body, origin_id: Some(origin), stanza_id: Some(id), .. })
            if from == "room@example/Alice/phone" && body == "private" && origin == "origin" && id == "server"));
    }

    #[tokio::test]
    async fn message_origins_survive_live_and_archive_event_forwarding() {
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        let (bridge, mut events) = tokio::sync::mpsc::channel(4);
        sender.send(::xmpp::XmppEvent::RoomMessage { room: "room@example".into(), nick: "me".into(), body: "room".into(), delay: None, stanza_id: None, origin_id: Some("room-origin".into()) }).await.unwrap();
        sender.send(::xmpp::XmppEvent::DirectMessage { from: "alice@example".into(), body: "direct".into(), delay: None, stanza_id: None, origin_id: Some("direct-origin".into()) }).await.unwrap();
        sender.send(::xmpp::XmppEvent::ArchiveMessage { to: Some("room@example/Alice".into()), query_id: "q".into(), from: "me@example".into(), body: "archive".into(), timestamp: Some("2025-01-01T12:00:00Z".into()), stanza_id: Some("server-id".into()), origin_id: Some("archive-origin".into()) }).await.unwrap();
        drop(sender);
        assert!(matches!(forward_session_events(receiver, &bridge, "me@example/device", false, 0).await, SessionOutcome::Dropped));
        assert!(matches!(events.try_recv(), Ok(XmppEvent::RoomMessage { origin_id: Some(origin), .. }) if origin == "room-origin"));
        assert!(matches!(events.try_recv(), Ok(XmppEvent::DirectMessage { origin_id: Some(origin), .. }) if origin == "direct-origin"));
        assert!(matches!(events.try_recv(), Ok(XmppEvent::ArchiveMessage { to: Some(to), origin_id: Some(origin), stanza_id: Some(id), .. }) if to == "room@example/Alice" && origin == "archive-origin" && id == "server-id"));
    }

    #[tokio::test]
    async fn archive_dates_are_required_and_invalid_dates_fail_the_page_instead_of_using_now() {
        for stamp in [None, Some(""), Some("invalid"), Some("2026-02-30T12:00:00Z"), Some("2026-01-01T12:00:00")] {
            let (sender, receiver) = tokio::sync::mpsc::channel(2);
            let (bridge, mut events) = tokio::sync::mpsc::channel(2);
            sender.send(::xmpp::XmppEvent::ArchiveMessage { to: None, query_id: "bad-page".into(), from: "alice@example".into(), body: "old text".into(), timestamp: stamp.map(str::to_string), stanza_id: Some("server-id".into()), origin_id: None }).await.unwrap();
            sender.send(::xmpp::XmppEvent::ArchiveComplete { query_id: "bad-page".into(), complete: true, first: Some("first".into()), last: Some("last".into()) }).await.unwrap();
            drop(sender);
            let _ = forward_session_events(receiver, &bridge, "me@example/device", false, 0).await;
            assert!(matches!(events.try_recv(), Ok(XmppEvent::ArchiveFailed { query_id, .. }) if query_id == "bad-page"));
            assert!(matches!(events.try_recv(), Ok(XmppEvent::ArchiveComplete { .. })));
            assert!(events.try_recv().is_err());
        }
        // Preserve valid historical time, including offset and fractional seconds.
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let (bridge, mut events) = tokio::sync::mpsc::channel(1);
        sender.send(::xmpp::XmppEvent::ArchiveMessage { to: None, query_id: "good-page".into(), from: "alice@example".into(), body: "old text".into(), timestamp: Some("2002-09-10T23:08:25.123+02:00".into()), stanza_id: None, origin_id: None }).await.unwrap();
        drop(sender);
        let _ = forward_session_events(receiver, &bridge, "me@example/device", false, 0).await;
        assert!(matches!(events.try_recv(), Ok(XmppEvent::ArchiveMessage { timestamp, .. }) if timestamp.to_rfc3339() == "2002-09-10T21:08:25.123+00:00"));
    }

    #[tokio::test]
    async fn bound_identity_precedes_lifecycle_and_incoming_data_on_every_session() {
        for established in [false, true] {
            let (sender, receiver) = tokio::sync::mpsc::channel(3);
            let (bridge, mut events) = tokio::sync::mpsc::channel(4);
            sender.send(::xmpp::XmppEvent::Connected).await.unwrap();
            sender.send(::xmpp::XmppEvent::DirectMessage { from: "alice@example".into(), body: "hello".into(), delay: None, stanza_id: None, origin_id: None }).await.unwrap();
            drop(sender);
            assert!(matches!(forward_session_events(receiver, &bridge, "canonical@example/assigned", established, 7).await, SessionOutcome::Dropped));
            assert!(matches!(events.try_recv(), Ok(XmppEvent::BoundJid(jid)) if jid == "canonical@example/assigned"));
            let lifecycle = events.try_recv().unwrap();
            if established { assert!(matches!(lifecycle, XmppEvent::Reconnected { generation: 7 })); }
            else { assert!(matches!(lifecycle, XmppEvent::Connected)); }
            assert!(matches!(events.try_recv(), Ok(XmppEvent::DirectMessage { body, .. }) if body == "hello"));
        }
    }

    #[tokio::test]
    async fn cancelled_room_commands_are_skipped_and_release_their_delivery_attempts() {
        let (bridge, mut events) = tokio::sync::mpsc::channel(8);
        let operation = RoomOperation::new();
        operation.cancel();
        assert!(current_room_command(XmppCommand::ScopedRoom { operation: operation.clone(), command: Box::new(XmppCommand::JoinRoom("room@example".into())) }, &bridge).await.is_none());
        let leave = XmppCommand::ScopedRoom { operation: operation.clone(), command: Box::new(XmppCommand::LeaveRoom { room: "room@example".into(), nick: "me".into() }) };
        assert!(leave.should_execute());
        let (context, command) = current_room_command(leave, &bridge).await.unwrap();
        assert_eq!(context.unwrap().id, operation.id);
        assert!(matches!(command, XmppCommand::LeaveRoom { .. }));
        assert!(events.try_recv().is_err());
        for id in [7, 0] {
            let command = XmppCommand::SendRoomMessage { id, origin_id: None, room: "room@example".into(), body: "recover text".into() };
            assert!(current_room_command(XmppCommand::ScopedRoom { operation: operation.clone(), command: Box::new(command) }, &bridge).await.is_none());
            if id > 0 { assert!(matches!(events.recv().await, Some(XmppEvent::SendResult { id: 7, ok: false }))); }
            else { assert!(matches!(events.recv().await, Some(XmppEvent::SendRejected { body, .. }) if body == "recover text")); }
        }
        let operation = RoomOperation::new();
        let id = operation.id.clone();
        let (context, command) = current_room_command(XmppCommand::ScopedRoom { operation, command: Box::new(XmppCommand::JoinRoom("room@example".into())) }, &bridge).await.unwrap();
        assert_eq!(context.unwrap().id, id);
        assert!(matches!(command, XmppCommand::JoinRoom(_)));
    }

    #[tokio::test]
    async fn forwarding_preserves_native_room_context_and_keeps_delivery_errors_independent() {
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        let (bridge, mut events) = tokio::sync::mpsc::channel(4);
        sender.send(::xmpp::XmppEvent::RoomEvent { room: "room@example".into(), context: "old-entry".into(), event: Box::new(::xmpp::XmppEvent::RoomLeft("room@example".into())) }).await.unwrap();
        sender.send(::xmpp::XmppEvent::MessageError { id: "origin".into(), from: Some("room@example".into()), error_type: "cancel".into(), condition: "forbidden".into(), text: None }).await.unwrap();
        drop(sender);
        assert!(matches!(forward_session_events(receiver, &bridge, "me@example/device", false, 0).await, SessionOutcome::Dropped));
        match events.recv().await {
            Some(XmppEvent::RoomEvent { room, context, event }) => {
                assert_eq!(room, "room@example");
                assert_eq!(context, "old-entry");
                assert!(matches!(*event, XmppEvent::RoomLeft(_)));
            }
            event => panic!("expected contextual room event, got {event:?}"),
        }
        assert!(matches!(events.recv().await, Some(XmppEvent::MessageError { origin_id, .. }) if origin_id == "origin"));
    }

    #[tokio::test]
    async fn local_send_rejection_is_recoverable_but_transport_failure_ends_the_session()
    {
        let (bridge, mut events) = tokio::sync::mpsc::channel(8);
        assert!(report_send_result(Err(::xmpp::XmppError::InvalidInput("unsupported characters".into())),
            1, "alice@example".into(), "recover\0me".into(), &bridge).await);
        assert!(matches!(events.recv().await, Some(XmppEvent::SendRejected { id: 1, body, .. }) if body == "recover\0me"));
        assert!(report_send_result(Ok(::xmpp::SendConfirmation::StreamAcknowledged),
            2, "alice@example".into(), "valid".into(), &bridge).await);
        assert!(matches!(events.recv().await, Some(XmppEvent::SendResult { id: 2, ok: true })));
        assert!(!report_send_result(Err(::xmpp::XmppError::Disconnected),
            3, "alice@example".into(), "retry".into(), &bridge).await);
        assert!(matches!(events.recv().await, Some(XmppEvent::SendResult { id: 3, ok: false })));
    }

    #[tokio::test]
    async fn closing_ui_cancels_in_flight_connection_work()
    {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Cancelled(Arc<AtomicBool>);
        impl Drop for Cancelled {
            fn drop(&mut self) { self.0.store(true, Ordering::SeqCst); }
        }
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let guard = Cancelled(cancelled.clone());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let work = async move {
            let _guard = guard;
            started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        };
        let close_ui = async move {
            started_rx.await.unwrap();
            drop(rx);
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(run_while_ui_connected(&tx, work), close_ui)
        }).await.expect("logout must cancel connection work without waiting for its I/O deadline");
        assert!(result.is_none());
        assert!(cancelled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn closed_ui_interrupts_an_idle_session_and_returns_control_for_cleanup()
    {
        let (_events, event_rx) = tokio::sync::mpsc::channel(1);
        let (bridge, ui) = tokio::sync::mpsc::channel(1);
        drop(ui);
        let events = forward_session_events(event_rx, &bridge, "me@example/device", false, 0);
        tokio::pin!(events);
        let outcome = run_while_ui_connected(&bridge,
            drive_session_events(events.as_mut(), std::future::pending())).await;
        assert!(outcome.is_none());
        // Completed work still returns its value to the owning cleanup scope.
        let (bridge, _ui) = tokio::sync::mpsc::channel(1);
        assert_eq!(run_while_ui_connected(&bridge, async { 42 }).await, Some(42));
    }

    #[tokio::test]
    async fn blocked_write_is_cancelled_at_its_deadline()
    {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Cancelled(Arc<AtomicBool>);
        impl Drop for Cancelled {
            fn drop(&mut self) { self.0.store(true, Ordering::SeqCst); }
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let guard = Cancelled(cancelled.clone());
        let operation = async move {
            let _guard = guard;
            std::future::pending::<Result<(), ::xmpp::XmppError>>().await
        };
        let result = with_io_timeout(Duration::from_millis(1), operation).await;
        assert!(matches!(result, Err(::xmpp::XmppError::Timeout(_))));
        assert!(cancelled.load(Ordering::SeqCst));
        assert!(with_io_timeout(Duration::from_secs(1), async { Ok(()) }).await.is_ok());
        assert!(matches!(with_io_timeout(Duration::from_secs(1), async {
            Err::<(), _>(::xmpp::XmppError::Disconnected)
        }).await, Err(::xmpp::XmppError::Disconnected)));
    }

    #[tokio::test]
    async fn new_session_rejects_queued_commands_until_its_own_ui_barrier()
    {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        for command in [
            XmppCommand::SessionReady(0),
            XmppCommand::SendDirectMessage { id: 1, origin_id: None, to: "a@example".into(), body: "stale".into() },
            XmppCommand::ForceReconnect,
            XmppCommand::SessionReady(1),
            XmppCommand::JoinRoom("stale-room@example".into()),
            XmppCommand::SessionReady(2),
            XmppCommand::JoinRoom("fresh-room@example".into()),
            XmppCommand::SendDirectMessage { id: 1, origin_id: None, to: "a@example".into(), body: "fresh retry".into() },
        ] {
            tx.send(command).await.unwrap();
        }
        drop(tx);
        let mut ready = false;
        assert!(matches!(next_session_command(&mut rx, &mut ready, 2).await,
            Some(XmppCommand::JoinRoom(room)) if room == "fresh-room@example"));
        assert!(ready);
        assert!(matches!(next_session_command(&mut rx, &mut ready, 2).await,
            Some(XmppCommand::SendDirectMessage { body, .. }) if body == "fresh retry"));
        assert!(next_session_command(&mut rx, &mut ready, 2).await.is_none());
    }

    #[tokio::test]
    async fn logout_before_ui_barrier_stops_waiting_session()
    {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.send(XmppCommand::SessionReady(0)).await.unwrap();
        drop(tx);
        assert!(next_session_command(&mut rx, &mut false, 1).await.is_none());
    }

    #[tokio::test]
    async fn command_completion_keeps_incoming_events_alive_until_native_close() {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(1);
        let (bridge_tx, mut bridge_rx) = tokio::sync::mpsc::channel(1);
        let events = forward_session_events(event_rx, &bridge_tx, "me@example/device", false, 0);
        tokio::pin!(events);
        let (outcome, finished) = drive_session_events(events.as_mut(), std::future::ready(SessionOutcome::LoggedOut)).await;
        assert!(matches!(outcome, SessionOutcome::LoggedOut));
        assert!(!finished);
        assert!(!event_tx.is_closed(), "command completion discarded the incoming queue before native close");
        finish_session_events(events.as_mut(), async {
            event_tx.send(::xmpp::XmppEvent::DirectMessage { from: "alice@example".into(), body: "final message during close".into(),
                delay: None, stanza_id: None, origin_id: None }).await.unwrap();
            drop(event_tx);
        }).await;
        assert!(matches!(bridge_rx.try_recv(), Ok(XmppEvent::DirectMessage { body, .. }) if body == "final message during close"));
    }

    #[tokio::test]
    async fn closing_preserves_a_message_already_waiting_for_bridge_capacity() {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(1);
        let (bridge_tx, mut bridge_rx) = tokio::sync::mpsc::channel(1);
        bridge_tx.send(XmppEvent::Reconnecting).await.unwrap();
        event_tx.send(::xmpp::XmppEvent::DirectMessage { from: "alice@example".into(), body: "backpressured message".into(),
            delay: None, stanza_id: None, origin_id: None }).await.unwrap();
        drop(event_tx);
        let events = forward_session_events(event_rx, &bridge_tx, "me@example/device", false, 0);
        tokio::pin!(events);
        assert!(iced::futures::poll!(events.as_mut()).is_pending());
        let (outcome, finished) = drive_session_events(events.as_mut(), std::future::ready(SessionOutcome::Dropped)).await;
        assert!(matches!(outcome, SessionOutcome::Dropped));
        assert!(!finished);
        finish_session_events(events.as_mut(), async {
            assert!(matches!(bridge_rx.recv().await, Some(XmppEvent::Reconnecting)));
        }).await;
        assert!(matches!(bridge_rx.try_recv(), Ok(XmppEvent::DirectMessage { body, .. }) if body == "backpressured message"));
    }

    #[tokio::test]
    async fn pending_probe_does_not_stop_bounded_event_queue_from_draining()
    {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(32);
        let (bridge_tx, mut bridge_rx) = tokio::sync::mpsc::channel(100);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let reader_tx = event_tx.clone();
        let reader = tokio::spawn(async move {
            // The reader must enqueue these messages before it can see the
            // probe response that follows them on the same TCP stream.
            for i in 0..64 {
                reader_tx.send(::xmpp::XmppEvent::DirectMessage {
                    from: "alice@example/resource".into(), body: i.to_string(),
                    delay: None, stanza_id: None,
                    origin_id: None,
                }).await.unwrap();
            }
            reply_tx.send(()).unwrap();
        });
        let probe = async {
            reply_rx.await.unwrap();
            SessionOutcome::LoggedOut
        };
        let events = forward_session_events(event_rx, &bridge_tx, "me@example/device", false, 0);
        tokio::pin!(events);
        let (outcome, finished) = tokio::time::timeout(Duration::from_secs(1), drive_session_events(events.as_mut(), probe)).await
            .expect("event backpressure must not prevent the reader reaching a probe reply");
        assert!(matches!(outcome, SessionOutcome::LoggedOut));
        assert!(!finished);
        reader.await.unwrap();
        drop(event_tx);
        finish_session_events(events.as_mut(), async {}).await;
        assert_eq!(bridge_rx.len(), 64);
        let mut expected = 0;
        while let Ok(XmppEvent::DirectMessage { body, .. }) = bridge_rx.try_recv() {
            assert_eq!(body, expected.to_string());
            expected += 1;
        }
    }

    #[tokio::test]
    async fn event_eof_cancels_pending_session_work()
    {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(1);
        let (bridge_tx, _bridge_rx) = tokio::sync::mpsc::channel(1);
        drop(event_tx);
        let events = forward_session_events(event_rx, &bridge_tx, "me@example/device", false, 0);
        tokio::pin!(events);
        let (outcome, finished) = drive_session_events(events.as_mut(), std::future::pending()).await;
        assert!(matches!(outcome, SessionOutcome::Dropped));
        assert!(finished);
    }

    #[tokio::test]
    async fn closed_ui_cancels_pending_session_work()
    {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(1);
        let (bridge_tx, bridge_rx) = tokio::sync::mpsc::channel(1);
        drop(bridge_rx);
        event_tx.send(::xmpp::XmppEvent::Connected).await.unwrap();
        let events = forward_session_events(event_rx, &bridge_tx, "me@example/device", false, 0);
        tokio::pin!(events);
        let (outcome, finished) = drive_session_events(events.as_mut(), std::future::pending()).await;
        assert!(matches!(outcome, SessionOutcome::UiGone));
        assert!(finished);
    }
}
