//! `libxmpp` is a small async XMPP client library built on Tokio.
//!
//! It currently supports connecting to an XMPP server with STARTTLS and
//! SASL authentication, joining and leaving Multi-User Chat (MUC) rooms,
//! sending and receiving group messages, exchanging one-to-one chat
//! messages, sending XEP-0199 pings to keep the connection alive or
//! probe whether it is still usable (and answering pings from others),
//! performing XEP-0030 service discovery to learn the identities,
//! features, and hosted items of remote entities as well as answering
//! such queries from others, negotiating XEP-0198 stream management
//! to acknowledge stanzas in both directions (answering the server's
//! acknowledgement requests and confirming delivery of sent stanzas via
//! [`XmppClient::request_ack`]), and advertising XEP-0115 entity
//! capabilities in outgoing presence while surfacing peers' capabilities
//! as [`XmppEvent::EntityCapabilities`] events, and recognising XEP-0203
//! delayed-delivery information on incoming messages (surfaced as the
//! `delay` field of [`XmppEvent::RoomMessage`] and
//! [`XmppEvent::DirectMessage`]). Server events are delivered as
//! [`XmppEvent`] values on an [`mpsc::Receiver`] returned from
//! [`XmppClient::new`].
//!
//! # Example
//!
//! ```no_run
//! use xmpp::{XmppClient, XmppEvent};
//!
//! # async fn run() -> Result<(), xmpp::XmppError> {
//! let (mut client, mut events) =
//!     XmppClient::new("user@example.com", "password").await?;
//!
//! client.join_room("room@conference.example.com", "my-nick").await?;
//!
//! while let Some(event) = events.recv().await
//! {
//!     match event
//!     {
//!         XmppEvent::RoomMessage { room, nick, body, .. } =>
//!         {
//!             println!("[{}] {}: {}", room, nick, body);
//!         }
//!         XmppEvent::DirectMessage { from, body, .. } =>
//!         {
//!             println!("{} -> {}", from, body);
//!         }
//!         _ => {}
//!     }
//! }
//!
//! client.close().await;
//! # Ok(())
//! # }
//! ```

use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use std::sync::{ Arc, Mutex };
use std::sync::atomic::{ AtomicBool, AtomicU32, Ordering };
use std::collections::{ HashMap, HashSet };
use std::time::Duration;

mod error;
mod tcp_stream;
mod xml_framer;
mod stanza;
mod xmpp;

pub use error::XmppError;
use stanza::Stanza;
use xml_framer::XmlFramer;

/// Registry of in-flight IQ requests awaiting a reply, keyed by stanza `id`.
///
/// A caller (e.g. [`XmppClient::ping`]) inserts a [`oneshot::Sender`] before
/// sending its request; the background reader task removes and fires it,
/// handing over the raw reply stanza XML, when the matching
/// `<iq type='result'>` or `<iq type='error'>` arrives. Callers that only need
/// to know a reply came (e.g. ping) ignore the payload; those that need the
/// response body (e.g. service discovery) parse it. The lock is only ever held
/// for the brief, non-async insert/remove, so a [`std::sync::Mutex`] is
/// sufficient.
pub(crate) type PendingIqs = Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>;

/// An event emitted by an [`XmppClient`] over its event channel.
///
/// Events cover the lifecycle of the connection itself, MUC room
/// membership and messages, and one-to-one chat messages. Lifecycle
/// events ([`Connecting`](Self::Connecting), [`EstablishingTls`](Self::EstablishingTls),
/// [`Authenticating`](Self::Authenticating), [`Connected`](Self::Connected))
/// are emitted in order during [`XmppClient::new`] and are intended for
/// progress reporting.
#[derive(Debug, Clone)]
pub enum XmppEvent
{
    /// The TCP connection to the server is being established.
    Connecting,
    /// STARTTLS negotiation is in progress.
    EstablishingTls,
    /// SASL authentication is in progress.
    Authenticating,
    /// The XMPP session is fully established and resource-bound.
    Connected,
    /// A MUC room was successfully joined. `members` is the initial
    /// roster delivered by the server when joining.
    RoomJoined { room: String, members: Vec<RoomMember> },
    /// The local user left the named MUC room.
    RoomLeft(String),
    /// Another occupant joined a MUC room the local user is in.
    MemberJoined { room: String, member: RoomMember },
    /// An occupant left a MUC room the local user is in.
    MemberLeft { room: String, nick: String },
    /// A group chat message was received from `nick` in `room`.
    /// `delay` is set when the message carries XEP-0203 delayed-delivery
    /// information (e.g. discussion history replayed on join).
    RoomMessage { room: String, nick: String, body: String, delay: Option<Delay> },
    /// A MUC room's subject was set or changed.
    RoomSubject { room: String, subject: String },
    /// A presence stanza of type `error` was received. `error_type` is
    /// the XMPP error class (e.g. `auth`, `cancel`), `condition` is the
    /// defined condition (e.g. `forbidden`, `not-authorized`), and
    /// `text` is the optional human-readable description.
    PresenceError { from: String, error_type: String, condition: String, text: Option<String> },
    /// A one-to-one chat message was received from `from`. `delay` is
    /// set when the message carries XEP-0203 delayed-delivery information
    /// (e.g. a message stored while this client was offline).
    DirectMessage { from: String, body: String, delay: Option<Delay> },
    /// A presence stanza from `from` advertised XEP-0115 entity capabilities.
    /// `node` and `ver` identify the peer's disco node (`node#ver`), which can
    /// be resolved to its feature set with [`XmppClient::disco_info`]; `hash`
    /// names the algorithm used to build `ver` (e.g. `sha-1`), absent in the
    /// legacy pre-1.4 caps format.
    EntityCapabilities { from: String, node: String, ver: String, hash: Option<String> },
}

