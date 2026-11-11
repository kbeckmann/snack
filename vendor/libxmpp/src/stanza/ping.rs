use super::Stanza;
use serde::Deserialize;

pub(crate) const PING_NS: &str = "urn:xmpp:ping";

/// An outgoing XEP-0199 ping request.
///
/// Serialises to `<iq type='get'><ping xmlns='urn:xmpp:ping'/></iq>`. When
/// `to` is `None` the ping targets the user's own server (the usual choice for
/// a keep-alive or liveness probe); otherwise it is addressed to the given JID.
pub struct PingRequest
{
    id: String,
    to: Option<String>,
}

impl PingRequest
{
    pub fn new(id: String, to: Option<String>) -> Self
    {
        return Self { id, to };
    }
}

impl Stanza for PingRequest
{
    fn to_xml(&self) -> String
    {
        match &self.to
        {
            Some(to) => format!(
                "<iq type='get' to='{}' id='{}'><ping xmlns='urn:xmpp:ping'/></iq>",
                super::xml::escape_attribute_value(to),
                super::xml::escape_attribute_value(&self.id)
            ),
            None => format!(
                "<iq type='get' id='{}'><ping xmlns='urn:xmpp:ping'/></iq>",
                super::xml::escape_attribute_value(&self.id)
            ),
        }
    }
}

/// A parsed incoming XEP-0199 ping (`<iq type='get'><ping
/// xmlns='urn:xmpp:ping'/></iq>`).
///
/// A peer sends one to probe whether we are reachable; `from` and `id` are
/// echoed back in the empty result that acknowledges it. The payload namespace
/// is resolved in scope so unrelated extensions cannot masquerade as pings.
#[derive(Deserialize, Debug)]
#[serde(rename = "iq")]
pub struct IncomingPing
{
    #[serde(rename = "@from", default)]
    pub from: Option<String>,
    #[serde(rename = "@id", default)]
    pub id: Option<String>,
    pub ping: PingPayload,
}

#[derive(Deserialize, Debug)]
pub struct PingPayload
{
    #[serde(rename = "@xmlns")]
    pub xmlns: String,
}

impl IncomingPing
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        use quick_xml::{events::Event, name::ResolveResult, NsReader};
        let attributes = super::xml::element(xml, b"iq", b"jabber:client")?;
        let attribute = |name| attributes.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
        if attribute("type").as_deref() != Some("get") { return Err("Expected IQ get for ping".into()); }
        let normalized_xml = crate::stanza::xml::normalize_xml(xml)?;
        let mut reader = NsReader::from_str(&normalized_xml);
        reader.config_mut().expand_empty_elements = true;
        let mut depth = 0usize;
        let mut seen_ping = false;
        loop {
            match reader.read_resolved_event().map_err(|e| e.to_string())? {
                (ns, Event::Start(e)) => {
                    match depth {
                        0 => {},
                        1 if !seen_ping && e.local_name().as_ref() == b"ping"
                            && matches!(ns, ResolveResult::Bound(ns) if ns.as_ref() == PING_NS.as_bytes()) => seen_ping = true,
                        _ => return Err("Expected one empty ping payload".into()),
                    }
                    depth += 1;
                }
                (_, Event::End(_)) => depth -= 1, // Complete XML was validated above.
                (_, Event::Text(e)) if depth == 2 && !e.is_empty()
                    || depth == 1 && !e.as_ref().iter().all(u8::is_ascii_whitespace) => return Err("Unexpected ping text".into()),
                (_, Event::CData(_) | Event::GeneralRef(_)) if depth > 0 => return Err("Unexpected ping content".into()),
                (_, Event::Eof) => return if seen_ping {
                    Ok(Self { from: attribute("from"), id: attribute("id"), ping: PingPayload { xmlns: PING_NS.into() } })
                } else { Err("Missing ping payload".into()) },
                _ => {},
            }
        }
    }
}

/// The empty result that acknowledges an incoming ping.
///
/// Serialises to `<iq type='result' to='...' id='...'/>`, echoing the `from`
/// and `id` of the ping it answers.
pub struct PongResponse
{
    id: String,
    to: String,
}

impl PongResponse
{
    pub fn new(id: String, to: String) -> Self
    {
        return Self { id, to };
    }
}

