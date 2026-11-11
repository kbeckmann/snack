//! Message errors are correlated by the root ID, never by echoed body text.
use quick_xml::{events::Event, name::ResolveResult, NsReader};

#[derive(Debug)]
pub struct MessageError
{
    pub id: String,
    pub from: Option<String>,
    pub condition: String,
    pub error_type: String,
    pub text: Option<String>,
}

#[cfg(test)]
mod tests
{
    use super::*;

    fn message(payload: &str) -> String {
        format!("<message xmlns='jabber:client' type='error' id='wire-id' from='alice@example/phone'><body>echoed body</body>{payload}</message>")
    }

    #[test]
    fn prefixed_errors_preserve_identity_and_decode_multilingual_text()
    {
        let xml = message("<error type='cancel' xmlns:e='urn:ietf:params:xml:ns:xmpp-stanzas'><e:forbidden/><e:text xml:lang='en'>No &amp; &#x1F600; <![CDATA[<access>]]></e:text><e:text xml:lang='sv'>Nej</e:text></error>");
        let parsed = MessageError::from_xml(&xml).unwrap();
        assert_eq!(parsed.id, "wire-id");
        assert_eq!(parsed.from.as_deref(), Some("alice@example/phone"));
        assert_eq!(parsed.error_type, "cancel");
        assert_eq!(parsed.condition, "forbidden");
        assert_eq!(parsed.text.as_deref(), Some("No & 😀 <access>"));
    }

    #[test]
    fn error_payload_requires_unambiguous_direct_namespaced_values()
    {
        for payload in [
            "<error type='cancel'><forbidden/></error>",
            "<error xmlns='evil' type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error>",
            "<wrapper><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></wrapper>",
            "<error type='cancel'><wrapper><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></wrapper></error>",
            "<error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/><conflict xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error>",
            "<error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'><child/></forbidden></error>",
            "<error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'>text</forbidden></error>",
            "<error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/><text xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'><child/></text></error>",
            "<error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/><text xmlns='urn:ietf:params:xml:ns:xmpp-stanzas' xml:lang='en'>one</text><text xmlns='urn:ietf:params:xml:ns:xmpp-stanzas' xml:lang='EN'>two</text></error>",
            "<error type='invalid'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error>",
            "<error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error><body>after error</body>",
        ] { assert!(MessageError::from_xml(&message(payload)).is_err(), "{payload}"); }
        assert!(MessageError::from_xml("<message xmlns='jabber:client' type='error' xmlns:x='evil' x:id='spoof'><error type='cancel'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></message>").is_err());
        assert!(MessageError::from_xml(&message("<error type='modify'><redirect xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'>xmpp:other@example</redirect></error>")).is_ok());
        assert_eq!(MessageError::from_xml(&message("<error type='cancel'><future-condition xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error>")).unwrap().condition, "undefined-condition");
        assert_eq!(MessageError::from_xml(&message("<error type='cancel'><future-condition xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'><details/></future-condition></error>")).unwrap().condition, "undefined-condition");
        assert!(MessageError::from_xml(&message("<error type='cancel' xml:lang='en'><forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/><text xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'>one</text><text xmlns='urn:ietf:params:xml:ns:xmpp-stanzas' xml:lang='EN'>two</text></error>")).is_err());
    }
}