/// [XEP-0203](https://xmpp.org/extensions/xep-0203.html) delayed-delivery
/// information attached to a message that was stored and delivered later than it
/// was originally sent — for example MUC discussion history replayed on join, or
/// a one-to-one message stored while the recipient was offline.
///
/// A message carrying no `<delay/>` element produces no `Delay`, so its presence
/// on an event is itself the signal that the message is not "live".
#[derive(Debug, Clone, PartialEq)]
pub struct Delay
{
    /// The time the message was originally sent, formatted per XEP-0082 (e.g.
    /// `2025-01-01T12:00:00Z`). Required by the specification, but optional here
    /// so a malformed `<delay/>` still yields the rest of its fields.
    pub stamp: Option<String>,
    /// The JID of the entity that applied the delay (the MUC or the sending
    /// user's server), when the server includes it.
    pub from: Option<String>,
    /// An optional human-readable description of the reason for the delay.
    pub reason: Option<String>,
}

impl From<stanza::delay::Delay> for Delay
{
    fn from(delay: stanza::delay::Delay) -> Self
    {
        // Treat an empty reason (e.g. `<delay .../>` with no text) as absent so
        // callers do not have to distinguish `Some("")` from `None`.
        let reason = delay.reason.filter(|r| !r.is_empty());
        return Delay { stamp: delay.stamp, from: delay.from, reason };
    }
}

/// A participant in a Multi-User Chat room.
///
/// The `affiliation` and `role` fields use the values defined by
/// [XEP-0045](https://xmpp.org/extensions/xep-0045.html) (e.g.
/// affiliation `owner`/`admin`/`member`/`none`, role
/// `moderator`/`participant`/`visitor`/`none`).
#[derive(Debug, Clone)]
pub struct RoomMember
{
    /// The participant's real JID, when the room is non-anonymous and
    /// the local user is permitted to see it.
    pub jid: Option<String>,
    /// The participant's in-room nickname.
    pub nick: String,
    /// MUC affiliation (e.g. `owner`, `admin`, `member`, `none`).
    pub affiliation: String,
    /// MUC role (e.g. `moderator`, `participant`, `visitor`, `none`).
    pub role: String,
    /// Optional presence `show` value (`away`, `chat`, `dnd`, `xa`).
    pub show: Option<String>,
    /// Optional human-readable presence status text.
    pub status: Option<String>,
}

/// An identity advertised by an entity in reply to a service discovery info
/// query, as defined by [XEP-0030](https://xmpp.org/extensions/xep-0030.html).
///
/// For example, a conference service typically reports category `conference`
/// with type `text`; a server reports category `server` with type `im`.
#[derive(Debug, Clone)]
pub struct DiscoIdentity
{
    /// The identity category (e.g. `server`, `conference`, `client`).
    pub category: String,
    /// The identity type within the category.
    pub kind: String,
    /// An optional human-readable name for the identity.
    pub name: Option<String>,
}

/// The result of a service discovery info query: the identities and feature
/// namespaces an entity advertises.
#[derive(Debug, Clone)]
pub struct DiscoInfo
{
    /// The identities reported by the entity.
    pub identities: Vec<DiscoIdentity>,
    /// The feature namespaces (`var` values) the entity supports, e.g.
    /// `urn:xmpp:ping` or `http://jabber.org/protocol/muc`.
    pub features: Vec<String>,
}

impl DiscoInfo
{
    /// Returns `true` if the entity advertised the given feature namespace.
    pub fn has_feature(&self, var: &str) -> bool
    {
        return self.features.iter().any(|f| f == var);
    }
}

