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
use std::sync::atomic::{ AtomicBool, AtomicU32, AtomicU64, Ordering };
use std::collections::HashMap;
use std::time::Duration;

mod error;
mod tcp_stream;
mod xml_framer;
mod stanza;
pub use stanza::jid_matches;
mod xmpp;
mod rooms;
mod roster;
mod recipient;

pub use error::XmppError;

/// Maximum encoded full JID length, including `@` and `/` (RFC 7622 §3.1).
pub const MAX_JID_BYTES: usize = 3071;
const MAX_JID_PART_BYTES: usize = 1023;

fn validate_recipient(to: &str) -> Result<(), XmppError>
{
    xmpp::parse_address(to).map(|_| ())
        .map_err(|error| XmppError::InvalidInput(format!("Invalid recipient: {error}")))
}

/// Validate text before queuing it. XML cannot encode certain control
/// characters, even as character references; Unicode and normal whitespace
/// (tabs, newlines and carriage returns) remain supported.
pub fn validate_message_text(text: &str) -> Result<(), XmppError>
{
    if !xml_framer::valid_xml_text(text) {
        return Err(XmppError::InvalidInput("The message contains unsupported control characters.".into()));
    }
    Ok(())
}

/// Remove only characters XML cannot encode. Applications can offer this as
/// an explicit, undoable edit instead of silently changing a user's message.
pub fn clean_message_text(text: &str) -> String
{
    text.chars().filter(|ch| xml_framer::valid_xml_character(*ch)).collect()
}

/// Validate recipient structure/lengths and XML-compatible message content.
pub fn validate_message(to: &str, body: &str) -> Result<(), XmppError>
{
    validate_recipient(to)?;
    validate_message_text(body)
}

/// Check login JID structure before changing local account state or connecting.
/// Requires nonempty user/domain parts and a nonempty resource when supplied,
/// each at most 1023 UTF-8 bytes in the transmitted representation.
pub fn validate_account_jid(jid: &str) -> Result<(), XmppError>
{
    xmpp::parse_jid(jid).map(|_| ())
}

/// Validate a bare room JID before queuing a join. A room needs both a
/// localpart and a domain; occupant resources belong in the nickname argument.
/// Uses the same encoded length and structural checks as native room commands.
pub fn validate_room_jid(room_jid: &str) -> Result<(), XmppError>
{
    if room_jid.contains('/') {
        return Err(XmppError::InvalidInput("Expected a bare room JID without an occupant resource.".into()));
    }
    xmpp::parse_jid(room_jid).map(|_| ())
        .map_err(|error| XmppError::InvalidInput(format!("Invalid room address: {error}")))
}

// Join/leave targets are local command input. Invalid room addresses must not
// be mistaken for a broken session by callers handling operation failures.
fn validate_room_target(room_jid: &str, nick: &str) -> Result<(), XmppError>
{
    validate_room_jid(room_jid)?;
    validate_message_text(nick)?;
    if nick.len() > MAX_JID_PART_BYTES {
        return Err(XmppError::InvalidInput("Room nickname exceeds 1023 UTF-8 bytes.".into()));
    }
    if nick.is_empty() {
        return Err(XmppError::InvalidInput("Expected a bare room JID and nonempty nickname.".into()));
    }
    xmpp::parse_jid(&format!("{room_jid}/{nick}"))
        .map(|_| ())
        .map_err(|error| XmppError::InvalidInput(format!("Invalid room address or nickname: {error}")))
}

fn validate_room_context(context: &str) -> Result<(), XmppError>
{
    if context.is_empty() || context.len() > 128 {
        return Err(XmppError::InvalidInput("Room operation context must contain 1–128 bytes.".into()));
    }
    Ok(())
}
use stanza::Stanza;
use xml_framer::XmlFramer;

/// Registry of in-flight IQ and archive requests, keyed by stanza `id`.
///
/// A caller (e.g. [`XmppClient::ping`]) inserts a [`oneshot::Sender`] before
/// sending its request; the background reader task removes and fires it,
/// handing over the raw reply stanza XML, when the matching
/// `<iq type='result'>` or `<iq type='error'>` arrives. Callers that only need
/// to know a reply came (e.g. ping) ignore the payload; those that need the
/// response body (e.g. service discovery) parse it. The lock is only ever held
/// for the brief, non-async insert/remove, so a [`std::sync::Mutex`] is
/// sufficient. Archive entries remain until their final IQ, so each result's
/// query ID and archive sender can be checked against an open request.
pub(crate) type PendingIqs = Arc<Mutex<HashMap<String, PendingIq>>>;

pub(crate) struct PendingIq
{
    kind: PendingIqKind,
    to: Option<String>,
    own_jid: String,
}

enum PendingIqKind
{
    Reply(oneshot::Sender<String>),
    Archive { deadline: Option<tokio::time::Instant> },
}

const ARCHIVE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

impl PendingIq
{
    fn new(sender: oneshot::Sender<String>, to: Option<&str>, own_jid: &str) -> Self
    {
        Self { kind: PendingIqKind::Reply(sender), to: to.map(str::to_string), own_jid: own_jid.to_string() }
    }

    fn archive(to: Option<&str>, own_jid: &str) -> Self
    {
        Self { kind: PendingIqKind::Archive { deadline: None }, to: to.map(str::to_string), own_jid: own_jid.to_string() }
    }

    fn is_archive(&self) -> bool
    {
        matches!(self.kind, PendingIqKind::Archive { .. })
    }

    fn archive_deadline(&self) -> Option<tokio::time::Instant>
    {
        match self.kind {
            PendingIqKind::Archive { deadline } => deadline,
            _ => None,
        }
    }

    fn touch_archive(&mut self, now: tokio::time::Instant)
    {
        if let PendingIqKind::Archive { deadline } = &mut self.kind {
            *deadline = Some(now + ARCHIVE_IDLE_TIMEOUT);
        }
    }

    /// IDs correlate requests, but do not authenticate their senders. A query
    /// to a bare JID can be answered by any of its resources; a query to a
    /// full JID must be answered by that resource. Our server may also return
    /// routing errors. Omitted senders identify our own account, not a peer.
    fn accepts(&self, from: Option<&str>, is_error: bool) -> bool
    {
        let own_bare = stanza::bare_jid(&self.own_jid);
        let own_server = own_bare.split_once('@').map_or(own_bare, |(_, domain)| domain);
        if self.is_archive() {
            let archive = self.to.as_deref().unwrap_or(own_bare);
            let archive_bare = stanza::bare_jid(archive);
            let archive_server = archive_bare.split_once('@').map_or(archive_bare, |(_, domain)| domain);
            // In particular, a MUC occupant (room/nick) is not the room's
            // archive (room). Treating them as the same enables impersonation.
            return match from {
                None => jid_matches(archive, own_bare),
                Some(from) => jid_matches(from, archive) || (is_error && (jid_matches(from, own_server) || jid_matches(from, archive_server))),
            };
        }
        let is_local = |jid: &str| jid_matches(jid, own_bare) || jid_matches(jid, &self.own_jid) || jid_matches(jid, own_server);
        match (self.to.as_deref(), from) {
            (None, None) => true,
            (None, Some(from)) => is_local(from),
            (Some(to), None) => is_local(to),
            (Some(to), Some(from)) => {
                let target_bare = stanza::bare_jid(to);
                let target_server = target_bare.split_once('@').map_or(target_bare, |(_, domain)| domain);
                (if to.contains('/') { jid_matches(from, to) } else { jid_matches(stanza::bare_jid(from), to) })
                    || (is_error && (jid_matches(from, own_server) || jid_matches(from, target_server)))
            }
        }
    }
}

fn expire_archive_queries(pending_iqs: &PendingIqs, now: tokio::time::Instant) -> Vec<String>
{
    let mut expired = Vec::new();
    pending_iqs.lock().unwrap().retain(|id, request| {
        if request.archive_deadline().is_some_and(|deadline| deadline <= now) {
            expired.push(id.clone());
            false
        } else { true }
    });
    expired
}

// Drop pending reply senders whenever the reader exits, including cancellation,
// so request futures receive Disconnected rather than waiting out their timers.
struct PendingIqCleanup(PendingIqs, SharedSmState);

struct PendingIqRequest { registry: PendingIqs, id: Option<String> }

impl Drop for PendingIqRequest
{
    fn drop(&mut self)
    {
        if let Some(id) = &self.id {
            self.registry.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(id);
        }
    }
}

impl PendingIqCleanup
{
    fn end_reply_waiters(&self)
    {
        // Keep archive authority until complete buffered results are drained;
        // ordinary IQ futures should observe disconnection before that drain
        // can block on a slow event consumer.
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|_, request| request.is_archive());
    }
}

impl Drop for PendingIqCleanup
{
    fn drop(&mut self)
    {
        let mut pending = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.1.closed.store(true, Ordering::SeqCst);
        pending.clear();
        self.1.acked.notify_waiters();
    }
}

/// Current room privacy settings explicitly reported by its service.
/// `None` means unknown; absence of a warning is not a privacy guarantee.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RoomPrivacy
{
    pub publicly_logged: Option<bool>,
    pub non_anonymous: Option<bool>,
}

impl RoomPrivacy
{
    pub(crate) fn merge(&mut self, update: Self) {
        if update.publicly_logged.is_some() { self.publicly_logged = update.publicly_logged; }
        if update.non_anonymous.is_some() { self.non_anonymous = update.non_anonymous; }
    }
}

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
    /// Room traffic carrying the caller's immutable operation context. Produced
    /// by `join_room_with_context`/`leave_room_with_context`. Entries created
    /// with ordinary `join_room` retain their unwrapped event format; a leave
    /// without a new context inherits the entry's context.
    RoomEvent { room: String, context: String, event: Box<XmppEvent> },
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
    RoomJoined { room: String, members: Vec<RoomMember>, own_nick: String },
    /// A room occupant changed nickname without leaving the room.
    MemberNicknameChanged { room: String, old_nick: String, new_nick: String, is_self: bool },
    /// A requested room entry ended before self presence confirmed it.
    RoomJoinFailed { room: String, reason: String },
    /// The local user left the named MUC room.
    RoomLeft(String),
    /// Another occupant joined a MUC room the local user is in.
    MemberJoined { room: String, member: RoomMember },
    /// An occupant left a MUC room the local user is in.
    MemberLeft { room: String, nick: String },
    /// A group chat message was received from `nick` in `room`.
    /// `delay` is set when the message carries XEP-0203 delayed-delivery
    /// information (e.g. discussion history replayed on join).
    /// `stanza_id` identifies the room's archived copy (XEP-0359).
    /// `origin_id` is sender-supplied metadata; validate the sender before correlating it.
    /// Large entry backlogs are delivered in bounded batches before membership
    /// is confirmed. Received messages are also retained if entry fails, is
    /// replaced, or the stream closes; `RoomJoined` need not precede them.
    RoomMessage { room: String, nick: String, body: String, delay: Option<Delay>, stanza_id: Option<String>, origin_id: Option<String> },
    /// Text from a tracked room's bare service address, not an occupant.
    /// The room remains recognized for this connection after departure; this
    /// event does not imply current membership or an occupant's identity.
    RoomServiceMessage { room: String, body: String, delay: Option<Delay>, stanza_id: Option<String>, origin_id: Option<String> },
    /// A MUC room's subject was set or changed.
    RoomSubject { room: String, subject: String },
    /// Current privacy snapshot, after entry confirmation or a live service update.
    RoomPrivacy { room: String, privacy: RoomPrivacy },
    /// A presence stanza of type `error` was received. `error_type` is
    /// the XMPP error class (e.g. `auth`, `cancel`), `condition` is the
    /// defined condition (e.g. `forbidden`, `not-authorized`), and
    /// `text` is the optional human-readable description.
    PresenceError { from: String, error_type: String, condition: String, text: Option<String> },
    /// A one-to-one chat message was received from `from`. `delay` is
    /// set when the message carries XEP-0203 delayed-delivery information
    /// (e.g. a message stored while this client was offline).
    /// `stanza_id` is assigned by the receiving account's archive (XEP-0359).
    /// `origin_id` is sender-supplied metadata, not proof of sender identity.
    DirectMessage { from: String, body: String, delay: Option<Delay>, stanza_id: Option<String>, origin_id: Option<String> },
    /// A private MUC message recognized from muc#user context or a room tracked
    /// in this connection. `from` retains the full occupant JID. Unmarked
    /// messages from unrecognized rooms remain DirectMessage.
    RoomPrivateMessage { from: String, body: String, delay: Option<Delay>, stanza_id: Option<String>, origin_id: Option<String> },
    /// A message was rejected by a recipient or routing entity. Consumers must
    /// correlate the ID with an outgoing message and validate the sender.
    MessageError { id: String, from: Option<String>, error_type: String, condition: String, text: Option<String> },
    /// A presence stanza from `from` advertised XEP-0115 entity capabilities.
    /// `node` and `ver` identify the peer's disco node (`node#ver`), which can
    /// be resolved to its feature set with [`XmppClient::disco_info`]; `hash`
    /// names the algorithm used to build `ver` (e.g. `sha-1`), absent in the
    /// legacy pre-1.4 caps format.
    EntityCapabilities { from: String, node: String, ver: String, hash: Option<String> },
    /// An archived message returned by a MAM query ([`XmppClient::query_archive`],
    /// XEP-0313). `query_id` correlates it with the originating call, `from` is
    /// the archived message's original full `from` JID, `body` its text, and
    /// `timestamp` the forwarded delayed-delivery stamp. `stanza_id` is the
    /// archive id (XEP-0359) of this message, matching the live copy's stanza
    /// id, used to deduplicate the two regardless of timestamp. `origin_id`
    /// belongs to the forwarded original message, not the archive envelope.
    /// `to` preserves the forwarded recipient, including an occupant resource
    /// needed to distinguish outgoing private room conversations.
    ArchiveMessage { query_id: String, from: String, to: Option<String>, body: String, timestamp: Option<String>, stanza_id: Option<String>, origin_id: Option<String> },
    /// A MAM query finished. `complete` is true when no further pages remain in
    /// the query's direction. `first`/`last` are the RSM cursors (the first and
    /// last archive ids of the returned page): use `first` as `before` to page
    /// further back, or `last` as `after` to continue catching up forwards.
    ArchiveComplete { query_id: String, complete: bool, first: Option<String>, last: Option<String> },
    /// A MAM query failed. Its partial results remain valid, but failure does
    /// not prove the archive is exhausted or synchronization is complete.
    ArchiveFailed { query_id: String, reason: String },
}

/// Strength of a successful confirmed send. Neither variant is a receipt from
/// the recipient or proof that the message has been read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendConfirmation
{
    /// XEP-0198: the server accepted responsibility for the sent stanzas.
    StreamAcknowledged,
    /// Legacy fallback: the server answered a probe sent after the message on
    /// the same stream. This checks connectivity, not stanza handling.
    ServerRoundTrip,
}

/// Whether a room departure still needs server confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomLeaveOutcome
{
    /// A departure is pending; wait for the room event carrying its context.
    Requested,
    /// There is no tracked membership or entry request. No stanza was sent.
    AlreadyAbsent,
}

/// Parameters for a MAM archive query; see [`XmppClient::query_archive`].
pub struct ArchiveQuery<'a>
{
    /// Echoed on every result and the `<fin>`; nonempty and unique in flight.
    pub query_id: &'a str,
    /// Room JID for a MUC archive, or `None` for the user's own archive.
    pub to: Option<&'a str>,
    /// Contact JID `with` filter, for a one-to-one archive.
    pub with: Option<&'a str>,
    /// RFC3339 lower time bound (used when catching up forwards).
    pub start: Option<&'a str>,
    /// RFC3339 upper time bound (used when paging backwards).
    pub end: Option<&'a str>,
    /// Page towards the present (`true`) or the past (`false`).
    pub forward: bool,
    /// RSM cursor from a previous page (`after` when forward, else `before`).
    pub cursor: Option<&'a str>,
    /// Page size.
    pub max: u32,
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
    let mut disco = disco.lock().unwrap();
    let ver = stanza::caps::compute_ver(&disco.info);
    let node = disco.caps_node.clone();
    let info = disco.info.clone();
    // A query for an already advertised hash must keep returning the matching
    // info even when the application reconfigures capabilities before it arrives.
    disco.advertised_info.entry(format!("{node}#{ver}")).or_insert(info);
    return stanza::caps::CapsPresence::new(node, ver);
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
    /// Immutable snapshots of locally advertised node#ver pairs. Only outgoing
    /// presence adds entries; peer queries cannot grow this map.
    pub(crate) advertised_info: HashMap<String, DiscoInfo>,
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
            advertised_info: HashMap::new(),
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
    closed: AtomicBool,
    closing: AtomicBool,
    close_sent: AtomicBool,
    inbound: AtomicU32,
    outbound: AtomicU32,
    last_ack: AtomicU32,
    ack_generation: AtomicU64,
    received_ack_generation: AtomicU64,
    /// Notified on every incoming `<a/>`, waking any [`XmppClient::request_ack`]
    /// awaiting confirmation.
    pub(crate) acked: Notify,
    write_failed: Notify,
}

impl SmState
{
    pub(crate) fn new() -> Self
    {
        return Self
        {
            enabled: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            close_sent: AtomicBool::new(false),
            inbound: AtomicU32::new(0),
            outbound: AtomicU32::new(0),
            last_ack: AtomicU32::new(0),
            ack_generation: AtomicU64::new(0),
            received_ack_generation: AtomicU64::new(0),
            acked: Notify::new(),
            write_failed: Notify::new(),
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

    pub(crate) fn begin_ack(&self) -> u64
    {
        self.received_ack_generation.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
    }

    #[cfg(test)]
    fn record_ack(&self, h: u32) -> bool
    {
        self.record_received_ack(h, self.begin_ack())
    }

    /// Record a valid peer count and wake ack waiters. The caller holds the
    /// writer lock so a just-written stanza is counted before its ack is checked.
    pub(crate) fn record_received_ack(&self, h: u32, generation: u64) -> bool
    {
        if !self.is_enabled() || self.closed.load(Ordering::SeqCst) { return false; }
        let previous = self.last_ack();
        let outstanding = self.outbound_count().wrapping_sub(previous);
        if h.wrapping_sub(previous) > outstanding { return false; }
        self.last_ack.store(h, Ordering::SeqCst);
        self.ack_generation.store(generation, Ordering::SeqCst);
        self.acked.notify_waiters();
        return true;
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

// A cancelled write_all/flush may already have put part of an XML frame on
// the wire. Keep the guard inside the writer lock: cancellation while merely
// waiting for that lock has not touched the stream and must remain harmless.
struct SessionWrite(Option<SharedSmState>);

impl Drop for SessionWrite
{
    fn drop(&mut self)
    {
        if let Some(state) = &self.0 {
            state.closed.store(true, Ordering::SeqCst);
            state.acked.notify_waiters();
            state.write_failed.notify_one();
        }
    }
}

pub(crate) async fn write_session(writer: &mut tcp_stream::TcpWriter, state: &SharedSmState, bytes: &[u8]) -> Result<(), XmppError>
{
    if state.closed.load(Ordering::SeqCst) { return Err(XmppError::Disconnected); }
    let mut guard = SessionWrite(Some(state.clone()));
    writer.write(bytes).await?;
    guard.0 = None;
    Ok(())
}

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

const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

async fn send_stream_close(writer: &SharedWriter, state: &SharedSmState)
{
    state.closing.store(true, Ordering::SeqCst);
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, async {
        let mut writer = writer.lock().await;
        if !state.close_sent.swap(true, Ordering::SeqCst) {
            write_session(&mut writer, state, b"</stream:stream>").await?;
        }
        Ok::<(), XmppError>(())
    }).await;
}

async fn reject_stream(writer: &SharedWriter, state: &SharedSmState, condition: &str)
{
    state.closing.store(true, Ordering::SeqCst);
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, async {
        let mut writer = writer.lock().await;
        if !state.close_sent.load(Ordering::SeqCst) {
            let error = format!("<stream:error><{condition} xmlns='urn:ietf:params:xml:ns:xmpp-streams'/></stream:error>");
            let _ = write_session(&mut writer, state, error.as_bytes()).await;
        }
    }).await;
    send_stream_close(writer, state).await;
    shutdown_writer(writer).await;
}

async fn shutdown_writer(writer: &SharedWriter)
{
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, async {
        writer.lock().await.shutdown().await;
    }).await;
}

/// An asynchronous XMPP client.
///
/// An `XmppClient` owns the TCP/TLS connection, the background reader
/// task that parses incoming stanzas, and the writer used to send
/// outgoing stanzas. Construct one with [`XmppClient::new`], drive it
/// by reading from the [`mpsc::Receiver`] it returns, and shut it down
/// cleanly with [`XmppClient::close`].
///
/// Cancelling or failing a write invalidates the session because a partial
/// XML frame may already be on the wire. Subsequent sends fail with
/// [`XmppError::Disconnected`]. Cancelling while waiting for the writer lock
/// or an IQ reply does not invalidate an otherwise usable connection.
pub struct XmppClient
{
    shutdown: Arc<Notify>,
    task: JoinHandle<()>,
    writer: SharedWriter,
    bound_jid: String,
    pending_iqs: PendingIqs,
    pending_iq_changed: Arc<Notify>,
    next_iq_id: u64,
    local_disco: LocalDiscoState,
    sm_state: SharedSmState,
    rooms: rooms::Rooms,
}

impl XmppClient
{
    /// Connect to an XMPP server and authenticate.
    ///
    /// `jid` is the bare or full JID of the account (e.g.
    /// `user@example.com`); the domain part is used to locate the
    /// server. `password` is the account password used during SASL
    /// authentication. If a resource is supplied, it is requested during binding;
    /// [`Self::get_jid`] returns the authoritative JID assigned by the server.
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
        let (bound_jid, tcp, sm_enabled, framer) = xmpp::setup_connection(&event_tx, jid, password).await?;
        Self::from_connection(bound_jid, tcp, sm_enabled, framer, event_tx, event_rx).await
    }

