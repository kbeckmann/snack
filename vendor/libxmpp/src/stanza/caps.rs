use super::Stanza;
use crate::DiscoInfo;
use base64::{ Engine, engine::general_purpose::STANDARD as BASE64 };
use sha1::{ Sha1, Digest };
use serde::Deserialize;

/// XML namespace for [XEP-0115](https://xmpp.org/extensions/xep-0115.html)
/// entity capabilities.
pub(crate) const CAPS_NS: &str = "http://jabber.org/protocol/caps";

/// The hashing algorithm libxmpp uses to compute the verification string, named
/// as it appears in the `<c/>` element's `hash` attribute. XEP-0115 defines
/// SHA-1 as the interoperable default understood by every deployed client.
pub(crate) const CAPS_HASH: &str = "sha-1";

/// Build the XEP-0115 §5.1 verification string for a set of advertised
/// identities and features.
///
/// This is the canonical concatenation the `ver` hash is computed over: each
/// identity as `category/type/lang/name`, then each feature, every field
/// terminated by `<`, with identities and features sorted so two entities
/// advertising the same disco info always produce the same string. libxmpp does
/// not advertise extended (XEP-0128) data forms, so that part of the algorithm
/// is omitted.
pub(crate) fn verification_string(info: &DiscoInfo) -> String
{
    let mut s = String::new();

    // Identities sort by category, then type, then xml:lang, then name. libxmpp
    // identities carry no xml:lang, so that field is always empty.
    let mut identities: Vec<String> = info.identities.iter().map(|i|
    {
        format!("{}/{}//{}", i.category, i.kind, i.name.as_deref().unwrap_or(""))
    }).collect();
    identities.sort();
    for identity in &identities
    {
        s.push_str(identity);
        s.push('<');
    }

    // Features sort by their namespace string.
    let mut features = info.features.clone();
    features.sort();
    for feature in &features
    {
        s.push_str(feature);
        s.push('<');
    }

    return s;
}

/// Compute the XEP-0115 verification hash (`ver`) for the given disco info: the
/// SHA-1 of the [`verification_string`], Base64-encoded.
///
/// The result pairs with [`CAPS_HASH`] in the advertised `<c/>` element and is
/// stable across runs and across clients advertising the same identities and
/// features.
pub(crate) fn compute_ver(info: &DiscoInfo) -> String
{
    let mut hasher = Sha1::new();
    hasher.update(verification_string(info).as_bytes());
    return BASE64.encode(hasher.finalize());
}

/// Build the bare `<c/>` entity-capabilities element for embedding in a presence
/// stanza. `node` identifies the client software (a URI) and `ver` is the
/// [`compute_ver`] hash of the disco info the client advertises.
pub(crate) fn caps_element(node: &str, ver: &str) -> String
{
    return format!(
        "<c xmlns='{}' hash='{}' node='{}' ver='{}'/>",
        CAPS_NS,
        CAPS_HASH,
        super::xml::escape_attribute_value(node),
        super::xml::escape_attribute_value(ver),
    );
}

/// An outgoing available presence that advertises this client's XEP-0115 entity
/// capabilities.
///
/// Serialises to `<presence><c xmlns='http://jabber.org/protocol/caps'
/// hash='sha-1' node='...' ver='...'/></presence>`, letting entities that
/// recognise the `node`/`ver` pair reuse a cached copy of this client's disco
/// info instead of issuing a fresh XEP-0030 query.
pub struct CapsPresence
{
    node: String,
    ver: String,
}

impl CapsPresence
{
    pub fn new(node: String, ver: String) -> Self
    {
        return Self { node, ver };
    }
}

impl Stanza for CapsPresence
{
    fn to_xml(&self) -> String
    {
        return format!("<presence>{}</presence>", caps_element(&self.node, &self.ver));
    }
}

/// The XEP-0115 `<c/>` capabilities element carried in an incoming presence
/// stanza.
///
/// `hash` names the algorithm used to build `ver` (absent in the legacy pre-1.4
/// format); `node` identifies the peer's client software and `ver` is the
/// verification hash. Together `node` and `ver` form the disco node
/// (`node#ver`) a receiver can query to learn the peer's features.
#[derive(Deserialize, Debug)]
#[serde(rename = "c")]
pub struct IncomingCaps
{
    #[serde(rename = "@hash", default)]
    pub hash: Option<String>,
    #[serde(rename = "@node")]
    pub node: String,
    #[serde(rename = "@ver")]
    pub ver: String,
}

