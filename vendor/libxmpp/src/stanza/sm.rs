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
        return quick_xml::de::from_str(xml).map_err(|e| e.to_string());
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
        let parsed: AckElement = quick_xml::de::from_str(xml).map_err(|e| e.to_string())?;
        return Ok(Self { h: parsed.h });
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

/// The `h` attribute is the only part of an incoming `<a/>` this library needs;
/// any `xmlns` is ignored (unknown attributes are dropped by the deserializer).
#[derive(Deserialize)]
#[serde(rename = "a")]
struct AckElement
{
    #[serde(rename = "@h")]
    h: u32,
}

#[cfg(test)]
mod tests
{
    use super::*;

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
    fn parse_ack_without_xmlns()
    {
        // Servers often omit the namespace on <a/> once the stream default is
        // established; the count is all we read.
        let ack = Ack::from_xml("<a h='0'/>").unwrap();
        assert_eq!(ack.h(), 0);
    }

    #[test]
    fn ack_without_h_is_rejected()
    {
        assert!(Ack::from_xml("<a xmlns='urn:xmpp:sm:3'/>").is_err());
    }
}
