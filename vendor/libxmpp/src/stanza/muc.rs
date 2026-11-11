use super::{Stanza, StanzaId, stanza_id_by};
use super::delay::Delay;
use serde::Deserialize;

pub struct MucJoinPresence
{
    room_jid: String,
    nick: String,
    id: Option<String>,
}

pub struct MucGroupMessage
{
    to: String,
    body: String,
    origin_id: Option<String>,
}

impl MucGroupMessage
{
    pub fn new(to: String, body: String) -> Self
    {
        Self { to, body, origin_id: None }
    }

    pub fn with_origin_id(mut self, origin_id: Option<&str>) -> Self
    {
        self.origin_id = origin_id.map(str::to_string);
        self
    }
}

impl Stanza for MucGroupMessage
{
    fn to_xml(&self) -> String
    {
        let (id, origin) = super::outgoing_origin(self.origin_id.as_deref());
        format!(
            "<message type='groupchat' to='{}'{}><body>{}</body>{}</message>",
            super::xml::escape_attribute_value(&self.to),
            id, super::xml::escape_text(&self.body), origin
        )
    }
}

impl MucJoinPresence
{
    pub fn new(room_jid: String, nick: String) -> Self
    {
        Self { room_jid, nick, id: None }
    }

    pub fn with_id(mut self, id: String) -> Self { self.id = Some(id); self }
}

impl Stanza for MucJoinPresence
{
    fn to_xml(&self) -> String
    {
        format!(
            "<presence to='{}/{}'{}><x xmlns='http://jabber.org/protocol/muc'/></presence>",
            super::xml::escape_attribute_value(&self.room_jid),
            super::xml::escape_attribute_value(&self.nick),
            self.id.as_ref().map(|id| format!(" id='{}'", super::xml::escape_attribute_value(id))).unwrap_or_default()
        )
    }
}

pub struct MucLeavePresence
{
    room_jid: String,
    nick: String,
    id: Option<String>,
}

impl MucLeavePresence
{
    pub fn new(room_jid: String, nick: String) -> Self
    {
        Self { room_jid, nick, id: None }
    }

    pub fn with_id(mut self, id: String) -> Self { self.id = Some(id); self }
}

impl Stanza for MucLeavePresence
{
    fn to_xml(&self) -> String
    {
        format!(
            "<presence type='unavailable' to='{}/{}'{}/>",
            super::xml::escape_attribute_value(&self.room_jid),
            super::xml::escape_attribute_value(&self.nick),
            self.id.as_ref().map(|id| format!(" id='{}'", super::xml::escape_attribute_value(id))).unwrap_or_default()
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
    #[serde(rename = "@nick", default)]
    pub nick: Option<String>,
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

#[derive(Debug)]
pub struct MucMessage
{
    pub origin_id: Option<String>,
    pub from: Option<String>,
    pub message_type: Option<String>,
    pub body: Option<String>,
    pub subject: Option<String>,
    pub has_thread: bool,
    pub privacy: crate::RoomPrivacy,
    pub delay: Option<Delay>,
    pub stanza_ids: Vec<StanzaId>,
}

// Validate the whole address before splitting: checking only the room key
// leaves its resource unchecked and can admit an unusable occupant nickname.
fn occupant_address(from: &str) -> Option<(&str, &str)>
{
    crate::xmpp::parse_address(from).ok()?;
    from.split_once('/')
}

impl MucMessage
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        let message = super::message_content::MessageContent::from_xml(xml)?;
        return Ok(Self { origin_id: message.origin_id, from: message.from, message_type: message.message_type, body: message.body,
            subject: message.subject, has_thread: message.has_thread, privacy: message.privacy, delay: message.delay, stanza_ids: super::parse_stanza_ids(xml)? });
    }

    pub fn room_and_nick(&self) -> Option<(&str, &str)>
    {
        occupant_address(self.from.as_deref()?)
    }
    /// XEP-0045 subject changes use groupchat, have no body or thread, and originate from
    /// the room's bare JID or an occupant. Return the room and its new subject.
    pub fn subject_change(&self) -> Option<(&str, &str)>
    {
        if self.message_type.as_deref() != Some("groupchat") || self.body.is_some() || self.has_thread { return None; }
        let subject = self.subject.as_deref()?;
        let from = self.from.as_deref()?;
        crate::xmpp::parse_address(from).ok()?;
        Some((from.split_once('/').map_or(from, |(room, _)| room), subject))
    }

    pub fn stanza_id(&self, room: &str) -> Option<&str>
    {
        return stanza_id_by(&self.stanza_ids, room);
    }
}

pub struct PresenceErrorStanza
{
    pub id: Option<String>,
    pub from: String,
    pub error_type: String,
    pub condition: String,
    pub text: Option<String>,
}

impl PresenceErrorStanza
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        let error = super::message_error::MessageError::from_presence(xml)?;
        let from = error.from.filter(|from| !from.is_empty()).ok_or("Missing @from in presence error")?;
        Ok(Self { id: if error.id.is_empty() { None } else { Some(error.id) }, from, error_type: error.error_type, condition: error.condition, text: error.text })
    }
}

