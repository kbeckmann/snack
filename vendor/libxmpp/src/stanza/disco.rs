use super::Stanza;
use crate::{DiscoInfo, DiscoItem};
use serde::Deserialize;

pub(crate) const DISCO_INFO_NS: &str = "http://jabber.org/protocol/disco#info";
pub(crate) const DISCO_ITEMS_NS: &str = "http://jabber.org/protocol/disco#items";

/// An outgoing XEP-0030 service discovery **info** query.
///
/// Serialises to
/// `<iq type='get' to='...'><query xmlns='http://jabber.org/protocol/disco#info'/></iq>`,
/// asking the target entity for the identities and features it advertises. When
/// `to` is `None` the query targets the user's own server. An optional `node`
/// scopes the query to a particular node of the entity (e.g. an ad-hoc command).
pub struct DiscoInfoRequest
{
    id: String,
    to: Option<String>,
    node: Option<String>,
}

impl DiscoInfoRequest
{
    pub fn new(id: String, to: Option<String>, node: Option<String>) -> Self
    {
        return Self { id, to, node };
    }
}

impl Stanza for DiscoInfoRequest
{
    fn to_xml(&self) -> String
    {
        return format!(
            "<iq type='get'{} id='{}'>{}</iq>",
            to_attr(&self.to),
            super::xml::escape_attribute_value(&self.id),
            query_element(DISCO_INFO_NS, &self.node)
        );
    }
}

/// An outgoing XEP-0030 service discovery **items** query.
///
/// Serialises to
/// `<iq type='get' to='...'><query xmlns='http://jabber.org/protocol/disco#items'/></iq>`,
/// asking the target entity to list the items it hosts (e.g. the rooms on a
/// conference service). When `to` is `None` the query targets the user's own
/// server. An optional `node` scopes the query to a particular node.
pub struct DiscoItemsRequest
{
    id: String,
    to: Option<String>,
    node: Option<String>,
}

impl DiscoItemsRequest
{
    pub fn new(id: String, to: Option<String>, node: Option<String>) -> Self
    {
        return Self { id, to, node };
    }
}

impl Stanza for DiscoItemsRequest
{
    fn to_xml(&self) -> String
    {
        return format!(
            "<iq type='get'{} id='{}'>{}</iq>",
            to_attr(&self.to),
            super::xml::escape_attribute_value(&self.id),
            query_element(DISCO_ITEMS_NS, &self.node)
        );
    }
}

fn to_attr(to: &Option<String>) -> String
{
    return match to
    {
        Some(to) => format!(" to='{}'", super::xml::escape_attribute_value(to)),
        None => String::new(),
    }
}

fn query_element(ns: &str, node: &Option<String>) -> String
{
    return match node
    {
        Some(node) => format!(
            "<query xmlns='{}' node='{}'/>",
            ns,
            super::xml::escape_attribute_value(node)
        ),
        None => format!("<query xmlns='{}'/>", ns),
    }
}

/// Parsed reply to a [`DiscoInfoRequest`].
///
/// On a successful query `iq_type` is `result` and `query` holds the advertised
/// identities and features. On failure `iq_type` is `error` and `query` is
/// absent.
#[derive(Deserialize, Debug)]
#[serde(rename = "iq")]
pub struct DiscoInfoResult
{
    #[serde(rename = "@type")]
    pub iq_type: String,
    #[serde(rename = "@from", default)]
    pub from: Option<String>,
    #[serde(default)]
    pub query: Option<DiscoInfoQuery>,
}

#[derive(Deserialize, Debug, Default)]
pub struct DiscoInfoQuery
{
    #[serde(rename = "@node", default)]
    pub node: Option<String>,
    #[serde(rename = "identity", default)]
    pub identities: Vec<Identity>,
    #[serde(rename = "feature", default)]
    pub features: Vec<Feature>,
}

