//! Common parsing for live direct and room message content.

use super::delay::Delay;
use std::collections::HashSet;

#[derive(Default)]
pub(crate) struct LocalizedText
{
    languages: HashSet<String>,
    pub value: Option<String>,
}

impl LocalizedText
{
    pub fn insert(&mut self, language: &str, default: &str, text: String) -> Result<(), String>
    {
        if !self.languages.insert(language.to_ascii_lowercase()) {
            return Err("Duplicate message text for the same language".into());
        }
        // Prefer the stanza's default language; otherwise retain the first
        // available translation. Do not drop a valid multilingual message.
        if self.value.is_none() || language.eq_ignore_ascii_case(default) { self.value = Some(text); }
        Ok(())
    }
}

pub(crate) struct MessageContent
{
    pub from: Option<String>,
    pub origin_id: Option<String>,
    pub to: Option<String>,
    pub message_type: Option<String>,
    pub body: Option<String>,
    pub subject: Option<String>,
    pub has_thread: bool,
    pub muc_private: bool,
    pub privacy: crate::RoomPrivacy,
    pub delay: Option<Delay>,
}

impl MessageContent
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        use quick_xml::{events::Event, name::ResolveResult, NsReader};
        enum Node { Root, Body(String), Subject(String), Delay, MucUser, MucStatus, Unknown }
        let attributes = super::xml::element(xml, b"message", b"jabber:client")?;
        let attribute = |key| attributes.iter().find(|(name, _)| name == key).map(|(_, value)| value.clone());
        let default_language = attribute("xml:lang").unwrap_or_default();
        let mut result = Self { origin_id: super::parse_origin_id(xml, false)?, from: attribute("from"), to: attribute("to"), message_type: attribute("type"), body: None, subject: None, has_thread: false, muc_private: false, privacy: Default::default(), delay: None };
        let mut body = LocalizedText::default();
        let mut subject = LocalizedText::default();
        let mut stack = Vec::new();
        let mut text = String::new();
        let normalized_xml = crate::stanza::xml::normalize_xml(xml)?;
        let mut reader = NsReader::from_str(&normalized_xml);
        reader.config_mut().expand_empty_elements = true;
        loop {
            match reader.read_resolved_event().map_err(|e| e.to_string())? {
                (ns, Event::Start(e)) => {
                    let namespace = match &ns { ResolveResult::Bound(ns) => ns.as_ref(), _ => b"" };
                    let mut attributes = Vec::new();
                    for attr in e.attributes() {
                        let attr = attr.map_err(|e| e.to_string())?;
                        attributes.push((attr.key.as_ref().to_vec(), attr.unescape_value().map_err(|e| e.to_string())?.into_owned()));
                    }
                    let attribute = |key: &[u8]| attributes.iter().find(|(name, _)| name == key).map(|(_, value)| value.clone());
                    let language = || attribute(b"xml:lang").unwrap_or_else(|| default_language.clone());
                    let node = match (stack.last(), e.local_name().as_ref(), namespace) {
                        (None, _, _) => Node::Root,
                        (Some(Node::Root), b"body", b"jabber:client") => Node::Body(language()),
                        (Some(Node::Root), b"subject", b"jabber:client") => Node::Subject(language()),
                        (Some(Node::Root), b"thread", b"jabber:client") => { result.has_thread = true; Node::Unknown },
                        (Some(Node::Root), b"x", b"http://jabber.org/protocol/muc#user") => { result.muc_private = true; Node::MucUser },
                        (Some(Node::MucUser), b"status", b"http://jabber.org/protocol/muc#user") => {
                            let code = attribute(b"code");
                            let setting = match code.as_deref() {
                                Some("170") => Some((&mut result.privacy.publicly_logged, true)),
                                Some("171") => Some((&mut result.privacy.publicly_logged, false)),
                                Some("172") => Some((&mut result.privacy.non_anonymous, true)),
                                Some("173") => Some((&mut result.privacy.non_anonymous, false)),
                                _ => None,
                            };
                            if let Some((setting, value)) = setting {
                                if setting.replace(value).is_some_and(|old| old != value) {
                                    return Err("Conflicting room privacy status codes".into());
                                }
                            }
                            Node::MucStatus
                        }
                        (Some(Node::MucStatus), _, _) => return Err("MUC status must be empty".into()),
                        (Some(Node::Root), b"delay", b"urn:xmpp:delay") => {
                            if result.delay.is_some() { return Err("Duplicate message delay".into()); }
                            result.delay = Some(Delay { from: attribute(b"from"), stamp: attribute(b"stamp"), reason: None });
                            Node::Delay
                        }
                        (Some(Node::Body(_) | Node::Subject(_) | Node::Delay), _, _) =>
                            return Err("Message text and delay must contain text only".into()),
                        _ => Node::Unknown,
                    };
                    if matches!(node, Node::Body(_) | Node::Subject(_) | Node::Delay) { text.clear(); }
                    stack.push(node);
                }
                (_, Event::End(_)) => match stack.pop().ok_or("Unexpected message closing element")? {
                    Node::Body(language) => body.insert(&language, &default_language, std::mem::take(&mut text))?,
                    Node::Subject(language) => subject.insert(&language, &default_language, std::mem::take(&mut text))?,
                    Node::Delay => { result.delay.as_mut().unwrap().reason = if text.is_empty() { None } else { Some(std::mem::take(&mut text)) }; }
                    _ => {},
                },
                (_, event @ (Event::Text(_) | Event::CData(_) | Event::GeneralRef(_))) => {
                    if matches!(stack.last(), Some(Node::MucStatus)) { return Err("MUC status must be empty".into()); }
                    if matches!(stack.last(), Some(Node::Body(_) | Node::Subject(_) | Node::Delay)) {
                        match event {
                            Event::Text(e) => text.push_str(&e.decode().map_err(|e| e.to_string())?),
                            Event::CData(e) => text.push_str(&e.decode().map_err(|e| e.to_string())?),
                            Event::GeneralRef(e) => {
                                let reference = format!("&{};", e.decode().map_err(|e| e.to_string())?);
                                text.push_str(&quick_xml::escape::unescape(&reference).map_err(|e| e.to_string())?);
                            }
                            _ => unreachable!(),
                        }
                    }
                }
                (_, Event::Eof) => {
                    result.body = body.value;
                    result.subject = subject.value;
                    return Ok(result);
                }
                _ => {},
            }
        }
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn privacy_status_codes_require_direct_muc_namespace_and_unqualified_code() {
        let xml = "<message xmlns='jabber:client' xmlns:u='http://jabber.org/protocol/muc#user' xmlns:e='extension'><u:x><u:status code='170'/><u:status code='172'/></u:x></message>";
        assert_eq!(MessageContent::from_xml(xml).unwrap().privacy, crate::RoomPrivacy { publicly_logged: Some(true), non_anonymous: Some(true) });
        for payload in [
            "<x xmlns='foreign'><status code='170'/></x>",
            "<wrapper><x xmlns='http://jabber.org/protocol/muc#user'><status code='170'/></x></wrapper>",
            "<x xmlns='http://jabber.org/protocol/muc#user'><status xmlns='foreign' code='170'/></x>",
            "<x xmlns='http://jabber.org/protocol/muc#user' xmlns:e='extension'><status e:code='170'/></x>",
            "<x xmlns='http://jabber.org/protocol/muc#user'><item><status code='170'/></item></x>",
        ] {
            let xml = format!("<message xmlns='jabber:client'>{payload}</message>");
            assert_eq!(MessageContent::from_xml(&xml).unwrap().privacy, crate::RoomPrivacy::default());
        }
        for payload in ["<status code='170'/><status code='171'/>", "<status code='172'/><status code='173'/>", "<status code='170'><child/></status>", "<status code='170'>text</status>"] {
            let xml = format!("<message xmlns='jabber:client'><x xmlns='http://jabber.org/protocol/muc#user'>{payload}</x></message>");
            assert!(MessageContent::from_xml(&xml).is_err(), "{xml}");
        }
    }

    #[test]
    fn messages_ignore_foreign_children_and_qualified_attributes() {
        let xml = "<c:message xmlns:c='jabber:client' xmlns:x='extension' xmlns:d='urn:xmpp:delay' from='room@example/alice' x:from='forged' to='me@example' type='groupchat'>\
            <x:body>forged</x:body><c:body> A&amp;B<![CDATA[<C>]]>&#33; </c:body>\
            <x:subject>forged</x:subject><c:subject>real subject</c:subject>\
            <x:delay stamp='forged'/><d:delay stamp='real-time' x:stamp='forged'>reason &amp; text</d:delay>\
            <x:wrapper><c:body>hidden</c:body></x:wrapper></c:message>";
        let chat = super::super::chat::IncomingChatMessage::from_xml(xml).unwrap();
        let muc = super::super::muc::MucMessage::from_xml(xml).unwrap();
        assert_eq!(chat.from.as_deref(), Some("room@example/alice"));
        assert_eq!(chat.to.as_deref(), Some("me@example"));
        assert_eq!(chat.body.as_deref(), Some(" A&B<C>! "));
        assert_eq!(muc.body, chat.body);
        assert_eq!(muc.subject.as_deref(), Some("real subject"));
        assert_eq!(muc.delay.as_ref().unwrap().stamp.as_deref(), Some("real-time"));
        assert_eq!(muc.delay.unwrap().reason.as_deref(), Some("reason & text"));
        let only_extensions = "<message xmlns='jabber:client' xmlns:x='extension' x:from='forged'><x:body>forged</x:body><x:subject>forged</x:subject><x:delay/></message>";
        let message = MessageContent::from_xml(only_extensions).unwrap();
        assert!(message.from.is_none());
        assert!(message.body.is_none());
        assert!(message.subject.is_none());
        assert!(message.delay.is_none());
    }

    #[test]
    fn translations_prefer_the_stanza_language_and_preserve_empty_bodies() {
        for (language, payload, expected) in [
            ("en", "<body xml:lang='sv'>hej</body><body>Hello</body>", "Hello"),
            ("en", "<body>Hello</body><body xml:lang='sv'>hej</body>", "Hello"),
            ("en", "<body xml:lang='sv'>hej</body><body xml:lang='fr'>salut</body>", "hej"),
            ("en", "<body xml:lang='sv'>hej</body><body/>", ""),
            ("en", "<body xml:lang=''>default</body><body>English</body>", "English"),
            ("EN", "<body xml:lang='sv'>hej</body><body xml:lang='en'>English</body>", "English"),
        ] {
            let xml = format!("<message xmlns='jabber:client' xml:lang='{language}'>{payload}<subject xml:lang='sv'>ämne</subject><subject>Subject</subject></message>");
            let message = MessageContent::from_xml(&xml).unwrap();
            assert_eq!(message.body.as_deref(), Some(expected));
            assert_eq!(message.subject.as_deref(), Some("Subject"));
        }
    }

    #[test]
    fn malformed_message_text_and_ambiguous_translations_are_rejected() {
        for payload in [
            "<body>one</body><body>two</body>",
            "<body xml:lang='en'>one</body><body xml:lang='EN'>two</body>",
            "<subject>one</subject><subject>two</subject>",
            "<body>one<nested/>two</body>",
            "<subject><nested/></subject>",
            "<delay xmlns='urn:xmpp:delay'><nested/></delay>",
            "<delay xmlns='urn:xmpp:delay'/><delay xmlns='urn:xmpp:delay'/>",
        ] {
            let xml = format!("<message xmlns='jabber:client' xml:lang='en'>{payload}</message>");
            assert!(MessageContent::from_xml(&xml).is_err(), "{xml}");
        }
        for xml in ["<message xmlns='foreign'><body>bad</body></message>", "<message xmlns='jabber:client'><body>unfinished", "<message xmlns='jabber:client'/><message xmlns='jabber:client'/>"] {
            assert!(MessageContent::from_xml(xml).is_err());
        }
    }
}
