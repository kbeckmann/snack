use tokio::sync::mpsc;
use std::collections::{HashMap, HashSet};

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::{XmppEvent, RoomMember};
use crate::error::XmppError;
use crate::tcp_stream::Tcp;
use crate::stanza;
use crate::stanza::Stanza;
use crate::xml_framer::XmlFramer;

pub async fn setup_connection(event_tx: &mpsc::Sender<XmppEvent>, jid: &str, password: &str) -> Result<(String, Tcp), XmppError>
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

    return Ok((bound_jid, tcp));
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
/// value of its `type` attribute. Dispatch is driven by this parsed view rather
/// than substring matching, so body text can never be mistaken for markup.
fn peek_root(xml: &str) -> Option<(String, Option<String>)>
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
                for attr in e.attributes().flatten()
                {
                    if attr.key.local_name().as_ref() == b"type"
                    {
                        stanza_type = attr.unescape_value().ok().map(|v| v.to_string());
                    }
                }

                return Some((name, stanza_type));
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

pub async fn process_stanza(
    xml: &str,
    event_tx: &mpsc::Sender<XmppEvent>,
    pending_joins: &mut HashMap<String, Vec<RoomMember>>,
    pending_messages: &mut HashMap<String, Vec<XmppEvent>>,
    joined_rooms: &mut HashSet<String>,
)
{
    // MAM (XEP-0313) stanzas must be handled first: an archive <result> wraps a
    // forwarded message that itself contains type='groupchat'/'chat', so the
    // generic message branches below would otherwise misroute it.
    if xml.contains("urn:xmpp:mam:2")
    {
        if xml.contains("<fin")
        {
            match stanza::mam::MamFinStanza::from_xml(xml)
            {
                Ok(fin) =>
                {
                    let _ = event_tx.send(XmppEvent::ArchiveComplete
                    {
                        query_id: fin.id.clone().unwrap_or_default(),
                        complete: fin.is_complete(),
                        first: fin.first_id(),
                        last: fin.last_id(),
                    }).await;
                }
                Err(e) => log::warn!("Failed to parse MAM fin: {}", e),
            }
        }
        else if xml.contains("<result")
        {
            match stanza::mam::MamResultStanza::from_xml(xml)
            {
                Ok(r) =>
                {
                    let msg = r.result.forwarded.message;
                    if let (Some(from), Some(body)) = (msg.from, msg.body)
                    {
                        let timestamp = r.result.forwarded.delay.and_then(|d| d.stamp);
                        let _ = event_tx.send(XmppEvent::ArchiveMessage
                        {
                            query_id: r.result.queryid.unwrap_or_default(),
                            from,
                            body,
                            timestamp,
                        }).await;
                    }
                }
                Err(e) => log::warn!("Failed to parse MAM result: {}", e),
            }
        }
        return;
    }

    // An <iq type='error'> response to one of our MAM queries (which carries no
    // mam namespace). Recognised by our `mam-` id prefix so the app can stop
    // waiting on it (e.g. when the server doesn't support MAM).
    if xml.contains("<iq") && (xml.contains("type='error'") || xml.contains("type=\"error\""))
    {
        if let Ok(iq) = stanza::mam::IqIdentity::from_xml(xml)
        {
            if iq.id.as_deref().is_some_and(|id| id.starts_with("mam-"))
            {
                let _ = event_tx.send(XmppEvent::ArchiveComplete
                {
                    query_id: iq.id.unwrap_or_default(),
                    complete: true,
                    first: None,
                    last: None,
                }).await;
                return;
            }
        }
    }

    let (root, stanza_type) = match peek_root(xml)
    {
        Some(v) => v,
        None => return,
    };

    match (root.as_str(), stanza_type.as_deref())
    {
        ("presence", Some("error")) =>
        {
            process_presence_error(xml, event_tx, pending_joins, pending_messages).await;
        }
        ("presence", _) =>
        {
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
        _ => {}
    }
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
        let timestamp = msg.delay.and_then(|d| d.stamp);
        let _ = event_tx.send(XmppEvent::DirectMessage { from, body, timestamp }).await;
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
            let timestamp = msg.delay.as_ref().and_then(|d| d.stamp.clone());
            let _ = event_tx.send(XmppEvent::RoomMessage
            {
                room: room.to_string(),
                nick: nick.to_string(),
                body: body.clone(),
                timestamp,
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
            let timestamp = msg.delay.as_ref().and_then(|d| d.stamp.clone());
            messages.push(XmppEvent::RoomMessage
            {
                room: room.to_string(),
                nick: nick.to_string(),
                body: body.clone(),
                timestamp,
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
        let (name, typ) = peek_root("<message type='chat' from='a@b'><body>hi</body></message>").unwrap();
        assert_eq!(name, "message");
        assert_eq!(typ.as_deref(), Some("chat"));
    }

    #[test]
    fn peek_root_ignores_body_text()
    {
        // A body that contains markup-like text must not change the dispatch:
        // the root element is still a type='chat' message.
        let (name, typ) = peek_root(
            "<message type='chat' from='a@b'><body>look: type='groupchat'</body></message>"
        ).unwrap();
        assert_eq!(name, "message");
        assert_eq!(typ.as_deref(), Some("chat"));
    }

    #[test]
    fn peek_root_handles_self_closing()
    {
        let (name, typ) = peek_root("<presence type='unavailable'/>").unwrap();
        assert_eq!(name, "presence");
        assert_eq!(typ.as_deref(), Some("unavailable"));
    }
}
