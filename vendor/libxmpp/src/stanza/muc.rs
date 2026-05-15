use super::Stanza;
use serde::Deserialize;

pub struct MucJoinPresence
{
    room_jid: String,
    nick: String,
}

pub struct MucGroupMessage
{
    to: String,
    body: String,
}

impl MucGroupMessage
{
    pub fn new(to: String, body: String) -> Self
    {
        Self { to, body }
    }
}

impl Stanza for MucGroupMessage
{
    fn to_xml(&self) -> String
    {
        format!(
            "<message type='groupchat' to='{}'><body>{}</body></message>",
            self.to,
            quick_xml::escape::escape(&self.body)
        )
    }
}

impl MucJoinPresence
{
    pub fn new(room_jid: String, nick: String) -> Self
    {
        Self { room_jid, nick }
    }
}

impl Stanza for MucJoinPresence
{
    fn to_xml(&self) -> String
    {
        format!(
            "<presence to='{}/{}'><x xmlns='http://jabber.org/protocol/muc'/></presence>",
            self.room_jid, self.nick
        )
    }
}

pub struct MucLeavePresence
{
    room_jid: String,
    nick: String,
}

impl MucLeavePresence
{
    pub fn new(room_jid: String, nick: String) -> Self
    {
        Self { room_jid, nick }
    }
}

impl Stanza for MucLeavePresence
{
    fn to_xml(&self) -> String
    {
        format!(
            "<presence type='unavailable' to='{}/{}'/>",
            self.room_jid, self.nick
        )
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename = "presence")]
pub struct MucPresence
{
    #[serde(rename = "@from")]
    pub from: String,
    #[serde(rename = "@type", default)]
    pub presence_type: Option<String>,
    #[serde(rename = "$value", default)]
    children: Vec<MucPresenceChild>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
enum MucPresenceChild
{
    Show(String),
    Status(String),
    X(MucUserX),
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Debug)]
pub struct MucUserX
{
    #[serde(rename = "$value", default)]
    children: Vec<MucUserXChild>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
enum MucUserXChild
{
    Item(MucItem),
    Status(MucStatus),
    #[serde(other)]
    Other,
}

impl MucUserX
{
    pub fn items(&self) -> impl Iterator<Item = &MucItem>
    {
        self.children.iter().filter_map(|c| match c
        {
            MucUserXChild::Item(i) => Some(i),
            _ => None,
        })
    }

    pub fn statuses(&self) -> impl Iterator<Item = &MucStatus>
    {
        self.children.iter().filter_map(|c| match c
        {
            MucUserXChild::Status(s) => Some(s),
            _ => None,
        })
    }

    pub fn jid(&self) -> Option<&str>
    {
        self.children.iter().find_map(|c| match c
        {
            MucUserXChild::Item(i) => i.jid.as_deref(),
            _ => None,
        })
    }
}

#[derive(Deserialize, Debug)]
pub struct MucItem
{
    #[serde(rename = "@affiliation", default)]
    pub affiliation: Option<String>,
    #[serde(rename = "@role", default)]
    pub role: Option<String>,
    #[serde(rename = "@jid", default)]
    pub jid: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct MucStatus
{
    #[serde(rename = "@code")]
    pub code: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename = "message")]
pub struct MucMessage
{
    #[serde(rename = "@from", default)]
    pub from: Option<String>,
    #[serde(rename = "@type", default)]
    pub message_type: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub delay: Option<MessageDelay>,
}

#[derive(Deserialize, Debug)]
pub struct MessageDelay
{
    #[serde(rename = "@stamp", default)]
    pub stamp: Option<String>,
}

impl MucMessage
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        return quick_xml::de::from_str(xml).map_err(|e| e.to_string());
    }

    pub fn room_and_nick(&self) -> Option<(&str, &str)>
    {
        let from = self.from.as_deref()?;
        let slash = from.find('/')?;
        return Some((&from[..slash], &from[slash + 1..]));
    }
}

pub struct PresenceErrorStanza
{
    pub from: String,
    pub error_type: String,
    pub condition: String,
    pub text: Option<String>,
}

