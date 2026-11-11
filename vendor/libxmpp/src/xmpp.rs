use tokio::sync::mpsc;
#[cfg(test)]
use std::collections::{ HashMap, HashSet };

#[cfg(test)]
use crate::RoomMember;
use crate::PendingIqs;

use quick_xml::events::Event;

use crate::{ XmppEvent, LocalDiscoState, SharedWriter, SharedSmState };
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
pub async fn setup_connection(event_tx: &mpsc::Sender<XmppEvent>, jid: &str, password: &str) -> Result<(String, Tcp, bool, XmlFramer), XmppError>
{
    let (username, domain) = parse_jid(jid)?;

    let _ = event_tx.send(XmppEvent::Connecting).await;
    let mut tcp = crate::discovery::connect(domain).await?;

    let mut framer = XmlFramer::new();

    log::debug!("Opening initial stream...");

    tcp.send(&stanza::stream::Stream::new(stanza::bare_jid(jid).to_string(), domain.to_string()).as_bytes()).await?;
    let (_stream, mut features) = read_stream_and_features(&mut tcp, &mut framer).await?;

    // TLS
    if features.starttls.is_some()
    {
        let _ = event_tx.send(XmppEvent::EstablishingTls).await;
        log::debug!("Starting TLS negotiation...");

        tcp.send(&stanza::stream::StartTlsRequest.as_bytes()).await?;
        let response = read_frame(&mut tcp, &mut framer).await?;

        validate_tls_proceed(&response)?;

        tcp = tcp.add_tls().await?;
        framer.reset();

        log::debug!("TLS established");
        log::debug!("Reopening stream over TLS...");

        tcp.send(&stanza::stream::Stream::new(stanza::bare_jid(jid).to_string(), domain.to_string()).as_bytes()).await?;
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
        tcp.send(&stanza::stream::Stream::new(stanza::bare_jid(jid).to_string(), domain.to_string()).as_bytes()).await?;
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
        do_bind(&mut tcp, &mut framer, domain, jid.split_once('/').map(|(_, resource)| resource)).await?
    }
    else
    {
        return Err(XmppError::Bind("server did not advertise resource binding after authentication".into()));
    };

    log::info!("Bound JID: {}", bound_jid);

    // XEP-0198 stream management. Enabled once the resource is bound, so the
    // server counts our stanzas (and we count its) from this point on. Requested
    // only when advertised; a refusal is non-fatal and simply leaves it off.
    let sm_enabled = if features.sm.iter().any(|feature| feature.xmlns.as_deref() == Some(stanza::sm::SM_NS))
    {
        enable_stream_management(&mut tcp, &mut framer).await?
    }
    else
    {
        false
    };

    return Ok((bound_jid, tcp, sm_enabled, framer));
}

// Negotiation replies may be interleaved with ordinary stanzas. Keep those
// until the client reader starts, without counting them as post-enable traffic.
async fn read_setup_reply(
    tcp: &mut Tcp, framer: &mut XmlFramer,
    expected: impl Fn(&str) -> bool,
) -> Result<String, XmppError>
{
    let mut deferred = Vec::new();
    let mut bytes = 0usize;
    loop {
        let frame = read_frame(tcp, framer).await?;
        if expected(&frame) {
            framer.defer_frames(deferred);
            return Ok(frame);
        }
        if !matches!(peek_root(&frame), Some((root, _, _)) if matches!(root.as_str(), "message" | "presence" | "iq")) {
            return Err(XmppError::Protocol("unexpected response during session setup".into()));
        }
        bytes += frame.len();
        if deferred.len() >= 1024 || bytes > 8 * 1024 * 1024 {
            return Err(XmppError::Protocol("too much interleaved traffic before setup reply".into()));
        }
        deferred.push(frame);
    }
}

/// Request XEP-0198 management. Only an explicit namespaced enabled/failed
/// response finishes negotiation; interleaved stanzas are kept for the reader.
async fn enable_stream_management(tcp: &mut Tcp, framer: &mut XmlFramer) -> Result<bool, XmppError>
{
    log::debug!("Requesting stream management (XEP-0198)...");
    tcp.send(&stanza::sm::Enable.as_bytes()).await?;
    let frame = read_setup_reply(tcp, framer, |frame|
        stanza::xml::is_element(frame, b"enabled", stanza::sm::SM_NS.as_bytes())
            || stanza::xml::is_element(frame, b"failed", stanza::sm::SM_NS.as_bytes())).await?;
    if stanza::xml::is_element(&frame, b"enabled", stanza::sm::SM_NS.as_bytes()) {
        let enabled = stanza::sm::Enabled::from_xml(&frame).map_err(XmppError::Parse)?;
        log::debug!("Stream management enabled: {:?}", enabled);
        return Ok(true);
    }
    log::warn!("Server declined stream management");
    return Ok(false);
}

fn validate_tls_proceed(xml: &str) -> Result<(), XmppError>
{
    stanza::xml::empty_element(xml, b"proceed", b"urn:ietf:params:xml:ns:xmpp-tls")
        .map(|_| ()).map_err(|e| XmppError::Tls(format!("invalid STARTTLS response: {e}")))
}

pub fn parse_jid(jid: &str) -> Result<(&str, &str), XmppError>
{
    let (username, domain) = parse_address(jid)?;
    let username = username.ok_or_else(|| XmppError::InvalidJid("expected user@domain".into()))?;
    Ok((username, domain))
}

// General stanza targets may be domain-only services. Login/binding require
// a user part, but otherwise share these shape and character checks.
pub(crate) fn parse_address(jid: &str) -> Result<(Option<&str>, &str), XmppError>
{
    if !stanza::jid_has_valid_lengths(jid) {
        return Err(XmppError::InvalidJid("each part must contain 1–1023 UTF-8 bytes".into()));
    }
    if !crate::xml_framer::valid_xml_text(jid) {
        return Err(XmppError::InvalidJid("address contains an unsupported XML character".into()));
    }
    let (bare, resource) = match jid.split_once('/') {
        Some((bare, resource)) => (bare, Some(resource)),
        None => (jid, None),
    };
    let (username, domain) = match bare.split_once('@') {
        Some((username, domain)) => (Some(username), domain),
        None => (None, bare),
    };
    if username.is_some_and(|username| username.chars().any(|ch| ch.is_whitespace() || ch.is_control() || matches!(ch, '\"' | '&' | '\'' | '/' | ':' | '<' | '>' | '@'))) {
        return Err(XmppError::InvalidJid("user contains a forbidden character".into()));
    }
    if domain.chars().any(|ch| ch.is_whitespace() || ch.is_control() || matches!(ch, '\"' | '&' | '\'' | '<' | '>' | '@')) {
        return Err(XmppError::InvalidJid("domain contains a forbidden character".into()));
    }
    if domain.contains(['[', ']', ':']) {
        let ipv6 = domain.strip_prefix('[').and_then(|value| value.strip_suffix(']'))
            .is_some_and(|address| address.parse::<std::net::Ipv6Addr>().is_ok());
        if !ipv6 { return Err(XmppError::InvalidJid("IPv6 domains require a valid address enclosed in brackets; JIDs do not include a port.".into())); }
    }
    if resource.is_some_and(|resource| resource.is_empty() || resource.chars().any(char::is_control)) {
        return Err(XmppError::InvalidJid("resource must be nonempty and contain no control characters".into()));
    }
    return Ok((username, domain));
}

// A stream error is fatal at every negotiation stage and after login. Match
// the complete namespaced root, never an extension or a nested error element.
pub(crate) fn stream_error(xml: &str) -> Option<XmppError>
{
    stanza::xml::is_element(xml, b"error", b"http://etherx.jabber.org/streams")
        .then(|| XmppError::Stream(xml.to_string()))
}

async fn read_frame(tcp: &mut Tcp, framer: &mut XmlFramer) -> Result<String, XmppError>
{
    loop
    {
        if let Some(frame) = framer.try_next()
        {
            if let Some(error) = stream_error(&frame) {
                let _ = tokio::time::timeout(crate::CLOSE_TIMEOUT, tcp.send(b"</stream:stream>")).await;
                return Err(error);
            }
            return Ok(frame);
        }

        if let Some(error) = framer.error() { return Err(XmppError::Parse(error.to_string())); }
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
    server: &str,
    resource: Option<&str>,
) -> Result<String, XmppError>
{
    let bind_req = stanza::bind::BindRequest::new("bind_1".to_string(), resource.map(str::to_string));
    tcp.send(&bind_req.as_bytes()).await?;

    let result_xml = read_setup_reply(tcp, framer, |frame| is_bind_reply(frame, "bind_1", server)).await?;
    let result = stanza::bind::BindResult::from_xml(&result_xml).map_err(XmppError::Bind)?;
    return result.bound_jid("bind_1").map_err(XmppError::Bind);
}

fn is_bind_reply(xml: &str, request_id: &str, server: &str) -> bool
{
    let Ok(attributes) = stanza::xml::element(xml, b"iq", b"jabber:client") else { return false; };
    let attribute = |key| attributes.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str());
    attribute("id") == Some(request_id) && matches!(attribute("type"), Some("result" | "error"))
        // Binding replies commonly omit from. If supplied, it identifies the
        // server, not another user or resource that happens to reuse our ID.
        && attribute("from").is_none_or(|from| stanza::jid_matches(from, server))
}

/// Inspect only the root element of a stanza, returning its local name and the
/// values of its `type` and `id` attributes. Dispatch is driven by this parsed
/// view rather than substring matching, so body text can never be mistaken for
/// markup.
fn peek_root(xml: &str) -> Option<(String, Option<String>, Option<String>)>
{
    peek_routing_root(xml).map(|root| (root.name, root.stanza_type, root.id))
}

struct StanzaRoot {
    name: String,
    stanza_type: Option<String>,
    id: Option<String>,
    to: Option<String>,
}

