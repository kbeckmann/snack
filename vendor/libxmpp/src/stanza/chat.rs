use super::Stanza;
use serde::Deserialize;

pub struct ChatMessage
{
    to: String,
    body: String,
}

impl ChatMessage
{
    pub fn new(to: String, body: String) -> Self
    {
        return Self { to, body };
    }
}

impl Stanza for ChatMessage
{
    fn to_xml(&self) -> String
    {
        return format!(
            "<message type='chat' to='{}'><body>{}</body></message>",
            self.to,
            quick_xml::escape::escape(&self.body)
        );
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename = "message")]
pub struct IncomingChatMessage
{
    #[serde(rename = "@from", default)]
    pub from: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub delay: Option<ChatMessageDelay>,
}

#[derive(Deserialize, Debug)]
pub struct ChatMessageDelay
{
    #[serde(rename = "@stamp", default)]
    pub stamp: Option<String>,
}

impl IncomingChatMessage
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
    fn outgoing_chat_message_escapes_body()
    {
        let msg = ChatMessage::new("user@example.com".to_string(), "<hi> & 'bye'".to_string());
        assert!(msg.to_xml().contains("&lt;hi&gt; &amp; &apos;bye&apos;"));
    }

    #[test]
    fn parse_incoming_chat_message()
    {
        let xml = "<message type='chat' from='alice@example.com/res'><body>Hey there!</body></message>";
        let msg = IncomingChatMessage::from_xml(xml).unwrap();
        assert_eq!(msg.from.as_deref(), Some("alice@example.com/res"));
        assert_eq!(msg.body.as_deref(), Some("Hey there!"));
        assert!(msg.delay.is_none());
    }

    #[test]
    fn parse_incoming_chat_message_with_delay()
    {
        let xml = "<message type='chat' from='alice@example.com'>\
            <body>Offline message</body>\
            <delay xmlns='urn:xmpp:delay' stamp='2025-01-01T12:00:00Z'/>\
            </message>";
        let msg = IncomingChatMessage::from_xml(xml).unwrap();
        assert_eq!(msg.from.as_deref(), Some("alice@example.com"));
        assert_eq!(msg.body.as_deref(), Some("Offline message"));
        assert_eq!(msg.delay.unwrap().stamp.as_deref(), Some("2025-01-01T12:00:00Z"));
    }
}
