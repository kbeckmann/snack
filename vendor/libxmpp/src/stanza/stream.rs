use super::Stanza;
use quick_xml::Reader;
use quick_xml::events::Event;
use serde::Deserialize;

#[derive(Debug)]
pub struct Stream
{
    from: String,
    to: String,
    id: Option<String>,
}

impl Stream
{
    pub fn new(from: String, to: String) -> Self
    {
        Self { from, to, id: None }
    }

    pub fn from_xml(xml: &str) -> Result<(Self, &str), String>
    {
        let mut reader = Reader::from_str(xml);

        loop
        {
            match reader.read_event().map_err(|e| e.to_string())?
            {
                Event::Start(e) | Event::Empty(e) =>
                {
                    let name = e.name();
                    let local = name.local_name();
                    if local.as_ref() == b"stream"
                    {
                        let mut from = String::new();
                        let mut to = String::new();
                        let mut id = None;

                        for attr in e.attributes().flatten()
                        {
                            match attr.key.local_name().as_ref()
                            {
                                b"from" => from = attr.unescape_value().map_err(|e| e.to_string())?.to_string(),
                                b"to" => to = attr.unescape_value().map_err(|e| e.to_string())?.to_string(),
                                b"id" => id = Some(attr.unescape_value().map_err(|e| e.to_string())?.to_string()),
                                _ => {}
                            }
                        }

                        let rest = &xml[reader.buffer_position() as usize..];
                        return Ok((Self { from, to, id }, rest));
                    }
                }
                Event::Eof => return Err("Unexpected end of XML".to_string()),
                _ => {}
            }
        }
    }
}

impl Stanza for Stream
{
    fn to_xml(&self) -> String
    {
        format!(
            r#"<?xml version='1.0'?><stream:stream from='{}' to='{}' version='1.0' xml:lang='en' xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams'>"#,
            self.from, self.to
        )
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename = "features")]
pub struct StreamFeatures
{
    #[serde(rename = "starttls", default)]
    pub starttls: Option<StartTls>,
    #[serde(rename = "mechanisms", default)]
    pub mechanisms: Option<Mechanisms>,
    #[serde(rename = "bind", default)]
    pub bind: Option<BindFeature>,
}

#[derive(Deserialize, Debug)]
pub struct StartTls {}

#[derive(Deserialize, Debug)]
pub struct BindFeature {}

pub struct StartTlsRequest;

impl Stanza for StartTlsRequest
{
    fn to_xml(&self) -> String
    {
        "<starttls xmlns='urn:ietf:params:xml:ns:xmpp-tls'/>".to_string()
    }
}

#[derive(Deserialize, Debug)]
pub struct Mechanisms
{
    #[serde(rename = "@xmlns", default)]
    pub xmlns: Option<String>,
    #[serde(rename = "mechanism", default)]
    pub mechanism: Vec<String>,
}

impl StreamFeatures
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        return quick_xml::de::from_str(xml).map_err(|e| e.to_string());
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    const XML: &str = "<?xml version='1.0'?><stream:stream xmlns='jabber:client' xml:lang='en' version='1.0' xmlns:stream='http://etherx.jabber.org/streams' from='devsn.se' to='mick@devsn.se' id='9cb4ab6e-ca02-4d01-b442-4cf87cb58c93'><stream:features><starttls xmlns='urn:ietf:params:xml:ns:xmpp-tls'/><mechanisms xmlns='urn:ietf:params:xml:ns:xmpp-sasl'><mechanism>SCRAM-SHA-1</mechanism></mechanisms></stream:features>";

    #[test]
    fn parse_stream()
    {
        let (stream, _rest) = Stream::from_xml(XML).unwrap();
        assert_eq!(stream.from, "devsn.se");
        assert_eq!(stream.to, "mick@devsn.se");
        assert_eq!(stream.id, Some("9cb4ab6e-ca02-4d01-b442-4cf87cb58c93".to_string()));
    }

    #[test]
    fn parse_stream_features()
    {
        let (_stream, rest) = Stream::from_xml(XML).unwrap();
        let features = StreamFeatures::from_xml(rest).unwrap();
        assert!(features.starttls.is_some());
        assert!(features.mechanisms.is_some());
    }

    #[test]
    fn parse_mechanisms()
    {
        let (_stream, rest) = Stream::from_xml(XML).unwrap();
        let features = StreamFeatures::from_xml(rest).unwrap();
        let mechanisms = features.mechanisms.unwrap();
        assert_eq!(mechanisms.mechanism.len(), 1);
        assert_eq!(mechanisms.mechanism[0], "SCRAM-SHA-1");
    }
}