#[derive(Deserialize, Debug)]
pub struct Identity
{
    #[serde(rename = "@category")]
    pub category: String,
    #[serde(rename = "@type")]
    pub kind: String,
    #[serde(rename = "@name", default)]
    pub name: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct Feature
{
    #[serde(rename = "@var")]
    pub var: String,
}

impl DiscoInfoResult
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        return quick_xml::de::from_str(&project_disco(xml, Some(DISCO_INFO_NS))?).map_err(|e| e.to_string());
    }
}

/// Parsed reply to a [`DiscoItemsRequest`].
///
/// On a successful query `iq_type` is `result` and `query` holds the listed
/// items. On failure `iq_type` is `error` and `query` is absent.
#[derive(Deserialize, Debug)]
#[serde(rename = "iq")]
pub struct DiscoItemsResult
{
    #[serde(rename = "@type")]
    pub iq_type: String,
    #[serde(rename = "@from", default)]
    pub from: Option<String>,
    #[serde(default)]
    pub query: Option<DiscoItemsQuery>,
}

#[derive(Deserialize, Debug, Default)]
pub struct DiscoItemsQuery
{
    #[serde(rename = "@node", default)]
    pub node: Option<String>,
    #[serde(rename = "item", default)]
    pub items: Vec<Item>,
}

#[derive(Deserialize, Debug)]
pub struct Item
{
    #[serde(rename = "@jid")]
    pub jid: String,
    #[serde(rename = "@name", default)]
    pub name: Option<String>,
    #[serde(rename = "@node", default)]
    pub node: Option<String>,
}

impl DiscoItemsResult
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        return quick_xml::de::from_str(&project_disco(xml, Some(DISCO_ITEMS_NS))?).map_err(|e| e.to_string());
    }
}

/// A parsed incoming XEP-0030 service discovery query (`<iq type='get'>`).
///
/// A remote entity sends one to learn what this client is and what it
/// supports. `query.xmlns` distinguishes an **info** query
/// (`http://jabber.org/protocol/disco#info`) from an **items** query
/// (`http://jabber.org/protocol/disco#items`); `from` and `id` are echoed back
/// in the reply so the server routes it to the asker and the asker can match it
/// to the request.
#[derive(Deserialize, Debug)]
#[serde(rename = "iq")]
pub struct IncomingDiscoQuery
{
    #[serde(rename = "@from", default)]
    pub from: Option<String>,
    #[serde(rename = "@id", default)]
    pub id: Option<String>,
    pub query: IncomingQuery,
}

#[derive(Deserialize, Debug)]
pub struct IncomingQuery
{
    #[serde(rename = "@xmlns")]
    pub xmlns: String,
    #[serde(rename = "@node", default)]
    pub node: Option<String>,
}

impl IncomingDiscoQuery
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        return quick_xml::de::from_str(&project_disco(xml, None)?).map_err(|e| e.to_string());
    }
}

