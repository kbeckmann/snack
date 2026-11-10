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
        return quick_xml::de::from_str(xml).map_err(|e| e.to_string());
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

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
