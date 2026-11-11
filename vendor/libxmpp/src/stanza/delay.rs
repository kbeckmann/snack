use serde::Deserialize;

/// A parsed [XEP-0203](https://xmpp.org/extensions/xep-0203.html) `<delay/>`
/// element, marking a stanza that was stored and delivered later than it was
/// originally sent — for example MUC discussion history replayed on join, or a
/// one-to-one message stored while the recipient was offline.
///
/// `stamp` is the time the stanza was originally sent, formatted per XEP-0082
/// (e.g. `2025-01-01T12:00:00Z`); the specification requires it, but it is kept
/// optional so a malformed element still parses. `from` is the JID of the entity
/// that applied the delay (the MUC or the sending user's server), present only
/// when the server includes it. `reason` is the optional human-readable
/// description carried as the element's text content.
#[derive(Deserialize, Debug, Clone)]
#[serde(rename = "delay")]
pub struct Delay
{
    #[serde(rename = "@stamp", default)]
    pub stamp: Option<String>,
    #[serde(rename = "@from", default)]
    pub from: Option<String>,
    #[serde(rename = "$text", default)]
    pub reason: Option<String>,
}

impl Delay
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        use quick_xml::{events::Event, Reader};
        let attributes = super::xml::element(xml, b"delay", b"urn:xmpp:delay")?;
        if attributes.iter().any(|(_, value)| !crate::xml_framer::valid_xml_text(value)) {
            return Err("Invalid XML character in delay attributes".into());
        }
        let attribute = |name| attributes.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
        let normalized_xml = super::xml::normalize_xml(xml)?;
        let mut reader = Reader::from_str(&normalized_xml);
        reader.config_mut().expand_empty_elements = true;
        let mut depth = 0;
        let mut reason = String::new();
        loop {
            match reader.read_event().map_err(|e| e.to_string())? {
                Event::Start(_) => {
                    if depth != 0 { return Err("Delay reason must contain text only".into()); }
                    depth += 1;
                }
                Event::End(_) => { depth -= 1; }
                Event::Text(text) if depth == 1 => reason.push_str(&text.decode().map_err(|e| e.to_string())?),
                Event::CData(text) if depth == 1 => reason.push_str(&text.decode().map_err(|e| e.to_string())?),
                Event::GeneralRef(reference) if depth == 1 => {
                    let reference = format!("&{};", reference.decode().map_err(|e| e.to_string())?);
                    reason.push_str(&quick_xml::escape::unescape(&reference).map_err(|e| e.to_string())?);
                }
                Event::Eof => break,
                _ => {},
            }
        }
        if !crate::xml_framer::valid_xml_text(&reason) { return Err("Invalid XML character in delay reason".into()); }
        Ok(Self { stamp: attribute("stamp"), from: attribute("from"), reason: (!reason.is_empty()).then_some(reason) })
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn delay_parser_requires_the_protocol_root_and_namespace() {
        for xml in [
            "<delay stamp='2025-01-01T12:00:00Z'/>",
            "<delay xmlns='urn:unrelated' stamp='2025-01-01T12:00:00Z'/>",
            "<message xmlns='urn:xmpp:delay' stamp='2025-01-01T12:00:00Z'/>",
            "<delay xmlns='urn:xmpp:delay'/><delay xmlns='urn:xmpp:delay'/>",
            "<delay xmlns='urn:xmpp:delay'><nested>reason</nested></delay>",
            "<delay xmlns='urn:xmpp:delay'>bad&#0;reason</delay>",
            "<delay xmlns='urn:xmpp:delay' stamp='bad&#0;stamp'/>",
        ] { assert!(Delay::from_xml(xml).is_err(), "accepted unrelated or malformed delay: {xml}"); }
    }

    #[test]
    fn delay_parser_uses_only_unqualified_protocol_attributes() {
        let delay = Delay::from_xml("<d:delay xmlns:d='urn:xmpp:delay' xmlns:x='urn:other' x:stamp='not a timestamp' x:from='wrong@example' stamp='2025-01-01T12:00:00Z' from='right@example'/>").unwrap();
        assert_eq!(delay.stamp.as_deref(), Some("2025-01-01T12:00:00Z"));
        assert_eq!(delay.from.as_deref(), Some("right@example"));
        let delay = Delay::from_xml("<delay xmlns='urn:xmpp:delay' xmlns:x='urn:other' x:stamp='spoofed'/>").unwrap();
        assert!(delay.stamp.is_none());
    }

    #[test]
    fn standalone_delay_matches_live_message_text_and_attribute_decoding() {
        let xml = "<d:delay xmlns:d='urn:xmpp:delay' from='a&#x9;b' stamp='2025-01-01T12:00:00Z'>before &amp; <![CDATA[inside\r\nnext]]> after&#xD;end</d:delay>";
        let standalone = Delay::from_xml(xml).unwrap();
        let message = super::super::chat::IncomingChatMessage::from_xml(&format!("<message xmlns='jabber:client'>{xml}<body>body</body></message>")).unwrap();
        let live = message.delay.unwrap();
        assert_eq!(standalone.stamp, live.stamp);
        assert_eq!(standalone.from, live.from);
        assert_eq!(standalone.reason, live.reason);
        assert_eq!(standalone.reason.as_deref(), Some("before & inside\nnext after\rend"));
    }

    #[test]
    fn parse_full_delay()
    {
        // The canonical XEP-0203 §5 example: a delay carrying the original send
        // time, the responsible entity, and a human-readable reason.
        let xml = "<delay xmlns='urn:xmpp:delay' from='capulet.com' \
            stamp='2002-09-10T23:08:25Z'>Offline Storage</delay>";
        let delay = Delay::from_xml(xml).unwrap();
        assert_eq!(delay.stamp.as_deref(), Some("2002-09-10T23:08:25Z"));
        assert_eq!(delay.from.as_deref(), Some("capulet.com"));
        assert_eq!(delay.reason.as_deref(), Some("Offline Storage"));
    }

    #[test]
    fn parse_minimal_delay()
    {
        // Only the required stamp is present; from and reason are absent.
        let xml = "<delay xmlns='urn:xmpp:delay' stamp='2025-01-01T12:00:00Z'/>";
        let delay = Delay::from_xml(xml).unwrap();
        assert_eq!(delay.stamp.as_deref(), Some("2025-01-01T12:00:00Z"));
        assert!(delay.from.is_none());
        assert!(delay.reason.is_none());
    }
}
