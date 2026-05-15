//! Parsers and serializers for OMEMO (legacy axolotl) stanzas.
//!
//! Targets XEP-0384 v0.3 (`eu.siacs.conversations.axolotl` namespace) for
//! maximum interop with existing clients (Conversations, Dino, Gajim,
//! Movim, etc.). The OMEMO 2 namespace (`urn:xmpp:omemo:2`) is similar but
//! not handled here.

use super::Stanza;

pub const OMEMO_NS: &str = "eu.siacs.conversations.axolotl";
pub const OMEMO_DEVICELIST_NODE: &str = "eu.siacs.conversations.axolotl.devicelist";
pub const OMEMO_BUNDLE_NODE_PREFIX: &str = "eu.siacs.conversations.axolotl.bundles:";

/// A single `<key rid='..' [prekey='true']>BASE64</key>` element inside
/// `<header>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmemoKey
{
    pub rid: u32,
    pub prekey: bool,
    pub data_b64: String,
}

/// Parsed OMEMO message ciphertext.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmemoEncrypted
{
    /// Sender device id.
    pub sid: u32,
    /// Per-recipient-device wrapped keys.
    pub keys: Vec<OmemoKey>,
    /// AES-GCM IV (12 or 16 bytes), base64.
    pub iv_b64: String,
    /// Encrypted payload, base64. May be absent for "key-only" OMEMO
    /// messages used during session setup.
    pub payload_b64: Option<String>,
}

impl OmemoEncrypted
{
    /// Build the `<encrypted>` child element XML.
    pub fn to_xml(&self) -> String
    {
        let mut keys_xml = String::new();
        for k in &self.keys
        {
            let prekey_attr = if k.prekey { " prekey='true'" } else { "" };
            keys_xml.push_str(&format!(
                "<key rid='{}'{}>{}</key>",
                k.rid, prekey_attr, k.data_b64
            ));
        }

        let payload_xml = match &self.payload_b64
        {
            Some(p) => format!("<payload>{}</payload>", p),
            None => String::new(),
        };

        return format!(
            "<encrypted xmlns='{}'>\
                <header sid='{}'>\
                    {}\
                    <iv>{}</iv>\
                </header>\
                {}\
            </encrypted>",
            OMEMO_NS, self.sid, keys_xml, self.iv_b64, payload_xml
        );
    }

    /// Extract an `OmemoEncrypted` from raw XML containing an `<encrypted>`
    /// element. Pure-string parsing rather than going through a DOM keeps
    /// the dependency surface small and matches the rest of the crate.
    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        let enc_start = find_tag_start(xml, "encrypted")
            .ok_or_else(|| "no <encrypted> element".to_string())?;
        let header_start = find_tag_start(&xml[enc_start..], "header")
            .ok_or_else(|| "no <header>".to_string())? + enc_start;

        let sid = read_u32_attr(&xml[header_start..], "sid")?;

        let header_end = find_close_tag(&xml[header_start..], "header")
            .ok_or_else(|| "unterminated <header>".to_string())? + header_start;
        let header_body = &xml[header_start..header_end];

        let mut keys = Vec::new();
        let mut pos = 0;
        while let Some(off) = find_tag_start(&header_body[pos..], "key")
        {
            let kstart = pos + off;
            let key_open_end = header_body[kstart..].find('>')
                .ok_or_else(|| "unterminated <key>".to_string())? + kstart + 1;
            let key_close = header_body[key_open_end..].find("</key>")
                .ok_or_else(|| "missing </key>".to_string())? + key_open_end;
            let rid = read_u32_attr(&header_body[kstart..key_open_end], "rid")?;
            let prekey = read_bool_attr(&header_body[kstart..key_open_end], "prekey");
            let data = header_body[key_open_end..key_close].trim().to_string();
            keys.push(OmemoKey { rid, prekey, data_b64: data });
            pos = key_close + "</key>".len();
        }

        let iv_open = find_tag_start(header_body, "iv")
            .ok_or_else(|| "no <iv>".to_string())?;
        let iv_open_end = header_body[iv_open..].find('>')
            .ok_or_else(|| "unterminated <iv>".to_string())? + iv_open + 1;
        let iv_close = header_body[iv_open_end..].find("</iv>")
            .ok_or_else(|| "missing </iv>".to_string())? + iv_open_end;
        let iv_b64 = header_body[iv_open_end..iv_close].trim().to_string();

