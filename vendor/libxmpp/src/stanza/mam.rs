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
        use quick_xml::escape::escape;

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
            Some(t) => format!(" to='{}'", escape(t)),
            None => String::new(),
        };

        return format!(
            "<iq type='set' id='{}'{}>\
                <query xmlns='urn:xmpp:mam:2' queryid='{}'>\
                    <x xmlns='jabber:x:data' type='submit'>{}</x>\
                    {}\
                </query>\
             </iq>",
            self.id, to_attr, self.id, fields, rsm,
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
        return quick_xml::de::from_str(xml).map_err(|e| e.to_string());
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
        return quick_xml::de::from_str(xml).map_err(|e| e.to_string());
    }
}

impl MamFinStanza
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        return quick_xml::de::from_str(xml).map_err(|e| e.to_string());
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
        let xml = "<message id='aaa' to='me@example'>\
            <result xmlns='urn:xmpp:mam:2' queryid='q1' id='arch-7'>\
                <forwarded xmlns='urn:xmpp:forward:0'>\
                    <delay xmlns='urn:xmpp:delay' stamp='2025-01-02T03:04:05Z'/>\
                    <message type='groupchat' from='room@conf.example/alice'>\
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
        let xml = "<message><result xmlns='urn:xmpp:mam:2' queryid='q2' id='arch-9'>\
            <forwarded xmlns='urn:xmpp:forward:0'>\
                <delay xmlns='urn:xmpp:delay' stamp='2025-01-02T03:04:05Z'/>\
                <message type='chat' from='bob@example/phone' to='me@example'>\
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
        let xml = "<iq type='result' id='q1'>\
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
        let xml = "<iq type='result' id='q9'>\
            <fin xmlns='urn:xmpp:mam:2' complete='true'>\
                <set xmlns='http://jabber.org/protocol/rsm'><count>3</count></set>\
            </fin></iq>";

        let f = MamFinStanza::from_xml(xml).unwrap();
        assert!(f.is_complete());
        assert_eq!(f.first_id(), None);
    }
}
