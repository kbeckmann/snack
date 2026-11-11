// Some parsed fields (e.g. the result `id`, `to`, `count`) are retained for
// protocol completeness even though the high-level client doesn't read them.
#![allow(dead_code)]

//! XEP-0313 Message Archive Management: query the server-side archive and
//! parse the `<result>` messages and the terminating `<fin>` IQ it returns.
//!
//! Two archives are addressed the same way: a MUC room archive (IQ sent `to`
//! the room JID) and the user's own archive for one-to-one chats (no `to`, with
//! a `with` filter naming the contact). Paging is done backwards through time
//! via RSM: an empty `<before/>` requests the most recent page within the
//! filter, and subsequent pages pass the previous page's first archive id.

use super::Stanza;
use serde::Deserialize;

/// Project only the protocol elements into the serde model. The XML
/// deserializer matches local names, so feeding it the original document would
/// allow foreign extension children and attributes to impersonate MAM data.
fn archive_payload(xml: &str, fin: bool) -> Result<String, String>
{
    use quick_xml::{events::{BytesEnd, BytesStart, BytesText, Event}, name::ResolveResult, NsReader, Writer};
    #[derive(Clone, Copy)]
    enum Node { Root, Result, Forwarded, Delay, Message, Body, Fin, Set, First, Last, Count, Unknown }
    use Node::*;
    let root = if fin { b"iq".as_slice() } else { b"message".as_slice() };
    super::xml::element(xml, root, b"jabber:client")?;
    let normalized_xml = crate::stanza::xml::normalize_xml(xml)?;
    let mut reader = NsReader::from_str(&normalized_xml);
    reader.config_mut().expand_empty_elements = true;
    let mut writer = Writer::new(Vec::new());
    let mut stack = Vec::new();
    let mut languages = Vec::<String>::new();
    let mut message_language = String::new();
    let mut body = super::message_content::LocalizedText::default();
    let mut body_text = String::new();
    loop {
        match reader.read_resolved_event().map_err(|e| e.to_string())? {
            (ns, Event::Start(e)) => {
                let namespace = match &ns { ResolveResult::Bound(ns) => ns.as_ref(), _ => b"" };
                let local = e.local_name();
                let node = match (stack.last(), local.as_ref(), namespace) {
                    (None, _, _) => Root,
                    (Some(Root), b"result", b"urn:xmpp:mam:2") if !fin => Result,
                    (Some(Root), b"fin", b"urn:xmpp:mam:2") if fin => Fin,
                    (Some(Result), b"forwarded", b"urn:xmpp:forward:0") => Forwarded,
                    (Some(Forwarded), b"delay", b"urn:xmpp:delay") => Delay,
                    (Some(Forwarded), b"message", b"jabber:client") => Message,
                    (Some(Message), b"body", b"jabber:client") => Body,
                    (Some(Fin), b"set", b"http://jabber.org/protocol/rsm") => Set,
                    (Some(Set), b"first", b"http://jabber.org/protocol/rsm") => First,
                    (Some(Set), b"last", b"http://jabber.org/protocol/rsm") => Last,
                    (Some(Set), b"count", b"http://jabber.org/protocol/rsm") => Count,
                    (Some(Body | First | Last | Count | Delay), _, _) =>
                        return Err("Archive text and delay elements cannot contain child elements".into()),
                    _ => Unknown,
                };
                let mut language = languages.last().cloned().unwrap_or_default();
                for attr in e.attributes() {
                    let attr = attr.map_err(|e| e.to_string())?;
                    if attr.key.as_ref() == b"xml:lang" { language = attr.unescape_value().map_err(|e| e.to_string())?.into_owned(); }
                }
                if matches!(node, Message) {
                    message_language = language.clone();
                    body = Default::default();
                }
                if matches!(node, Body) { body_text.clear(); }
                languages.push(language);
                if !matches!(node, Unknown | Body) {
                    let name = std::str::from_utf8(local.as_ref()).map_err(|e| e.to_string())?;
                    let mut start = BytesStart::new(name);
                    super::xml::copy_unqualified_attributes(&e, &mut start)?;
                    writer.write_event(Event::Start(start)).map_err(|e| e.to_string())?;
                }
                stack.push(node);
            }
            (_, Event::End(e)) => {
                let node = stack.pop().ok_or("Unexpected archive closing element")?;
                let language = languages.pop().ok_or("Missing archive language scope")?;
                if matches!(node, Body) { body.insert(&language, &message_language, std::mem::take(&mut body_text))?; }
                if matches!(node, Message) {
                    if let Some(value) = body.value.take() {
                        writer.write_event(Event::Start(BytesStart::new("body"))).map_err(|e| e.to_string())?;
                        writer.write_event(Event::Text(BytesText::from_escaped(super::xml::escape_text(&value)))).map_err(|e| e.to_string())?;
                        writer.write_event(Event::End(BytesEnd::new("body"))).map_err(|e| e.to_string())?;
                    }
                }
                if !matches!(node, Unknown | Body) {
                    let local = e.local_name();
                    let name = std::str::from_utf8(local.as_ref()).map_err(|e| e.to_string())?;
                    writer.write_event(Event::End(BytesEnd::new(name))).map_err(|e| e.to_string())?;
                }
            }
            (_, event @ (Event::Text(_) | Event::CData(_) | Event::GeneralRef(_))) => {
                if matches!(stack.last(), Some(Body)) {
                    match event {
                        Event::Text(e) => body_text.push_str(&e.decode().map_err(|e| e.to_string())?),
                        Event::CData(e) => body_text.push_str(&e.decode().map_err(|e| e.to_string())?),
                        Event::GeneralRef(e) => {
                            let reference = format!("&{};", e.decode().map_err(|e| e.to_string())?);
                            body_text.push_str(&quick_xml::escape::unescape(&reference).map_err(|e| e.to_string())?);
                        }
                        _ => unreachable!(),
                    }
                } else if matches!(stack.last(), Some(First | Last | Count)) {
                    writer.write_event(event).map_err(|e| e.to_string())?;
                }
            }
            (_, Event::Eof) => return String::from_utf8(writer.into_inner()).map_err(|e| e.to_string()),
            _ => {},
        }
    }
}

