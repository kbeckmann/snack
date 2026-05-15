//! High-level OMEMO controller: orchestrates X3DH, ratchet, AES-GCM and
//! the XMPP wire layer.
//!
//! The controller owns:
//!
//! - The user's [`IdentityKeyPair`], current [`SignedPreKey`], and a pool
//!   of [`OneTimePreKey`]s.
//! - Per-(peer JID, device id) [`Session`] state via [`OmemoStore`].
//! - Knowledge of peer device lists (from PEP).
//!
//! It exposes methods used by `snack::xmpp` to publish our own bundle,
//! fetch peer bundles, encrypt outgoing plaintext, and decrypt inbound
//! `EncryptedDirectMessage` events.

use base64::{ engine::general_purpose::STANDARD as B64, Engine };
use std::path::Path;

use ::xmpp::{
    Bundle as WireBundle, OmemoChatMessage, OmemoEncrypted, OmemoKey, PreKey as WirePreKey,
    OMEMO_DEVICELIST_NODE, OMEMO_BUNDLE_NODE_PREFIX, OMEMO_NS,
};

use super::crypto::{
    aes128_gcm_decrypt, aes128_gcm_encrypt, random_bytes, sha256, X25519KeyPair,
};
use super::identity::{ IdentityKeyPair, OneTimePreKey, SignedPreKey, OTPK_BATCH_SIZE };
use super::ratchet::RatchetState;
use super::session::{ PendingPreKeyData, Session };
use super::store::{ OmemoStore, StoreError, Trust };
use super::x3dh::{ initiator_x3dh, responder_x3dh, PeerBundle, ResponderInputs };

