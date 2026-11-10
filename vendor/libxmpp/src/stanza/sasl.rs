use base64::{ Engine, engine::general_purpose::STANDARD as BASE64 };
use hmac::{ Hmac, Mac, EagerHash };
use hmac::digest::KeyInit;
use sha1::Sha1;
use sha2::{ Sha256, Sha512 };
use quick_xml::Reader;
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

impl<D> ScramClient<D>
where
    D: EagerHash,
{
    pub fn new(username: &str, password: &str, mechanism_name: &'static str) -> Self
    {
        use rand::RngExt;

        let nonce: String = rand::rng()
            .sample_iter(&rand::distr::Alphanumeric)
            .take(24)
            .map(char::from)
            .collect();

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
        self.client_first_bare = format!("n={},r={}", self.username, self.nonce);
        return format!("n,,{}", self.client_first_bare);
    }

    fn client_final(&mut self, server_first: &str) -> Result<String, String>
    {
        let mut server_nonce = "";
        let mut salt_b64 = "";
        let mut iterations: u32 = 0;

        for part in server_first.split(',')
        {
            if let Some(v) = part.strip_prefix("r=") { server_nonce = v; }
            else if let Some(v) = part.strip_prefix("s=") { salt_b64 = v; }
            else if let Some(v) = part.strip_prefix("i=")
            {
                iterations = v.parse().map_err(|_| "Invalid iteration count".to_string())?;
            }
        }

        if !server_nonce.starts_with(&self.nonce)
        {
            return Err("Server nonce doesn't start with client nonce".to_string());
        }

        let salt = BASE64.decode(salt_b64).map_err(|e| e.to_string())?;

        self.salted_password = pbkdf2::<D>(self.password.as_bytes(), &salt, iterations);

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
        let sig_b64 = server_final.strip_prefix("v=")
            .ok_or("Invalid server final message")?;
        let server_sig = BASE64.decode(sig_b64).map_err(|e| e.to_string())?;

        let server_key = hmac_compute::<D>(&self.salted_password, b"Server Key");
        let expected = hmac_compute::<D>(&server_key, self.auth_message.as_bytes());

        if server_sig != expected
        {
            return Err("Server signature verification failed".to_string());
        }

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

/// Check if the XML contains a `<failure` element.
pub fn is_failure(xml: &str) -> bool
{
    return xml.contains("<failure");
}

fn extract_element_text(xml: &str, element: &str) -> Result<String, String>
{
    let mut reader = Reader::from_str(xml);
    let mut in_element = false;

    loop
    {
        match reader.read_event()
        {
            Ok(Event::Start(e)) =>
            {
                if e.local_name().as_ref() == element.as_bytes()
                {
                    in_element = true;
                }
            }
            Ok(Event::Text(e)) if in_element =>
            {
                return Ok(e.decode().map_err(|e| e.to_string())?.to_string());
            }
            Ok(Event::End(_)) if in_element =>
            {
                return Ok(String::new());
            }
            Ok(Event::Empty(e)) =>
            {
                if e.local_name().as_ref() == element.as_bytes()
                {
                    return Ok(String::new());
                }
            }
            Ok(Event::Eof) => return Err(format!("Element <{}> not found", element)),
            Err(e) => return Err(e.to_string()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

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
    }
}