/// Whether a stanza has the named direct child in the MAM namespace.
/// Resolve namespaces rather than searching message bodies for protocol text.
pub fn has_payload(xml: &str, local_name: &[u8]) -> bool
{
    use quick_xml::{events::Event, name::ResolveResult, NsReader};
    let Ok(normalized_xml) = crate::stanza::xml::normalize_xml(xml) else { return false; };
    let mut reader = NsReader::from_str(&normalized_xml);
    let mut depth = 0usize;
    loop
    {
        match reader.read_resolved_event()
        {
            Ok((ns, Event::Start(e))) =>
            {
                if depth == 1 && e.local_name().as_ref() == local_name
                    && matches!(ns, ResolveResult::Bound(ns) if ns.as_ref() == b"urn:xmpp:mam:2")
                { return true; }
                depth += 1;
            }
            Ok((ns, Event::Empty(e))) =>
            {
                if depth == 1 && e.local_name().as_ref() == local_name
                    && matches!(ns, ResolveResult::Bound(ns) if ns.as_ref() == b"urn:xmpp:mam:2")
                { return true; }
            }
            Ok((_, Event::End(_))) => depth = depth.saturating_sub(1),
            Ok((_, Event::Eof)) | Err(_) => return false,
            _ => {}
        }
    }
}

/// An outgoing MAM query IQ (`type='set'`).
///
/// Two paging directions are supported. Backward (`forward = false`) walks the
/// archive towards the past: it sets the `end` time bound and pages with RSM
/// `<before>` (an empty `<before/>` requests the most recent page). Forward
/// (`forward = true`) walks towards the present from a `start` time bound,
/// paging with RSM `<after>` — used to catch up history missed while offline.
pub struct MamQuery
{
    /// IQ id; we reuse it as the `queryid` so results and the `<fin>` correlate.
    pub id: String,
    /// Target archive: the room JID for a MUC archive, `None` for the user's
    /// own archive.
    pub to: Option<String>,
    /// `with` form filter — the contact JID for a one-to-one archive.
    pub with: Option<String>,
    /// `start` form filter (RFC3339) — only messages at/after this.
    pub start: Option<String>,
    /// `end` form filter (RFC3339) — only messages at/before this.
    pub end: Option<String>,
    /// Paging direction (see the type docs).
    pub forward: bool,
    /// RSM cursor: an `<after>` id when `forward`, otherwise a `<before>` id.
    /// `None` requests the first page in the chosen direction.
    pub cursor: Option<String>,
    /// RSM `<max>` page size.
    pub max: u32,
}