        let after_header = &xml[header_end + "</header>".len()..];
        let payload_b64 = if let Some(p_open) = find_tag_start(after_header, "payload")
        {
            let p_open_end = after_header[p_open..].find('>')
                .ok_or_else(|| "unterminated <payload>".to_string())? + p_open + 1;
            let p_close = after_header[p_open_end..].find("</payload>")
                .ok_or_else(|| "missing </payload>".to_string())? + p_open_end;
            Some(after_header[p_open_end..p_close].trim().to_string())
        }
        else
        {
            None
        };

        return Ok(Self { sid, keys, iv_b64, payload_b64 });
    }
}

/// Outgoing `<message type='chat'>` carrying an OMEMO payload. Includes the
/// standard hint elements expected by other clients (XEP-0334 store hint,
/// XEP-0380 explicit message encryption).
pub struct OmemoChatMessage
{
    pub to: String,
    pub body_hint: Option<String>,
    pub encrypted: OmemoEncrypted,
}

impl Stanza for OmemoChatMessage
{
    fn to_xml(&self) -> String
    {
        let hint = match &self.body_hint
        {
            Some(b) => format!("<body>{}</body>", quick_xml::escape::escape(b)),
            None => String::new(),
        };

        return format!(
            "<message to='{}' type='chat'>\
                {encrypted}\
                {hint}\
                <store xmlns='urn:xmpp:hints'/>\
                <encryption xmlns='urn:xmpp:eme:0' namespace='{ns}' name='OMEMO'/>\
            </message>",
            self.to,
            encrypted = self.encrypted.to_xml(),
            hint = hint,
            ns = OMEMO_NS,
        );
    }
}

/// `<list xmlns='eu.siacs.conversations.axolotl'><device id='..'/>...</list>`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceList
{
    pub devices: Vec<u32>,
}

impl DeviceList
{
    pub fn to_xml(&self) -> String
    {
        let mut s = format!("<list xmlns='{}'>", OMEMO_NS);
        for d in &self.devices
        {
            s.push_str(&format!("<device id='{}'/>", d));
        }
        s.push_str("</list>");
        return s;
    }

    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        let start = find_tag_start(xml, "list")
            .ok_or_else(|| "no <list>".to_string())?;
        let open_end = xml[start..].find('>')
            .ok_or_else(|| "unterminated <list>".to_string())? + start + 1;
        let end = xml[open_end..].find("</list>")
            .ok_or_else(|| "missing </list>".to_string())? + open_end;
        let body = &xml[open_end..end];

        let mut devices = Vec::new();
        let mut pos = 0;
        while let Some(off) = find_tag_start(&body[pos..], "device")
        {
            let dstart = pos + off;
            let dend = body[dstart..].find('>')
                .ok_or_else(|| "unterminated <device>".to_string())? + dstart + 1;
            let id = read_u32_attr(&body[dstart..dend], "id")?;
            devices.push(id);
            pos = dend;
        }

        return Ok(Self { devices });
    }
}

