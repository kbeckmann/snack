//! `libxmpp` is a small async XMPP client library built on Tokio.
//!
//! It currently supports connecting to an XMPP server with STARTTLS and
//! SASL authentication, joining and leaving Multi-User Chat (MUC) rooms,
//! sending and receiving group messages, and exchanging one-to-one chat
//! messages. Server events are delivered as [`XmppEvent`] values on an
//! [`mpsc::Receiver`] returned from [`XmppClient::new`].
//!
//! # Example
//!
//! ```no_run
//! use xmpp::{XmppClient, XmppEvent};
//!
//! # async fn run() -> Result<(), String> {
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
use tokio::task::JoinHandle;
use std::sync::Arc;
use std::collections::{HashMap, HashSet};

mod tcp_stream;
mod xml_framer;
mod stanza;
mod xmpp;

use stanza::Stanza;
use xml_framer::XmlFramer;

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
    /// `timestamp` is set when the message carries a delayed-delivery
    /// stamp (e.g. history replay on join).
    RoomMessage { room: String, nick: String, body: String, timestamp: Option<String> },
    /// A MUC room's subject was set or changed.
    RoomSubject { room: String, subject: String },
    /// A presence stanza of type `error` was received. `error_type` is
    /// the XMPP error class (e.g. `auth`, `cancel`), `condition` is the
    /// defined condition (e.g. `forbidden`, `not-authorized`), and
    /// `text` is the optional human-readable description.
    PresenceError { from: String, error_type: String, condition: String, text: Option<String> },
    /// A one-to-one chat message was received from `from`. `timestamp`
    /// is set when the message carries a delayed-delivery stamp.
    DirectMessage { from: String, body: String, timestamp: Option<String> },
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
    writer: tcp_stream::TcpWriter,
    bound_jid: String,
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
    pub async fn new(jid: &str, password: &str) -> Result<(Self, mpsc::Receiver<XmppEvent>), String>
    {
        let (event_tx, event_rx) = mpsc::channel(32);

        // Setup TCP connection, TLS, and SASL authentication.
        let (bound_jid, tcp) = xmpp::setup_connection(&event_tx, jid, password).await?;

        // We now have a working xmpp connection, split and spawn reader loop.
        let (mut reader, mut writer) = tcp.split()?;

        // Broadcast initial available presence so the server routes
        // incoming directed messages (e.g. one-to-one chats) to this resource.
        writer.write(b"<presence/>").await?;
        let shutdown = Arc::new(Notify::new());
        let shutdown_clone = shutdown.clone();
        let event_tx_loop = event_tx.clone();

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
                    xmpp::process_stanza(&stanza_xml, &event_tx_loop, &mut pending_joins, &mut pending_messages, &mut joined_rooms).await;
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

        return Ok((Self { shutdown, task, writer, bound_jid }, event_rx));
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
    pub async fn join_room(&mut self, room_jid: &str, nick: &str) -> Result<(), String>
    {
        let presence = stanza::muc::MucJoinPresence::new(room_jid.to_string(), nick.to_string());
        return self.writer.write(&presence.as_bytes()).await;
    }

    /// Leave a MUC room previously joined as `nick`.
    ///
    /// On success a [`RoomLeft`](XmppEvent::RoomLeft) event is emitted
    /// once the server confirms the departure.
    pub async fn leave_room(&mut self, room_jid: &str, nick: &str) -> Result<(), String>
    {
        let presence = stanza::muc::MucLeavePresence::new(room_jid.to_string(), nick.to_string());
        return self.writer.write(&presence.as_bytes()).await;
    }

    /// Send a group chat message to a MUC room.
    ///
    /// `room_jid` is the bare JID of the room. The local user must
    /// already have joined the room.
    pub async fn send_room_message(&mut self, room_jid: &str, body: &str) -> Result<(), String>
    {
        let msg = stanza::muc::MucGroupMessage::new(room_jid.to_string(), body.to_string());
        return self.writer.write(&msg.as_bytes()).await;
    }

    /// Send a one-to-one chat message.
    ///
    /// `to` is the recipient's bare or full JID. Replies will arrive
    /// as [`DirectMessage`](XmppEvent::DirectMessage) events.
    pub async fn send_message(&mut self, to: &str, body: &str) -> Result<(), String>
    {
        let msg = stanza::chat::ChatMessage::new(to.to_string(), body.to_string());
        return self.writer.write(&msg.as_bytes()).await;
    }

    /// Shut down the client.
    ///
    /// Stops the background reader task, waits for it to exit, and
    /// closes the underlying socket. After this call the event
    /// receiver returned from [`XmppClient::new`] will yield `None`.
    pub async fn close(mut self)
    {
        self.shutdown.notify_one();
        let _ = self.task.await;
        self.writer.shutdown().await;
    }
}
