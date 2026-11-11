use super::{Stanza, StanzaId, stanza_id_by, bare_jid};
use super::delay::Delay;

pub struct ChatMessage
{
    to: String,
    body: String,
    origin_id: Option<String>,
    muc_private: bool,
}

impl ChatMessage
{
    pub fn new(to: String, body: String) -> Self
    {
        return Self { to, body, origin_id: None, muc_private: false };
    }

    pub fn with_muc_private(mut self) -> Self { self.muc_private = true; self }

    pub fn with_origin_id(mut self, origin_id: Option<&str>) -> Self
    {
        self.origin_id = origin_id.map(str::to_string);
        self
    }
}

impl Stanza for ChatMessage
{
    fn to_xml(&self) -> String
    {
        let (id, origin) = super::outgoing_origin(self.origin_id.as_deref());
        let muc = if self.muc_private { "<x xmlns='http://jabber.org/protocol/muc#user'/>" } else { "" };
        return format!(
            "<message type='chat' to='{}'{}><body>{}</body>{}{muc}</message>",
            super::xml::escape_attribute_value(&self.to),
            id, super::xml::escape_text(&self.body), origin
        );
    }
}

#[derive(Debug)]
pub struct IncomingChatMessage
{
    pub origin_id: Option<String>,
    pub from: Option<String>,
    pub muc_private: bool,
    pub to: Option<String>,
    pub body: Option<String>,
    pub delay: Option<Delay>,
    pub stanza_ids: Vec<StanzaId>,
}

impl IncomingChatMessage
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        let message = super::message_content::MessageContent::from_xml(xml)?;
        return Ok(Self { origin_id: message.origin_id, from: message.from, muc_private: message.muc_private, to: message.to, body: message.body, delay: message.delay,
            stanza_ids: super::parse_stanza_ids(xml)? });
    }
    pub fn stanza_id(&self) -> Option<&str>
    {
        let to_bare = bare_jid(self.to.as_deref()?);
        return stanza_id_by(&self.stanza_ids, to_bare);
    }
}

#[cfg(test)]
mod tests
{
    use super::*;
    use crate::stanza::Stanza;

    #[test]
    fn outgoing_chat_message_xml()
    {
        let msg = ChatMessage::new("user@example.com".to_string(), "Hello!".to_string());
        assert_eq!(
            msg.to_xml(),
            "<message type='chat' to='user@example.com'><body>Hello!</body></message>"
        );
    }

    #[test]
    fn outgoing_message_origins_are_escaped_in_both_representations()
    {
        for xml in [
            ChatMessage::new("alice@example".into(), "hello".into()).with_origin_id(Some("id'&<")).to_xml(),
            super::super::muc::MucGroupMessage::new("room@example".into(), "hello".into()).with_origin_id(Some("id'&<")).to_xml(),
        ] {
            let framed = xml.replacen("<message ", "<message xmlns='jabber:client' ", 1);
            let root = super::super::xml::element(&framed, b"message", b"jabber:client").unwrap();
            assert!(root.iter().any(|(key, value)| key == "id" && value == "id'&<"));
            assert!(xml.contains("<origin-id xmlns='urn:xmpp:sid:0' id='id&apos;&amp;&lt;'/>") );
        }
    }

    #[test]
    fn outgoing_chat_message_escapes_body()
    {
        let msg = ChatMessage::new("user@example.com".to_string(), "<hi> & 'bye'".to_string());
        assert!(msg.to_xml().contains("&lt;hi&gt; &amp; &apos;bye&apos;"));
    }

    #[test]
    fn parse_incoming_chat_message()
    {
        let xml = "<message xmlns='jabber:client' type='chat' from='alice@example.com/res'><body>Hey there!</body></message>";
        let msg = IncomingChatMessage::from_xml(xml).unwrap();
        assert_eq!(msg.from.as_deref(), Some("alice@example.com/res"));
        assert_eq!(msg.body.as_deref(), Some("Hey there!"));
        assert!(msg.delay.is_none());
    }

    #[test]
    fn parse_incoming_chat_message_with_delay()
    {
        let xml = "<message xmlns='jabber:client' type='chat' from='alice@example.com'>\
            <body>Offline message</body>\
            <delay xmlns='urn:xmpp:delay' stamp='2025-01-01T12:00:00Z'/>\
            </message>";
        let msg = IncomingChatMessage::from_xml(xml).unwrap();
        assert_eq!(msg.from.as_deref(), Some("alice@example.com"));
        assert_eq!(msg.body.as_deref(), Some("Offline message"));
        assert_eq!(msg.delay.unwrap().stamp.as_deref(), Some("2025-01-01T12:00:00Z"));
    }
    #[test]
    fn parse_incoming_chat_message_stanza_id()
    {
        // The receiving account's server stamps the message for its own archive;
        // `by` is the bare JID it was addressed `to` (resource stripped).
        let xml = "<message xmlns='jabber:client' type='chat' from='alice@example.com/res' to='me@example.com/laptop'>\
            <body>Hey</body>\
            <stanza-id xmlns='urn:xmpp:sid:0' id='arch-9' by='me@example.com'/>\
            </message>";
        let msg = IncomingChatMessage::from_xml(xml).unwrap();
        assert_eq!(msg.stanza_id(), Some("arch-9"));
    }
}
