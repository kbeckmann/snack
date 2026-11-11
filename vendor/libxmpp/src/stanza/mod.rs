pub mod stream;
pub mod sasl;
mod saslprep;
pub mod bind;
pub mod muc;
pub mod chat;
pub mod ping;
pub mod disco;
pub mod sm;
pub mod caps;
pub mod delay;
pub mod xml;
pub mod message_error;
mod message_content;

pub trait Stanza
{
    fn to_xml(&self) -> String;

    fn as_bytes(&self) -> Vec<u8>
    {
        return self.to_xml().into_bytes();
    }
}

// Use the same identity in the root attribute and the XEP-0359 extension so
// peers that understand either representation can correlate retries.
pub fn outgoing_origin(id: Option<&str>) -> (String, String)
{
    match id {
        Some(id) => {
            let id = xml::escape_attribute_value(id);
            (format!(" id='{}'", id), format!("<origin-id xmlns='urn:xmpp:sid:0' id='{}'/>", id))
        }
        None => (String::new(), String::new()),
    }
}

pub mod mam;
/// A XEP-0359 `<stanza-id xmlns='urn:xmpp:sid:0' id='…' by='…'/>` element. A
/// message can carry several (one per archiving entity); the `by` attribute
/// names whose archive assigned the `id`. For a MUC message that is the room
/// JID, for a one-to-one message the receiving account's bare JID — and that
/// same `id` is what the corresponding MAM `<result>` reports, which makes it a
/// stable, timestamp-independent key for deduplicating the live copy against the
/// archived copy.
#[derive(Debug, Clone)]
pub struct StanzaId
{
    pub id: Option<String>,
    pub by: Option<String>,
}

/// Strip any resource part from a JID, yielding the bare JID.
pub fn bare_jid(jid: &str) -> &str
{
    return match jid.find('/')
    {
        Some(slash) => &jid[..slash],
        None => jid,
    };
}

// Bound the exact UTF-8 representation currently used on the wire. This does
// not implement PRECIS/IDNA preparation or complete JID syntax validation.
pub(crate) fn jid_has_valid_lengths(jid: &str) -> bool
{
    if jid.len() > crate::MAX_JID_BYTES { return false; }
    let (bare, resource) = match jid.split_once('/') {
        Some((bare, resource)) => (bare, Some(resource)),
        None => (jid, None),
    };
    let (local, domain) = match bare.split_once('@') {
        Some((local, domain)) => (Some(local), domain),
        None => (None, bare),
    };
    let valid = |part: &str| !part.is_empty() && part.len() <= crate::MAX_JID_PART_BYTES;
    valid(domain) && local.is_none_or(valid) && resource.is_none_or(valid)
}

/// Compare JIDs without changing localpart/resource identity. Domains support
/// ASCII case equivalence and bracketed IPv6 address equivalence only; this is
/// not full PRECIS/IDNA preparation and never resolves DNS aliases.
pub fn jid_matches(left: &str, right: &str) -> bool
{
    if crate::xmpp::parse_address(left).is_err() || crate::xmpp::parse_address(right).is_err() { return false; }
    fn parts(jid: &str) -> Option<(Option<&str>, &str, Option<&str>)> {
        let (bare, resource) = match jid.split_once('/') {
            Some((bare, resource)) if !resource.is_empty() => (bare, Some(resource)),
            Some(_) => return None,
            None => (jid, None),
        };
        let (local, domain) = match bare.split_once('@') {
            Some((local, domain)) if !local.is_empty() && !domain.contains('@') => (Some(local), domain),
            Some(_) => return None,
            None => (None, bare),
        };
        if domain.is_empty() || domain.chars().any(|c| c.is_whitespace() || c.is_control()) { return None; }
        Some((local, domain, resource))
    }
    fn ipv6(domain: &str) -> Option<std::net::Ipv6Addr> {
        domain.strip_prefix('[')?.strip_suffix(']')?.parse().ok()
    }
    let (Some((ll, ld, lr)), Some((rl, rd, rr))) = (parts(left), parts(right)) else { return false; };
    if ll != rl || lr != rr { return false; }
    if ld.contains(['[', ']', ':']) || rd.contains(['[', ']', ':']) {
        return matches!((ipv6(ld), ipv6(rd)), (Some(left), Some(right)) if left == right);
    }
    ld.eq_ignore_ascii_case(rd)
}

