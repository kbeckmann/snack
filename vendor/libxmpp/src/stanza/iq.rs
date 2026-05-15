//! Lightweight IQ stanza helpers and PubSub/PEP request builders.

use super::Stanza;

/// A correlated IQ request. The `id` is used to route the response back to
/// the awaiting caller.
pub struct Iq
{
    pub id: String,
    pub iq_type: String,
    pub to: Option<String>,
    pub payload_xml: String,
}

impl Iq
{
    pub fn get(id: String, to: Option<String>, payload_xml: String) -> Self
    {
        return Self { id, iq_type: "get".to_string(), to, payload_xml };
    }

    pub fn set(id: String, to: Option<String>, payload_xml: String) -> Self
    {
        return Self { id, iq_type: "set".to_string(), to, payload_xml };
    }

    pub fn result(id: String, to: Option<String>, payload_xml: String) -> Self
    {
        return Self { id, iq_type: "result".to_string(), to, payload_xml };
    }
}

impl Stanza for Iq
{
    fn to_xml(&self) -> String
    {
        let to_attr = match &self.to
        {
            Some(t) => format!(" to='{}'", t),
            None => String::new(),
        };

        return format!(
            "<iq type='{}' id='{}'{}>{}</iq>",
            self.iq_type, self.id, to_attr, self.payload_xml,
        );
    }
}

/// PubSub publish payload (XEP-0060 §7.1) without the wrapping <iq>.
pub fn pubsub_publish_payload(node: &str, item_id: Option<&str>, item_xml: &str) -> String
{
    let item_id_attr = item_id.map(|i| format!(" id='{}'", i)).unwrap_or_default();
    return format!(
        "<pubsub xmlns='http://jabber.org/protocol/pubsub'>\
            <publish node='{}'>\
                <item{}>{}</item>\
            </publish>\
        </pubsub>",
        node, item_id_attr, item_xml,
    );
}

/// PubSub publish with options to relax the access model so that subscribers
/// don't need to be on the roster. Used for OMEMO device list / bundles which
/// must be readable by anyone who wants to send us an encrypted message.
pub fn pubsub_publish_open_payload(node: &str, item_id: Option<&str>, item_xml: &str) -> String
{
    let item_id_attr = item_id.map(|i| format!(" id='{}'", i)).unwrap_or_default();
    return format!(
        "<pubsub xmlns='http://jabber.org/protocol/pubsub'>\
            <publish node='{}'>\
                <item{}>{}</item>\
            </publish>\
            <publish-options>\
                <x xmlns='jabber:x:data' type='submit'>\
                    <field var='FORM_TYPE' type='hidden'>\
                        <value>http://jabber.org/protocol/pubsub#publish-options</value>\
                    </field>\
                    <field var='pubsub#access_model'><value>open</value></field>\
                    <field var='pubsub#persist_items'><value>true</value></field>\
                </x>\
            </publish-options>\
        </pubsub>",
        node, item_id_attr, item_xml,
    );
}

/// PubSub items request (XEP-0060 §6.5).
pub fn pubsub_items_payload(node: &str, max_items: Option<u32>) -> String
{
    let max_attr = max_items.map(|n| format!(" max_items='{}'", n)).unwrap_or_default();
    return format!(
        "<pubsub xmlns='http://jabber.org/protocol/pubsub'>\
            <items node='{}'{}/>\
        </pubsub>",
        node, max_attr,
    );
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn iq_get_serialises()
    {
        let iq = Iq::get("abc".into(), Some("bob@example.com".into()), "<x/>".into());
        assert_eq!(iq.to_xml(), "<iq type='get' id='abc' to='bob@example.com'><x/></iq>");
    }

    #[test]
    fn iq_set_no_to()
    {
        let iq = Iq::set("99".into(), None, "<p/>".into());
        assert_eq!(iq.to_xml(), "<iq type='set' id='99'><p/></iq>");
    }

    #[test]
    fn pubsub_publish_payload_open_includes_publish_options()
    {
        let p = pubsub_publish_open_payload("node:1", Some("item1"), "<x/>");
        assert!(p.contains("<publish node='node:1'>"));
        assert!(p.contains("<item id='item1'>"));
        assert!(p.contains("<publish-options>"));
        assert!(p.contains("pubsub#access_model"));
    }

    #[test]
    fn pubsub_items_payload_max()
    {
        let p = pubsub_items_payload("node:1", Some(5));
        assert!(p.contains("<items node='node:1' max_items='5'/>"));
    }
}