/// An item hosted by an entity, as returned by a service discovery items
/// query (e.g. a chat room on a conference service).
#[derive(Debug, Clone)]
pub struct DiscoItem
{
    /// The JID of the item.
    pub jid: String,
    /// An optional human-readable name for the item.
    pub name: Option<String>,
    /// An optional node, used when the item is a node of `jid` rather than a
    /// distinct entity.
    pub node: Option<String>,
}

/// Build the error returned when a service discovery query is answered with an
/// IQ error (or any reply lacking the expected `<query>` payload).
fn iq_error(query: &str, iq_type: &str) -> XmppError
{
    return XmppError::Protocol(format!("{} query returned iq type '{}'", query, iq_type));
}

/// Build the available-presence stanza advertising the current XEP-0115 entity
/// capabilities: the configured caps `node` paired with the `ver` hash of the
/// disco info the client currently advertises. Recomputed on demand so it always
/// reflects the latest `set_disco_*`/`set_caps_node` configuration.
fn build_caps_presence(disco: &LocalDiscoState) -> stanza::caps::CapsPresence
{
    let disco = disco.lock().unwrap();
    let ver = stanza::caps::compute_ver(&disco.info);
    return stanza::caps::CapsPresence::new(disco.caps_node.clone(), ver);
}

/// The identities, features, and items this client advertises when answering
/// incoming XEP-0030 service discovery queries from other entities.
///
/// Held behind a [`Mutex`] so it can be reconfigured at runtime (e.g. with
/// [`XmppClient::set_disco_info`]) while the background reader task reads it to
/// build replies.
#[derive(Debug, Clone)]
pub(crate) struct LocalDisco
{
    pub(crate) info: DiscoInfo,
    pub(crate) items: Vec<DiscoItem>,
    /// The URI identifying this client's software, advertised as the `node` of
    /// the XEP-0115 entity-capabilities `<c/>` element. The `ver` half of the
    /// caps node is derived from `info`.
    pub(crate) caps_node: String,
}

/// Default XEP-0115 caps node: the library's project homepage, identifying the
/// software behind the advertised capabilities.
pub(crate) const DEFAULT_CAPS_NODE: &str = "https://github.com/mikaelsahlstrom/libxmpp";

impl Default for LocalDisco
{
    fn default() -> Self
    {
        // Advertise the one identity and the features we actually answer here:
        // the two service-discovery namespaces, XEP-0199 ping and XEP-0115
        // entity capabilities. Callers can replace these via the `set_disco_*`
        // methods on [`XmppClient`].
        return LocalDisco
        {
            info: DiscoInfo
            {
                identities: vec![DiscoIdentity
                {
                    category: "client".to_string(),
                    // Default kind to bot. Should be set to a better value
                    // by the calling client by setting a new [`DiscoInfo`].
                    kind: "bot".to_string(),
                    name: Some("libxmpp".to_string()),
                }],
                features: vec![
                    "http://jabber.org/protocol/caps".to_string(),
                    "http://jabber.org/protocol/disco#info".to_string(),
                    "http://jabber.org/protocol/disco#items".to_string(),
                    "urn:xmpp:ping".to_string(),
                ],
            },
            items: Vec::new(),
            caps_node: DEFAULT_CAPS_NODE.to_string(),
        };
    }
}

/// Shared handle to the [`LocalDisco`] advertised by a client. Cloned into the
/// background reader task so it can answer incoming queries.
pub(crate) type LocalDiscoState = Arc<Mutex<LocalDisco>>;

/// Shared handle to the connection's write half. Both the public client methods
/// and the background reader task (when auto-replying to incoming IQs) send
/// through it, so it is guarded by an async [`tokio::sync::Mutex`].
pub(crate) type SharedWriter = Arc<tokio::sync::Mutex<tcp_stream::TcpWriter>>;

/// Shared [XEP-0198](https://xmpp.org/extensions/xep-0198.html) stream-management
/// state.
///
/// Created disabled and flipped on by [`enable`](Self::enable) once the server
/// confirms with `<enabled/>`. Once enabled, the reader task counts every
/// inbound stanza it handles ([`record_inbound`](Self::record_inbound)) so it
/// can answer the server's `<r/>` acknowledgement requests, and every outbound
/// stanza is counted ([`record_outbound`](Self::record_outbound)) so a caller
/// can tell how many of its stanzas the server has acknowledged in an incoming
/// `<a h='N'/>`. All counters are 32-bit and wrap at 2^32, matching the `h`
/// value defined by the XEP.
///
/// The counters are plain atomics rather than a lock because each update is a
/// single fetch-add or store; `acked` is notified whenever a server ack lands so
/// [`XmppClient::request_ack`] can wake.
pub(crate) struct SmState
{
    enabled: AtomicBool,
    inbound: AtomicU32,
    outbound: AtomicU32,
    last_ack: AtomicU32,
    /// Notified on every incoming `<a/>`, waking any [`XmppClient::request_ack`]
    /// awaiting confirmation.
    pub(crate) acked: Notify,
}