/// A parsed incoming presence, viewed only for an optional XEP-0115 `<c/>`
/// capabilities child.
///
/// Reads the unqualified sender and a direct, correctly namespaced `<c/>`.
/// Other presence children remain available to the MUC layer independently.
#[derive(Debug)]
pub struct PresenceCaps
{
    pub from: Option<String>,
    caps: Option<IncomingCaps>,
}

impl PresenceCaps
{
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        use quick_xml::{events::Event, name::ResolveResult, NsReader};
        let attributes = super::xml::element(xml, b"presence", b"jabber:client")?;
        let from = attributes.into_iter().find(|(name, _)| name == "from").map(|(_, value)| value);
        let normalized_xml = crate::stanza::xml::normalize_xml(xml)?;
        let mut reader = NsReader::from_str(&normalized_xml);
        reader.config_mut().expand_empty_elements = true;
        let mut depth = 0usize;
        let mut caps = None;
        let mut in_caps = false;
        loop {
            match reader.read_resolved_event().map_err(|e| e.to_string())? {
                (ns, Event::Start(e)) => {
                    if in_caps { return Err("Capabilities element must be empty".into()); }
                    if depth == 1 && e.local_name().as_ref() == b"c"
                        && matches!(ns, ResolveResult::Bound(ns) if ns.as_ref() == CAPS_NS.as_bytes()) {
                        if caps.is_some() { return Err("Duplicate capabilities element".into()); }
                        let (mut node, mut ver, mut hash) = (None, None, None);
                        for attr in e.attributes() {
                            let attr = attr.map_err(|e| e.to_string())?;
                            let target = match attr.key.as_ref() {
                                b"node" => &mut node,
                                b"ver" => &mut ver,
                                b"hash" => &mut hash,
                                _ => continue,
                            };
                            *target = Some(attr.unescape_value().map_err(|e| e.to_string())?.into_owned());
                        }
                        let node = node.filter(|value| !value.is_empty()).ok_or("Missing capabilities node")?;
                        let ver = ver.filter(|value| !value.is_empty()).ok_or("Missing capabilities verification string")?;
                        caps = Some(IncomingCaps { node, ver, hash });
                        in_caps = true;
                    }
                    depth += 1;
                }
                (_, Event::End(_)) => {
                    depth -= 1; // The complete document was validated above.
                    if depth == 1 { in_caps = false; }
                }
                (_, Event::Text(e)) if in_caps && !e.is_empty() => return Err("Capabilities element must be empty".into()),
                (_, Event::CData(_) | Event::GeneralRef(_)) if in_caps => return Err("Capabilities element must be empty".into()),
                (_, Event::Eof) => return Ok(Self { from, caps }),
                _ => {},
            }
        }
    }

    /// The `<c/>` capabilities element, if the presence carried one.
    pub fn caps(&self) -> Option<&IncomingCaps> { self.caps.as_ref() }
}

#[cfg(test)]
mod tests
{
    use super::*;
    use crate::DiscoIdentity;

    /// The canonical XEP-0115 §5.2 "simple" example, whose published `ver` is
    /// `QgayPKawpkPSDYmwT/WM94uAlu0=`. Matching it proves the string-building and
    /// hashing follow the specification.
    #[test]
    fn canonical_verification_string_and_ver()
    {
        let info = DiscoInfo
        {
            identities: vec![DiscoIdentity
            {
                category: "client".to_string(),
                kind: "pc".to_string(),
                name: Some("Exodus 0.9.1".to_string()),
            }],
            features: vec![
                "http://jabber.org/protocol/muc".to_string(),
                "http://jabber.org/protocol/disco#items".to_string(),
                "http://jabber.org/protocol/caps".to_string(),
                "http://jabber.org/protocol/disco#info".to_string(),
            ],
        };

        assert_eq!(
            verification_string(&info),
            "client/pc//Exodus 0.9.1<\
             http://jabber.org/protocol/caps<\
             http://jabber.org/protocol/disco#info<\
             http://jabber.org/protocol/disco#items<\
             http://jabber.org/protocol/muc<"
        );
        assert_eq!(compute_ver(&info), "QgayPKawpkPSDYmwT/WM94uAlu0=");
    }

    #[test]
    fn verification_string_omits_empty_identity_name()
    {
        let info = DiscoInfo
        {
            identities: vec![DiscoIdentity
            {
                category: "client".to_string(),
                kind: "bot".to_string(),
                name: None,
            }],
            features: vec!["urn:xmpp:ping".to_string()],
        };

        assert_eq!(verification_string(&info), "client/bot//<urn:xmpp:ping<");
    }