impl Stanza for PongResponse
{
    fn to_xml(&self) -> String
    {
        return format!(
            "<iq type='result' to='{}' id='{}'/>",
            super::xml::escape_attribute_value(&self.to),
            super::xml::escape_attribute_value(&self.id)
        );
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn outgoing_ping_to_server()
    {
        let req = PingRequest::new("ping_1".to_string(), None);
        assert_eq!(
            req.to_xml(),
            "<iq type='get' id='ping_1'><ping xmlns='urn:xmpp:ping'/></iq>"
        );
    }

    #[test]
    fn outgoing_ping_to_target()
    {
        let req = PingRequest::new("ping_2".to_string(), Some("peer@example.com".to_string()));
        assert_eq!(
            req.to_xml(),
            "<iq type='get' to='peer@example.com' id='ping_2'><ping xmlns='urn:xmpp:ping'/></iq>"
        );
    }

    #[test]
    fn parse_incoming_ping()
    {
        let xml = "<iq xmlns='jabber:client' type='get' from='peer@example.com/x' id='ping_9'>\
            <ping xmlns='urn:xmpp:ping'/></iq>";

        let ping = IncomingPing::from_xml(xml).unwrap();
        assert_eq!(ping.from.as_deref(), Some("peer@example.com/x"));
        assert_eq!(ping.id.as_deref(), Some("ping_9"));
        assert_eq!(ping.ping.xmlns, PING_NS);
    }

    #[test]
    fn incoming_ping_rejects_non_ping_iq()
    {
        // A disco query carries no <ping>, so it must not parse as a ping.
        let xml = "<iq xmlns='jabber:client' type='get' from='a@b' id='q1'>\
            <query xmlns='http://jabber.org/protocol/disco#info'/></iq>";
        assert!(IncomingPing::from_xml(xml).is_err());
    }

    #[test]
    fn ping_resolves_prefixes_and_uses_only_unqualified_routing_attributes() {
        let xml = "<c:iq xmlns:c='jabber:client' xmlns:p='urn:xmpp:ping' xmlns:e='extension' type='get' from='real@example' id='real' e:from='forged' e:id='forged'><p:ping/></c:iq>";
        let ping = IncomingPing::from_xml(xml).unwrap();
        assert_eq!(ping.from.as_deref(), Some("real@example"));
        assert_eq!(ping.id.as_deref(), Some("real"));
        assert_eq!(ping.ping.xmlns, PING_NS);
        let ping = IncomingPing::from_xml("<iq xmlns='jabber:client' xmlns:e='extension' type='get' e:from='forged' e:id='forged'><ping xmlns='urn:xmpp:ping'/></iq>").unwrap();
        assert!(ping.from.is_none());
        assert!(ping.id.is_none());
    }

    #[test]
    fn ping_requires_one_empty_payload_in_the_ping_namespace() {
        for payload in [
            "<ping/>", "<e:ping xmlns='urn:xmpp:ping'/>",
            "<wrapper><ping xmlns='urn:xmpp:ping'/></wrapper>",
            "<ping xmlns='urn:xmpp:ping'/><ping xmlns='urn:xmpp:ping'/>",
            "<ping xmlns='urn:xmpp:ping'>text</ping>",
            "<ping xmlns='urn:xmpp:ping'><nested/></ping>",
            "<ping xmlns='urn:xmpp:ping'><![CDATA[text]]></ping>",
            "<ping xmlns='urn:xmpp:ping'>&amp;</ping>",
        ] {
            let xml = format!("<iq xmlns='jabber:client' xmlns:e='extension' type='get'>{payload}</iq>");
            assert!(IncomingPing::from_xml(&xml).is_err(), "{xml}");
        }
        for xml in ["<iq xmlns='extension' type='get'><ping xmlns='urn:xmpp:ping'/></iq>",
            "<iq xmlns='jabber:client' type='result'><ping xmlns='urn:xmpp:ping'/></iq>",
            "<iq xmlns='jabber:client' type='get'><ping xmlns='urn:xmpp:ping'/></iq><extra/>"] {
            assert!(IncomingPing::from_xml(xml).is_err(), "{xml}");
        }
    }

    #[test]
    fn pong_response_to_xml()
    {
        let pong = PongResponse::new("ping_9".to_string(), "peer@example.com/x".to_string());
        assert_eq!(
            pong.to_xml(),
            "<iq type='result' to='peer@example.com/x' id='ping_9'/>"
        );
    }
}
