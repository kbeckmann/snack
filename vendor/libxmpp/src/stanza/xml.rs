//! Namespace-aware validation shared by protocol control elements.

// XML end-of-line normalization precedes parsing and reference expansion,
// including in CDATA. Explicit character references must retain their values.
pub(crate) fn normalize_xml(xml: &str) -> Result<std::borrow::Cow<'_, str>, String>
{
    use std::borrow::Cow;
    if !xml.contains('\r') { return normalize_attributes(xml); }
    let line_endings = xml.replace("\r\n", "\n").replace('\r', "\n");
    match normalize_attributes(&line_endings)? {
        Cow::Borrowed(_) => Ok(Cow::Owned(line_endings)),
        Cow::Owned(normalized) => Ok(Cow::Owned(normalized)),
    }
}

// Attribute whitespace becomes spaces; namespace declarations additionally
// need canonical escaping because quick-xml resolves their raw value bytes.
fn normalize_attributes(xml: &str) -> Result<std::borrow::Cow<'_, str>, String>
{
    use quick_xml::{events::Event, Reader};
    let mut reader = Reader::from_str(xml);
    let mut output = None::<String>;
    let mut copied = 0;
    loop {
        match reader.read_event().map_err(|e| e.to_string())? {
            Event::Start(element) | Event::Empty(element) => {
                for attribute in element.attributes().with_checks(false) {
                    let attribute = attribute.map_err(|e| e.to_string())?;
                    let namespace = attribute.key.as_ref() == b"xmlns" || attribute.key.as_ref().starts_with(b"xmlns:");
                    let raw = std::str::from_utf8(attribute.value.as_ref()).map_err(|e| e.to_string())?;
                    if !namespace && !raw.contains(['\t', '\r', '\n']) { continue; }
                    // Literal CRLF becomes one space. Referenced whitespace is
                    // decoded afterwards and must retain its actual character.
                    let literal = raw.replace("\r\n", " ").replace(['\t', '\r', '\n'], " ");
                    let canonical = if namespace {
                        let value = quick_xml::escape::unescape(&literal).map_err(|e| e.to_string())?;
                        if !crate::xml_framer::valid_xml_text(&value) { return Err("Invalid XML namespace character".into()); }
                        escape_attribute_value(&value)
                    } else { literal };
                    if canonical == raw { continue; }
                    // Reader::from_str borrows each raw attribute value from
                    // the original input. Check its byte range before slicing;
                    // only that value is rewritten, preserving all other XML.
                    let start = (raw.as_ptr() as usize).checked_sub(xml.as_ptr() as usize).ok_or("Invalid attribute range")?;
                    let end = start.checked_add(raw.len()).ok_or("Invalid attribute range")?;
                    if start < copied || xml.get(start..end) != Some(raw) { return Err("Invalid attribute range".into()); }
                    let output = output.get_or_insert_with(|| String::with_capacity(xml.len()));
                    output.push_str(&xml[copied..start]);
                    output.push_str(&canonical);
                    copied = end;
                }
            }
            Event::Eof => break,
            _ => {},
        }
    }
    Ok(match output {
        Some(mut output) => { output.push_str(&xml[copied..]); std::borrow::Cow::Owned(output) },
        None => std::borrow::Cow::Borrowed(xml),
    })
}

// A literal CR is normalized by every conforming XML peer. Represent it as
// a reference when serializing intended text, preserving the application value.
pub(crate) fn escape_text(value: &str) -> String
{
    quick_xml::escape::escape(value).replace('\r', "&#xD;")
}

pub(crate) fn escape_attribute_value(value: &str) -> String
{
    quick_xml::escape::escape(value).replace('\t', "&#x9;").replace('\n', "&#xA;").replace('\r', "&#xD;")
}

// A projected element uses its own attribute delimiters. Copy decoded values,
// escaping them for that new element rather than reusing the original raw XML.
// Qualified extension attributes must still never become protocol identity.
pub(crate) fn copy_unqualified_attributes(source: &quick_xml::events::BytesStart<'_>, target: &mut quick_xml::events::BytesStart<'_>) -> Result<(), String>
{
    for attribute in source.attributes() {
        let attribute = attribute.map_err(|e| e.to_string())?;
        if attribute.key.as_ref() == b"xmlns" || attribute.key.as_ref().contains(&b':') { continue; }
        let value = attribute.unescape_value().map_err(|e| e.to_string())?;
        let escaped = escape_attribute_value(&value);
        // The byte-slice form copies already escaped XML. The string form
        // escapes again and would expose entity spellings as application data.
        target.push_attribute((attribute.key.as_ref(), escaped.as_bytes()));
    }
    Ok(())
}