/// Pick, from a message's `<stanza-id>` elements, the `id` assigned by `by`.
pub fn stanza_id_by<'a>(ids: &'a [StanzaId], by: &str) -> Option<&'a str>
{
    let mut matches = ids.iter().filter(|s| s.by.as_deref().is_some_and(|authority| jid_matches(authority, by)))
        .filter_map(|s| s.id.as_deref().filter(|id| !id.is_empty()));
    let id = matches.next()?;
    // Multiple IDs from the same authority violate XEP-0359. Decline to use
    // either rather than selecting an attacker-controlled first duplicate.
    return if matches.next().is_none() { Some(id) } else { None };
}

/// Parse only empty direct-child XEP-0359 IDs, resolving prefix and default
/// namespace scopes. Serde's local-name matching cannot distinguish extensions
/// named stanza-id in unrelated namespaces.
pub fn parse_stanza_ids(xml: &str) -> Result<Vec<StanzaId>, String>
{
    use quick_xml::{events::Event, name::ResolveResult, NsReader};
    let normalized_xml = crate::stanza::xml::normalize_xml(xml)?;
    let mut reader = NsReader::from_str(&normalized_xml);
    let mut depth = 0usize;
    let mut seen_root = false;
    let mut ids = Vec::new();
    let mut candidate = None;
    loop {
        match reader.read_resolved_event().map_err(|e| e.to_string())? {
            (ns, Event::Start(e)) => {
                if depth == 0 {
                    if seen_root || e.local_name().as_ref() != b"message" { return Err("Expected one message root".into()); }
                    seen_root = true;
                } else if depth == 1 && e.local_name().as_ref() == b"stanza-id"
                    && matches!(ns, ResolveResult::Bound(ns) if ns.as_ref() == b"urn:xmpp:sid:0") {
                    candidate = read_stanza_id(&e);
                } else if depth == 2 {
                    candidate = None; // An ID must have no child elements.
                }
                depth += 1;
            }
            (ns, Event::Empty(e)) => {
                if depth == 0 {
                    if seen_root || e.local_name().as_ref() != b"message" { return Err("Expected one message root".into()); }
                    seen_root = true;
                } else if depth == 1 && e.local_name().as_ref() == b"stanza-id"
                    && matches!(ns, ResolveResult::Bound(ns) if ns.as_ref() == b"urn:xmpp:sid:0") {
                    if let Some(id) = read_stanza_id(&e) { ids.push(id); }
                } else if depth == 2 { candidate = None; }
            }
            (_, Event::End(_)) => {
                if depth == 2 {
                    if let Some(id) = candidate.take() { ids.push(id); }
                }
                depth = depth.checked_sub(1).ok_or("Unexpected closing element")?;
            }
            (_, Event::Text(e)) => {
                if depth == 2 && !e.is_empty() { candidate = None; }
                if depth == 0 && !e.as_ref().iter().all(u8::is_ascii_whitespace) {
                    return Err("Text outside message root".into());
                }
            }
            (_, Event::CData(_)) | (_, Event::GeneralRef(_)) if depth == 2 => candidate = None,
            (_, Event::Eof) => return if seen_root && depth == 0 { Ok(ids) } else { Err("Incomplete message".into()) },
            _ => {},
        }
    }
}