fn peek_routing_root(xml: &str) -> Option<StanzaRoot>
{
    use quick_xml::{NsReader, name::ResolveResult};
    let normalized_xml = crate::stanza::xml::normalize_xml(xml).ok()?;
    let mut reader = NsReader::from_str(&normalized_xml);

    loop
    {
        match reader.read_resolved_event()
        {
            Ok((namespace, Event::Start(e))) | Ok((namespace, Event::Empty(e))) =>
            {
                let local = e.local_name();
                let name = std::str::from_utf8(local.as_ref()).ok()?.to_string();
                // Frames carry the stream's inherited declarations. Local-name
                // matching alone would route foreign extension elements as
                // messages, presence or IQ replies (and count them for SM).
                if matches!(name.as_str(), "message" | "presence" | "iq")
                    && !matches!(namespace, ResolveResult::Bound(ns) if ns.as_ref() == b"jabber:client") {
                    return None;
                }

                let mut stanza_type = None;
                let mut id = None;
                let mut to = None;
                for attr in e.attributes()
                {
                    let attr = attr.ok()?;
                    // Stanza attributes are unqualified. An extension's
                    // `x:id`, `x:type`, or `x:to` must not override core attributes.
                    match attr.key.as_ref()
                    {
                        b"type" => stanza_type = Some(attr.unescape_value().ok()?.into_owned()),
                        b"id" => id = Some(attr.unescape_value().ok()?.into_owned()),
                        b"to" => to = Some(attr.unescape_value().ok()?.into_owned()),
                        _ => {}
                    }
                }

                return Some(StanzaRoot { name, stanza_type, id, to });
            }
            Ok((_, Event::Eof)) | Err(_) => return None,
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
    write_stanza_locked(&mut w, sm, bytes).await
}

// Room joins acquire the writer before replacing registry state. Keep stream
// checks and SM accounting identical to ordinary sends while holding that lock.
pub(crate) async fn write_stanza_locked(w: &mut crate::tcp_stream::TcpWriter, sm: &SharedSmState, bytes: &[u8]) -> Result<(), XmppError>
{
    if sm.closed.load(std::sync::atomic::Ordering::SeqCst) || sm.closing.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(XmppError::Disconnected);
    }
    crate::write_session(w, sm, bytes).await?;
    if sm.is_enabled()
    {
        sm.record_outbound();
    }

    return Ok(());
}

pub(crate) async fn cancel_expired_room(writer: &SharedWriter, sm: &SharedSmState, rooms: &crate::rooms::Rooms, room: &str, nick: String) -> Result<bool, XmppError>
{
    // Serialize the registry check with outgoing writes: a new join registered
    // before this lock must not be cancelled by the preceding entry's timeout.
    let mut writer = writer.lock().await;
    if crate::rooms::key(&rooms.lock().unwrap(), room).is_some() { return Ok(false); }
    if sm.closed.load(std::sync::atomic::Ordering::SeqCst) || sm.closing.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(XmppError::Disconnected);
    }
    let leave = stanza::muc::MucLeavePresence::new(room.into(), nick);
    crate::write_session(&mut writer, sm, &leave.as_bytes()).await?;
    if sm.is_enabled() { sm.record_outbound(); }
    Ok(true)
}

pub(crate) async fn write_room_leave(writer: &SharedWriter, sm: &SharedSmState, rooms: &crate::rooms::Rooms, room: &str, request_id: &str) -> Result<(), XmppError>
{
    let mut writer = writer.lock().await;
    if sm.closed.load(std::sync::atomic::Ordering::SeqCst) || sm.closing.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(XmppError::Disconnected);
    }
    // Resolve the target after acquiring the writer. A room rename can arrive
    // while this write is queued, or another request can replace this leave.
    let nick = {
        let rooms = rooms.lock().unwrap();
        let Some(state) = rooms.get(room).filter(|state| state.request_id == request_id && state.phase == crate::rooms::Phase::Leaving) else { return Ok(()); };
        if state.leave_target.as_deref() == Some(&state.nick) { return Ok(()); }
        state.nick.clone()
    };
    crate::validate_room_target(room, &nick)?;
    let presence = stanza::muc::MucLeavePresence::new(room.into(), nick.clone()).with_id(request_id.into());
    crate::write_session(&mut writer, sm, &presence.as_bytes()).await?;
    if sm.is_enabled() { sm.record_outbound(); }
    if let Some(state) = rooms.lock().unwrap().get_mut(room).filter(|state| state.request_id == request_id) {
        state.leave_target = Some(nick);
    }
    Ok(())
}

#[cfg(test)]
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
    // Model the namespace scope supplied by the actual client stream rather
    // than weakening production validation for unqualified test fixtures.
    let mut framer = XmlFramer::new_client_stream();
    framer.feed(xml.as_bytes());
    let Some(xml) = framer.try_next() else { return; };
    let rooms: crate::rooms::Rooms = Default::default();
    {
        let mut registry = rooms.lock().unwrap();
        for jid in pending_joins.keys().chain(joined_rooms.iter()).cloned().collect::<HashSet<_>>() {
            let mut room = crate::rooms::Room::joining("fixture".into(), String::new());
            if joined_rooms.contains(&jid) { room.phase = crate::rooms::Phase::Joined; }
            room.members = crate::roster::Roster::from_members(pending_joins.remove(&jid).unwrap_or_default());
            room.restore_messages(pending_messages.remove(&jid).unwrap_or_default());
            registry.insert(jid.clone(), room);
        }
    }
    process_stanza_with_count(&xml, true, event_tx, &rooms, pending_iqs, writer, local_disco, sm, None).await;
    pending_joins.clear();
    pending_messages.clear();
    joined_rooms.clear();
    for (jid, mut room) in rooms.lock().unwrap().drain() {
        match room.phase {
            crate::rooms::Phase::Joining => {
                pending_joins.insert(jid.clone(), room.members.take());
                pending_messages.insert(jid, room.messages);
            }
            crate::rooms::Phase::Joined => { joined_rooms.insert(jid); }
            crate::rooms::Phase::Leaving => {},
        }
    }
}