impl MucPresence
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        use quick_xml::{events::{BytesEnd, BytesStart, Event}, name::ResolveResult, NsReader, Writer};
        #[derive(Clone, Copy)]
        enum Node { Root, Show, Status, User, Item, Code, Unknown }
        use Node::*;
        super::xml::element(xml, b"presence", b"jabber:client")?;
        let normalized_xml = crate::stanza::xml::normalize_xml(xml)?;
        let mut reader = NsReader::from_str(&normalized_xml);
        reader.config_mut().expand_empty_elements = true;
        let mut writer = Writer::new(Vec::new());
        let mut stack = Vec::new();
        let mut seen_user = false;
        let mut seen_show = false;
        loop {
            match reader.read_resolved_event().map_err(|e| e.to_string())? {
                (ns, Event::Start(e)) => {
                    let namespace = match &ns { ResolveResult::Bound(ns) => ns.as_ref(), _ => b"" };
                    let local = e.local_name();
                    let node = match (stack.last(), local.as_ref(), namespace) {
                        (None, _, _) => Root,
                        (Some(Root), b"show", b"jabber:client") => {
                            if seen_show { return Err("Duplicate presence show".into()); }
                            seen_show = true;
                            Show
                        }
                        (Some(Root), b"status", b"jabber:client") => Status,
                        (Some(Root), b"x", b"http://jabber.org/protocol/muc#user") => {
                            if seen_user { return Err("Duplicate MUC user payload".into()); }
                            seen_user = true;
                            User
                        }
                        // XEP-0045's muc#user schema permits multiple items,
                        // including occupants connected from several resources.
                        // Keep them all; a later status 110 still confirms entry.
                        (Some(User), b"item", b"http://jabber.org/protocol/muc#user") => Item,
                        (Some(User), b"status", b"http://jabber.org/protocol/muc#user") => Code,
                        (Some(Show | Status | Code), _, _) => return Err("Presence text and status codes cannot contain child elements".into()),
                        _ => Unknown,
                    };
                    if !matches!(node, Unknown) {
                        let name = std::str::from_utf8(local.as_ref()).map_err(|e| e.to_string())?;
                        let mut start = BytesStart::new(name);
                        super::xml::copy_unqualified_attributes(&e, &mut start)?;
                        writer.write_event(Event::Start(start)).map_err(|e| e.to_string())?;
                    }
                    stack.push(node);
                }
                (_, Event::End(e)) => {
                    if !matches!(stack.pop().ok_or("Unexpected presence closing element")?, Unknown) {
                        let local = e.local_name();
                        let name = std::str::from_utf8(local.as_ref()).map_err(|e| e.to_string())?;
                        writer.write_event(Event::End(BytesEnd::new(name))).map_err(|e| e.to_string())?;
                    }
                }
                (_, event @ (Event::Text(_) | Event::CData(_) | Event::GeneralRef(_))) => {
                    if matches!(stack.last(), Some(Code)) { return Err("MUC status code element must be empty".into()); }
                    if matches!(stack.last(), Some(Show | Status)) { writer.write_event(event).map_err(|e| e.to_string())?; }
                }
                (_, Event::Eof) => break,
                _ => {},
            }
        }
        let xml = String::from_utf8(writer.into_inner()).map_err(|e| e.to_string())?;
        return quick_xml::de::from_str(&xml).map_err(|e| e.to_string());
    }

    pub fn room_and_nick(&self) -> Option<(&str, &str)>
    {
        occupant_address(&self.from)
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
    fn topic_changes_require_groupchat_type_and_standalone_subject() {
        for from in ["room@example", "room@example/alice/phone"] {
            for kind in ["", " type='chat'", " type='normal'", " type='headline'", " type='error'", " type='unknown'"] {
                let xml = format!("<message xmlns='jabber:client'{kind} from='{from}'><subject>ordinary subject</subject></message>");
                assert!(MucMessage::from_xml(&xml).unwrap().subject_change().is_none(), "{xml}");
            }
            for payload in ["<subject>topic</subject>", "<subject/>", "<subject>topic</subject><body/>", "<subject>topic</subject><thread/>", "<subject>topic</subject><thread xmlns='extension'/>"] {
                let xml = format!("<message xmlns='jabber:client' type='groupchat' from='{from}'>{payload}</message>");
                let message = MucMessage::from_xml(&xml).unwrap();
                let expected = if payload.contains("<body/>") || payload.contains("<thread/>") { None }
                    else { Some(("room@example", if payload == "<subject/>" { "" } else { "topic" })) };
                assert_eq!(message.subject_change(), expected, "{xml}");
            }
        }
    }

    #[test]
    fn presence_resolves_muc_namespaces_and_ignores_qualified_identity_attributes() {
        let xml = "<c:presence xmlns:c='jabber:client' xmlns:u='http://jabber.org/protocol/muc#user' xmlns:e='extension' from='room@example/alice' e:from='forged'>\
            <e:show>dnd</e:show><c:show>away</c:show><e:status>forged</e:status><c:status>A&amp;B<![CDATA[<C>]]>&#33;</c:status>\
            <e:x><e:item jid='forged'/><e:status code='110'/></e:x>\
            <u:x><e:item jid='forged'/><u:item jid='alice@example' affiliation='member' role='participant' e:jid='forged'/>\
            <e:status code='110'/><u:status code='110' e:code='forged'/></u:x></c:presence>";
        let presence = MucPresence::from_xml(xml).unwrap();
        assert_eq!(presence.room_and_nick(), Some(("room@example", "alice")));
        assert_eq!(presence.show(), Some("away"));
        assert_eq!(presence.status(), Some("A&B<C>!"));
        assert!(presence.is_self_presence());
        assert_eq!(presence.muc_user_x().unwrap().jid(), Some("alice@example"));
        let foreign = xml.replace("http://jabber.org/protocol/muc#user", "foreign");
        let presence = MucPresence::from_xml(&foreign).unwrap();
        assert!(presence.muc_user_x().is_none());
        assert!(!presence.is_self_presence());
        let foreign_code = xml.replace("<u:status code='110' e:code='forged'/>", "");
        assert!(!MucPresence::from_xml(&foreign_code).unwrap().is_self_presence());
    }

    #[test]
    fn multi_session_occupant_presence_keeps_all_items_and_self_confirmation() {
        for self_presence in [false, true] {
            let code = if self_presence { "<u:status code='110'/>" } else { "<e:status code='110'/>" };
            let xml = format!("<c:presence xmlns:c='jabber:client' xmlns:u='http://jabber.org/protocol/muc#user' xmlns:e='extension' from='room@example/nick'>\
                <u:x><u:item affiliation='member' role='participant' jid='person@example/phone'/>\
                <e:item jid='foreign@example'/><u:item affiliation='member' role='participant' jid='person@example/desktop'/>{code}</u:x></c:presence>");
            let presence = MucPresence::from_xml(&xml).expect("MUC user payloads permit multiple items");
            assert_eq!(presence.is_self_presence(), self_presence);
            let payload = presence.muc_user_x().unwrap();
            assert_eq!(payload.items().count(), 2);
            assert_eq!(payload.jid(), Some("person@example/phone"));
            assert!(payload.items().all(|item| item.role.as_deref() == Some("participant")));
        }
    }

    #[test]
    fn malformed_or_ambiguous_muc_presence_is_rejected() {
        for payload in [
            "<x xmlns='http://jabber.org/protocol/muc#user'/><x xmlns='http://jabber.org/protocol/muc#user'/>",
            "<x xmlns='http://jabber.org/protocol/muc#user'><status code='110'><child/></status></x>",
            "<x xmlns='http://jabber.org/protocol/muc#user'><status code='110'>text</status></x>",
            "<x xmlns='http://jabber.org/protocol/muc#user' xmlns:e='extension'><status e:code='110'/></x>",
            "<show><child/></show>",
            "<status><child/></status>",
        ] {
            let xml = format!("<presence xmlns='jabber:client' from='room@example/nick'>{payload}</presence>");
            assert!(MucPresence::from_xml(&xml).is_err(), "{xml}");
        }
        assert!(MucPresence::from_xml("<presence xmlns='foreign' from='room@example/nick'/>").is_err());
        assert!(MucPresence::from_xml("<presence xmlns='jabber:client' xmlns:e='extension' e:from='forged'/>").is_err());
    }

    #[test]
    fn occupant_address_routing_checks_complete_resource_lengths_and_controls() {
        for from in ["".to_string(), "/nick".into(), "room@example/".into(),
            format!("room@example/{}", "a".repeat(1024)),
            format!("room@example/{}", "😀".repeat(256)),
            "room@example/nick&#9;name".into()] {
            let presence = MucPresence::from_xml(&format!("<presence xmlns='jabber:client' from='{from}'/>")).unwrap();
            let message = MucMessage::from_xml(&format!("<message xmlns='jabber:client' type='groupchat' from='{from}'><body>text</body></message>")).unwrap();
            assert!(presence.room_and_nick().is_none(), "invalid presence sender accepted");
            assert!(message.room_and_nick().is_none(), "invalid message sender accepted");
        }
        for nick in ["nick/phone".to_string(), format!("{}abc", "😀".repeat(255)), "with spaces &amp; symbols".into()] {
            let xml = format!("<presence xmlns='jabber:client' from='room@example/{nick}'/>");
            let presence = MucPresence::from_xml(&xml).unwrap();
            let xml = format!("<message xmlns='jabber:client' type='groupchat' from='room@example/{nick}'><body>text</body></message>");
            let message = MucMessage::from_xml(&xml).unwrap();
            let expected = nick.replace("&amp;", "&");
            assert_eq!(presence.room_and_nick(), Some(("room@example", expected.as_str())));
            assert_eq!(message.room_and_nick(), Some(("room@example", expected.as_str())));
        }
    }

    #[test]
    fn parse_presence_error()
    {
        let xml = "<presence xmlns='jabber:client' type='error' to='user@example.com/res' from='room@conference.example.com'>\
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
        let xml = "<presence xmlns='jabber:client' type='error' from='room@conference.example.com'>\
            <error type='auth'>\
            <forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/>\
            </error></presence>";

        let err = PresenceErrorStanza::from_xml(xml).unwrap();
        assert_eq!(err.from, "room@conference.example.com");
        assert_eq!(err.error_type, "auth");
        assert_eq!(err.condition, "forbidden");
        assert!(err.text.is_none());
    }

    #[test]
    fn presence_errors_require_direct_namespaced_conditions_and_decode_text() {
        let valid = "<p:presence xmlns:p='jabber:client' xmlns:e='urn:ietf:params:xml:ns:xmpp-stanzas' xmlns:x='extension' from='room@example/nick' x:from='forged' type='error'><p:error type='cancel' x:type='forged'><e:forbidden/><e:text xml:lang='en'>A&amp;B<![CDATA[<C>]]>&#33;</e:text><e:text xml:lang='sv'>Nej</e:text></p:error></p:presence>";
        let parsed = PresenceErrorStanza::from_xml(valid).unwrap();
        assert_eq!(parsed.from, "room@example/nick");
        assert_eq!(parsed.error_type, "cancel");
        assert_eq!(parsed.condition, "forbidden");
        assert_eq!(parsed.text.as_deref(), Some("A&B<C>!"));
        for bad in [
            valid.replace("urn:ietf:params:xml:ns:xmpp-stanzas", "foreign"),
            valid.replace("<e:forbidden/>", "<x:forbidden/>"),
            valid.replace("<e:forbidden/>", "<e:forbidden/><e:conflict/>"),
            valid.replace("<e:forbidden/>", "<x:wrapper><e:forbidden/></x:wrapper>"),
            valid.replace("type='error'", "type='available'"),
            valid.replace("from='room@example/nick'", ""),
            valid.replace("<p:error type='cancel'", "<p:error xmlns:p='foreign' type='cancel'"),
        ] {
            assert!(PresenceErrorStanza::from_xml(&bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn parse_muc_message_stanza_id()
    {
        let xml = "<message xmlns='jabber:client' type='groupchat' from='room@conf.example/alice'>\
            <body>hi</body>\
            <stanza-id xmlns='urn:xmpp:sid:0' id='arch-7' by='room@conf.example'/>\
            </message>";
        let msg = MucMessage::from_xml(xml).unwrap();
        // Picks the id assigned by the room, ignoring stamps entirely.
        assert_eq!(msg.stanza_id("room@conf.example"), Some("arch-7"));
        // A different archiving entity does not match.
        assert_eq!(msg.stanza_id("someone-else@example"), None);
    }
    #[test]
    fn parse_muc_message_without_stanza_id()
    {
        let xml = "<message xmlns='jabber:client' type='groupchat' from='room@conf.example/alice'><body>hi</body></message>";
        let msg = MucMessage::from_xml(xml).unwrap();
        assert!(msg.stanza_ids.is_empty());
        assert_eq!(msg.stanza_id("room@conf.example"), None);
    }
}
