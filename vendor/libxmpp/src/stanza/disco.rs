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
            quick_xml::escape::escape(&self.id),
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
            quick_xml::escape::escape(&self.id),
            query_element(DISCO_ITEMS_NS, &self.node)
        );
    }
}

fn to_attr(to: &Option<String>) -> String
{
    return match to
    {
        Some(to) => format!(" to='{}'", quick_xml::escape::escape(to)),
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
            quick_xml::escape::escape(node)
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
        return quick_xml::de::from_str(xml).map_err(|e| e.to_string());
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
        return quick_xml::de::from_str(xml).map_err(|e| e.to_string());
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
        return quick_xml::de::from_str(xml).map_err(|e| e.to_string());
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
                Some(name) => format!(" name='{}'", quick_xml::escape::escape(name)),
                None => String::new(),
            };
            children.push_str(&format!(
                "<identity category='{}' type='{}'{}/>",
                quick_xml::escape::escape(&identity.category),
                quick_xml::escape::escape(&identity.kind),
                name_attr
            ));
        }
        for feature in &self.info.features
        {
            children.push_str(&format!(
                "<feature var='{}'/>",
                quick_xml::escape::escape(feature)
            ));
        }

        return format!(
            "<iq type='result' to='{}' id='{}'>{}</iq>",
            quick_xml::escape::escape(&self.to),
            quick_xml::escape::escape(&self.id),
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
                Some(name) => format!(" name='{}'", quick_xml::escape::escape(name)),
                None => String::new(),
            };
            let node_attr = match &item.node
            {
                Some(node) => format!(" node='{}'", quick_xml::escape::escape(node)),
                None => String::new(),
            };
            children.push_str(&format!(
                "<item jid='{}'{}{}/>",
                quick_xml::escape::escape(&item.jid),
                name_attr,
                node_attr
            ));
        }

        return format!(
            "<iq type='result' to='{}' id='{}'>{}</iq>",
            quick_xml::escape::escape(&self.to),
            quick_xml::escape::escape(&self.id),
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
        Some(node) => format!(" node='{}'", quick_xml::escape::escape(node)),
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
        let xml = "<iq type='result' from='example.com' id='disco_1'>\
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
        let xml = "<iq type='error' from='example.com' id='disco_1'>\
            <query xmlns='http://jabber.org/protocol/disco#info'/>\
            <error type='cancel'><service-unavailable \
            xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></iq>";

        let result = DiscoInfoResult::from_xml(xml).unwrap();
        assert_eq!(result.iq_type, "error");
    }

    #[test]
    fn parse_items_result()
    {
        let xml = "<iq type='result' from='conference.example.com' id='disco_3'>\
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
        let xml = "<iq type='get' from='asker@example.com/x' id='req_1'>\
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
        let xml = "<iq type='get' from='asker@example.com' id='req_2'>\
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
        let xml = "<iq type='get' from='a@b' id='p1'><ping xmlns='urn:xmpp:ping'/></iq>";
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