    #[test]
    fn caps_presence_to_xml()
    {
        let presence = CapsPresence::new(
            "https://github.com/mikaelsahlstrom/libxmpp".to_string(),
            "QgayPKawpkPSDYmwT/WM94uAlu0=".to_string(),
        );

        assert_eq!(
            presence.to_xml(),
            "<presence><c xmlns='http://jabber.org/protocol/caps' hash='sha-1' \
             node='https://github.com/mikaelsahlstrom/libxmpp' \
             ver='QgayPKawpkPSDYmwT/WM94uAlu0='/></presence>"
        );
    }

    #[test]
    fn parse_presence_with_caps()
    {
        let xml = "<presence xmlns='jabber:client' from='romeo@montague.lit/orchard'>\
            <c xmlns='http://jabber.org/protocol/caps' hash='sha-1' \
            node='http://code.google.com/p/exodus' ver='QgayPKawpkPSDYmwT/WM94uAlu0='/>\
            </presence>";

        let presence = PresenceCaps::from_xml(xml).unwrap();
        assert_eq!(presence.from.as_deref(), Some("romeo@montague.lit/orchard"));

        let caps = presence.caps().unwrap();
        assert_eq!(caps.hash.as_deref(), Some("sha-1"));
        assert_eq!(caps.node, "http://code.google.com/p/exodus");
        assert_eq!(caps.ver, "QgayPKawpkPSDYmwT/WM94uAlu0=");
    }

    #[test]
    fn capabilities_use_expanded_names_and_unqualified_identity_attributes() {
        let xml = "<p:presence xmlns:p='jabber:client' xmlns:c='http://jabber.org/protocol/caps' xmlns:e='extension' from='real@example/device' e:from='forged'><e:c node='bad' ver='bad'/><c:c node='real&amp;node' ver='hash' hash='sha-1' e:node='forged'/></p:presence>";
        let presence = PresenceCaps::from_xml(xml).unwrap();
        assert_eq!(presence.from.as_deref(), Some("real@example/device"));
        assert_eq!(presence.caps().unwrap().node, "real&node");
        for payload in ["<c node='bad' ver='bad'/>", "<c xmlns='extension' node='bad' ver='bad'/>",
            "<extension><c xmlns='http://jabber.org/protocol/caps' node='nested' ver='bad'/></extension>"] {
            let xml = format!("<presence xmlns='jabber:client'>{payload}</presence>");
            assert!(PresenceCaps::from_xml(&xml).unwrap().caps().is_none(), "{xml}");
        }
        let presence = PresenceCaps::from_xml("<presence xmlns='jabber:client' xmlns:e='extension' e:from='forged'><c xmlns='http://jabber.org/protocol/caps' node='legacy' ver='version'/></presence>").unwrap();
        assert!(presence.from.is_none());
        assert!(presence.caps().unwrap().hash.is_none());
    }

    #[test]
    fn capabilities_reject_ambiguous_or_nonempty_payloads() {
        for payload in [
            "<c node='one' ver='v'/><c node='two' ver='v'/>",
            "<c node='one' ver='v'>text</c>", "<c node='one' ver='v'><child/></c>",
            "<c node='one' ver='v'><![CDATA[text]]></c>", "<c node='one' ver='v'>&amp;</c>",
            "<c xmlns:e='extension' e:node='forged' ver='v'/>", "<c node='one'/>", "<c node='' ver='v'/>",
        ] {
            let xml = format!("<p:presence xmlns:p='jabber:client' xmlns='http://jabber.org/protocol/caps'>{payload}</p:presence>");
            assert!(PresenceCaps::from_xml(&xml).is_err(), "{xml}");
        }
        for xml in ["<presence xmlns='extension'/>", "<presence xmlns='jabber:client'/><extra/>", "<presence xmlns='jabber:client'>"] {
            assert!(PresenceCaps::from_xml(xml).is_err());
        }
    }

    #[test]
    fn parse_presence_without_caps()
    {
        // A plain presence (or one carrying only other extensions) yields no caps.
        let xml = "<presence xmlns='jabber:client' from='a@b/c'><show>away</show></presence>";
        let presence = PresenceCaps::from_xml(xml).unwrap();
        assert!(presence.caps().is_none());
    }
}
