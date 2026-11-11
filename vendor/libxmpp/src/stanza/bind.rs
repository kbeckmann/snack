use super::Stanza;

pub struct BindRequest
{
    id: String,
    resource: Option<String>,
}

impl BindRequest
{
    pub fn new(id: String, resource: Option<String>) -> Self
    {
        return Self { id, resource };
    }
}

impl Stanza for BindRequest
{
    fn to_xml(&self) -> String
    {
        match &self.resource
        {
            Some(res) => format!(
                "<iq type='set' id='{}'><bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'><resource>{}</resource></bind></iq>",
                super::xml::escape_attribute_value(&self.id),
                super::xml::escape_text(res)
            ),
            None => format!(
                "<iq type='set' id='{}'><bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'/></iq>",
                super::xml::escape_attribute_value(&self.id)
            ),
        }
    }
}

#[derive(Debug)]
pub struct BindResult
{
    pub iq_type: String,
    pub id: String,
    pub bind: Option<BindInner>,
}

#[derive(Debug)]
pub struct BindInner
{
    pub jid: Option<String>,
}

impl BindResult
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        use quick_xml::{events::Event, name::ResolveResult, NsReader};
        const BIND: &[u8] = b"urn:ietf:params:xml:ns:xmpp-bind";
        let attributes = super::xml::element(xml, b"iq", b"jabber:client")?;
        let attribute = |key| attributes.iter().find(|(name, _)| name == key)
            .map(|(_, value)| value.clone()).filter(|value| !value.is_empty()).ok_or_else(|| format!("Missing bind IQ {key}"));
        let mut result = Self { iq_type: attribute("type")?, id: attribute("id")?, bind: None };
        let normalized_xml = crate::stanza::xml::normalize_xml(xml)?;
        let mut reader = NsReader::from_str(&normalized_xml);
        let mut depth = 0usize;
        let mut in_bind = false;
        let mut jid = None::<String>;
        loop {
            match reader.read_resolved_event().map_err(|e| e.to_string())? {
                (ns, event @ (Event::Start(_) | Event::Empty(_))) => {
                    let empty = matches!(event, Event::Empty(_));
                    let e = match event { Event::Start(e) | Event::Empty(e) => e, _ => unreachable!() };
                    let valid_namespace = matches!(ns, ResolveResult::Bound(ns) if ns.as_ref() == BIND);
                    if depth == 1 && e.local_name().as_ref() == b"bind" && valid_namespace {
                        if result.bind.is_some() { return Err("Duplicate bind payload".into()); }
                        result.bind = Some(BindInner { jid: None });
                        in_bind = !empty;
                    } else if depth == 2 && in_bind && e.local_name().as_ref() == b"jid" && valid_namespace {
                        let bind = result.bind.as_mut().unwrap();
                        if bind.jid.is_some() { return Err("Duplicate bound JID".into()); }
                        if empty { bind.jid = Some(String::new()); }
                        else { jid = Some(String::new()); }
                    } else if depth == 3 && jid.is_some() {
                        return Err("Bound JID must contain text only".into());
                    }
                    if !empty { depth += 1; }
                }
                (_, Event::End(_)) => {
                    if depth == 3 {
                        if let Some(jid) = jid.take() { result.bind.as_mut().unwrap().jid = Some(jid); }
                    }
                    if depth == 2 { in_bind = false; }
                    depth -= 1; // The complete XML was validated above.
                }
                (_, Event::Text(e)) => {
                    if let Some(jid) = jid.as_mut() { jid.push_str(&e.decode().map_err(|e| e.to_string())?); }
                }
                (_, Event::CData(e)) => {
                    if let Some(jid) = jid.as_mut() { jid.push_str(&e.decode().map_err(|e| e.to_string())?); }
                }
                (_, Event::GeneralRef(e)) => {
                    if let Some(jid) = jid.as_mut() {
                        let reference = format!("&{};", e.decode().map_err(|e| e.to_string())?);
                        jid.push_str(&quick_xml::escape::unescape(&reference).map_err(|e| e.to_string())?);
                    }
                }
                (_, Event::Eof) => return Ok(result),
                _ => {},
            }
        }
    }

    pub fn bound_jid(self, request_id: &str) -> Result<String, String>
    {
        if self.id != request_id || self.iq_type != "result" { return Err("Bind IQ did not match the successful request".into()); }
        let jid = self.bind.and_then(|bind| bind.jid).ok_or("No namespaced JID in bind result")?;
        if !jid.contains('/') { return Err("Bind result must contain a full JID".into()); }
        crate::xmpp::parse_jid(&jid).map_err(|e| e.to_string())?;
        return Ok(jid);
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    fn result(payload: &str) -> String {
        format!("<iq xmlns='jabber:client' type='result' id='bind_1'>{payload}</iq>")
    }

    #[test]
    fn bind_result_requires_namespaced_payload_and_full_jid() {
        let valid = result("<b:bind xmlns:b='urn:ietf:params:xml:ns:xmpp-bind'><b:jid>me@example/r&amp;&lt;</b:jid></b:bind>");
        assert_eq!(BindResult::from_xml(&valid).unwrap().bound_jid("bind_1").unwrap(), "me@example/r&<");
        assert!(BindResult::from_xml(&valid).unwrap().bound_jid("different-request").is_err());
        for jid in ["", "me@example", "me@example/", "@example/resource", "me@/resource", "me@@example/resource", "me @example/resource", "me@example/a\nb"] {
            let xml = result(&format!("<bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'><jid>{jid}</jid></bind>"));
            assert!(BindResult::from_xml(&xml).unwrap().bound_jid("bind_1").is_err(), "{jid}");
        }
        for payload in [
            "<bind><jid>me@example/resource</jid></bind>",
            "<bind xmlns='unrelated'><jid>me@example/resource</jid></bind>",
            "<bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'><jid xmlns='unrelated'>me@example/resource</jid></bind>",
            "<wrapper><bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'><jid>me@example/resource</jid></bind></wrapper>",
        ] {
            assert!(BindResult::from_xml(&result(payload)).unwrap().bound_jid("bind_1").is_err(), "{payload}");
        }
    }

    #[test]
    fn bound_resource_lengths_are_checked_in_utf8_bytes() {
        for (resource, accepted) in [(format!("{}abc", "🙂".repeat(255)), true), ("🙂".repeat(256), false)] {
            let xml = result(&format!("<bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'><jid>me@example/{resource}</jid></bind>"));
            assert_eq!(BindResult::from_xml(&xml).unwrap().bound_jid("bind_1").is_ok(), accepted);
        }
    }

    #[test]
    fn ambiguous_or_malformed_bind_replies_are_rejected() {
        for payload in [
            "<bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'><jid>me@example/a</jid><jid>me@example/b</jid></bind>",
            "<bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'/><bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'/>",
            "<bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'><jid>me@example/<nested/></jid></bind>",
        ] {
            assert!(BindResult::from_xml(&result(payload)).is_err(), "{payload}");
        }
        assert!(BindResult::from_xml("<iq xmlns='unrelated' type='result' id='bind_1'/>").is_err());
        let error = result("<bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'><jid>me@example/a</jid></bind>").replace("type='result'", "type='error'");
        assert!(BindResult::from_xml(&error).unwrap().bound_jid("bind_1").is_err());
    }
}