impl Stanza for MamQuery
{
    fn to_xml(&self) -> String
    {
        use super::xml::escape_text as escape;

        let mut fields = String::from(
            "<field var='FORM_TYPE' type='hidden'><value>urn:xmpp:mam:2</value></field>",
        );
        if let Some(with) = &self.with
        {
            fields.push_str(&format!("<field var='with'><value>{}</value></field>", escape(with)));
        }
        if let Some(start) = &self.start
        {
            fields.push_str(&format!("<field var='start'><value>{}</value></field>", escape(start)));
        }
        if let Some(end) = &self.end
        {
            fields.push_str(&format!("<field var='end'><value>{}</value></field>", escape(end)));
        }

        let paging = if self.forward
        {
            // Forward: an `<after>` cursor continues towards the present; no
            // cursor on the first page just starts from `start`.
            match &self.cursor
            {
                Some(c) => format!("<after>{}</after>", escape(c)),
                None => String::new(),
            }
        }
        else
        {
            // Backward: an empty `<before/>` requests the most recent page.
            match &self.cursor
            {
                Some(c) => format!("<before>{}</before>", escape(c)),
                None => "<before/>".to_string(),
            }
        };
        let rsm = format!(
            "<set xmlns='http://jabber.org/protocol/rsm'><max>{}</max>{}</set>",
            self.max, paging,
        );

        let to_attr = match &self.to
        {
            Some(t) => format!(" to='{}'", super::xml::escape_attribute_value(t)),
            None => String::new(),
        };

        return format!(
            "<iq type='set' id='{}'{}>\
                <query xmlns='urn:xmpp:mam:2' queryid='{}'>\
                    <x xmlns='jabber:x:data' type='submit'>{}</x>\
                    {}\
                </query>\
             </iq>",
            super::xml::escape_attribute_value(&self.id), to_attr, super::xml::escape_attribute_value(&self.id), fields, rsm,
        );
    }
}

// ----- Inbound: a single archived message -----------------------------------

#[derive(Deserialize, Debug)]
#[serde(rename = "message")]
pub struct MamResultStanza
{
    pub result: MamResult,
}

#[derive(Deserialize, Debug)]
pub struct MamResult
{
    #[serde(rename = "@queryid", default)]
    pub queryid: Option<String>,
    #[serde(rename = "@id", default)]
    pub id: Option<String>,
    pub forwarded: Forwarded,
}

#[derive(Deserialize, Debug)]
pub struct Forwarded
{
    #[serde(default)]
    pub delay: Option<ForwardedDelay>,
    pub message: ForwardedMessage,
}

#[derive(Deserialize, Debug)]
pub struct ForwardedDelay
{
    #[serde(rename = "@stamp", default)]
    pub stamp: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct ForwardedMessage
{
    #[serde(skip)]
    pub origin_id: Option<String>,
    #[serde(rename = "@from", default)]
    pub from: Option<String>,
    #[serde(rename = "@to", default)]
    pub to: Option<String>,
    #[serde(rename = "@type", default)]
    pub message_type: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
}

impl MamResultStanza
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        let mut parsed: Self = quick_xml::de::from_str(&archive_payload(xml, false)?).map_err(|error| error.to_string())?;
        parsed.result.forwarded.message.origin_id = super::parse_origin_id(xml, true)?;
        Ok(parsed)
    }
}

// ----- Inbound: the terminating <fin> IQ ------------------------------------

#[derive(Deserialize, Debug)]
#[serde(rename = "iq")]
pub struct MamFinStanza
{
    #[serde(rename = "@id", default)]
    pub id: Option<String>,
    pub fin: MamFin,
}

#[derive(Deserialize, Debug)]
pub struct MamFin
{
    #[serde(rename = "@complete", default)]
    pub complete: Option<String>,
    #[serde(default)]
    pub set: Option<RsmSet>,
}

