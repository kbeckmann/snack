use tokio::sync::mpsc;
use std::collections::{ HashMap, HashSet };

use crate::PendingIqs;

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::{ XmppEvent, RoomMember, LocalDiscoState, SharedWriter, SharedSmState };
use crate::error::XmppError;
use crate::tcp_stream::Tcp;
use crate::stanza;
use crate::stanza::Stanza;
use crate::xml_framer::XmlFramer;

/// Establish and negotiate an XMPP session: TCP, STARTTLS, SASL, resource
/// binding and — when the server offers it — XEP-0198 stream management.
///
/// The returned `bool` reports whether stream management was successfully
/// enabled, so the caller can start counting stanzas in both directions.
pub async fn setup_connection(event_tx: &mpsc::Sender<XmppEvent>, jid: &str, password: &str) -> Result<(String, Tcp, bool), XmppError>
{
    let (username, domain) = parse_jid(jid)?;

    let _ = event_tx.send(XmppEvent::Connecting).await;
    let mut tcp = Tcp::new()
        .connect(domain.to_string(), 5222).await?;

    let mut framer = XmlFramer::new();

    log::debug!("Opening initial stream...");

    tcp.send(&stanza::stream::Stream::new(jid.to_string(), domain.to_string()).as_bytes()).await?;
    let (_stream, mut features) = read_stream_and_features(&mut tcp, &mut framer).await?;

    // TLS
    if features.starttls.is_some()
    {
        let _ = event_tx.send(XmppEvent::EstablishingTls).await;
        log::debug!("Starting TLS negotiation...");

        tcp.send(&stanza::stream::StartTlsRequest.as_bytes()).await?;
        let response = read_frame(&mut tcp, &mut framer).await?;

        if response.contains("<failure")
        {
            return Err(XmppError::Tls("server refused STARTTLS".to_string()));
        }

        tcp = tcp.add_tls().await?;
        framer.reset();

        log::debug!("TLS established");
        log::debug!("Reopening stream over TLS...");

        tcp.send(&stanza::stream::Stream::new(jid.to_string(), domain.to_string()).as_bytes()).await?;
        let (_stream, new_features) = read_stream_and_features(&mut tcp, &mut framer).await?;
        features = new_features;
    }

    // Never send credentials over an unencrypted channel.
    if !tcp.is_encrypted()
    {
        return Err(XmppError::TlsRequired);
    }

    // SASL authentication
    if let Some(ref mechs) = features.mechanisms
    {
        let mechanism = select_mechanism(&mechs.mechanism)?;

        let _ = event_tx.send(XmppEvent::Authenticating).await;
        log::debug!("Starting SASL {:?} authentication...", mechanism);

        do_sasl(&mut tcp, &mut framer, username, password, &mechanism).await?;
        framer.reset();

        log::debug!("SASL authentication successful");

        log::debug!("Reopening stream after authentication...");
        tcp.send(&stanza::stream::Stream::new(jid.to_string(), domain.to_string()).as_bytes()).await?;
        let (_stream, new_features) = read_stream_and_features(&mut tcp, &mut framer).await?;
        features = new_features;
    }
    else
    {
        // Require authentication.
        return Err(XmppError::Protocol("server does not offer SASL mechanisms".to_string()));
    }

    // Resource binding
    let bound_jid = if features.bind.is_some()
    {
        log::debug!("Binding resource...");
        do_bind(&mut tcp, &mut framer).await?
    }
    else
    {
        jid.to_string()
    };

    log::info!("Bound JID: {}", bound_jid);

    // XEP-0198 stream management. Enabled once the resource is bound, so the
    // server counts our stanzas (and we count its) from this point on. Requested
    // only when advertised; a refusal is non-fatal and simply leaves it off.
    let sm_enabled = if !features.sm.is_empty()
    {
        enable_stream_management(&mut tcp, &mut framer).await?
    }
    else
    {
        false
    };

    return Ok((bound_jid, tcp, sm_enabled));
}

/// Ask the server to enable XEP-0198 stream management and report whether it
/// agreed.
///
/// Sends `<enable/>` and inspects the reply: `<enabled/>` turns management on,
/// while a `<failed/>` (or anything else) leaves it off. A refusal is not an
/// error — the session is fully usable without stream management.
async fn enable_stream_management(tcp: &mut Tcp, framer: &mut XmlFramer) -> Result<bool, XmppError>
{
    log::debug!("Requesting stream management (XEP-0198)...");

    tcp.send(&stanza::sm::Enable.as_bytes()).await?;
    let frame = read_frame(tcp, framer).await?;

    // Distinguish `<enabled/>` from `<failed/>` by the element name rather than
    // by deserializing, since a `<failed/>` would parse into the same optional
    // fields and be mistaken for success.
    match peek_root(&frame)
    {
        Some((root, _, _)) if root == "enabled" =>
        {
            match stanza::sm::Enabled::from_xml(&frame)
            {
                Ok(enabled) => log::debug!("Stream management enabled: {:?}", enabled),
                Err(_) => log::debug!("Stream management enabled"),
            }
            return Ok(true);
        }
        _ =>
        {
            log::warn!("Server declined stream management: {}", frame);
            return Ok(false);
        }
    }
}

