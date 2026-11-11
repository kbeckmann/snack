use base64::{ Engine, engine::general_purpose::STANDARD as BASE64 };
use hmac::{ Hmac, Mac, EagerHash };
use hmac::digest::KeyInit;
use sha1::Sha1;
use sha2::{ Sha256, Sha512 };
use quick_xml::{NsReader, name::ResolveResult};
use quick_xml::events::Event;
use std::marker::PhantomData;

fn hmac_compute<D>(key: &[u8], data: &[u8]) -> Vec<u8>
where
    D: EagerHash,
{
    let mut mac = Hmac::<D>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);

    return mac.finalize().into_bytes().to_vec();
}

// A hostile server controls this value. Bound synchronous password derivation so
// a challenge cannot monopolize the connection's runtime indefinitely.
const MAX_SCRAM_ITERATIONS: u32 = 1_000_000;

fn scram_attributes(message: &str) -> Result<Vec<(u8, &str)>, String>
{
    let mut attributes = Vec::new();
    for part in message.split(',')
    {
        let bytes = part.as_bytes();
        if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b'='
            || bytes[2..].iter().any(|b| *b == 0 || *b >= 0x80)
        {
            return Err("Invalid SCRAM attribute".to_string());
        }
        if bytes[0] == b'm'
        {
            return Err("Unsupported mandatory SCRAM extension".to_string());
        }
        if attributes.iter().any(|(key, _)| *key == bytes[0])
        {
            return Err("Duplicate SCRAM attribute".to_string());
        }
        attributes.push((bytes[0], &part[2..]));
    }
    return Ok(attributes);
}

fn pbkdf2<D>(password: &[u8], salt: &[u8], iterations: u32) -> Vec<u8>
where
    D: EagerHash,
{
    let mut salt_block = salt.to_vec();
    salt_block.extend_from_slice(&1u32.to_be_bytes());

    let mut u = hmac_compute::<D>(password, &salt_block);
    let mut result = u.clone();

    for _ in 1..iterations
    {
        u = hmac_compute::<D>(password, &u);
        for (r, b) in result.iter_mut().zip(u.iter())
        {
            *r ^= b;
        }
    }

    return result;
}

pub trait ScramAuth: Send
{
    fn auth_xml(&mut self) -> String;
    fn response_xml(&mut self, challenge_b64: &str) -> Result<String, String>;
    fn verify_success(&self, success_b64: &str) -> Result<(), String>;
}

pub struct ScramClient<D>
{
    username: String,
    password: String,
    nonce: String,
    client_first_bare: String,
    salted_password: Vec<u8>,
    auth_message: String,
    mechanism_name: &'static str,
    _marker: PhantomData<D>,
}

// Uniformly random letters and digits from the OS generator. Rejecting
// bytes outside 0..248 keeps every one of the 62 symbols equally likely.
pub(crate) fn random_alphanumeric(length: usize) -> String
{
    const ALPHABET: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(length);
    let mut bytes = [0u8; 64];
    while out.len() < length {
        getrandom::fill(&mut bytes).expect("the operating system random number generator failed");
        for byte in bytes {
            if byte < 248 && out.len() < length { out.push(ALPHABET[usize::from(byte % 62)] as char); }
        }
    }
    out
}