pub async fn process_stanza_with_count(
    xml: &str,
    count_inbound: bool,
    event_tx: &mpsc::Sender<XmppEvent>,
    rooms: &crate::rooms::Rooms,
    pending_iqs: &PendingIqs,
    writer: &SharedWriter,
    local_disco: &LocalDiscoState,
    sm: &SharedSmState,
    own_jid: Option<&str>,
)
{
    // An outgoing operation can retire room buffers while the reader already
    // has more frames queued. Deliver that accepted data before dispatching the
    // next frame, including errors and frames ignored by recipient filtering.
    // take_ready releases the registry lock before channel backpressure awaits.
    for event in crate::rooms::take_ready(rooms) { let _ = event_tx.send(event).await; }

    let StanzaRoot { name: root, stanza_type, id, to } = match peek_routing_root(xml)
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
            if !count_inbound || !sm.is_enabled() || stanza::xml::empty_element(xml, b"r", stanza::sm::SM_NS.as_bytes()).is_err() { return; }
            let ack = stanza::sm::Ack::new(sm.inbound_count());
            let mut writer = writer.lock().await;
            if !sm.closing.load(std::sync::atomic::Ordering::SeqCst) {
                if let Err(e) = crate::write_session(&mut writer, sm, &ack.as_bytes()).await {
                    log::warn!("Failed to answer stream-management <r/>: {}", e);
                }
            }
            return;
        }
        "a" =>
        {
            if !count_inbound || !sm.is_enabled() { return; }
            match stanza::sm::Ack::from_xml(xml)
            {
                Ok(ack) => {
                    // The peer can read a stanza before our write future returns.
                    // Synchronize with the writer's outbound-count update.
                    let generation = sm.begin_ack();
                    let _writer = writer.lock().await;
                    if !sm.record_received_ack(ack.h(), generation) {
                        log::warn!("Ignoring invalid stream acknowledgement count {}", ack.h());
                    }
                },
                Err(e) => log::warn!("Failed to parse stream-management <a/>: {}", e),
            }
            return;
        }
        _ => {}
    }

    // Count every stanza we handle so the server's next `<r/>` can be answered.
    if count_inbound && sm.is_enabled() && matches!(root.as_str(), "message" | "presence" | "iq")
    {
        sm.record_inbound();
    }

    // Check only the outer stanza: forwarded archive/carbons payloads can
    // legitimately name other recipients. The server preserves the original
    // resource, so compare bare accounts, not just this connection's full JID.
    if matches!(root.as_str(), "message" | "presence" | "iq") {
        if let Some(to) = to.as_deref() {
            if parse_address(to).is_err() || own_jid.is_some_and(|own|
                !crate::recipient::matches_account(to, own)) {
                return;
            }
        }
    }

    if root == "message" && stanza_type.as_deref() == Some("error")
    {
        match stanza::message_error::MessageError::from_xml(xml) {
            Ok(error) => {
                let _ = event_tx.send(XmppEvent::MessageError { id: error.id, from: error.from,
                    error_type: error.error_type, condition: error.condition, text: error.text }).await;
            }
            Err(e) => log::warn!("Invalid message error: {}", e),
        }
        return;
    }

    // Errors need not carry a MAM payload. Correlate with our actual registry,
    // not an ID prefix that any sender can reproduce.
    if root == "iq" && stanza_type.as_deref() == Some("error")
    {
        if let Some(query_id) = id.as_deref()
        {
            if accept_archive_reply(xml, b"iq", query_id, true, true, pending_iqs)
            {
                let _ = event_tx.send(XmppEvent::ArchiveFailed
                {
                    query_id: query_id.to_string(),
                    reason: "archive query rejected by server".into(),
                }).await;
                return;
            }
        }
    }

    // MAM (XEP-0313) stanzas must be handled first: an archive <result> wraps a
    // forwarded message that itself contains type='groupchat'/'chat', so the
    // generic message branches below would otherwise misroute it.
    if (root == "message" && stanza::mam::has_payload(xml, b"result"))
        || (root == "iq" && stanza::mam::has_payload(xml, b"fin"))
    {
        if root == "iq"
        {
            match stanza::mam::MamFinStanza::from_xml(xml)
            {
                Ok(fin) =>
                {
                    if stanza_type.as_deref() != Some("result") { return; }
                    let Some(query_id) = id.as_deref() else { return; };
                    if !accept_archive_reply(xml, b"iq", query_id, false, true, pending_iqs) { return; }
                    let _ = event_tx.send(XmppEvent::ArchiveComplete
                    {
                        query_id: query_id.to_string(),
                        complete: fin.is_complete(),
                        first: fin.first_id(),
                        last: fin.last_id(),
                    }).await;
                }
                Err(e) => log::warn!("Failed to parse MAM fin: {}", e),
            }
        }
        else
        {
            match stanza::mam::MamResultStanza::from_xml(xml)
            {
                Ok(r) =>
                {
                    let Some(query_id) = r.result.queryid.as_deref() else { return; };
                    let msg = r.result.forwarded.message;
                    // Validate full forwarded addresses just as for live chat.
                    // Invalid routing metadata must not renew the query deadline.
                    if msg.from.as_deref().is_some_and(|from| parse_address(from).is_err())
                        || msg.to.as_deref().is_some_and(|to| parse_address(to).is_err()) { return; }
                    if !accept_archive_reply(xml, b"message", query_id, false, false, pending_iqs) { return; }
                    // The archive `<result id>` is this message's XEP-0359 stanza
                    // id; keep it to match the live copy.
                    let stanza_id = r.result.id;
                    if let (Some(from), Some(body)) = (msg.from, msg.body)
                    {
                        let timestamp = r.result.forwarded.delay.and_then(|d| d.stamp);
                        let _ = event_tx.send(XmppEvent::ArchiveMessage
                        {
                            query_id: query_id.to_string(),
                            to: msg.to,
                            from,
                            body,
                            timestamp,
                            stanza_id,
                            origin_id: msg.origin_id,
                        }).await;
                    }
                }
                Err(e) => log::warn!("Failed to parse MAM result: {}", e),
            }
        }
        return;
    }

    match (root.as_str(), stanza_type.as_deref())
    {
        ("presence", Some("error")) =>
        {
            process_presence_error(xml, event_tx, rooms).await;
        }
        ("presence", _) =>
        {
            // Surface any XEP-0115 caps first; the same presence is then handled
            // for MUC roster/room semantics.
            process_entity_caps(xml, event_tx).await;
            process_muc_presence(xml, event_tx, rooms, writer, sm).await;
        }
        // RFC 6121 defaults absent and unrecognized message types to normal.
        // These are ordinary incoming messages and must not disappear merely
        // because the sender did not explicitly request a chat session.
        ("message", kind) if !matches!(kind, Some("groupchat" | "headline" | "error")) =>
        {
            process_chat_message(xml, event_tx, rooms).await;
        }
        ("message", Some("groupchat")) =>
        {
            process_groupchat_message(xml, event_tx, rooms).await;
        }
        ("iq", Some("result")) | ("iq", Some("error")) =>
        {
            // Wake any caller awaiting this IQ's reply, handing over the raw
            // reply stanza so it can parse the response body. Both a result and
            // an error count as a reply: either proves the peer is reachable.
            if let Some(id) = id
            {
                if let Ok(attrs) = stanza::xml::element(xml, b"iq", b"jabber:client") {
                    let from = attrs.iter().find(|(key, _)| key == "from").map(|(_, value)| value.as_str());
                    let mut pending = pending_iqs.lock().unwrap();
                    if pending.get(&id).is_some_and(|request| !request.is_archive() && request.accepts(from, stanza_type.as_deref() == Some("error"))) {
                        if let Some(request) = pending.remove(&id) {
                            if let crate::PendingIqKind::Reply(sender) = request.kind {
                                let _ = sender.send(xml.to_string());
                            }
                        }
                    }
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

fn accept_archive_reply(
    xml: &str, root: &[u8], query_id: &str, is_error: bool, terminal: bool, pending_iqs: &PendingIqs,
) -> bool
{
    let Ok(attrs) = stanza::xml::element(xml, root, b"jabber:client") else { return false; };
    let from = attrs.iter().find(|(key, _)| key == "from").map(|(_, value)| value.as_str());
    let mut pending = pending_iqs.lock().unwrap();
    if !pending.get(query_id).is_some_and(|request| request.is_archive() && request.accepts(from, is_error)) {
        return false;
    }
    if terminal { pending.remove(query_id); }
    else if let Some(request) = pending.get_mut(query_id) {
        request.touch_archive(tokio::time::Instant::now());
    }
    true
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

        let disco = local_disco.lock().unwrap();
        let info = match node.as_deref() {
            None => Some(&disco.info),
            Some(node) => disco.advertised_info.get(node).or_else(|| {
                (node == format!("{}#{}", disco.caps_node, stanza::caps::compute_ver(&disco.info)))
                    .then_some(&disco.info)
            }),
        };
        if info.is_none() {
            // No per-node application configuration exists. Unknown nodes must
            // not masquerade as the root entity or as another capability hash.
            return Some(format!(
                "<iq type='error' to='{}' id='{}'><query xmlns='{}' node='{}'/><error type='cancel'><item-not-found xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></iq>",
                crate::stanza::xml::escape_attribute_value(&to), crate::stanza::xml::escape_attribute_value(&id),
                crate::stanza::xml::escape_attribute_value(&query.query.xmlns), crate::stanza::xml::escape_attribute_value(node.as_deref().unwrap()),
            ));
        }
        return match query.query.xmlns.as_str()
        {
            stanza::disco::DISCO_INFO_NS =>
                Some(stanza::disco::DiscoInfoResponse::new(id, to, node, info.unwrap().clone()).to_xml()),
            stanza::disco::DISCO_ITEMS_NS => {
                // Capability nodes describe an info snapshot, not the root's
                // hosted items. They have no child items of their own.
                let items = if node.is_none() { disco.items.clone() } else { Vec::new() };
                Some(stanza::disco::DiscoItemsResponse::new(id, to, node, items).to_xml())
            }
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

async fn process_presence_error(xml: &str, event_tx: &mpsc::Sender<XmppEvent>, rooms: &crate::rooms::Rooms)
{
    let error = match stanza::muc::PresenceErrorStanza::from_xml(xml) {
        Ok(error) => error,
        Err(e) => { log::warn!("Failed to parse presence error: {}", e); return; }
    };
    for event in crate::rooms::error(rooms, error) { let _ = event_tx.send(event).await; }
}

async fn process_muc_presence(xml: &str, event_tx: &mpsc::Sender<XmppEvent>, rooms: &crate::rooms::Rooms, writer: &SharedWriter, sm: &SharedSmState)
{
    let presence = match stanza::muc::MucPresence::from_xml(xml) {
        Ok(presence) => presence,
        Err(e) => { log::warn!("Failed to parse MUC presence: {}", e); return; }
    };
    let old_target = presence.room_and_nick().and_then(|(jid, _)| {
        let rooms = rooms.lock().unwrap();
        let key = crate::rooms::key(&rooms, jid)?;
        rooms.get(key).filter(|state| state.phase == crate::rooms::Phase::Leaving)
            .map(|state| (key.to_string(), state.request_id.clone(), state.nick.clone()))
    });
    for event in crate::rooms::presence(rooms, &presence) { let _ = event_tx.send(event).await; }
    if let Some((jid, request_id, nick)) = old_target {
        let renamed = rooms.lock().unwrap().get(&jid).is_some_and(|state|
            state.phase == crate::rooms::Phase::Leaving && state.request_id == request_id && state.nick != nick);
        if renamed {
            let result = tokio::time::timeout(crate::CLOSE_TIMEOUT, write_room_leave(writer, sm, rooms, &jid, &request_id)).await;
            if !matches!(result, Ok(Ok(()))) {
                log::warn!("Could not cancel room entry after nickname changed; ending the session");
                sm.closed.store(true, std::sync::atomic::Ordering::SeqCst);
                sm.acked.notify_waiters();
            }
        }
    }
}

async fn process_chat_message(xml: &str, event_tx: &mpsc::Sender<XmppEvent>, rooms: &crate::rooms::Rooms)
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

    let room_service = msg.from.as_deref().is_some_and(|from|
        !from.contains('/') && rooms.lock().unwrap().is_room(from));
    if room_service {
        // Release parsed body storage before parsing the service stanza.
        drop(msg);
        process_groupchat_message(xml, event_tx, rooms).await;
        return;
    }
    let stanza_id = msg.stanza_id().map(str::to_owned);
    if let (Some(from), Some(body)) = (msg.from, msg.body)
    {
        if parse_address(&from).is_err() { return; }
        let delay = msg.delay.map(Into::into);
        let origin_id = msg.origin_id;
        let private_room = from.split_once('/').is_some_and(|(room, _)|
            msg.muc_private || rooms.lock().unwrap().is_room(room));
        let event = if private_room {
            XmppEvent::RoomPrivateMessage { from, body, delay, stanza_id, origin_id }
        } else { XmppEvent::DirectMessage { from, body, delay, stanza_id, origin_id } };
        let _ = event_tx.send(event).await;
    }
}

async fn process_groupchat_message(xml: &str, event_tx: &mpsc::Sender<XmppEvent>, rooms: &crate::rooms::Rooms)
{
    let msg = match stanza::muc::MucMessage::from_xml(xml) {
        Ok(message) => message,
        Err(e) => { log::warn!("Failed to parse MUC message: {}", e); return; }
    };
    let subject = msg.subject_change();
    let Some(from) = msg.from.as_deref() else { return; };
    if parse_address(from).is_err() { return; }
    let (room, nick) = from.split_once('/').map_or((from, None), |(room, nick)| (room, Some(nick)));
    if nick.is_none() {
        for event in crate::rooms::service_message(rooms, &msg) { let _ = event_tx.send(event).await; }
        return;
    }
    let stanza_id = msg.stanza_id(room).map(str::to_string);
    // Resolve identity and capture context with the same registry lock used
    // for buffering, so a replacement request cannot relabel old traffic.
    let events = {
        let mut registry = rooms.lock().unwrap();
        let Some(room) = crate::rooms::key(&registry, room).map(str::to_string) else { return; };
        let mut events = Vec::new();
        if let Some((_, subject)) = subject { events.push(XmppEvent::RoomSubject { room: room.clone(), subject: subject.into() }); }
        if let (Some(body), Some(nick)) = (&msg.body, nick) {
            events.push(XmppEvent::RoomMessage { room: room.clone(), nick: nick.into(), body: body.clone(),
                delay: msg.delay.clone().map(Into::into), stanza_id, origin_id: msg.origin_id.clone() });
        }
        let closing = registry.is_closing();
        let state = registry.get_mut(&room).unwrap();
        match state.phase {
            crate::rooms::Phase::Joining if !closing => {
                let Some(batch) = state.buffer_messages(events) else { return; };
                events = batch;
            }
            crate::rooms::Phase::Joining => { state.remember_flushed_subject(&events); },
            // Departure is not confirmed yet: genuine final messages still
            // belong in history. The operation context prevents stale controls
            // from affecting a later entry in the application.
            crate::rooms::Phase::Leaving => { state.remember_flushed_subject(&events); },
            crate::rooms::Phase::Joined => { state.remember_flushed_subject(&events); },
        }
        let context = state.context.clone();
        let mut ready = registry.take_ready();
        ready.extend(events.into_iter().map(|event| crate::rooms::contextual_event(&room, context.as_deref(), event)));
        ready
    };
    for event in events { let _ = event_tx.send(event).await; }
}

#[cfg(test)]
mod tests
{
    use super::*;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn retired_room_data_precedes_later_buffered_frames_under_backpressure() {
        for (xml, kind) in [
            ("<presence xmlns='jabber:client' type='error' from='room@example/me'><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></presence>", "presence error"),
            ("<message xmlns='jabber:client' type='chat' from='contact@example'><body>later direct</body></message>", "direct"),
            ("<message xmlns='jabber:client' type='error' from='contact@example' id='send'><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></message>", "message error"),
            ("<message xmlns='jabber:client' type='chat' from='contact@example' to='other@example'><body>wrong recipient</body></message>", "foreign recipient"),
        ] {
            let rooms: crate::rooms::Rooms = Default::default();
            let mut original = crate::rooms::Room::joining("first".into(), "me".into());
            original.context = Some("original".into());
            rooms.lock().unwrap().insert("room@example".into(), original);
            let (tx, mut rx) = mpsc::channel(1);
            process_groupchat_message("<message xmlns='jabber:client' type='groupchat' from='room@example/alice'><body>before replacement</body></message>", &tx, &rooms).await;
            {
                let mut registry = rooms.lock().unwrap();
                registry.defer_buffered("room@example");
                let mut replacement = crate::rooms::Room::joining("second".into(), "me".into());
                replacement.context = Some("replacement".into());
                registry.insert("room@example".into(), replacement);
            }
            let (writer, _server) = loopback_writer().await;
            let sm = disabled_sm();
            sm.enable();
            let pending_iqs: PendingIqs = Default::default();
            let local_disco = Arc::new(Mutex::new(crate::LocalDisco::default()));
            tx.send(XmppEvent::Connected).await.unwrap();
            let dispatch = process_stanza_with_count(xml, true, &tx, &rooms, &pending_iqs, &writer, &local_disco, &sm, Some("me@example/device"));
            tokio::pin!(dispatch);
            tokio::select! {
                biased;
                _ = &mut dispatch => panic!("dispatch bypassed pending room data: {kind}"),
                _ = std::future::ready(()) => {},
            }
            assert!(rooms.try_lock().is_ok(), "a full event channel must not hold the registry");
            assert!(matches!(rx.recv().await, Some(XmppEvent::Connected)));
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                tokio::join!(dispatch, async {
                    assert!(matches!(rx.recv().await, Some(XmppEvent::RoomEvent { context, event, .. })
                        if context == "original" && matches!(event.as_ref(), XmppEvent::RoomMessage { body, .. } if body == "before replacement")), "{kind}");
                    if kind != "foreign recipient" {
                        let event = rx.recv().await.unwrap();
                        match kind {
                            "presence error" => assert!(matches!(event, XmppEvent::RoomEvent { context, event, .. }
                                if context == "replacement" && matches!(event.as_ref(), XmppEvent::PresenceError { .. }))),
                            "direct" => assert!(matches!(event, XmppEvent::DirectMessage { body, .. } if body == "later direct")),
                            "message error" => assert!(matches!(event, XmppEvent::MessageError { id, .. } if id == "send")),
                            _ => unreachable!(),
                        }
                    }
                });
            }).await.unwrap();
            assert_eq!(sm.inbound_count(), 1);
            assert!(rx.try_recv().is_err());
            assert!(crate::rooms::take_ready(&rooms).is_empty());
        }
    }

    #[tokio::test]
    async fn prejoin_message_bursts_flush_in_order_without_confirming_membership() {
        let rooms: crate::rooms::Rooms = Default::default();
        let mut room = crate::rooms::Room::joining("entry".into(), "me".into());
        room.context = Some("original".into());
        rooms.lock().unwrap().insert("room@example".into(), room);
        let count = crate::rooms::MAX_JOIN_BUFFER_EVENTS;
        let (tx, mut rx) = mpsc::channel(count + 2);
        process_groupchat_message("<message xmlns='jabber:client' type='groupchat' from='room@example/alice'><subject>retained topic</subject></message>", &tx, &rooms).await;
        for index in 0..count {
            let xml = format!("<message xmlns='jabber:client' type='groupchat' from='room@example/alice'><body>{index}</body><origin-id xmlns='urn:xmpp:sid:0' id='origin-{index}'/></message>");
            process_groupchat_message(&xml, &tx, &rooms).await;
        }
        assert!(rooms.lock().unwrap()["room@example"].messages.len() <= count);
        assert!(rooms.lock().unwrap()["room@example"].phase == crate::rooms::Phase::Joining);
        let mut bodies = Vec::new();
        while let Ok(event) = rx.try_recv() {
            let XmppEvent::RoomEvent { context, event, .. } = event else { panic!("missing context"); };
            assert_eq!(context, "original");
            match *event {
                XmppEvent::RoomMessage { body, origin_id, .. } => {
                    assert_eq!(origin_id, Some(format!("origin-{}", bodies.len())));
                    bodies.push(body);
                }
                XmppEvent::RoomSubject { subject, .. } => assert_eq!(subject, "retained topic"),
                event => panic!("unexpected membership event: {event:?}"),
            }
        }
        assert_eq!(bodies, (0..count).map(|index| index.to_string()).collect::<Vec<_>>());
        process_groupchat_message("<message xmlns='jabber:client' type='groupchat' from='room@example/alice'><body>tail</body></message>", &tx, &rooms).await;
        assert!(rx.try_recv().is_err());
        let presence = stanza::muc::MucPresence::from_xml("<presence xmlns='jabber:client' from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").unwrap();
        let events = crate::rooms::presence(&rooms, &presence);
        let events: Vec<_> = events.into_iter().map(|event| match event {
            XmppEvent::RoomEvent { context, event, .. } => { assert_eq!(context, "original"); *event },
            event => panic!("missing context: {event:?}"),
        }).collect();
        assert!(matches!(&events[..], [XmppEvent::RoomJoined { .. }, XmppEvent::RoomSubject { subject, .. }, XmppEvent::RoomMessage { body, .. }] if subject == "retained topic" && body == "tail"));
    }

    #[tokio::test]
    async fn prejoin_buffer_flushes_large_unicode_messages_before_the_count_limit() {
        let rooms: crate::rooms::Rooms = Default::default();
        rooms.lock().unwrap().insert("room@example".into(), crate::rooms::Room::joining("entry".into(), "me".into()));
        let (tx, mut rx) = mpsc::channel(16);
        let body = "😀".repeat(crate::rooms::MAX_JOIN_BUFFER_BYTES / 16);
        let xml = format!("<message xmlns='jabber:client' type='groupchat' from='room@example/alice'><body>{body}</body><delay xmlns='urn:xmpp:delay' stamp='2025-01-01T00:00:00Z'>history</delay></message>");
        for _ in 0..5 { process_groupchat_message(&xml, &tx, &rooms).await; }
        assert!(rooms.lock().unwrap()["room@example"].phase == crate::rooms::Phase::Joining);
        let mut delivered = 0;
        while let Ok(event) = rx.try_recv() {
            assert!(matches!(event, XmppEvent::RoomMessage { body: received, delay: Some(delay), .. } if received == body && delay.reason.as_deref() == Some("history")));
            delivered += 1;
        }
        assert!(delivered > 0, "large bodies must flush before reaching the event-count limit");
        assert_eq!(delivered + rooms.lock().unwrap()["room@example"].messages.len(), 5);
    }

    #[tokio::test]
    async fn prejoin_buffer_accounts_for_large_origin_metadata() {
        let rooms: crate::rooms::Rooms = Default::default();
        rooms.lock().unwrap().insert("room@example".into(), crate::rooms::Room::joining("entry".into(), "me".into()));
        let (tx, mut rx) = mpsc::channel(4);
        let origin = "i".repeat(crate::rooms::MAX_JOIN_BUFFER_BYTES / 2);
        let xml = format!("<message xmlns='jabber:client' type='groupchat' from='room@example/alice'><body>small body</body><origin-id xmlns='urn:xmpp:sid:0' id='{origin}'/></message>");
        for _ in 0..2 { process_groupchat_message(&xml, &tx, &rooms).await; }
        assert!(rooms.lock().unwrap()["room@example"].messages.is_empty());
        for _ in 0..2 {
            assert!(matches!(rx.try_recv(), Ok(XmppEvent::RoomMessage { body, origin_id: Some(id), .. }) if body == "small body" && id == origin));
        }
    }

    #[tokio::test]
    async fn prejoin_batch_delivery_waits_for_a_slow_consumer_without_holding_the_registry() {
        let rooms: crate::rooms::Rooms = Default::default();
        rooms.lock().unwrap().insert("room@example".into(), crate::rooms::Room::joining("entry".into(), "me".into()));
        let (tx, mut rx) = mpsc::channel(1);
        for index in 0..crate::rooms::MAX_JOIN_BUFFER_EVENTS {
            let xml = format!("<message xmlns='jabber:client' type='groupchat' from='room@example/alice'><body>{index}</body></message>");
            process_groupchat_message(&xml, &tx, &rooms).await;
        }
        tx.send(XmppEvent::Connected).await.unwrap();
        let xml = format!("<message xmlns='jabber:client' type='groupchat' from='room@example/alice'><body>{}</body></message>", crate::rooms::MAX_JOIN_BUFFER_EVENTS);
        let delivery = process_groupchat_message(&xml, &tx, &rooms);
        tokio::pin!(delivery);
        tokio::select! {
            biased;
            _ = &mut delivery => panic!("delivery bypassed the full event channel"),
            _ = std::future::ready(()) => {},
        }
        {
            let registry = rooms.try_lock().expect("a slow consumer must not block room operations");
            assert!(registry["room@example"].messages.is_empty());
            assert!(registry["room@example"].phase == crate::rooms::Phase::Joining);
        }
        assert!(matches!(rx.recv().await, Some(XmppEvent::Connected)));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(delivery, async {
                for index in 0..=crate::rooms::MAX_JOIN_BUFFER_EVENTS {
                    assert!(matches!(rx.recv().await, Some(XmppEvent::RoomMessage { body, .. }) if body == index.to_string()));
                }
            });
        }).await.unwrap();
    }

    #[tokio::test]
    async fn prejoin_topic_replay_tracks_later_handoffs_and_close_traffic() {
        for closing in [false, true] {
            let rooms: crate::rooms::Rooms = Default::default();
            rooms.lock().unwrap().insert("room@example".into(), crate::rooms::Room::joining("entry".into(), "me".into()));
            let (tx, mut rx) = mpsc::channel(crate::rooms::MAX_JOIN_BUFFER_EVENTS + 4);
            process_groupchat_message("<message xmlns='jabber:client' type='groupchat' from='room@example/alice'><subject>old</subject></message>", &tx, &rooms).await;
            for _ in 0..crate::rooms::MAX_JOIN_BUFFER_EVENTS {
                process_groupchat_message("<message xmlns='jabber:client' type='groupchat' from='room@example/alice'><body>history</body></message>", &tx, &rooms).await;
            }
            while rx.try_recv().is_ok() {}
            process_groupchat_message("<message xmlns='jabber:client' type='groupchat' from='room@example/alice'><subject>new</subject></message>", &tx, &rooms).await;
            if closing { rooms.lock().unwrap().begin_close(); }
            else { rooms.lock().unwrap().defer_buffered("room@example"); }
            if closing {
                process_groupchat_message("<message xmlns='jabber:client' type='groupchat' from='room@example/alice'><subject>final</subject></message>", &tx, &rooms).await;
            }
            let presence = stanza::muc::MucPresence::from_xml("<presence xmlns='jabber:client' from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>").unwrap();
            let events = crate::rooms::presence(&rooms, &presence);
            assert!(events.iter().any(|event| matches!(event, XmppEvent::RoomJoined { .. })));
            assert!(matches!(events.last(), Some(XmppEvent::RoomSubject { subject, .. }) if subject == if closing { "final" } else { "new" }));
        }
    }

    #[tokio::test]
    async fn message_titles_and_threads_do_not_change_room_topics() {
        let rooms: crate::rooms::Rooms = Default::default();
        let mut room = crate::rooms::Room::joining("entry".into(), "me".into());
        room.phase = crate::rooms::Phase::Joined;
        rooms.lock().unwrap().insert("room@example".into(), room);
        let (tx, mut rx) = mpsc::channel(8);
        for (payload, body) in [
            ("<body>ordinary message</body>", Some("ordinary message")),
            ("<body/>", Some("")),
            ("<thread>discussion</thread>", None),
            ("<thread/>", None),
            ("<c:thread xmlns:c='jabber:client'>discussion</c:thread>", None),
        ] {
            let xml = format!("<message xmlns='jabber:client' type='groupchat' from='room@example/alice'><subject>message title</subject>{payload}</message>");
            process_groupchat_message(&xml, &tx, &rooms).await;
            if let Some(body) = body {
                assert!(matches!(rx.try_recv(), Ok(XmppEvent::RoomMessage { body: actual, .. }) if actual == body));
            }
            assert!(rx.try_recv().is_err(), "message subject with {payload} changed room topic");
        }
        for payload in ["<thread xmlns='foreign'>extension</thread>", "<body xmlns='foreign'>extension</body>", "<x xmlns='foreign'><thread xmlns='jabber:client'>nested</thread></x>"] {
            let xml = format!("<message xmlns='jabber:client' type='groupchat' from='room@example/alice'><subject>real topic</subject>{payload}</message>");
            process_groupchat_message(&xml, &tx, &rooms).await;
            assert!(matches!(rx.try_recv(), Ok(XmppEvent::RoomSubject { subject, .. }) if subject == "real topic"));
            assert!(rx.try_recv().is_err());
        }
    }

    #[test]
    fn starttls_requires_a_complete_namespaced_proceed_response()
    {
        assert!(validate_tls_proceed("<proceed xmlns='urn:ietf:params:xml:ns:xmpp-tls'/>").is_ok());
        assert!(validate_tls_proceed("<t:proceed xmlns:t='urn:ietf:params:xml:ns:xmpp-tls'></t:proceed>").is_ok());
        for xml in ["<proceed/>", "<proceed xmlns='unrelated'/>",
            "<failure xmlns='urn:ietf:params:xml:ns:xmpp-tls'/>",
            "<t:failure xmlns:t='urn:ietf:params:xml:ns:xmpp-tls'/>",
            "<message><proceed xmlns='urn:ietf:params:xml:ns:xmpp-tls'/></message>",
            "<proceed xmlns='urn:ietf:params:xml:ns:xmpp-tls'><extra/></proceed>",
            "<proceed xmlns='urn:ietf:params:xml:ns:xmpp-tls'>text</proceed>",
            "<proceed xmlns='urn:ietf:params:xml:ns:xmpp-tls'>",
            "<proceed xmlns='urn:ietf:params:xml:ns:xmpp-tls'/><extra/>"] {
            assert!(matches!(validate_tls_proceed(xml), Err(XmppError::Tls(_))), "{xml}");
        }
    }

    #[test]
    fn parse_jid_strips_resource()
    {
        let (user, domain) = parse_jid("user@example.com/resource").unwrap();
        assert_eq!(user, "user");
        assert_eq!(domain, "example.com");
    }

    #[test]
    fn account_jid_requires_nonempty_valid_sections()
    {
        for jid in ["@example", "user@", "user@@example", "user@exam ple", "us er@example", "user/path@example", "user@example/", "user@example/a\nb", "user:bad@example", "user@example\0"] {
            assert!(matches!(parse_jid(jid), Err(XmppError::InvalidJid(_))), "{jid}");
        }
        assert_eq!(parse_jid("user@example/a/b@c").unwrap(), ("user", "example"));
        assert!(parse_jid("a=,b@example/resource with spaces").is_ok());
        assert!(parse_jid("å@example/resource").is_ok());
    }

    #[test]
    fn account_addresses_cannot_put_xml_noncharacters_on_the_stream() {
        for jid in ["user\u{ffff}@example", "user@example/\u{fffe}", "user@exa\u{ffff}mple"] {
            assert!(matches!(parse_jid(jid), Err(XmppError::InvalidJid(_))));
        }
    }

    #[test]
    fn account_jid_parts_are_limited_by_utf8_bytes_on_the_wire() {
        let local = format!("{}a", "é".repeat(511));
        let resource = format!("{}abc", "🙂".repeat(255));
        assert_eq!(local.len(), 1023);
        assert_eq!(resource.len(), 1023);
        assert!(parse_jid(&format!("{local}@example/{resource}")).is_ok());
        for jid in [format!("{}@example/resource", "é".repeat(512)),
            format!("user@{}", "d".repeat(1024)), format!("user@example/{}", "🙂".repeat(256))] {
            assert!(matches!(parse_jid(&jid), Err(XmppError::InvalidJid(_))), "oversized JID part accepted");
        }
    }

    #[test]
    fn ipv6_account_domains_keep_their_brackets_and_reject_ports_or_malformed_literals() {
        assert_eq!(parse_jid("user@[2001:db8::1]/resource").unwrap(), ("user", "[2001:db8::1]"));
        assert_eq!(parse_jid("user@[::1]").unwrap(), ("user", "[::1]"));
        for jid in ["user@::1", "user@[::1]:5222", "user@example:5222", "user@[example]", "user@[::1", "user@::1]", "user@ho[st]", "user@[fe80::1%en0]"] {
            assert!(parse_jid(jid).is_err(), "{jid}");
        }
    }

    #[test]
    fn parse_jid_requires_at()
    {
        assert!(matches!(parse_jid("no-at-sign"), Err(XmppError::InvalidJid(_))));
    }

    #[test]
    fn peek_root_reads_name_and_type()
    {
        let (name, typ, _id) = peek_root("<message xmlns='jabber:client' type='chat' from='a@b'><body>hi</body></message>").unwrap();
        assert_eq!(name, "message");
        assert_eq!(typ.as_deref(), Some("chat"));
    }

    #[test]
    fn peek_root_ignores_body_text()
    {
        // A body that contains markup-like text must not change the dispatch:
        // the root element is still a type='chat' message.
        let (name, typ, _id) = peek_root(
            "<message xmlns='jabber:client' type='chat' from='a@b'><body>look: type='groupchat'</body></message>"
        ).unwrap();
        assert_eq!(name, "message");
        assert_eq!(typ.as_deref(), Some("chat"));
    }

    #[test]
    fn peek_root_handles_self_closing()
    {
        let (name, typ, _id) = peek_root("<presence xmlns='jabber:client' type='unavailable'/>").unwrap();
        assert_eq!(name, "presence");
        assert_eq!(typ.as_deref(), Some("unavailable"));
    }

    #[test]
    fn peek_root_reads_iq_id()
    {
        let (name, typ, id) = peek_root("<iq xmlns='jabber:client' type='result' id='ping_1'/>").unwrap();
        assert_eq!(name, "iq");
        assert_eq!(typ.as_deref(), Some("result"));
        assert_eq!(id.as_deref(), Some("ping_1"));
    }

    #[test]
    fn stanza_roots_require_the_client_namespace()
    {
        for name in ["message", "presence", "iq"] {
            for namespace in ["", "jabber:server", "urn:example:extension"] {
                assert!(peek_root(&format!("<{name} xmlns='{namespace}'/>")).is_none());
                assert!(peek_root(&format!("<x:{name} xmlns:x='{namespace}'/>")).is_none());
            }
            assert!(peek_root(&format!("<{name}/>")).is_none());
            let (root, _, _) = peek_root(&format!("<c:{name} xmlns:c='jabber:client'/>")).unwrap();
            assert_eq!(root, name);
        }
    }

    #[tokio::test]
    async fn stream_setup_accepts_both_content_namespace_declaration_styles() {
        use tokio::io::AsyncWriteExt;
        for prefix_free in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let socket = tokio::net::TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
            let (mut server, _) = listener.accept().await.unwrap();
            let mut tcp = Tcp::Connected(socket, "example".into());
            let (header, feature_name, message_namespace) = if prefix_free {
                ("<stream xmlns='http://etherx.jabber.org/streams' from='example'>", "features", " xmlns='jabber:client'")
            } else {
                ("<s:stream xmlns:s='http://etherx.jabber.org/streams' xmlns='jabber:client' from='example'>", "s:features", "")
            };
            let wire = format!("{header}<{feature_name}><mechanisms xmlns='urn:ietf:params:xml:ns:xmpp-sasl'><mechanism>SCRAM-SHA-256</mechanism></mechanisms></{feature_name}><message{message_namespace} from='alice@example'><body>following stanza</body></message>");
            server.write_all(wire.as_bytes()).await.unwrap();
            let mut framer = XmlFramer::new();
            let (_, features) = tokio::time::timeout(std::time::Duration::from_secs(2), read_stream_and_features(&mut tcp, &mut framer)).await.unwrap().unwrap();
            assert_eq!(features.mechanisms.unwrap().mechanism, ["SCRAM-SHA-256"]);
            let message = tokio::time::timeout(std::time::Duration::from_secs(2), read_frame(&mut tcp, &mut framer)).await.unwrap().unwrap();
            assert_eq!(stanza::chat::IncomingChatMessage::from_xml(&message).unwrap().body.as_deref(), Some("following stanza"));
        }
    }

    #[tokio::test]
    async fn stream_setup_validates_header_attributes_and_preserves_following_stanzas() {
        use tokio::io::AsyncWriteExt;
        for (attributes, valid) in [("", true), (" x:extra='value'", false),
            (" xmlns:p='urn:same' xmlns:q='urn:same' p:a='one' q:a='two'", false)] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let socket = tokio::net::TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
            let (mut server, _) = listener.accept().await.unwrap();
            let mut tcp = Tcp::Connected(socket, "example".into());
            let wire = format!("<s:stream xmlns:s='http://etherx.jabber.org/streams' xmlns='jabber:client' from='example'{attributes}><s:features><mechanisms xmlns='urn:ietf:params:xml:ns:xmpp-sasl'><mechanism>SCRAM-SHA-256</mechanism></mechanisms></s:features><message from='alice@example'><body>following stanza</body></message>");
            server.write_all(wire.as_bytes()).await.unwrap();
            let mut framer = XmlFramer::new();
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), read_stream_and_features(&mut tcp, &mut framer)).await.unwrap();
            if valid {
                let (_, features) = result.unwrap();
                assert_eq!(features.mechanisms.unwrap().mechanism, ["SCRAM-SHA-256"]);
                let following = tokio::time::timeout(std::time::Duration::from_secs(2), read_frame(&mut tcp, &mut framer)).await.unwrap().unwrap();
                assert_eq!(stanza::chat::IncomingChatMessage::from_xml(&following).unwrap().body.as_deref(), Some("following stanza"));
            } else {
                assert!(matches!(result, Err(XmppError::Parse(_))), "invalid header attributes accepted: {attributes}");
            }
        }
    }

    #[tokio::test]
    async fn foreign_namespace_frames_cannot_emit_events_resolve_iqs_or_count_as_stanzas()
    {
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let (writer, _server) = loopback_writer().await;
        let sm = disabled_sm();
        sm.enable();
        let pending_iqs: PendingIqs = Arc::new(Mutex::new(HashMap::new()));
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        pending_iqs.lock().unwrap().insert("ping_1".into(), crate::PendingIq::new(tx, None, "me@example/test"));
        let local_disco = Arc::new(Mutex::new(crate::LocalDisco::default()));
        for xml in [
            "<message xmlns='urn:example:extension' type='chat' from='alice@example'><body>forged</body></message>",
            "<presence xmlns='jabber:server' from='room@example/nick'/>",
            "<x:iq xmlns:x='urn:example:extension' type='result' id='ping_1'/>",
            "<message xmlns='' type='chat' from='alice@example'><body>forged</body></message>",
        ] {
            process_stanza_with_count(xml, true, &event_tx, &Default::default(), &pending_iqs, &writer, &local_disco, &sm, None).await;
        }
        assert_eq!(sm.inbound_count(), 0);
        assert!(event_rx.try_recv().is_err());
        assert!(matches!(rx.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)));
        assert!(pending_iqs.lock().unwrap().contains_key("ping_1"));
        process_stanza_with_count("<c:message xmlns:c='jabber:client' type='chat' from='alice@example'><c:body>valid</c:body></c:message>",
            true, &event_tx, &Default::default(), &pending_iqs,
            &writer, &local_disco, &sm, None).await;
        assert_eq!(sm.inbound_count(), 1);
        assert!(matches!(event_rx.try_recv(), Ok(XmppEvent::DirectMessage { body, .. }) if body == "valid"));
    }

    #[test]
    fn extension_attributes_cannot_replace_stanza_identity()
    {
        for attributes in [
            "id='real' type='get' x:id='fake' x:type='result'",
            "x:id='fake' x:type='result' id='real' type='get'",
        ] {
            let xml = format!("<iq xmlns='jabber:client' xmlns:x='urn:example:extension' {attributes}/>");
            let (_, typ, id) = peek_root(&xml).unwrap();
            assert_eq!(typ.as_deref(), Some("get"));
            assert_eq!(id.as_deref(), Some("real"));
        }
        let (_, typ, id) = peek_root("<iq xmlns='jabber:client' xmlns:x='urn:example:extension' x:id='fake' x:type='result'/>").unwrap();
        assert_eq!((typ, id), (None, None));
        assert!(peek_root("<iq xmlns='jabber:client' id='one' id='two'/>").is_none());
        assert!(peek_root("<iq xmlns='jabber:client' id='&undefined;'/>").is_none());
    }

    #[tokio::test]
    async fn supplied_resource_is_requested_but_server_assigned_jid_is_authoritative()
    {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket = tokio::net::TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (mut server, _) = listener.accept().await.unwrap();
        let mut tcp = Tcp::Connected(socket, "localhost".into());
        let mut framer = XmlFramer::new_client_stream();
        framer.feed(b"<iq xmlns='jabber:client' type='result' id='bind_1'><bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'><jid>canonical@example/assigned</jid></bind></iq>");
        assert_eq!(do_bind(&mut tcp, &mut framer, "example", Some("requested<&")).await.unwrap(), "canonical@example/assigned");
        let expected = stanza::bind::BindRequest::new("bind_1".into(), Some("requested<&".into())).as_bytes();
        let mut received = vec![0; expected.len()];
        tokio::time::timeout(std::time::Duration::from_secs(1), server.read_exact(&mut received)).await.unwrap().unwrap();
        assert_eq!(received, expected);
        assert!(String::from_utf8(received).unwrap().contains("<resource>requested&lt;&amp;</resource>"));
    }

    #[tokio::test]
    async fn bind_waits_for_its_request_and_preserves_interleaved_stanzas()
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket = tokio::net::TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (_server, _) = listener.accept().await.unwrap();
        let mut tcp = Tcp::Connected(socket, "localhost".into());
        let mut framer = XmlFramer::new_client_stream();
        framer.feed(b"<iq xmlns='jabber:client' type='result' id='unrelated'><bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'><jid>wrong@example/resource</jid></bind></iq><message id='keep'/><iq xmlns='jabber:client' type='result' id='bind_1'><bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'><jid>right@example/resource</jid></bind></iq>");
        assert_eq!(do_bind(&mut tcp, &mut framer, "example", None).await.unwrap(), "right@example/resource");
        assert!(framer.try_next().unwrap().contains("unrelated"));
        assert!(framer.last_frame_unmanaged());
        assert!(framer.try_next().unwrap().contains("keep"));
    }

    #[test]
    fn binding_accepts_equivalent_ipv6_server_but_no_other_authority() {
        for (from, accepted) in [("[2001:0db8:0:0:0:0:0:1]", true), ("[2001:db8::2]", false),
            ("user@[2001:db8::1]", false), ("[2001:db8::1]/resource", false)] {
            let xml = format!("<iq xmlns='jabber:client' from='{from}' type='result' id='bind_1'/>");
            assert_eq!(is_bind_reply(&xml, "bind_1", "[2001:db8::1]"), accepted);
        }
    }

    #[tokio::test]
    async fn setup_reports_stream_errors_instead_of_continuing_negotiation() {
        use tokio::io::AsyncReadExt;
        for stage in 0..5 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let socket = tokio::net::TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
            let (mut peer, _) = listener.accept().await.unwrap();
            let mut tcp = Tcp::Connected(socket, "example".into());
            let mut framer = XmlFramer::new_client_stream();
            framer.feed(b"<s:error xmlns:s='http://etherx.jabber.org/streams'><policy-violation xmlns='urn:ietf:params:xml:ns:xmpp-streams'/></s:error>");
            // Test the common reader, feature exchange, binding, SASL and SM.
            let result = match stage {
                0 => read_frame(&mut tcp, &mut framer).await.map(|_| ()),
                1 => {
                    framer.reset();
                    framer.feed(b"<stream:stream xmlns:stream='http://etherx.jabber.org/streams' xmlns='jabber:client'><stream:error><host-unknown xmlns='urn:ietf:params:xml:ns:xmpp-streams'/></stream:error>");
                    read_stream_and_features(&mut tcp, &mut framer).await.map(|_| ())
                }
                2 => do_bind(&mut tcp, &mut framer, "example", None).await.map(|_| ()),
                3 => do_sasl(&mut tcp, &mut framer, "me", "test-password", &SaslMechanism::Plain).await,
                _ => enable_stream_management(&mut tcp, &mut framer).await.map(|_| ()),
            };
            assert!(matches!(&result, Err(XmppError::Stream(reason)) if reason.contains(if stage == 1 { "host-unknown" } else { "policy-violation" })), "stage={stage}, result={result:?}");
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                let mut received = Vec::new();
                while !received.ends_with(b"</stream:stream>") {
                    assert!(peer.read_buf(&mut received).await.unwrap() > 0);
                    assert!(received.len() < 65536);
                }
            }).await.unwrap();
        }
    }

    #[tokio::test]
    async fn bind_ignores_other_senders_and_requests_even_when_the_id_matches() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket = tokio::net::TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (_server, _) = listener.accept().await.unwrap();
        let mut tcp = Tcp::Connected(socket, "example".into());
        let mut framer = XmlFramer::new_client_stream();
        let unrelated = [
            "<iq type='result' from='attacker@example' id='bind_1'><bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'><jid>wrong@example/resource</jid></bind></iq>",
            "<iq type='error' from='other.example' id='bind_1'/>",
            "<iq type='get' from='example' id='bind_1'><ping xmlns='urn:xmpp:ping'/></iq>",
        ];
        for xml in unrelated { framer.feed(xml.as_bytes()); }
        framer.feed(b"<iq type='result' from='EXAMPLE' id='bind_1'><bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'><jid>canonical@example/resource</jid></bind></iq>");
        assert_eq!(do_bind(&mut tcp, &mut framer, "example", None).await.unwrap(), "canonical@example/resource");
        for xml in unrelated {
            let frame = framer.try_next().expect("unrelated stanza must be preserved");
            assert_eq!(peek_root(&frame), peek_root(&xml.replacen("<iq", "<iq xmlns='jabber:client'", 1)));
            assert!(framer.last_frame_unmanaged());
        }
        assert!(framer.try_next().is_none());
        assert!(is_bind_reply("<iq xmlns='jabber:client' from='example' type='error' id='bind_1'/>", "bind_1", "example"));
        assert!(!is_bind_reply("<iq xmlns='jabber:client' from='example/resource' type='result' id='bind_1'/>", "bind_1", "example"));
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
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        pending_iqs.lock().unwrap().insert("ping_1".to_string(), crate::PendingIq::new(tx, None, "me@example/test"));

        let (writer, _server) = loopback_writer().await;
        let local_disco: LocalDiscoState = std::sync::Arc::new(std::sync::Mutex::new(crate::LocalDisco::default()));

        for xml in [
            "<iq xmlns:x='urn:example:extension' type='result' x:id='ping_1'/>",
            "<iq xmlns:x='urn:example:extension' x:type='result' id='ping_1'/>",
            "<iq xmlns:x='urn:example:extension' type='get' id='ping_1' x:type='result'/>",
            "<iq type='result' id='ping_1' from='mallory@evil.example/phone'/>",
            "<iq type='error' id='ping_1' from='mallory@evil.example/phone'/>",
            "<iq xmlns:x='urn:example:extension' type='result' id='ping_1' from='mallory@evil.example/phone' x:from='example'/>",
        ] {
            process_stanza(xml, &event_tx, &mut pending_joins, &mut pending_messages,
                &mut joined_rooms, &pending_iqs, &writer, &local_disco, &disabled_sm()).await;
            assert!(matches!(rx.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)));
            assert!(pending_iqs.lock().unwrap().contains_key("ping_1"));
        }

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
        assert_eq!(peek_root(&rx.await.unwrap()).unwrap().2.as_deref(), Some("ping_1"));
        assert!(pending_iqs.lock().unwrap().is_empty());
    }

    #[test]
    fn discovery_nodes_preserve_advertised_hashes_and_reject_unknown_nodes() {
        let disco: LocalDiscoState = Arc::new(Mutex::new(crate::LocalDisco::default()));
        let query = |namespace: &str, node: Option<&str>| {
            let attr = node.map(|node| format!(" node='{}'", crate::stanza::xml::escape_attribute_value(node))).unwrap_or_default();
            build_iq_get_reply(&format!("<iq xmlns='jabber:client' type='get' from='peer@example' id='q'><query xmlns='{namespace}'{attr}/></iq>"), &disco).unwrap()
        };
        let old_info = disco.lock().unwrap().info.clone();
        let old_node = format!("{}#{}", crate::DEFAULT_CAPS_NODE, stanza::caps::compute_ver(&old_info));
        let _ = crate::build_caps_presence(&disco);
        {
            let mut config = disco.lock().unwrap();
            config.info.features.push("urn:test:new-feature".into());
            config.caps_node = "https://example/new-software".into();
            config.items.push(crate::DiscoItem { jid: "hosted@example".into(), name: None, node: None });
        }
        let new_info = disco.lock().unwrap().info.clone();
        let new_node = format!("https://example/new-software#{}", stanza::caps::compute_ver(&new_info));
        let _ = crate::build_caps_presence(&disco);
        let old_reply = query(stanza::disco::DISCO_INFO_NS, Some(&old_node));
        let old = stanza::disco::DiscoInfoResult::from_xml(&old_reply.replacen("<iq ", "<iq xmlns='jabber:client' ", 1)).unwrap().query.unwrap();
        let decoded = crate::DiscoInfo {
            identities: old.identities.into_iter().map(|identity| crate::DiscoIdentity {
                category: identity.category, kind: identity.kind, name: identity.name,
            }).collect(),
            features: old.features.into_iter().map(|feature| feature.var).collect(),
        };
        assert_eq!(stanza::caps::compute_ver(&decoded), stanza::caps::compute_ver(&old_info));
        assert!(!old_reply.contains("urn:test:new-feature"));
        for node in [None, Some(new_node.as_str())] {
            assert!(query(stanza::disco::DISCO_INFO_NS, node).contains("urn:test:new-feature"));
        }
        assert!(query(stanza::disco::DISCO_ITEMS_NS, None).contains("hosted@example"));
        assert!(!query(stanza::disco::DISCO_ITEMS_NS, Some(&old_node)).contains("hosted@example"));
        let snapshots = disco.lock().unwrap().advertised_info.len();
        for namespace in [stanza::disco::DISCO_INFO_NS, stanza::disco::DISCO_ITEMS_NS] {
            for node in ["unknown", "", "https://example/new-software#wrong", "unknown'&<"] {
                let reply = query(namespace, Some(node));
                assert!(reply.contains("type='error'"), "{reply}");
                assert!(reply.contains("<item-not-found "), "{reply}");
                assert!(!reply.contains("<identity"));
                assert!(!reply.contains("hosted@example"));
                let parsed = stanza::disco::DiscoInfoResult::from_xml(&reply.replacen("<iq ", "<iq xmlns='jabber:client' ", 1)).unwrap();
                assert_eq!(parsed.iq_type, "error");
            }
        }
        assert_eq!(disco.lock().unwrap().advertised_info.len(), snapshots);
    }

    #[test]
    fn discovery_replies_use_resolved_payload_names_and_real_routing_attributes() {
        let disco: LocalDiscoState = Arc::new(Mutex::new(crate::LocalDisco::default()));
        let xml = "<c:iq xmlns:c='jabber:client' xmlns:d='http://jabber.org/protocol/disco#info' xmlns:e='extension' type='get' from='real@example' id='real' e:from='forged' e:id='forged'><d:query/></c:iq>";
        let reply = build_iq_get_reply(xml, &disco).expect("prefixed discovery must be answered");
        assert!(reply.contains("to='real@example' id='real'"));
        assert!(reply.contains("<identity"));
        for xml in [
            "<iq xmlns='jabber:client' xmlns:e='extension' e:from='forged' id='q' type='get'><query xmlns='http://jabber.org/protocol/disco#info'/></iq>",
            "<iq xmlns='jabber:client' xmlns:e='extension' from='real@example' e:id='forged' type='get'><query xmlns='http://jabber.org/protocol/disco#info'/></iq>",
            "<iq xmlns='jabber:client' xmlns:e='extension' from='real@example' id='q' type='get'><e:query xmlns='http://jabber.org/protocol/disco#info'/></iq>",
        ] { assert!(build_iq_get_reply(xml, &disco).is_none(), "{xml}"); }
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

    #[test]
    fn ping_replies_require_the_real_payload_namespace_and_routing_attributes() {
        let disco: LocalDiscoState = Arc::new(Mutex::new(crate::LocalDisco::default()));
        let xml = "<c:iq xmlns:c='jabber:client' xmlns:p='urn:xmpp:ping' xmlns:e='extension' type='get' from='real@example' id='real' e:from='forged' e:id='forged'><p:ping/></c:iq>";
        assert_eq!(build_iq_get_reply(xml, &disco).as_deref(), Some("<iq type='result' to='real@example' id='real'/>"));
        for xml in [
            "<iq xmlns='jabber:client' xmlns:e='extension' from='real@example' id='q' type='get'><e:ping xmlns='urn:xmpp:ping'/></iq>",
            "<iq xmlns='jabber:client' xmlns:e='extension' e:from='forged' id='q' type='get'><ping xmlns='urn:xmpp:ping'/></iq>",
            "<iq xmlns='jabber:client' xmlns:e='extension' from='real@example' e:id='forged' type='get'><ping xmlns='urn:xmpp:ping'/></iq>",
            "<iq xmlns='jabber:client' from='real@example' id='q' type='get'><ping xmlns='urn:xmpp:ping'/><query xmlns='http://jabber.org/protocol/disco#info'/></iq>",
        ] { assert!(build_iq_get_reply(xml, &disco).is_none(), "{xml}"); }
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
    async fn ack_validation_waits_until_writer_has_counted_its_stanza()
    {
        let sm = disabled_sm();
        sm.enable();
        let (writer, _server) = loopback_writer().await;
        let guard = writer.lock().await;
        let mut processing = Box::pin(process_with_sm(&sm, &writer, "<a xmlns='urn:xmpp:sm:3' h='1'/>"));
        tokio::select! {
            _ = &mut processing => panic!("ack raced ahead of the writer's counter update"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(1)) => {},
        }
        sm.record_outbound();
        drop(guard);
        processing.await;
        assert_eq!(sm.last_ack(), 1);
    }

    #[tokio::test]
    async fn stream_controls_require_negotiated_version_three_namespace()
    {
        use tokio::io::AsyncReadExt;
        let sm = disabled_sm();
        let (writer, mut server) = loopback_writer().await;
        process_with_sm(&sm, &writer, "<r xmlns='urn:xmpp:sm:3'/>").await;
        process_with_sm(&sm, &writer, "<a xmlns='urn:xmpp:sm:3' h='1'/>").await;
        assert_eq!(sm.last_ack(), 0);
        sm.enable();
        sm.record_outbound();
        for xml in ["<r/>", "<r xmlns='unrelated'/>", "<r xmlns='urn:xmpp:sm:2'/>",
            "<r xmlns='urn:xmpp:sm:3'><extra/></r>", "<a xmlns='unrelated' h='1'/>"] {
            process_with_sm(&sm, &writer, xml).await;
        }
        assert_eq!(sm.last_ack(), 0);
        let mut buf = [0; 128];
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50), server.read(&mut buf)).await.is_err());
        process_with_sm(&sm, &writer, "<s:a xmlns:s='urn:xmpp:sm:3' h='1'/>").await;
        assert_eq!(sm.last_ack(), 1);
        process_with_sm(&sm, &writer, "<s:r xmlns:s='urn:xmpp:sm:3'/>").await;
        let n = server.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"<a xmlns='urn:xmpp:sm:3' h='0'/>");
    }

    #[tokio::test]
    async fn stream_management_negotiation_keeps_interleaved_messages_in_order()
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket = tokio::net::TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (_server, _) = listener.accept().await.unwrap();
        let mut tcp = Tcp::Connected(socket, "localhost".into());
        let mut framer = XmlFramer::new_client_stream();
        framer.feed(b"<message id='one'/><presence id='two'/><enabled xmlns='urn:xmpp:sm:3'/><message id='three'/>");
        assert!(enable_stream_management(&mut tcp, &mut framer).await.unwrap());
        for expected in ["one", "two"] {
            assert!(framer.try_next().unwrap().contains(expected));
            assert!(framer.last_frame_unmanaged());
        }
        assert!(framer.try_next().unwrap().contains("three"));
        assert!(!framer.last_frame_unmanaged());
    }

    #[tokio::test]
    async fn stream_management_enable_validates_response_namespace()
    {
        for (response, expected) in [
            ("<s:enabled xmlns:s='urn:xmpp:sm:3'/>", Some(true)),
            ("<enabled xmlns='unrelated'/>", None),
            ("<enabled xmlns='urn:xmpp:sm:2'/>", None),
            ("<failed xmlns='urn:xmpp:sm:3'><unexpected-request/></failed>", Some(false)),
            ("<enabled xmlns='urn:xmpp:sm:3' max='invalid'/>", None),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let socket = tokio::net::TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
            let (_server, _) = listener.accept().await.unwrap();
            let mut tcp = Tcp::Connected(socket, "localhost".into());
            let mut framer = XmlFramer::new_client_stream();
            framer.feed(response.as_bytes());
            assert_eq!(enable_stream_management(&mut tcp, &mut framer).await.ok(), expected, "{response}");
        }
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

        for xml in [
            "<presence from='a@b/c'/>",
            "<presence from='a@b/c'><c xmlns='extension' node='forged' ver='v'/></presence>",
            "<presence from='a@b/c'><extension><c xmlns='http://jabber.org/protocol/caps' node='nested' ver='v'/></extension></presence>",
            "<presence xmlns:e='extension' e:from='forged'><c xmlns='http://jabber.org/protocol/caps' node='node' ver='v'/></presence>",
        ] {
            process_stanza(xml, &event_tx, &mut pending_joins, &mut pending_messages,
                &mut joined_rooms, &pending_iqs, &writer, &local_disco, &disabled_sm()).await;
            assert!(matches!(event_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)), "{xml}");
        }
    }

    #[tokio::test]
    async fn expired_room_cleanup_cannot_cancel_a_replacement_request() {
        use tokio::io::AsyncReadExt;
        let (writer, mut server) = loopback_writer().await;
        let sm = disabled_sm();
        let rooms: crate::rooms::Rooms = Default::default();
        let blocked = writer.lock().await;
        let cleanup = tokio::spawn({
            let writer = writer.clone();
            let rooms = rooms.clone();
            async move { cancel_expired_room(&writer, &sm, &rooms, "room@example", "old-nick".into()).await }
        });
        rooms.lock().unwrap().insert("room@EXAMPLE".into(), crate::rooms::Room::joining("new-request".into(), "new-nick".into()));
        drop(blocked);
        assert!(!cleanup.await.unwrap().unwrap());
        let mut byte = [0];
        assert!(tokio::time::timeout(std::time::Duration::from_millis(20), server.read(&mut byte)).await.is_err());
    }

    #[tokio::test]
    async fn malformed_presence_errors_and_warnings_do_not_cancel_a_join() {
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let rooms: crate::rooms::Rooms = Default::default();
        rooms.lock().unwrap().insert("room@example".into(), crate::rooms::Room::joining("request".into(), "me".into()));
        for payload in [
            "<error type='cancel'><forbidden xmlns='foreign'/></error>",
            "<error xmlns='foreign' type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error>",
            "<error type='continue'><unexpected-request xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error>",
        ] {
            let xml = format!("<presence xmlns='jabber:client' type='error' from='room@example/me'>{payload}</presence>");
            process_presence_error(&xml, &event_tx, &rooms).await;
            assert!(rooms.lock().unwrap().contains_key("room@example"));
            assert!(event_rx.try_recv().is_err());
        }
        process_presence_error("<presence xmlns='jabber:client' type='error' from='room@example/me'><error type='auth'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/><text xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'>No &amp; access</text></error></presence>",
            &event_tx, &rooms).await;
        assert!(!rooms.lock().unwrap().contains_key("room@example"));
        assert!(matches!(event_rx.try_recv(), Ok(XmppEvent::PresenceError { from, condition, text: Some(text), .. })
            if from == "room@example/me" && condition == "forbidden" && text == "No & access"));
    }

    #[tokio::test]
    async fn muc_presence_requires_real_user_payload_and_availability() {
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let mut joins = HashMap::new();
        joins.insert("room@example".into(), Vec::new());
        let mut messages = HashMap::new();
        let mut rooms = HashSet::new();
        let pending_iqs = Arc::new(Mutex::new(HashMap::new()));
        let local_disco = Arc::new(Mutex::new(crate::LocalDisco::default()));
        let (writer, _server) = loopback_writer().await;
        let sm = disabled_sm();
        for xml in [
            "<presence from='room@example/me'><x xmlns='extension'><item jid='forged'/><status code='110'/></x></presence>",
            "<presence from='room@example/me' type='subscribe'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>",
            "<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status xmlns='extension' code='110'/></x></presence>",
        ] {
            process_stanza(xml, &event_tx, &mut joins, &mut messages, &mut rooms,
                &pending_iqs, &writer, &local_disco, &sm).await;
            assert!(event_rx.try_recv().is_err());
            assert!(rooms.is_empty());
        }
        process_stanza("<presence from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item jid='me@example'/><status code='110'/></x></presence>",
            &event_tx, &mut joins, &mut messages, &mut rooms, &pending_iqs, &writer, &local_disco, &sm).await;
        assert!(matches!(event_rx.try_recv(), Ok(XmppEvent::RoomJoined { room, .. }) if room == "room@example"));
        assert!(rooms.contains("room@example"));
        process_stanza("<presence from='room@example/other' type='unavailable'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status xmlns='extension' code='110'/></x></presence>",
            &event_tx, &mut joins, &mut messages, &mut rooms, &pending_iqs, &writer, &local_disco, &sm).await;
        assert!(matches!(event_rx.try_recv(), Ok(XmppEvent::MemberLeft { room, nick }) if room == "room@example" && nick == "other"));
        assert!(rooms.contains("room@example"));
    }

    #[tokio::test]
    async fn normal_and_default_message_types_reach_the_client() {
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let mut joins = HashMap::new();
        let mut messages = HashMap::new();
        let mut rooms = HashSet::new();
        let pending_iqs = Arc::new(Mutex::new(HashMap::new()));
        let local_disco = Arc::new(Mutex::new(crate::LocalDisco::default()));
        let (writer, _server) = loopback_writer().await;
        let sm = disabled_sm();
        sm.enable();
        for attributes in ["", "type='normal'", "type='future-message-type'", "type='chat'", "xmlns:x='extension' x:type='error'"] {
            let xml = format!("<message from='alice@example/phone' xml:lang='en' {attributes}><body xml:lang='sv'>hej</body><body>Hello</body><delay xmlns='extension' stamp='forged'/></message>");
            process_stanza(&xml, &event_tx, &mut joins, &mut messages, &mut rooms,
                &pending_iqs, &writer, &local_disco, &sm).await;
            assert!(matches!(event_rx.try_recv(), Ok(XmppEvent::DirectMessage { from, body, delay: None, .. })
                if from == "alice@example/phone" && body == "Hello"), "{attributes}");
        }
        assert_eq!(sm.inbound_count(), 5);
        assert!(event_rx.try_recv().is_err());
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
            Ok(XmppEvent::DirectMessage { from, body, delay, .. }) =>
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
        for _ in 0..5 { sm.record_outbound(); }
        let (writer, _server) = loopback_writer().await;

        process_with_sm(&sm, &writer, "<a xmlns='urn:xmpp:sm:3' h='5'/>").await;

        assert_eq!(sm.last_ack(), 5);
        process_with_sm(&sm, &writer, "<a xmlns='urn:xmpp:sm:3' h='6'/>").await;
        process_with_sm(&sm, &writer, "<a xmlns='urn:xmpp:sm:3' h='4'/>").await;
        assert_eq!(sm.last_ack(), 5);
        // An <a/> is a control element, not a counted stanza.
        assert_eq!(sm.inbound_count(), 0);
    }
    #[tokio::test]
    async fn archive_traffic_is_counted_without_misrouting_live_messages()
    {
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let (writer, _server) = loopback_writer().await;
        let sm = disabled_sm();
        sm.enable();
        let pending_iqs = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        pending_iqs.lock().unwrap().insert("mam-1".into(), crate::PendingIq::archive(None, "me@example/test"));
        let local_disco = Arc::new(Mutex::new(crate::LocalDisco::default()));
        let mut joins = HashMap::new();
        let mut messages = HashMap::new();
        let mut rooms = HashSet::new();
        // A matching query ID cannot make a foreign forwarded namespace into
        // archived traffic. Nor may a malformed fin consume the pending query.
        for xml in [
            "<message><result xmlns='urn:xmpp:mam:2' queryid='mam-1'><forwarded xmlns='foreign'><message xmlns='jabber:client' from='forged@example'><body>forged</body></message></forwarded></result></message>",
            "<iq type='result' id='mam-1'><fin xmlns='urn:xmpp:mam:2'/><fin xmlns='urn:xmpp:mam:2'/></iq>",
        ] {
            process_stanza(xml, &event_tx, &mut joins, &mut messages, &mut rooms,
                &pending_iqs, &writer, &local_disco, &sm).await;
            assert!(event_rx.try_recv().is_err());
            assert!(pending_iqs.lock().unwrap().contains_key("mam-1"));
        }
        for xml in [
            "<message type='chat'><result xmlns='urn:xmpp:mam:2' queryid='mam-1' id='archive-1'><forwarded xmlns='urn:xmpp:forward:0'><delay xmlns='urn:xmpp:delay' stamp='2026-01-01T00:00:00Z'/><message xmlns='jabber:client' from='alice@example/phone'><body>archived</body><origin-id xmlns='urn:xmpp:sid:0' id='archived-origin'/></message></forwarded></result></message>",
            "<iq type='result' id='mam-1'><fin xmlns='urn:xmpp:mam:2' complete='true'/></iq>",
            "<message type='chat' from='alice@example/phone'><body>urn:xmpp:mam:2 &lt;fin&gt; is protocol text</body></message>",
        ] {
            process_stanza(xml, &event_tx, &mut joins, &mut messages, &mut rooms,
                &pending_iqs, &writer, &local_disco, &sm).await;
        }
        assert_eq!(sm.inbound_count(), 5);
        assert!(matches!(event_rx.try_recv(), Ok(XmppEvent::ArchiveMessage { query_id, stanza_id: Some(id), body, origin_id: Some(origin), .. })
            if query_id == "mam-1" && id == "archive-1" && body == "archived" && origin == "archived-origin"));
        assert!(matches!(event_rx.try_recv(), Ok(XmppEvent::ArchiveComplete { query_id, complete: true, .. }) if query_id == "mam-1"));
        assert!(matches!(event_rx.try_recv(), Ok(XmppEvent::DirectMessage { body, .. }) if body.contains("protocol text")));
    }

    #[tokio::test]
    async fn archive_query_uses_the_counted_writer()
    {
        use tokio::io::AsyncReadExt;
        let (writer, mut server) = loopback_writer().await;
        let sm = disabled_sm();
        sm.enable();
        let mut client = crate::XmppClient {
            shutdown: Arc::new(tokio::sync::Notify::new()),
            task: tokio::spawn(async {}), writer, bound_jid: "me@example/test".into(),
            pending_iqs: Arc::new(Mutex::new(HashMap::new())), pending_iq_changed: Arc::new(tokio::sync::Notify::new()), next_iq_id: 0,
            local_disco: Arc::new(Mutex::new(crate::LocalDisco::default())), rooms: Default::default(), sm_state: sm.clone(),
        };
        client.query_archive(crate::ArchiveQuery {
            query_id: "mam-a'b", to: None, with: Some("alice@example"),
            start: None, end: None, forward: false, cursor: None, max: 20,
        }).await.unwrap();
        assert_eq!(sm.outbound_count(), 1);
        let mut bytes = [0; 4096];
        let n = tokio::time::timeout(std::time::Duration::from_secs(1), server.read(&mut bytes)).await.unwrap().unwrap();
        let xml = std::str::from_utf8(&bytes[..n]).unwrap();
        assert!(xml.contains("id='mam-a&apos;b'"));
        client.close().await;
    }

    #[test]
    fn only_accepted_archive_results_renew_the_idle_deadline()
    {
        let now = tokio::time::Instant::now();
        let pending: PendingIqs = Arc::new(Mutex::new(HashMap::new()));
        let mut request = crate::PendingIq::archive(Some("room@conference.example"), "me@example/test");
        request.touch_archive(now - std::time::Duration::from_secs(30));
        let old_deadline = request.archive_deadline();
        pending.lock().unwrap().insert("q".into(), request);
        assert!(!accept_archive_reply("<message xmlns='jabber:client' from='room@conference.example/mallory'/>", b"message", "q", false, false, &pending));
        assert_eq!(pending.lock().unwrap()["q"].archive_deadline(), old_deadline);
        assert!(accept_archive_reply("<message xmlns='jabber:client' from='room@conference.example'/>", b"message", "q", false, false, &pending));
        assert!(pending.lock().unwrap()["q"].archive_deadline().unwrap() >= now + crate::ARCHIVE_IDLE_TIMEOUT);
    }

}