    // Carry the setup framer into the session: the final socket read may have
    // already buffered complete or partial stanzas following the bind/SM reply.
    async fn from_connection(
        bound_jid: String,
        tcp: tcp_stream::Tcp,
        sm_enabled: bool,
        mut framer: XmlFramer,
        event_tx: mpsc::Sender<XmppEvent>,
        event_rx: mpsc::Receiver<XmppEvent>,
    ) -> Result<(Self, mpsc::Receiver<XmppEvent>), XmppError>
    {
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
        write_session(&mut writer, &sm_state, &build_caps_presence(&local_disco).as_bytes()).await?;
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
        let pending_iq_changed = Arc::new(Notify::new());
        let pending_iq_changed_loop = pending_iq_changed.clone();

        let local_disco_loop = local_disco.clone();

        let sm_state_loop = sm_state.clone();
        let rooms = Arc::new(Mutex::new(rooms::Registry::new(pending_iq_changed.clone())));
        let rooms_loop = rooms.clone();

        // Publish the lifecycle boundary before any buffered incoming events.
        let _ = event_tx.send(XmppEvent::Connected).await;
        let bound_jid_loop = bound_jid.clone();
        let task = tokio::spawn(async move
        {
            let _room_cleanup = rooms::Cleanup(rooms_loop.clone());
            // Mark the connection closed before room cleanup wakes a pending
            // rejoin. Guards are dropped in reverse declaration order.
            let pending_cleanup = PendingIqCleanup(pending_iqs_loop.clone(), sm_state_loop.clone());
            let mut draining_failed_session = false;

            'session: loop
            {
                for event in rooms::take_ready(&rooms_loop) { let _ = event_tx_loop.send(event).await; }
                // Parse all data on the framer and process stanzas.
                while let Some(stanza_xml) = framer.try_next()
                {
                    if !framer.stream_is_open() {
                        send_stream_close(&writer_loop, &sm_state_loop).await;
                        shutdown_writer(&writer_loop).await;
                        // After both XML streams close, exchange TLS close_notify.
                        // Waiting is bounded so an uncooperative peer cannot keep
                        // the event receiver and requests alive indefinitely.
                        if matches!(reader, tcp_stream::TcpReader::Tls(_)) {
                            let _ = tokio::time::timeout(CLOSE_TIMEOUT, async {
                                while reader.read().await.is_ok() {}
                            }).await;
                        }
                        break 'session;
                    }
                    if let Some(error) = xmpp::stream_error(&stanza_xml) {
                        log::warn!("{}", error);
                        send_stream_close(&writer_loop, &sm_state_loop).await;
                        shutdown_writer(&writer_loop).await;
                        break 'session;
                    }
                    log::debug!("Received stanza: {}", stanza_xml);
                    xmpp::process_stanza_with_count(&stanza_xml, !framer.last_frame_unmanaged(), &event_tx_loop, &rooms_loop, &pending_iqs_loop, &writer_loop, &local_disco_loop, &sm_state_loop, Some(&bound_jid_loop)).await;
                    if sm_state_loop.closed.load(Ordering::SeqCst) && !draining_failed_session {
                        // Stop request waiters promptly, but keep delivering
                        // complete frames already received before the write failed.
                        pending_cleanup.end_reply_waiters();
                        draining_failed_session = true;
                    }
                }
                if sm_state_loop.closed.load(Ordering::SeqCst) { break 'session; }

                if let Some(error) = framer.error() {
                    log::error!("Invalid XMPP stream: {}", error);
                    reject_stream(&writer_loop, &sm_state_loop, error.condition()).await;
                    break;
                }

                let archive_deadline = pending_iqs_loop.lock().unwrap().values()
                    .filter_map(PendingIq::archive_deadline).min();
                let request_deadline = archive_deadline.into_iter().chain(rooms::next_deadline(&rooms_loop)).min();
                // Sleep until the next archive or room-entry deadline, with a
                // notification for new requests. Idle sessions need no polling.
                let request_timeout = async {
                    if let Some(deadline) = request_deadline {
                        tokio::time::sleep_until(deadline).await;
                    } else { std::future::pending::<()>().await; }
                };
                tokio::select!
                {
                    _ = request_timeout => {
                        for (room, nick, context, buffered) in rooms::expire(&rooms_loop, tokio::time::Instant::now()) {
                            for event in buffered { let _ = event_tx_loop.send(rooms::contextual_event(&room, context.as_deref(), event)).await; }
                            // Cancel the server-side entry as well, so a late
                            // acceptance cannot leave a hidden room membership.
                            let cleanup = tokio::time::timeout(CLOSE_TIMEOUT, xmpp::cancel_expired_room(&writer_loop, &sm_state_loop, &rooms_loop, &room, nick)).await;
                            let replaced = rooms_loop.lock().unwrap().contains_key(&room);
                            if !replaced && !matches!(cleanup, Ok(Ok(false))) {
                                let failure = XmppEvent::RoomJoinFailed { room: room.clone(), reason: "Timed out waiting for room entry confirmation.".into() };
                                let _ = event_tx_loop.send(rooms::contextual_event(&room, context.as_deref(), failure)).await;
                            }
                            if !matches!(cleanup, Ok(Ok(_))) { break 'session; }
                        }
                        for query_id in expire_archive_queries(&pending_iqs_loop, tokio::time::Instant::now()) {
                            let _ = event_tx_loop.send(XmppEvent::ArchiveFailed {
                                query_id, reason: "archive query timed out waiting for results".into(),
                            }).await;
                        }
                    }
                    _ = pending_iq_changed_loop.notified() => {}
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
                    _ = sm_state_loop.write_failed.notified() => { break; }
                    _ = shutdown_clone.notified() =>
                    {
                        break;
                    }
                }
            }
            // End pending network requests before data delivery can block on
            // a slow consumer. Room cleanup remains after the final data drain.
            drop(pending_cleanup);
            // A closed stream does not invalidate messages already received
            // from requested rooms whose entry was never confirmed.
            for event in rooms::drain_buffered(&rooms_loop) { let _ = event_tx_loop.send(event).await; }
        });

        return Ok((Self { shutdown, task, writer, bound_jid, pending_iqs, pending_iq_changed, next_iq_id: 0, local_disco, sm_state, rooms }, event_rx));
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
    /// the initial roster. Equivalent domain spellings reuse a tracked room;
    /// its events retain the address spelling originally registered.
    pub async fn join_room(&mut self, room_jid: &str, nick: &str) -> Result<(), XmppError>
    {
        self.join_room_inner(room_jid, nick, None).await
    }

    /// Join with a caller-assigned operation context (1–128 bytes). Room events
    /// keep this context even if another operation starts before they are read.
    /// Use a distinct context for each operation. It is local metadata and is
    /// never transmitted to the room service.
    pub async fn join_room_with_context(&mut self, room_jid: &str, nick: &str, context: &str) -> Result<(), XmppError>
    {
        validate_room_context(context)?;
        self.join_room_inner(room_jid, nick, Some(context)).await
    }

    async fn join_room_inner(&mut self, room_jid: &str, nick: &str, context: Option<&str>) -> Result<(), XmppError>
    {
        validate_room_target(room_jid, nick)?;
        // Do not relabel an old self-unavailable stanza as a failure of this
        // new entry. Keep the Leaving record until the server confirms it.
        rooms::wait_for_departure(&self.rooms, &self.sm_state, room_jid, Duration::from_secs(10)).await?;
        let id = self.next_iq_id("join");
        // Waiting for the writer does not start a new membership. Incoming
        // confirmation, roster and topic updates still belong to the preceding
        // entry, and cancellation here must leave that entry untouched.
        let writer = self.writer.clone();
        let mut writer = writer.lock().await;
        if self.sm_state.closed.load(Ordering::SeqCst) || self.sm_state.closing.load(Ordering::SeqCst) {
            return Err(XmppError::Disconnected);
        }
        let room_key = rooms::key(&self.rooms.lock().unwrap(), room_jid).unwrap_or(room_jid).to_string();
        let room_jid = room_key.as_str();
        let presence = stanza::muc::MucJoinPresence::new(room_jid.to_string(), nick.to_string()).with_id(id.clone());
        let mut joining = rooms::Room::joining(id.clone(), nick.into());
        joining.context = context.map(str::to_string);
        let previous = {
            let mut rooms = self.rooms.lock().unwrap();
            rooms.defer_buffered(room_jid);
            rooms.insert(room_jid.into(), joining)
        };
        let mut guard = rooms::RequestGuard { rooms: self.rooms.clone(), room: room_jid.into(), request_id: Some(id), previous, leaving: false };
        xmpp::write_stanza_locked(&mut writer, &self.sm_state, &presence.as_bytes()).await?;
        if let Some(room) = self.rooms.lock().unwrap().get_mut(room_jid) {
            if room.phase == rooms::Phase::Joining { room.deadline = Some(tokio::time::Instant::now() + Duration::from_secs(60)); }
        }
        self.pending_iq_changed.notify_one();
        guard.request_id = None;
        Ok(())
    }

    /// Leave a MUC room previously joined as `nick`.
    ///
    /// On success a [`RoomLeft`](XmppEvent::RoomLeft) event is emitted
    /// once the server confirms the departure. Already absent rooms are a no-op.
    pub async fn leave_room(&mut self, room_jid: &str, nick: &str) -> Result<(), XmppError>
    {
        self.request_room_leave(room_jid, nick, None).await.map(|_| ())
    }

    /// Leave with a new operation context, so a queued departure confirmation
    /// cannot be confused with a subsequent rejoin of the same address.
    pub async fn leave_room_with_context(&mut self, room_jid: &str, nick: &str, context: &str) -> Result<(), XmppError>
    {
        self.request_room_leave(room_jid, nick, Some(context)).await.map(|_| ())
    }

    /// Request departure only if this connection tracks a membership or entry.
    /// An absent room needs no wire request or future server acknowledgement.
    /// Repeated departures reuse the pending request, adopting a supplied context.
    pub async fn request_room_leave(&mut self, room_jid: &str, nick: &str, context: Option<&str>) -> Result<RoomLeaveOutcome, XmppError>
    {
        validate_room_target(room_jid, nick)?;
        if let Some(context) = context { validate_room_context(context)?; }
        let id = self.next_iq_id("leave");
        // Check and replace under one lock: a server departure can remove the
        // membership concurrently. Never create a phantom request for it.
        let (room_key, previous) = {
            let mut rooms = self.rooms.lock().unwrap();
            let Some(room_key) = rooms::key(&rooms, room_jid).map(str::to_string) else { return Ok(RoomLeaveOutcome::AlreadyAbsent); };
            let Some(room) = rooms.get_mut(&room_key) else { return Ok(RoomLeaveOutcome::AlreadyAbsent); };
            if room.phase == rooms::Phase::Leaving {
                if let Some(context) = context { room.context = Some(context.into()); }
                return Ok(RoomLeaveOutcome::Requested);
            }
            let leaving = rooms::Room::departing(id.clone(), context, room);
            rooms.defer_buffered(&room_key);
            let previous = rooms.insert(room_key.clone(), leaving);
            (room_key, previous)
        };
        let room_jid = room_key.as_str();
        let mut guard = rooms::RequestGuard { rooms: self.rooms.clone(), room: room_jid.into(), request_id: Some(id.clone()), previous, leaving: true };
        xmpp::write_room_leave(&self.writer, &self.sm_state, &self.rooms, room_jid, &id).await?;
        guard.request_id = None;
        Ok(RoomLeaveOutcome::Requested)
    }

    /// Send a group chat message to a MUC room.
    ///
    /// `room_jid` is the bare JID of the room. The local user must
    /// already have joined the room.
    pub async fn send_room_message(&mut self, room_jid: &str, body: &str) -> Result<(), XmppError>
    {
        validate_message(room_jid, body)?;
        let msg = stanza::muc::MucGroupMessage::new(room_jid.to_string(), body.to_string());
        return self.send_stanza(&msg.as_bytes()).await;
    }

    /// Write a room message and await server confirmation. The deadline covers
    /// both socket backpressure and confirmation. On failure, callers should
    /// retain the message for retry; it might already have reached the server.
    /// Reuse the persisted `origin_id` for all attempts of the same message.
    pub async fn send_room_message_confirmed(&mut self, room_jid: &str, body: &str, origin_id: Option<&str>, timeout: Duration) -> Result<SendConfirmation, XmppError>
    {
        validate_message(room_jid, body)?;
        let msg = stanza::muc::MucGroupMessage::new(room_jid.to_string(), body.to_string()).with_origin_id(origin_id);
        self.send_confirmed(&msg, origin_id, timeout).await
    }

    /// Send a one-to-one chat message.
    ///
    /// `to` is the recipient's bare or full JID. Replies will arrive
    /// as [`DirectMessage`](XmppEvent::DirectMessage) events.
    pub async fn send_message(&mut self, to: &str, body: &str) -> Result<(), XmppError>
    {
        validate_message(to, body)?;
        let msg = stanza::chat::ChatMessage::new(to.to_string(), body.to_string());
        return self.send_stanza(&msg.as_bytes()).await;
    }

    /// Write a direct message and await server confirmation. With XEP-0198,
    /// success means the server accepted responsibility. Otherwise a server
    /// ping round trip is required. Neither confirms recipient delivery/read.
    /// `timeout` bounds the write and confirmation together.
    /// Supply a persisted `origin_id` to reuse the same wire identity on retry.
    pub async fn send_message_confirmed(&mut self, to: &str, body: &str, origin_id: Option<&str>, timeout: Duration) -> Result<SendConfirmation, XmppError>
    {
        validate_message(to, body)?;
        let msg = stanza::chat::ChatMessage::new(to.to_string(), body.to_string()).with_origin_id(origin_id);
        self.send_confirmed(&msg, origin_id, timeout).await
    }

    /// Send a private room message to a full occupant JID, preserving PM context
    /// for recipients and carbon copies without addressing the public room.
    pub async fn send_room_private_message_confirmed(&mut self, to: &str, body: &str, origin_id: Option<&str>, timeout: Duration) -> Result<SendConfirmation, XmppError>
    {
        let (room, nick) = to.split_once('/').ok_or_else(|| XmppError::InvalidInput("Private room messages require an occupant nickname.".into()))?;
        validate_room_target(room, nick)?;
        validate_message(to, body)?;
        let msg = stanza::chat::ChatMessage::new(to.into(), body.into()).with_origin_id(origin_id).with_muc_private();
        self.send_confirmed(&msg, origin_id, timeout).await
    }

    async fn send_confirmed(&mut self, message: &(dyn Stanza + Sync), origin_id: Option<&str>, timeout: Duration) -> Result<SendConfirmation, XmppError>
    {
        if origin_id.is_some_and(|id| id.is_empty() || !xml_framer::valid_xml_text(id)) {
            return Err(XmppError::InvalidInput("The message identity is invalid.".into()));
        }
        tokio::time::timeout(timeout, async {
            self.send_stanza(&message.as_bytes()).await?;
            if self.is_stream_management_enabled() {
                self.request_ack(timeout).await?;
                Ok(SendConfirmation::StreamAcknowledged)
            } else {
                self.ping(None, timeout).await?;
                Ok(SendConfirmation::ServerRoundTrip)
            }
        }).await.map_err(|_| XmppError::Timeout("message write or server confirmation timed out".into()))?
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
        self.request_iq(&id, &request, to, timeout).await?;
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

        let reply = self.request_iq(&id, &request, to, timeout).await?;
        let result = stanza::disco::DiscoInfoResult::from_xml(&reply).map_err(XmppError::Parse)?;

        // An error reply commonly echoes an empty `<query/>`, so detect failure
        // from the iq type rather than the presence of the query payload.
        if result.iq_type == "error"
        {
            return Err(iq_error("disco#info", &result.iq_type));
        }
        let query = result.query.ok_or_else(|| iq_error("disco#info", &result.iq_type))?;
        if query.node.as_deref() != node {
            return Err(XmppError::Protocol("Discovery info reply does not match the requested node.".into()));
        }

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

        let reply = self.request_iq(&id, &request, to, timeout).await?;
        let result = stanza::disco::DiscoItemsResult::from_xml(&reply).map_err(XmppError::Parse)?;

        // An error reply commonly echoes an empty `<query/>`, so detect failure
        // from the iq type rather than the presence of the query payload.
        if result.iq_type == "error"
        {
            return Err(iq_error("disco#items", &result.iq_type));
        }
        let query = result.query.ok_or_else(|| iq_error("disco#items", &result.iq_type))?;
        if query.node.as_deref() != node {
            return Err(XmppError::Protocol("Discovery items reply does not match the requested node.".into()));
        }

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
        if self.sm_state.closed.load(Ordering::SeqCst) || self.sm_state.closing.load(Ordering::SeqCst) { return Err(XmppError::Disconnected); }
        if !self.sm_state.is_enabled()
        {
            return Err(XmppError::Protocol("stream management is not enabled".to_string()));
        }

        let deadline = tokio::time::Instant::now() + timeout;
        // Snapshot the target while holding the writer so automatic replies
        // written before <r/> are included. The timeout covers lock contention
        // and socket backpressure as well as waiting for the server's response.
        let (target, generation) = tokio::time::timeout_at(deadline, async {
            let mut writer = self.writer.lock().await;
            let target = self.sm_state.outbound_count();
            let generation = self.sm_state.received_ack_generation.load(Ordering::SeqCst);
            write_session(&mut writer, &self.sm_state, &stanza::sm::AckRequest.as_bytes()).await?;
            Ok::<_, XmppError>((target, generation))
        }).await.map_err(|_| XmppError::Timeout("stream-management request write timed out".into()))??;

        loop
        {
            // Register interest before reading the counter so an ack that lands
            // between the check and the await is not missed.
            let notified = self.sm_state.acked.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.sm_state.closed.load(Ordering::SeqCst) || self.sm_state.closing.load(Ordering::SeqCst) { return Err(XmppError::Disconnected); }
            let elapsed = self.sm_state.ack_generation.load(Ordering::SeqCst).wrapping_sub(generation);
            if elapsed != 0 && elapsed <= u64::MAX / 2
                && ack_reached(self.sm_state.last_ack(), target)
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
        loop {
            self.next_iq_id = self.next_iq_id.wrapping_add(1);
            let id = format!("{}_{}", prefix, self.next_iq_id);
            if !self.pending_iqs.lock().unwrap().contains_key(&id) { return id; }
        }
    }

    /// Write an outgoing stanza, counting it toward XEP-0198 stream management
    /// when enabled. Room joins register state while holding the writer and
    /// use the same locked-write helper for checks and outbound accounting.
    async fn send_stanza(&self, bytes: &[u8]) -> Result<(), XmppError>
    {
        if !std::str::from_utf8(bytes).is_ok_and(xml_framer::valid_xml_text) {
            return Err(XmppError::InvalidInput("Outgoing text contains unsupported control characters.".into()));
        }
        if self.sm_state.closed.load(Ordering::SeqCst) || self.sm_state.closing.load(Ordering::SeqCst) {
            return Err(XmppError::Disconnected);
        }
        return xmpp::write_stanza(&self.writer, &self.sm_state, bytes).await;
    }

    /// Send an IQ `request` already carrying stanza `id`, then wait for the
    /// matching reply and return its raw XML.
    ///
    /// The pending-reply entry is registered before the request is written so
    /// the reader task can never deliver the reply before this call is ready
    /// for it, and is removed again if the write fails or the wait times out.
    async fn request_iq(&mut self, id: &str, request: &(dyn Stanza + Sync), to: Option<&str>, timeout: Duration) -> Result<String, XmppError>
    {
        if let Some(to) = to { validate_recipient(to)?; }
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending_iqs.lock().unwrap();
            // Serialize registration with reader cleanup so a request cannot be
            // inserted after EOF has already cleared the registry.
            if self.sm_state.closed.load(Ordering::SeqCst) || self.sm_state.closing.load(Ordering::SeqCst) { return Err(XmppError::Disconnected); }
            if pending.contains_key(id) { return Err(XmppError::Protocol("IQ id is already in flight".into())); }
            pending.insert(id.to_string(), PendingIq::new(tx, to, &self.bound_jid));
        }
        let _pending = PendingIqRequest { registry: self.pending_iqs.clone(), id: Some(id.to_string()) };
        tokio::time::timeout(timeout, async {
            self.send_stanza(&request.as_bytes()).await?;
            rx.await.map_err(|_| XmppError::Disconnected)
        }).await.map_err(|_| XmppError::Timeout(format!("no reply for iq id {}", id)))?
    }

    /// Query the message archive (XEP-0313 MAM).
    ///
    /// The query's `query_id` is echoed back on every resulting
    /// [`ArchiveMessage`](XmppEvent::ArchiveMessage) and the final
    /// [`ArchiveComplete`](XmppEvent::ArchiveComplete), so concurrent queries
    /// can be told apart. IDs must be nonempty and unique among in-flight
    /// requests. Replies are accepted only from the queried archive.
    /// A query with no results or final reply for 60 seconds emits
    /// [`ArchiveFailed`](XmppEvent::ArchiveFailed); each accepted result renews
    /// that idle deadline. The caller can separately bound the initial write.
    /// See [`ArchiveQuery`] for the parameters.
    pub async fn query_archive(&mut self, q: ArchiveQuery<'_>) -> Result<(), XmppError>
    {
        if let Some(to) = q.to { validate_recipient(to)?; }
        if let Some(contact) = q.with { validate_recipient(contact)?; }
        let query = stanza::mam::MamQuery
        {
            id: q.query_id.to_string(),
            to: q.to.map(str::to_string),
            with: q.with.map(str::to_string),
            start: q.start.map(str::to_string),
            end: q.end.map(str::to_string),
            forward: q.forward,
            cursor: q.cursor.map(str::to_string),
            max: q.max,
        };
        {
            let mut pending = self.pending_iqs.lock().unwrap();
            if self.sm_state.closed.load(Ordering::SeqCst) || self.sm_state.closing.load(Ordering::SeqCst) { return Err(XmppError::Disconnected); }
            if q.query_id.is_empty() || pending.contains_key(q.query_id) {
                return Err(XmppError::Protocol("archive query id is empty or already in flight".into()));
            }
            pending.insert(q.query_id.to_string(), PendingIq::archive(q.to, &self.bound_jid));
        }
        let mut pending = PendingIqRequest { registry: self.pending_iqs.clone(), id: Some(q.query_id.to_string()) };
        self.send_stanza(&query.as_bytes()).await?;
        if let Some(request) = self.pending_iqs.lock().unwrap().get_mut(q.query_id) {
            request.touch_archive(tokio::time::Instant::now());
        }
        self.pending_iq_changed.notify_one();
        // The reader now owns the registration until the final IQ or EOF.
        pending.id = None;
        Ok(())
    }

    /// Shut down the client.
    ///
    /// Sends the closing XML tag and briefly lets the reader finish processing
    /// the peer's final stanzas and closing response, then closes the socket.
    /// A blocked event consumer or peer cannot prevent shutdown indefinitely.
    pub async fn close(mut self)
    {
        // Publish already-received data while the consumer is still draining,
        // even when the peer never confirms the closing stream.
        self.rooms.lock().unwrap().begin_close();
        send_stream_close(&self.writer, &self.sm_state).await;
        if tokio::time::timeout(CLOSE_TIMEOUT, &mut self.task).await.is_err() {
            self.shutdown.notify_one();
            self.task.abort();
            let _ = (&mut self.task).await;
        }
        shutdown_writer(&self.writer).await;
    }
}

impl Drop for XmppClient
{
    fn drop(&mut self)
    {
        // Dropping a JoinHandle detaches its task; explicitly cancel the reader
        // so abandoned clients do not retain a socket and keep sending replies.
        self.task.abort();
        self.rooms.lock().unwrap_or_else(|e| e.into_inner()).clear();
        drop(PendingIqCleanup(self.pending_iqs.clone(), self.sm_state.clone()));
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

        assert!(sm.record_ack(1));
        assert_eq!(sm.last_ack(), 1);
        assert!(!sm.record_ack(5));
        assert_eq!(sm.last_ack(), 1);
    }

    #[test]
    fn acknowledgement_freshness_tracks_receipt_before_writer_lock_wait()
    {
        let sm = SmState::new();
        sm.enable();
        sm.record_outbound();
        let received = sm.begin_ack();
        let probe_baseline = sm.received_ack_generation.load(Ordering::SeqCst);
        assert!(sm.record_received_ack(1, received));
        assert_eq!(sm.ack_generation.load(Ordering::SeqCst), probe_baseline);
        let invalid = sm.begin_ack();
        assert!(!sm.record_received_ack(2, invalid));
        assert_eq!(sm.ack_generation.load(Ordering::SeqCst), probe_baseline);
        let fresh = sm.begin_ack();
        assert!(sm.record_received_ack(1, fresh));
        assert!(sm.ack_generation.load(Ordering::SeqCst) > probe_baseline);
    }

    #[test]
    fn peer_acknowledgements_cannot_move_backwards_or_ahead_of_sent_stanzas()
    {
        let sm = SmState::new();
        sm.enable();
        sm.outbound.store(2, Ordering::SeqCst);
        assert!(sm.record_ack(1));
        let generation = sm.ack_generation.load(Ordering::SeqCst);
        assert!(!sm.record_ack(0));
        assert!(!sm.record_ack(3));
        assert_eq!(sm.last_ack(), 1);
        assert_eq!(sm.ack_generation.load(Ordering::SeqCst), generation);
        assert!(sm.record_ack(2));
        assert!(sm.record_ack(2)); // A fresh idle probe may repeat the same count.
        assert_eq!(sm.ack_generation.load(Ordering::SeqCst), generation + 4);
        sm.last_ack.store(u32::MAX - 1, Ordering::SeqCst);
        sm.outbound.store(1, Ordering::SeqCst);
        assert!(sm.record_ack(u32::MAX));
        assert!(sm.record_ack(0));
        assert!(sm.record_ack(1));
        assert!(!sm.record_ack(0));
        assert!(!sm.record_ack(2));
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
    async fn test_connection(framer: XmlFramer) -> (XmppClient, mpsc::Receiver<XmppEvent>, tokio::net::TcpStream)
    {
        test_connection_with_sm(framer, false).await
    }

    async fn test_connection_with_sm(framer: XmlFramer, sm_enabled: bool) -> (XmppClient, mpsc::Receiver<XmppEvent>, tokio::net::TcpStream)
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let (tx, rx) = mpsc::channel(32);
        let (client, events) = XmppClient::from_connection(
            "me@example/test".into(), tcp_stream::Tcp::Connected(client, "example".into()),
            sm_enabled, framer, tx, rx,
        ).await.unwrap();
        (client, events, server)
    }

    async fn read_until_written(server: &mut tokio::net::TcpStream, needle: &[u8]) -> Vec<u8>
    {
        use tokio::io::AsyncReadExt;
        tokio::time::timeout(Duration::from_secs(1), async {
            let mut bytes = Vec::new();
            while !bytes.windows(needle.len()).any(|window| window == needle) {
                let mut chunk = [0; 4096];
                let n = server.read(&mut chunk).await.unwrap();
                assert!(n > 0, "client closed before the expected write");
                bytes.extend_from_slice(&chunk[..n]);
                assert!(bytes.len() < 65536);
            }
            bytes
        }).await.expect("expected client write did not arrive")
    }

    #[tokio::test]
    async fn discovery_replies_must_match_the_requested_node_and_namespace() {
        use tokio::io::AsyncWriteExt;
        for items in [false, true] {
            for (requested, echoed, namespace_ok, success) in [
                (Some("scope&node"), Some("scope&node"), true, true),
                (Some("scope"), None, true, false),
                (Some("scope"), Some("other"), true, false),
                (None, Some("scope"), true, false),
                (None, None, true, true),
                (Some("scope"), Some("scope"), false, false),
            ] {
                let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
                assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
                read_until_written(&mut server, b"</presence>").await;
                let request = tokio::spawn(async move {
                    let result = if items {
                        client.disco_items(Some("disco.example"), requested, Duration::from_secs(2)).await.map(|items| {
                            assert_eq!(items.len(), 1);
                            assert_eq!(items[0].jid, "room@example");
                        })
                    } else {
                        client.disco_info(Some("disco.example"), requested, Duration::from_secs(2)).await.map(|info| {
                            assert_eq!(info.features, ["real"]);
                        })
                    };
                    (client, result)
                });
                let wire = String::from_utf8(read_until_written(&mut server, b"</iq>").await).unwrap();
                let mut reader = quick_xml::Reader::from_str(&wire);
                let quick_xml::events::Event::Start(root) = reader.read_event().unwrap() else { panic!("expected outgoing IQ"); };
                let id = root.try_get_attribute("id").unwrap().unwrap().unescape_value().unwrap().into_owned();
                if let Some(node) = requested { assert!(wire.contains(&format!("node='{}'", quick_xml::escape::escape(node)))); }
                let namespace = if !namespace_ok { "extension" } else if items { stanza::disco::DISCO_ITEMS_NS } else { stanza::disco::DISCO_INFO_NS };
                let node = echoed.map(|node| format!(" node='{}'", quick_xml::escape::escape(node))).unwrap_or_default();
                let payload = if items { "<d:item jid='room@example'/><item xmlns='extension' jid='forged'/>" }
                    else { "<d:feature var='real'/><feature xmlns='extension' var='forged'/>" };
                let reply = format!("<iq type='result' from='disco.example' id='{id}' xmlns:d='{namespace}'><d:query{node}>{payload}</d:query></iq>");
                server.write_all(reply.as_bytes()).await.unwrap();
                let (client, result) = tokio::time::timeout(Duration::from_secs(3), request).await.unwrap().unwrap();
                if success { result.unwrap(); }
                else { assert!(matches!(result, Err(XmppError::Protocol(_))), "items={items}, requested={requested:?}, echoed={echoed:?}: {result:?}"); }
                assert!(client.pending_iqs.lock().unwrap().is_empty());
                drop(server);
                client.close().await;
            }
        }
    }

    #[tokio::test]
    async fn confirmed_messages_wait_until_their_stanza_is_acknowledged()
    {
        use tokio::io::AsyncWriteExt;
        for room in [false, true] {
            let (mut client, mut events, mut server) = test_connection_with_sm(XmlFramer::new_client_stream(), true).await;
            assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
            let sending = tokio::spawn(async move {
                let result = if room { client.send_room_message_confirmed("room@example", "payload", Some("stable-id"), Duration::from_secs(2)).await }
                    else { client.send_message_confirmed("alice@example", "payload", Some("stable-id"), Duration::from_secs(2)).await };
                (client, result)
            });
            let bytes = read_until_written(&mut server, b"<r xmlns=").await;
            let xml = String::from_utf8(bytes).unwrap();
            assert!(xml.find("<body>payload</body>").unwrap() < xml.find("<r xmlns=").unwrap());
            assert_eq!(xml.matches("id='stable-id'").count(), 2);
            assert!(xml.contains("<origin-id xmlns='urn:xmpp:sid:0' id='stable-id'/>"));
            // Only initial presence has been handled. The following incoming
            // message is a barrier proving this insufficient ack was processed.
            server.write_all(b"<a xmlns='urn:xmpp:sm:3' h='1'/><message type='chat' from='alice@example'><body>barrier</body></message>").await.unwrap();
            assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(),
                Some(XmppEvent::DirectMessage { body, .. }) if body == "barrier"));
            assert!(!sending.is_finished());
            server.write_all(b"<a xmlns='urn:xmpp:sm:3' h='2'/>").await.unwrap();
            let (client, result) = tokio::time::timeout(Duration::from_secs(1), sending).await.unwrap().unwrap();
            assert_eq!(result.unwrap(), SendConfirmation::StreamAcknowledged);
            client.close().await;
        }
    }

    #[tokio::test]
    async fn equivalent_room_addresses_preserve_membership_events_and_leave_context() {
        use tokio::io::AsyncWriteExt;
        async fn next(events: &mut mpsc::Receiver<XmppEvent>, key: &str, context: &str) -> XmppEvent {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap().unwrap() {
                XmppEvent::RoomEvent { room, context: actual, event } => {
                    assert_eq!(room, key);
                    assert_eq!(actual, context);
                    *event
                }
                event => panic!("expected contextual room event, got {event:?}"),
            }
        }
        for (key, peer) in [("room@EXAMPLE", "room@example"),
            ("room@example", "room@EXAMPLE"),
            ("room@[2001:db8::1]", "room@[2001:0db8:0:0:0:0:0:1]")] {
            let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
            assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
            client.join_room_with_context(key, "me", "entry").await.unwrap();
            server.write_all(format!("<message type='groupchat' from='{peer}/alice'><subject>topic</subject></message><message type='groupchat' from='{peer}/alice'><body>buffered</body></message><presence from='{peer}/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").as_bytes()).await.unwrap();
            assert!(matches!(next(&mut events, key, "entry").await, XmppEvent::RoomJoined { room, .. } if room == key));
            assert!(matches!(next(&mut events, key, "entry").await, XmppEvent::RoomSubject { room, subject } if room == key && subject == "topic"));
            assert!(matches!(next(&mut events, key, "entry").await, XmppEvent::RoomMessage { room, body, .. } if room == key && body == "buffered"));
            {
                let writer = client.writer.clone();
                let _blocked = writer.lock().await;
                assert!(tokio::time::timeout(Duration::from_millis(20), client.join_room_with_context(peer, "me", "cancelled")).await.is_err());
                let rooms = client.rooms.lock().unwrap();
                assert_eq!(rooms.len(), 1);
                assert!(rooms[key].phase == rooms::Phase::Joined);
                assert_eq!(rooms[key].context.as_deref(), Some("entry"));
            }
            client.join_room_with_context(peer, "me", "replacement").await.unwrap();
            assert_eq!(client.rooms.lock().unwrap().len(), 1);
            server.write_all(format!("<presence from='{peer}/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").as_bytes()).await.unwrap();
            assert!(matches!(next(&mut events, key, "replacement").await, XmppEvent::RoomJoined { room, .. } if room == key));
            assert_eq!(client.request_room_leave(peer, "me", Some("departure")).await.unwrap(), RoomLeaveOutcome::Requested);
            let needle = format!("to='{key}/me'");
            read_until_written(&mut server, needle.as_bytes()).await;
            server.write_all(format!("<presence type='unavailable' from='{peer}/me'><x xmlns='http://jabber.org/protocol/muc#user'><item nick='renamed'/><status code='110'/><status code='303'/></x></presence>").as_bytes()).await.unwrap();
            let needle = format!("to='{key}/renamed'");
            read_until_written(&mut server, needle.as_bytes()).await;
            server.write_all(format!("<message type='groupchat' from='{peer}/alice'><body>final</body></message><presence type='unavailable' from='{peer}/renamed'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").as_bytes()).await.unwrap();
            assert!(matches!(next(&mut events, key, "departure").await, XmppEvent::RoomMessage { room, body, .. } if room == key && body == "final"));
            assert!(matches!(next(&mut events, key, "departure").await, XmppEvent::RoomLeft(room) if room == key));
            assert!(client.rooms.lock().unwrap().is_empty());
            client.join_room_with_context(key, "me", "retry").await.unwrap();
            server.write_all(format!("<presence type='error' from='{peer}/other'><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></presence><presence type='error' from='{peer}/me'><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></presence>").as_bytes()).await.unwrap();
            assert!(matches!(next(&mut events, key, "retry").await, XmppEvent::PresenceError { from, .. } if from == format!("{key}/me")));
            assert!(client.rooms.lock().unwrap().is_empty());
            client.close().await;
        }
    }

    #[tokio::test]
    async fn unconfirmed_room_messages_survive_failure_departure_and_replacement() {
        use tokio::io::AsyncWriteExt;
        for end in ["error", "unavailable", "timeout", "eof", "leave", "replace", "cancelled leave", "cancelled join"] {
            let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
            assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
            client.join_room_with_context("room@example", "me", "original").await.unwrap();
            server.write_all(b"<message type='groupchat' from='room@example/alice'><body>one</body></message><message type='groupchat' from='room@example/bob'><body>two</body></message><message type='chat' from='alice@example'><body>barrier</body></message>").await.unwrap();
            assert!(matches!(events.recv().await, Some(XmppEvent::DirectMessage { .. })));
            let mut server = Some(server);
            match end {
                "error" => server.as_mut().unwrap().write_all(b"<presence type='error' from='room@example/me'><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></presence>").await.unwrap(),
                "unavailable" => server.as_mut().unwrap().write_all(b"<presence type='unavailable' from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap(),
                "timeout" => {
                    client.rooms.lock().unwrap().get_mut("room@example").unwrap().deadline = Some(tokio::time::Instant::now());
                    client.pending_iq_changed.notify_one();
                }
                "eof" => { drop(server.take()); }
                "leave" => client.leave_room_with_context("room@example", "me", "departure").await.unwrap(),
                "replace" => client.join_room_with_context("room@example", "me", "replacement").await.unwrap(),
                _ => {
                    let writer = client.writer.clone();
                    let _blocked = writer.lock().await;
                    let result = if end == "cancelled leave" {
                        tokio::time::timeout(Duration::from_millis(20), client.leave_room_with_context("room@example", "me", "cancelled")).await
                    } else {
                        tokio::time::timeout(Duration::from_millis(20), client.join_room_with_context("room@example", "me", "cancelled")).await
                    };
                    assert!(result.is_err());
                }
            }
            if end.starts_with("cancelled") {
                assert_eq!(client.rooms.lock().unwrap()["room@example"].context.as_deref(), Some("original"));
            }
            if end == "cancelled join" {
                // The queued replacement never owned this entry. Its original
                // buffer stays pending until the original join is confirmed.
                server.as_mut().unwrap().write_all(b"<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
                assert!(matches!(take_context_event(&mut events, "original").await, XmppEvent::RoomJoined { .. }));
            }
            for expected in ["one", "two"] {
                assert!(matches!(take_context_event(&mut events, "original").await, XmppEvent::RoomMessage { body, .. } if body == expected), "{end}");
            }
            match end {
                "error" => assert!(matches!(take_context_event(&mut events, "original").await, XmppEvent::PresenceError { .. })),
                "unavailable" | "timeout" => assert!(matches!(take_context_event(&mut events, "original").await, XmppEvent::RoomJoinFailed { .. })),
                "leave" => {
                    server.as_mut().unwrap().write_all(b"<presence type='unavailable' from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
                    assert!(matches!(take_context_event(&mut events, "departure").await, XmppEvent::RoomLeft(_)));
                }
                "replace" => {
                    server.as_mut().unwrap().write_all(b"<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
                    assert!(matches!(take_context_event(&mut events, "replacement").await, XmppEvent::RoomJoined { .. }));
                }
                "cancelled leave" => {
                    server.as_mut().unwrap().write_all(b"<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
                    assert!(matches!(take_context_event(&mut events, "original").await, XmppEvent::RoomJoined { .. }));
                }
                _ => {},
            }
            if let Some(server) = server.as_mut() { server.write_all(b"</stream:stream>").await.unwrap(); }
            client.close().await;
        }
    }

    #[tokio::test]
    async fn cancelling_a_room_write_preserves_messages_received_during_the_attempt() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.join_room_with_context("room@example", "me", "original").await.unwrap();
        {
            let writer = client.writer.clone();
            let _blocked = writer.lock().await;
            let changing = client.join_room_with_context("room@example", "me", "cancelled");
            tokio::pin!(changing);
            tokio::select! {
                biased;
                result = &mut changing => panic!("write completed while blocked: {result:?}"),
                _ = std::future::ready(()) => {},
            }
            server.write_all(b"<message type='groupchat' from='room@example/alice'><body>during write</body></message><message type='chat' from='alice@example'><body>barrier</body></message>").await.unwrap();
            assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { .. })));
            // The queued replacement never retires the preceding entry.
        }
        assert_eq!(client.rooms.lock().unwrap()["room@example"].context.as_deref(), Some("original"));
        server.write_all(b"<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence><message type='chat' from='alice@example'><body>done</body></message></stream:stream>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "original").await, XmppEvent::RoomJoined { .. }));
        assert!(matches!(take_context_event(&mut events, "original").await, XmppEvent::RoomMessage { body, .. } if body == "during write"));
        assert!(matches!(events.recv().await, Some(XmppEvent::DirectMessage { .. })));
        client.close().await;
        assert!(events.recv().await.is_none());
    }

    #[tokio::test]
    async fn closing_flushes_unconfirmed_room_messages_before_the_peer_replies() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.join_room_with_context("room@example", "me", "original").await.unwrap();
        server.write_all(b"<message type='groupchat' from='room@example/alice'><body>before close</body></message><message type='chat' from='alice@example'><body>barrier</body></message>").await.unwrap();
        assert!(matches!(events.recv().await, Some(XmppEvent::DirectMessage { .. })));
        let closing = tokio::spawn(client.close());
        read_until_written(&mut server, b"</stream:stream>").await;
        assert!(matches!(take_context_event(&mut events, "original").await, XmppEvent::RoomMessage { body, .. } if body == "before close"));
        assert!(!closing.is_finished());
        server.write_all(b"<message type='groupchat' from='room@example/alice'><body>during close</body></message>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "original").await, XmppEvent::RoomMessage { body, .. } if body == "during close"));
        assert!(!closing.is_finished());
        server.write_all(b"</stream:stream>").await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), closing).await.unwrap().unwrap();
        assert!(events.recv().await.is_none());
    }

    #[tokio::test]
    async fn eof_marks_the_session_closed_before_a_backpressured_room_buffer_drain() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.join_room_with_context("room@example", "me", "original").await.unwrap();
        let mut wire = "<message type='chat' from='alice@example'><body>fill</body></message>".repeat(32);
        wire.push_str("<message type='groupchat' from='room@example/alice'><body>retained</body></message>");
        server.write_all(wire.as_bytes()).await.unwrap();
        server.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !client.sm_state.closed.load(Ordering::SeqCst) { tokio::task::yield_now().await; }
        }).await.expect("EOF must invalidate the session even while final data is backpressured");
        for _ in 0..32 { assert!(matches!(events.recv().await, Some(XmppEvent::DirectMessage { .. }))); }
        assert!(matches!(take_context_event(&mut events, "original").await, XmppEvent::RoomMessage { body, .. } if body == "retained"));
        client.close().await;
        assert!(events.recv().await.is_none());
    }

    #[tokio::test]
    async fn room_requests_fence_unsolicited_traffic_and_reconcile_the_initial_roster() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        server.write_all(b"<presence from='unrequested@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence><message type='groupchat' from='unrequested@example/other'><body>unsolicited</body></message><message type='chat' from='alice@example'><body>barrier</body></message>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { .. })));
        assert!(client.rooms.lock().unwrap().is_empty());
        client.join_room("room@example", "me").await.unwrap();
        let written = read_until_written(&mut server, b"protocol/muc'/></presence>").await;
        let id = client.rooms.lock().unwrap()["room@example"].request_id.clone();
        assert!(String::from_utf8(written).unwrap().contains(&format!("id='{id}'")));
        server.write_all(b"<presence from='room@example/alice'><x xmlns='http://jabber.org/protocol/muc#user'><item role='participant'/></x></presence><presence from='room@example/alice'><x xmlns='http://jabber.org/protocol/muc#user'><item role='moderator'/></x></presence><presence from='room@example/bob'><x xmlns='http://jabber.org/protocol/muc#user'><item/></x></presence><presence type='unavailable' from='room@example/bob'><x xmlns='http://jabber.org/protocol/muc#user'><item/></x></presence><message type='groupchat' from='room@example/alice'><body>buffered</body><origin-id xmlns='urn:xmpp:sid:0' id='buffered-origin'/></message><presence from='room@example/other'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence><presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
        match tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap() {
            Some(XmppEvent::RoomJoined { room, members, .. }) => {
                assert_eq!(room, "room@example");
                assert_eq!(members.len(), 2);
                assert_eq!(members[0].nick, "alice");
                assert_eq!(members[0].role, "moderator");
                assert_eq!(members[1].nick, "me");
            }
            event => panic!("expected room entry, got {event:?}"),
        }
        assert!(matches!(events.recv().await, Some(XmppEvent::RoomMessage { body, origin_id: Some(origin), .. }) if body == "buffered" && origin == "buffered-origin"));
        client.leave_room("room@example", "me").await.unwrap();
        server.write_all(b"<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence><message type='groupchat' from='room@example/alice'><body>final message</body></message><presence type='unavailable' from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence><message type='groupchat' from='room@example/alice'><body>after departure</body></message><presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence><message type='chat' from='alice@example'><body>barrier</body></message>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::RoomMessage { body, .. }) if body == "final message"));
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::RoomLeft(room)) if room == "room@example"));
        assert!(matches!(events.recv().await, Some(XmppEvent::DirectMessage { .. })));
        assert!(client.rooms.lock().unwrap().is_empty());
        client.close().await;
    }

    #[tokio::test]
    async fn room_entry_accepts_multi_session_presence_and_releases_buffered_messages() {
        use tokio::io::AsyncWriteExt;
        for sm_enabled in [false, true] {
            let (mut client, mut events, mut server) = test_connection_with_sm(XmlFramer::new_client_stream(), sm_enabled).await;
            assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
            for room in ["first@example", "second@example"] {
                client.join_room_with_context(room, "me", room).await.unwrap();
            }
            read_until_written(&mut server, b"second@example/me").await;
            for room in ["first@example", "second@example"] {
                let xml = format!("<presence from='{room}/alice'><x xmlns='http://jabber.org/protocol/muc#user'>\
                    <item affiliation='member' role='participant' jid='alice@example/phone'/>\
                    <item affiliation='member' role='participant' jid='alice@example/desktop'/></x></presence>\
                    <message type='groupchat' from='{room}/alice'><body>before confirmation</body></message>\
                    <presence from='{room}/me'><x xmlns='http://jabber.org/protocol/muc#user'>\
                    <item affiliation='member' role='participant' jid='me@example/phone'/>\
                    <item affiliation='member' role='participant' jid='me@example/test'/><status code='110'/></x></presence>");
                server.write_all(xml.as_bytes()).await.unwrap();
            }
            server.write_all(b"<message type='chat' from='contact@example'><body>barrier</body></message>").await.unwrap();
            let mut room_events = Vec::new();
            loop {
                match tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap().unwrap() {
                    XmppEvent::DirectMessage { body, .. } if body == "barrier" => break,
                    XmppEvent::RoomEvent { room, context, event } => { assert_eq!(room, context); room_events.push((room, *event)); },
                    event => panic!("unexpected event: {event:?}"),
                }
            }
            assert_eq!(room_events.iter().filter(|(_, event)| matches!(event, XmppEvent::RoomJoined { .. })).count(), 2,
                "multi-item presence must confirm both room entries instead of timing out: {room_events:?}");
            assert_eq!(room_events.len(), 4);
            for (index, room) in ["first@example", "second@example"].into_iter().enumerate() {
                assert_eq!(room_events[index * 2].0, room);
                assert!(matches!(&room_events[index * 2].1, XmppEvent::RoomJoined { own_nick, members, .. }
                    if own_nick == "me" && members.len() == 2 && members[0].nick == "alice" && members[1].nick == "me"));
                assert!(matches!(&room_events[index * 2 + 1].1, XmppEvent::RoomMessage { body, .. } if body == "before confirmation"));
                assert!(client.rooms.lock().unwrap()[room].phase == rooms::Phase::Joined);
            }
            client.close().await;
        }
    }

    #[tokio::test]
    async fn queued_replacement_join_does_not_take_ownership_before_it_can_write() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for already_joined in [false, true] {
            let (mut client, mut events, mut server) = test_connection_with_sm(XmlFramer::new_client_stream(), true).await;
            assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
            client.join_room_with_context("room@example", "me", "entry").await.unwrap();
            read_until_written(&mut server, b"protocol/muc'/></presence>").await;
            if already_joined {
                server.write_all(b"<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
                assert!(matches!(take_context_event(&mut events, "entry").await, XmppEvent::RoomJoined { .. }));
            }
            let sent_before = client.sm_state.outbound_count();
            let writer = client.writer.clone();
            let blocked = writer.lock().await;
            {
                let replacement = client.join_room_with_context("room@example", "other", "replacement");
                tokio::pin!(replacement);
                tokio::select! {
                    biased;
                    result = &mut replacement => panic!("replacement bypassed held writer: {result:?}"),
                    _ = std::future::ready(()) => {},
                }
                server.write_all(b"<presence from='room@example/alice'><x xmlns='http://jabber.org/protocol/muc#user'><item/></x></presence><message type='groupchat' from='room@example'><subject>current topic</subject></message><presence from='room@example/me'><show>away</show><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence><message type='chat' from='contact@example'><body>barrier</body></message>").await.unwrap();
                let mut original_updates = Vec::new();
                loop {
                    match tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap().unwrap() {
                        XmppEvent::DirectMessage { body, .. } if body == "barrier" => break,
                        XmppEvent::RoomEvent { context, event, .. } => { assert_eq!(context, "entry"); original_updates.push(*event); },
                        other => panic!("unexpected event: {other:?}"),
                    }
                }
                assert!(original_updates.iter().any(|event| matches!(event, XmppEvent::RoomSubject { subject, .. } if subject == "current topic")), "queued replacement stole the original entry's updates: {original_updates:?}");
                if already_joined {
                    assert!(original_updates.iter().any(|event| matches!(event, XmppEvent::MemberJoined { member, .. } if member.nick == "me" && member.show.as_deref() == Some("away"))));
                } else {
                    assert!(original_updates.iter().any(|event| matches!(event, XmppEvent::RoomJoined { members, .. } if members.len() == 2)));
                }
            }
            drop(blocked);
            {
                let rooms = client.rooms.lock().unwrap();
                let room = &rooms["room@example"];
                assert!(room.phase == rooms::Phase::Joined);
                assert_eq!(room.context.as_deref(), Some("entry"));
                assert_eq!(room.nick, "me");
                assert!(room.members.snapshot().iter().any(|member| member.nick == "me" && member.show.as_deref() == Some("away")));
            }
            assert_eq!(client.sm_state.outbound_count(), sent_before, "cancelled replacement must not count an unsent stanza");
            let mut byte = [0];
            assert!(tokio::time::timeout(Duration::from_millis(20), server.read(&mut byte)).await.is_err());
            server.write_all(b"<message type='groupchat' from='room@example/alice'><body>still joined</body></message>").await.unwrap();
            assert!(matches!(take_context_event(&mut events, "entry").await, XmppEvent::RoomMessage { body, .. } if body == "still joined"));
            client.close().await;
        }
    }

    #[tokio::test]
    async fn cancelled_room_writes_and_failed_entry_do_not_leave_phantom_requests() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        {
            let writer = client.writer.clone();
            let _blocked = writer.lock().await;
            assert!(tokio::time::timeout(Duration::from_millis(20), client.join_room("cancelled@example", "me")).await.is_err());
            assert!(client.rooms.lock().unwrap().is_empty());
        }
        client.join_room("room@example", "me").await.unwrap();
        let id = client.rooms.lock().unwrap()["room@example"].request_id.clone();
        {
            let writer = client.writer.clone();
            let _blocked = writer.lock().await;
            assert!(tokio::time::timeout(Duration::from_millis(20), client.leave_room("room@example", "me")).await.is_err());
            let rooms = client.rooms.lock().unwrap();
            assert_eq!(rooms["room@example"].request_id, id);
            assert!(rooms["room@example"].phase == rooms::Phase::Joining);
            assert!(rooms["room@example"].deadline.is_some());
        }
        let forged = format!("<presence type='error' from='room@example/other' id='{id}'><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></presence><presence type='error' from='room@example/me' id='old-request'><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></presence><message type='chat' from='alice@example'><body>barrier</body></message>");
        server.write_all(forged.as_bytes()).await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { .. })));
        assert!(client.rooms.lock().unwrap().contains_key("room@example"));
        server.write_all(b"<presence type='unavailable' from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::RoomJoinFailed { room, .. }) if room == "room@example"));
        assert!(client.rooms.lock().unwrap().is_empty());
        client.close().await;
    }

    #[tokio::test]
    async fn room_entry_deadlines_expire_and_reader_exit_cleans_the_registry() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.join_room("room@example", "me").await.unwrap();
        {
            let mut rooms = client.rooms.lock().unwrap();
            assert!(rooms["room@example"].deadline.is_some());
            rooms.get_mut("room@example").unwrap().deadline = Some(tokio::time::Instant::now());
        }
        client.pending_iq_changed.notify_one();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::RoomJoinFailed { room, .. }) if room == "room@example"));
        let written = read_until_written(&mut server, b"to='room@example/me'/>").await;
        assert!(String::from_utf8(written).unwrap().contains("to='room@example/me'"));
        server.write_all(b"<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence><message type='chat' from='alice@example'><body>barrier</body></message>").await.unwrap();
        assert!(matches!(events.recv().await, Some(XmppEvent::DirectMessage { .. })));
        assert!(client.rooms.lock().unwrap().is_empty());
        client.join_room("next@example", "me").await.unwrap();
        drop(server);
        assert!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap().is_none());
        assert!(client.rooms.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn server_assigned_room_nicknames_are_used_when_leaving() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.join_room("room@example", "requested").await.unwrap();
        server.write_all(b"<presence from='room@example/assigned'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/><status code='210'/></x></presence>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::RoomJoined { own_nick, .. }) if own_nick == "assigned"));
        assert!(client.rooms.lock().unwrap()["room@example"].deadline.is_none());
        server.write_all(b"<presence type='unavailable' from='room@example/assigned'><x xmlns='http://jabber.org/protocol/muc#user'><item nick='renamed'/><status code='110'/><status code='303'/></x></presence><presence from='room@example/renamed'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
        assert!(matches!(events.recv().await, Some(XmppEvent::MemberNicknameChanged { old_nick, new_nick, is_self: true, .. }) if old_nick == "assigned" && new_nick == "renamed"));
        assert!(matches!(events.recv().await, Some(XmppEvent::MemberJoined { member, .. }) if member.nick == "renamed"));
        assert!(client.rooms.lock().unwrap()["room@example"].phase == rooms::Phase::Joined);
        client.leave_room("room@example", "requested").await.unwrap();
        let written = read_until_written(&mut server, b"to='room@example/renamed'").await;
        assert!(String::from_utf8(written).unwrap().contains("to='room@example/renamed'"));
        client.close().await;
    }

    #[tokio::test]
    async fn losing_the_socket_before_confirmation_is_not_send_success()
    {
        let (mut client, _events, mut server) = test_connection_with_sm(XmlFramer::new_client_stream(), true).await;
        let sending = tokio::spawn(async move {
            let result = client.send_message_confirmed("alice@example", "payload", None, Duration::from_secs(2)).await;
            (client, result)
        });
        read_until_written(&mut server, b"<r xmlns=").await;
        drop(server);
        let (client, result) = tokio::time::timeout(Duration::from_secs(1), sending).await.unwrap().unwrap();
        assert!(matches!(result, Err(XmppError::Disconnected)));
        client.close().await;
    }

    #[tokio::test]
    async fn legacy_confirmed_sends_require_a_correlated_server_round_trip()
    {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        let sending = tokio::spawn(async move {
            let result = client.send_message_confirmed("alice@example", "payload", None, Duration::from_secs(2)).await;
            (client, result)
        });
        let bytes = read_until_written(&mut server, b"<ping xmlns=").await;
        let xml = String::from_utf8(bytes).unwrap();
        assert!(xml.find("<body>payload</body>").unwrap() < xml.find("<ping xmlns=").unwrap());
        server.write_all(b"<iq type='result' id='ping_1' from='mallory@example/phone'/><message type='chat' from='alice@example'><body>barrier</body></message>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(),
            Some(XmppEvent::DirectMessage { body, .. }) if body == "barrier"));
        assert!(!sending.is_finished());
        // A server that does not implement ping still answers with an error.
        server.write_all(b"<iq type='error' id='ping_1' from='example'><error type='cancel'><service-unavailable xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></iq>").await.unwrap();
        let (client, result) = tokio::time::timeout(Duration::from_secs(1), sending).await.unwrap().unwrap();
        assert_eq!(result.unwrap(), SendConfirmation::ServerRoundTrip);
        client.close().await;
    }

    #[tokio::test]
    async fn confirmed_send_deadlines_bound_writes_and_response_waits()
    {
        for managed in [false, true] {
            let (mut client, _events, _server) = test_connection_with_sm(XmlFramer::new_client_stream(), managed).await;
            let writer = client.writer.clone();
            let guard = writer.lock().await;
            for invalid_id in ["", "bad\0id"] {
                assert!(matches!(client.send_message_confirmed("alice@example", "payload", Some(invalid_id), Duration::from_millis(20)).await,
                    Err(XmppError::InvalidInput(_))));
            }
            let result = tokio::time::timeout(Duration::from_secs(1),
                client.send_message_confirmed("alice@example", "blocked", None, Duration::from_millis(20))).await.unwrap();
            assert!(matches!(result, Err(XmppError::Timeout(_))));
            drop(guard);
            let result = tokio::time::timeout(Duration::from_secs(1),
                client.send_message_confirmed("alice@example", "unanswered", None, Duration::from_millis(20))).await.unwrap();
            assert!(matches!(result, Err(XmppError::Timeout(_))));
            assert!(client.pending_iqs.lock().unwrap().is_empty());
            client.close().await;
        }
    }

    #[test]
    fn message_validation_preserves_unicode_and_normal_whitespace()
    {
        assert!(validate_message("alice@example/phone", "\tHello 😀\n<&>\råäö").is_ok());
        for text in ["bad\0text", "bad\u{0008}text", "\u{000b}", "\u{fffe}", "\u{ffff}"] {
            assert!(matches!(validate_message_text(text), Err(XmppError::InvalidInput(_))));
        }
        assert!(matches!(validate_message("", "hello"), Err(XmppError::InvalidInput(_))));
        assert!(matches!(validate_message("bad\0recipient", "hello"), Err(XmppError::InvalidInput(_))));
        assert_eq!(clean_message_text("a\0b\u{0001}c\t\n\r😀"), "abc\t\n\r😀");
    }

    #[tokio::test]
    async fn absent_and_repeated_departures_do_not_create_phantom_or_duplicate_requests() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        read_until_written(&mut server, b"</presence>").await;
        assert_eq!(client.request_room_leave("room@example", "me", Some("skipped-entry")).await.unwrap(), RoomLeaveOutcome::AlreadyAbsent);
        assert!(client.rooms.lock().unwrap().is_empty());
        client.leave_room("room@example", "me").await.unwrap();
        let mut byte = [0];
        assert!(tokio::time::timeout(Duration::from_millis(20), server.read(&mut byte)).await.is_err());
        tokio::time::timeout(Duration::from_secs(1), client.join_room_with_context("room@example", "me", "entry")).await.unwrap().unwrap();
        read_until_written(&mut server, b"protocol/muc'/></presence>").await;
        server.write_all(b"<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "entry").await, XmppEvent::RoomJoined { .. }));
        assert_eq!(client.request_room_leave("room@example", "me", Some("departure-1")).await.unwrap(), RoomLeaveOutcome::Requested);
        read_until_written(&mut server, b"/>").await;
        let id = client.rooms.lock().unwrap()["room@example"].request_id.clone();
        assert_eq!(client.request_room_leave("room@example", "me", Some("departure-2")).await.unwrap(), RoomLeaveOutcome::Requested);
        assert_eq!(client.rooms.lock().unwrap()["room@example"].request_id, id);
        assert!(tokio::time::timeout(Duration::from_millis(20), server.read(&mut byte)).await.is_err());
        server.write_all(b"<presence type='unavailable' from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "departure-2").await, XmppEvent::RoomLeft(_)));
        assert_eq!(client.request_room_leave("room@example", "me", Some("already-left")).await.unwrap(), RoomLeaveOutcome::AlreadyAbsent);
        tokio::time::timeout(Duration::from_secs(1), client.join_room_with_context("room@example", "me", "replacement")).await.unwrap().unwrap();
        client.close().await;
    }

    #[tokio::test]
    async fn rapid_rejoin_waits_for_the_previous_departure_before_writing() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut client, mut events, mut server) = joined_room_for_leave_test().await;
        client.leave_room_with_context("room@example", "old", "departure").await.unwrap();
        read_until_written(&mut server, b"/>").await;
        let rooms = client.rooms.clone();
        let rejoining = tokio::spawn(async move {
            let result = client.join_room_with_context("room@example", "new", "replacement").await;
            (client, result)
        });
        let mut byte = [0];
        assert!(tokio::time::timeout(Duration::from_millis(30), server.read(&mut byte)).await.is_err());
        assert!(!rejoining.is_finished());
        assert!(rooms.lock().unwrap()["room@example"].phase == rooms::Phase::Leaving);
        assert_eq!(rooms.lock().unwrap()["room@example"].context.as_deref(), Some("departure"));
        server.write_all(b"<presence type='unavailable' from='room@example/old'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "departure").await, XmppEvent::RoomLeft(_)));
        let (client, result) = tokio::time::timeout(Duration::from_secs(1), rejoining).await.unwrap().unwrap();
        result.unwrap();
        let wire = String::from_utf8(read_until_written(&mut server, b"protocol/muc'/></presence>").await).unwrap();
        assert!(wire.contains("to='room@example/new'"));
        server.write_all(b"<presence from='room@example/new'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "replacement").await, XmppEvent::RoomJoined { own_nick, .. } if own_nick == "new"));
        assert!(client.rooms.lock().unwrap()["room@example"].phase == rooms::Phase::Joined);
        client.close().await;
    }

    #[tokio::test]
    async fn cancelled_rejoin_waits_leave_departure_intact_and_eof_wakes_the_next_waiter() {
        let (mut client, mut events, mut server) = joined_room_for_leave_test().await;
        client.leave_room_with_context("room@example", "old", "departure").await.unwrap();
        read_until_written(&mut server, b"/>").await;
        let id = client.rooms.lock().unwrap()["room@example"].request_id.clone();
        assert!(tokio::time::timeout(Duration::from_millis(20), client.join_room_with_context("room@example", "new", "cancelled")).await.is_err());
        assert_eq!(client.rooms.lock().unwrap()["room@example"].request_id, id);
        assert_eq!(client.rooms.lock().unwrap()["room@example"].context.as_deref(), Some("departure"));
        let rejoining = tokio::spawn(async move {
            let result = client.join_room_with_context("room@example", "new", "next").await;
            (client, result)
        });
        drop(server);
        let (client, result) = tokio::time::timeout(Duration::from_secs(1), rejoining).await.unwrap().unwrap();
        assert!(matches!(result, Err(XmppError::Disconnected)));
        assert!(client.rooms.lock().unwrap().is_empty());
        assert!(events.recv().await.is_none());
    }

    #[tokio::test]
    async fn queued_room_events_keep_their_original_context_across_leave_and_rejoin() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection_with_sm(XmlFramer::new_client_stream(), true).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.join_room_with_context("room@example", "first", "entry-A").await.unwrap();
        let wire = read_until_written(&mut server, b"protocol/muc'/></presence>").await;
        assert!(!String::from_utf8(wire).unwrap().contains("entry-A"));
        // Include backlog before the self presence: buffered data must retain
        // the same context as the room entry when flushed after confirmation.
        server.write_all(b"<message type='groupchat' from='room@example/alice'><subject>topic</subject></message><message type='groupchat' from='room@example/alice'><body>old message</body></message><presence from='room@example/first'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence><message type='chat' from='alice@example'><body>barrier</body></message>").await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while client.sm_state.inbound_count() < 4 { tokio::task::yield_now().await; }
        }).await.unwrap();
        client.leave_room_with_context("room@example", "first", "departure-B").await.unwrap();
        server.write_all(b"<message type='groupchat' from='room@example/alice'><body>during departure</body></message><presence type='unavailable' from='room@example/first'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while client.rooms.lock().unwrap().contains_key("room@example") { tokio::task::yield_now().await; }
        }).await.unwrap();
        client.join_room_with_context("room@example", "second", "entry-C").await.unwrap();
        let old_join = take_context_event(&mut events, "entry-A").await;
        assert!(matches!(old_join, XmppEvent::RoomJoined { own_nick, .. } if own_nick == "first"));
        assert!(matches!(take_context_event(&mut events, "entry-A").await, XmppEvent::RoomSubject { .. }));
        assert!(matches!(take_context_event(&mut events, "entry-A").await, XmppEvent::RoomMessage { body, .. } if body == "old message"));
        assert!(matches!(events.recv().await, Some(XmppEvent::DirectMessage { .. })));
        assert!(matches!(take_context_event(&mut events, "departure-B").await, XmppEvent::RoomMessage { body, .. } if body == "during departure"));
        assert!(matches!(take_context_event(&mut events, "departure-B").await, XmppEvent::RoomLeft(_)));
        assert_eq!(client.rooms.lock().unwrap()["room@example"].context.as_deref(), Some("entry-C"));
        server.write_all(b"<presence from='room@example/second'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence><message type='groupchat' from='room@example/alice'><body>new message</body></message>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "entry-C").await, XmppEvent::RoomJoined { .. }));
        assert!(matches!(take_context_event(&mut events, "entry-C").await, XmppEvent::RoomMessage { body, .. } if body == "new message"));
        // The ordinary leave API inherits an existing contextual entry.
        client.leave_room("room@example", "second").await.unwrap();
        server.write_all(b"<presence type='unavailable' from='room@example/second'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "entry-C").await, XmppEvent::RoomLeft(_)));
        client.close().await;
    }

    #[tokio::test]
    async fn room_errors_timeouts_and_cancelled_writes_preserve_operation_context() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        for context in [String::new(), "x".repeat(129)] {
            assert!(matches!(client.join_room_with_context("room@example", "me", &context).await, Err(XmppError::InvalidInput(_))));
            assert!(matches!(client.leave_room_with_context("room@example", "me", &context).await, Err(XmppError::InvalidInput(_))));
        }
        assert!(client.rooms.lock().unwrap().is_empty());
        client.join_room_with_context("room@example", "me", "original").await.unwrap();
        {
            let writer = client.writer.clone();
            let _blocked = writer.lock().await;
            assert!(tokio::time::timeout(Duration::from_millis(20), client.leave_room_with_context("room@example", "me", "cancelled-leave")).await.is_err());
            assert!(tokio::time::timeout(Duration::from_millis(20), client.join_room_with_context("room@example", "other", "cancelled-rejoin")).await.is_err());
            assert_eq!(client.rooms.lock().unwrap()["room@example"].context.as_deref(), Some("original"));
        }
        server.write_all(b"<presence type='error' from='room@example/me'><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></presence>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "original").await, XmppEvent::PresenceError { .. }));
        client.join_room_with_context("room@example", "me", "timed-entry").await.unwrap();
        client.rooms.lock().unwrap().get_mut("room@example").unwrap().deadline = Some(tokio::time::Instant::now());
        client.pending_iq_changed.notify_one();
        assert!(matches!(take_context_event(&mut events, "timed-entry").await, XmppEvent::RoomJoinFailed { .. }));
        client.close().await;
    }

    async fn take_context_event(events: &mut mpsc::Receiver<XmppEvent>, expected: &str) -> XmppEvent {
        match tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap() {
            Some(XmppEvent::RoomEvent { room, context, event }) => {
                assert_eq!(room, "room@example");
                assert_eq!(context, expected);
                *event
            }
            event => panic!("expected room event for {expected}, got {event:?}"),
        }
    }

    #[tokio::test]
    async fn nickname_change_while_leave_is_waiting_updates_the_unsent_target() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut client, mut events, mut server) = joined_room_for_leave_test().await;
        let writer = client.writer.clone();
        let rooms = client.rooms.clone();
        let blocked = writer.lock().await;
        let departure = tokio::spawn(async move {
            let result = client.leave_room("room@example", "old").await;
            (client, result)
        });
        tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                if rooms.lock().unwrap()["room@example"].phase == rooms::Phase::Leaving { break; }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        server.write_all(b"<presence type='unavailable' from='room@example/old'><x xmlns='http://jabber.org/protocol/muc#user'><item nick='new'/><status code='110'/><status code='303'/></x></presence>").await.unwrap();
        tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                if rooms.lock().unwrap()["room@example"].nick == "new" { break; }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        drop(blocked);
        let (client, result) = tokio::time::timeout(Duration::from_secs(1), departure).await.unwrap().unwrap();
        result.unwrap();
        server.write_all(b"<message type='chat' from='alice@example'><body>barrier</body></message>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { .. })));
        let wire = String::from_utf8(read_until_written(&mut server, b"/>").await).unwrap();
        assert!(wire.contains("to='room@example/new'"));
        assert!(!wire.contains("to='room@example/old'"));
        assert_eq!(wire.matches("<presence").count(), 1);
        let mut byte = [0];
        assert!(tokio::time::timeout(Duration::from_millis(20), server.read(&mut byte)).await.is_err());
        assert_eq!(client.sm_state.outbound.load(Ordering::SeqCst), 3);
        server.write_all(b"<presence type='unavailable' from='room@example/new'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
        assert!(matches!(events.recv().await, Some(XmppEvent::RoomLeft(room)) if room == "room@example"));
        client.close().await;
    }

    #[tokio::test]
    async fn nickname_change_after_leave_was_written_reissues_it_for_the_current_nickname() {
        use tokio::io::AsyncWriteExt;
        for policy_change in [false, true] {
            let (mut client, mut events, mut server) = joined_room_for_leave_test().await;
            client.leave_room("room@example", "old").await.unwrap();
            let wire = String::from_utf8(read_until_written(&mut server, b"/>").await).unwrap();
            assert!(wire.contains("to='room@example/old'"));
            let rename = if policy_change {
                "<presence from='room@example/new'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/><status code='210'/></x></presence>"
            } else {
                "<presence type='unavailable' from='room@example/old'><x xmlns='http://jabber.org/protocol/muc#user'><item nick='new'/><status code='110'/><status code='303'/></x></presence><presence from='room@example/new'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>"
            };
            server.write_all(format!("{rename}<message type='chat' from='alice@example'><body>barrier</body></message>").as_bytes()).await.unwrap();
            assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { .. })));
            let wire = String::from_utf8(read_until_written(&mut server, b"/>").await).unwrap();
            assert!(wire.contains("to='room@example/new'"));
            assert_eq!(wire.matches("<presence").count(), 1);
            assert_eq!(client.sm_state.outbound.load(Ordering::SeqCst), 4);
            server.write_all(b"<presence type='unavailable' from='room@example/new'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
            assert!(matches!(events.recv().await, Some(XmppEvent::RoomLeft(room)) if room == "room@example"));
            assert!(client.rooms.lock().unwrap().is_empty());
            client.close().await;
        }
    }

    #[tokio::test]
    async fn blocked_nickname_departure_cleanup_ends_the_session() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = joined_room_for_leave_test().await;
        client.leave_room("room@example", "old").await.unwrap();
        read_until_written(&mut server, b"/>").await;
        let writer = client.writer.clone();
        let blocked = writer.lock().await;
        server.write_all(b"<presence type='unavailable' from='room@example/old'><x xmlns='http://jabber.org/protocol/muc#user'><item nick='new'/><status code='110'/><status code='303'/></x></presence>").await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(2), events.recv()).await.unwrap().is_none());
        assert!(client.sm_state.closed.load(Ordering::SeqCst));
        assert!(client.rooms.lock().unwrap().is_empty());
        drop(blocked);
    }

    #[tokio::test]
    async fn rejected_original_entry_retires_pending_departure_and_allows_rejoin() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (leave_written, bare_error) in [(false, false), (true, false), (true, true)] {
            let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
            assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
            client.join_room_with_context("room@example", "me", "entry").await.unwrap();
            read_until_written(&mut server, b"protocol/muc'/></presence>").await;
            let join_id = client.rooms.lock().unwrap()["room@example"].request_id.clone();
            let writer = client.writer.clone();
            let rooms = client.rooms.clone();
            let sm = client.sm_state.clone();
            let mut blocked = if leave_written { None } else { Some(writer.lock().await) };
            {
                let departure = client.leave_room_with_context("room@example", "me", "departure");
                tokio::pin!(departure);
                if leave_written {
                    (&mut departure).await.unwrap();
                    read_until_written(&mut server, b"to='room@example/me'").await;
                } else {
                    tokio::select! {
                        biased;
                        result = &mut departure => panic!("departure bypassed blocked writer: {result:?}"),
                        _ = std::future::ready(()) => {},
                    }
                }
                let waiting = rooms::wait_for_departure(&rooms, &sm, "room@example", Duration::from_secs(2));
                tokio::pin!(waiting);
                tokio::select! {
                    biased;
                    result = &mut waiting => panic!("departure finished before rejection: {result:?}"),
                    _ = std::future::ready(()) => {},
                }
                // Neither a different occupant, an unrelated ID, nor a warning
                // can prove the pending entry failed.
                server.write_all(format!("<presence type='error' from='room@example/other' id='{join_id}'><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></presence><presence type='error' from='room@example/me' id='unrelated'><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></presence><presence type='error' from='room@example/me' id='{join_id}'><error type='continue'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></presence><message type='chat' from='contact@example'><body>ignored reports</body></message>").as_bytes()).await.unwrap();
                assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { body, .. }) if body == "ignored reports"));
                let from = if bare_error { "room@example" } else { "room@example/me" };
                server.write_all(format!("<presence type='error' from='{from}' id='{join_id}'><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></presence><message type='chat' from='contact@example'><body>entry rejected</body></message>").as_bytes()).await.unwrap();
                assert!(matches!(take_context_event(&mut events, "entry").await, XmppEvent::PresenceError { condition, .. } if condition == "forbidden"));
                assert!(matches!(take_context_event(&mut events, "departure").await, XmppEvent::RoomLeft(room) if room == "room@example"));
                assert!(matches!(events.recv().await, Some(XmppEvent::DirectMessage { body, .. }) if body == "entry rejected"));
                tokio::time::timeout(Duration::from_secs(1), &mut waiting).await.unwrap().unwrap();
                drop(blocked.take());
                if !leave_written { (&mut departure).await.unwrap(); }
            }
            assert!(client.rooms.lock().unwrap().is_empty());
            if !leave_written {
                let mut byte = [0];
                assert!(tokio::time::timeout(Duration::from_millis(20), server.read(&mut byte)).await.is_err(), "retired departure wrote a stale presence");
            }
            tokio::time::timeout(Duration::from_secs(1), client.join_room_with_context("room@example", "me", "replacement")).await.unwrap().unwrap();
            read_until_written(&mut server, b"protocol/muc'/></presence>").await;
            server.write_all(b"<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
            assert!(matches!(take_context_event(&mut events, "replacement").await, XmppEvent::RoomJoined { .. }));
            client.close().await;
        }
    }

    #[tokio::test]
    async fn stale_entry_rejection_cannot_retire_a_confirmed_membership_while_leaving() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.join_room_with_context("room@example", "me", "entry").await.unwrap();
        read_until_written(&mut server, b"protocol/muc'/></presence>").await;
        let join_id = client.rooms.lock().unwrap()["room@example"].request_id.clone();
        client.leave_room_with_context("room@example", "me", "departure").await.unwrap();
        read_until_written(&mut server, b"to='room@example/me'").await;
        server.write_all(format!("<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence><presence type='error' from='room@example/me' id='{join_id}'><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></presence><message type='chat' from='contact@example'><body>confirmed membership retained</body></message>").as_bytes()).await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { .. })));
        assert!(client.rooms.lock().unwrap()["room@example"].phase == rooms::Phase::Leaving);
        server.write_all(b"<presence type='unavailable' from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "departure").await, XmppEvent::RoomLeft(_)));
        client.close().await;
    }

    #[tokio::test]
    async fn rejected_departure_during_entry_preserves_confirmation_and_roster() {
        use tokio::io::AsyncWriteExt;
        for (confirmed_while_leaving, service_subject) in [(false, false), (false, true), (true, false), (true, true)] {
            let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
            assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
            client.join_room_with_context("room@example", "me", "entry").await.unwrap();
            read_until_written(&mut server, b"protocol/muc'/></presence>").await;
            server.write_all(b"<presence from='room@example/alice'><x xmlns='http://jabber.org/protocol/muc#user'><item/></x></presence><message type='groupchat' from='room@example'><subject>pending topic</subject></message><message type='chat' from='contact@example'><body>before leave</body></message>").await.unwrap();
            assert!(matches!(events.recv().await, Some(XmppEvent::DirectMessage { .. })));
            let (join_id, deadline) = {
                let rooms = client.rooms.lock().unwrap();
                (rooms["room@example"].request_id.clone(), rooms["room@example"].deadline)
            };
            client.leave_room_with_context("room@example", "me", "departure").await.unwrap();
            read_until_written(&mut server, b"to='room@example/me'").await;
            assert!(matches!(take_context_event(&mut events, "entry").await, XmppEvent::RoomSubject { subject, .. } if subject == "pending topic"));
            server.write_all(b"<presence from='room@example/bob'><x xmlns='http://jabber.org/protocol/muc#user'><item role='participant'/></x></presence><presence type='unavailable' from='room@example/alice'><x xmlns='http://jabber.org/protocol/muc#user'><item/></x></presence>").await.unwrap();
            let subject_from = if service_subject { "room@example" } else { "room@example/bob" };
            server.write_all(format!("<message type='groupchat' from='{subject_from}'><subject>latest topic</subject></message>").as_bytes()).await.unwrap();
            assert!(matches!(take_context_event(&mut events, "departure").await, XmppEvent::RoomSubject { subject, .. } if subject == "latest topic"));
            if confirmed_while_leaving {
                server.write_all(b"<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/><status code='100'/></x></presence>").await.unwrap();
            }
            let own_nick = if confirmed_while_leaving && service_subject { "renamed" } else { "me" };
            if own_nick != "me" {
                server.write_all(b"<presence from='room@example/renamed'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/><status code='210'/></x></presence>").await.unwrap();
            }
            server.write_all(format!("<presence type='error' from='room@example/{own_nick}'><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></presence>").as_bytes()).await.unwrap();
            assert!(matches!(take_context_event(&mut events, "departure").await, XmppEvent::PresenceError { .. }));
            if !confirmed_while_leaving {
                let rooms = client.rooms.lock().unwrap();
                let room = &rooms["room@example"];
                assert!(room.phase == rooms::Phase::Joining, "a rejected leave is not entry confirmation");
                assert_eq!(room.request_id, join_id);
                assert_eq!(room.deadline, deadline);
                assert_eq!(room.context.as_deref(), Some("entry"));
                drop(rooms);
                server.write_all(b"<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/><status code='100'/></x></presence>").await.unwrap();
            }
            match take_context_event(&mut events, "entry").await {
                XmppEvent::RoomJoined { members, own_nick: actual_nick, .. } => {
                    assert_eq!(actual_nick, own_nick);
                    assert_eq!(members.iter().map(|member| member.nick.as_str()).collect::<Vec<_>>(), ["bob", own_nick]);
                }
                event => panic!("entry confirmation missing: {event:?}"),
            }
            assert!(matches!(take_context_event(&mut events, "entry").await, XmppEvent::RoomPrivacy { privacy: RoomPrivacy { non_anonymous: Some(true), .. }, .. }));
            assert!(matches!(take_context_event(&mut events, "entry").await, XmppEvent::RoomSubject { subject, .. } if subject == "latest topic"));
            client.close().await;
        }
    }

    #[tokio::test]
    async fn cancelled_departure_resumes_the_updated_entry_without_fabricating_a_join() {
        use tokio::io::AsyncWriteExt;
        for confirmed_while_leaving in [false, true] {
            let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
            assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
            client.join_room_with_context("room@example", "me", "entry").await.unwrap();
            read_until_written(&mut server, b"protocol/muc'/></presence>").await;
            server.write_all(b"<presence from='room@example/alice'><x xmlns='http://jabber.org/protocol/muc#user'><item/></x></presence><message type='chat' from='contact@example'><body>initial roster received</body></message>").await.unwrap();
            assert!(matches!(events.recv().await, Some(XmppEvent::DirectMessage { .. })));
            let writer = client.writer.clone();
            let blocked = writer.lock().await;
            {
                let departure = client.leave_room_with_context("room@example", "me", "cancelled");
                tokio::pin!(departure);
                tokio::select! {
                    biased;
                    result = &mut departure => panic!("departure bypassed held writer: {result:?}"),
                    _ = std::future::ready(()) => {},
                }
                server.write_all(b"<presence from='room@example/bob'><x xmlns='http://jabber.org/protocol/muc#user'><item/></x></presence><presence type='unavailable' from='room@example/alice'><x xmlns='http://jabber.org/protocol/muc#user'><item/></x></presence>").await.unwrap();
                if confirmed_while_leaving {
                    server.write_all(b"<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
                }
                server.write_all(b"<message type='chat' from='contact@example'><body>updates received</body></message>").await.unwrap();
                assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { .. })));
            }
            drop(blocked);
            assert_eq!(client.rooms.lock().unwrap()["room@example"].context.as_deref(), Some("entry"));
            if !confirmed_while_leaving {
                assert!(client.rooms.lock().unwrap()["room@example"].phase == rooms::Phase::Joining);
                server.write_all(b"<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
            }
            assert!(matches!(take_context_event(&mut events, "entry").await, XmppEvent::RoomJoined { members, .. }
                if members.iter().map(|member| member.nick.as_str()).collect::<Vec<_>>() == ["bob", "me"]));
            server.write_all(b"<message type='chat' from='contact@example'><body>no extra join</body></message>").await.unwrap();
            assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { .. })));
            client.close().await;
        }
    }

    #[tokio::test]
    async fn cancelled_unsent_departure_keeps_server_nickname_and_privacy_updates() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.join_room_with_context("room@example", "old", "original").await.unwrap();
        read_until_written(&mut server, b"protocol/muc'/></presence>").await;
        server.write_all(b"<presence from='room@example/old'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/><status code='170'/></x></presence>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "original").await, XmppEvent::RoomJoined { .. }));
        assert!(matches!(take_context_event(&mut events, "original").await, XmppEvent::RoomPrivacy { .. }));
        let writer = client.writer.clone();
        let blocked = writer.lock().await;
        let client_rooms = client.rooms.clone();
        {
            let departure = client.leave_room_with_context("room@example", "old", "cancelled");
            tokio::pin!(departure);
            tokio::select! {
                biased;
                result = &mut departure => panic!("leave wrote through held writer: {result:?}"),
                _ = std::future::ready(()) => {},
            }
            server.write_all(b"<message type='groupchat' from='room@example'><x xmlns='http://jabber.org/protocol/muc#user'><status code='172'/></x></message><presence type='unavailable' from='room@example/old'><x xmlns='http://jabber.org/protocol/muc#user'><item nick='new'/><status code='110'/><status code='303'/></x></presence><message type='chat' from='alice@example'><body>barrier</body></message>").await.unwrap();
            tokio::time::timeout(Duration::from_millis(500), async {
                loop {
                    if client_rooms.lock().unwrap()["room@example"].nick == "new" { break; }
                    tokio::task::yield_now().await;
                }
            }).await.unwrap();
        }
        {
            let rooms = client.rooms.lock().unwrap();
            let room = &rooms["room@example"];
            assert_eq!(room.nick, "new");
            assert_eq!(room.privacy, RoomPrivacy { publicly_logged: Some(true), non_anonymous: Some(true) });
            assert_eq!(room.context.as_deref(), Some("original"));
            assert!(room.phase == rooms::Phase::Joined);
        }
        drop(blocked);
        assert!(matches!(take_context_event(&mut events, "original").await, XmppEvent::MemberNicknameChanged { old_nick, new_nick, is_self: true, .. } if old_nick == "old" && new_nick == "new"));
        assert!(matches!(take_context_event(&mut events, "original").await, XmppEvent::RoomPrivacy { privacy: RoomPrivacy { publicly_logged: Some(true), non_anonymous: Some(true) }, .. }));
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { .. })));
        client.leave_room("room@example", "old").await.unwrap();
        let wire = String::from_utf8(read_until_written(&mut server, b"/>").await).unwrap();
        assert!(wire.contains("to='room@example/new'"));
        assert!(!wire.contains("to='room@example/old'"));
        server.write_all(b"<presence type='unavailable' from='room@example/new'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "original").await, XmppEvent::RoomLeft(_)));
        client.close().await;
    }

    #[tokio::test]
    async fn cancelled_established_departure_reconciles_other_occupants() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.join_room_with_context("room@example", "me", "entry").await.unwrap();
        read_until_written(&mut server, b"protocol/muc'/></presence>").await;
        server.write_all(b"<presence from='room@example/alice'><x xmlns='http://jabber.org/protocol/muc#user'><item jid='alice@example' role='participant'/></x></presence><presence from='room@example/carol'><x xmlns='http://jabber.org/protocol/muc#user'><item role='participant'/></x></presence><presence from='room@example/erin'><x xmlns='http://jabber.org/protocol/muc#user'><item/></x></presence><presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "entry").await, XmppEvent::RoomJoined { members, .. } if members.len() == 4));
        let writer = client.writer.clone();
        let blocked = writer.lock().await;
        {
            let departure = client.leave_room_with_context("room@example", "me", "cancelled");
            tokio::pin!(departure);
            tokio::select! {
                biased;
                result = &mut departure => panic!("departure bypassed held writer: {result:?}"),
                _ = std::future::ready(()) => {},
            }
            server.write_all(b"<presence from='room@example/alice'><show>away</show><status>back later</status><x xmlns='http://jabber.org/protocol/muc#user'><item role='moderator'/></x></presence><presence type='unavailable' from='room@example/carol'><x xmlns='http://jabber.org/protocol/muc#user'><item nick='carrie'/><status code='303'/></x></presence><presence from='room@example/carrie'><status>renamed</status><x xmlns='http://jabber.org/protocol/muc#user'><item role='participant'/></x></presence><presence type='unavailable' from='room@example/erin'><x xmlns='http://jabber.org/protocol/muc#user'><item/></x></presence><presence from='room@example/bob'><x xmlns='http://jabber.org/protocol/muc#user'><item role='participant'/></x></presence><message type='chat' from='contact@example'><body>updates received</body></message>").await.unwrap();
            assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { .. })));
        }
        drop(blocked);
        server.write_all(b"<message type='chat' from='contact@example'><body>after cancellation</body></message>").await.unwrap();
        let mut updates = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap().unwrap();
            match event {
                XmppEvent::DirectMessage { body, .. } if body == "after cancellation" => break,
                XmppEvent::RoomEvent { context, event, .. } => { assert_eq!(context, "entry"); updates.push(*event); },
                other => panic!("unexpected update: {other:?}"),
            }
        }
        assert!(updates.iter().any(|event| matches!(event, XmppEvent::MemberLeft { nick, .. } if nick == "erin")), "departure during a cancelled leave was lost: {updates:?}");
        assert!(updates.iter().any(|event| matches!(event, XmppEvent::MemberNicknameChanged { old_nick, new_nick, is_self: false, .. } if old_nick == "carol" && new_nick == "carrie")));
        assert!(updates.iter().any(|event| matches!(event, XmppEvent::MemberJoined { member, .. } if member.nick == "alice" && member.show.as_deref() == Some("away") && member.status.as_deref() == Some("back later") && member.role == "moderator" && member.jid.as_deref() == Some("alice@example"))));
        assert!(updates.iter().any(|event| matches!(event, XmppEvent::MemberJoined { member, .. } if member.nick == "carrie" && member.status.as_deref() == Some("renamed"))));
        assert!(updates.iter().any(|event| matches!(event, XmppEvent::MemberJoined { member, .. } if member.nick == "bob")));
        assert_eq!(updates.len(), 5);
        client.close().await;
    }

    #[tokio::test]
    async fn rejected_established_departures_reconcile_rosters_without_duplicate_replays() {
        use tokio::io::AsyncWriteExt;
        async fn through_barrier(events: &mut mpsc::Receiver<XmppEvent>, expected_context: &str) -> Vec<XmppEvent> {
            let mut updates = Vec::new();
            loop {
                match tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap().unwrap() {
                    XmppEvent::DirectMessage { body, .. } if body == "barrier" => return updates,
                    XmppEvent::RoomEvent { context, event, .. } => { assert_eq!(context, expected_context); updates.push(*event); },
                    other => panic!("unexpected event: {other:?}"),
                }
            }
        }
        for mode in ["sent rejection", "cancel after rejection", "finish after rejection"] {
            for changed_context in [false, true] {
                let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
                assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
                client.join_room_with_context("room@example", "me", "entry").await.unwrap();
                read_until_written(&mut server, b"protocol/muc'/></presence>").await;
                server.write_all(b"<presence from='room@example/alice'><x xmlns='http://jabber.org/protocol/muc#user'><item jid='alice@example'/></x></presence><presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
                assert!(matches!(take_context_event(&mut events, "entry").await, XmppEvent::RoomJoined { .. }));
                let writer = client.writer.clone();
                let mut blocked = if mode == "sent rejection" { None } else { Some(writer.lock().await) };
                let tracked = client.rooms.clone();
                let departure_context = if changed_context { "departure" } else { "entry" };
                {
                    let departure = client.leave_room_with_context("room@example", "me", departure_context);
                    tokio::pin!(departure);
                    if mode == "sent rejection" {
                        departure.as_mut().await.unwrap();
                        read_until_written(&mut server, b"/>").await;
                    } else {
                        tokio::select! {
                            biased;
                            result = &mut departure => panic!("departure bypassed held writer: {result:?}"),
                            _ = std::future::ready(()) => {},
                        }
                    }
                    server.write_all(b"<presence from='room@example/alice'><show>away</show><x xmlns='http://jabber.org/protocol/muc#user'><item/></x></presence><presence from='room@example/bob'><x xmlns='http://jabber.org/protocol/muc#user'><item/></x></presence><message type='chat' from='contact@example'><body>barrier</body></message>").await.unwrap();
                    assert!(through_barrier(&mut events, departure_context).await.is_empty());
                    let id = tracked.lock().unwrap()["room@example"].request_id.clone();
                    server.write_all(format!("<presence type='error' id='{id}' from='room@example/me'><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></presence><message type='chat' from='contact@example'><body>barrier</body></message>").as_bytes()).await.unwrap();
                    let updates = through_barrier(&mut events, departure_context).await;
                    assert!(matches!(updates.first(), Some(XmppEvent::PresenceError { .. })));
                    assert_eq!(updates.len(), 3, "{mode}, changed_context={changed_context}: {updates:?}");
                    assert!(updates.iter().any(|event| matches!(event, XmppEvent::MemberJoined { member, .. } if member.nick == "alice" && member.show.as_deref() == Some("away") && member.jid.as_deref() == Some("alice@example"))));
                    assert!(updates.iter().any(|event| matches!(event, XmppEvent::MemberJoined { member, .. } if member.nick == "bob")));
                    // Further presence can arrive after rejection but before
                    // the blocked outgoing future is cancelled or completed.
                    server.write_all(b"<presence from='room@example/alice'><show>chat</show><x xmlns='http://jabber.org/protocol/muc#user'><item/></x></presence><message type='chat' from='contact@example'><body>barrier</body></message>").await.unwrap();
                    let updates = through_barrier(&mut events, departure_context).await;
                    assert!(matches!(updates.as_slice(), [XmppEvent::MemberJoined { member, .. }] if member.show.as_deref() == Some("chat")));
                    if mode == "finish after rejection" {
                        drop(blocked.take());
                        departure.as_mut().await.unwrap();
                    }
                }
                drop(blocked);
                let restored = mode == "cancel after rejection";
                let context = if restored { "entry" } else { departure_context };
                server.write_all(b"<message type='chat' from='contact@example'><body>barrier</body></message>").await.unwrap();
                let updates = through_barrier(&mut events, context).await;
                if restored && changed_context {
                    assert_eq!(updates.len(), 2);
                    assert!(updates.iter().any(|event| matches!(event, XmppEvent::MemberJoined { member, .. } if member.nick == "alice" && member.show.as_deref() == Some("chat"))));
                    assert!(updates.iter().any(|event| matches!(event, XmppEvent::MemberJoined { member, .. } if member.nick == "bob")));
                } else { assert!(updates.is_empty(), "unchanged contexts must not replay already delivered membership events: {updates:?}"); }
                {
                    let rooms = tracked.lock().unwrap();
                    assert!(rooms["room@example"].phase == rooms::Phase::Joined);
                    assert_eq!(rooms["room@example"].context.as_deref(), Some(context));
                    assert_eq!(rooms["room@example"].members.snapshot().len(), 3);
                }
                client.close().await;
            }
        }
    }

    #[tokio::test]
    async fn cancelled_departure_preserves_subject_updates_in_the_restored_context() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (service_subject, changed_context, rejected) in [
            (false, true, false), (true, true, false),
            (false, false, false), (true, false, false),
            (false, true, true), (true, true, true),
        ] {
            for subject in [Some("latest topic"), Some(""), Some("original topic"), None] {
                let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
                assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
                client.join_room_with_context("room@example", "me", "entry").await.unwrap();
                read_until_written(&mut server, b"protocol/muc'/></presence>").await;
                server.write_all(b"<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
                assert!(matches!(take_context_event(&mut events, "entry").await, XmppEvent::RoomJoined { .. }));
                let from = if service_subject { "room@example" } else { "room@example/alice" };
                server.write_all(format!("<message type='groupchat' from='{from}'><subject>original topic</subject></message>").as_bytes()).await.unwrap();
                assert!(matches!(take_context_event(&mut events, "entry").await, XmppEvent::RoomSubject { subject, .. } if subject == "original topic"));
                let writer = client.writer.clone();
                let blocked = writer.lock().await;
                let tracked = client.rooms.clone();
                let departure_context = if changed_context { "cancelled" } else { "entry" };
                {
                    let departure = client.request_room_leave("room@example", "me", changed_context.then_some("cancelled"));
                    tokio::pin!(departure);
                    tokio::select! {
                        biased;
                        result = &mut departure => panic!("departure bypassed held writer: {result:?}"),
                        _ = std::future::ready(()) => {},
                    }
                    if subject.is_some() {
                        server.write_all(format!("<message type='groupchat' from='{from}'><subject>intermediate topic</subject></message>").as_bytes()).await.unwrap();
                        assert!(matches!(take_context_event(&mut events, departure_context).await, XmppEvent::RoomSubject { subject, .. } if subject == "intermediate topic"));
                    }
                    if rejected {
                        let id = tracked.lock().unwrap()["room@example"].request_id.clone();
                        server.write_all(format!("<presence type='error' from='room@example/me' id='{id}'><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></presence>").as_bytes()).await.unwrap();
                        assert!(matches!(take_context_event(&mut events, departure_context).await, XmppEvent::PresenceError { .. }));
                    }
                    if let Some(expected) = subject {
                        server.write_all(format!("<message type='groupchat' from='{from}'><subject>{expected}</subject></message>").as_bytes()).await.unwrap();
                        assert!(matches!(take_context_event(&mut events, departure_context).await, XmppEvent::RoomSubject { subject, .. } if subject == expected));
                    }
                    server.write_all(b"<message type='chat' from='contact@example'><body>before cancellation</body></message>").await.unwrap();
                    assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { .. })));
                }
                drop(blocked);
                {
                    let rooms = tracked.lock().unwrap();
                    assert!(rooms["room@example"].phase == rooms::Phase::Joined);
                    assert_eq!(rooms["room@example"].context.as_deref(), Some("entry"));
                }
                server.write_all(b"<message type='chat' from='contact@example'><body>after cancellation</body></message>").await.unwrap();
                if let Some(expected) = subject.filter(|subject| changed_context && *subject != "original topic") {
                    assert!(matches!(take_context_event(&mut events, "entry").await, XmppEvent::RoomSubject { subject, .. } if subject == expected));
                }
                // No fabricated join, intermediate-topic replay, or duplicate
                // subject when the operation context never changed.
                assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { body, .. }) if body == "after cancellation"));
                let mut byte = [0];
                assert!(tokio::time::timeout(Duration::from_millis(20), server.read(&mut byte)).await.is_err(), "cancelled departure reached the wire");
                client.close().await;
            }
        }
    }

    #[tokio::test]
    async fn rejected_departures_preserve_and_publish_room_privacy_updates() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = joined_room_for_leave_test().await;
        server.write_all(b"<message type='groupchat' from='room@example'><x xmlns='http://jabber.org/protocol/muc#user'><status code='170'/><status code='172'/></x></message>").await.unwrap();
        assert!(matches!(events.recv().await, Some(XmppEvent::RoomPrivacy { privacy: RoomPrivacy { publicly_logged: Some(true), non_anonymous: Some(true) }, .. })));
        client.leave_room_with_context("room@example", "old", "departure").await.unwrap();
        read_until_written(&mut server, b"to='room@example/old'").await;
        // A partial service update can arrive while departure is unconfirmed.
        server.write_all(b"<message type='groupchat' from='room@example'><x xmlns='http://jabber.org/protocol/muc#user'><status code='171'/></x></message><presence type='error' from='room@example/old'><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></presence>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "departure").await, XmppEvent::PresenceError { .. }));
        let expected = RoomPrivacy { publicly_logged: Some(false), non_anonymous: Some(true) };
        assert_eq!(client.rooms.lock().unwrap()["room@example"].privacy, expected);
        assert!(matches!(take_context_event(&mut events, "departure").await, XmppEvent::RoomPrivacy { privacy, .. } if privacy == expected));
        server.write_all(b"<message type='groupchat' from='room@example'><x xmlns='http://jabber.org/protocol/muc#user'><status code='170'/></x></message>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "departure").await, XmppEvent::RoomPrivacy { privacy: RoomPrivacy { publicly_logged: Some(true), non_anonymous: Some(true) }, .. }));
        client.close().await;
    }

    #[tokio::test]
    async fn rejected_departure_publishes_the_current_self_nickname() {
        use tokio::io::AsyncWriteExt;
        for assigned in [false, true] {
            let (mut client, mut events, mut server) = joined_room_for_leave_test().await;
            client.leave_room_with_context("room@example", "old", "departure").await.unwrap();
            read_until_written(&mut server, b"to='room@example/old'").await;
            let rename = if assigned {
                "<presence from='room@example/new'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/><status code='210'/></x></presence>"
            } else {
                "<presence type='unavailable' from='room@example/old'><x xmlns='http://jabber.org/protocol/muc#user'><item nick='new'/><status code='110'/><status code='303'/></x></presence>"
            };
            server.write_all(rename.as_bytes()).await.unwrap();
            // The reader follows a server rename with a leave for the new nick.
            read_until_written(&mut server, b"to='room@example/new'").await;
            let id = client.rooms.lock().unwrap()["room@example"].request_id.clone();
            server.write_all(format!("<presence type='error' id='{id}' from='room@example/new'><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></presence><message type='chat' from='contact@example'><body>barrier</body></message>").as_bytes()).await.unwrap();
            assert!(matches!(take_context_event(&mut events, "departure").await, XmppEvent::PresenceError { .. }));
            assert!(matches!(take_context_event(&mut events, "departure").await,
                XmppEvent::MemberNicknameChanged { old_nick, new_nick, is_self: true, .. } if old_nick == "old" && new_nick == "new"));
            assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { .. })));
            {
                let rooms = client.rooms.lock().unwrap();
                assert!(rooms["room@example"].phase == rooms::Phase::Joined);
                assert_eq!(rooms["room@example"].nick, "new");
            }
            // Later availability must not repeat the rename or fabricate a join.
            server.write_all(b"<presence from='room@example/new'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence><message type='chat' from='contact@example'><body>next barrier</body></message>").await.unwrap();
            assert!(matches!(take_context_event(&mut events, "departure").await, XmppEvent::MemberJoined { member, .. } if member.nick == "new"));
            assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { .. })));
            client.close().await;
        }
    }

    async fn joined_room_for_leave_test() -> (XmppClient, mpsc::Receiver<XmppEvent>, tokio::net::TcpStream) {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection_with_sm(XmlFramer::new_client_stream(), true).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.join_room("room@example", "old").await.unwrap();
        read_until_written(&mut server, b"protocol/muc'/></presence>").await;
        server.write_all(b"<presence from='room@example/old'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
        assert!(matches!(events.recv().await, Some(XmppEvent::RoomJoined { .. })));
        (client, events, server)
    }

    #[tokio::test]
    async fn legacy_private_context_survives_departure_but_not_a_new_connection() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = joined_room_for_leave_test().await;
        client.leave_room("room@example", "old").await.unwrap();
        read_until_written(&mut server, b"to='room@example/old'").await;
        server.write_all(b"<presence type='unavailable' from='room@example/old'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence><message type='chat' from='room@EXAMPLE/Alice/phone'><body>late private</body></message><message type='chat' from='alice@example/phone'><body>ordinary</body></message>").await.unwrap();
        assert!(matches!(events.recv().await, Some(XmppEvent::RoomLeft(_))));
        assert!(matches!(events.recv().await, Some(XmppEvent::RoomPrivateMessage { from, body, .. }) if from == "room@EXAMPLE/Alice/phone" && body == "late private"));
        assert!(matches!(events.recv().await, Some(XmppEvent::DirectMessage { from, .. }) if from == "alice@example/phone"));
        assert!(client.rooms.lock().unwrap().is_empty());
        server.write_all(b"</stream:stream>").await.unwrap();
        client.close().await;

        // A new native connection has its own classification context. Failed
        // entries must not turn an ordinary contact into a remembered room.
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.join_room("contact@example", "me").await.unwrap();
        server.write_all(b"<presence type='error' from='contact@example/me'><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></presence><message type='chat' from='room@example/Alice'><body>fresh connection</body></message><message type='chat' from='contact@example/phone'><body>ordinary contact</body></message></stream:stream>").await.unwrap();
        assert!(matches!(events.recv().await, Some(XmppEvent::PresenceError { .. })));
        for expected in ["room@example/Alice", "contact@example/phone"] {
            assert!(matches!(events.recv().await, Some(XmppEvent::DirectMessage { from, .. }) if from == expected));
        }
        client.close().await;
    }

    #[tokio::test]
    async fn archive_events_preserve_full_private_recipients() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        let mut query = archive_query("private-page", None);
        query.with = Some("room@example/Alice");
        client.query_archive(query).await.unwrap();
        server.write_all(b"<message from='me@example'><result xmlns='urn:xmpp:mam:2' queryid='private-page' id='private-archive-id'><forwarded xmlns='urn:xmpp:forward:0'><delay xmlns='urn:xmpp:delay' stamp='2025-01-01T12:00:00Z'/><message xmlns='jabber:client' from='me@example/laptop' to='room@example/Alice'><body>private archive</body><x xmlns='http://jabber.org/protocol/muc#user'/></message></forwarded></result></message></stream:stream>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(),
            Some(XmppEvent::ArchiveMessage { from, to: Some(to), body, .. })
                if from == "me@example/laptop" && to == "room@example/Alice" && body == "private archive"));
        client.close().await;
    }

    #[tokio::test]
    async fn room_private_context_and_full_reply_targets_survive_the_wire() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection_with_sm(XmlFramer::new_client_stream(), true).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        server.write_all(b"<message type='chat' from='room@example/Alice' to='me@example/test'><body>private</body><x xmlns='http://jabber.org/protocol/muc#user'/></message>").await.unwrap();
        assert!(matches!(events.recv().await, Some(XmppEvent::RoomPrivateMessage { from, body, .. }) if from == "room@example/Alice" && body == "private"));
        for context in ["", "<x xmlns='foreign'/>", "<wrapper xmlns='foreign'><x xmlns='http://jabber.org/protocol/muc#user'/></wrapper>"] {
            server.write_all(format!("<message type='chat' from='room@example/Bob'><body>legacy</body>{context}</message>").as_bytes()).await.unwrap();
            assert!(matches!(events.recv().await, Some(XmppEvent::DirectMessage { from, .. }) if from == "room@example/Bob"));
        }
        server.write_all(b"<message type='chat' from=''><body>invalid sender</body></message><message type='chat' from='room@example/'><body>invalid resource</body></message><message type='chat' from='bad@@example/nick'><body>invalid domain</body></message><message type='chat' from='alice@example'><body>barrier</body></message>").await.unwrap();
        assert!(matches!(events.recv().await, Some(XmppEvent::DirectMessage { body, .. }) if body == "barrier"));
        assert!(matches!(client.send_room_private_message_confirmed("room@example", "bad target", None, Duration::from_secs(1)).await, Err(XmppError::InvalidInput(_))));
        let sending = tokio::spawn(async move {
            let result = client.send_room_private_message_confirmed("room@example/Alice/phone", "private reply", Some("private-origin"), Duration::from_secs(2)).await;
            (client, result)
        });
        let wire = String::from_utf8(read_until_written(&mut server, b"<r xmlns=").await).unwrap();
        assert!(wire.contains("to='room@example/Alice/phone'"));
        assert!(wire.contains("<x xmlns='http://jabber.org/protocol/muc#user'/>"));
        assert!(wire.contains("<origin-id xmlns='urn:xmpp:sid:0' id='private-origin'/>"));
        server.write_all(b"<a xmlns='urn:xmpp:sm:3' h='2'/>").await.unwrap();
        let (client, result) = sending.await.unwrap();
        assert_eq!(result.unwrap(), SendConfirmation::StreamAcknowledged);
        server.write_all(b"</stream:stream>").await.unwrap();
        client.close().await;
    }

    #[tokio::test]
    async fn joining_roster_churn_preserves_order_and_the_current_occupant_identity() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.join_room("room@example", "me").await.unwrap();
        let mut wire = String::new();
        for index in 0..64 {
            wire.push_str(&format!("<presence from='room@example/member-{index:02}'><x xmlns='http://jabber.org/protocol/muc#user'><item jid='user-{index}@example/phone' role='participant'/></x></presence>"));
        }
        wire.push_str("<presence type='unavailable' from='room@example/member-31'><x xmlns='http://jabber.org/protocol/muc#user'><item nick='renamed'/><status code='303'/></x></presence><presence from='room@example/renamed'><show>away</show><x xmlns='http://jabber.org/protocol/muc#user'><item role='moderator'/></x></presence><presence type='unavailable' from='room@example/member-04'/><presence from='room@example/member-04'><x xmlns='http://jabber.org/protocol/muc#user'><item role='visitor'/></x></presence><presence from='room@example/member-63'><x xmlns='http://jabber.org/protocol/muc#user'><item jid='replacement@example/laptop' role='moderator'/></x></presence><presence type='unavailable' from='room@example/member-00'/><presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>");
        server.write_all(wire.as_bytes()).await.unwrap();
        let Some(XmppEvent::RoomJoined { members, .. }) = tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap() else { panic!("missing joined roster"); };
        let mut expected: Vec<_> = (1..64).filter(|index| *index != 4).map(|index| if index == 31 { "renamed".into() } else { format!("member-{index:02}") }).collect();
        expected.extend(["member-04".into(), "me".into()]);
        assert_eq!(members.iter().map(|member| member.nick.clone()).collect::<Vec<_>>(), expected);
        let renamed = members.iter().find(|member| member.nick == "renamed").unwrap();
        assert_eq!(renamed.jid.as_deref(), Some("user-31@example/phone"));
        assert_eq!(renamed.role, "moderator");
        assert_eq!(renamed.show.as_deref(), Some("away"));
        assert!(members.iter().find(|member| member.nick == "member-04").unwrap().jid.is_none());
        assert_eq!(members.iter().find(|member| member.nick == "member-63").unwrap().jid.as_deref(), Some("replacement@example/laptop"));
        server.write_all(b"</stream:stream>").await.unwrap();
        client.close().await;
    }

    #[tokio::test]
    async fn joining_rosters_keep_known_member_addresses_until_departure_or_explicit_replacement() {
        use tokio::io::AsyncWriteExt;
        for change in ["update", "rename", "replacement", "nickname reuse"] {
            let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
            assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
            client.join_room("room@example", "me").await.unwrap();
            let mut wire = "<presence from='room@example/alice'><x xmlns='http://jabber.org/protocol/muc#user'><item jid='alice@example/phone' role='participant'/></x></presence>".to_string();
            let nick = if change == "rename" {
                wire.push_str("<presence type='unavailable' from='room@example/alice'><x xmlns='http://jabber.org/protocol/muc#user'><item nick='newalice'/><status code='303'/></x></presence>");
                "newalice"
            } else {
                if change == "nickname reuse" { wire.push_str("<presence type='unavailable' from='room@example/alice'/>"); }
                "alice"
            };
            let jid_attribute = if change == "replacement" { "jid='alice@example/laptop'" } else { "" };
            wire.push_str(&format!("<presence from='room@example/{nick}'><show>away</show><status>BRB</status><x xmlns='http://jabber.org/protocol/muc#user'><item {jid_attribute} role='moderator'/></x></presence><presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>"));
            server.write_all(wire.as_bytes()).await.unwrap();
            let Some(XmppEvent::RoomJoined { members, .. }) = tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap() else { panic!("missing joined roster"); };
            assert_eq!(members.len(), 2);
            assert_eq!(members[0].nick, nick);
            assert_eq!(members[1].nick, "me");
            assert_eq!(members[0].role, "moderator");
            assert_eq!(members[0].show.as_deref(), Some("away"));
            assert_eq!(members[0].status.as_deref(), Some("BRB"));
            let expected = match change {
                "replacement" => Some("alice@example/laptop"),
                "nickname reuse" => None,
                _ => Some("alice@example/phone"),
            };
            assert_eq!(members[0].jid.as_deref(), expected, "{change}");
            server.write_all(b"</stream:stream>").await.unwrap();
            client.close().await;
        }
    }

    #[tokio::test]
    async fn recipient_checks_apply_to_archive_envelopes_not_forwarded_addresses() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.query_archive(archive_query("recipient-archive", None)).await.unwrap();
        let result = |recipient: &str, body: &str| format!("<message from='me@example' to='{recipient}'><result xmlns='urn:xmpp:mam:2' queryid='recipient-archive' id='{body}'><forwarded xmlns='urn:xmpp:forward:0'><message xmlns='jabber:client' from='me@example/laptop' to='alice@example/phone'><body>{body}</body></message></forwarded></result></message>");
        server.write_all(format!("{}{}<iq type='result' from='me@example' to='me@example' id='recipient-archive'><fin xmlns='urn:xmpp:mam:2' complete='true'/></iq></stream:stream>", result("other@example/device", "wrong envelope"), result("ME@EXAMPLE/test", "valid outgoing archive")).as_bytes()).await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(),
            Some(XmppEvent::ArchiveMessage { body, to: Some(to), .. }) if body == "valid outgoing archive" && to == "alice@example/phone"));
        assert!(matches!(events.recv().await, Some(XmppEvent::ArchiveComplete { .. })));
        client.close().await;
    }

    #[tokio::test]
    async fn live_stanzas_cannot_target_another_account_or_change_its_room_state() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection_with_sm(XmlFramer::new_client_stream(), true).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.join_room("room@example", "me").await.unwrap();
        let (tx, mut reply) = tokio::sync::oneshot::channel();
        client.pending_iqs.lock().unwrap().insert("recipient-check".into(), PendingIq::new(tx, None, &client.bound_jid));
        let before = client.sm_state.inbound_count();
        server.write_all(b"<message from='alice@example' to='other@example/device'><body>wrong account</body></message><message from='alice@example' to='me@example/'><body>invalid resource</body></message><message from='alice@example' to=''><body>empty recipient</body></message><presence from='room@example/me' to='other@example/device'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence><iq type='result' from='me@example' to='other@example/device' id='recipient-check'/><message from='alice@example' to='ME@EXAMPLE/old-resource' xmlns:e='extension' e:to='other@example/device'><body>valid recipient</body></message>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { body, .. }) if body == "valid recipient"));
        assert!(client.rooms.lock().unwrap()["room@example"].phase == rooms::Phase::Joining);
        assert!(matches!(reply.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)));
        assert_eq!(client.sm_state.inbound_count(), before + 6);
        server.write_all(b"<presence from='room@example/me' to='me@example'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence><iq type='result' from='me@example' to='me@example/test' id='recipient-check'/>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::RoomJoined { .. })));
        assert!(tokio::time::timeout(Duration::from_secs(1), &mut reply).await.unwrap().unwrap().contains("recipient-check"));
        server.write_all(b"</stream:stream>").await.unwrap();
        client.close().await;
    }

    #[tokio::test]
    async fn service_text_shares_entry_buffering_and_survives_rejected_entry() {
        use tokio::io::AsyncWriteExt;
        for accepted in [false, true] {
            let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
            assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
            client.join_room_with_context("room@example", "me", "service-entry").await.unwrap();
            server.write_all(b"<message type='groupchat' from='room@example'><body>early service</body><delay xmlns='urn:xmpp:delay' stamp='2025-01-01T00:00:00Z'/></message><message from='room@example'><body>early normal</body></message><message from='contact@example'><body>barrier</body></message>").await.unwrap();
            assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { .. })));
            if accepted {
                server.write_all(b"<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
                assert!(matches!(take_context_event(&mut events, "service-entry").await, XmppEvent::RoomJoined { .. }));
            } else {
                server.write_all(b"<presence type='error' from='room@example/me'><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></presence>").await.unwrap();
            }
            assert!(matches!(take_context_event(&mut events, "service-entry").await, XmppEvent::RoomServiceMessage { body, delay: Some(_), .. } if body == "early service"));
            assert!(matches!(take_context_event(&mut events, "service-entry").await, XmppEvent::RoomServiceMessage { body, delay: None, .. } if body == "early normal"));
            if !accepted { assert!(matches!(take_context_event(&mut events, "service-entry").await, XmppEvent::PresenceError { .. })); }
            server.write_all(b"</stream:stream>").await.unwrap();
            client.close().await;
        }
    }

    #[tokio::test]
    async fn bare_room_service_text_keeps_its_identity_after_departure() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = joined_room_for_leave_test().await;
        server.write_all(b"<message type='groupchat' from='room@EXAMPLE'><body>service text</body><origin-id xmlns='urn:xmpp:sid:0' id='service-origin'/><stanza-id xmlns='urn:xmpp:sid:0' by='room@example' id='service-id'/></message>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::RoomServiceMessage { room, body, stanza_id: Some(id), origin_id: Some(origin), .. }) if room == "room@example" && body == "service text" && id == "service-id" && origin == "service-origin"));
        server.write_all(b"<message from='room@example'><body>normal service text</body></message><message from='contact@example'><body>ordinary contact</body></message>").await.unwrap();
        assert!(matches!(events.recv().await, Some(XmppEvent::RoomServiceMessage { body, .. }) if body == "normal service text"));
        assert!(matches!(events.recv().await, Some(XmppEvent::DirectMessage { from, .. }) if from == "contact@example"));
        client.leave_room("room@example", "old").await.unwrap();
        read_until_written(&mut server, b"to='room@example/old'").await;
        server.write_all(b"<presence type='unavailable' from='room@example/old'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence><message from='room@EXAMPLE'><body>after departure</body></message><message type='groupchat' from='unrequested@example'><body>unsolicited</body></message><message from='contact@example'><body>barrier</body></message></stream:stream>").await.unwrap();
        assert!(matches!(events.recv().await, Some(XmppEvent::RoomLeft(_))));
        assert!(matches!(events.recv().await, Some(XmppEvent::RoomServiceMessage { room, body, .. }) if room == "room@example" && body == "after departure"));
        assert!(matches!(events.recv().await, Some(XmppEvent::DirectMessage { body, .. }) if body == "barrier"));
        assert!(client.rooms.lock().unwrap().is_empty());
        client.close().await;
        let (client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        server.write_all(b"<message from='room@example'><body>new connection</body></message></stream:stream>").await.unwrap();
        assert!(matches!(events.recv().await, Some(XmppEvent::DirectMessage { from, .. }) if from == "room@example"));
        client.close().await;
    }

    #[tokio::test]
    async fn early_privacy_updates_wait_for_actual_room_confirmation() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.join_room_with_context("room@example", "me", "early-privacy").await.unwrap();
        server.write_all(b"<presence from='room@example/other'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/><status code='100'/></x></presence><message type='groupchat' from='room@example'><x xmlns='http://jabber.org/protocol/muc#user'><status code='170'/></x></message><message type='chat' from='alice@example'><body>barrier</body></message>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { .. })));
        server.write_all(b"<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence></stream:stream>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "early-privacy").await, XmppEvent::RoomJoined { .. }));
        assert!(matches!(take_context_event(&mut events, "early-privacy").await, XmppEvent::RoomPrivacy { privacy: RoomPrivacy { publicly_logged: Some(true), non_anonymous: None }, .. }));
        client.close().await;
    }

    #[tokio::test]
    async fn privacy_notices_follow_confirmed_entry_and_only_live_room_service_updates() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.join_room_with_context("room@example", "me", "privacy-entry").await.unwrap();
        server.write_all(b"<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/><status code='100'/><status code='170'/></x></presence>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "privacy-entry").await, XmppEvent::RoomJoined { .. }));
        assert!(matches!(take_context_event(&mut events, "privacy-entry").await, XmppEvent::RoomPrivacy { privacy: RoomPrivacy { publicly_logged: Some(true), non_anonymous: Some(true) }, .. }));
        server.write_all(b"<message type='groupchat' from='room@EXAMPLE'><x xmlns='http://jabber.org/protocol/muc#user'><status code='171'/><status code='173'/></x></message>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "privacy-entry").await, XmppEvent::RoomPrivacy { privacy: RoomPrivacy { publicly_logged: Some(false), non_anonymous: Some(false) }, .. }));
        server.write_all(b"<message type='groupchat' from='room@example/alice'><x xmlns='http://jabber.org/protocol/muc#user'><status code='170'/></x></message><message type='groupchat' from='room@example'><x xmlns='foreign'><status code='170'/></x></message><message type='groupchat' from='other@example'><x xmlns='http://jabber.org/protocol/muc#user'><status code='170'/></x></message><message type='groupchat' from='room@example'><x xmlns='http://jabber.org/protocol/muc#user'><status code='170'/></x><delay xmlns='urn:xmpp:delay' stamp='2025-01-01T00:00:00Z'/></message><message type='chat' from='alice@example'><body>barrier</body></message>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { .. })));
        server.write_all(b"<message type='groupchat' from='room@example'><x xmlns='http://jabber.org/protocol/muc#user'><status code='170'/></x></message></stream:stream>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "privacy-entry").await, XmppEvent::RoomPrivacy { privacy: RoomPrivacy { publicly_logged: Some(true), non_anonymous: Some(false) }, .. }));
        client.close().await;
    }

    #[tokio::test]
    async fn bare_room_topics_are_delivered_with_registered_identity_and_context() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.join_room_with_context("room@example", "me", "entry").await.unwrap();
        server.write_all(b"<message type='groupchat' from='room@EXAMPLE'><subject>initial topic</subject><delay xmlns='urn:xmpp:delay' stamp='2025-01-01T00:00:00Z'/></message><presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "entry").await, XmppEvent::RoomJoined { .. }));
        assert!(matches!(take_context_event(&mut events, "entry").await, XmppEvent::RoomSubject { room, subject } if room == "room@example" && subject == "initial topic"));
        server.write_all(b"<message type='groupchat' from='room@example'><subject>live topic</subject></message><message type='groupchat' from='room@example'><subject/></message>").await.unwrap();
        assert!(matches!(take_context_event(&mut events, "entry").await, XmppEvent::RoomSubject { subject, .. } if subject == "live topic"));
        assert!(matches!(take_context_event(&mut events, "entry").await, XmppEvent::RoomSubject { subject, .. } if subject.is_empty()));
        server.write_all(b"<message type='groupchat' from='other@example'><subject>unrequested</subject></message><message type='groupchat' from='room@example/'><subject>invalid empty resource</subject></message><message type='chat' from='alice@example'><body>barrier</body></message></stream:stream>").await.unwrap();
        assert!(matches!(events.recv().await, Some(XmppEvent::DirectMessage { .. })));
        client.close().await;
        assert!(events.recv().await.is_none());
    }

    #[tokio::test]
    async fn malformed_service_nicknames_cannot_poison_membership_or_prevent_leaving() {
        use tokio::io::AsyncWriteExt;
        for status in ["303", "210"] {
            let (mut client, mut events, mut server) = joined_room_for_leave_test().await;
            let invalid = "😀".repeat(256); // 1024 UTF-8 bytes, beyond a JID resource.
            let presence = if status == "303" {
                format!("<presence type='unavailable' from='room@example/old'><x xmlns='http://jabber.org/protocol/muc#user'><item nick='{invalid}'/><status code='110'/><status code='303'/></x></presence>")
            } else {
                format!("<presence from='room@example/{invalid}'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/><status code='210'/></x></presence>")
            };
            server.write_all(format!("{presence}<message type='chat' from='alice@example'><body>barrier</body></message>").as_bytes()).await.unwrap();
            assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { .. })), "status {status}");
            assert_eq!(client.rooms.lock().unwrap()["room@example"].nick, "old");
            client.leave_room("room@example", "old").await.unwrap();
            read_until_written(&mut server, b"to='room@example/old'").await;
            server.write_all(b"<presence type='unavailable' from='room@example/old'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence></stream:stream>").await.unwrap();
            assert!(matches!(events.recv().await, Some(XmppEvent::RoomLeft(room)) if room == "room@example"));
            client.close().await;
        }
    }

    #[tokio::test]
    async fn invalid_room_targets_are_local_errors_without_writes_or_membership_changes() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection_with_sm(XmlFramer::new_client_stream(), true).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.join_room("room@example", "me").await.unwrap();
        read_until_written(&mut server, b"protocol/muc'/></presence>").await;
        server.write_all(b"<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::RoomJoined { .. })));
        let request_id = client.rooms.lock().unwrap()["room@example"].request_id.clone();
        let before = client.sm_state.outbound.load(Ordering::SeqCst);
        for (room, nick) in [
            ("", "me"), ("no-domain", "me"), ("@example", "me"), ("room@", "me"),
            ("room@@example", "me"), ("room@example/other", "me"), ("room @example", "me"),
            ("room@example", ""), ("room@example", "bad\nnick"), ("room@example", "bad\0nick"),
        ] {
            assert!(matches!(client.join_room(room, nick).await, Err(XmppError::InvalidInput(_))), "join {room:?}/{nick:?}");
            assert!(matches!(client.leave_room(room, nick).await, Err(XmppError::InvalidInput(_))), "leave {room:?}/{nick:?}");
        }
        assert_eq!(client.sm_state.outbound.load(Ordering::SeqCst), before);
        {
            let rooms = client.rooms.lock().unwrap();
            assert_eq!(rooms.len(), 1);
            assert!(rooms["room@example"].phase == rooms::Phase::Joined);
            assert_eq!(rooms["room@example"].request_id, request_id);
        }
        client.send_message("alice@example", "session still works").await.unwrap();
        let written = read_until_written(&mut server, b"</message>").await;
        assert_eq!(written, stanza::chat::ChatMessage::new("alice@example".into(), "session still works".into()).as_bytes());
        assert_eq!(client.sm_state.outbound.load(Ordering::SeqCst), before + 1);
        client.close().await;
    }

    #[test]
    fn room_jid_preflight_matches_native_command_validation() {
        for room in ["room@example".to_string(), "rüm@conference.example".into(),
            format!("{}@{}", "a".repeat(1023), "b".repeat(1023))] {
            assert!(validate_room_jid(&room).is_ok());
            assert!(validate_room_target(&room, "my/nick").is_ok());
        }
        for room in ["".to_string(), "example".into(), "@example".into(), "room@".into(),
            "room@example/nick".into(), "room@example/".into(), "bad room@example".into(), "bad\0room@example".into(),
            format!("{}@example", "é".repeat(512)), format!("room@{}", "a".repeat(1024))] {
            assert!(matches!(validate_room_jid(&room), Err(XmppError::InvalidInput(_))), "{room:?}");
            assert!(matches!(validate_room_target(&room, "nick"), Err(XmppError::InvalidInput(_))), "{room:?}");
        }
    }

    #[test]
    fn room_target_validation_preserves_valid_resource_characters() {
        for nick in ["my nick", "my/nick", "😀 åäö", "<nick>&'"] {
            assert!(validate_room_target("room@example", nick).is_ok(), "{nick}");
        }
    }

    #[tokio::test]
    async fn invalid_outgoing_text_does_not_write_or_damage_the_session()
    {
        use tokio::io::AsyncWriteExt;
        let (mut client, _events, mut server) = test_connection_with_sm(XmlFramer::new_client_stream(), true).await;
        assert!(matches!(client.send_message("alice@example", "bad\0body").await, Err(XmppError::InvalidInput(_))));
        assert!(matches!(client.send_room_message("room@example", "bad\0body").await, Err(XmppError::InvalidInput(_))));
        assert!(matches!(client.send_room_message_confirmed("room@example", "bad\0body", None, Duration::from_secs(1)).await, Err(XmppError::InvalidInput(_))));
        assert!(matches!(client.send_message_confirmed("alice@example", "bad\0body", None, Duration::from_secs(1)).await, Err(XmppError::InvalidInput(_))));
        assert!(matches!(client.join_room("bad\0room", "nick").await, Err(XmppError::InvalidInput(_))));
        assert_eq!(client.sm_state.outbound_count(), 1); // Initial presence only.
        let sending = tokio::spawn(async move {
            let result = client.send_message_confirmed("alice@example", "good", None, Duration::from_secs(1)).await;
            (client, result)
        });
        let bytes = read_until_written(&mut server, b"<r xmlns=").await;
        assert!(!bytes.contains(&0));
        server.write_all(b"<a xmlns='urn:xmpp:sm:3' h='2'/>").await.unwrap();
        let (client, result) = sending.await.unwrap();
        assert_eq!(result.unwrap(), SendConfirmation::StreamAcknowledged);
        client.close().await;
    }

    #[tokio::test]
    async fn message_errors_are_surfaced_without_dispatching_echoed_payloads_as_messages()
    {
        use tokio::io::AsyncWriteExt;
        let (client, mut events, mut server) = test_connection_with_sm(XmlFramer::new_client_stream(), true).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        server.write_all(b"<message type='error' id='wire-id' from='alice@example/phone'><body>echoed, not a new chat</body><result xmlns='urn:xmpp:mam:2' queryid='fake'/><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/><text xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'>Not allowed</text></error></message><message type='chat' from='alice@example/phone'><body>barrier</body></message>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(),
            Some(XmppEvent::MessageError { id, condition, text: Some(text), .. }) if id == "wire-id" && condition == "forbidden" && text == "Not allowed"));
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(),
            Some(XmppEvent::DirectMessage { body, .. }) if body == "barrier"));
        assert!(events.try_recv().is_err());
        assert_eq!(client.sm_state.inbound_count(), 2);
        assert_eq!(client.sm_state.outbound_count(), 1);
        client.close().await;
    }

    #[tokio::test]
    async fn pre_management_messages_are_delivered_without_inflating_handled_count()
    {
        let mut framer = XmlFramer::new_client_stream();
        framer.defer_frames(vec![
            "<message xmlns='jabber:client' type='chat' from='alice@example'><body>before</body></message>".into(),
            "<iq xmlns='jabber:client' type='get' from='alice@example' id='p'><ping xmlns='urn:xmpp:ping'/></iq>".into(),
        ]);
        framer.feed(b"<message type='chat' from='alice@example'><body>after</body></message>");
        let (client, mut events, _server) = test_connection_with_sm(framer, true).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        for expected in ["before", "after"] {
            let event = tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap();
            assert!(matches!(event, Some(XmppEvent::DirectMessage { body, .. }) if body == expected));
        }
        assert_eq!(client.sm_state.inbound_count(), 1);
        // Initial presence and the deferred ping's response were sent after enable.
        assert_eq!(client.sm_state.outbound_count(), 2);
        client.close().await;
    }

    #[tokio::test]
    async fn session_preserves_complete_and_partial_stanzas_buffered_during_setup()
    {
        use tokio::io::AsyncWriteExt;
        let mut framer = XmlFramer::new_client_stream();
        framer.feed(b"<enabled xmlns='urn:xmpp:sm:3'/><message type='chat' from='alice@example'><body>complete</body></message><message type='chat' from='alice@example'><body>par");
        assert!(framer.try_next().unwrap().starts_with("<enabled"));
        let (client, mut events, mut server) = test_connection(framer).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        let first = tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap();
        assert!(matches!(first, Some(XmppEvent::DirectMessage { body, .. }) if body == "complete"));
        server.write_all(b"tial</body></message>").await.unwrap();
        let second = tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap();
        assert!(matches!(second, Some(XmppEvent::DirectMessage { body, .. }) if body == "partial"));
        client.close().await;
    }

    #[tokio::test]
    async fn idle_ack_probe_requires_a_fresh_acknowledgement()
    {
        let (mut client, _events, _server) = test_connection(XmlFramer::new_client_stream()).await;
        client.sm_state.enable();
        client.sm_state.record_outbound();
        client.sm_state.record_ack(1);
        assert!(matches!(client.request_ack(Duration::from_millis(20)).await, Err(XmppError::Timeout(_))));
        client.close().await;
    }

    #[tokio::test]
    async fn fresh_ack_with_an_unchanged_count_completes_idle_probe()
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut client, _events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        client.sm_state.enable();
        let responder = tokio::spawn(async move {
            let mut data = Vec::new();
            loop {
                let mut chunk = [0u8; 4096];
                let n = server.read(&mut chunk).await.unwrap();
                assert_ne!(n, 0);
                data.extend_from_slice(&chunk[..n]);
                if data.windows(3).any(|w| w == b"<r ") { break; }
            }
            server.write_all(b"<a xmlns='urn:xmpp:sm:3' h='0'/>").await.unwrap();
            server
        });
        client.request_ack(Duration::from_secs(1)).await.unwrap();
        let _server = responder.await.unwrap();
        client.close().await;
    }

    #[tokio::test]
    async fn acknowledgement_timeout_also_bounds_writer_lock_wait()
    {
        let (mut client, _events, _server) = test_connection(XmlFramer::new_client_stream()).await;
        client.sm_state.enable();
        let writer = client.writer.clone();
        let guard = writer.lock().await;
        let result = tokio::time::timeout(Duration::from_secs(1), client.request_ack(Duration::from_millis(20)))
            .await.expect("request_ack exceeded its own timeout while waiting for the writer");
        assert!(matches!(result, Err(XmppError::Timeout(_))));
        drop(guard);
        client.close().await;
    }

    #[tokio::test]
    async fn message_line_endings_are_preserved_on_send_and_normalized_on_receive()
    {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.send_message("alice@example", "a\r\nb\rc").await.unwrap();
        let written = read_until_written(&mut server, b"</message>").await;
        assert!(String::from_utf8(written).unwrap().contains("<body>a&#xD;\nb&#xD;c</body>"));
        server.write_all(b"<message from='alice@example'><body>a\r\nb&#xD;c<![CDATA[d\re]]></body></message>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(),
            Some(XmppEvent::DirectMessage { body, .. }) if body == "a\nb\rcd\ne"));
        client.close().await;
    }

    #[tokio::test]
    async fn quoted_occupant_names_cannot_turn_availability_into_another_members_departure()
    {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.join_room("room@example", "me").await.unwrap();
        read_until_written(&mut server, b"to='room@example/me'").await;
        server.write_all(b"<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::RoomJoined { .. })));
        server.write_all(b"<presence from='room@example/victim'><x xmlns='http://jabber.org/protocol/muc#user'><item role='participant'/></x></presence>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::MemberJoined { member, .. }) if member.nick == "victim"));
        server.write_all(br#"<presence from='room@example/victim" type="unavailable'><x xmlns='http://jabber.org/protocol/muc#user'><item role='participant'/></x></presence>"#).await.unwrap();
        let quoted_nick = "victim\" type=\"unavailable";
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::MemberJoined { member, .. }) if member.nick == quoted_nick));
        assert!(client.rooms.lock().unwrap()["room@example"].phase == rooms::Phase::Joined);
        client.close().await;
    }

    #[tokio::test]
    async fn server_stream_error_closes_session_and_pending_requests_without_waiting_for_eof()
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (client, mut events, mut server) = test_connection_with_sm(XmlFramer::new_client_stream(), true).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        let (tx, rx) = oneshot::channel();
        client.pending_iqs.lock().unwrap().insert("pending".into(), PendingIq::new(tx, None, "me@example/test"));
        // An extension named error and a nested stream:error do not end a session.
        server.write_all(b"<error xmlns='extension'/><message from='alice@example'><x><s:error xmlns:s='http://etherx.jabber.org/streams'/></x><body>before error</body></message>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::DirectMessage { body, .. }) if body == "before error"));
        server.write_all(b"<s:error xmlns:s='http://etherx.jabber.org/streams'><conflict xmlns='urn:ietf:params:xml:ns:xmpp-streams'/></s:error><message from='alice@example'><body>after fatal error</body></message>").await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap().is_none());
        assert!(tokio::time::timeout(Duration::from_secs(1), rx).await.unwrap().is_err());
        assert!(client.sm_state.closed.load(Ordering::SeqCst));
        assert_eq!(client.sm_state.inbound_count(), 1);
        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), server.read_to_end(&mut received)).await.unwrap().unwrap();
        assert!(String::from_utf8(received).unwrap().ends_with("</stream:stream>"));
        client.close().await;
    }

    #[tokio::test]
    async fn restricted_markup_closes_the_stream_without_delivering_its_message()
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for markup in ["<!-- forbidden -->", "<?forbidden data?>", "<!DOCTYPE message>"] {
            let (client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
            assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
            server.write_all(format!("<message type='chat' from='alice@example'>{markup}<body>must not be delivered</body></message>").as_bytes()).await.unwrap();
            assert!(tokio::time::timeout(Duration::from_secs(2), events.recv()).await.unwrap().is_none());
            client.close().await;
            let mut received = Vec::new();
            tokio::time::timeout(Duration::from_secs(1), server.read_to_end(&mut received)).await.unwrap().unwrap();
            let received = String::from_utf8(received).unwrap();
            assert!(received.contains("<restricted-xml xmlns='urn:ietf:params:xml:ns:xmpp-streams'/>") , "{received}");
            assert!(received.ends_with("</stream:stream>"));
        }
    }

    #[tokio::test]
    async fn invalid_namespace_bindings_terminate_the_stream_without_delivering_the_message() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for declaration in ["xmlns='http://www.w3.org/XML/1998/namespace'", "xmlns:p=''"] {
            let (client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
            assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
            let xml = format!("<message type='chat' from='alice@example'><extra {declaration}/><body>must not be delivered</body></message>");
            server.write_all(xml.as_bytes()).await.unwrap();
            assert!(tokio::time::timeout(Duration::from_secs(2), events.recv()).await.unwrap().is_none());
            client.close().await;
            let mut received = Vec::new();
            tokio::time::timeout(Duration::from_secs(1), server.read_to_end(&mut received)).await.unwrap().unwrap();
            let received = String::from_utf8(received).unwrap();
            assert!(received.contains("<not-well-formed xmlns='urn:ietf:params:xml:ns:xmpp-streams'/>"), "{received}");
            assert!(received.ends_with("</stream:stream>"));
        }
    }

    #[tokio::test]
    async fn malformed_utf8_terminates_stream_instead_of_delivering_corrupt_text()
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        server.write_all(b"<message type='chat' from='alice@example'><body>bad\xff</body></message>").await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(2), events.recv()).await.unwrap().is_none());
        client.close().await;
        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), server.read_to_end(&mut received)).await.unwrap().unwrap();
        let received = String::from_utf8(received).unwrap();
        assert!(received.contains("<not-well-formed xmlns='urn:ietf:params:xml:ns:xmpp-streams'/>") );
        assert!(received.ends_with("</stream:stream>"));
    }

    #[tokio::test]
    async fn local_close_sends_stream_end_and_delivers_peers_final_message()
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        let responder = tokio::spawn(async move {
            let mut received = Vec::new();
            loop {
                let mut buf = [0; 1024];
                let n = server.read(&mut buf).await.unwrap();
                assert!(n > 0, "client closed TCP before sending its XML close tag");
                received.extend_from_slice(&buf[..n]);
                if String::from_utf8_lossy(&received).contains("</stream:stream>") { break; }
            }
            server.write_all(b"<message type='chat' from='alice@example'><body>last message</body></message></stream:stream>").await.unwrap();
            received
        });
        tokio::time::timeout(Duration::from_secs(2), client.close()).await.unwrap();
        let received = responder.await.unwrap();
        assert_eq!(String::from_utf8(received).unwrap().matches("</stream:stream>").count(), 1);
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        assert!(matches!(events.recv().await, Some(XmppEvent::DirectMessage { body, .. }) if body == "last message"));
        assert!(events.recv().await.is_none());
    }

    #[tokio::test]
    async fn remote_xml_close_ends_events_without_waiting_for_tcp_eof()
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        server.write_all(b"</stream:stream>").await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(2), events.recv()).await.unwrap().is_none());
        assert!(matches!(client.send_message("alice@example", "too late").await, Err(XmppError::Disconnected)));
        client.close().await;
        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), server.read_to_end(&mut received)).await.unwrap().unwrap();
        assert_eq!(String::from_utf8(received).unwrap().matches("</stream:stream>").count(), 1);
    }

    #[tokio::test]
    async fn close_completes_even_when_the_event_channel_is_full()
    {
        let mut framer = XmlFramer::new_client_stream();
        for _ in 0..64 {
            framer.feed(b"<message type='chat' from='alice@example'><body>queued</body></message>");
        }
        let (client, events, _server) = test_connection(framer).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while events.len() < 32 { tokio::task::yield_now().await; }
        }).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), client.close()).await
            .expect("close deadlocked behind a full event channel");
    }

    #[tokio::test]
    async fn dropping_a_client_cancels_its_reader_task()
    {
        let (client, _events, _server) = test_connection(XmlFramer::new_client_stream()).await;
        let task = client.task.abort_handle();
        drop(client);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !task.is_finished() { tokio::task::yield_now().await; }
        }).await.expect("dropping the client detached a live reader");
    }

    #[tokio::test]
    async fn reader_eof_wakes_pending_iq_requests()
    {
        let (client, _events, server) = test_connection(XmlFramer::new_client_stream()).await;
        let (tx, rx) = oneshot::channel();
        client.pending_iqs.lock().unwrap().insert("pending".into(), PendingIq::new(tx, None, &client.bound_jid));
        drop(server);
        assert!(tokio::time::timeout(Duration::from_secs(1), rx).await
            .expect("pending IQ was not notified of EOF").is_err());
        assert!(client.pending_iqs.lock().unwrap().is_empty());
        client.close().await;
    }

    #[test]
    fn pending_iq_accepts_only_the_requested_entity_or_authoritative_errors()
    {
        let own = "me@example/test";
        for target in [None, Some("example"), Some("me@example")] {
            let (tx, _) = oneshot::channel();
            let request = PendingIq::new(tx, target, own);
            for from in [None, Some("example")] {
                assert!(request.accepts(from, target.is_some()));
            }
            assert!(!request.accepts(Some("mallory@example/phone"), false));
            assert!(!request.accepts(Some("mallory@example/phone"), true));
            assert!(!request.accepts(Some("evil.example"), false));
        }
        let (tx, _) = oneshot::channel();
        let account = PendingIq::new(tx, None, own);
        assert!(account.accepts(Some("me@example"), false));
        assert!(account.accepts(Some(own), false));
        assert!(!account.accepts(Some("me@example/other"), false));

        for target in ["alice@remote", "alice@remote/phone", "conference.remote"] {
            let (tx, _) = oneshot::channel();
            let request = PendingIq::new(tx, Some(target), own);
            assert!(request.accepts(Some(target), false));
            for from in [None, Some("mallory@remote/phone"), Some("evil.example"), Some("me@example")] {
                assert!(!request.accepts(from, false), "target={target}, from={from:?}");
                assert!(!request.accepts(from, true), "target={target}, from={from:?}");
            }
            assert!(!request.accepts(Some("example"), false));
            assert!(request.accepts(Some("example"), true));
        }
        let (tx, _) = oneshot::channel();
        let bare = PendingIq::new(tx, Some("alice@remote"), own);
        assert!(bare.accepts(Some("alice@remote/phone"), false));
        assert!(bare.accepts(Some("remote"), true));
        assert!(!bare.accepts(Some("remote"), false));
        let (tx, _) = oneshot::channel();
        let full = PendingIq::new(tx, Some("alice@remote/phone"), own);
        assert!(!full.accepts(Some("alice@remote/other"), false));
        assert!(!full.accepts(Some("alice@remote"), false));
    }

    #[test]
    fn equivalent_domains_preserve_iq_authority_boundaries()
    {
        let (tx, _) = oneshot::channel();
        let request = PendingIq::new(tx, Some("alice@[2001:db8::1]/Phone"), "me@EXAMPLE.org/device");
        assert!(request.accepts(Some("alice@[2001:0db8:0:0:0:0:0:1]/Phone"), false));
        assert!(request.accepts(Some("example.ORG"), true));
        for from in ["alice@[2001:db8::1]/phone", "Alice@[2001:db8::1]/Phone", "alice@[2001:db8::2]/Phone"] {
            assert!(!request.accepts(Some(from), false));
        }
        let archive = PendingIq::archive(Some("room@CONFERENCE.example"), "me@example/device");
        assert!(archive.accepts(Some("room@conference.EXAMPLE"), false));
        assert!(!archive.accepts(Some("room@conference.example/occupant"), false));
    }

    #[test]
    fn message_targets_reject_malformed_jids_but_allow_domain_only_services() {
        for to in ["user@@example", "user@bad domain", "bad domain", "user@example:5222", "user@[invalid]", "user@example/phone\tadmin", "bad user@example"] {
            assert!(matches!(validate_message(to, "body"), Err(XmppError::InvalidInput(_))), "{to:?}");
        }
        for to in ["service.example", "service.example/resource", "alice@example", "alice@example/team/alice@phone", "alice@[::1]/phone"] {
            assert!(validate_message(to, "body").is_ok(), "{to}");
        }
    }

    #[test]
    fn recipient_and_room_lengths_count_bytes_instead_of_characters() {
        let resource = format!("{}abc", "🙂".repeat(255));
        assert!(validate_message(&format!("alice@example/{resource}"), "body").is_ok());
        assert!(validate_room_target("room@example", &resource).is_ok());
        assert!(matches!(validate_message(&format!("alice@example/{resource}x"), "body"), Err(XmppError::InvalidInput(_))));
        assert!(matches!(validate_room_target("room@example", &format!("{resource}x")), Err(XmppError::InvalidInput(_))));
        assert!(matches!(validate_message(&"d".repeat(1024), "body"), Err(XmppError::InvalidInput(_))));
    }

    #[tokio::test]
    async fn invalid_targets_do_not_register_requests_write_stanzas_or_disconnect() {
        let (mut client, mut events, _server) = test_connection_with_sm(XmlFramer::new_client_stream(), true).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        let sent = client.sm_state.outbound_count();
        let oversized = format!("{}@example", "u".repeat(1024));
        for target in [oversized.as_str(), "user@@example", "bad domain", "user@[invalid]", "user@example/bad\tresource"] {
            assert!(matches!(client.send_message(target, "body").await, Err(XmppError::InvalidInput(_))));
            assert!(matches!(client.ping(Some(target), Duration::from_millis(50)).await, Err(XmppError::InvalidInput(_))));
            assert!(matches!(client.disco_info(Some(target), None, Duration::from_millis(50)).await, Err(XmppError::InvalidInput(_))));
            assert!(matches!(client.disco_items(Some(target), None, Duration::from_millis(50)).await, Err(XmppError::InvalidInput(_))));
            assert!(matches!(client.query_archive(archive_query("target", Some(target))).await, Err(XmppError::InvalidInput(_))));
            let mut filter = archive_query("filter", None);
            filter.with = Some(target);
            assert!(matches!(client.query_archive(filter).await, Err(XmppError::InvalidInput(_))));
        }
        assert!(matches!(client.join_room("room@example", &"🙂".repeat(256)).await, Err(XmppError::InvalidInput(_))));
        assert!(client.pending_iqs.lock().unwrap().is_empty());
        assert!(client.rooms.lock().unwrap().is_empty());
        assert_eq!(client.sm_state.outbound_count(), sent);
        client.send_message("alice@example", "still usable").await.unwrap();
        assert_eq!(client.sm_state.outbound_count(), sent + 1);
        client.close().await;
    }

    #[test]
    fn public_iq_futures_can_run_on_a_multithreaded_executor()
    {
        fn require_send<T: Send>(_: T) {}
        // Compile-time coverage: no socket or running executor is necessary.
        let _check = |client: &mut XmppClient| {
            require_send(client.ping(None, Duration::from_secs(1)));
            require_send(client.disco_info(None, None, Duration::from_secs(1)));
            require_send(client.disco_items(None, None, Duration::from_secs(1)));
        };
    }

    #[tokio::test]
    async fn targeted_ping_ignores_matching_ids_from_other_senders()
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        let (done_tx, mut done_rx) = oneshot::channel();
        let ping = async {
            let result = client.ping(Some("alice@remote/phone"), Duration::from_secs(5)).await;
            done_tx.send(result).unwrap();
        };
        let exchange = async {
            let mut request = Vec::new();
            while !String::from_utf8_lossy(&request).contains("<ping xmlns=") {
                let mut bytes = [0; 4096];
                let n = tokio::time::timeout(Duration::from_secs(1), server.read(&mut bytes)).await.unwrap().unwrap();
                assert!(n > 0);
                request.extend_from_slice(&bytes[..n]);
            }
            server.write_all(b"<iq type='result' id='ping_1' from='mallory@remote/phone'/><iq type='error' id='ping_1'/><message type='chat' from='alice@remote/phone'><body>barrier</body></message>").await.unwrap();
            // Receiving the following message proves the reader has already dealt
            // with both forged replies; the request must still be waiting.
            assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(),
                Some(XmppEvent::DirectMessage { body, .. }) if body == "barrier"));
            assert!(matches!(done_rx.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
            server.write_all(b"<iq type='result' id='ping_1' from='alice@remote/phone'/>").await.unwrap();
            let result = tokio::time::timeout(Duration::from_secs(1), done_rx).await.unwrap().unwrap();
            assert!(result.is_ok());
        };
        tokio::join!(ping, exchange);
        client.close().await;
    }

    #[tokio::test]
    async fn rejoin_waiter_observes_failed_connection_before_event_backpressure_clears()
    {
        let mut framer = XmlFramer::new_client_stream();
        framer.feed("<message type='chat' from='alice@example'><body>queued</body></message>".repeat(42).as_bytes());
        let (mut client, mut events, _server) = test_connection(framer).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        tokio::time::timeout(Duration::from_secs(1), async {
            while events.len() < 32 { tokio::task::yield_now().await; }
        }).await.unwrap();
        client.join_room("room@example", "me").await.unwrap();
        client.leave_room("room@example", "me").await.unwrap();
        let writer = client.writer.clone();
        let state = client.sm_state.clone();
        let mut rejoin = Box::pin(client.join_room("room@example", "me"));
        tokio::select! {
            biased;
            result = &mut rejoin => panic!("rejoin finished before departure: {result:?}"),
            _ = std::future::ready(()) => {},
        }
        // A normal stream acknowledgement is only a wakeup, not confirmation
        // that the old room membership ended.
        state.acked.notify_waiters();
        tokio::select! {
            biased;
            result = &mut rejoin => panic!("acknowledgement completed room departure: {result:?}"),
            _ = std::future::ready(()) => {},
        }
        {
            let mut writer = writer.lock().await;
            writer.shutdown().await;
            assert!(write_session(&mut writer, &state, b"<presence/>").await.is_err());
        }
        let result = tokio::time::timeout(Duration::from_secs(1), rejoin).await
            .expect("rejoin waited for event delivery after the session failed");
        assert!(matches!(result, Err(XmppError::Disconnected)));
        // No queued data was consumed to make the rejoin future finish.
        assert_eq!(events.len(), 32);
        let mut count = 0;
        while let Some(event) = tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap() {
            assert!(matches!(event, XmppEvent::DirectMessage { .. }));
            count += 1;
        }
        assert_eq!(count, 42);
        client.close().await;
    }

    #[tokio::test]
    async fn failed_writes_preserve_complete_incoming_frames_behind_a_full_event_queue()
    {
        let mut framer = XmlFramer::new_client_stream();
        let mut wire = (0..42).map(|i| format!("<message type='chat' from='alice@example'><body>received-{i}</body></message>")).collect::<String>();
        wire.push_str("<message type='groupchat' from='room@example/alice'><body>retained room message</body></message>");
        wire.push_str("<message from='me@example'><result xmlns='urn:xmpp:mam:2' queryid='retained-archive' id='retained-id'><forwarded xmlns='urn:xmpp:forward:0'><message xmlns='jabber:client' from='alice@example/phone'><body>retained archive</body></message></forwarded></result></message><iq type='result' id='retained-archive'><fin xmlns='urn:xmpp:mam:2' complete='true'/></iq>");
        framer.feed(wire.as_bytes());
        let (mut client, mut events, _server) = test_connection(framer).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        tokio::time::timeout(Duration::from_secs(1), async {
            while events.len() < 32 { tokio::task::yield_now().await; }
        }).await.unwrap();
        client.join_room_with_context("room@example", "me", "original").await.unwrap();
        client.query_archive(archive_query("retained-archive", None)).await.unwrap();
        let (reply_tx, reply_rx) = oneshot::channel();
        client.pending_iqs.lock().unwrap().insert("waiting-reply".into(), PendingIq::new(reply_tx, None, &client.bound_jid));
        client.writer.lock().await.shutdown().await;
        assert!(client.send_presence().await.is_err());
        assert!(client.sm_state.closed.load(Ordering::SeqCst));
        let first = events.recv().await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(1), reply_rx).await.unwrap().is_err(), "reply waiter survived failed connection");
        let received = tokio::time::timeout(Duration::from_secs(1), async {
            let mut received = match first { XmppEvent::DirectMessage { body, .. } => vec![body], other => panic!("unexpected first event: {other:?}") };
            let mut room_message = false;
            let mut archive_message = false;
            let mut archive_complete = false;
            while let Some(event) = events.recv().await {
                match event {
                    XmppEvent::DirectMessage { body, .. } => received.push(body),
                    XmppEvent::RoomEvent { context, event, .. } => {
                        assert_eq!(context, "original");
                        assert!(matches!(*event, XmppEvent::RoomMessage { body, .. } if body == "retained room message"));
                        room_message = true;
                    }
                    XmppEvent::ArchiveMessage { query_id, body, .. } => {
                        assert_eq!(query_id, "retained-archive");
                        assert_eq!(body, "retained archive");
                        archive_message = true;
                    }
                    XmppEvent::ArchiveComplete { query_id, .. } => {
                        assert_eq!(query_id, "retained-archive");
                        archive_complete = true;
                    }
                    other => panic!("unexpected event during drain: {other:?}"),
                }
            }
            assert!(room_message, "pending room message was lost");
            assert!(archive_message && archive_complete, "received archive data was lost");
            received
        }).await.unwrap();
        assert_eq!(received, (0..42).map(|i| format!("received-{i}")).collect::<Vec<_>>());
        client.close().await;
    }

    #[tokio::test]
    async fn cancelling_a_partial_stanza_write_invalidates_the_session()
    {
        use tokio::io::AsyncReadExt;
        for managed in [false, true] {
            let (mut client, mut events, mut server) = test_connection_with_sm(XmlFramer::new_client_stream(), managed).await;
            assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
            read_until_written(&mut server, b"</presence>").await;
            let before = client.sm_state.outbound_count();
            let body = "x".repeat(16 * 1024 * 1024);
            let mut sending = Box::pin(client.send_message("alice@example", &body));
            let mut prefix = [0; 64];
            let n = tokio::select! {
                biased;
                result = &mut sending => panic!("large write completed without draining peer: {result:?}"),
                result = server.read(&mut prefix) => result.unwrap(),
                _ = tokio::time::sleep(Duration::from_secs(3)) => panic!("write made no progress"),
            };
            assert!(n > 0 && prefix[..n].starts_with(b"<message"));
            drop(sending);
            assert!(client.sm_state.closed.load(Ordering::SeqCst), "cancelled partial write left the session usable");
            assert_eq!(client.sm_state.outbound_count(), before);
            assert!(matches!(client.send_message("alice@example", "must not follow a fragment").await, Err(XmppError::Disconnected)));
            assert!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap().is_none());
            client.close().await;
        }
    }

    #[tokio::test]
    async fn cancelled_iq_requests_are_removed_from_the_registry()
    {
        let (mut client, _events, _server) = test_connection(XmlFramer::new_client_stream()).await;
        let registry = client.pending_iqs.clone();
        let mut request = Box::pin(client.ping(None, Duration::from_secs(60)));
        tokio::select! {
            result = &mut request => panic!("request unexpectedly completed: {:?}", result),
            _ = async { while registry.lock().unwrap().is_empty() { tokio::task::yield_now().await; } } => {}
        }
        drop(request);
        assert!(registry.lock().unwrap().is_empty());
        client.close().await;
    }

    fn archive_query<'a>(id: &'a str, to: Option<&'a str>) -> ArchiveQuery<'a>
    {
        ArchiveQuery { query_id: id, to, with: None, start: None, end: None,
            forward: false, cursor: None, max: 20 }
    }

    #[test]
    fn archive_origins_distinguish_rooms_from_their_occupants()
    {
        let room = PendingIq::archive(Some("room@conference.example"), "me@example/test");
        assert!(room.accepts(Some("room@conference.example"), false));
        for from in [None, Some("room@conference.example/mallory"), Some("other@conference.example"), Some("me@example")] {
            assert!(!room.accepts(from, false));
            assert!(!room.accepts(from, true));
        }
        assert!(room.accepts(Some("example"), true));
        assert!(room.accepts(Some("conference.example"), true));
        assert!(!room.accepts(Some("conference.example"), false));
        for to in [None, Some("me@example")] {
            let own = PendingIq::archive(to, "me@example/test");
            assert!(own.accepts(None, false));
            assert!(own.accepts(Some("me@example"), false));
            assert!(!own.accepts(Some("me@example/other"), false));
            assert!(!own.accepts(Some("example"), false));
        }
    }

    #[tokio::test]
    async fn archive_results_validate_complete_forwarded_sender_and_recipient_addresses() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.query_archive(archive_query("addresses", None)).await.unwrap();
        let long = format!("alice@example/{}", "a".repeat(1024));
        let mut replies = String::new();
        for (from, to) in [
            ("", ""), ("alice@example/", ""), ("alice@@example/phone", ""),
            ("alice@example/with&#9;tab", ""), (long.as_str(), ""),
            ("alice@example/phone", " to='me@example/'"),
            ("alice@example/phone", " to='me@@example/device'"),
            ("alice@example/phone", " to=''"),
        ] {
            replies.push_str(&format!("<message><result xmlns='urn:xmpp:mam:2' queryid='addresses' id='invalid'><forwarded xmlns='urn:xmpp:forward:0'><message xmlns='jabber:client' from='{from}'{to}><body>invalid</body></message></forwarded></result></message>"));
        }
        replies.push_str("<message><result xmlns='urn:xmpp:mam:2' queryid='addresses' id='valid'><forwarded xmlns='urn:xmpp:forward:0'><message xmlns='jabber:client' from='alice@EXAMPLE/team/phone' to='me@example/with spaces'><body>valid</body></message></forwarded></result></message><iq type='result' id='addresses'><fin xmlns='urn:xmpp:mam:2' complete='true'/></iq></stream:stream>");
        server.write_all(replies.as_bytes()).await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(),
            Some(XmppEvent::ArchiveMessage { from, to: Some(to), body, .. }) if from == "alice@EXAMPLE/team/phone" && to == "me@example/with spaces" && body == "valid"));
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(), Some(XmppEvent::ArchiveComplete { .. })));
        client.close().await;
        assert!(events.recv().await.is_none());
    }

    #[tokio::test]
    async fn archive_replies_require_an_open_query_and_the_exact_archive_sender()
    {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.query_archive(archive_query("own", None)).await.unwrap();
        client.query_archive(archive_query("room", Some("room@conference.example"))).await.unwrap();
        assert!(matches!(client.query_archive(archive_query("own", Some("evil.example"))).await, Err(XmppError::Protocol(_))));
        assert!(matches!(client.query_archive(archive_query("", None)).await, Err(XmppError::Protocol(_))));
        assert_eq!(client.pending_iqs.lock().unwrap().len(), 2);

        let result = |from: &str, query: &str, body: &str| format!(
            "<message {from}><result xmlns='urn:xmpp:mam:2' queryid='{query}' id='id-{body}'><forwarded xmlns='urn:xmpp:forward:0'><message xmlns='jabber:client' from='alice@example/phone'><body>{body}</body></message></forwarded></result></message>");
        let mut replies = String::new();
        replies.push_str(&result("from='mallory@example/phone'", "own", "forged"));
        replies.push_str(&result("from='room@conference.example/mallory'", "room", "forged"));
        replies.push_str(&result("from='room@conference.example'", "unknown", "unsolicited"));
        replies.push_str("<iq type='result' from='room@conference.example/mallory' id='room'><fin xmlns='urn:xmpp:mam:2' complete='true'/></iq>");
        replies.push_str("<iq type='result' from='me@example/other' id='own'><fin xmlns='urn:xmpp:mam:2' complete='true'/></iq>");
        replies.push_str("<iq type='set' from='room@conference.example' id='room'><fin xmlns='urn:xmpp:mam:2' complete='true'/></iq>");
        replies.push_str("<iq type='error' from='mallory@example/phone' id='own'/>");
        replies.push_str(&result("", "own", "personal"));
        replies.push_str(&result("from='room@conference.example'", "room", "group"));
        replies.push_str("<iq type='result' from='me@example' id='own'><fin xmlns='urn:xmpp:mam:2' complete='true'/></iq>");
        replies.push_str("<iq type='result' from='room@conference.example' id='room'><fin xmlns='urn:xmpp:mam:2' complete='true'/></iq>");
        replies.push_str(&result("from='room@conference.example'", "room", "late"));
        replies.push_str("<message type='chat' from='alice@example/phone'><body>barrier</body></message>");
        server.write_all(replies.as_bytes()).await.unwrap();
        for (query, body) in [("own", "personal"), ("room", "group")] {
            assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(),
                Some(XmppEvent::ArchiveMessage { query_id, body: actual, .. }) if query_id == query && actual == body));
        }
        for query in ["own", "room"] {
            assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(),
                Some(XmppEvent::ArchiveComplete { query_id, complete: true, .. }) if query_id == query));
        }
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(),
            Some(XmppEvent::DirectMessage { body, .. }) if body == "barrier"));
        assert!(events.try_recv().is_err());
        assert!(client.pending_iqs.lock().unwrap().is_empty());
        client.close().await;
    }

    #[tokio::test]
    async fn cancelling_an_archive_write_removes_its_registration()
    {
        let (mut client, _events, _server) = test_connection(XmlFramer::new_client_stream()).await;
        let registry = client.pending_iqs.clone();
        let writer = client.writer.clone();
        let guard = writer.lock().await;
        let mut query = Box::pin(client.query_archive(archive_query("cancel", None)));
        tokio::select! {
            result = &mut query => panic!("query unexpectedly completed: {result:?}"),
            _ = async { while !registry.lock().unwrap().contains_key("cancel") { tokio::task::yield_now().await; } } => {}
        }
        drop(query);
        assert!(registry.lock().unwrap().is_empty());
        assert!(!client.sm_state.closed.load(Ordering::SeqCst));
        drop(guard);
        client.send_presence().await.unwrap();
        client.close().await;
    }

    #[tokio::test]
    async fn archive_errors_match_the_open_query_without_an_id_prefix()
    {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.query_archive(archive_query("custom", Some("room@conference.example"))).await.unwrap();
        server.write_all(b"<iq type='error' from='room@conference.example/mallory' id='custom'/><message type='chat' from='alice@example'><body>barrier</body></message>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(),
            Some(XmppEvent::DirectMessage { body, .. }) if body == "barrier"));
        assert!(client.pending_iqs.lock().unwrap().contains_key("custom"));
        // Errors may echo a payload; they must not be mistaken for success.
        server.write_all(b"<iq type='error' from='conference.example' id='custom'><fin xmlns='urn:xmpp:mam:2'/></iq>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(),
            Some(XmppEvent::ArchiveFailed { query_id, .. }) if query_id == "custom"));
        assert!(client.pending_iqs.lock().unwrap().is_empty());
        client.close().await;
    }

    #[tokio::test]
    async fn failed_archive_writes_do_not_leave_open_queries()
    {
        let (mut client, mut events, _server) = test_connection(XmlFramer::new_client_stream()).await;
        client.writer.lock().await.shutdown().await;
        assert!(client.query_archive(archive_query("failed", None)).await.is_err());
        assert!(client.pending_iqs.lock().unwrap().is_empty());
        assert!(client.sm_state.closed.load(Ordering::SeqCst));
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        assert!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap().is_none());
        assert!(matches!(client.ping(None, Duration::from_secs(60)).await, Err(XmppError::Disconnected)));
        client.close().await;
    }

    #[test]
    fn archive_expiry_keeps_active_queries_and_pending_iq_callers()
    {
        let now = tokio::time::Instant::now();
        let pending: PendingIqs = Arc::new(Mutex::new(HashMap::new()));
        let mut expired = PendingIq::archive(None, "me@example/test");
        expired.touch_archive(now - ARCHIVE_IDLE_TIMEOUT);
        let mut active = PendingIq::archive(None, "me@example/test");
        active.touch_archive(now - Duration::from_secs(10));
        // A result arriving during a long transfer renews the idle timer.
        active.touch_archive(now);
        let (tx, mut rx) = oneshot::channel();
        pending.lock().unwrap().extend([
            ("expired".into(), expired),
            ("active".into(), active),
            ("writing".into(), PendingIq::archive(None, "me@example/test")),
            ("ping".into(), PendingIq::new(tx, None, "me@example/test")),
        ]);
        assert_eq!(expire_archive_queries(&pending, now), vec!["expired"]);
        assert!(expire_archive_queries(&pending, now + Duration::from_secs(59)).is_empty());
        assert_eq!(expire_archive_queries(&pending, now + ARCHIVE_IDLE_TIMEOUT), vec!["active"]);
        assert!(matches!(rx.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        assert!(pending.lock().unwrap().contains_key("writing"));
    }

    #[tokio::test]
    async fn silent_archive_queries_time_out_without_needing_more_network_traffic()
    {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut events, mut server) = test_connection(XmlFramer::new_client_stream()).await;
        assert!(matches!(events.recv().await, Some(XmppEvent::Connected)));
        client.query_archive(archive_query("idle", None)).await.unwrap();
        // Move only this request's deadline; the test still exercises the
        // real reader's timer and its wakeup while the socket remains idle.
        client.pending_iqs.lock().unwrap().get_mut("idle").unwrap()
            .touch_archive(tokio::time::Instant::now() - ARCHIVE_IDLE_TIMEOUT);
        client.pending_iq_changed.notify_one();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(),
            Some(XmppEvent::ArchiveFailed { query_id, reason }) if query_id == "idle" && reason.contains("timed out")));
        assert!(client.pending_iqs.lock().unwrap().is_empty());
        server.write_all(b"<message><result xmlns='urn:xmpp:mam:2' queryid='idle' id='late'><forwarded xmlns='urn:xmpp:forward:0'><message xmlns='jabber:client' from='alice@example'><body>late</body></message></forwarded></result></message><iq type='result' id='idle'><fin xmlns='urn:xmpp:mam:2' complete='true'/></iq><message type='chat' from='alice@example'><body>barrier</body></message>").await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap(),
            Some(XmppEvent::DirectMessage { body, .. }) if body == "barrier"));
        assert!(events.try_recv().is_err());
        client.query_archive(archive_query("retry", None)).await.unwrap();
        assert!(client.pending_iqs.lock().unwrap().contains_key("retry"));
        client.close().await;
    }

    #[tokio::test]
    async fn generated_iq_ids_skip_ids_reserved_by_archive_queries()
    {
        let (mut client, _events, _server) = test_connection(XmlFramer::new_client_stream()).await;
        client.query_archive(archive_query("ping_1", None)).await.unwrap();
        assert_eq!(client.next_iq_id("ping"), "ping_2");
        client.close().await;
    }

    #[tokio::test]
    async fn iq_timeout_bounds_the_writer_lock_and_cleans_up()
    {
        let (mut client, _events, _server) = test_connection(XmlFramer::new_client_stream()).await;
        let writer = client.writer.clone();
        let guard = writer.lock().await;
        let result = tokio::time::timeout(Duration::from_secs(1), client.ping(None, Duration::from_millis(20)))
            .await.expect("IQ exceeded its timeout waiting for the writer");
        assert!(matches!(result, Err(XmppError::Timeout(_))));
        assert!(client.pending_iqs.lock().unwrap().is_empty());
        drop(guard);
        client.close().await;
    }

    #[tokio::test]
    async fn requests_after_eof_fail_immediately_instead_of_waiting_for_timeouts()
    {
        let (mut client, _events, server) = test_connection(XmlFramer::new_client_stream()).await;
        client.sm_state.enable();
        drop(server);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !client.task.is_finished() { tokio::task::yield_now().await; }
        }).await.unwrap();
        assert!(matches!(client.ping(None, Duration::from_secs(60)).await, Err(XmppError::Disconnected)));
        assert!(matches!(client.request_ack(Duration::from_secs(60)).await, Err(XmppError::Disconnected)));
        assert!(client.pending_iqs.lock().unwrap().is_empty());
        client.close().await;
    }

}