impl PresenceErrorStanza
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        use quick_xml::Reader;
        use quick_xml::events::Event;

        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);

        let mut from = String::new();
        let mut error_type = String::new();
        let mut condition = String::new();
        let mut text: Option<String> = None;
        let mut in_error = false;
        let mut in_text = false;

        loop
        {
            match reader.read_event().map_err(|e| e.to_string())?
            {
                Event::Start(ref e) | Event::Empty(ref e) =>
                {
                    let local = e.local_name();
                    let tag = std::str::from_utf8(local.as_ref()).unwrap_or("");
                    match tag
                    {
                        "presence" =>
                        {
                            for attr in e.attributes().flatten()
                            {
                                if attr.key.local_name().as_ref() == b"from"
                                {
                                    from = attr.unescape_value()
                                        .map(|v| v.to_string())
                                        .unwrap_or_default();
                                }
                            }
                        }
                        "error" =>
                        {
                            in_error = true;
                            for attr in e.attributes().flatten()
                            {
                                if attr.key.local_name().as_ref() == b"type"
                                {
                                    error_type = attr.unescape_value()
                                        .map(|v| v.to_string())
                                        .unwrap_or_default();
                                }
                            }
                        }
                        "text" if in_error => in_text = true,
                        other if in_error && other != "text" && condition.is_empty() =>
                        {
                            condition = other.to_string();
                        }
                        _ => {}
                    }
                }
                Event::Text(e) if in_text =>
                {
                    text = std::str::from_utf8(e.as_ref()).ok().map(|s| s.to_string());
                    in_text = false;
                }
                Event::End(ref e) =>
                {
                    let local = e.local_name();
                    let tag = std::str::from_utf8(local.as_ref()).unwrap_or("");
                    match tag
                    {
                        "error" => in_error = false,
                        "text" => in_text = false,
                        _ => {}
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }

        if from.is_empty()
        {
            return Err("Missing @from in presence error".to_string());
        }

        return Ok(Self { from, error_type, condition, text });
    }
}

impl MucPresence
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        return quick_xml::de::from_str(xml).map_err(|e| e.to_string());
    }

    pub fn room_and_nick(&self) -> Option<(&str, &str)>
    {
        let slash = self.from.find('/')?;
        return Some((&self.from[..slash], &self.from[slash + 1..]));
    }

    pub fn show(&self) -> Option<&str>
    {
        return self.children.iter().find_map(|c| match c
        {
            MucPresenceChild::Show(s) => Some(s.as_str()),
            _ => None,
        });
    }

    pub fn status(&self) -> Option<&str>
    {
        return self.children.iter().find_map(|c| match c
        {
            MucPresenceChild::Status(s) => Some(s.as_str()),
            _ => None,
        });
    }

    pub fn muc_user_x(&self) -> Option<&MucUserX>
    {
        return self.children.iter().find_map(|c| match c
        {
            MucPresenceChild::X(x) if !x.children.is_empty() => Some(x),
            _ => None,
        });
    }

    pub fn is_self_presence(&self) -> bool
    {
        return self.muc_user_x()
            .map(|x| x.statuses().any(|s| s.code == "110"))
            .unwrap_or(false);
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn parse_presence_error()
    {
        let xml = "<presence type='error' to='user@example.com/res' from='room@conference.example.com'>\
            <error type='cancel' by='conference.example.com'>\
            <remote-server-not-found xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/>\
            <text xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'>Server-to-server connection failed: unable to resolve service</text>\
            </error></presence>";

        let err = PresenceErrorStanza::from_xml(xml).unwrap();
        assert_eq!(err.from, "room@conference.example.com");
        assert_eq!(err.error_type, "cancel");
        assert_eq!(err.condition, "remote-server-not-found");
        assert_eq!(err.text.as_deref(), Some("Server-to-server connection failed: unable to resolve service"));
    }

    #[test]
    fn parse_presence_error_no_text()
    {
        let xml = "<presence type='error' from='room@conference.example.com'>\
            <error type='auth'>\
            <forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/>\
            </error></presence>";

        let err = PresenceErrorStanza::from_xml(xml).unwrap();
        assert_eq!(err.from, "room@conference.example.com");
        assert_eq!(err.error_type, "auth");
        assert_eq!(err.condition, "forbidden");
        assert!(err.text.is_none());
    }
}
