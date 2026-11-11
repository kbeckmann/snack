use super::Stanza;
use quick_xml::{NsReader, name::ResolveResult};
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
        // Preserve the returned suffix's original offsets even if namespace
        // references in the header change length during normalization.
        // XMPP treats U+FEFF as text rather than an encoding marker. It is
        // not permitted as character data before the stream's opening tag.
        if xml.starts_with('\u{feff}') { return Err("Unexpected text before stream header".into()); }
        let mut raw = quick_xml::Reader::from_str(xml);
        let mut declaration_allowed = true;
        let header_end = loop {
            match raw.read_event().map_err(|e| e.to_string())? {
                Event::Start(_) => break raw.buffer_position() as usize,
                Event::Decl(declaration) if declaration_allowed => {
                    crate::xml_framer::validate_declaration(&declaration)
                        .map_err(|error| format!("Invalid stream declaration: {}", error.condition()))?;
                    declaration_allowed = false;
                }
                Event::Text(text) if text.as_ref().iter().all(u8::is_ascii_whitespace) => declaration_allowed = false,
                Event::Eof => return Err("Incomplete stream header".into()),
                _ => return Err("Invalid stream preamble".into()),
            }
        };
        let normalized_xml = super::xml::normalize_xml(&xml[..header_end])?;
        let mut reader = NsReader::from_str(&normalized_xml);
        loop {
            match reader.read_event().map_err(|e| e.to_string())? {
                event @ Event::Start(_) => {
                    crate::xml_framer::validate_node(&reader, &event)
                        .map_err(|error| format!("Invalid stream header: {}", error.condition()))?;
                    let Event::Start(e) = event else { unreachable!() };
                    if e.local_name().as_ref() != b"stream"
                        || !matches!(reader.resolver().resolve_element(e.name()).0, ResolveResult::Bound(ns) if ns.as_ref() == b"http://etherx.jabber.org/streams") {
                        return Err("Expected namespaced stream header".into());
                    }
                    let mut from = String::new();
                    let mut to = String::new();
                    let mut id = None;
                    for (index, attr) in e.attributes().enumerate() {
                        if index >= crate::xml_framer::MAX_ATTRIBUTES { return Err("Too many stream attributes".into()); }
                        let attr = attr.map_err(|e| e.to_string())?;
                        let value = attr.unescape_value().map_err(|e| e.to_string())?;
                        if !crate::xml_framer::valid_xml_text(&value) { return Err("Invalid XML character in stream header".into()); }
                        match attr.key.as_ref() {
                            b"from" => from = value.into_owned(),
                            b"to" => to = value.into_owned(),
                            b"id" => id = Some(value.into_owned()),
                            _ => {},
                        }
                    }
                    let rest = &xml[header_end..];
                    return Ok((Self { from, to, id }, rest));
                }
                Event::Decl(_) => {},
                Event::Text(e) if e.as_ref().iter().all(u8::is_ascii_whitespace) => {},
                _ => return Err("Invalid or incomplete stream header".into()),
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
            super::xml::escape_attribute_value(&self.from),
            super::xml::escape_attribute_value(&self.to)
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
    #[serde(rename = "sm", default)]
    pub sm: Vec<SmFeature>,
}

#[derive(Deserialize, Debug)]
pub struct StartTls {}

#[derive(Deserialize, Debug)]
pub struct BindFeature {}

/// An `<sm/>` stream feature advertising
/// [XEP-0198](https://xmpp.org/extensions/xep-0198.html) stream management.
/// Present in the post-authentication features when the server supports it.
///
/// Servers commonly advertise more than one — typically both
/// `urn:xmpp:sm:2` and `urn:xmpp:sm:3` — so `StreamFeatures::sm` is a `Vec`.
/// The `xmlns` distinguishes the versions.
#[derive(Deserialize, Debug)]
pub struct SmFeature
{
    #[serde(rename = "@xmlns", default)]
    pub xmlns: Option<String>,
}

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
        use quick_xml::{name::ResolveResult, NsReader};
        const STREAM: &[u8] = b"http://etherx.jabber.org/streams";
        const SASL: &[u8] = b"urn:ietf:params:xml:ns:xmpp-sasl";
        let normalized_xml = super::xml::normalize_xml(xml)?;
        let mut reader = NsReader::from_str(&normalized_xml);
        let mut features = Self { starttls: None, mechanisms: None, bind: None, sm: Vec::new() };
        let mut depth = 0usize;
        let mut seen_root = false;
        let mut in_mechanisms = false;
        let mut mechanism = None::<String>;
        loop {
            let event = reader.read_event().map_err(|e| e.to_string())?;
            crate::xml_framer::validate_node(&reader, &event)
                .map_err(|error| format!("Invalid stream features: {}", error.condition()))?;
            match event {
                event @ (Event::Start(_) | Event::Empty(_)) => {
                    let empty = matches!(event, Event::Empty(_));
                    let e = match event { Event::Start(e) | Event::Empty(e) => e, _ => unreachable!() };
                    let namespace = match reader.resolver().resolve_element(e.name()).0 { ResolveResult::Bound(ns) => ns.as_ref().to_vec(), _ => Vec::new() };
                    for attr in e.attributes() { attr.map_err(|e| e.to_string())?; }
                    let local = e.local_name();
                    let name = local.as_ref();
                    if depth == 0 {
                        if seen_root || name != b"features" || namespace != STREAM {
                            return Err("Expected namespaced stream features".into());
                        }
                        seen_root = true;
                    } else if depth == 1 {
                        match (name, namespace.as_slice()) {
                            (b"starttls", b"urn:ietf:params:xml:ns:xmpp-tls") => features.starttls = Some(StartTls {}),
                            (b"bind", b"urn:ietf:params:xml:ns:xmpp-bind") => features.bind = Some(BindFeature {}),
                            (b"sm", b"urn:xmpp:sm:2" | b"urn:xmpp:sm:3") =>
                                features.sm.push(SmFeature { xmlns: Some(String::from_utf8(namespace).unwrap()) }),
                            (b"mechanisms", ns) if ns == SASL => {
                                if features.mechanisms.is_some() { return Err("Duplicate SASL mechanisms feature".into()); }
                                features.mechanisms = Some(Mechanisms { xmlns: Some(String::from_utf8(SASL.to_vec()).unwrap()), mechanism: Vec::new() });
                                in_mechanisms = !empty;
                            }
                            _ => {},
                        }
                    } else if depth == 2 && in_mechanisms && name == b"mechanism" && namespace == SASL && !empty {
                        mechanism = Some(String::new());
                    } else if depth == 3 {
                        mechanism = None; // A mechanism name cannot contain markup.
                    }
                    if !empty { depth += 1; }
                }
                Event::End(_) => {
                    if depth == 3 {
                        if let Some(name) = mechanism.take() {
                            if !name.trim().is_empty() { features.mechanisms.as_mut().unwrap().mechanism.push(name.trim().into()); }
                        }
                    }
                    if depth == 2 { in_mechanisms = false; }
                    depth = depth.checked_sub(1).ok_or("Unexpected feature closing element")?;
                }
                Event::Text(e) => {
                    if let Some(name) = mechanism.as_mut() { name.push_str(&e.decode().map_err(|e| e.to_string())?); }
                    if depth == 0 && !e.as_ref().iter().all(u8::is_ascii_whitespace) { return Err("Text outside stream features".into()); }
                }
                Event::CData(e) => {
                    if depth == 0 { return Err("Content outside stream features".into()); }
                    if let Some(name) = mechanism.as_mut() { name.push_str(&e.decode().map_err(|e| e.to_string())?); }
                }
                Event::GeneralRef(e) => {
                    if depth == 0 { return Err("Content outside stream features".into()); }
                    if let Some(name) = mechanism.as_mut() {
                        name.push(e.resolve_char_ref().map_err(|e| e.to_string())?.ok_or("Invalid mechanism character reference")?);
                    }
                }
                Event::Eof => return if seen_root && depth == 0 { Ok(features) } else { Err("Incomplete stream features".into()) },
                _ => {},
            }
        }
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    const XML: &str = "<?xml version='1.0'?><stream:stream xmlns='jabber:client' xml:lang='en' version='1.0' xmlns:stream='http://etherx.jabber.org/streams' from='devsn.se' to='mick@devsn.se' id='9cb4ab6e-ca02-4d01-b442-4cf87cb58c93'><stream:features><starttls xmlns='urn:ietf:params:xml:ns:xmpp-tls'/><mechanisms xmlns='urn:ietf:params:xml:ns:xmpp-sasl'><mechanism>SCRAM-SHA-1</mechanism></mechanisms></stream:features>";

    #[test]
    fn standalone_header_rejects_restricted_preambles_and_invalid_namespaces() {
        let header = "<s:stream xmlns:s='http://etherx.jabber.org/streams' from='example'>";
        for prefix in ["<!-- forbidden -->", "<?forbidden instruction?>", "<!DOCTYPE stream>", "\u{feff}",
            "<?xml version='1.1'?>", "<?xml version='1.0' encoding='ISO-8859-1'?>", "<?xml version='1.0' standalone='yes'?>",
            " <?xml version='1.0'?>", "<?xml version='1.0'?><?xml version='1.0'?>"] {
            assert!(Stream::from_xml(&format!("{prefix}{header}")).is_err(), "accepted preamble {prefix:?}");
        }
        for attributes in [" x:extra='value'", " 7attribute='value'",
            " xmlns:p='urn:same' xmlns:q='urn:&#115;ame' p:a='one' q:a='two'"] {
            let header = format!("<s:stream xmlns:s='http://etherx.jabber.org/streams'{attributes}>");
            assert!(Stream::from_xml(&header).is_err(), "accepted header {header}");
        }
    }

    #[test]
    fn standalone_features_apply_stream_xml_validation() {
        for payload in ["<!-- forbidden -->", "<?forbidden instruction?>", "<7extension/>",
            "<extra x:attribute='value'/>", "<extra xmlns:p='urn:same' xmlns:q='urn:same' p:a='one' q:a='two'/>",
            "<extra>bad\0text</extra>", "<extra><![CDATA[bad\0text]]></extra>"] {
            let xml = format!("<s:features xmlns:s='http://etherx.jabber.org/streams'>{payload}</s:features>");
            assert!(StreamFeatures::from_xml(&xml).is_err(), "accepted feature XML {xml:?}");
        }
    }

    #[test]
    fn validated_header_preserves_unicode_identity_and_unparsed_suffix() {
        for prefix in ["", "<?xml version='1.0' encoding='utf-8'?>", "\r\n"] {
            let suffix = "\r\n<message note='not parsed yet'/><incomplete";
            let xml = format!("{prefix}<s:stream xmlns:s='http://etherx.jabber.org/&#115;treams' xmlns:x='urn:extra' x:名='value' from='é😀&#xD;example'>{suffix}");
            let (header, rest) = Stream::from_xml(&xml).unwrap();
            assert_eq!(header.from, "é😀\rexample");
            assert_eq!(rest, suffix);
            assert_eq!(rest.as_ptr(), xml[xml.len() - suffix.len()..].as_ptr());
        }
    }

    #[test]
    fn parse_stream()
    {
        let (stream, _rest) = Stream::from_xml(XML).unwrap();
        assert_eq!(stream.from, "devsn.se");
        assert_eq!(stream.to, "mick@devsn.se");
        assert_eq!(stream.id, Some("9cb4ab6e-ca02-4d01-b442-4cf87cb58c93".to_string()));
    }

    #[test]
    fn stream_header_requires_namespace_and_unqualified_identity_attributes()
    {
        for header in ["<stream:stream>", "<s:stream xmlns:s='unrelated'>", "<message/>",
            "<s:stream xmlns:s='http://etherx.jabber.org/streams' from='a' from='b'>",
            "<s:stream xmlns:s='http://etherx.jabber.org/streams' from='&#0;'>"] {
            assert!(Stream::from_xml(header).is_err(), "{header}");
        }
        let (stream, _) = Stream::from_xml("<s:stream xmlns:s='http://etherx.jabber.org/streams' xmlns:x='unrelated' from='good' x:from='wrong'>").unwrap();
        assert_eq!(stream.from, "good");
    }

    #[test]
    fn stream_features_reject_character_data_outside_the_root() {
        let features = "<features xmlns='http://etherx.jabber.org/streams'/>";
        for outside in ["<![CDATA[extra]]>", "<![CDATA[ ]]>", "&#x20;", "&#65;"] {
            for xml in [format!("{outside}{features}"), format!("{features}{outside}")] {
                assert!(StreamFeatures::from_xml(&xml).is_err(), "ignored out-of-root data: {xml}");
            }
        }
    }

    #[test]
    fn stream_features_preserve_valid_text_inside_the_root_and_outer_whitespace() {
        let xml = " \r\n<features xmlns='http://etherx.jabber.org/streams'><extra xmlns='urn:example'><![CDATA[valid extension text]]>&#x20;</extra><mechanisms xmlns='urn:ietf:params:xml:ns:xmpp-sasl'><mechanism><![CDATA[SCRAM]]>-SHA-&#50;56</mechanism></mechanisms></features>\t";
        let features = StreamFeatures::from_xml(xml).unwrap();
        assert_eq!(features.mechanisms.unwrap().mechanism, ["SCRAM-SHA-256"]);
    }

    #[test]
    fn parse_stream_features()
    {
        let mut framer = crate::xml_framer::XmlFramer::new();
        framer.feed(XML.as_bytes());
        framer.try_next().unwrap();
        let features = StreamFeatures::from_xml(&framer.try_next().unwrap()).unwrap();
        assert!(features.starttls.is_some());
        assert!(features.mechanisms.is_some());
    }

    #[test]
    fn parse_mechanisms()
    {
        let mut framer = crate::xml_framer::XmlFramer::new();
        framer.feed(XML.as_bytes());
        framer.try_next().unwrap();
        let features = StreamFeatures::from_xml(&framer.try_next().unwrap()).unwrap();
        let mechanisms = features.mechanisms.unwrap();
        assert_eq!(mechanisms.mechanism.len(), 1);
        assert_eq!(mechanisms.mechanism[0], "SCRAM-SHA-1");
    }

    #[test]
    fn features_resolve_prefixes_and_ignore_unrelated_namespaces()
    {
        let xml = "<s:features xmlns:s='http://etherx.jabber.org/streams' xmlns:a='urn:ietf:params:xml:ns:xmpp-sasl' xmlns:m='urn:xmpp:sm:3'>            <starttls xmlns='unrelated'/><bind xmlns='unrelated'/><sm xmlns='unrelated'/>            <m:sm/><a:mechanisms><mechanism xmlns='unrelated'>PLAIN</mechanism>            <a:mechanism>SCRAM-SHA-256</a:mechanism><a:mechanism><nested/>PLAIN</a:mechanism></a:mechanisms></s:features>";
        let features = StreamFeatures::from_xml(xml).unwrap();
        assert!(features.starttls.is_none());
        assert!(features.bind.is_none());
        assert_eq!(features.sm.len(), 1);
        assert_eq!(features.sm[0].xmlns.as_deref(), Some("urn:xmpp:sm:3"));
        assert_eq!(features.mechanisms.unwrap().mechanism, vec!["SCRAM-SHA-256"]);
        let features = StreamFeatures::from_xml("<features xmlns='http://etherx.jabber.org/streams'><mechanisms xmlns='unrelated'><mechanism>PLAIN</mechanism></mechanisms></features>").unwrap();
        assert!(features.mechanisms.is_none());
    }

    #[test]
    fn malformed_or_wrong_namespace_feature_roots_are_rejected()
    {
        for xml in ["<features/>", "<features xmlns='unrelated'/>",
            "<features xmlns='http://etherx.jabber.org/streams'>",
            "<features xmlns='http://etherx.jabber.org/streams'/><extra/>"] {
            assert!(StreamFeatures::from_xml(xml).is_err(), "{xml}");
        }
    }

    #[test]
    fn parse_sm_feature()
    {
        // Post-authentication features advertising resource binding alongside
        // XEP-0198 stream management.
        let xml = "<stream:features xmlns:stream='http://etherx.jabber.org/streams'>\
            <bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'/>\
            <sm xmlns='urn:xmpp:sm:3'/></stream:features>";
        let features = StreamFeatures::from_xml(xml).unwrap();
        assert!(features.bind.is_some());
        assert_eq!(features.sm.len(), 1);
    }

    #[test]
    fn parse_multiple_sm_features()
    {
        // Servers commonly advertise stream management in both the :2 and :3
        // namespaces; both `<sm>` elements must be collected rather than
        // rejected as a duplicate field.
        let xml = "<stream:features xmlns:stream='http://etherx.jabber.org/streams'>\
            <bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'/>\
            <sm xmlns='urn:xmpp:sm:2'/>\
            <sm xmlns='urn:xmpp:sm:3'/></stream:features>";
        let features = StreamFeatures::from_xml(xml).unwrap();
        assert_eq!(features.sm.len(), 2);
        assert!(features.sm.iter().any(|sm| sm.xmlns.as_deref() == Some("urn:xmpp:sm:3")));
    }

    #[test]
    fn parse_features_without_sm()
    {
        let xml = "<stream:features xmlns:stream='http://etherx.jabber.org/streams'>\
            <bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'/></stream:features>";
        let features = StreamFeatures::from_xml(xml).unwrap();
        assert!(features.sm.is_empty());
    }
}
