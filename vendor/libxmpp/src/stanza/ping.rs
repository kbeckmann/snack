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
                quick_xml::escape::escape(to),
                quick_xml::escape::escape(&self.id)
            ),
            None => format!(
                "<iq type='get' id='{}'><ping xmlns='urn:xmpp:ping'/></iq>",
                quick_xml::escape::escape(&self.id)
            ),
        }
    }
}

/// A parsed incoming XEP-0199 ping (`<iq type='get'><ping
/// xmlns='urn:xmpp:ping'/></iq>`).
///
/// A peer sends one to probe whether we are reachable; `from` and `id` are
/// echoed back in the empty result that acknowledges it. `ping.xmlns` is
/// captured so a `<ping>` in some unrelated namespace is not mistaken for a
/// real ping.
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
        return quick_xml::de::from_str(xml).map_err(|e| e.to_string());
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
            quick_xml::escape::escape(&self.to),
            quick_xml::escape::escape(&self.id)
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
        let xml = "<iq type='get' from='peer@example.com/x' id='ping_9'>\
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
        let xml = "<iq type='get' from='a@b' id='q1'>\
            <query xmlns='http://jabber.org/protocol/disco#info'/></iq>";
        assert!(IncomingPing::from_xml(xml).is_err());
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
