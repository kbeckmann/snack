use tokio::sync::mpsc;
use std::collections::{HashMap, HashSet};

use crate::{XmppEvent, RoomMember};
use crate::tcp_stream::Tcp;
use crate::stanza;
use crate::stanza::Stanza;
use crate::xml_framer::XmlFramer;

pub async fn setup_connection(event_tx: &mpsc::Sender<XmppEvent>, jid: &str, password: &str) -> Result<(String, Tcp), String>
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
            return Err("STARTTLS failed".to_string());
        }

        tcp = tcp.add_tls().await?;
        framer.reset();

        log::debug!("TLS established");
        log::debug!("Reopening stream over TLS...");

        tcp.send(&stanza::stream::Stream::new(jid.to_string(), domain.to_string()).as_bytes()).await?;
        let (_stream, new_features) = read_stream_and_features(&mut tcp, &mut framer).await?;
        features = new_features;
    }

    // SASL authentication
    if let Some(ref mechs) = features.mechanisms
    {
        let mechanism = select_mechanism(&mechs.mechanism)?;

        let _ = event_tx.send(XmppEvent::Authenticating).await;
        log::debug!("Starting SASL {:?} authentication...", mechanism);

        do_sasl(&mut tcp, &mut framer, &username, password, &mechanism).await?;
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
        return Err("Server doesn't offer SASL mechanisms".to_string());
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

pub fn parse_jid(jid: &str) -> Result<(&str, &str), String>
{
    let at = jid.find('@').ok_or_else(|| format!("Invalid JID (no @): {}", jid))?;
    return Ok((&jid[..at], &jid[at + 1..]));
}

async fn read_frame(tcp: &mut Tcp, framer: &mut XmlFramer) -> Result<String, String>
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
) -> Result<(stanza::stream::Stream, stanza::stream::StreamFeatures), String>
{
    let header = read_frame(tcp, framer).await?;
    let (stream, _) = stanza::stream::Stream::from_xml(&header)?;

    log::debug!("Stream opened: {:?}", stream);

    let features_xml = read_frame(tcp, framer).await?;
    let features = stanza::stream::StreamFeatures::from_xml(&features_xml)?;

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

fn select_mechanism(mechanisms: &[String]) -> Result<SaslMechanism, String>
{
    if mechanisms.iter().any(|m| m == "SCRAM-SHA-512") { return Ok(SaslMechanism::ScramSha512); }
    if mechanisms.iter().any(|m| m == "SCRAM-SHA-256") { return Ok(SaslMechanism::ScramSha256); }
    if mechanisms.iter().any(|m| m == "SCRAM-SHA-1") { return Ok(SaslMechanism::ScramSha1); }
    if mechanisms.iter().any(|m| m == "PLAIN") { return Ok(SaslMechanism::Plain); }

    return Err("No supported SASL mechanism found".to_string());
}

async fn do_sasl(
    tcp: &mut Tcp,
    framer: &mut XmlFramer,
    username: &str,
    password: &str,
    mechanism: &SaslMechanism,
) -> Result<(), String>
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
) -> Result<(), String>
{
    // Send <auth>
    tcp.send(scram.auth_xml().as_bytes()).await?;

    // Read <challenge>
    let challenge_xml = read_frame(tcp, framer).await?;
    if stanza::sasl::is_failure(&challenge_xml)
    {
        return Err(format!("SASL auth failed: {}", challenge_xml));
    }

    let challenge_b64 = stanza::sasl::parse_challenge(&challenge_xml)?;

    // Send <response>
    let response_xml = scram.response_xml(&challenge_b64)?;
    tcp.send(response_xml.as_bytes()).await?;

    // Read <success>
    let success_xml = read_frame(tcp, framer).await?;
    if stanza::sasl::is_failure(&success_xml)
    {
        return Err(format!("SASL auth failed: {}", success_xml));
    }

    let success_b64 = stanza::sasl::parse_success(&success_xml)?;
    scram.verify_success(&success_b64)?;

    return Ok(());
}

async fn do_sasl_plain(
    tcp: &mut Tcp,
    framer: &mut XmlFramer,
    auth: &stanza::sasl::PlainAuth,
) -> Result<(), String>
{
    tcp.send(auth.auth_xml().as_bytes()).await?;

    let response_xml = read_frame(tcp, framer).await?;
    if stanza::sasl::is_failure(&response_xml)
    {
        return Err(format!("SASL auth failed: {}", response_xml));
    }

    return Ok(());
}

async fn do_bind(
    tcp: &mut Tcp,
    framer: &mut XmlFramer,
) -> Result<String, String>
{
    let bind_req = stanza::bind::BindRequest::new("bind_1".to_string(), None);
    tcp.send(&bind_req.as_bytes()).await?;

    let result_xml = read_frame(tcp, framer).await?;
    let result = stanza::bind::BindResult::from_xml(&result_xml)?;

    if result.iq_type != "result"
    {
        return Err(format!("Bind failed: {:?}", result));
    }

    return result.bind
        .and_then(|b| b.jid)
        .ok_or_else(|| "No JID in bind result".to_string());
}

pub async fn process_stanza(
    xml: &str,
    event_tx: &mpsc::Sender<XmppEvent>,
    pending_joins: &mut HashMap<String, Vec<RoomMember>>,
    pending_messages: &mut HashMap<String, Vec<XmppEvent>>,
    joined_rooms: &mut HashSet<String>,
)
{
    if xml.contains("<presence") && (xml.contains("type='error'") || xml.contains("type=\"error\""))
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
    else if xml.contains("<presence") && xml.contains("http://jabber.org/protocol/muc#user")
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
    else if xml.contains("<message") && (xml.contains("type='chat'") || xml.contains("type=\"chat\""))
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
    else if xml.contains("<message") && (xml.contains("type='groupchat'") || xml.contains("type=\"groupchat\""))
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
}