impl SmState
{
    pub(crate) fn new() -> Self
    {
        return Self
        {
            enabled: AtomicBool::new(false),
            inbound: AtomicU32::new(0),
            outbound: AtomicU32::new(0),
            last_ack: AtomicU32::new(0),
            acked: Notify::new(),
        };
    }

    pub(crate) fn enable(&self)
    {
        self.enabled.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_enabled(&self) -> bool
    {
        return self.enabled.load(Ordering::SeqCst);
    }

    /// Count one handled inbound stanza.
    pub(crate) fn record_inbound(&self)
    {
        self.inbound.fetch_add(1, Ordering::SeqCst);
    }

    /// The number of inbound stanzas handled so far, as reported in our `<a/>`.
    pub(crate) fn inbound_count(&self) -> u32
    {
        return self.inbound.load(Ordering::SeqCst);
    }

    /// Count one outbound stanza put on the wire.
    pub(crate) fn record_outbound(&self)
    {
        self.outbound.fetch_add(1, Ordering::SeqCst);
    }

    /// The number of outbound stanzas we have sent so far.
    pub(crate) fn outbound_count(&self) -> u32
    {
        return self.outbound.load(Ordering::SeqCst);
    }

    /// Record the `h` from an incoming `<a/>` and wake any ack waiter.
    pub(crate) fn record_ack(&self, h: u32)
    {
        self.last_ack.store(h, Ordering::SeqCst);
        self.acked.notify_waiters();
    }

    /// The highest stanza count the server has acknowledged.
    pub(crate) fn last_ack(&self) -> u32
    {
        return self.last_ack.load(Ordering::SeqCst);
    }
}

/// Shared handle to the connection's [`SmState`]. Cloned into the reader task so
/// it can count inbound stanzas and answer `<r/>` acknowledgement requests.
pub(crate) type SharedSmState = Arc<SmState>;

/// Returns `true` once `acked` (the count the server has acknowledged) has
/// reached `target` (the outbound count at the time `<r/>` was sent), accounting
/// for the 32-bit wraparound of the stream-management counter.
fn ack_reached(acked: u32, target: u32) -> bool
{
    // Treat `acked` as having reached `target` when it is equal to or ahead of
    // it in modular arithmetic: the forward distance from `target` to `acked` is
    // in the near half of the 32-bit space.
    return acked.wrapping_sub(target) <= u32::MAX / 2;
}

/// An asynchronous XMPP client.
///
/// An `XmppClient` owns the TCP/TLS connection, the background reader
/// task that parses incoming stanzas, and the writer used to send
/// outgoing stanzas. Construct one with [`XmppClient::new`], drive it
/// by reading from the [`mpsc::Receiver`] it returns, and shut it down
/// cleanly with [`XmppClient::close`].
pub struct XmppClient
{
    shutdown: Arc<Notify>,
    task: JoinHandle<()>,
    writer: SharedWriter,
    bound_jid: String,
    pending_iqs: PendingIqs,
    next_iq_id: u64,
    local_disco: LocalDiscoState,
    sm_state: SharedSmState,
}

impl XmppClient
{
    /// Connect to an XMPP server and authenticate.
    ///
    /// `jid` is the bare or full JID of the account (e.g.
    /// `user@example.com`); the domain part is used to locate the
    /// server. `password` is the account password used during SASL
    /// authentication.
    ///
    /// On success this returns the connected client together with a
    /// receiver of [`XmppEvent`]s. The receiver must be drained for the
    /// client to make progress; if it is dropped, server events will
    /// be discarded. The lifecycle events
    /// [`Connecting`](XmppEvent::Connecting),
    /// [`EstablishingTls`](XmppEvent::EstablishingTls),
    /// [`Authenticating`](XmppEvent::Authenticating) and
    /// [`Connected`](XmppEvent::Connected) are sent on the channel as
    /// the connection progresses.
    ///
    /// Returns `Err` with a human-readable description if the TCP
    /// connection, TLS upgrade, SASL authentication, or resource
    /// binding fails.
    pub async fn new(jid: &str, password: &str) -> Result<(Self, mpsc::Receiver<XmppEvent>), XmppError>
    {
        let (event_tx, event_rx) = mpsc::channel(32);

        // Setup TCP connection, TLS, SASL authentication, and (if offered)
        // XEP-0198 stream management.
        let (bound_jid, tcp, sm_enabled) = xmpp::setup_connection(&event_tx, jid, password).await?;

        // We now have a working xmpp connection, split and spawn reader loop.
        let (mut reader, mut writer) = tcp.split()?;

        // Once management is on, both sides count stanzas from this point;
        // enable before sending anything so the very first stanza is counted.
        let sm_state: SharedSmState = Arc::new(SmState::new());
        if sm_enabled
        {
            sm_state.enable();
        }

        let local_disco: LocalDiscoState = Arc::new(Mutex::new(LocalDisco::default()));

        // Broadcast initial available presence so the server routes incoming
        // directed messages (e.g. one-to-one chats) to this resource. The
        // presence advertises this client's XEP-0115 entity capabilities so
        // peers can cache its disco info by `node`/`ver` instead of querying it.
        writer.write(&build_caps_presence(&local_disco).as_bytes()).await?;
        if sm_state.is_enabled()
        {
            sm_state.record_outbound();
        }

        // Share the write half with the reader task so it can auto-reply to
        // incoming IQ queries (e.g. service discovery) without round-tripping
        // through the owning client.
        let writer: SharedWriter = Arc::new(tokio::sync::Mutex::new(writer));
        let writer_loop = writer.clone();

        let shutdown = Arc::new(Notify::new());
        let shutdown_clone = shutdown.clone();
        let event_tx_loop = event_tx.clone();

        let pending_iqs: PendingIqs = Arc::new(Mutex::new(HashMap::new()));
        let pending_iqs_loop = pending_iqs.clone();

        let local_disco_loop = local_disco.clone();

        let sm_state_loop = sm_state.clone();

        let task = tokio::spawn(async move
        {
            let mut framer = XmlFramer::new_opened();
            let mut pending_joins: HashMap<String, Vec<RoomMember>> = HashMap::new();
            let mut pending_messages: HashMap<String, Vec<XmppEvent>> = HashMap::new();
            let mut joined_rooms: HashSet<String> = HashSet::new();

            loop
            {
                // Parse all data on the framer and process stanzas.
                while let Some(stanza_xml) = framer.try_next()
                {
                    log::debug!("Received stanza: {}", stanza_xml);
                    xmpp::process_stanza(&stanza_xml, &event_tx_loop, &mut pending_joins, &mut pending_messages, &mut joined_rooms, &pending_iqs_loop, &writer_loop, &local_disco_loop, &sm_state_loop).await;
                }

                // Collect data.
                tokio::select!
                {
                    result = reader.read() =>
                    {
                        match result
                        {
                            Ok(data) =>
                            {
                                framer.feed(&data);
                            }
                            Err(e) =>
                            {
                                log::error!("Read error: {}", e);
                                break;
                            }
                        }
                    }
                    _ = shutdown_clone.notified() =>
                    {
                        break;
                    }
                }
            }
        });

        let _ = event_tx.send(XmppEvent::Connected).await;

        return Ok((Self { shutdown, task, writer, bound_jid, pending_iqs, next_iq_id: 0, local_disco, sm_state }, event_rx));
    }