impl<D> ScramClient<D>
where
    D: EagerHash,
{
    pub fn new(username: &str, password: &str, mechanism_name: &'static str) -> Self
    {
        let nonce = random_alphanumeric(24);

        return Self
        {
            username: username.to_string(),
            password: password.to_string(),
            nonce,
            client_first_bare: String::new(),
            salted_password: Vec::new(),
            auth_message: String::new(),
            mechanism_name,
            _marker: PhantomData,
        };
    }

    fn client_first(&mut self) -> String
    {
        self.salted_password.clear();
        self.auth_message.clear();
        let username = self.username.replace('=', "=3D").replace(',', "=2C");
        self.client_first_bare = format!("n={},r={}", username, self.nonce);
        return format!("n,,{}", self.client_first_bare);
    }

    fn client_final(&mut self, server_first: &str) -> Result<String, String>
    {
        self.salted_password.clear();
        self.auth_message.clear();
        if self.client_first_bare.is_empty()
        {
            return Err("SCRAM challenge received before client first message".to_string());
        }
        let attributes = scram_attributes(server_first)?;
        let [(b'r', server_nonce), (b's', salt_b64), (b'i', count), ..] = attributes.as_slice()
            else { return Err("Missing or misplaced SCRAM challenge attributes".to_string()); };
        if !server_nonce.starts_with(&self.nonce) || server_nonce.len() <= self.nonce.len()
            || !server_nonce.bytes().all(|b| (0x21..=0x7e).contains(&b))
        {
            return Err("Server nonce must extend the client nonce".to_string());
        }
        if !count.bytes().all(|b| b.is_ascii_digit()) || count.starts_with('0')
        {
            return Err("Invalid SCRAM iteration count".to_string());
        }
        let iterations: u32 = count.parse().map_err(|_| "Invalid SCRAM iteration count".to_string())?;
        if iterations > MAX_SCRAM_ITERATIONS
        {
            return Err("SCRAM iteration count exceeds client limit".to_string());
        }
        let salt = BASE64.decode(salt_b64).map_err(|e| e.to_string())?;

        // SCRAM hashes the SASLprep stored-string form (RFC5802 2.2), not
        // the original UTF-8 bytes. Keep the original credential for retries.
        // Do not include preparation diagnostics: they can expose its contents.
        let password = super::saslprep::password(&self.password)
            .map_err(|_| "SCRAM password cannot be prepared for authentication".to_string())?;
        self.salted_password = pbkdf2::<D>(password.as_bytes(), &salt, iterations);

        let client_key = hmac_compute::<D>(&self.salted_password, b"Client Key");
        let stored_key = D::digest(&client_key).to_vec();

        let client_final_without_proof = format!("c=biws,r={}", server_nonce);

        self.auth_message = format!(
            "{},{},{}", self.client_first_bare, server_first, client_final_without_proof
        );

        let client_signature = hmac_compute::<D>(&stored_key, self.auth_message.as_bytes());

        let client_proof: Vec<u8> = client_key.iter()
            .zip(client_signature.iter())
            .map(|(k, s)| k ^ s)
            .collect();

        return Ok(format!("{},p={}", client_final_without_proof, BASE64.encode(&client_proof)));
    }

    fn verify_server(&self, server_final: &str) -> Result<(), String>
    {
        if self.salted_password.is_empty() || self.auth_message.is_empty()
        {
            return Err("SCRAM success received before a valid challenge".to_string());
        }
        let attributes = scram_attributes(server_final)?;
        let Some((b'v', sig_b64)) = attributes.first()
            else { return Err("Invalid SCRAM server final message".to_string()); };
        if attributes.iter().any(|(key, _)| *key == b'e')
        {
            return Err("Conflicting SCRAM server final attributes".to_string());
        }
        let server_sig = BASE64.decode(sig_b64).map_err(|e| e.to_string())?;
        let server_key = hmac_compute::<D>(&self.salted_password, b"Server Key");
        let mut verifier = Hmac::<D>::new_from_slice(&server_key)
            .expect("HMAC accepts any key length");
        verifier.update(self.auth_message.as_bytes());
        verifier.verify_slice(&server_sig)
            .map_err(|_| "Server signature verification failed".to_string())?;

        return Ok(());
    }
}

impl<D> ScramAuth for ScramClient<D>
where
    D: EagerHash + Send,
{
    fn auth_xml(&mut self) -> String
    {
        let client_first = self.client_first();

        return format!(
            "<auth xmlns='urn:ietf:params:xml:ns:xmpp-sasl' mechanism='{}'>{}</auth>",
            self.mechanism_name,
            BASE64.encode(client_first.as_bytes())
        );
    }

    fn response_xml(&mut self, challenge_b64: &str) -> Result<String, String>
    {
        let challenge_bytes = BASE64.decode(challenge_b64).map_err(|e| e.to_string())?;
        let server_first = String::from_utf8(challenge_bytes).map_err(|e| e.to_string())?;
        let client_final = self.client_final(&server_first)?;

        return Ok(format!(
            "<response xmlns='urn:ietf:params:xml:ns:xmpp-sasl'>{}</response>",
            BASE64.encode(client_final.as_bytes())
        ));
    }

    fn verify_success(&self, success_b64: &str) -> Result<(), String>
    {
        let success_bytes = BASE64.decode(success_b64).map_err(|e| e.to_string())?;
        let server_final = String::from_utf8(success_bytes).map_err(|e| e.to_string())?;

        return self.verify_server(&server_final);
    }
}

pub type ScramSha1Client = ScramClient<Sha1>;
pub type ScramSha256Client = ScramClient<Sha256>;
pub type ScramSha512Client = ScramClient<Sha512>;

#[cfg(test)]
pub(crate) fn rfc5802_client() -> ScramSha1Client {
    let mut client = ScramSha1Client::new("user", "pencil", "SCRAM-SHA-1");
    client.nonce = "fyko+d2lbbFgONRv9qkxdawL".into();
    client
}

pub struct PlainAuth
{
    username: String,
    password: String,
}

impl PlainAuth
{
    pub fn new(username: &str, password: &str) -> Self
    {
        Self
        {
            username: username.to_string(),
            password: password.to_string(),
        }
    }

    pub fn auth_xml(&self) -> String
    {
        let payload = format!("\0{}\0{}", self.username, self.password);
        return format!(
            "<auth xmlns='urn:ietf:params:xml:ns:xmpp-sasl' mechanism='PLAIN'>{}</auth>",
            BASE64.encode(payload.as_bytes())
        );
    }
}

/// Extract the text content of a `<challenge>` element.
pub fn parse_challenge(xml: &str) -> Result<String, String>
{
    return extract_element_text(xml, "challenge");
}

/// Extract the text content of a `<success>` element.
pub fn parse_success(xml: &str) -> Result<String, String>
{
    return extract_element_text(xml, "success");
}

const SASL_NAMESPACE: &[u8] = b"urn:ietf:params:xml:ns:xmpp-sasl";

/// Whether the root is a SASL failure, including namespace-prefixed responses.
pub fn is_failure(xml: &str) -> bool
{
    super::xml::is_element(xml, b"failure", SASL_NAMESPACE)
}

// Authentication outcomes must be the expected root in the SASL namespace.
// Consume the complete element rather than returning after its first text chunk:
// nested markup, malformed endings and unrelated trailing elements are errors.
fn extract_element_text(xml: &str, element: &str) -> Result<String, String>
{
    super::xml::element(xml, element.as_bytes(), SASL_NAMESPACE)?;
    let normalized_xml = crate::stanza::xml::normalize_xml(xml)?;
    let mut reader = NsReader::from_str(&normalized_xml);
    reader.config_mut().expand_empty_elements = true;
    let mut seen_root = false;
    let mut in_element = false;
    let mut text = String::new();
    loop
    {
        match reader.read_resolved_event().map_err(|e| e.to_string())?
        {
            (ns, Event::Start(e)) if !seen_root =>
            {
                if e.local_name().as_ref() != element.as_bytes()
                    || !matches!(ns, ResolveResult::Bound(ns) if ns.as_ref() == SASL_NAMESPACE)
                {
                    return Err(format!("Expected SASL <{}> response", element));
                }
                for attr in e.attributes() { attr.map_err(|e| e.to_string())?; }
                seen_root = true;
                in_element = true;
            }
            (_, Event::Text(e)) if in_element =>
                text.push_str(&e.decode().map_err(|e| e.to_string())?),
            (_, Event::CData(e)) if in_element =>
                text.push_str(&e.decode().map_err(|e| e.to_string())?),
            (_, Event::GeneralRef(e)) if in_element =>
            {
                let ch = e.resolve_char_ref().map_err(|e| e.to_string())?
                    .ok_or("Invalid SASL character reference")?;
                text.push(ch);
            }
            (_, Event::End(_)) if in_element => in_element = false,
            (_, Event::Comment(_)) => {},
            (_, Event::Text(e)) if e.as_ref().iter().all(u8::is_ascii_whitespace) => {},
            (_, Event::Eof) if seen_root && !in_element => return Ok(text),
            (_, Event::Eof) => return Err(format!("Incomplete SASL <{}> response", element)),
            _ => return Err(format!("Invalid SASL <{}> response", element)),
        }
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn sasl_payload_rejects_undeclared_attribute_namespaces() {
        for name in ["success", "challenge"] {
            let xml = format!("<{name} xmlns='urn:ietf:params:xml:ns:xmpp-sasl' x:extra='value'>YQ==</{name}>");
            assert!(extract_element_text(&xml, name).is_err(), "{xml}");
        }
    }

    #[test]
    fn sasl_failure_requires_a_complete_valid_element() {
        for xml in [
            "<failure xmlns='urn:ietf:params:xml:ns:xmpp-sasl'>",
            "<failure xmlns='urn:ietf:params:xml:ns:xmpp-sasl'/><extra/>",
            "<failure xmlns='urn:ietf:params:xml:ns:xmpp-sasl' x:extra='value'/>",
            "<failure xmlns='urn:ietf:params:xml:ns:xmpp-sasl'><not-authorized></failure>",
        ] { assert!(!is_failure(xml), "{xml}"); }
    }

    #[test]
    fn standalone_sasl_helpers_share_the_stream_xml_validation() {
        let namespace = "urn:ietf:params:xml:ns:xmpp-sasl";
        for name in ["success", "challenge", "failure"] {
            for (attributes, content) in [
                ("xmlns:x='urn:same' xmlns:y='urn:&#115;ame' x:a='1' y:a='2'", "YQ=="),
                ("xmlns:x='http://www.w3.org/XML/1998/namespace'", "YQ=="),
                ("a='&#0;'", "YQ=="),
                ("", "YQ<!-- split -->=="),
                ("", "<?extension?>YQ=="),
                ("", "<![CDATA[\0]]>"),
                ("", "&#0;"),
            ] {
                let xml = format!("<{name} xmlns='{namespace}' {attributes}>{content}</{name}>");
                let mut framer = crate::xml_framer::XmlFramer::new_opened();
                framer.feed(xml.as_bytes());
                assert!(framer.try_next().is_none(), "stream accepted {xml:?}");
                assert!(framer.error().is_some());
                if name == "failure" { assert!(!is_failure(&xml), "{xml:?}"); }
                else { assert!(extract_element_text(&xml, name).is_err(), "{xml:?}"); }
            }
            let attributes: String = (0..255).map(|i| format!(" a{i}='value'")).collect();
            let valid = format!("<{name} xmlns='{namespace}'{attributes}>YQ==</{name}>");
            let invalid = valid.replacen(" a0=", " extra='value' a0=", 1);
            if name == "failure" {
                assert!(is_failure(&valid));
                assert!(!is_failure(&invalid));
            } else {
                assert_eq!(extract_element_text(&valid, name).unwrap(), "YQ==");
                assert!(extract_element_text(&invalid, name).is_err());
            }
        }
        assert_eq!(parse_challenge(&format!("<s:challenge xmlns:s='{namespace}' xmlns:x='urn:extra' x:note='value'>Y&#81;<![CDATA[==]]></s:challenge>")).unwrap(), "YQ==");
        assert!(is_failure(&format!("<s:failure xmlns:s='{namespace}' xmlns:x='urn:extra' x:note='value'><s:temporary-auth-failure/><x:info>reason</x:info></s:failure>")));
    }

    #[test]
    fn sasl_payload_requires_complete_namespaced_root()
    {
        let ns = "urn:ietf:params:xml:ns:xmpp-sasl";
        assert_eq!(parse_success(&format!("<success xmlns='{ns}'/>")).unwrap(), "");
        assert_eq!(parse_success(&format!("<s:success xmlns:s='{ns}'>YWJj<![CDATA[ZA==]]></s:success>")).unwrap(), "YWJjZA==");
        assert_eq!(parse_challenge(&format!("<challenge xmlns='{ns}'>&#89;Q==</challenge>")).unwrap(), "YQ==");
        assert!(is_failure(&format!("<s:failure xmlns:s='{ns}'><s:not-authorized/></s:failure>")));
        assert!(!is_failure("<message><failure/></message>"));
        assert!(!is_failure("<failure xmlns='unrelated'/>"));
        for invalid in [
            "<success/>",
            "<success xmlns='unrelated'/>",
            "<message><success xmlns='urn:ietf:params:xml:ns:xmpp-sasl'/></message>",
            "<success xmlns='urn:ietf:params:xml:ns:xmpp-sasl'>data",
            "<success xmlns='urn:ietf:params:xml:ns:xmpp-sasl'>data</challenge>",
            "<success xmlns='urn:ietf:params:xml:ns:xmpp-sasl'><nested/></success>",
            "<success xmlns='urn:ietf:params:xml:ns:xmpp-sasl'/><extra/>",
            "<success xmlns='urn:ietf:params:xml:ns:xmpp-sasl'/>trailing",
        ]
        {
            assert!(parse_success(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn escapes_scram_username_delimiters()
    {
        let mut client = ScramSha256Client::new("a=2C,b", "secret", "SCRAM-SHA-256");
        assert_eq!(client.client_first(), format!("n,,n=a=3D2C=2Cb,r={}", client.nonce));
    }

    #[test]
    fn rejects_invalid_challenges_before_deriving_password()
    {
        let mut client = ScramSha256Client::new("user", "secret", "SCRAM-SHA-256");
        client.nonce = "client".into();
        client.client_first();
        for challenge in [
            "r=clientserver,s=c2FsdA==", // Missing iteration count.
            "r=clientserver,i=1", // Missing salt.
            "s=c2FsdA==,r=clientserver,i=1", // Invalid order.
            "r=client,s=c2FsdA==,i=1", // No server nonce.
            "r=other,s=c2FsdA==,i=1",
            "r=client server,s=c2FsdA==,i=1",
            "r=clientserver,s=c2FsdA==,i=0",
            "r=clientserver,s=c2FsdA==,i=01",
            "r=clientserver,s=c2FsdA==,i=+1",
            "r=clientserver,s=c2FsdA==,i=1000001",
            "r=clientserver,s=c2FsdA==,i=4294967295",
            "r=clientserver,s=c2FsdA==,i=1,i=2",
            "r=clientserver,s=c2FsdA==,i=1,r=clientother",
            "r=clientserver,s=not-base64,i=1",
            "m=required,r=clientserver,s=c2FsdA==,i=1",
            "r=clientserver,s=c2FsdA==,i=1,m=required",
            "r=clientserver,s=c2FsdA==,i=1,broken",
        ]
        {
            assert!(client.client_final(challenge).is_err(), "{challenge}");
            assert!(client.salted_password.is_empty());
            assert!(client.auth_message.is_empty());
        }
        assert!(client.client_final("r=clientserver,s=c2FsdA==,i=1,x=optional").is_ok());
        // A subsequent invalid challenge must invalidate any previous proof.
        assert!(client.client_final("r=bad").is_err());
        assert!(client.verify_server("v=AAAA").is_err());
    }

    #[test]
    fn rejects_success_before_challenge_and_after_restart()
    {
        let mut client = ScramSha512Client::new("user", "secret", "SCRAM-SHA-512");
        assert!(client.verify_server("v=AAAA").is_err());
        assert!(client.client_final("r=invalid,s=c2FsdA==,i=1").is_err());
        client.client_first();
        let challenge = format!("r={}server,s=c2FsdA==,i=1", client.nonce);
        client.client_final(&challenge).unwrap();
        client.client_first();
        assert!(client.verify_server("v=AAAA").is_err());
    }

    #[test]
    fn unicode_scram_password_uses_the_rfc4013_prepared_form() {
        let mut client = rfc5802_client();
        client.password = "pen\u{00ad}cil".into();
        client.client_first();
        let final_message = client.client_final("r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,s=QSXCR+Q6sek8bf92,i=4096").unwrap();
        assert_eq!(final_message, "c=biws,r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,p=v0X8v3Bz2T0CJGbJQyF0X+HI4Ts=");
        client.verify_server("v=rmF9pqV8S7suAoZWja4dJRkFsKQ=").unwrap();
        assert_eq!(client.password, "pen\u{00ad}cil", "authentication must not rewrite the stored credential");
    }

    #[test]
    fn scram_uses_unicode32_normalization_corrections() {
        // Independent vectors: Python unicodedata.ucd_3_2_0.normalize('NFKC')
        // followed by hashlib.pbkdf2_hmac/HMAC on the RFC5802 transcript.
        for (password, proof, signature) in [
            ("\u{2f868}", "7ujCCcmaWG7leYwcabAeTfwqAPw=", "5VdIkzqj6DX74OXGTNhPHs+Doio="),
            ("\u{2f874}", "5i7czWY2I9Rb3K89Kjd+S4s9a3M=", "E8rIl1M/Iv6vTYZzGvEKdL/i9tk="),
            ("\u{2f91f}", "5uhGJRmiZSEkK1muUqu6kxCpPDY=", "9NzZfjWnRnRrXBOFzuXDeUhPIls="),
            ("\u{2f95f}", "c3z+xeOU2Y8ETEDoL5IzpfX3z/o=", "w/Dfw2Re1A8Q5xSHgixkOiXZgco="),
            ("\u{2f9bf}", "oZ83HeZ2i+RW3/tMU4QXS6vRgl8=", "a97asm+MBuDdledQyVV6LDRDm5w="),
            ("\u{f951}", "z4O+OtsGEIARN/KURB/lx3WhrlQ=", "l9gLol1trVc6ZKYZyBNt+fhkCWw="),
        ] {
            let mut client = rfc5802_client();
            client.password = password.into();
            client.client_first();
            let result = client.client_final("r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,s=QSXCR+Q6sek8bf92,i=4096").unwrap();
            assert_eq!(result.rsplit_once(",p=").unwrap().1, proof, "{password:?}");
            client.verify_server(&format!("v={signature}")).unwrap();
        }
    }

    #[test]
    fn scram_accepts_valid_unicode32_bidirectional_passwords() {
        let mut client = rfc5802_client();
        client.password = "\u{0627}\u{2801}\u{0628}".into();
        client.client_first();
        let result = client.client_final("r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,s=QSXCR+Q6sek8bf92,i=4096").unwrap();
        // Independent Python Unicode3.2 + hashlib/HMAC reference vector.
        assert_eq!(result.rsplit_once(",p=").unwrap().1, "FQ4nlG6Z6ne8Qwhspv5wNOjtn5Q=");
        client.verify_server("v=0DWwGgWd/OxRN/fz6ZT1d7pk7mM=").unwrap();
    }

    #[test]
    fn scram_rejects_unicode32_lcat_inside_bidirectional_passwords() {
        for middle in ['\u{17b4}', '\u{17b5}', '\u{1885}', '\u{1886}'] {
            let mut client = rfc5802_client();
            client.password = format!("\u{0627}{middle}\u{0628}");
            client.client_first();
            assert!(client.client_final("r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,s=QSXCR+Q6sek8bf92,i=4096").is_err());
            assert!(client.salted_password.is_empty());
        }
    }

    #[test]
    fn all_scram_hashes_prepare_passwords_without_case_folding() {
        fn check<D: EagerHash>() {
            let proof = |password: &str| {
                let mut client = ScramClient::<D>::new("user", password, "test");
                client.nonce = "client".into();
                client.client_first();
                client.client_final("r=clientserver,s=c2FsdA==,i=1").unwrap()
            };
            for (original, prepared) in [
                ("I\u{00ad}X", "IX"), ("\u{00aa}", "a"), ("\u{2168}", "IX"),
                ("a\u{00a0}b", "a b"), ("e\u{0301}", "é"), ("\u{00ad}", ""),
                ("\u{2f868}", "\u{2136a}"), ("\u{2f874}", "\u{5f33}"),
                ("\u{2f91f}", "\u{43ab}"), ("\u{2f95f}", "\u{7aae}"), ("\u{2f9bf}", "\u{4d57}"),
            ] { assert_eq!(proof(original), proof(prepared), "{original:?}"); }
            assert_ne!(proof("USER"), proof("user"));
            assert!(!proof("\u{0627}\u{0628}").is_empty(), "valid bidirectional text remains supported");
        }
        check::<Sha1>();
        check::<Sha256>();
        check::<Sha512>();
    }

    #[test]
    fn invalid_prepared_passwords_invalidate_previous_scram_proofs() {
        let challenge = "r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,s=QSXCR+Q6sek8bf92,i=4096";
        for invalid in ["secret\u{0007}", "secret\u{e000}", "secret\u{0221}", "\u{0627}1", "secret\u{1d2c}"] {
            let mut client = rfc5802_client();
            client.client_first();
            client.client_final(challenge).unwrap();
            client.password = invalid.into();
            let error = client.client_final(challenge).expect_err("invalid preparation must not produce a password proof");
            assert_eq!(error, "SCRAM password cannot be prepared for authentication");
            assert!(client.salted_password.is_empty());
            assert!(client.auth_message.is_empty());
            assert!(client.verify_server("v=rmF9pqV8S7suAoZWja4dJRkFsKQ=").is_err());
        }
    }

    #[test]
    fn test_scram_sha1_rfc5802()
    {
        // RFC 5802 test vectors
        let mut client = ScramSha1Client {
            username: "user".to_string(),
            password: "pencil".to_string(),
            nonce: "fyko+d2lbbFgONRv9qkxdawL".to_string(),
            client_first_bare: String::new(),
            salted_password: Vec::new(),
            auth_message: String::new(),
            mechanism_name: "SCRAM-SHA-1",
            _marker: PhantomData,
        };

        let client_first = client.client_first();
        assert_eq!(client_first, "n,,n=user,r=fyko+d2lbbFgONRv9qkxdawL");

        let server_first = "r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,s=QSXCR+Q6sek8bf92,i=4096";
        let client_final = client.client_final(server_first).unwrap();
        assert_eq!(
            client_final,
            "c=biws,r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,p=v0X8v3Bz2T0CJGbJQyF0X+HI4Ts="
        );

        client.verify_server("v=rmF9pqV8S7suAoZWja4dJRkFsKQ=").unwrap();
        client.verify_server("v=rmF9pqV8S7suAoZWja4dJRkFsKQ=,x=optional").unwrap();
        for invalid in [
            "v=rmF9pqV8S7suAoZWja4dJRkFsKQ=,e=invalid-proof",
            "v=rmF9pqV8S7suAoZWja4dJRkFsKQ=,v=AAAA",
            "v=rmF9pqV8S7suAoZWja4dJRkFsKQ=,m=required",
            "v=AAAA", "e=invalid-proof", "v=not-base64",
        ]
        {
            assert!(client.verify_server(invalid).is_err(), "{invalid}");
        }
    }
}