// Project only the expanded names that belong to this protocol before serde
// reads their values. Serde alone compares local names and loses namespace scope.
fn project_disco(xml: &str, result_namespace: Option<&str>) -> Result<String, String>
{
    use quick_xml::{events::{BytesEnd, BytesStart, Event}, name::ResolveResult, NsReader, Writer};
    #[derive(Clone, Copy)]
    enum Node { Root, Query, Leaf, Unknown }
    use Node::*;
    let attributes = super::xml::element(xml, b"iq", b"jabber:client")?;
    let kind = attributes.iter().find(|(name, _)| name == "type").map(|(_, value)| value.as_str());
    if !matches!((result_namespace, kind), (None, Some("get")) | (Some(_), Some("result" | "error"))) {
        return Err("Unexpected discovery IQ type".into());
    }
    let normalized_xml = crate::stanza::xml::normalize_xml(xml)?;
    let mut reader = NsReader::from_str(&normalized_xml);
    reader.config_mut().expand_empty_elements = true;
    let mut writer = Writer::new(Vec::new());
    let mut stack = Vec::new();
    let mut query_namespace = None;
    let mut request_children = 0;
    loop {
        match reader.read_resolved_event().map_err(|e| e.to_string())? {
            (ns, Event::Start(e)) => {
                if result_namespace.is_none() && stack.len() == 1 {
                    request_children += 1;
                    if request_children > 1 { return Err("Discovery request must have one payload".into()); }
                }
                let namespace = match &ns { ResolveResult::Bound(ns) => ns.as_ref(), _ => b"" };
                let local = e.local_name();
                let node = match (stack.last(), local.as_ref()) {
                    (None, _) => Root,
                    (Some(Root), b"query") if kind != Some("error")
                        && matches!(namespace, b"http://jabber.org/protocol/disco#info" | b"http://jabber.org/protocol/disco#items")
                        && result_namespace.is_none_or(|expected| namespace == expected.as_bytes()) => {
                        if query_namespace.is_some() { return Err("Duplicate discovery query".into()); }
                        query_namespace = Some(std::str::from_utf8(namespace).map_err(|e| e.to_string())?.to_string());
                        Query
                    }
                    (Some(Query), _) if result_namespace.is_none() => return Err("Discovery request query must be empty".into()),
                    (Some(Query), b"identity" | b"feature") if namespace == DISCO_INFO_NS.as_bytes()
                        && query_namespace.as_deref() == Some(DISCO_INFO_NS) => Leaf,
                    (Some(Query), b"item") if namespace == DISCO_ITEMS_NS.as_bytes()
                        && query_namespace.as_deref() == Some(DISCO_ITEMS_NS) => Leaf,
                    (Some(Leaf), _) => return Err("Discovery entry must be empty".into()),
                    _ => Unknown,
                };
                if !matches!(node, Unknown) {
                    let name = std::str::from_utf8(local.as_ref()).map_err(|e| e.to_string())?;
                    let mut start = BytesStart::new(name);
                    super::xml::copy_unqualified_attributes(&e, &mut start)?;
                    if matches!(node, Query) { start.push_attribute(("xmlns", query_namespace.as_deref().unwrap())); }
                    writer.write_event(Event::Start(start)).map_err(|e| e.to_string())?;
                }
                stack.push(node);
            }
            (_, Event::End(e)) => {
                if !matches!(stack.pop().ok_or("Unexpected discovery closing element")?, Unknown) {
                    let local = e.local_name();
                    writer.write_event(Event::End(BytesEnd::new(std::str::from_utf8(local.as_ref()).map_err(|e| e.to_string())?))).map_err(|e| e.to_string())?;
                }
            }
            (_, Event::Text(e)) => {
                if matches!(stack.last(), Some(Leaf)) && !e.is_empty()
                    || matches!(stack.last(), Some(Query)) && (result_namespace.is_none() && !e.is_empty()
                        || !e.as_ref().iter().all(u8::is_ascii_whitespace))
                    || result_namespace.is_none() && matches!(stack.last(), Some(Root)) && !e.as_ref().iter().all(u8::is_ascii_whitespace) {
                    return Err("Unexpected discovery text".into());
                }
            }
            (_, Event::CData(_) | Event::GeneralRef(_)) if matches!(stack.last(), Some(Leaf | Query)) => return Err("Unexpected discovery content".into()),
            (_, Event::CData(_) | Event::GeneralRef(_)) if result_namespace.is_none() && matches!(stack.last(), Some(Root)) => return Err("Unexpected discovery request content".into()),
            (_, Event::Eof) => return String::from_utf8(writer.into_inner()).map_err(|e| e.to_string()),
            _ => {},
        }
    }
}

/// An outgoing reply to a service discovery **info** query, advertising the
/// identities and features this client supports.
///
/// Serialises to `<iq type='result' to='...' id='...'><query
/// xmlns='http://jabber.org/protocol/disco#info'>...</query></iq>`. `to` and
/// `id` echo the originating request; an optional `node` is echoed when the
/// request scoped its query to a node.
pub struct DiscoInfoResponse
{
    id: String,
    to: String,
    node: Option<String>,
    info: DiscoInfo,
}