pub fn parse_jid(jid: &str) -> Result<(&str, &str), XmppError>
{
    let at = jid.find('@').ok_or_else(|| XmppError::InvalidJid(format!("missing '@': {}", jid)))?;
    let username = &jid[..at];
    let domain = &jid[at + 1..];

    // Strip any resource part (after '/') so a full JID like
    // `user@example.com/resource` still resolves to the bare domain.
    let domain = match domain.find('/')
    {
        Some(slash) => &domain[..slash],
        None => domain,
    };

    return Ok((username, domain));
}

async fn read_frame(tcp: &mut Tcp, framer: &mut XmlFramer) -> Result<String, XmppError>
{
    loop
    {
        if let Some(frame) = framer.try_next()
        {
            return Ok(frame);
        }

        let data = tcp.recv().await?;
        framer.feed(&data);
    }
}

async fn read_stream_and_features(
    tcp: &mut Tcp,
    framer: &mut XmlFramer,
) -> Result<(stanza::stream::Stream, stanza::stream::StreamFeatures), XmppError>
{
    let header = read_frame(tcp, framer).await?;
    let (stream, _) = stanza::stream::Stream::from_xml(&header).map_err(XmppError::Parse)?;

    log::debug!("Stream opened: {:?}", stream);

    let features_xml = read_frame(tcp, framer).await?;
    let features = stanza::stream::StreamFeatures::from_xml(&features_xml).map_err(XmppError::Parse)?;

    log::debug!("Stream features: {:?}", features);

    return Ok((stream, features));
}

#[derive(Debug)]
enum SaslMechanism
{
    ScramSha512,
    ScramSha256,
    ScramSha1,
    Plain,
}

fn select_mechanism(mechanisms: &[String]) -> Result<SaslMechanism, XmppError>
{
    if mechanisms.iter().any(|m| m == "SCRAM-SHA-512") { return Ok(SaslMechanism::ScramSha512); }
    if mechanisms.iter().any(|m| m == "SCRAM-SHA-256") { return Ok(SaslMechanism::ScramSha256); }
    if mechanisms.iter().any(|m| m == "SCRAM-SHA-1") { return Ok(SaslMechanism::ScramSha1); }
    if mechanisms.iter().any(|m| m == "PLAIN") { return Ok(SaslMechanism::Plain); }

    return Err(XmppError::NoSaslMechanism);
}

async fn do_sasl(
    tcp: &mut Tcp,
    framer: &mut XmlFramer,
    username: &str,
    password: &str,
    mechanism: &SaslMechanism,
) -> Result<(), XmppError>
{
    match mechanism
    {
        SaslMechanism::ScramSha512 =>
        {
            let mut scram = stanza::sasl::ScramSha512Client::new(username, password, "SCRAM-SHA-512");
            do_sasl_scram(tcp, framer, &mut scram).await
        }
        SaslMechanism::ScramSha256 =>
        {
            let mut scram = stanza::sasl::ScramSha256Client::new(username, password, "SCRAM-SHA-256");
            do_sasl_scram(tcp, framer, &mut scram).await
        }
        SaslMechanism::ScramSha1 =>
        {
            let mut scram = stanza::sasl::ScramSha1Client::new(username, password, "SCRAM-SHA-1");
            do_sasl_scram(tcp, framer, &mut scram).await
        }
        SaslMechanism::Plain =>
        {
            let auth = stanza::sasl::PlainAuth::new(username, password);
            do_sasl_plain(tcp, framer, &auth).await
        }
    }
}

async fn do_sasl_scram(
    tcp: &mut Tcp,
    framer: &mut XmlFramer,
    scram: &mut dyn stanza::sasl::ScramAuth,
) -> Result<(), XmppError>
{
    // Send <auth>
    tcp.send(scram.auth_xml().as_bytes()).await?;

    // Read <challenge>
    let challenge_xml = read_frame(tcp, framer).await?;
    if stanza::sasl::is_failure(&challenge_xml)
    {
        return Err(XmppError::Auth(challenge_xml));
    }

    let challenge_b64 = stanza::sasl::parse_challenge(&challenge_xml).map_err(XmppError::Parse)?;

    // Send <response>
    let response_xml = scram.response_xml(&challenge_b64).map_err(XmppError::Auth)?;
    tcp.send(response_xml.as_bytes()).await?;

    // Read <success>
    let success_xml = read_frame(tcp, framer).await?;
    if stanza::sasl::is_failure(&success_xml)
    {
        return Err(XmppError::Auth(success_xml));
    }

    let success_b64 = stanza::sasl::parse_success(&success_xml).map_err(XmppError::Parse)?;
    scram.verify_success(&success_b64).map_err(XmppError::Auth)?;

    return Ok(());
}