#[derive(Deserialize, Debug)]
pub struct RsmSet
{
    #[serde(default)]
    pub first: Option<RsmFirst>,
    #[serde(default)]
    pub last: Option<String>,
    #[serde(default)]
    pub count: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct RsmFirst
{
    // `<first>` also carries an `index` attribute, but we only need its text
    // (the archive id used as the next page's cursor).
    #[serde(rename = "$text", default)]
    pub value: Option<String>,
}

/// Minimal `<iq>` identity used to recognise an error response to a MAM query
/// (which carries no mam namespace) by its echoed id.
#[derive(Deserialize, Debug)]
#[serde(rename = "iq")]
pub struct IqIdentity
{
    #[serde(rename = "@id", default)]
    pub id: Option<String>,
    #[serde(rename = "@type", default)]
    pub iq_type: Option<String>,
}

impl IqIdentity
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        let normalized_xml = super::xml::normalize_xml(xml)?;
        return quick_xml::de::from_str(&normalized_xml).map_err(|e| e.to_string());
    }
}

impl MamFinStanza
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        return quick_xml::de::from_str(&archive_payload(xml, true)?).map_err(|e| e.to_string());
    }

    pub fn is_complete(&self) -> bool
    {
        return matches!(self.fin.complete.as_deref(), Some("true") | Some("1"));
    }

    /// The archive id of the first message in the returned page — the cursor to
    /// pass as `<before>` for the next (older) page.
    pub fn first_id(&self) -> Option<String>
    {
        return self.fin.set.as_ref().and_then(|s| s.first.as_ref()).and_then(|f| f.value.clone());
    }

    /// The archive id of the last message in the returned page — the cursor to
    /// pass as `<after>` for the next (newer) page when catching up.
    pub fn last_id(&self) -> Option<String>
    {
        return self.fin.set.as_ref().and_then(|s| s.last.clone());
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn builds_backward_muc_query_with_empty_before()
    {
        let q = MamQuery
        {
            id: "q1".to_string(),
            to: Some("room@conf.example".to_string()),
            with: None,
            start: None,
            end: Some("2025-01-01T00:00:00Z".to_string()),
            forward: false,
            cursor: None,
            max: 50,
        };
        let xml = q.to_xml();
        assert!(xml.contains("to='room@conf.example'"));
        assert!(xml.contains("queryid='q1'"));
        assert!(xml.contains("<field var='end'><value>2025-01-01T00:00:00Z</value></field>"));
        assert!(xml.contains("<before/>"));
        assert!(xml.contains("<max>50</max>"));
        assert!(!xml.contains("var='with'"));
        assert!(!xml.contains("<after"));
    }

    #[test]
    fn builds_backward_user_query_with_cursor()
    {
        let q = MamQuery
        {
            id: "q2".to_string(),
            to: None,
            with: Some("bob@example".to_string()),
            start: None,
            end: None,
            forward: false,
            cursor: Some("cursor-id".to_string()),
            max: 30,
        };
        let xml = q.to_xml();
        assert!(!xml.contains(" to='"));
        assert!(xml.contains("<field var='with'><value>bob@example</value></field>"));
        assert!(xml.contains("<before>cursor-id</before>"));
    }

    #[test]
    fn builds_forward_catchup_query()
    {
        // First catch-up page: a start bound, no RSM cursor, no <before>.
        let first = MamQuery
        {
            id: "f1".to_string(),
            to: Some("room@conf.example".to_string()),
            with: None,
            start: Some("2025-01-01T00:00:00Z".to_string()),
            end: None,
            forward: true,
            cursor: None,
            max: 50,
        };
        let xml = first.to_xml();
        assert!(xml.contains("<field var='start'><value>2025-01-01T00:00:00Z</value></field>"));
        assert!(!xml.contains("<before"));
        assert!(!xml.contains("<after"));

        // Subsequent page: an <after> cursor continues towards the present.
        let next = MamQuery
        {
            id: "f2".to_string(),
            to: Some("room@conf.example".to_string()),
            with: None,
            start: None,
            end: None,
            forward: true,
            cursor: Some("arch-50".to_string()),
            max: 50,
        };
        let xml = next.to_xml();
        assert!(xml.contains("<after>arch-50</after>"));
        assert!(!xml.contains("<before"));
    }

    #[test]
    fn parses_muc_archive_result()
    {
        let xml = "<message xmlns='jabber:client' id='aaa' to='me@example'>\
            <result xmlns='urn:xmpp:mam:2' queryid='q1' id='arch-7'>\
                <forwarded xmlns='urn:xmpp:forward:0'>\
                    <delay xmlns='urn:xmpp:delay' stamp='2025-01-02T03:04:05Z'/>\
                    <message xmlns='jabber:client' type='groupchat' from='room@conf.example/alice'>\
                        <body>hello world</body>\
                    </message>\
                </forwarded>\
            </result>\
        </message>";

        let r = MamResultStanza::from_xml(xml).unwrap();
        assert_eq!(r.result.queryid.as_deref(), Some("q1"));
        assert_eq!(r.result.id.as_deref(), Some("arch-7"));
        assert_eq!(r.result.forwarded.delay.unwrap().stamp.as_deref(), Some("2025-01-02T03:04:05Z"));
        assert_eq!(r.result.forwarded.message.from.as_deref(), Some("room@conf.example/alice"));
        assert_eq!(r.result.forwarded.message.message_type.as_deref(), Some("groupchat"));
        assert_eq!(r.result.forwarded.message.body.as_deref(), Some("hello world"));
    }

    #[test]
    fn parses_direct_archive_result()
    {
        let xml = "<message xmlns='jabber:client'><result xmlns='urn:xmpp:mam:2' queryid='q2' id='arch-9'>\
            <forwarded xmlns='urn:xmpp:forward:0'>\
                <delay xmlns='urn:xmpp:delay' stamp='2025-01-02T03:04:05Z'/>\
                <message xmlns='jabber:client' type='chat' from='bob@example/phone' to='me@example'>\
                    <body>hi there</body>\
                </message>\
            </forwarded></result></message>";

        let r = MamResultStanza::from_xml(xml).unwrap();
        assert_eq!(r.result.forwarded.message.from.as_deref(), Some("bob@example/phone"));
        assert_eq!(r.result.forwarded.message.to.as_deref(), Some("me@example"));
        assert_eq!(r.result.forwarded.message.body.as_deref(), Some("hi there"));
    }

    #[test]
    fn parses_fin_incomplete_with_cursor()
    {
        let xml = "<iq xmlns='jabber:client' type='result' id='q1'>\
            <fin xmlns='urn:xmpp:mam:2'>\
                <set xmlns='http://jabber.org/protocol/rsm'>\
                    <first index='0'>arch-1</first>\
                    <last>arch-50</last>\
                    <count>123</count>\
                </set>\
            </fin></iq>";

        let f = MamFinStanza::from_xml(xml).unwrap();
        assert_eq!(f.id.as_deref(), Some("q1"));
        assert!(!f.is_complete());
        assert_eq!(f.first_id().as_deref(), Some("arch-1"));
        assert_eq!(f.last_id().as_deref(), Some("arch-50"));
    }

    #[test]
    fn parses_fin_complete()
    {
        let xml = "<iq xmlns='jabber:client' type='result' id='q9'>\
            <fin xmlns='urn:xmpp:mam:2' complete='true'>\
                <set xmlns='http://jabber.org/protocol/rsm'><count>3</count></set>\
            </fin></iq>";

        let f = MamFinStanza::from_xml(xml).unwrap();
        assert!(f.is_complete());
        assert_eq!(f.first_id(), None);
    }
    #[test]
    fn dispatch_requires_a_direct_child_in_the_mam_namespace()
    {
        assert!(has_payload("<message><result xmlns='urn:xmpp:mam:2'/></message>", b"result"));
        assert!(has_payload("<message xmlns:m='urn:xmpp:mam:2'><m:result/></message>", b"result"));
        assert!(!has_payload("<message><result xmlns='other'/></message>", b"result"));
        assert!(!has_payload("<message><body>urn:xmpp:mam:2</body></message>", b"result"));
        assert!(!has_payload("<message><wrapper><result xmlns='urn:xmpp:mam:2'/></wrapper></message>", b"result"));
    }

    #[test]
    fn archive_fields_use_resolved_namespaces_and_unqualified_attributes() {
        let xml = "<c:message xmlns:c='jabber:client' xmlns:m='urn:xmpp:mam:2' xmlns:f='urn:xmpp:forward:0' xmlns:d='urn:xmpp:delay' xmlns:x='extension'>\
            <x:result queryid='forged'/><m:result queryid='real' id='archive' x:queryid='forged'>\
            <x:forwarded/><f:forwarded><x:delay stamp='forged'/><d:delay stamp='real-time' x:stamp='forged'/>\
            <x:message from='forged'/><c:message from='alice@example' x:from='forged'>\
            <x:body>forged</x:body><c:body>A&amp;B<![CDATA[<C>]]>&#33;</c:body>\
            <x:wrapper><c:body>hidden</c:body></x:wrapper></c:message></f:forwarded></m:result></c:message>";
        let r = MamResultStanza::from_xml(xml).unwrap();
        assert_eq!(r.result.queryid.as_deref(), Some("real"));
        assert_eq!(r.result.forwarded.delay.unwrap().stamp.as_deref(), Some("real-time"));
        assert_eq!(r.result.forwarded.message.from.as_deref(), Some("alice@example"));
        assert_eq!(r.result.forwarded.message.body.as_deref(), Some("A&B<C>!"));
        for (old, new) in [
            ("urn:xmpp:mam:2", "foreign-mam"),
            ("urn:xmpp:forward:0", "foreign-forward"),
            ("<c:message from=", "<c:message xmlns:c='foreign' from="),
        ] {
            let bad = xml.replace(old, new);
            assert!(MamResultStanza::from_xml(&bad).is_err());
        }
        let bad = xml.replace("<c:body>A", "<c:body><x:nested/>A");
        assert!(MamResultStanza::from_xml(&bad).is_err());
        let foreign_body = xml.replace("<c:body>A&amp;B<![CDATA[<C>]]>&#33;</c:body>", "");
        assert!(MamResultStanza::from_xml(&foreign_body).unwrap().result.forwarded.message.body.is_none());
    }

    #[test]
    fn archive_cursors_ignore_extensions_and_reject_ambiguous_structure() {
        let xml = "<c:iq xmlns:c='jabber:client' xmlns:m='urn:xmpp:mam:2' xmlns:r='http://jabber.org/protocol/rsm' xmlns:x='extension' id='q' x:id='forged'>\
            <m:fin x:complete='true'><x:set><x:first>forged</x:first></x:set>\
            <r:set><x:first>forged</x:first><r:first index='0'>a&amp;b</r:first>\
            <r:last><![CDATA[last<id>]]></r:last><r:count>2</r:count></r:set></m:fin></c:iq>";
        let f = MamFinStanza::from_xml(xml).unwrap();
        assert_eq!(f.id.as_deref(), Some("q"));
        assert!(!f.is_complete());
        assert_eq!(f.first_id().as_deref(), Some("a&b"));
        assert_eq!(f.last_id().as_deref(), Some("last<id>"));
        let foreign = xml.replace("http://jabber.org/protocol/rsm", "foreign");
        let f = MamFinStanza::from_xml(&foreign).unwrap();
        assert!(f.first_id().is_none());
        assert!(f.last_id().is_none());
        for bad in [
            xml.replace("<r:first index='0'>", "<r:first index='0'><x:nested/>"),
            xml.replace("</r:set>", "<r:first>duplicate</r:first></r:set>"),
            xml.replace("</m:fin>", "</m:fin><m:fin/>"),
            xml.replace("urn:xmpp:mam:2", "foreign"),
        ] {
            assert!(MamFinStanza::from_xml(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn archived_translations_inherit_language_through_the_forwarding_envelope() {
        let xml = "<message xmlns='jabber:client' xml:lang='en'><result xmlns='urn:xmpp:mam:2' queryid='q'><forwarded xmlns='urn:xmpp:forward:0' xml:lang='sv'><message xmlns='jabber:client'><body xml:lang='en'>Hello</body><body>hej &amp; välkommen</body></message></forwarded></result></message>";
        assert_eq!(MamResultStanza::from_xml(xml).unwrap().result.forwarded.message.body.as_deref(), Some("hej & välkommen"));
        let overridden = xml.replace("<message xmlns='jabber:client'><body", "<message xmlns='jabber:client' xml:lang='en'><body");
        assert!(MamResultStanza::from_xml(&overridden).is_err()); // Both bodies now have English as their effective language.
        let duplicate = xml.replace("<body>hej", "<body xml:lang='EN'>hej");
        assert!(MamResultStanza::from_xml(&duplicate).is_err());
    }

}