    /// Return the full JID assigned to this session by the server,
    /// including the resource bound during connection.
    pub fn get_jid(&self) -> &str
    {
        return &self.bound_jid;
    }

    /// Join a MUC room.
    ///
    /// `room_jid` is the bare JID of the room (e.g.
    /// `room@conference.example.com`) and `nick` is the nickname to
    /// use inside the room. When the server accepts the join, a
    /// [`RoomJoined`](XmppEvent::RoomJoined) event is delivered with
    /// the initial roster.
    pub async fn join_room(&mut self, room_jid: &str, nick: &str) -> Result<(), XmppError>
    {
        let presence = stanza::muc::MucJoinPresence::new(room_jid.to_string(), nick.to_string());
        return self.send_stanza(&presence.as_bytes()).await;
    }

    /// Leave a MUC room previously joined as `nick`.
    ///
    /// On success a [`RoomLeft`](XmppEvent::RoomLeft) event is emitted
    /// once the server confirms the departure.
    pub async fn leave_room(&mut self, room_jid: &str, nick: &str) -> Result<(), XmppError>
    {
        let presence = stanza::muc::MucLeavePresence::new(room_jid.to_string(), nick.to_string());
        return self.send_stanza(&presence.as_bytes()).await;
    }

    /// Send a group chat message to a MUC room.
    ///
    /// `room_jid` is the bare JID of the room. The local user must
    /// already have joined the room.
    pub async fn send_room_message(&mut self, room_jid: &str, body: &str) -> Result<(), XmppError>
    {
        let msg = stanza::muc::MucGroupMessage::new(room_jid.to_string(), body.to_string());
        return self.send_stanza(&msg.as_bytes()).await;
    }