pub fn empty_element(xml: &str, name: &[u8], namespace: &[u8]) -> Result<Vec<(String, String)>, String>
{
    return parse_element(xml, name, namespace, true);
}

pub fn element(xml: &str, name: &[u8], namespace: &[u8]) -> Result<Vec<(String, String)>, String>
{
    return parse_element(xml, name, namespace, false);
}

pub fn is_element(xml: &str, name: &[u8], namespace: &[u8]) -> bool
{
    return element(xml, name, namespace).is_ok();
}

fn parse_element(xml: &str, name: &[u8], namespace: &[u8], empty: bool) -> Result<Vec<(String, String)>, String>
{
    use quick_xml::{events::Event, name::ResolveResult, NsReader};
    let normalized_xml = crate::stanza::xml::normalize_xml(xml)?;
    let mut reader = NsReader::from_str(&normalized_xml);
    reader.config_mut().expand_empty_elements = true;
    let mut seen_root = false;
    let mut depth = 0usize;
    let mut attributes = Vec::new();
    loop {
        let event = reader.read_event().map_err(|e| e.to_string())?;
        crate::xml_framer::validate_node(&reader, &event)
            .map_err(|error| format!("Invalid XMPP XML: {}", error.condition()))?;
        match event {
            Event::Start(e) => {
                if depth == 0 {
                    if seen_root || e.local_name().as_ref() != name
                        || !matches!(reader.resolver().resolve_element(e.name()).0, ResolveResult::Bound(ns) if ns.as_ref() == namespace) {
                        return Err("Unexpected protocol element or namespace".into());
                    }
                    seen_root = true;
                } else if empty { return Err("Protocol element must be empty".into()); }
                for attr in e.attributes() {
                    let attr = attr.map_err(|e| e.to_string())?;
                    if depth == 0 {
                        let key = std::str::from_utf8(attr.key.as_ref()).map_err(|e| e.to_string())?.to_string();
                        attributes.push((key, attr.unescape_value().map_err(|e| e.to_string())?.into_owned()));
                    }
                }
                depth += 1;
            }
            Event::End(_) => depth = depth.checked_sub(1).ok_or("Unexpected closing element")?,
            Event::Text(e) => {
                if (empty && depth > 0 && !e.is_empty())
                    || (depth == 0 && !e.as_ref().iter().all(u8::is_ascii_whitespace)) {
                    return Err("Unexpected protocol text".into());
                }
            }
            Event::CData(_) | Event::GeneralRef(_) if empty || depth == 0 =>
                return Err("Unexpected protocol content".into()),
            Event::DocType(_) => return Err("DTD is not allowed in XMPP".into()),
            Event::Eof => return if seen_root && depth == 0 { Ok(attributes) } else { Err("Incomplete protocol element".into()) },
            _ => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_namespace_bindings_are_rejected_in_stanzas_and_headers() {
        for declaration in [
            "xmlns='http://www.w3.org/XML/1998/namespace'",
            "xmlns='http://www.w3.org/2000/xmlns/'",
            "xmlns:p=''", "xmlns:xml='urn:wrong'", "xmlns:xmlns='urn:wrong'",
            "xmlns:p='http://www.w3.org/XML/1998/namespace'",
            "xmlns:p='http://www.w3.org/2000/xmlns/'",
        ] {
            let xml = format!("<message xmlns='jabber:client'><extra {declaration}/><body>text</body></message>");
            let mut framer = crate::xml_framer::XmlFramer::new_opened();
            framer.feed(xml.as_bytes());
            assert!(framer.try_next().is_none(), "stream accepted reserved binding: {declaration}");
            assert!(framer.error().is_some());
            assert!(element(&xml, b"message", b"jabber:client").is_err(), "helper accepted {declaration}");
            let header = format!("<s:stream xmlns:s='http://etherx.jabber.org/streams' {declaration}>");
            let mut framer = crate::xml_framer::XmlFramer::new();
            framer.feed(header.as_bytes());
            assert!(framer.try_next().is_none(), "header framer accepted {declaration}");
            assert!(framer.error().is_some());
            assert!(crate::stanza::stream::Stream::from_xml(&header).is_err(), "header parser accepted {declaration}");
        }
    }

    #[test]
    fn xmlns_prefix_is_not_an_element_namespace() {
        let xml = "<message xmlns='jabber:client'><xmlns:extra/><body>text</body></message>";
        let mut framer = crate::xml_framer::XmlFramer::new_opened();
        framer.feed(xml.as_bytes());
        assert!(framer.try_next().is_none());
        assert!(framer.error().is_some());
        assert!(element(xml, b"message", b"jabber:client").is_err());
    }

    #[test]
    fn legal_xml_binding_default_undeclaration_and_xml_like_prefixes_still_work() {
        let xml = "<message xmlns='jabber:client' xmlns:xml='http://www.w3.org/XML/1998/namespace'><body xml:lang='en'>text</body><extra xmlns=''><Xmlns:node xmlns:Xmlns='urn:extra'/></extra></message>";
        let mut framer = crate::xml_framer::XmlFramer::new_opened();
        framer.feed(xml.as_bytes());
        assert!(framer.try_next().is_some());
        assert!(framer.error().is_none());
        assert_eq!(crate::stanza::chat::IncomingChatMessage::from_xml(xml).unwrap().body.as_deref(), Some("text"));
        let header = "<s:stream xmlns:s='http://etherx.jabber.org/streams' xmlns='jabber:client' xmlns:xml='http://www.w3.org/XML/1998/namespace'>";
        let mut framer = crate::xml_framer::XmlFramer::new();
        framer.feed(header.as_bytes());
        assert!(framer.try_next().is_some());
        assert!(crate::stanza::stream::Stream::from_xml(header).is_ok());
    }

    #[test]
    fn standalone_protocol_parsers_reject_invalid_xml_like_the_stream() {
        for xml in [
            "<message xmlns='jabber:client' x:extra='ignored'><body>text</body></message>",
            "<message xmlns='jabber:client' xmlns:p='urn:same' xmlns:q='urn:&#115;ame' p:a='one' q:a='two'><body>text</body></message>",
            "<message xmlns='jabber:client'><7extension/><body>text</body></message>",
            "<message xmlns='jabber:client'><body>bad\0text</body></message>",
            "<message xmlns='jabber:client'><body><![CDATA[bad\0text]]></body></message>",
            "<message xmlns='jabber:client'><body>bad&#0;text</body></message>",
            "<message xmlns='jabber:client'><!-- forbidden --><body>text</body></message>",
            "<message xmlns='jabber:client'><?forbidden instruction?><body>text</body></message>",
        ] {
            let mut framer = crate::xml_framer::XmlFramer::new_client_stream();
            framer.feed(xml.as_bytes());
            assert!(framer.try_next().is_none());
            assert!(framer.error().is_some(), "stream accepted {xml:?}");
            assert!(element(xml, b"message", b"jabber:client").is_err(), "standalone helper accepted {xml:?}");
            assert!(crate::stanza::chat::IncomingChatMessage::from_xml(xml).is_err(), "public message parser accepted {xml:?}");
        }
    }

    #[test]
    fn standalone_protocol_parsers_enforce_the_stream_attribute_limit() {
        for count in [crate::xml_framer::MAX_ATTRIBUTES - 1, crate::xml_framer::MAX_ATTRIBUTES] {
            let attributes: String = (0..count).map(|index| format!(" a{index}='value'")).collect();
            let xml = format!("<message xmlns='jabber:client'{attributes}><body>text</body></message>");
            let expected = count < crate::xml_framer::MAX_ATTRIBUTES; // xmlns is also an attribute.
            let mut framer = crate::xml_framer::XmlFramer::new_opened();
            framer.feed(xml.as_bytes());
            assert_eq!(framer.try_next().is_some(), expected);
            assert_eq!(element(&xml, b"message", b"jabber:client").is_ok(), expected);
            assert_eq!(crate::stanza::chat::IncomingChatMessage::from_xml(&xml).is_ok(), expected);
        }
    }

    #[test]
    fn standalone_protocol_parsers_preserve_valid_unicode_names_and_references() {
        let xml = "<message xmlns='jabber:&#99;lient' xmlns:é='urn:extra' é:名='value'><body>é &amp; <![CDATA[🙂]]>&#xD;end</body><é:名/></message>";
        let mut framer = crate::xml_framer::XmlFramer::new_client_stream();
        framer.feed(xml.as_bytes());
        let framed = framer.try_next().unwrap();
        let direct = crate::stanza::chat::IncomingChatMessage::from_xml(xml).unwrap();
        let streamed = crate::stanza::chat::IncomingChatMessage::from_xml(&framed).unwrap();
        assert_eq!(direct.body, streamed.body);
        assert_eq!(direct.body.as_deref(), Some("é & 🙂\rend"));
    }

    #[test]
    fn message_text_normalizes_literal_line_endings_but_preserves_character_references() {
        let xml = "<message xmlns='jabber:client' from='alice@example'><body>a\r\nb\rc&#xD;d<![CDATA[e\r\nf\rg&#xD;]]></body></message>";
        let message = crate::stanza::chat::IncomingChatMessage::from_xml(xml).unwrap();
        assert_eq!(message.body.as_deref(), Some("a\nb\nc\rde\nf\ng&#xD;"));
    }

    #[test]
    fn outgoing_message_text_preserves_intended_carriage_returns() {
        use crate::stanza::Stanza;
        let body = "a\r\nb\rc\n\t & text";
        let wire = crate::stanza::chat::ChatMessage::new("alice@example".into(), body.into()).to_xml()
            .replacen("<message ", "<message xmlns='jabber:client' ", 1);
        assert!(wire.contains("<body>a&#xD;\nb&#xD;c\n\t &amp; text</body>"), "{wire}");
        assert_eq!(crate::stanza::chat::IncomingChatMessage::from_xml(&wire).unwrap().body.as_deref(), Some(body));
    }

    #[test]
    fn archived_message_projection_preserves_referenced_carriage_returns() {
        let xml = "<message xmlns='jabber:client'><result xmlns='urn:xmpp:mam:2' queryid='q' id='a'><forwarded xmlns='urn:xmpp:forward:0'><delay xmlns='urn:xmpp:delay' stamp='2026-01-01T00:00:00Z'/><message xmlns='jabber:client' from='alice@example'><body>a\r\nb&#xD;c<![CDATA[d\re]]></body></message></forwarded></result></message>";
        let archive = crate::stanza::mam::MamResultStanza::from_xml(xml).unwrap();
        assert_eq!(archive.result.forwarded.message.body.as_deref(), Some("a\nb\rcd\ne"));
    }

    #[test]
    fn quoted_attributes_cannot_change_presence_identity_or_affiliation() {
        let xml = r#"<presence xmlns='jabber:client' from='room@example/nick" type="unavailable'><x xmlns='http://jabber.org/protocol/muc#user'><item nick='new" affiliation="owner'/></x></presence>"#;
        let presence = crate::stanza::muc::MucPresence::from_xml(xml).unwrap();
        assert_eq!(presence.from, "room@example/nick\" type=\"unavailable");
        assert!(presence.presence_type.is_none());
        let item = presence.muc_user_x().unwrap().items().next().unwrap();
        assert_eq!(item.nick.as_deref(), Some("new\" affiliation=\"owner"));
        assert!(item.affiliation.is_none());
    }

    #[test]
    fn quoted_discovery_attributes_preserve_names_and_request_identity() {
        let response = r#"<iq xmlns='jabber:client' type='result'><query xmlns='http://jabber.org/protocol/disco#info'><identity category='client' type='pc' name='The "quoted" &amp; O&apos;Brien client'/></query></iq>"#;
        let parsed = crate::stanza::disco::DiscoInfoResult::from_xml(response).unwrap();
        assert_eq!(parsed.query.unwrap().identities[0].name.as_deref(), Some("The \"quoted\" & O'Brien client"));
        let request = r#"<iq xmlns='jabber:client' type='get' from='peer@example' id='request" extra="value'><query xmlns='http://jabber.org/protocol/disco#info' node='node"/branch'/></iq>"#;
        let parsed = crate::stanza::disco::IncomingDiscoQuery::from_xml(request).unwrap();
        assert_eq!(parsed.id.as_deref(), Some("request\" extra=\"value"));
        assert_eq!(parsed.query.node.as_deref(), Some("node\"/branch"));
    }

    #[test]
    fn quoted_archive_attributes_preserve_message_identity_and_sender() {
        let xml = r#"<message xmlns='jabber:client'><result xmlns='urn:xmpp:mam:2' queryid='query" extra="value' id='archive"&#x9;id'><forwarded xmlns='urn:xmpp:forward:0'><delay xmlns='urn:xmpp:delay' stamp='2026-01-01T00:00:00Z'/><message xmlns='jabber:client' from='room@example/nick" type="error'><body>preserve this message</body></message></forwarded></result></message>"#;
        let archive = crate::stanza::mam::MamResultStanza::from_xml(xml).unwrap().result;
        assert_eq!(archive.queryid.as_deref(), Some("query\" extra=\"value"));
        assert_eq!(archive.id.as_deref(), Some("archive\"\tid"));
        assert_eq!(archive.forwarded.message.from.as_deref(), Some("room@example/nick\" type=\"error"));
        assert_eq!(archive.forwarded.message.body.as_deref(), Some("preserve this message"));
    }

    #[test]
    fn attribute_whitespace_is_normalized_before_references_are_decoded() {
        let xml = "<iq xmlns='jabber:client' id='a\r\nb\tc&#xA;d&#xD;e&#x9;f' from='alice@example/one\ntwo'/>";
        let attrs = empty_element(xml, b"iq", b"jabber:client").unwrap();
        assert_eq!(attrs.iter().find(|(key, _)| key == "id").unwrap().1, "a b c\nd\re\tf");
        assert_eq!(attrs.iter().find(|(key, _)| key == "from").unwrap().1, "alice@example/one two");
    }

    #[test]
    fn outgoing_message_attributes_preserve_whitespace_through_standard_xml_parsing() {
        use crate::stanza::Stanza;
        let recipient = "alice@example/one\ttwo\nthree\rfour";
        let origin = "attempt\twith\nwhitespace\r";
        let wire = crate::stanza::chat::ChatMessage::new(recipient.into(), "message\n\t & text".into())
            .with_origin_id(Some(origin)).to_xml().replacen("<message ", "<message xmlns='jabber:client' ", 1);
        assert!(wire.contains("id='attempt&#x9;with&#xA;whitespace&#xD;'"), "{wire}");
        let attrs = element(&wire, b"message", b"jabber:client").unwrap();
        assert_eq!(attrs.iter().find(|(key, _)| key == "id").unwrap().1, origin);
        assert_eq!(attrs.iter().find(|(key, _)| key == "to").unwrap().1, recipient);
        assert_eq!(crate::stanza::parse_origin_id(&wire, false).unwrap().as_deref(), Some(origin));
        assert_eq!(crate::stanza::chat::IncomingChatMessage::from_xml(&wire).unwrap().body.as_deref(), Some("message\n\t & text"));
    }

    #[test]
    fn namespace_normalization_preserves_other_xml_and_is_idempotent() {
        let xml = "<message xmlns='jabber:&#x63;lient' note=\"keep 'quotes' &amp; references\"><body><![CDATA[xmlns='urn:&#115;kip']]>&amp; unchanged</body><x xmlns='urn:a&#38;b'/></message>";
        let normalized = normalize_xml(xml).unwrap();
        assert_eq!(normalized, "<message xmlns='jabber:client' note=\"keep 'quotes' &amp; references\"><body><![CDATA[xmlns='urn:&#115;kip']]>&amp; unchanged</body><x xmlns='urn:a&amp;b'/></message>");
        assert_eq!(normalize_xml(&normalized).unwrap(), normalized);
        assert!(matches!(normalize_xml("<message xmlns='jabber:client'/>"), Ok(std::borrow::Cow::Borrowed(_))));
    }

    #[test]
    fn namespace_attribute_normalization_preserves_referenced_whitespace() {
        let xml = "<x xmlns='urn:a\r\nb\tc&#xA;d&#x9;e&#xD;f'/>";
        assert_eq!(normalize_xml(xml).unwrap(), "<x xmlns='urn:a b c&#xA;d&#x9;e&#xD;f'/>");
        assert!(normalize_xml("<x xmlns='urn:a&#0;'/>").is_err());
        assert!(normalize_xml("<x xmlns='urn:a&unknown;'/>").is_err());
        assert!(empty_element("<a xmlns='urn:xmpp:sm:3' xmlns:xml='&#104;ttp://www.w3.org/XML/1998/namespace'/>", b"a", b"urn:xmpp:sm:3").is_ok());
    }

    #[test]
    fn escaped_stream_header_namespaces_do_not_shift_the_borrowed_suffix() {
        let xml = "<s:stream xmlns:s='http://etherx.jabber.org/&#115;treams' from='exam\r\nple'>\r\n<message note='unchanged'/><presence/>";
        let (header, suffix) = crate::stanza::stream::Stream::from_xml(xml).unwrap();
        assert!(crate::stanza::Stanza::to_xml(&header).contains("from='exam ple'"));
        assert_eq!(suffix, "\r\n<message note='unchanged'/><presence/>");
        assert!(crate::stanza::stream::Stream::from_xml(&format!("\u{feff}{xml}")).is_err());
    }
}
