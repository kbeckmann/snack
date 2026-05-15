use super::Stanza;
use serde::Deserialize;

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
                self.id, res
            ),
            None => format!(
                "<iq type='set' id='{}'><bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'/></iq>",
                self.id
            ),
        }
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename = "iq")]
pub struct BindResult
{
    #[serde(rename = "@type")]
    pub iq_type: String,
    #[serde(rename = "@id")]
    pub id: String,
    #[serde(default)]
    pub bind: Option<BindInner>,
}

#[derive(Deserialize, Debug)]
pub struct BindInner
{
    #[serde(default)]
    pub jid: Option<String>,
}

impl BindResult
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        return quick_xml::de::from_str(xml).map_err(|e| e.to_string());
    }
}