    /// Send a one-to-one chat message.
    ///
    /// `to` is the recipient's bare or full JID. Replies will arrive
    /// as [`DirectMessage`](XmppEvent::DirectMessage) events.
    pub async fn send_message(&mut self, to: &str, body: &str) -> Result<(), XmppError>
    {
        let msg = stanza::chat::ChatMessage::new(to.to_string(), body.to_string());
        return self.send_stanza(&msg.as_bytes()).await;
    }

    /// Send an [XEP-0199](https://xmpp.org/extensions/xep-0199.html) ping and
    /// wait for the reply, returning the round-trip time.
    ///
    /// When `to` is `None` the ping targets the user's own server, which is the
    /// usual choice for a keep-alive (defeating NAT/proxy idle timeouts) or for
    /// probing whether the connection is still usable. Pass `Some(jid)` to ping
    /// another entity instead.
    ///
    /// Any reply resolves the call successfully, including an error such as
    /// `service-unavailable`: a reply of any kind proves the peer is reachable.
    /// Returns [`XmppError::Timeout`] if no reply arrives within `timeout`, or
    /// [`XmppError::Disconnected`] if the connection drops while waiting.
    pub async fn ping(&mut self, to: Option<&str>, timeout: Duration) -> Result<Duration, XmppError>
    {
        let id = self.next_iq_id("ping");
        let request = stanza::ping::PingRequest::new(id.clone(), to.map(|s| s.to_string()));

        let start = std::time::Instant::now();
        // Any reply (result or error) proves the peer is reachable; the body is
        // irrelevant for a ping, so the reply XML is discarded.
        self.request_iq(&id, &request, timeout).await?;
        return Ok(start.elapsed());
    }

    /// Send an [XEP-0030](https://xmpp.org/extensions/xep-0030.html) service
    /// discovery **info** query and wait for the reply.
    ///
    /// The reply describes the target entity: the [`DiscoIdentity`] values it
    /// advertises and the feature namespaces it supports. When `to` is `None`
    /// the query targets the user's own server. Pass `Some(node)` to scope the
    /// query to a particular node of the entity (e.g. an ad-hoc command);
    /// otherwise pass `None`.
    ///
    /// Returns [`XmppError::Protocol`] if the entity answers with an IQ error
    /// (e.g. it does not support service discovery), [`XmppError::Timeout`] if
    /// no reply arrives within `timeout`, or [`XmppError::Disconnected`] if the
    /// connection drops while waiting.
    pub async fn disco_info(&mut self, to: Option<&str>, node: Option<&str>, timeout: Duration) -> Result<DiscoInfo, XmppError>
    {
        let id = self.next_iq_id("disco");
        let request = stanza::disco::DiscoInfoRequest::new(
            id.clone(),
            to.map(|s| s.to_string()),
            node.map(|s| s.to_string()),
        );

        let reply = self.request_iq(&id, &request, timeout).await?;
        let result = stanza::disco::DiscoInfoResult::from_xml(&reply).map_err(XmppError::Parse)?;

        // An error reply commonly echoes an empty `<query/>`, so detect failure
        // from the iq type rather than the presence of the query payload.
        if result.iq_type == "error"
        {
            return Err(iq_error("disco#info", &result.iq_type));
        }
        let query = result.query.ok_or_else(|| iq_error("disco#info", &result.iq_type))?;

        let identities = query.identities.into_iter().map(|i| DiscoIdentity
        {
            category: i.category,
            kind: i.kind,
            name: i.name,
        }).collect();
        let features = query.features.into_iter().map(|f| f.var).collect();

        return Ok(DiscoInfo { identities, features });
    }

    /// Send an [XEP-0030](https://xmpp.org/extensions/xep-0030.html) service
    /// discovery **items** query and wait for the reply.
    ///
    /// The reply lists the [`DiscoItem`]s hosted by the target entity, such as
    /// the rooms on a conference service. When `to` is `None` the query targets
    /// the user's own server. Pass `Some(node)` to scope the query to a
    /// particular node; otherwise pass `None`.
    ///
    /// Returns [`XmppError::Protocol`] if the entity answers with an IQ error,
    /// [`XmppError::Timeout`] if no reply arrives within `timeout`, or
    /// [`XmppError::Disconnected`] if the connection drops while waiting.
    pub async fn disco_items(&mut self, to: Option<&str>, node: Option<&str>, timeout: Duration) -> Result<Vec<DiscoItem>, XmppError>
    {
        let id = self.next_iq_id("disco");
        let request = stanza::disco::DiscoItemsRequest::new(
            id.clone(),
            to.map(|s| s.to_string()),
            node.map(|s| s.to_string()),
        );

        let reply = self.request_iq(&id, &request, timeout).await?;
        let result = stanza::disco::DiscoItemsResult::from_xml(&reply).map_err(XmppError::Parse)?;

        // An error reply commonly echoes an empty `<query/>`, so detect failure
        // from the iq type rather than the presence of the query payload.
        if result.iq_type == "error"
        {
            return Err(iq_error("disco#items", &result.iq_type));
        }
        let query = result.query.ok_or_else(|| iq_error("disco#items", &result.iq_type))?;

        let items = query.items.into_iter().map(|i| DiscoItem
        {
            jid: i.jid,
            name: i.name,
            node: i.node,
        }).collect();

        return Ok(items);
    }