impl MessageError
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        Self::from_stanza(xml, b"message", true)
    }

    pub(crate) fn from_presence(xml: &str) -> Result<Self, String>
    {
        Self::from_stanza(xml, b"presence", false)
    }

    fn from_stanza(xml: &str, root: &[u8], require_id: bool) -> Result<Self, String>
    {
        let attrs = super::xml::element(xml, root, b"jabber:client")?;
        let attr = |name: &str| attrs.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
        if attr("type").as_deref() != Some("error") { return Err("Not a stanza error".into()); }
        let id = attr("id").unwrap_or_default();
        if require_id && id.is_empty() { return Err("Message error has no ID".into()); }
        let from = attr("from");
        let mut default_language = attr("xml:lang").unwrap_or_default().to_ascii_lowercase();
        let normalized_xml = crate::stanza::xml::normalize_xml(xml)?;
        let mut reader = NsReader::from_str(&normalized_xml);
        reader.config_mut().expand_empty_elements = true;
        let mut depth = 0usize;
        let mut in_error = false;
        let mut seen_error = false;
        let mut condition = None;
        let mut error_type = None;
        let mut text = None;
        let mut capture_text = false;
        let mut in_text = false;
        let mut in_condition = false;
        let mut condition_text_allowed = false;
        let mut condition_known = true;
        let mut text_languages = std::collections::HashSet::new();
        loop {
            match reader.read_resolved_event().map_err(|e| e.to_string())? {
                (ns, Event::Start(element)) => {
                    if in_text || (in_condition && condition_known) { return Err("Error values must contain text only".into()); }
                    if depth == 1 && seen_error { return Err("Error payload must be last".into()); }
                    if depth == 1 && element.local_name().as_ref() == b"error"
                        && matches!(ns, ResolveResult::Bound(ref ns) if ns.as_ref() == b"jabber:client") {
                        if seen_error { return Err("Duplicate message error payload".into()); }
                        seen_error = true;
                        in_error = true;
                        let mut kind = None;
                        for attr in element.attributes() {
                            let attr = attr.map_err(|e| e.to_string())?;
                            if attr.key.as_ref() == b"type" { kind = Some(attr.unescape_value().map_err(|e| e.to_string())?.into_owned()); }
                            if attr.key.as_ref() == b"xml:lang" { default_language = attr.unescape_value().map_err(|e| e.to_string())?.to_ascii_lowercase(); }
                        }
                        if !matches!(kind.as_deref(), Some("auth" | "cancel" | "continue" | "modify" | "wait")) {
                            return Err("Invalid message error type".into());
                        }
                        error_type = kind;
                    } else if depth == 2 && in_error
                        && matches!(ns, ResolveResult::Bound(ref ns) if ns.as_ref() == b"urn:ietf:params:xml:ns:xmpp-stanzas") {
                        let name = element.local_name();
                        let name = std::str::from_utf8(name.as_ref()).map_err(|e| e.to_string())?;
                        if name == "text" {
                            let mut language = default_language.clone();
                            for attr in element.attributes() {
                                let attr = attr.map_err(|e| e.to_string())?;
                                if attr.key.as_ref() == b"xml:lang" { language = attr.unescape_value().map_err(|e| e.to_string())?.to_ascii_lowercase(); }
                            }
                            if !text_languages.insert(language) { return Err("Duplicate error text language".into()); }
                            in_text = true;
                            capture_text = text.is_none();
                            if capture_text { text = Some(String::new()); }
                        } else {
                            if condition.is_some() { return Err("Ambiguous message error condition".into()); }
                            let known = matches!(name, "bad-request" | "conflict" | "feature-not-implemented" | "forbidden"
                                | "gone" | "internal-server-error" | "item-not-found" | "jid-malformed"
                                | "not-acceptable" | "not-allowed" | "not-authorized" | "policy-violation"
                                | "recipient-unavailable" | "redirect" | "registration-required"
                                | "remote-server-not-found" | "remote-server-timeout" | "resource-constraint"
                                | "service-unavailable" | "subscription-required" | "undefined-condition" | "unexpected-request");
                            condition = Some(if known { name } else { "undefined-condition" }.to_string());
                            in_condition = true;
                            condition_known = known;
                            condition_text_allowed = !known || matches!(name, "gone" | "redirect");
                        }
                    }
                    depth += 1;
                }
                (_, Event::End(_)) => {
                    if depth == 3 { capture_text = false; in_text = false; in_condition = false; }
                    if depth == 2 { in_error = false; }
                    depth -= 1;
                }
                (_, Event::Text(value)) if capture_text => {
                    text.as_mut().unwrap().push_str(&value.decode().map_err(|e| e.to_string())?);
                }
                (_, Event::Text(_)) | (_, Event::CData(_)) | (_, Event::GeneralRef(_)) if in_condition && !condition_text_allowed => {
                    return Err("Error condition must be empty".into());
                }
                (_, Event::CData(value)) if capture_text => {
                    text.as_mut().unwrap().push_str(&value.decode().map_err(|e| e.to_string())?);
                }
                (_, Event::GeneralRef(value)) if capture_text => {
                    let reference = format!("&{};", value.decode().map_err(|e| e.to_string())?);
                    text.as_mut().unwrap().push_str(&quick_xml::escape::unescape(&reference).map_err(|e| e.to_string())?);
                }
                (_, Event::Eof) => break,
                _ => {},
            }
        }
        Ok(Self { id, from, condition: condition.ok_or("No namespaced message error condition")?,
            error_type: error_type.ok_or("No message error type")?, text })
    }
}