/// `<bundle xmlns='..'>`. Contains identity key + signed prekey + one-time
/// prekeys for a single device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle
{
    pub signed_pre_key_id: u32,
    pub signed_pre_key_public_b64: String,
    pub signed_pre_key_signature_b64: String,
    pub identity_key_b64: String,
    pub pre_keys: Vec<PreKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreKey
{
    pub id: u32,
    pub public_b64: String,
}

impl Bundle
{
    pub fn to_xml(&self) -> String
    {
        let mut prekeys_xml = String::from("<prekeys>");
        for p in &self.pre_keys
        {
            prekeys_xml.push_str(&format!(
                "<preKeyPublic preKeyId='{}'>{}</preKeyPublic>",
                p.id, p.public_b64
            ));
        }
        prekeys_xml.push_str("</prekeys>");

        return format!(
            "<bundle xmlns='{}'>\
                <signedPreKeyPublic signedPreKeyId='{}'>{}</signedPreKeyPublic>\
                <signedPreKeySignature>{}</signedPreKeySignature>\
                <identityKey>{}</identityKey>\
                {}\
            </bundle>",
            OMEMO_NS,
            self.signed_pre_key_id, self.signed_pre_key_public_b64,
            self.signed_pre_key_signature_b64,
            self.identity_key_b64,
            prekeys_xml,
        );
    }

    pub fn from_xml(xml: &str) -> Result<Self, String>
    {
        let start = find_tag_start(xml, "bundle")
            .ok_or_else(|| "no <bundle>".to_string())?;
        let open_end = xml[start..].find('>')
            .ok_or_else(|| "unterminated <bundle>".to_string())? + start + 1;
        let end = xml[open_end..].find("</bundle>")
            .ok_or_else(|| "missing </bundle>".to_string())? + open_end;
        let body = &xml[open_end..end];

        let (spk_pub_b64, spk_id) =
        {
            let s = find_tag_start(body, "signedPreKeyPublic")
                .ok_or_else(|| "no <signedPreKeyPublic>".to_string())?;
            let e_open = body[s..].find('>')
                .ok_or_else(|| "unterminated <signedPreKeyPublic>".to_string())? + s + 1;
            let id = read_u32_attr(&body[s..e_open], "signedPreKeyId")?;
            let close = body[e_open..].find("</signedPreKeyPublic>")
                .ok_or_else(|| "missing </signedPreKeyPublic>".to_string())? + e_open;
            (body[e_open..close].trim().to_string(), id)
        };

        let spk_sig_b64 = extract_text(body, "signedPreKeySignature")
            .ok_or_else(|| "no <signedPreKeySignature>".to_string())?;
        let identity_key_b64 = extract_text(body, "identityKey")
            .ok_or_else(|| "no <identityKey>".to_string())?;

        let mut pre_keys = Vec::new();
        let mut pos = 0;
        while let Some(off) = find_tag_start(&body[pos..], "preKeyPublic")
        {
            let s = pos + off;
            let e_open = body[s..].find('>')
                .ok_or_else(|| "unterminated <preKeyPublic>".to_string())? + s + 1;
            let id = read_u32_attr(&body[s..e_open], "preKeyId")?;
            let close = body[e_open..].find("</preKeyPublic>")
                .ok_or_else(|| "missing </preKeyPublic>".to_string())? + e_open;
            pre_keys.push(PreKey
            {
                id,
                public_b64: body[e_open..close].trim().to_string(),
            });
            pos = close + "</preKeyPublic>".len();
        }

        return Ok(Self
        {
            signed_pre_key_id: spk_id,
            signed_pre_key_public_b64: spk_pub_b64,
            signed_pre_key_signature_b64: spk_sig_b64,
            identity_key_b64,
            pre_keys,
        });
    }
}

fn find_tag_start(xml: &str, tag: &str) -> Option<usize>
{
    // Match either "<tag " or "<tag>" or "<tag/" (open or self-close)
    let bytes = xml.as_bytes();
    let needle = format!("<{}", tag);
    let mut from = 0;

    while let Some(rel) = xml[from..].find(&needle)
    {
        let pos = from + rel;
        let next = bytes.get(pos + needle.len()).copied().unwrap_or(0);

        if matches!(next, b' ' | b'>' | b'/' | b'\t' | b'\n' | b'\r')
        {
            return Some(pos);
        }

        from = pos + needle.len();
    }

    return None;
}

fn find_close_tag(xml: &str, tag: &str) -> Option<usize>
{
    let needle = format!("</{}>", tag);
    return xml.find(&needle);
}

fn read_u32_attr(open_tag: &str, name: &str) -> Result<u32, String>
{
    let value = read_attr(open_tag, name)
        .ok_or_else(|| format!("attribute '{}' missing", name))?;
    return value.parse().map_err(|e: std::num::ParseIntError| e.to_string());
}

fn read_bool_attr(open_tag: &str, name: &str) -> bool
{
    return matches!(
        read_attr(open_tag, name).as_deref(),
        Some("1") | Some("true") | Some("True") | Some("TRUE"),
    );
}

fn read_attr(open_tag: &str, name: &str) -> Option<String>
{
    let mut from = 0;
    while let Some(rel) = open_tag[from..].find(name)
    {
        let pos = from + rel;
        // Ensure preceded by whitespace / tag start
        let prev = pos.checked_sub(1).and_then(|p| open_tag.as_bytes().get(p)).copied().unwrap_or(0);
        if matches!(prev, b' ' | b'\t' | b'\n' | b'\r' | b'<')
        {
            let after = pos + name.len();
            let bytes = open_tag.as_bytes();
            if bytes.get(after).copied() == Some(b'=')
            {
                let quote = bytes.get(after + 1).copied()?;
                if quote == b'\'' || quote == b'"'
                {
                    let value_start = after + 2;
                    let value_end = open_tag[value_start..].find(quote as char)? + value_start;
                    return Some(open_tag[value_start..value_end].to_string());
                }
            }
        }

        from = pos + name.len();
    }
    return None;
}

fn extract_text(xml: &str, tag: &str) -> Option<String>
{
    let s = find_tag_start(xml, tag)?;
    let open_end = xml[s..].find('>')? + s + 1;
    let needle = format!("</{}>", tag);
    let close = xml[open_end..].find(&needle)? + open_end;
    return Some(xml[open_end..close].trim().to_string());
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn roundtrip_device_list()
    {
        let dl = DeviceList { devices: vec![111, 222, 333] };
        let xml = dl.to_xml();
        let dl2 = DeviceList::from_xml(&xml).unwrap();
        assert_eq!(dl, dl2);
    }

    #[test]
    fn parse_empty_device_list()
    {
        let dl = DeviceList::from_xml(
            "<list xmlns='eu.siacs.conversations.axolotl'></list>",
        ).unwrap();
        assert!(dl.devices.is_empty());
    }

    #[test]
    fn parse_encrypted_with_payload()
    {
        let xml = "<encrypted xmlns='eu.siacs.conversations.axolotl'>\
            <header sid='1234'>\
                <key rid='10' prekey='true'>QUJD</key>\
                <key rid='20'>WFla</key>\
                <iv>SVZJVklWSVY=</iv>\
            </header>\
            <payload>UEFZTE9BRA==</payload>\
        </encrypted>";

        let e = OmemoEncrypted::from_xml(xml).unwrap();
        assert_eq!(e.sid, 1234);
        assert_eq!(e.keys.len(), 2);
        assert_eq!(e.keys[0].rid, 10);
        assert!(e.keys[0].prekey);
        assert_eq!(e.keys[0].data_b64, "QUJD");
        assert_eq!(e.keys[1].rid, 20);
        assert!(!e.keys[1].prekey);
        assert_eq!(e.iv_b64, "SVZJVklWSVY=");
        assert_eq!(e.payload_b64.as_deref(), Some("UEFZTE9BRA=="));
    }

    #[test]
    fn parse_encrypted_without_payload()
    {
        let xml = "<encrypted xmlns='eu.siacs.conversations.axolotl'>\
            <header sid='1'>\
                <key rid='2' prekey='true'>K</key>\
                <iv>I</iv>\
            </header>\
        </encrypted>";
        let e = OmemoEncrypted::from_xml(xml).unwrap();
        assert!(e.payload_b64.is_none());
    }

    #[test]
    fn roundtrip_encrypted()
    {
        let e = OmemoEncrypted
        {
            sid: 42,
            keys: vec![
                OmemoKey { rid: 1, prekey: false, data_b64: "AA==".into() },
                OmemoKey { rid: 2, prekey: true, data_b64: "BB==".into() },
            ],
            iv_b64: "CC==".into(),
            payload_b64: Some("DD==".into()),
        };
        let xml = e.to_xml();
        let parsed = OmemoEncrypted::from_xml(&xml).unwrap();
        assert_eq!(e, parsed);
    }

    #[test]
    fn roundtrip_bundle()
    {
        let b = Bundle
        {
            signed_pre_key_id: 7,
            signed_pre_key_public_b64: "SPK".into(),
            signed_pre_key_signature_b64: "SIG".into(),
            identity_key_b64: "IK".into(),
            pre_keys: vec![
                PreKey { id: 1, public_b64: "P1".into() },
                PreKey { id: 2, public_b64: "P2".into() },
            ],
        };
        let xml = b.to_xml();
        let parsed = Bundle::from_xml(&xml).unwrap();
        assert_eq!(b, parsed);
    }

    #[test]
    fn omemo_chat_message_includes_hints()
    {
        let m = OmemoChatMessage
        {
            to: "bob@example.com".into(),
            body_hint: Some("OMEMO".into()),
            encrypted: OmemoEncrypted
            {
                sid: 1,
                keys: vec![OmemoKey { rid: 2, prekey: false, data_b64: "A".into() }],
                iv_b64: "I".into(),
                payload_b64: Some("P".into()),
            },
        };
        let xml = m.to_xml();
        assert!(xml.contains("<store xmlns='urn:xmpp:hints'/>"));
        assert!(xml.contains("urn:xmpp:eme:0"));
        assert!(xml.contains("type='chat'"));
        assert!(xml.contains("to='bob@example.com'"));
    }
}