    /// Whether [XEP-0198](https://xmpp.org/extensions/xep-0198.html) stream
    /// management was negotiated for this session.
    ///
    /// Stream management is requested automatically during connection whenever
    /// the server advertises it. When enabled, this client answers the server's
    /// acknowledgement requests and [`request_ack`](Self::request_ack) can be
    /// used to confirm delivery of sent stanzas.
    pub fn is_stream_management_enabled(&self) -> bool
    {
        return self.sm_state.is_enabled();
    }

    /// Request an [XEP-0198](https://xmpp.org/extensions/xep-0198.html) stream
    /// acknowledgement and wait until the server confirms it has handled every
    /// stanza sent so far.
    ///
    /// This sends `<r/>` and waits for the matching `<a h='N'/>`. Because stream
    /// management counts stanzas in order, a returned `Ok(())` means the server
    /// has received and processed all stanzas written before this call — a
    /// stronger delivery guarantee than a [`ping`](Self::ping), which only
    /// proves the peer is reachable.
    ///
    /// Returns [`XmppError::Protocol`] if stream management was not negotiated
    /// (check [`is_stream_management_enabled`](Self::is_stream_management_enabled)
    /// first), or [`XmppError::Timeout`] if no acknowledgement arrives within
    /// `timeout`.
    pub async fn request_ack(&mut self, timeout: Duration) -> Result<(), XmppError>
    {
        if !self.sm_state.is_enabled()
        {
            return Err(XmppError::Protocol("stream management is not enabled".to_string()));
        }

        // The count the server must reach to have acknowledged everything sent
        // before this `<r/>` (which is itself a control element and uncounted).
        let target = self.sm_state.outbound_count();
        let deadline = tokio::time::Instant::now() + timeout;

        self.writer.lock().await.write(&stanza::sm::AckRequest.as_bytes()).await?;

        loop
        {
            // Register interest before reading the counter so an ack that lands
            // between the check and the await is not missed.
            let notified = self.sm_state.acked.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if ack_reached(self.sm_state.last_ack(), target)
            {
                return Ok(());
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero()
            {
                return Err(XmppError::Timeout("no stream-management acknowledgement".to_string()));
            }

            if tokio::time::timeout(remaining, notified).await.is_err()
            {
                return Err(XmppError::Timeout("no stream-management acknowledgement".to_string()));
            }
        }
    }

    /// Replace the identities and features this client advertises when it
    /// answers an incoming [XEP-0030](https://xmpp.org/extensions/xep-0030.html)
    /// service discovery **info** query.
    ///
    /// By default a client advertises a single `client`/`bot` identity named
    /// `libxmpp` and the two service-discovery features it answers. Call this to
    /// present a different identity or to advertise additional features the
    /// surrounding application supports (using [`add_disco_feature`] for the
    /// latter is usually more convenient). Takes effect for queries received
    /// after it returns.
    ///
    /// [`add_disco_feature`]: Self::add_disco_feature
    pub fn set_disco_info(&self, info: DiscoInfo)
    {
        self.local_disco.lock().unwrap().info = info;
    }

    /// Advertise an additional feature namespace (e.g.
    /// `http://jabber.org/protocol/muc`) in replies to service discovery info
    /// queries. A namespace already advertised is not added again.
    pub fn add_disco_feature(&self, var: &str)
    {
        let mut disco = self.local_disco.lock().unwrap();
        if !disco.info.features.iter().any(|f| f == var)
        {
            disco.info.features.push(var.to_string());
        }
    }

    /// Replace the items this client advertises when it answers an incoming
    /// service discovery **items** query.
    ///
    /// A plain client hosts nothing and so advertises an empty list by default.
    pub fn set_disco_items(&self, items: Vec<DiscoItem>)
    {
        self.local_disco.lock().unwrap().items = items;
    }

    /// Set the URI advertised as the `node` of this client's
    /// [XEP-0115](https://xmpp.org/extensions/xep-0115.html) entity-capabilities
    /// `<c/>` element.
    ///
    /// The node identifies the client software and defaults to the libxmpp
    /// project homepage. It takes effect the next time capabilities are
    /// broadcast, so call [`send_presence`](Self::send_presence) afterwards to
    /// re-advertise.
    pub fn set_caps_node(&self, node: &str)
    {
        self.local_disco.lock().unwrap().caps_node = node.to_string();
    }

    /// The current [XEP-0115](https://xmpp.org/extensions/xep-0115.html)
    /// verification hash (`ver`) for the capabilities this client advertises,
    /// derived from its disco info.
    ///
    /// This is the value paired with the caps node in the `<c/>` element; it
    /// changes whenever the advertised identities or features change.
    pub fn caps_ver(&self) -> String
    {
        return stanza::caps::compute_ver(&self.local_disco.lock().unwrap().info);
    }

    /// Broadcast available presence advertising this client's current
    /// [XEP-0115](https://xmpp.org/extensions/xep-0115.html) entity
    /// capabilities.
    ///
    /// An initial presence is sent automatically on connect; call this to
    /// re-advertise after changing the disco info or caps node (e.g. via
    /// [`add_disco_feature`](Self::add_disco_feature) or
    /// [`set_caps_node`](Self::set_caps_node)) so peers refresh their cached
    /// view of this client's capabilities.
    pub async fn send_presence(&self) -> Result<(), XmppError>
    {
        let presence = build_caps_presence(&self.local_disco);
        return self.send_stanza(&presence.as_bytes()).await;
    }

    /// Allocate the next unique IQ stanza `id`, tagged with `prefix` so it is
    /// recognisable in logs (e.g. `ping_3`, `disco_7`).
    fn next_iq_id(&mut self, prefix: &str) -> String
    {
        self.next_iq_id += 1;
        return format!("{}_{}", prefix, self.next_iq_id);
    }

    /// Write an outgoing stanza, counting it toward XEP-0198 stream management
    /// when enabled. All the send methods funnel through here so the outbound
    /// stanza counter stays consistent with what the server sees.
    async fn send_stanza(&self, bytes: &[u8]) -> Result<(), XmppError>
    {
        return xmpp::write_stanza(&self.writer, &self.sm_state, bytes).await;
    }

    /// Send an IQ `request` already carrying stanza `id`, then wait for the
    /// matching reply and return its raw XML.
    ///
    /// The pending-reply entry is registered before the request is written so
    /// the reader task can never deliver the reply before this call is ready
    /// for it, and is removed again if the write fails or the wait times out.
    async fn request_iq(&mut self, id: &str, request: &dyn Stanza, timeout: Duration) -> Result<String, XmppError>
    {
        let (tx, rx) = oneshot::channel();
        self.pending_iqs.lock().unwrap().insert(id.to_string(), tx);

        if let Err(e) = self.send_stanza(&request.as_bytes()).await
        {
            self.pending_iqs.lock().unwrap().remove(id);
            return Err(e);
        }

        match tokio::time::timeout(timeout, rx).await
        {
            // Reply arrived (result or error); hand back its XML.
            Ok(Ok(reply)) => return Ok(reply),
            // Sender dropped: the reader task ended, so the connection is gone.
            Ok(Err(_)) => return Err(XmppError::Disconnected),
            // No reply within the deadline; drop the pending entry.
            Err(_) =>
            {
                self.pending_iqs.lock().unwrap().remove(id);
                return Err(XmppError::Timeout(format!("no reply for iq id {}", id)));
            }
        }
    }

    /// Shut down the client.
    ///
    /// Stops the background reader task, waits for it to exit, and
    /// closes the underlying socket. After this call the event
    /// receiver returned from [`XmppClient::new`] will yield `None`.
    pub async fn close(self)
    {
        self.shutdown.notify_one();
        let _ = self.task.await;
        self.writer.lock().await.shutdown().await;
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn sm_state_counts_in_both_directions()
    {
        let sm = SmState::new();
        assert!(!sm.is_enabled());

        sm.enable();
        assert!(sm.is_enabled());

        sm.record_inbound();
        sm.record_inbound();
        assert_eq!(sm.inbound_count(), 2);

        sm.record_outbound();
        assert_eq!(sm.outbound_count(), 1);

        sm.record_ack(5);
        assert_eq!(sm.last_ack(), 5);
    }

    #[test]
    fn ack_reached_handles_normal_progress()
    {
        // The server has reached exactly the target: everything is acknowledged.
        assert!(ack_reached(5, 5));
        // Ahead of the target (a later ack) also counts.
        assert!(ack_reached(6, 5));
        // Behind the target: not yet acknowledged.
        assert!(!ack_reached(4, 5));
    }

    #[test]
    fn ack_reached_handles_counter_wraparound()
    {
        // The 32-bit counter wrapped past 2^32: target just below the wrap, ack
        // just after it, so the ack is still "ahead" in modular terms.
        let target = u32::MAX - 1;
        let acked = 2u32; // wrapped around
        assert!(ack_reached(acked, target));

        // Not yet reached across the wrap boundary.
        assert!(!ack_reached(u32::MAX, 2));
    }
}