impl DiscoInfoResponse
{
    pub fn new(id: String, to: String, node: Option<String>, info: DiscoInfo) -> Self
    {
        return Self { id, to, node, info };
    }
}

impl Stanza for DiscoInfoResponse
{
    fn to_xml(&self) -> String
    {
        let mut children = String::new();
        for identity in &self.info.identities
        {
            let name_attr = match &identity.name
            {
                Some(name) => format!(" name='{}'", super::xml::escape_attribute_value(name)),
                None => String::new(),
            };
            children.push_str(&format!(
                "<identity category='{}' type='{}'{}/>",
                super::xml::escape_attribute_value(&identity.category),
                super::xml::escape_attribute_value(&identity.kind),
                name_attr
            ));
        }
        for feature in &self.info.features
        {
            children.push_str(&format!(
                "<feature var='{}'/>",
                super::xml::escape_attribute_value(feature)
            ));
        }

        return format!(
            "<iq type='result' to='{}' id='{}'>{}</iq>",
            super::xml::escape_attribute_value(&self.to),
            super::xml::escape_attribute_value(&self.id),
            query_with_children(DISCO_INFO_NS, &self.node, &children)
        );
    }
}

/// An outgoing reply to a service discovery **items** query, listing the items
/// this client hosts (empty by default for a plain client).
///
/// Serialises to `<iq type='result' to='...' id='...'><query
/// xmlns='http://jabber.org/protocol/disco#items'>...</query></iq>`.
pub struct DiscoItemsResponse
{
    id: String,
    to: String,
    node: Option<String>,
    items: Vec<DiscoItem>,
}

impl DiscoItemsResponse
{
    pub fn new(id: String, to: String, node: Option<String>, items: Vec<DiscoItem>) -> Self
    {
        return Self { id, to, node, items };
    }
}

impl Stanza for DiscoItemsResponse
{
    fn to_xml(&self) -> String
    {
        let mut children = String::new();
        for item in &self.items
        {
            let name_attr = match &item.name
            {
                Some(name) => format!(" name='{}'", super::xml::escape_attribute_value(name)),
                None => String::new(),
            };
            let node_attr = match &item.node
            {
                Some(node) => format!(" node='{}'", super::xml::escape_attribute_value(node)),
                None => String::new(),
            };
            children.push_str(&format!(
                "<item jid='{}'{}{}/>",
                super::xml::escape_attribute_value(&item.jid),
                name_attr,
                node_attr
            ));
        }

        return format!(
            "<iq type='result' to='{}' id='{}'>{}</iq>",
            super::xml::escape_attribute_value(&self.to),
            super::xml::escape_attribute_value(&self.id),
            query_with_children(DISCO_ITEMS_NS, &self.node, &children)
        );
    }
}

/// Build a `<query>` element for the given namespace, echoing an optional
/// `node` and wrapping `children`. An empty body collapses to a self-closing
/// element.
fn query_with_children(ns: &str, node: &Option<String>, children: &str) -> String
{
    let node_attr = match node
    {
        Some(node) => format!(" node='{}'", super::xml::escape_attribute_value(node)),
        None => String::new(),
    };

    return match children.is_empty()
    {
        true => format!("<query xmlns='{}'{}/>", ns, node_attr),
        false => format!("<query xmlns='{}'{}>{}</query>", ns, node_attr, children),
    };
}

#[cfg(test)]
mod tests
{
    use super::*;
    use crate::DiscoIdentity;