async fn do_sasl_plain(
    tcp: &mut Tcp,
    framer: &mut XmlFramer,
    auth: &stanza::sasl::PlainAuth,
) -> Result<(), XmppError>
{
    tcp.send(auth.auth_xml().as_bytes()).await?;

    let response_xml = read_frame(tcp, framer).await?;
    if stanza::sasl::is_failure(&response_xml)
    {
        return Err(XmppError::Auth(response_xml));
    }

    return Ok(());
}

async fn do_bind(
    tcp: &mut Tcp,
    framer: &mut XmlFramer,
) -> Result<String, XmppError>
{
    let bind_req = stanza::bind::BindRequest::new("bind_1".to_string(), None);
    tcp.send(&bind_req.as_bytes()).await?;

    let result_xml = read_frame(tcp, framer).await?;
    let result = stanza::bind::BindResult::from_xml(&result_xml).map_err(XmppError::Parse)?;

    if result.iq_type != "result"
    {
        return Err(XmppError::Bind(format!("server returned {:?}", result)));
    }

    return result.bind
        .and_then(|b| b.jid)
        .ok_or_else(|| XmppError::Bind("no JID in bind result".to_string()));
}

/// Inspect only the root element of a stanza, returning its local name and the
/// values of its `type` and `id` attributes. Dispatch is driven by this parsed
/// view rather than substring matching, so body text can never be mistaken for
/// markup.
fn peek_root(xml: &str) -> Option<(String, Option<String>, Option<String>)>
{
    let mut reader = Reader::from_str(xml);

    loop
    {
        match reader.read_event()
        {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) =>
            {
                let local = e.local_name();
                let name = std::str::from_utf8(local.as_ref()).ok()?.to_string();

                let mut stanza_type = None;
                let mut id = None;
                for attr in e.attributes().flatten()
                {
                    match attr.key.local_name().as_ref()
                    {
                        b"type" => stanza_type = attr.unescape_value().ok().map(|v| v.to_string()),
                        b"id" => id = attr.unescape_value().ok().map(|v| v.to_string()),
                        _ => {}
                    }
                }

                return Some((name, stanza_type, id));
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

/// Write an outgoing **stanza** through the shared writer, counting it toward
/// XEP-0198 stream management when enabled.
///
/// The count is bumped while the writer lock is held, so the outbound counter
/// advances in the exact order stanzas hit the wire even when the owning client
/// and the reader task (auto-replying to IQs) write concurrently. Only real
/// stanzas (`message`/`presence`/`iq`) go through here; stream-management
/// control elements (`<r/>`, `<a/>`) are written directly and never counted.
pub(crate) async fn write_stanza(writer: &SharedWriter, sm: &SharedSmState, bytes: &[u8]) -> Result<(), XmppError>
{
    let mut w = writer.lock().await;
    w.write(bytes).await?;
    if sm.is_enabled()
    {
        sm.record_outbound();
    }

    return Ok(());
}

pub async fn process_stanza(
    xml: &str,
    event_tx: &mpsc::Sender<XmppEvent>,
    pending_joins: &mut HashMap<String, Vec<RoomMember>>,
    pending_messages: &mut HashMap<String, Vec<XmppEvent>>,
    joined_rooms: &mut HashSet<String>,
    pending_iqs: &PendingIqs,
    writer: &SharedWriter,
    local_disco: &LocalDiscoState,
    sm: &SharedSmState,
)
{
    let (root, stanza_type, id) = match peek_root(xml)
    {
        Some(v) => v,
        None => return,
    };

    // XEP-0198 control elements are not stanzas: handle them without counting or
    // routing, then stop. `<r/>` must be answered with our handled-stanza count;
    // `<a/>` reports how many of our stanzas the server has handled.
    match root.as_str()
    {
        "r" =>
        {
            let ack = stanza::sm::Ack::new(sm.inbound_count());
            if let Err(e) = writer.lock().await.write(&ack.as_bytes()).await
            {
                log::warn!("Failed to answer stream-management <r/>: {}", e);
            }
            return;
        }
        "a" =>
        {
            match stanza::sm::Ack::from_xml(xml)
            {
                Ok(ack) => sm.record_ack(ack.h()),
                Err(e) => log::warn!("Failed to parse stream-management <a/>: {}", e),
            }
            return;
        }
        _ => {}
    }

    // Count every stanza we handle so the server's next `<r/>` can be answered.
    if sm.is_enabled() && matches!(root.as_str(), "message" | "presence" | "iq")
    {
        sm.record_inbound();
    }

    match (root.as_str(), stanza_type.as_deref())
    {
        ("presence", Some("error")) =>
        {
            process_presence_error(xml, event_tx, pending_joins, pending_messages).await;
        }
        ("presence", _) =>
        {
            // Surface any XEP-0115 caps first; the same presence is then handled
            // for MUC roster/room semantics.
            process_entity_caps(xml, event_tx).await;
            process_muc_presence(xml, event_tx, pending_joins, pending_messages, joined_rooms).await;
        }
        ("message", Some("chat")) =>
        {
            process_chat_message(xml, event_tx).await;
        }
        ("message", Some("groupchat")) =>
        {
            process_groupchat_message(xml, event_tx, pending_joins, pending_messages, joined_rooms).await;
        }
        ("iq", Some("result")) | ("iq", Some("error")) =>
        {
            // Wake any caller awaiting this IQ's reply, handing over the raw
            // reply stanza so it can parse the response body. Both a result and
            // an error count as a reply: either proves the peer is reachable.
            if let Some(id) = id
            {
                if let Some(tx) = pending_iqs.lock().unwrap().remove(&id)
                {
                    let _ = tx.send(xml.to_string());
                }
            }
        }
        ("iq", Some("get")) =>
        {
            // Another entity is querying us; answer the ones we understand
            // (XEP-0030 service discovery). Unhandled gets are ignored.
            process_iq_get(xml, writer, local_disco, sm).await;
        }
        _ => {}
    }
}

/// Answer an incoming `<iq type='get'>`. XEP-0199 pings and XEP-0030 service
/// discovery queries are handled; anything else is ignored.
async fn process_iq_get(
    xml: &str,
    writer: &SharedWriter,
    local_disco: &LocalDiscoState,
    sm: &SharedSmState,
)
{
    let response_xml = match build_iq_get_reply(xml, local_disco)
    {
        Some(reply) => reply,
        None => return,
    };

    // The reply is an `<iq type='result'>`, a counted stanza under stream
    // management, so route it through `write_stanza`.
    if let Err(e) = write_stanza(writer, sm, response_xml.as_bytes()).await
    {
        log::warn!("Failed to reply to incoming iq get: {}", e);
    }
}

/// Build the reply to an incoming `<iq type='get'>`, or `None` if it is a kind
/// we do not answer (or lacks the `from`/`id` needed to reply).
///
/// Kept synchronous and separate from the socket write so the disco state lock
/// is never held across an await, and so the routing is unit-testable without a
/// connection.
fn build_iq_get_reply(xml: &str, local_disco: &LocalDiscoState) -> Option<String>
{
    // XEP-0199 ping: acknowledge with an empty result.
    if let Ok(ping) = stanza::ping::IncomingPing::from_xml(xml)
    {
        if ping.ping.xmlns == stanza::ping::PING_NS
        {
            let to = ping.from?;
            let id = ping.id?;
            return Some(stanza::ping::PongResponse::new(id, to).to_xml());
        }
    }

    // XEP-0030 service discovery: advertise our identities, features, or items.
    if let Ok(query) = stanza::disco::IncomingDiscoQuery::from_xml(xml)
    {
        let to = query.from?;
        let id = query.id?;
        let node = query.query.node;

        return match query.query.xmlns.as_str()
        {
            stanza::disco::DISCO_INFO_NS =>
            {
                let info = local_disco.lock().unwrap().info.clone();
                Some(stanza::disco::DiscoInfoResponse::new(id, to, node, info).to_xml())
            }
            stanza::disco::DISCO_ITEMS_NS =>
            {
                let items = local_disco.lock().unwrap().items.clone();
                Some(stanza::disco::DiscoItemsResponse::new(id, to, node, items).to_xml())
            }
            // A query in some other namespace is not service discovery.
            _ => None,
        };
    }

    return None;
}

/// Emit an [`XmppEvent::EntityCapabilities`] when an incoming presence carries
/// an XEP-0115 `<c/>` element.
///
/// A presence with no caps element (or no `from` to attribute them to) produces
/// no event, and this never consumes the presence: MUC handling of the same
/// stanza runs independently afterwards.
async fn process_entity_caps(xml: &str, event_tx: &mpsc::Sender<XmppEvent>)
{
    let presence = match stanza::caps::PresenceCaps::from_xml(xml)
    {
        Ok(p) => p,
        Err(e) =>
        {
            log::warn!("Failed to parse presence for entity caps: {}", e);
            return;
        }
    };

    // Clone the caps fields out before moving `from`, ending the borrow.
    let (node, ver, hash) = match presence.caps()
    {
        Some(caps) => (caps.node.clone(), caps.ver.clone(), caps.hash.clone()),
        None => return,
    };

    let from = match presence.from
    {
        Some(from) => from,
        None => return,
    };

    let _ = event_tx.send(XmppEvent::EntityCapabilities { from, node, ver, hash }).await;
}

async fn process_presence_error(
    xml: &str,
    event_tx: &mpsc::Sender<XmppEvent>,
    pending_joins: &mut HashMap<String, Vec<RoomMember>>,
    pending_messages: &mut HashMap<String, Vec<XmppEvent>>,
)
{
    let error = match stanza::muc::PresenceErrorStanza::from_xml(xml)
    {
        Ok(e) => e,
        Err(e) =>
        {
            log::warn!("Failed to parse presence error: {}", e);
            return;
        }
    };

    let room = match error.from.find('/')
    {
        Some(slash) => &error.from[..slash],
        None => &error.from,
    };

    pending_joins.remove(room);
    pending_messages.remove(room);

    let _ = event_tx.send(XmppEvent::PresenceError
    {
        from: error.from,
        error_type: error.error_type,
        condition: error.condition,
        text: error.text,
    }).await;
}

async fn process_muc_presence(
    xml: &str,
    event_tx: &mpsc::Sender<XmppEvent>,
    pending_joins: &mut HashMap<String, Vec<RoomMember>>,
    pending_messages: &mut HashMap<String, Vec<XmppEvent>>,
    joined_rooms: &mut HashSet<String>,
)
{
    let presence = match stanza::muc::MucPresence::from_xml(xml)
    {
        Ok(p) => p,
        Err(e) =>
        {
            log::warn!("Failed to parse MUC presence: {}", e);
            return;
        }
    };

    let (room, nick) = match presence.room_and_nick()
    {
        Some(v) => v,
        None => return,
    };

    let is_leave = presence.presence_type.as_deref() == Some("unavailable");

    if is_leave && joined_rooms.contains(room)
    {
        if presence.is_self_presence()
        {
            joined_rooms.remove(room);
            let _ = event_tx.send(XmppEvent::RoomLeft(room.to_string())).await;
        }
        else
        {
            let _ = event_tx.send(XmppEvent::MemberLeft
            {
                room: room.to_string(),
                nick: nick.to_string(),
            }).await;
        }
        return;
    }

    if let Some(x) = presence.muc_user_x()
    {
        let member = RoomMember
        {
            jid: x.jid().map(|s| s.to_string()),
            nick: nick.to_string(),
            affiliation: x.items().next().and_then(|i| i.affiliation.clone()).unwrap_or_default(),
            role: x.items().next().and_then(|i| i.role.clone()).unwrap_or_default(),
            show: presence.show().map(|s| s.to_string()),
            status: presence.status().map(|s| s.to_string()),
        };

        if joined_rooms.contains(room)
        {
            let _ = event_tx.send(XmppEvent::MemberJoined
            {
                room: room.to_string(),
                member,
            }).await;
        }
        else
        {
            let members = pending_joins.entry(room.to_string()).or_default();
            members.push(member);

            if presence.is_self_presence()
            {
                let members = pending_joins.remove(room).unwrap_or_default();
                joined_rooms.insert(room.to_string());
                let _ = event_tx.send(XmppEvent::RoomJoined
                {
                    room: room.to_string(),
                    members,
                }).await;

                for event in pending_messages.remove(room).unwrap_or_default()
                {
                    let _ = event_tx.send(event).await;
                }
            }
        }
    }
}

async fn process_chat_message(xml: &str, event_tx: &mpsc::Sender<XmppEvent>)
{
    let msg = match stanza::chat::IncomingChatMessage::from_xml(xml)
    {
        Ok(m) => m,
        Err(e) =>
        {
            log::warn!("Failed to parse chat message: {}", e);
            return;
        }
    };

    if let (Some(from), Some(body)) = (msg.from, msg.body)
    {
        let delay = msg.delay.map(Into::into);
        let _ = event_tx.send(XmppEvent::DirectMessage { from, body, delay }).await;
    }
}

async fn process_groupchat_message(
    xml: &str,
    event_tx: &mpsc::Sender<XmppEvent>,
    pending_joins: &mut HashMap<String, Vec<RoomMember>>,
    pending_messages: &mut HashMap<String, Vec<XmppEvent>>,
    joined_rooms: &mut HashSet<String>,
)
{
    let msg = match stanza::muc::MucMessage::from_xml(xml)
    {
        Ok(m) => m,
        Err(e) =>
        {
            log::warn!("Failed to parse MUC message: {}", e);
            return;
        }
    };

    let (room, nick) = match msg.room_and_nick()
    {
        Some(v) => v,
        None => return,
    };

    if joined_rooms.contains(room)
    {
        if let Some(ref subject) = msg.subject
        {
            let _ = event_tx.send(XmppEvent::RoomSubject
            {
                room: room.to_string(),
                subject: subject.clone(),
            }).await;
        }

        if let Some(ref body) = msg.body
        {
            let delay = msg.delay.as_ref().map(|d| d.clone().into());
            let _ = event_tx.send(XmppEvent::RoomMessage
            {
                room: room.to_string(),
                nick: nick.to_string(),
                body: body.clone(),
                delay,
            }).await;
        }
    }
    else if pending_joins.contains_key(room)
    {
        let messages = pending_messages.entry(room.to_string()).or_default();

        if let Some(ref subject) = msg.subject
        {
            messages.push(XmppEvent::RoomSubject
            {
                room: room.to_string(),
                subject: subject.clone(),
            });
        }

        if let Some(ref body) = msg.body
        {
            let delay = msg.delay.as_ref().map(|d| d.clone().into());
            messages.push(XmppEvent::RoomMessage
            {
                room: room.to_string(),
                nick: nick.to_string(),
                body: body.clone(),
                delay,
            });
        }
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn parse_jid_strips_resource()
    {
        let (user, domain) = parse_jid("user@example.com/resource").unwrap();
        assert_eq!(user, "user");
        assert_eq!(domain, "example.com");
    }

    #[test]
    fn parse_jid_requires_at()
    {
        assert!(matches!(parse_jid("no-at-sign"), Err(XmppError::InvalidJid(_))));
    }

    #[test]
    fn peek_root_reads_name_and_type()
    {
        let (name, typ, _id) = peek_root("<message type='chat' from='a@b'><body>hi</body></message>").unwrap();
        assert_eq!(name, "message");
        assert_eq!(typ.as_deref(), Some("chat"));
    }

    #[test]
    fn peek_root_ignores_body_text()
    {
        // A body that contains markup-like text must not change the dispatch:
        // the root element is still a type='chat' message.
        let (name, typ, _id) = peek_root(
            "<message type='chat' from='a@b'><body>look: type='groupchat'</body></message>"
        ).unwrap();
        assert_eq!(name, "message");
        assert_eq!(typ.as_deref(), Some("chat"));
    }

    #[test]
    fn peek_root_handles_self_closing()
    {
        let (name, typ, _id) = peek_root("<presence type='unavailable'/>").unwrap();
        assert_eq!(name, "presence");
        assert_eq!(typ.as_deref(), Some("unavailable"));
    }

    #[test]
    fn peek_root_reads_iq_id()
    {
        let (name, typ, id) = peek_root("<iq type='result' id='ping_1'/>").unwrap();
        assert_eq!(name, "iq");
        assert_eq!(typ.as_deref(), Some("result"));
        assert_eq!(id.as_deref(), Some("ping_1"));
    }

    /// Build a [`SharedWriter`] backed by a real loopback socket, returning the
    /// server end so a test can read back whatever the client task writes.
    async fn loopback_writer() -> (SharedWriter, tokio::net::TcpStream)
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (accepted, connected) = tokio::join!(listener.accept(), tokio::net::TcpStream::connect(addr));
        let server = accepted.unwrap().0;
        let (_reader, write_half) = tokio::io::split(connected.unwrap());
        let writer = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::tcp_stream::TcpWriter::Plain(write_half),
        ));
        return (writer, server);
    }

    /// A fresh, disabled stream-management state for tests that do not exercise
    /// XEP-0198.
    fn disabled_sm() -> SharedSmState
    {
        return std::sync::Arc::new(crate::SmState::new());
    }

    #[tokio::test]
    async fn iq_result_wakes_pending_caller()
    {
        let (event_tx, _event_rx) = mpsc::channel(8);
        let mut pending_joins = HashMap::new();
        let mut pending_messages = HashMap::new();
        let mut joined_rooms = HashSet::new();

        let pending_iqs: PendingIqs = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (tx, rx) = tokio::sync::oneshot::channel();
        pending_iqs.lock().unwrap().insert("ping_1".to_string(), tx);

        let (writer, _server) = loopback_writer().await;
        let local_disco: LocalDiscoState = std::sync::Arc::new(std::sync::Mutex::new(crate::LocalDisco::default()));

        process_stanza(
            "<iq type='result' id='ping_1'/>",
            &event_tx,
            &mut pending_joins,
            &mut pending_messages,
            &mut joined_rooms,
            &pending_iqs,
            &writer,
            &local_disco,
            &disabled_sm(),
        ).await;

        // The caller receives the raw reply stanza so it can parse any payload.
        assert_eq!(rx.await.unwrap(), "<iq type='result' id='ping_1'/>");
        assert!(pending_iqs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn disco_info_get_is_answered()
    {
        use tokio::io::AsyncReadExt;

        let (event_tx, _event_rx) = mpsc::channel(8);
        let mut pending_joins = HashMap::new();
        let mut pending_messages = HashMap::new();
        let mut joined_rooms = HashSet::new();
        let pending_iqs: PendingIqs = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));

        let (writer, mut server) = loopback_writer().await;
        let local_disco: LocalDiscoState = std::sync::Arc::new(std::sync::Mutex::new(crate::LocalDisco::default()));

        process_stanza(
            "<iq type='get' from='asker@example.com/x' id='q1'>\
             <query xmlns='http://jabber.org/protocol/disco#info'/></iq>",
            &event_tx,
            &mut pending_joins,
            &mut pending_messages,
            &mut joined_rooms,
            &pending_iqs,
            &writer,
            &local_disco,
            &disabled_sm(),
        ).await;

        let mut buf = vec![0u8; 4096];
        let n = server.read(&mut buf).await.unwrap();
        let response = String::from_utf8_lossy(&buf[..n]);

        assert!(response.contains("type='result'"), "got: {}", response);
        assert!(response.contains("to='asker@example.com/x'"), "got: {}", response);
        assert!(response.contains("id='q1'"), "got: {}", response);
        assert!(response.contains("<identity category='client' type='bot'"), "got: {}", response);
        assert!(response.contains("<feature var='http://jabber.org/protocol/disco#info'/>"), "got: {}", response);
    }

    #[tokio::test]
    async fn ping_get_is_answered_with_pong()
    {
        use tokio::io::AsyncReadExt;

        let (event_tx, _event_rx) = mpsc::channel(8);
        let mut pending_joins = HashMap::new();
        let mut pending_messages = HashMap::new();
        let mut joined_rooms = HashSet::new();
        let pending_iqs: PendingIqs = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));

        let (writer, mut server) = loopback_writer().await;
        let local_disco: LocalDiscoState = std::sync::Arc::new(std::sync::Mutex::new(crate::LocalDisco::default()));

        process_stanza(
            "<iq type='get' from='peer@example.com/x' id='p1'><ping xmlns='urn:xmpp:ping'/></iq>",
            &event_tx,
            &mut pending_joins,
            &mut pending_messages,
            &mut joined_rooms,
            &pending_iqs,
            &writer,
            &local_disco,
            &disabled_sm(),
        ).await;

        let mut buf = vec![0u8; 4096];
        let n = server.read(&mut buf).await.unwrap();
        let response = String::from_utf8_lossy(&buf[..n]);

        assert_eq!(response, "<iq type='result' to='peer@example.com/x' id='p1'/>");
    }

    #[tokio::test]
    async fn unhandled_get_is_ignored()
    {
        use tokio::io::AsyncReadExt;

        let (event_tx, _event_rx) = mpsc::channel(8);
        let mut pending_joins = HashMap::new();
        let mut pending_messages = HashMap::new();
        let mut joined_rooms = HashSet::new();
        let pending_iqs: PendingIqs = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));

        let (writer, mut server) = loopback_writer().await;
        let local_disco: LocalDiscoState = std::sync::Arc::new(std::sync::Mutex::new(crate::LocalDisco::default()));

        // A roster query is a get we do not answer here; nothing should be
        // written back.
        process_stanza(
            "<iq type='get' from='asker@example.com' id='r1'><query xmlns='jabber:iq:roster'/></iq>",
            &event_tx,
            &mut pending_joins,
            &mut pending_messages,
            &mut joined_rooms,
            &pending_iqs,
            &writer,
            &local_disco,
            &disabled_sm(),
        ).await;

        // No reply is sent, so a short read times out instead of returning data.
        let mut buf = vec![0u8; 64];
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), server.read(&mut buf)).await;
        assert!(result.is_err(), "expected no response, but socket had data");
    }

    /// Drive `process_stanza` for a single frame with default state, returning
    /// the shared stream-management state it acted on so a test can inspect the
    /// counters.
    async fn process_with_sm(sm: &SharedSmState, writer: &SharedWriter, xml: &str)
    {
        let (event_tx, _event_rx) = mpsc::channel(8);
        let mut pending_joins = HashMap::new();
        let mut pending_messages = HashMap::new();
        let mut joined_rooms = HashSet::new();
        let pending_iqs: PendingIqs = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        let local_disco: LocalDiscoState = std::sync::Arc::new(std::sync::Mutex::new(crate::LocalDisco::default()));

        process_stanza(
            xml,
            &event_tx,
            &mut pending_joins,
            &mut pending_messages,
            &mut joined_rooms,
            &pending_iqs,
            writer,
            &local_disco,
            sm,
        ).await;
    }

    #[tokio::test]
    async fn handled_stanzas_are_counted_when_sm_enabled()
    {
        let sm = disabled_sm();
        sm.enable();
        let (writer, _server) = loopback_writer().await;

        process_with_sm(&sm, &writer, "<presence from='a@b'/>").await;
        process_with_sm(&sm, &writer, "<message type='chat' from='a@b'><body>hi</body></message>").await;
        process_with_sm(&sm, &writer, "<iq type='result' id='x'/>").await;

        assert_eq!(sm.inbound_count(), 3);
    }

    #[tokio::test]
    async fn stanzas_are_not_counted_when_sm_disabled()
    {
        let sm = disabled_sm();
        let (writer, _server) = loopback_writer().await;

        process_with_sm(&sm, &writer, "<presence from='a@b'/>").await;

        assert_eq!(sm.inbound_count(), 0);
    }

    #[tokio::test]
    async fn ack_request_is_answered_with_handled_count()
    {
        use tokio::io::AsyncReadExt;

        let sm = disabled_sm();
        sm.enable();
        let (writer, mut server) = loopback_writer().await;

        // Handle two stanzas, then answer the server's <r/> with <a h='2'/>.
        process_with_sm(&sm, &writer, "<presence from='a@b'/>").await;
        process_with_sm(&sm, &writer, "<presence from='c@d'/>").await;
        process_with_sm(&sm, &writer, "<r xmlns='urn:xmpp:sm:3'/>").await;

        let mut buf = vec![0u8; 128];
        let n = server.read(&mut buf).await.unwrap();
        let response = String::from_utf8_lossy(&buf[..n]);

        assert_eq!(response, "<a xmlns='urn:xmpp:sm:3' h='2'/>");
        // The <r/> itself is a control element and is not counted as a stanza.
        assert_eq!(sm.inbound_count(), 2);
    }

    #[tokio::test]
    async fn presence_with_caps_emits_event()
    {
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let mut pending_joins = HashMap::new();
        let mut pending_messages = HashMap::new();
        let mut joined_rooms = HashSet::new();
        let pending_iqs: PendingIqs = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        let local_disco: LocalDiscoState = std::sync::Arc::new(std::sync::Mutex::new(crate::LocalDisco::default()));
        let (writer, _server) = loopback_writer().await;

        process_stanza(
            "<presence from='romeo@montague.lit/orchard'>\
             <c xmlns='http://jabber.org/protocol/caps' hash='sha-1' \
             node='http://code.google.com/p/exodus' ver='QgayPKawpkPSDYmwT/WM94uAlu0='/>\
             </presence>",
            &event_tx,
            &mut pending_joins,
            &mut pending_messages,
            &mut joined_rooms,
            &pending_iqs,
            &writer,
            &local_disco,
            &disabled_sm(),
        ).await;

        match event_rx.try_recv()
        {
            Ok(XmppEvent::EntityCapabilities { from, node, ver, hash }) =>
            {
                assert_eq!(from, "romeo@montague.lit/orchard");
                assert_eq!(node, "http://code.google.com/p/exodus");
                assert_eq!(ver, "QgayPKawpkPSDYmwT/WM94uAlu0=");
                assert_eq!(hash.as_deref(), Some("sha-1"));
            }
            other => panic!("expected EntityCapabilities event, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn presence_without_caps_emits_no_caps_event()
    {
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let mut pending_joins = HashMap::new();
        let mut pending_messages = HashMap::new();
        let mut joined_rooms = HashSet::new();
        let pending_iqs: PendingIqs = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        let local_disco: LocalDiscoState = std::sync::Arc::new(std::sync::Mutex::new(crate::LocalDisco::default()));
        let (writer, _server) = loopback_writer().await;

        // A bare presence with no <c/> must not produce an EntityCapabilities event.
        process_stanza(
            "<presence from='a@b/c'/>",
            &event_tx,
            &mut pending_joins,
            &mut pending_messages,
            &mut joined_rooms,
            &pending_iqs,
            &writer,
            &local_disco,
            &disabled_sm(),
        ).await;

        assert!(matches!(event_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn direct_message_surfaces_delay()
    {
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let mut pending_joins = HashMap::new();
        let mut pending_messages = HashMap::new();
        let mut joined_rooms = HashSet::new();
        let pending_iqs: PendingIqs = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        let local_disco: LocalDiscoState = std::sync::Arc::new(std::sync::Mutex::new(crate::LocalDisco::default()));
        let (writer, _server) = loopback_writer().await;

        // A one-to-one message stored while offline carries an XEP-0203 delay
        // describing when and by whom it was held.
        process_stanza(
            "<message type='chat' from='alice@example.com'>\
             <body>Offline message</body>\
             <delay xmlns='urn:xmpp:delay' from='example.com' \
             stamp='2025-01-01T12:00:00Z'>Offline Storage</delay>\
             </message>",
            &event_tx,
            &mut pending_joins,
            &mut pending_messages,
            &mut joined_rooms,
            &pending_iqs,
            &writer,
            &local_disco,
            &disabled_sm(),
        ).await;

        match event_rx.try_recv()
        {
            Ok(XmppEvent::DirectMessage { from, body, delay }) =>
            {
                assert_eq!(from, "alice@example.com");
                assert_eq!(body, "Offline message");
                let delay = delay.expect("expected delay information");
                assert_eq!(delay.stamp.as_deref(), Some("2025-01-01T12:00:00Z"));
                assert_eq!(delay.from.as_deref(), Some("example.com"));
                assert_eq!(delay.reason.as_deref(), Some("Offline Storage"));
            }
            other => panic!("expected DirectMessage event, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn direct_message_without_delay_has_none()
    {
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let mut pending_joins = HashMap::new();
        let mut pending_messages = HashMap::new();
        let mut joined_rooms = HashSet::new();
        let pending_iqs: PendingIqs = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        let local_disco: LocalDiscoState = std::sync::Arc::new(std::sync::Mutex::new(crate::LocalDisco::default()));
        let (writer, _server) = loopback_writer().await;

        // A live message carries no <delay/>, so its delay field is absent.
        process_stanza(
            "<message type='chat' from='alice@example.com'><body>Hi</body></message>",
            &event_tx,
            &mut pending_joins,
            &mut pending_messages,
            &mut joined_rooms,
            &pending_iqs,
            &writer,
            &local_disco,
            &disabled_sm(),
        ).await;

        match event_rx.try_recv()
        {
            Ok(XmppEvent::DirectMessage { delay, .. }) => assert!(delay.is_none()),
            other => panic!("expected DirectMessage event, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn incoming_ack_updates_last_ack()
    {
        let sm = disabled_sm();
        sm.enable();
        let (writer, _server) = loopback_writer().await;

        process_with_sm(&sm, &writer, "<a xmlns='urn:xmpp:sm:3' h='5'/>").await;

        assert_eq!(sm.last_ack(), 5);
        // An <a/> is a control element, not a counted stanza.
        assert_eq!(sm.inbound_count(), 0);
    }
}
