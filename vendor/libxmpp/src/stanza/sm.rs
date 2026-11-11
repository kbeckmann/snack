use super::Stanza;
use serde::Deserialize;

pub(crate) const SM_NS: &str = "urn:xmpp:sm:3";

/// An outgoing request to enable [XEP-0198](https://xmpp.org/extensions/xep-0198.html)
/// stream management.
///
/// Serialises to `<enable xmlns='urn:xmpp:sm:3'/>`. This library requests
/// non-resumable management (it never sends `resume='true'`): it acknowledges
/// stanzas in both directions so delivery can be confirmed, but does not attempt
/// to resume a broken stream, which would require reconnection logic the library
/// does not provide.
pub struct Enable;

impl Stanza for Enable
{
    fn to_xml(&self) -> String
    {
        return format!("<enable xmlns='{}'/>", SM_NS);
    }
}

/// A parsed `<enabled/>` reply confirming the server has turned on stream
/// management.
///
/// The attributes are captured for completeness (and to aid debugging), though
/// this library uses only the fact that the element is `<enabled/>` rather than
/// `<failed/>`: `id`/`resume`/`location` matter only for resumption, which is
/// not attempted.
#[derive(Deserialize, Debug)]
#[serde(rename = "enabled")]
pub struct Enabled
{
    #[serde(rename = "@id", default)]
    pub id: Option<String>,
    #[serde(rename = "@resume", default)]
    pub resume: Option<String>,
    #[serde(rename = "@max", default)]
    pub max: Option<u32>,
    #[serde(rename = "@location", default)]
    pub location: Option<String>,
}

impl Enabled
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        super::xml::empty_element(xml, b"enabled", SM_NS.as_bytes())?;
        let normalized_xml = super::xml::normalize_xml(xml)?;
        return quick_xml::de::from_str(&normalized_xml).map_err(|e| e.to_string());
    }
}

/// An outgoing acknowledgement request (`<r/>`), asking the peer to report how
/// many stanzas it has handled by replying with an [`Ack`].
pub struct AckRequest;

impl Stanza for AckRequest
{
    fn to_xml(&self) -> String
    {
        return format!("<r xmlns='{}'/>", SM_NS);
    }
}

/// A stream-management acknowledgement (`<a h='N'/>`) reporting the number of
/// handled stanzas, `h`.
///
/// It travels in both directions: this library sends one to answer a peer's
/// `<r/>` (with `h` set to the count of stanzas we have handled), and receives
/// one when the peer answers ours (with `h` set to the count of *our* stanzas
/// the peer has handled). Per XEP-0198 the counter is a 32-bit value that wraps
/// at 2^32.
pub struct Ack
{
    h: u32,
}

impl Ack
{
    pub fn new(h: u32) -> Self
    {
        return Self { h };
    }

    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        let attributes = super::xml::empty_element(xml, b"a", SM_NS.as_bytes())?;
        let count = attributes.iter().find(|(name, _)| name == "h")
            .ok_or("Missing stream acknowledgement count")?;
        let h = count.1.parse().map_err(|_| "Invalid stream acknowledgement count".to_string())?;
        return Ok(Self { h });
    }

    pub fn h(&self) -> u32
    {
        return self.h;
    }
}

impl Stanza for Ack
{
    fn to_xml(&self) -> String
    {
        return format!("<a xmlns='{}' h='{}'/>", SM_NS, self.h);
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn protocol_controls_require_the_correct_root_namespace_and_empty_content()
    {
        assert_eq!(Ack::from_xml("<s:a xmlns:s='urn:xmpp:sm:3' h='2'></s:a>").unwrap().h(), 2);
        assert!(Enabled::from_xml("<s:enabled xmlns:s='urn:xmpp:sm:3'/>").is_ok());
        for xml in ["<a xmlns='unrelated' h='1'/>", "<a xmlns='urn:xmpp:sm:2' h='1'/>",
            "<a xmlns='urn:xmpp:sm:3' h='1'><extra/></a>", "<a xmlns='urn:xmpp:sm:3' h='1'>text</a>",
            "<a xmlns='urn:xmpp:sm:3' h='1' h='2'/>", "<a xmlns='urn:xmpp:sm:3' h='1'/><extra/>",
            "<a xmlns='urn:xmpp:sm:3' h='1'>"] {
            assert!(Ack::from_xml(xml).is_err(), "{xml}");
        }
        assert!(Enabled::from_xml("<enabled xmlns='unrelated'/>").is_err());
        assert!(Enabled::from_xml("<failed xmlns='urn:xmpp:sm:3'/>").is_err());
    }

    #[test]
    fn enable_to_xml()
    {
        assert_eq!(Enable.to_xml(), "<enable xmlns='urn:xmpp:sm:3'/>");
    }

    #[test]
    fn ack_request_to_xml()
    {
        assert_eq!(AckRequest.to_xml(), "<r xmlns='urn:xmpp:sm:3'/>");
    }

    #[test]
    fn ack_to_xml()
    {
        assert_eq!(Ack::new(42).to_xml(), "<a xmlns='urn:xmpp:sm:3' h='42'/>");
    }

    #[test]
    fn parse_enabled()
    {
        let enabled = Enabled::from_xml(
            "<enabled xmlns='urn:xmpp:sm:3' id='some-id' resume='true' max='300'/>"
        ).unwrap();
        assert_eq!(enabled.id.as_deref(), Some("some-id"));
        assert_eq!(enabled.resume.as_deref(), Some("true"));
        assert_eq!(enabled.max, Some(300));
    }

    #[test]
    fn parse_bare_enabled()
    {
        let enabled = Enabled::from_xml("<enabled xmlns='urn:xmpp:sm:3'/>").unwrap();
        assert_eq!(enabled.id, None);
        assert_eq!(enabled.resume, None);
    }

    #[test]
    fn parse_ack()
    {
        let ack = Ack::from_xml("<a xmlns='urn:xmpp:sm:3' h='7'/>").unwrap();
        assert_eq!(ack.h(), 7);
    }

    #[test]
    fn parse_ack_without_xmlns_is_rejected()
    {
        assert!(Ack::from_xml("<a h='0'/>").is_err());
    }

    #[test]
    fn ack_without_h_is_rejected()
    {
        assert!(Ack::from_xml("<a xmlns='urn:xmpp:sm:3'/>").is_err());
    }
}
