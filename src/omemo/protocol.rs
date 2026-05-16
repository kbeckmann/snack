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
use super::signal_message::{
    aes256_cbc_decrypt, aes256_cbc_encrypt,
    decode_prekey_message, decode_signal_message,
    derive_message_keys, encode_prekey_message, encode_signal_message,
    strip_djb_prefix, wire_key_33,
    PreKeySignalMessage, SignalMessage,
};
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

    /// Build our own bundle as it should be published. Each public key is
    /// emitted in libsignal wire form (33 bytes: `0x05 || raw 32-byte`)
    /// and the signed pre-key signature is over the wire-form bytes —
    /// this is what Conversations / Dino / Gajim verify.
    pub fn build_bundle(&self) -> WireBundle
    {
        let pre_keys = self.one_time_pre_keys.iter().map(|p| WirePreKey
        {
            id: p.id,
            public_b64: B64.encode(wire_key_33(&p.keypair.public_bytes())),
        }).collect();

        return WireBundle
        {
            signed_pre_key_id: self.signed_pre_key.id,
            signed_pre_key_public_b64: B64.encode(wire_key_33(&self.signed_pre_key.keypair.public_bytes())),
            signed_pre_key_signature_b64: B64.encode(self.signed_pre_key.signature),
            identity_key_b64: B64.encode(wire_key_33(&self.identity.public_bytes())),
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
        let peer_devices = match self.store.peer_device_list(peer_jid)
        {
            Some(list) if !list.is_empty() => list,
            _ => self.fetch_peer_device_list(client, peer_jid).await?,
        };

        if peer_devices.is_empty()
        {
            return Err(OmemoError::NoPeerDevices(peer_jid.to_string()));
        }

        for &did in &peer_devices
        {
            self.ensure_session(client, peer_jid, did).await?;
        }

        // Outer OMEMO payload: AES-128-GCM with a random key/IV. The
        // 16-byte AES key plus 16-byte GCM auth tag is what each per-
        // recipient `<key>` wraps via libsignal.
        let mut key = [0u8; OMEMO_AES_KEY_LEN];
        let mut iv = [0u8; OMEMO_AES_IV_LEN];
        key.copy_from_slice(&random_bytes(OMEMO_AES_KEY_LEN));
        iv.copy_from_slice(&random_bytes(OMEMO_AES_IV_LEN));

        let ct_with_tag = aes128_gcm_encrypt(&key, &iv, plaintext.as_bytes())?;
        let split_at = ct_with_tag.len().saturating_sub(OMEMO_AES_TAG_LEN);
        let (ciphertext, tag) = ct_with_tag.split_at(split_at);

        let mut key_with_tag = [0u8; 32];
        key_with_tag[..OMEMO_AES_KEY_LEN].copy_from_slice(&key);
        key_with_tag[OMEMO_AES_KEY_LEN..].copy_from_slice(tag);

        let our_identity_wire = wire_key_33(&self.identity.public_bytes());

        let mut wire_keys = Vec::with_capacity(peer_devices.len());
        for &did in &peer_devices
        {
            let mut session = match self.store.session(peer_jid, did)
            {
                Some(s) => s,
                None => continue,
            };

            // Look up the peer identity we recorded during X3DH.
            let peer_identity_raw = match self.store.trust(peer_jid, did)
            {
                Some((_, key)) => key,
                None => continue, // shouldn't happen if ensure_session ran
            };
            let peer_identity_wire = wire_key_33(&peer_identity_raw);

            // Ratchet step yields the message key plus the header info
            // (ratchet pub + counters) we'll embed in the WhisperMessage.
            let step = session.ratchet.encrypt()?;
            let (cipher_key, mac_key, cbc_iv) = derive_message_keys(&step.message_key)?;
            let inner_ct = aes256_cbc_encrypt(&cipher_key, &cbc_iv, &key_with_tag);

            let whisper = SignalMessage
            {
                ratchet_key_wire: wire_key_33(&step.header_dh_pub).to_vec(),
                counter: step.n,
                previous_counter: step.pn,
                ciphertext: inner_ct,
            };

            let inner_envelope = encode_signal_message(
                &mac_key,
                &our_identity_wire,
                &peer_identity_wire,
                &whisper,
            );

            let pending = session.pending_pre_key.clone();
            let wire_bytes = if let Some(p) = pending
            {
                let prekey_msg = PreKeySignalMessage
                {
                    registration_id: p.registration_id,
                    pre_key_id: p.used_one_time_pre_key_id,
                    signed_pre_key_id: p.used_signed_pre_key_id,
                    base_key_wire: wire_key_33(&p.ephemeral_key_pub).to_vec(),
                    identity_key_wire: wire_key_33(&p.identity_key_pub).to_vec(),
                    inner_message_bytes: inner_envelope,
                };
                encode_prekey_message(&prekey_msg)
            }
            else
            {
                inner_envelope
            };

            let is_prekey = session.pending_pre_key.is_some();

            wire_keys.push(OmemoKey
            {
                rid: did,
                prekey: is_prekey,
                data_b64: B64.encode(&wire_bytes),
            });

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
    /// when a `<key>` targeted at our device id was found and the inner
    /// libsignal envelope decrypted successfully.
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

        let wrapped_bytes = B64.decode(&key.data_b64)
            .map_err(|e| OmemoError::Base64(e.to_string()))?;

        // Unwrap a possible PreKeyWhisperMessage envelope. If we end up
        // with a fresh responder session it gets stored before the
        // ratchet decrypt step.
        let (inner_envelope, peer_identity_raw, established_session) = if key.prekey
        {
            let prekey = decode_prekey_message(&wrapped_bytes)
                .map_err(|e| OmemoError::InvalidStanza(format!("prekey envelope: {}", e)))?;
            let peer_identity = strip_djb_prefix(&prekey.identity_key_wire)
                .map_err(|e| OmemoError::InvalidStanza(format!("identity_key: {}", e)))?;
            let peer_base = strip_djb_prefix(&prekey.base_key_wire)
                .map_err(|e| OmemoError::InvalidStanza(format!("base_key: {}", e)))?;

            // Build the responder session using the SPK/OTPK ids the
            // initiator told us about.
            let session = match self.store.session(peer_jid, encrypted.sid)
            {
                Some(s) => s,
                None => self.build_responder_session(
                    prekey.signed_pre_key_id,
                    prekey.pre_key_id,
                    peer_identity,
                    peer_base,
                )?,
            };
            (prekey.inner_message_bytes, peer_identity, session)
        }
        else
        {
            let session = self.store.session(peer_jid, encrypted.sid)
                .ok_or_else(|| OmemoError::InvalidStanza(
                    "no session and not a prekey message".into()
                ))?;
            let peer_identity = self.store.trust(peer_jid, encrypted.sid)
                .map(|(_, k)| k)
                .unwrap_or([0u8; 32]);
            (wrapped_bytes, peer_identity, session)
        };

        let our_identity_wire = wire_key_33(&self.identity.public_bytes());
        let peer_identity_wire = wire_key_33(&peer_identity_raw);

        // Derive the message key first via the ratchet step so we can
        // verify the MAC and decrypt the inner ciphertext.
        let mut session = established_session;

        // Peek into the WhisperMessage protobuf to extract header info
        // (ratchet pub + counters) before MAC validation, then re-derive
        // keys, then validate MAC + decrypt.
        let (ratchet_pub, n, pn, _ct_unused) = peek_whisper_header(&inner_envelope)?;

        let message_key = session.ratchet.decrypt(ratchet_pub, n, pn)?;
        let (cipher_key, mac_key, cbc_iv) = derive_message_keys(&message_key)?;

        let whisper = decode_signal_message(
            &inner_envelope,
            &mac_key,
            &peer_identity_wire,
            &our_identity_wire,
        ).map_err(|e| OmemoError::InvalidStanza(format!("decrypt envelope: {}", e)))?;

        let key_with_tag = aes256_cbc_decrypt(&cipher_key, &cbc_iv, &whisper.ciphertext)
            .map_err(|e| OmemoError::InvalidStanza(format!("cbc decrypt: {}", e)))?;
        if key_with_tag.len() != 32
        {
            return Err(OmemoError::InvalidStanza(format!(
                "expected 32-byte key+tag, got {}", key_with_tag.len()
            )));
        }

        // Successful unwrap → drop any pending pre-key state.
        session.pending_pre_key = None;
        self.store.put_session(peer_jid, encrypted.sid, session);

        // Trust on first use: record the peer identity we learned from
        // the prekey envelope (or the existing trust entry's value).
        if self.store.trust(peer_jid, encrypted.sid).is_none()
        {
            self.store.set_trust(peer_jid, encrypted.sid, Trust::Tofu, peer_identity_raw);
        }

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
        key16.copy_from_slice(&key_with_tag[..OMEMO_AES_KEY_LEN]);
        tag.copy_from_slice(&key_with_tag[OMEMO_AES_KEY_LEN..]);

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
        signed_pre_key_id: u32,
        pre_key_id: Option<u32>,
        peer_identity_raw: [u8; 32],
        peer_ephemeral_pub: [u8; 32],
    ) -> Result<Session, OmemoError>
    {
        if signed_pre_key_id != self.signed_pre_key.id
        {
            return Err(OmemoError::BadBundle);
        }

        let spk = self.signed_pre_key.clone();
        let otpk = match pre_key_id
        {
            Some(id) => self.store.take_one_time_pre_key(id).or_else(||
                self.one_time_pre_keys.iter().find(|p| p.id == id).cloned()
            ),
            None => None,
        };

        let sk = responder_x3dh(&ResponderInputs
        {
            identity: &self.identity.keypair,
            signed_pre_key: &spk.keypair,
            one_time_pre_key: otpk.as_ref().map(|p| &p.keypair),
            peer_identity_key: peer_identity_raw,
            peer_ephemeral_key: peer_ephemeral_pub,
        })?;

        let ratchet = RatchetState::init_responder(
            sk,
            spk.keypair.private_bytes(),
            spk.keypair.public_bytes(),
        );

        let session = Session { ratchet, pending_pre_key: None };

        if let Some(p) = otpk
        {
            self.one_time_pre_keys.retain(|x| x.id != p.id);
        }

        return Ok(session);
    }
}

fn peek_whisper_header(envelope: &[u8]) -> Result<([u8; 32], u32, u32, ()), OmemoError>
{
    if envelope.len() < 1 + 8
    {
        return Err(OmemoError::InvalidStanza("envelope too short".into()));
    }
    let proto = &envelope[1..envelope.len() - 8];
    let mut pos = 0;
    let mut ratchet_key: Option<[u8; 32]> = None;
    let mut counter: u32 = 0;
    let mut previous_counter: u32 = 0;

    while pos < proto.len()
    {
        let (tag, used) = read_varint(&proto[pos..])?;
        pos += used;
        let field = (tag >> 3) as u32;
        let wire = (tag & 0x07) as u8;

        match (field, wire)
        {
            (1, 2) =>
            {
                let (len, used) = read_varint(&proto[pos..])?;
                pos += used;
                let end = pos + len as usize;
                let raw = proto.get(pos..end)
                    .ok_or_else(|| OmemoError::InvalidStanza("truncated ratchet_key".into()))?;
                ratchet_key = Some(strip_djb_prefix(raw)
                    .map_err(|e| OmemoError::InvalidStanza(format!("{}", e)))?);
                pos = end;
            }
            (2, 0) =>
            {
                let (v, used) = read_varint(&proto[pos..])?;
                pos += used;
                counter = v as u32;
            }
            (3, 0) =>
            {
                let (v, used) = read_varint(&proto[pos..])?;
                pos += used;
                previous_counter = v as u32;
            }
            (_, 0) =>
            {
                let (_, used) = read_varint(&proto[pos..])?;
                pos += used;
            }
            (_, 2) =>
            {
                let (len, used) = read_varint(&proto[pos..])?;
                pos += used;
                pos = pos.saturating_add(len as usize).min(proto.len());
            }
            (_, _) =>
            {
                return Err(OmemoError::InvalidStanza("unknown wire type".into()));
            }
        }
    }

    let ratchet_key = ratchet_key.ok_or_else(||
        OmemoError::InvalidStanza("missing ratchet_key".into())
    )?;
    return Ok((ratchet_key, counter, previous_counter, ()));
}

fn read_varint(buf: &[u8]) -> Result<(u64, usize), OmemoError>
{
    let mut shift = 0u32;
    let mut result = 0u64;
    let mut consumed = 0usize;
    for &b in buf
    {
        consumed += 1;
        result |= ((b & 0x7f) as u64) << shift;
        if (b & 0x80) == 0 { return Ok((result, consumed)); }
        shift += 7;
        if shift > 63
        {
            return Err(OmemoError::InvalidStanza("varint overflow".into()));
        }
    }
    return Err(OmemoError::InvalidStanza("varint truncated".into()));
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
    use crate::omemo::ratchet::RatchetState;
    use crate::omemo::signal_message::{
        decode_signal_message, encode_signal_message, encode_prekey_message,
        decode_prekey_message, derive_message_keys, aes256_cbc_encrypt,
        aes256_cbc_decrypt, PreKeySignalMessage, SignalMessage, wire_key_33,
    };
    use crate::omemo::x3dh::{ initiator_x3dh, responder_x3dh, PeerBundle, ResponderInputs };
    use crate::omemo::crypto::{ xeddsa_sign, X25519KeyPair };

    /// Whole pipeline: Alice X3DH → ratchet → libsignal envelope →
    /// Bob parses PreKeyWhisperMessage → responder X3DH → ratchet →
    /// recovers the inner 32-byte key+tag.
    #[test]
    fn libsignal_envelope_roundtrip_prekey()
    {
        let alice_id = X25519KeyPair::generate();
        let bob_id = X25519KeyPair::generate();
        let bob_spk = X25519KeyPair::generate();
        let bob_opk = X25519KeyPair::generate();
        let sig = xeddsa_sign(
            &bob_id.private_bytes(),
            &wire_key_33(&bob_spk.public_bytes()),
        );

        let peer = PeerBundle
        {
            identity_key: bob_id.public_bytes(),
            signed_pre_key_id: 1,
            signed_pre_key: bob_spk.public_bytes(),
            signed_pre_key_sig: sig,
            one_time_pre_key: Some((42, bob_opk.public_bytes())),
        };

        // Alice runs initiator X3DH.
        let init = initiator_x3dh(&alice_id, &peer).unwrap();
        let mut alice_ratchet = RatchetState::init_initiator(
            init.shared_secret, peer.signed_pre_key
        ).unwrap();

        // Alice encrypts the 32-byte OMEMO key+tag.
        let inner_plain = [7u8; 32];
        let step = alice_ratchet.encrypt().unwrap();
        let (cipher_key, mac_key, cbc_iv) = derive_message_keys(&step.message_key).unwrap();
        let inner_ct = aes256_cbc_encrypt(&cipher_key, &cbc_iv, &inner_plain);

        let alice_ik_wire = wire_key_33(&alice_id.public_bytes());
        let bob_ik_wire = wire_key_33(&bob_id.public_bytes());

        let whisper = SignalMessage
        {
            ratchet_key_wire: wire_key_33(&step.header_dh_pub).to_vec(),
            counter: step.n,
            previous_counter: step.pn,
            ciphertext: inner_ct,
        };

        let inner_env = encode_signal_message(&mac_key, &alice_ik_wire, &bob_ik_wire, &whisper);

        let prekey = PreKeySignalMessage
        {
            registration_id: 1,
            pre_key_id: Some(42),
            signed_pre_key_id: 1,
            base_key_wire: wire_key_33(&init.ephemeral_key.public_bytes()).to_vec(),
            identity_key_wire: alice_ik_wire.to_vec(),
            inner_message_bytes: inner_env,
        };

        let wire = encode_prekey_message(&prekey);

        // Bob receives and parses the PreKey envelope.
        let parsed = decode_prekey_message(&wire).unwrap();
        let peer_ik_raw = crate::omemo::signal_message::strip_djb_prefix(&parsed.identity_key_wire).unwrap();
        let peer_base_raw = crate::omemo::signal_message::strip_djb_prefix(&parsed.base_key_wire).unwrap();
        assert_eq!(peer_ik_raw, alice_id.public_bytes());
        assert_eq!(parsed.signed_pre_key_id, 1);
        assert_eq!(parsed.pre_key_id, Some(42));

        // Bob runs responder X3DH with the keys named in the envelope.
        let sk = responder_x3dh(&ResponderInputs
        {
            identity: &bob_id,
            signed_pre_key: &bob_spk,
            one_time_pre_key: Some(&bob_opk),
            peer_identity_key: peer_ik_raw,
            peer_ephemeral_key: peer_base_raw,
        }).unwrap();
        assert_eq!(sk, init.shared_secret);

        let mut bob_ratchet = RatchetState::init_responder(
            sk, bob_spk.private_bytes(), bob_spk.public_bytes(),
        );

        // Bob runs the ratchet step matching the header, then validates
        // the MAC and decrypts AES-CBC.
        let (rk, n, pn, _) = peek_whisper_header(&parsed.inner_message_bytes).unwrap();
        let mk = bob_ratchet.decrypt(rk, n, pn).unwrap();
        let (ck, mk2, iv) = derive_message_keys(&mk).unwrap();

        let signal = decode_signal_message(
            &parsed.inner_message_bytes,
            &mk2,
            &alice_ik_wire,
            &bob_ik_wire,
        ).unwrap();

        let recovered = aes256_cbc_decrypt(&ck, &iv, &signal.ciphertext).unwrap();
        assert_eq!(recovered, &inner_plain[..]);
    }

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