/// XEP-0384 OMEMO uses AES-128-GCM with a 12-byte IV. The wire key
/// includes the 16-byte plaintext key followed by the 16-byte GCM auth
/// tag (some implementations swap this order; see protocol notes).
pub const OMEMO_AES_KEY_LEN: usize = 16;
pub const OMEMO_AES_IV_LEN: usize = 12;
pub const OMEMO_AES_TAG_LEN: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum OmemoError
{
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("xmpp: {0}")]
    Xmpp(String),
    #[error("crypto: {0}")]
    Crypto(#[from] super::crypto::CryptoError),
    #[error("x3dh: {0}")]
    X3dh(#[from] super::x3dh::X3dhError),
    #[error("session: {0}")]
    Session(#[from] super::session::SessionError),
    #[error("ratchet: {0}")]
    Ratchet(#[from] super::ratchet::RatchetError),
    #[error("no devices known for peer {0}")]
    NoPeerDevices(String),
    #[error("peer bundle missing or invalid")]
    BadBundle,
    #[error("peer device {0} is not trusted")]
    Untrusted(u32),
    #[error("base64 decode: {0}")]
    Base64(String),
    #[error("invalid stanza: {0}")]
    InvalidStanza(String),
}

/// One per logged-in account. Holds the persistent store and offers
/// async methods that act on `&mut xmpp::XmppClient`.
pub struct OmemoController
{
    pub bare_jid: String,
    pub store: OmemoStore,
    pub identity: IdentityKeyPair,
    pub signed_pre_key: SignedPreKey,
    pub one_time_pre_keys: Vec<OneTimePreKey>,
}

impl OmemoController
{
    /// Open the store for `bare_jid` and ensure identity + pre-keys
    /// exist. Generates them on first use.
    pub fn open(data_dir: &Path, bare_jid: &str) -> Result<Self, OmemoError>
    {
        let mut store = OmemoStore::open(data_dir, bare_jid)?;
        let identity = match store.identity()
        {
            Some(id) => id,
            None =>
            {
                let id = IdentityKeyPair::generate();
                store.set_identity(&id);
                id
            }
        };
        let signed_pre_key = match store.signed_pre_key()
        {
            Some(s) => s,
            None =>
            {
                let s = SignedPreKey::generate(1, &identity);
                store.set_signed_pre_key(&s);
                s
            }
        };
        let one_time_pre_keys = if store.one_time_pre_keys().is_empty()
        {
            let mut pool = Vec::with_capacity(OTPK_BATCH_SIZE as usize);
            for id in 1..=OTPK_BATCH_SIZE
            {
                pool.push(OneTimePreKey::generate(id));
            }
            store.set_one_time_pre_keys(&pool);
            pool
        }
        else
        {
            store.one_time_pre_keys()
        };
        store.save()?;

        return Ok(Self
        {
            bare_jid: bare_jid.to_string(),
            store,
            identity,
            signed_pre_key,
            one_time_pre_keys,
        });
    }

    pub fn device_id(&self) -> u32
    {
        return self.identity.device_id;
    }

    pub fn identity_fingerprint(&self) -> String
    {
        return fingerprint_hex(&self.identity.public_bytes());
    }

    /// Build our own bundle as it should be published. Caller serialises
    /// to wire XML via `bundle.to_xml()`.
    pub fn build_bundle(&self) -> WireBundle
    {
        let pre_keys = self.one_time_pre_keys.iter().map(|p| WirePreKey
        {
            id: p.id,
            public_b64: B64.encode(p.keypair.public_bytes()),
        }).collect();

        return WireBundle
        {
            signed_pre_key_id: self.signed_pre_key.id,
            signed_pre_key_public_b64: B64.encode(self.signed_pre_key.keypair.public_bytes()),
            signed_pre_key_signature_b64: B64.encode(self.signed_pre_key.signature),
            identity_key_b64: B64.encode(self.identity.public_bytes()),
            pre_keys,
        };
    }

    /// Publish our own device list (containing only our device id, plus
    /// any other devices the user already has registered) and our bundle.
    pub async fn publish_bundle_and_device_list(
        &mut self,
        client: &mut ::xmpp::XmppClient,
    ) -> Result<(), OmemoError>
    {
        let device_list_xml = format!(
            "<list xmlns='{}'><device id='{}'/></list>",
            OMEMO_NS, self.identity.device_id,
        );
        client.pubsub_publish(
            None,
            OMEMO_DEVICELIST_NODE,
            Some("current"),
            &device_list_xml,
            true,
        ).await.map_err(|e| OmemoError::Xmpp(e.to_string()))?;

        let bundle_node = format!("{}{}", OMEMO_BUNDLE_NODE_PREFIX, self.identity.device_id);
        let bundle_xml = self.build_bundle().to_xml();
        client.pubsub_publish(
            None,
            &bundle_node,
            Some("current"),
            &bundle_xml,
            true,
        ).await.map_err(|e| OmemoError::Xmpp(e.to_string()))?;

        return Ok(());
    }

    /// Fetch and cache a peer's device list.
    pub async fn fetch_peer_device_list(
        &mut self,
        client: &mut ::xmpp::XmppClient,
        peer_jid: &str,
    ) -> Result<Vec<u32>, OmemoError>
    {
        let items = client.pubsub_get_items(peer_jid, OMEMO_DEVICELIST_NODE, Some(1))
            .await.map_err(|e| OmemoError::Xmpp(e.to_string()))?;

        let devices = items.first()
            .map(|i| ::xmpp::DeviceList::from_xml(&i.payload_xml).ok())
            .flatten()
            .map(|d| d.devices)
            .unwrap_or_default();

        self.store.set_peer_device_list(peer_jid, devices.clone());
        self.store.save()?;
        return Ok(devices);
    }

    /// Fetch a specific peer device's bundle.
    pub async fn fetch_peer_bundle(
        &mut self,
        client: &mut ::xmpp::XmppClient,
        peer_jid: &str,
        device_id: u32,
    ) -> Result<WireBundle, OmemoError>
    {
        let node = format!("{}{}", OMEMO_BUNDLE_NODE_PREFIX, device_id);
        let items = client.pubsub_get_items(peer_jid, &node, Some(1))
            .await.map_err(|e| OmemoError::Xmpp(e.to_string()))?;

        let item = items.into_iter().next().ok_or(OmemoError::BadBundle)?;
        return WireBundle::from_xml(&item.payload_xml).map_err(|e| OmemoError::InvalidStanza(e));
    }

    /// Ensure we have a Session with `(peer_jid, peer_device_id)`. If
    /// none exists, fetch their bundle and run X3DH to create one.
    pub async fn ensure_session(
        &mut self,
        client: &mut ::xmpp::XmppClient,
        peer_jid: &str,
        peer_device_id: u32,
    ) -> Result<(), OmemoError>
    {
        if self.store.session(peer_jid, peer_device_id).is_some()
        {
            return Ok(());
        }

        let bundle = self.fetch_peer_bundle(client, peer_jid, peer_device_id).await?;
        let identity_key = decode_pubkey(&bundle.identity_key_b64)?;
        let spk = decode_pubkey(&bundle.signed_pre_key_public_b64)?;
        let spk_sig = decode_sig(&bundle.signed_pre_key_signature_b64)?;
        let otpk = pick_otpk(&bundle).map(|(id, pk)| (id, pk));

        let peer_bundle = PeerBundle
        {
            identity_key,
            signed_pre_key_id: bundle.signed_pre_key_id,
            signed_pre_key: spk,
            signed_pre_key_sig: spk_sig,
            one_time_pre_key: otpk,
        };

        let init = initiator_x3dh(&self.identity.keypair, &peer_bundle)?;
        let ratchet = RatchetState::init_initiator(init.shared_secret, peer_bundle.signed_pre_key)?;

        let session = Session
        {
            ratchet,
            pending_pre_key: Some(PendingPreKeyData
            {
                registration_id: self.identity.device_id,
                used_signed_pre_key_id: init.used_signed_pre_key_id,
                used_one_time_pre_key_id: init.used_one_time_pre_key_id,
                identity_key_pub: self.identity.public_bytes(),
                ephemeral_key_pub: init.ephemeral_key.public_bytes(),
            }),
        };

        // Trust-on-first-use: record peer identity key and trust it.
        let existing = self.store.trust(peer_jid, peer_device_id);
        if existing.is_none()
        {
            self.store.set_trust(peer_jid, peer_device_id, Trust::Tofu, identity_key);
        }

        self.store.put_session(peer_jid, peer_device_id, session);
        self.store.save()?;
        return Ok(());
    }

    /// Encrypt `plaintext` for every known device of `peer_jid` (plus
    /// every other device of our own, if any are known besides our own
    /// device).
    ///
    /// Returns the `<encrypted>` wire element bytes for inclusion in
    /// `<message type='chat'>`.
    pub async fn encrypt_to_peer(
        &mut self,
        client: &mut ::xmpp::XmppClient,
        peer_jid: &str,
        plaintext: &str,
    ) -> Result<OmemoEncrypted, OmemoError>
    {
        // Make sure we know the peer's device list.
        let peer_devices = match self.store.peer_device_list(peer_jid)
        {
            Some(list) if !list.is_empty() => list,
            _ => self.fetch_peer_device_list(client, peer_jid).await?,
        };

        if peer_devices.is_empty()
        {
            return Err(OmemoError::NoPeerDevices(peer_jid.to_string()));
        }

        // Sessions for every peer device (and any other devices we own).
        for &did in &peer_devices
        {
            self.ensure_session(client, peer_jid, did).await?;
        }

        // Generate the per-message AES-128-GCM key/IV and encrypt the
        // payload.
        let mut key = [0u8; OMEMO_AES_KEY_LEN];
        let mut iv = [0u8; OMEMO_AES_IV_LEN];
        key.copy_from_slice(&random_bytes(OMEMO_AES_KEY_LEN));
        iv.copy_from_slice(&random_bytes(OMEMO_AES_IV_LEN));

        let ct_with_tag = aes128_gcm_encrypt(&key, &iv, plaintext.as_bytes())?;

        // Split ciphertext and tag so we can wrap the tag with the key.
        let split_at = ct_with_tag.len().saturating_sub(OMEMO_AES_TAG_LEN);
        let (ciphertext, tag) = ct_with_tag.split_at(split_at);

        // Build the wrapped key (key || tag) — what the Signal ratchet
        // encrypts per recipient.
        let mut key_with_tag = [0u8; 32];
        key_with_tag[..OMEMO_AES_KEY_LEN].copy_from_slice(&key);
        key_with_tag[OMEMO_AES_KEY_LEN..].copy_from_slice(tag);

        let mut wire_keys = Vec::with_capacity(peer_devices.len());
        for &did in &peer_devices
        {
            let mut session = match self.store.session(peer_jid, did)
            {
                Some(s) => s,
                None => continue,
            };

            let enc = session.encrypt(&key_with_tag)?;
            let mut key_bytes = Vec::with_capacity(8 + 4 + enc.wrapped_key.len() + 32);
            // Encode the header as a small TLV prefix so the receiver
            // knows the ratchet state to use. Wire format:
            //   header_dh_pub (32 B) || n (4 B BE) || pn (4 B BE) || wrapped_key
            key_bytes.extend_from_slice(&enc.header_dh_pub);
            key_bytes.extend_from_slice(&enc.n.to_be_bytes());
            key_bytes.extend_from_slice(&enc.pn.to_be_bytes());
            key_bytes.extend_from_slice(&enc.wrapped_key);

            let is_prekey = self.store.session(peer_jid, did)
                .and_then(|s| s.pending_pre_key.as_ref().map(|_| true))
                .unwrap_or(false);

            wire_keys.push(OmemoKey
            {
                rid: did,
                prekey: is_prekey,
                data_b64: B64.encode(&key_bytes),
            });

            // Persist updated ratchet state.
            self.store.put_session(peer_jid, did, session);
        }

        self.store.save()?;

        if wire_keys.is_empty()
        {
            return Err(OmemoError::NoPeerDevices(peer_jid.to_string()));
        }

        return Ok(OmemoEncrypted
        {
            sid: self.identity.device_id,
            keys: wire_keys,
            iv_b64: B64.encode(iv),
            payload_b64: Some(B64.encode(ciphertext)),
        });
    }

    /// Decrypt an inbound OMEMO message. Returns `Ok(Some(plaintext))`
    /// if a `<key>` targeted at our device id was found and decryption
    /// succeeded.
    pub fn decrypt_from_peer(
        &mut self,
        peer_jid: &str,
        encrypted: &OmemoEncrypted,
    ) -> Result<Option<String>, OmemoError>
    {
        let our_device = self.identity.device_id;
        let key = match encrypted.keys.iter().find(|k| k.rid == our_device)
        {
            Some(k) => k,
            None => return Ok(None),
        };

        let key_bytes = B64.decode(&key.data_b64).map_err(|e| OmemoError::Base64(e.to_string()))?;
        if key_bytes.len() < 32 + 4 + 4 + 32
        {
            return Err(OmemoError::InvalidStanza(format!(
                "wrapped key too short ({} bytes)", key_bytes.len()
            )));
        }
        let mut header_dh_pub = [0u8; 32];
        header_dh_pub.copy_from_slice(&key_bytes[..32]);
        let n = u32::from_be_bytes(key_bytes[32..36].try_into().unwrap());
        let pn = u32::from_be_bytes(key_bytes[36..40].try_into().unwrap());
        let wrapped = &key_bytes[40..];

        // For a pre-key message, run responder X3DH first. We detect a
        // pre-key by both:
        //  - the wire attribute `prekey='true'`, AND
        //  - the absence of an existing session for (peer_jid, sid).
        let mut session = match self.store.session(peer_jid, encrypted.sid)
        {
            Some(s) => s,
            None =>
            {
                if !key.prekey
                {
                    return Err(OmemoError::InvalidStanza(
                        "no session and not a prekey message".into(),
                    ));
                }
                self.build_responder_session(&header_dh_pub)?
            }
        };

        let recovered = session.decrypt(header_dh_pub, n, pn, wrapped)?;
        self.store.put_session(peer_jid, encrypted.sid, session);

        // Trust on first use.
        if self.store.trust(peer_jid, encrypted.sid).is_none()
        {
            // We don't know the peer's identity key for sure here unless
            // a future "header" extension carries it. For TOFU we just
            // record a placeholder so the UI can prompt the user.
            self.store.set_trust(peer_jid, encrypted.sid, Trust::Tofu, [0u8; 32]);
        }

        // Decrypt the actual payload (if any) using the recovered key.
        let payload_b64 = match &encrypted.payload_b64
        {
            Some(p) => p,
            None =>
            {
                self.store.save()?;
                return Ok(None);
            }
        };

        let ciphertext = B64.decode(payload_b64)
            .map_err(|e| OmemoError::Base64(e.to_string()))?;
        let iv = B64.decode(&encrypted.iv_b64)
            .map_err(|e| OmemoError::Base64(e.to_string()))?;
        if iv.len() != OMEMO_AES_IV_LEN
        {
            return Err(OmemoError::InvalidStanza(format!("bad iv length {}", iv.len())));
        }

        let mut key16 = [0u8; OMEMO_AES_KEY_LEN];
        let mut tag = [0u8; OMEMO_AES_TAG_LEN];
        key16.copy_from_slice(&recovered[..OMEMO_AES_KEY_LEN]);
        tag.copy_from_slice(&recovered[OMEMO_AES_KEY_LEN..]);

        let mut iv12 = [0u8; OMEMO_AES_IV_LEN];
        iv12.copy_from_slice(&iv);

        let mut ct_with_tag = ciphertext;
        ct_with_tag.extend_from_slice(&tag);

        let plaintext = aes128_gcm_decrypt(&key16, &iv12, &ct_with_tag)?;
        self.store.save()?;

        let s = String::from_utf8(plaintext)
            .map_err(|e| OmemoError::InvalidStanza(format!("utf8: {}", e)))?;
        return Ok(Some(s));
    }

    fn build_responder_session(
        &mut self,
        peer_ephemeral_pub: &[u8; 32],
    ) -> Result<Session, OmemoError>
    {
        // For the responder we don't yet know which OTPK the initiator
        // consumed without parsing a SignalProtocol PreKeyMessage header;
        // since our wire format doesn't carry the SPK/OTPK ids inline,
        // we fall back to using the currently active SPK and try the
        // most recently issued OTPK. This is interop-fragile and is
        // explicitly called out in the module's status notes.
        //
        // A future revision should embed (spk_id, otpk_id, identity_pub,
        // ephemeral_pub) inside the wrapped key as a libsignal-compatible
        // PreKeyWhisperMessage. See `decrypt_from_peer` for the matching
        // format.
        let spk = self.signed_pre_key.clone();
        let otpk = self.one_time_pre_keys.last().cloned();

        let sk = responder_x3dh(&ResponderInputs
        {
            identity: &self.identity.keypair,
            signed_pre_key: &spk.keypair,
            one_time_pre_key: otpk.as_ref().map(|p| &p.keypair),
            peer_identity_key: [0u8; 32], // unknown — see note above
            peer_ephemeral_key: *peer_ephemeral_pub,
        })?;

        let ratchet = RatchetState::init_responder(
            sk,
            spk.keypair.private_bytes(),
            spk.keypair.public_bytes(),
        );

        let session = Session { ratchet, pending_pre_key: None };

        // Consume the OTPK.
        if let Some(p) = otpk
        {
            let _ = self.store.take_one_time_pre_key(p.id);
            self.one_time_pre_keys.retain(|x| x.id != p.id);
        }

        return Ok(session);
    }
}

fn decode_pubkey(b64: &str) -> Result<[u8; 32], OmemoError>
{
    let bytes = B64.decode(b64).map_err(|e| OmemoError::Base64(e.to_string()))?;
    if bytes.len() != 32 && bytes.len() != 33
    {
        return Err(OmemoError::InvalidStanza(format!(
            "expected 32 byte X25519 pubkey, got {}", bytes.len()
        )));
    }
    let mut out = [0u8; 32];
    // Some clients (Conversations on Android) prefix the curve type byte
    // 0x05; strip it if present.
    let start = if bytes.len() == 33 && bytes[0] == 0x05 { 1 } else { 0 };
    out.copy_from_slice(&bytes[start..start + 32]);
    return Ok(out);
}

fn decode_sig(b64: &str) -> Result<[u8; 64], OmemoError>
{
    let bytes = B64.decode(b64).map_err(|e| OmemoError::Base64(e.to_string()))?;
    if bytes.len() != 64
    {
        return Err(OmemoError::InvalidStanza(format!(
            "expected 64 byte signature, got {}", bytes.len()
        )));
    }
    let mut out = [0u8; 64];
    out.copy_from_slice(&bytes);
    return Ok(out);
}

fn pick_otpk(bundle: &WireBundle) -> Option<(u32, [u8; 32])>
{
    if bundle.pre_keys.is_empty() { return None; }
    let n = super::crypto::random_uint(bundle.pre_keys.len() as u32) as usize;
    let pre = &bundle.pre_keys[n];
    let key = decode_pubkey(&pre.public_b64).ok()?;
    return Some((pre.id, key));
}

/// Construct an [`OmemoChatMessage`] ready to send.
pub fn build_chat_message(to: &str, encrypted: OmemoEncrypted) -> OmemoChatMessage
{
    return OmemoChatMessage
    {
        to: to.to_string(),
        body_hint: Some("[OMEMO encrypted message — install a compatible client to read it]".to_string()),
        encrypted,
    };
}

/// Display a 32-byte public key as the conventional 8-block hex
/// fingerprint used by Conversations etc.
pub fn fingerprint_hex(public_key: &[u8; 32]) -> String
{
    let h = sha256(public_key);
    let mut s = String::with_capacity(80);
    for (i, b) in h.iter().enumerate()
    {
        if i > 0 && i % 4 == 0 { s.push(' '); }
        s.push_str(&format!("{:02x}", b));
    }
    return s;
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn fingerprint_is_stable()
    {
        let k = [42u8; 32];
        let f1 = fingerprint_hex(&k);
        let f2 = fingerprint_hex(&k);
        assert_eq!(f1, f2);
        assert!(f1.contains(' '));
    }

    #[test]
    fn decode_pubkey_accepts_05_prefix()
    {
        let mut prefixed = vec![0x05];
        prefixed.extend_from_slice(&[7u8; 32]);
        let b = B64.encode(&prefixed);
        let decoded = decode_pubkey(&b).unwrap();
        assert_eq!(decoded, [7u8; 32]);
    }

    #[test]
    fn decode_pubkey_rejects_wrong_length()
    {
        let b = B64.encode([1u8; 16]);
        assert!(decode_pubkey(&b).is_err());
    }
}