fn read_stanza_id(element: &quick_xml::events::BytesStart<'_>) -> Option<StanzaId>
{
    let mut id = None;
    let mut by = None;
    for attr in element.attributes() {
        let attr = attr.ok()?;
        match attr.key.as_ref() {
            b"id" => id = Some(attr.unescape_value().ok()?.into_owned()),
            b"by" => by = Some(attr.unescape_value().ok()?.into_owned()),
            _ => {},
        }
    }
    if id.as_deref().is_none_or(str::is_empty) || by.as_deref().is_none_or(str::is_empty) { return None; }
    return Some(StanzaId { id, by });
}

/// Read an origin from the original message, or the direct MAM-forwarded
/// message. It is an identifier supplied by the sender, not proof of identity.
/// Bad or ambiguous metadata never discards otherwise valid message content.
pub(crate) fn parse_origin_id(xml: &str, archive: bool) -> Result<Option<String>, String>
{
    use quick_xml::{events::Event, name::ResolveResult, NsReader};
    #[derive(Clone, Copy)]
    enum Node { Root, Result, Forwarded, Message, Origin, Other }
    use Node::*;
    self::xml::element(xml, b"message", b"jabber:client")?;
    let normalized_xml = crate::stanza::xml::normalize_xml(xml)?;
    let mut reader = NsReader::from_str(&normalized_xml);
    reader.config_mut().expand_empty_elements = true;
    let mut stack = Vec::new();
    let mut count = 0;
    let mut origin = None;
    loop {
        match reader.read_resolved_event().map_err(|error| error.to_string())? {
            (ns, Event::Start(element)) => {
                let namespace = match &ns { ResolveResult::Bound(ns) => ns.as_ref(), _ => b"" };
                let node = match (stack.last(), element.local_name().as_ref(), namespace) {
                    (None, _, _) => if archive { Root } else { Message },
                    (Some(Root), b"result", b"urn:xmpp:mam:2") => Result,
                    (Some(Result), b"forwarded", b"urn:xmpp:forward:0") => Forwarded,
                    (Some(Forwarded), b"message", b"jabber:client") => Message,
                    (Some(Message), b"origin-id", b"urn:xmpp:sid:0") => {
                        count += 1;
                        origin = None;
                        if count == 1 {
                            for attribute in element.attributes() {
                                let attribute = attribute.map_err(|error| error.to_string())?;
                                if attribute.key.as_ref() == b"id" {
                                    origin = Some(attribute.unescape_value().map_err(|error| error.to_string())?.into_owned()).filter(|value| !value.is_empty());
                                }
                            }
                        }
                        Origin
                    }
                    (Some(Origin), _, _) => { origin = None; Other }
                    _ => Other,
                };
                stack.push(node);
            }
            (_, Event::End(_)) => { stack.pop(); }
            (_, Event::Text(text)) if matches!(stack.last(), Some(Origin)) && !text.is_empty() => origin = None,
            (_, Event::CData(_) | Event::GeneralRef(_)) if matches!(stack.last(), Some(Origin)) => origin = None,
            (_, Event::Eof) => return Ok(if count == 1 { origin } else { None }),
            _ => {},
        }
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn encoded_jid_length_limits_include_each_part_and_separators() {
        let full = format!("{}@{}/{}", "u".repeat(1023), "d".repeat(1023), "r".repeat(1023));
        assert_eq!(full.len(), crate::MAX_JID_BYTES);
        assert!(jid_has_valid_lengths(&full));
        assert!(!jid_has_valid_lengths(&format!("{full}x")));
        assert!(jid_has_valid_lengths(&"d".repeat(1023)));
        assert!(!jid_has_valid_lengths(&"d".repeat(1024)));
        let resource = format!("{}@", "a/".repeat(511));
        assert!(jid_has_valid_lengths(&format!("u@d/{resource}")));
        assert!(!jid_has_valid_lengths(&format!("u@d/{resource}x")));
        assert!(!jid_matches(&format!("{}@example", "u".repeat(1024)), &format!("{}@example", "u".repeat(1024))));
    }

    #[test]
    fn domain_equivalence_does_not_fold_users_resources_or_malformed_literals() {
        for (left, right) in [("me@EXAMPLE/Phone", "me@example/Phone"), ("[::1]", "[0:0:0:0:0:0:0:1]")] {
            assert!(jid_matches(left, right));
        }
        for (left, right) in [("Me@example", "me@example"), ("me@example/Phone", "me@example/phone"),
            ("me@example", "me@example/phone"), ("[::ffff:127.0.0.1]", "127.0.0.1"),
            ("::1", "[::1]"), ("[invalid]", "[invalid]"), ("@example", "@example"),
            ("a@@example", "a@@example"), ("example/", "example/"), ("", ""),
            ("example.", "example"), ("[::1]", "[::2]"),
            ("bad user@example", "bad user@example"), ("user:bad@example", "user:bad@example"),
            ("user@example/bad\tresource", "user@example/bad\tresource")] {
            assert!(!jid_matches(left, right), "{left} versus {right}");
        }
    }

    #[test]
    fn origin_ids_are_scoped_to_the_original_message_and_reject_ambiguous_metadata() {
        for (payload, expected) in [
            ("<s:origin-id id='client&amp;id'/>", Some("client&id")),
            ("<origin-id xmlns='extension' id='wrong'/>", None),
            ("<extension><s:origin-id id='nested'/></extension>", None),
            ("<e:origin-id xmlns='urn:xmpp:sid:0' id='wrong'/>", None),
            ("<s:origin-id e:id='qualified'/>", None),
            ("<s:origin-id id=''/>", None),
            ("<s:origin-id id='one'/><s:origin-id id='two'/>", None),
            ("<s:origin-id/><s:origin-id id='two'/>", None),
            ("<s:origin-id id='one'>text</s:origin-id>", None),
            ("<s:origin-id id='one'><child/></s:origin-id>", None),
            ("<s:origin-id id='one'><![CDATA[text]]></s:origin-id>", None),
            ("<s:origin-id id='one'>&amp;</s:origin-id>", None),
        ] {
            let xml = format!("<message xmlns='jabber:client' xmlns:s='urn:xmpp:sid:0' xmlns:e='extension' from='room@example/nick' id='different-root-id'><body>kept</body>{payload}</message>");
            let direct = chat::IncomingChatMessage::from_xml(&xml).unwrap();
            let room = muc::MucMessage::from_xml(&xml).unwrap();
            assert_eq!(direct.origin_id.as_deref(), expected, "{xml}");
            assert_eq!(room.origin_id.as_deref(), expected, "{xml}");
            assert_eq!(direct.body.as_deref(), Some("kept"));
            let archive = format!("<message xmlns='jabber:client'><origin-id xmlns='urn:xmpp:sid:0' id='outer-envelope'/><result xmlns='urn:xmpp:mam:2' queryid='q' id='archive'><forwarded xmlns='urn:xmpp:forward:0'>{xml}</forwarded></result></message>");
            let result = mam::MamResultStanza::from_xml(&archive).unwrap();
            assert_eq!(result.result.forwarded.message.origin_id.as_deref(), expected, "{archive}");
            assert_eq!(result.result.forwarded.message.body.as_deref(), Some("kept"));
        }
    }

    #[test]
    fn stanza_ids_require_namespace_direct_child_and_empty_content()
    {
        for (attributes, payload, expected) in [
            ("", "<stanza-id xmlns='urn:xmpp:sid:0' by='me@example' id='good'/>", Some("good")),
            ("xmlns:s='urn:xmpp:sid:0'", "<s:stanza-id by='me@example' id='good'></s:stanza-id>", Some("good")),
            ("", "<stanza-id by='me@example' id='bad'/>", None),
            ("", "<stanza-id xmlns='unrelated' by='me@example' id='bad'/>", None),
            ("", "<extension><stanza-id xmlns='urn:xmpp:sid:0' by='me@example' id='bad'/></extension>", None),
            ("", "<stanza-id xmlns='urn:xmpp:sid:0' by='me@example' id='bad'>text</stanza-id>", None),
            ("", "<stanza-id xmlns='urn:xmpp:sid:0' by='me@example' id='bad'><nested/></stanza-id>", None),
            ("", "<stanza-id xmlns='urn:xmpp:sid:0' by='me@example' id='bad'><nested></nested></stanza-id>", None),
            ("xmlns:s='urn:xmpp:sid:0'", "<s:stanza-id by='me@example' s:id='bad'/>", None),
            ("", "<stanza-id xmlns='urn:xmpp:sid:0' by='me@example' id=''/><stanza-id xmlns='urn:xmpp:sid:0' by='me@example' id='good'/>", Some("good")),
            ("", "<stanza-id xmlns='urn:xmpp:sid:0' by='me@example'/><stanza-id xmlns='urn:xmpp:sid:0' by='me@example' id='good'/>", Some("good")),
            ("", "<stanza-id xmlns='urn:xmpp:sid:0' by='me@example' id='one'/><stanza-id xmlns='urn:xmpp:sid:0' by='me@example' id='two'/>", None),
        ] {
            let xml = format!("<message xmlns='jabber:client' from='room@example/nick' to='me@example/resource' {attributes}><body>hello</body>{payload}</message>");
            let chat = chat::IncomingChatMessage::from_xml(&xml).unwrap();
            assert_eq!(chat.stanza_id(), expected, "{xml}");
            let room_xml = xml.replace("by='me@example'", "by='room@example'");
            let muc = muc::MucMessage::from_xml(&room_xml).unwrap();
            assert_eq!(muc.stanza_id("room@example"), expected, "{room_xml}");
        }
    }

    #[test]
    fn stanza_id_authorities_use_domain_equivalence_and_reject_equivalent_duplicates()
    {
        for (expected_by, supplied_by) in [("me@EXAMPLE", "me@example"),
            ("me@[2001:db8::1]", "me@[2001:0db8:0:0:0:0:0:1]")] {
            let message = |payload: &str| format!("<message xmlns='jabber:client' from='peer@example/device' to='{expected_by}/device'><body>hello</body>{payload}</message>");
            let first = format!("<stanza-id xmlns='urn:xmpp:sid:0' by='{supplied_by}' id='one'/>");
            let parsed = chat::IncomingChatMessage::from_xml(&message(&first)).unwrap();
            assert_eq!(parsed.stanza_id(), Some("one"));
            let room = muc::MucMessage::from_xml(&message(&first)).unwrap();
            assert_eq!(room.stanza_id(expected_by), Some("one"));
            let duplicate = format!("{first}<stanza-id xmlns='urn:xmpp:sid:0' by='{expected_by}' id='two'/>");
            let parsed = chat::IncomingChatMessage::from_xml(&message(&duplicate)).unwrap();
            assert_eq!(parsed.stanza_id(), None);
            let room = muc::MucMessage::from_xml(&message(&duplicate)).unwrap();
            assert_eq!(room.stanza_id(expected_by), None);
        }
        for by in ["Me@example", "me@example/occupant", "other@example", "me@elsewhere", "me@[::2]"] {
            let xml = format!("<message xmlns='jabber:client' to='me@example/device'><body>hello</body><stanza-id xmlns='urn:xmpp:sid:0' by='{by}' id='foreign'/></message>");
            assert_eq!(chat::IncomingChatMessage::from_xml(&xml).unwrap().stanza_id(), None);
        }
    }

    #[test]
    fn message_parser_rejects_unfinished_or_multiple_roots()
    {
        assert!(parse_stanza_ids("<message><body>unfinished").is_err());
        assert!(parse_stanza_ids("<message/><message/>").is_err());
    }
}