    #[test]
    fn discovery_results_resolve_prefixes_and_ignore_foreign_or_nested_entries() {
        let xml = "<c:iq xmlns:c='jabber:client' xmlns:d='http://jabber.org/protocol/disco#info' xmlns:e='extension' type='result' from='real.example' e:from='forged'>
            <e:query><e:feature var='forged'/></e:query>
            <d:query><e:identity category='forged' type='bad'/><d:identity category='server' type='im' e:name='forged'/>
            <d:feature var='real' e:var='forged'/><e:feature var='forged'/><e:extension><d:feature var='nested'/></e:extension></d:query></c:iq>";
        let result = DiscoInfoResult::from_xml(xml).unwrap();
        assert_eq!(result.from.as_deref(), Some("real.example"));
        let query = result.query.unwrap();
        assert_eq!(query.identities.len(), 1);
        assert_eq!(query.identities[0].category, "server");
        assert!(query.identities[0].name.is_none());
        assert_eq!(query.features.len(), 1);
        assert_eq!(query.features[0].var, "real");
        let xml = "<iq xmlns='jabber:client' xmlns:d='http://jabber.org/protocol/disco#items' xmlns:e='extension' type='result'><d:query>
            <e:item jid='forged'/><d:item jid='real@example' name='Real &amp; Room' e:jid='forged'/><e:extension><d:item jid='nested'/></e:extension></d:query></iq>";
        let items = DiscoItemsResult::from_xml(xml).unwrap().query.unwrap().items;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].jid, "real@example");
        assert_eq!(items[0].name.as_deref(), Some("Real & Room"));
        for namespace in ["extension", DISCO_ITEMS_NS] {
            let xml = format!("<iq xmlns='jabber:client' type='result'><query xmlns='{namespace}'/></iq>");
            assert!(DiscoInfoResult::from_xml(&xml).unwrap().query.is_none());
        }
    }

    #[test]
    fn discovery_rejects_ambiguous_or_malformed_entries() {
        for payload in [
            "<query/><query/>", "<query><feature var='real'><nested/></feature></query>",
            "<query><feature var='real'>text</feature></query>",
            "<query><identity category='server' e:type='forged'/></query>",
            "<query><feature e:var='forged'/></query>", "<query>text</query>",
        ] {
            let xml = format!("<c:iq xmlns:c='jabber:client' xmlns='http://jabber.org/protocol/disco#info' xmlns:e='extension' type='result'>{payload}</c:iq>");
            assert!(DiscoInfoResult::from_xml(&xml).is_err(), "{xml}");
        }
        assert!(DiscoItemsResult::from_xml("<iq xmlns='jabber:client' type='result' xmlns:e='extension'><query xmlns='http://jabber.org/protocol/disco#items'><item e:jid='forged'/></query></iq>").is_err());
        // An IQ error is a failure even when it echoes malformed query data.
        assert_eq!(DiscoInfoResult::from_xml("<iq xmlns='jabber:client' type='error'><query xmlns='http://jabber.org/protocol/disco#info'><identity/></query></iq>").unwrap().iq_type, "error");
    }

    #[test]
    fn incoming_discovery_uses_namespace_scope_and_unqualified_routing_attributes() {
        let xml = "<c:iq xmlns:c='jabber:client' xmlns:d='http://jabber.org/protocol/disco#info' xmlns:e='extension' type='get' from='real@example' id='real' e:from='forged' e:id='forged'><d:query node='real&amp;node' e:node='forged'/></c:iq>";
        let request = IncomingDiscoQuery::from_xml(xml).unwrap();
        assert_eq!(request.from.as_deref(), Some("real@example"));
        assert_eq!(request.id.as_deref(), Some("real"));
        assert_eq!(request.query.xmlns, DISCO_INFO_NS);
        assert_eq!(request.query.node.as_deref(), Some("real&node"));
        let request = IncomingDiscoQuery::from_xml("<iq xmlns='jabber:client' xmlns:e='extension' e:from='forged' e:id='forged' type='get'><query xmlns='http://jabber.org/protocol/disco#items'/></iq>").unwrap();
        assert!(request.from.is_none());
        assert!(request.id.is_none());
        for payload in ["<e:query xmlns='http://jabber.org/protocol/disco#info'/>",
            "<extension><query xmlns='http://jabber.org/protocol/disco#info'/></extension>",
            "<query xmlns='http://jabber.org/protocol/disco#info'><nested/></query>",
            "<extension/><query xmlns='http://jabber.org/protocol/disco#info'/>",
            "text<query xmlns='http://jabber.org/protocol/disco#info'/>",
            "<query xmlns='http://jabber.org/protocol/disco#info'/><query xmlns='http://jabber.org/protocol/disco#items'/>"] {
            let xml = format!("<iq xmlns='jabber:client' xmlns:e='extension' type='get'>{payload}</iq>");
            assert!(IncomingDiscoQuery::from_xml(&xml).is_err(), "{xml}");
        }
    }

    #[test]
    fn outgoing_info_query_to_server()
    {
        let req = DiscoInfoRequest::new("disco_1".to_string(), None, None);
        assert_eq!(
            req.to_xml(),
            "<iq type='get' id='disco_1'><query xmlns='http://jabber.org/protocol/disco#info'/></iq>"
        );
    }

    #[test]
    fn outgoing_info_query_to_target_with_node()
    {
        let req = DiscoInfoRequest::new(
            "disco_2".to_string(),
            Some("conference.example.com".to_string()),
            Some("http://jabber.org/protocol/commands".to_string()),
        );
        assert_eq!(
            req.to_xml(),
            "<iq type='get' to='conference.example.com' id='disco_2'>\
             <query xmlns='http://jabber.org/protocol/disco#info' \
             node='http://jabber.org/protocol/commands'/></iq>"
        );
    }

    #[test]
    fn outgoing_items_query_to_target()
    {
        let req = DiscoItemsRequest::new(
            "disco_3".to_string(),
            Some("conference.example.com".to_string()),
            None,
        );
        assert_eq!(
            req.to_xml(),
            "<iq type='get' to='conference.example.com' id='disco_3'>\
             <query xmlns='http://jabber.org/protocol/disco#items'/></iq>"
        );
    }

    #[test]
    fn parse_info_result()
    {
        let xml = "<iq xmlns='jabber:client' type='result' from='example.com' id='disco_1'>\
            <query xmlns='http://jabber.org/protocol/disco#info'>\
            <identity category='server' type='im' name='Example Server'/>\
            <feature var='http://jabber.org/protocol/disco#info'/>\
            <feature var='urn:xmpp:ping'/>\
            </query></iq>";

        let result = DiscoInfoResult::from_xml(xml).unwrap();
        assert_eq!(result.iq_type, "result");
        assert_eq!(result.from.as_deref(), Some("example.com"));

        let query = result.query.unwrap();
        assert_eq!(query.identities.len(), 1);
        assert_eq!(query.identities[0].category, "server");
        assert_eq!(query.identities[0].kind, "im");
        assert_eq!(query.identities[0].name.as_deref(), Some("Example Server"));

        let features: Vec<&str> = query.features.iter().map(|f| f.var.as_str()).collect();
        assert_eq!(features, vec!["http://jabber.org/protocol/disco#info", "urn:xmpp:ping"]);
    }

    #[test]
    fn parse_info_error_reply()
    {
        // An error reply typically echoes the original (empty) <query/>, so the
        // iq type is what distinguishes failure from an empty result.
        let xml = "<iq xmlns='jabber:client' type='error' from='example.com' id='disco_1'>\
            <query xmlns='http://jabber.org/protocol/disco#info'/>\
            <error type='cancel'><service-unavailable \
            xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></iq>";

        let result = DiscoInfoResult::from_xml(xml).unwrap();
        assert_eq!(result.iq_type, "error");
    }

    #[test]
    fn parse_items_result()
    {
        let xml = "<iq xmlns='jabber:client' type='result' from='conference.example.com' id='disco_3'>\
            <query xmlns='http://jabber.org/protocol/disco#items'>\
            <item jid='room1@conference.example.com' name='Room One'/>\
            <item jid='room2@conference.example.com'/>\
            </query></iq>";

        let result = DiscoItemsResult::from_xml(xml).unwrap();
        assert_eq!(result.iq_type, "result");

        let query = result.query.unwrap();
        assert_eq!(query.items.len(), 2);
        assert_eq!(query.items[0].jid, "room1@conference.example.com");
        assert_eq!(query.items[0].name.as_deref(), Some("Room One"));
        assert_eq!(query.items[1].jid, "room2@conference.example.com");
        assert_eq!(query.items[1].name, None);
    }

    #[test]
    fn parse_incoming_info_query()
    {
        let xml = "<iq xmlns='jabber:client' type='get' from='asker@example.com/x' id='req_1'>\
            <query xmlns='http://jabber.org/protocol/disco#info'/></iq>";

        let query = IncomingDiscoQuery::from_xml(xml).unwrap();
        assert_eq!(query.from.as_deref(), Some("asker@example.com/x"));
        assert_eq!(query.id.as_deref(), Some("req_1"));
        assert_eq!(query.query.xmlns, DISCO_INFO_NS);
        assert_eq!(query.query.node, None);
    }

    #[test]
    fn parse_incoming_items_query_with_node()
    {
        let xml = "<iq xmlns='jabber:client' type='get' from='asker@example.com' id='req_2'>\
            <query xmlns='http://jabber.org/protocol/disco#items' \
            node='http://jabber.org/protocol/commands'/></iq>";

        let query = IncomingDiscoQuery::from_xml(xml).unwrap();
        assert_eq!(query.query.xmlns, DISCO_ITEMS_NS);
        assert_eq!(query.query.node.as_deref(), Some("http://jabber.org/protocol/commands"));
    }

    #[test]
    fn incoming_query_rejects_non_query_iq()
    {
        // A ping get carries no <query>, so it must not parse as a disco query.
        let xml = "<iq xmlns='jabber:client' type='get' from='a@b' id='p1'><ping xmlns='urn:xmpp:ping'/></iq>";
        assert!(IncomingDiscoQuery::from_xml(xml).is_err());
    }

    #[test]
    fn info_response_to_xml()
    {
        let info = DiscoInfo
        {
            identities: vec![DiscoIdentity
            {
                category: "client".to_string(),
                kind: "bot".to_string(),
                name: Some("libxmpp".to_string()),
            }],
            features: vec![DISCO_INFO_NS.to_string(), "urn:xmpp:ping".to_string()],
        };
        let response = DiscoInfoResponse::new(
            "req_1".to_string(),
            "asker@example.com/x".to_string(),
            None,
            info,
        );

        assert_eq!(
            response.to_xml(),
            "<iq type='result' to='asker@example.com/x' id='req_1'>\
             <query xmlns='http://jabber.org/protocol/disco#info'>\
             <identity category='client' type='bot' name='libxmpp'/>\
             <feature var='http://jabber.org/protocol/disco#info'/>\
             <feature var='urn:xmpp:ping'/>\
             </query></iq>"
        );
    }

    #[test]
    fn empty_items_response_to_xml()
    {
        let response = DiscoItemsResponse::new(
            "req_2".to_string(),
            "asker@example.com".to_string(),
            None,
            Vec::new(),
        );

        assert_eq!(
            response.to_xml(),
            "<iq type='result' to='asker@example.com' id='req_2'>\
             <query xmlns='http://jabber.org/protocol/disco#items'/></iq>"
        );
    }

    #[test]
    fn items_response_echoes_node_and_lists_items()
    {
        let response = DiscoItemsResponse::new(
            "req_3".to_string(),
            "asker@example.com".to_string(),
            Some("the-node".to_string()),
            vec![DiscoItem
            {
                jid: "room@conference.example.com".to_string(),
                name: Some("Room".to_string()),
                node: None,
            }],
        );

        assert_eq!(
            response.to_xml(),
            "<iq type='result' to='asker@example.com' id='req_3'>\
             <query xmlns='http://jabber.org/protocol/disco#items' node='the-node'>\
             <item jid='room@conference.example.com' name='Room'/>\
             </query></iq>"
        );
    }
}
