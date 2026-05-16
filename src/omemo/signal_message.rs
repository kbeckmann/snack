//! libsignal wire envelope for OMEMO.
//!
//! Encodes / decodes the two libsignal-protocol message kinds shipped
//! inside an OMEMO `<key>` element:
//!
//! - [`SignalMessage`] (`WhisperMessage`) — used for all messages after
//!   a session has been established
//! - [`PreKeySignalMessage`] (`PreKeyWhisperMessage`) — used for the
//!   first message Alice sends Bob; carries the SPK/OTPK ids and Alice's
//!   identity + ephemeral keys so Bob can run responder X3DH
//!
//! On-wire layout (each element of an OMEMO `<key>`):
//!
//! ```text
//! <version_byte> || <protobuf bytes> [ || <8-byte MAC> ]
//! ```
//!
//! The MAC is HMAC-SHA256 truncated to 8 bytes, computed over
//! `sender_identity_key_wire || receiver_identity_key_wire ||
//! version_byte || protobuf_bytes`. Each identity key on the wire is the
//! `0x05` type byte followed by the 32-byte Curve25519 public key.
//!
//! The inner ciphertext is AES-256-CBC of the plaintext (typically the
//! 32-byte `aes_key || aes_gcm_tag` produced by the OMEMO payload step)
//! with PKCS#7 padding. AES key, MAC key and IV are derived from the
//! Double Ratchet message key via HKDF-SHA256(info = "WhisperMessageKeys",
//! salt = zero32, L = 80).

use aes::cipher::{ block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit };
use cbc::{ Decryptor as CbcDecryptor, Encryptor as CbcEncryptor };
use hmac::{ Hmac, Mac };
use sha2::Sha256;

use super::crypto::{ hkdf_sha256, CryptoError };

type HmacSha256 = Hmac<Sha256>;
type Aes256CbcEnc = CbcEncryptor<aes::Aes256>;
type Aes256CbcDec = CbcDecryptor<aes::Aes256>;

/// libsignal protocol message version. The version byte on the wire is
/// `(MESSAGE_VERSION << 4) | MESSAGE_VERSION` = `0x33` for version 3.
pub const MESSAGE_VERSION: u8 = 3;
pub const VERSION_BYTE: u8 = (MESSAGE_VERSION << 4) | MESSAGE_VERSION;

/// Wire-form prefix byte on every Curve25519 public key (libsignal DJB
/// key format).
pub const DJB_TYPE: u8 = 0x05;

#[derive(Debug, thiserror::Error)]
pub enum SignalMessageError
{
    #[error("crypto: {0}")]
    Crypto(#[from] CryptoError),
    #[error("invalid format: {0}")]
    InvalidFormat(String),
    #[error("bad mac")]
    BadMac,
    #[error("unsupported version {0:#x}")]
    UnsupportedVersion(u8),
}

// =====================================================================
//                       Public message structs
// =====================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalMessage
{
    pub ratchet_key_wire: Vec<u8>,
    pub counter: u32,
    pub previous_counter: u32,
    pub ciphertext: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreKeySignalMessage
{
    pub registration_id: u32,
    pub pre_key_id: Option<u32>,
    pub signed_pre_key_id: u32,
    pub base_key_wire: Vec<u8>,
    pub identity_key_wire: Vec<u8>,
    pub inner_message_bytes: Vec<u8>,
}

// =====================================================================
//                  Public envelope encode / decode API
// =====================================================================

/// Build a WhisperMessage wire envelope.
pub fn encode_signal_message(
    mac_key: &[u8; 32],
    sender_identity_wire: &[u8; 33],
    receiver_identity_wire: &[u8; 33],
    msg: &SignalMessage,
) -> Vec<u8>
{
    let proto = encode_whisper_proto(msg);
    let mut to_mac = Vec::with_capacity(1 + proto.len());
    to_mac.push(VERSION_BYTE);
    to_mac.extend_from_slice(&proto);
    let mac = compute_mac(mac_key, sender_identity_wire, receiver_identity_wire, &to_mac);

    let mut out = Vec::with_capacity(1 + proto.len() + 8);
    out.push(VERSION_BYTE);
    out.extend_from_slice(&proto);
    out.extend_from_slice(&mac);
    return out;
}

/// Parse and verify a WhisperMessage wire envelope.
pub fn decode_signal_message(
    bytes: &[u8],
    mac_key: &[u8; 32],
    sender_identity_wire: &[u8; 33],
    receiver_identity_wire: &[u8; 33],
) -> Result<SignalMessage, SignalMessageError>
{
    if bytes.len() < 1 + 8
    {
        return Err(SignalMessageError::InvalidFormat("envelope too short".into()));
    }
    let version = bytes[0];
    if (version >> 4) != MESSAGE_VERSION
    {
        return Err(SignalMessageError::UnsupportedVersion(version));
    }

    let split = bytes.len() - 8;
    let signed = &bytes[..split];
    let mac_recv = &bytes[split..];

    let expected = compute_mac(mac_key, sender_identity_wire, receiver_identity_wire, signed);
    if !super::crypto::ct_eq(&expected, mac_recv)
    {
        return Err(SignalMessageError::BadMac);
    }

    return decode_whisper_proto(&signed[1..]);
}

/// Build a PreKeyWhisperMessage wire envelope. The inner `WhisperMessage`
/// bytes are passed in pre-MACed.
pub fn encode_prekey_message(msg: &PreKeySignalMessage) -> Vec<u8>
{
    let proto = encode_prekey_proto(msg);
    let mut out = Vec::with_capacity(1 + proto.len());
    out.push(VERSION_BYTE);
    out.extend_from_slice(&proto);
    return out;
}

/// Parse a PreKeyWhisperMessage wire envelope.
pub fn decode_prekey_message(bytes: &[u8]) -> Result<PreKeySignalMessage, SignalMessageError>
{
    if bytes.len() < 2
    {
        return Err(SignalMessageError::InvalidFormat("envelope too short".into()));
    }
    let version = bytes[0];
    if (version >> 4) != MESSAGE_VERSION
    {
        return Err(SignalMessageError::UnsupportedVersion(version));
    }
    return decode_prekey_proto(&bytes[1..]);
}

/// HKDF-SHA256 expand of a 32-byte ratchet message key to (cipher_key,
/// mac_key, iv).
pub fn derive_message_keys(message_key: &[u8; 32]) -> Result<([u8; 32], [u8; 32], [u8; 16]), CryptoError>
{
    let okm = hkdf_sha256(&[0u8; 32], message_key, b"WhisperMessageKeys", 80)?;
    let mut cipher = [0u8; 32];
    let mut mac = [0u8; 32];
    let mut iv = [0u8; 16];
    cipher.copy_from_slice(&okm[..32]);
    mac.copy_from_slice(&okm[32..64]);
    iv.copy_from_slice(&okm[64..80]);
    return Ok((cipher, mac, iv));
}

/// AES-256-CBC with PKCS#7 padding.
pub fn aes256_cbc_encrypt(key: &[u8; 32], iv: &[u8; 16], plaintext: &[u8]) -> Vec<u8>
{
    let pad_len = 16 - (plaintext.len() % 16);
    let mut buf = Vec::with_capacity(plaintext.len() + pad_len);
    buf.extend_from_slice(plaintext);
    buf.resize(plaintext.len() + pad_len, 0);
    let enc = Aes256CbcEnc::new(key.into(), iv.into());
    let final_len =
    {
        let out = enc.encrypt_padded_mut::<Pkcs7>(&mut buf, plaintext.len())
            .expect("padded encrypt");
        out.len()
    };
    buf.truncate(final_len);
    return buf;
}

/// AES-256-CBC decrypt with PKCS#7 padding.
pub fn aes256_cbc_decrypt(key: &[u8; 32], iv: &[u8; 16], ciphertext: &[u8]) -> Result<Vec<u8>, SignalMessageError>
{
    let mut buf = ciphertext.to_vec();
    let dec = Aes256CbcDec::new(key.into(), iv.into());
    let pt = dec.decrypt_padded_mut::<Pkcs7>(&mut buf).map_err(|e| {
        SignalMessageError::InvalidFormat(format!("cbc unpad: {}", e))
    })?;
    return Ok(pt.to_vec());
}

/// Curve25519 public key encoding: prepend the DJB type byte.
pub fn wire_key_33(raw32: &[u8; 32]) -> [u8; 33]
{
    let mut out = [0u8; 33];
    out[0] = DJB_TYPE;
    out[1..].copy_from_slice(raw32);
    return out;
}

/// Strip the DJB type byte. Accepts both 32- and 33-byte forms for
/// compatibility with older OMEMO publishers that strip the prefix.
pub fn strip_djb_prefix(b: &[u8]) -> Result<[u8; 32], SignalMessageError>
{
    let raw = if b.len() == 33 && b[0] == DJB_TYPE { &b[1..] }
        else if b.len() == 32 { b }
        else { return Err(SignalMessageError::InvalidFormat(format!("bad pubkey len {}", b.len()))); };
    let mut out = [0u8; 32];
    out.copy_from_slice(raw);
    return Ok(out);
}

// =====================================================================
//                       MAC over the envelope
// =====================================================================

fn compute_mac(
    mac_key: &[u8; 32],
    sender_identity_wire: &[u8; 33],
    receiver_identity_wire: &[u8; 33],
    signed: &[u8],
) -> [u8; 8]
{
    let mut mac = <HmacSha256 as hmac::Mac>::new_from_slice(mac_key).expect("hmac key");
    mac.update(sender_identity_wire);
    mac.update(receiver_identity_wire);
    mac.update(signed);
    let full = mac.finalize().into_bytes();
    let mut out = [0u8; 8];
    out.copy_from_slice(&full[..8]);
    return out;
}

// =====================================================================
//                       Hand-rolled protobuf
// =====================================================================

const WIRE_VARINT: u8 = 0;
const WIRE_LEN: u8 = 2;

fn enc_tag(field: u32, wire: u8, out: &mut Vec<u8>)
{
    enc_varint((field << 3) as u64 | wire as u64, out);
}

fn enc_varint(mut v: u64, out: &mut Vec<u8>)
{
    while v >= 0x80
    {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn enc_bytes(field: u32, b: &[u8], out: &mut Vec<u8>)
{
    enc_tag(field, WIRE_LEN, out);
    enc_varint(b.len() as u64, out);
    out.extend_from_slice(b);
}

fn enc_uint(field: u32, v: u32, out: &mut Vec<u8>)
{
    enc_tag(field, WIRE_VARINT, out);
    enc_varint(v as u64, out);
}

fn dec_varint(buf: &[u8]) -> Result<(u64, usize), SignalMessageError>
{
    let mut shift = 0u32;
    let mut result = 0u64;
    let mut consumed = 0usize;
    for &b in buf
    {
        consumed += 1;
        result |= ((b & 0x7f) as u64) << shift;
        if (b & 0x80) == 0
        {
            return Ok((result, consumed));
        }
        shift += 7;
        if shift > 63
        {
            return Err(SignalMessageError::InvalidFormat("varint overflow".into()));
        }
    }
    return Err(SignalMessageError::InvalidFormat("varint truncated".into()));
}

fn encode_whisper_proto(m: &SignalMessage) -> Vec<u8>
{
    let mut out = Vec::new();
    enc_bytes(1, &m.ratchet_key_wire, &mut out);
    enc_uint(2, m.counter, &mut out);
    enc_uint(3, m.previous_counter, &mut out);
    enc_bytes(4, &m.ciphertext, &mut out);
    return out;
}

fn decode_whisper_proto(bytes: &[u8]) -> Result<SignalMessage, SignalMessageError>
{
    let mut pos = 0;
    let mut ratchet_key = Vec::new();
    let mut counter = 0u32;
    let mut previous_counter = 0u32;
    let mut ciphertext = Vec::new();

    while pos < bytes.len()
    {
        let (tag, used) = dec_varint(&bytes[pos..])?;
        pos += used;
        let field = (tag >> 3) as u32;
        let wire = (tag & 0x07) as u8;

        match (field, wire)
        {
            (1, WIRE_LEN) =>
            {
                let (len, used) = dec_varint(&bytes[pos..])?;
                pos += used;
                let end = pos + len as usize;
                ratchet_key = bytes.get(pos..end)
                    .ok_or_else(|| SignalMessageError::InvalidFormat("truncated ratchet_key".into()))?
                    .to_vec();
                pos = end;
            }
            (2, WIRE_VARINT) =>
            {
                let (v, used) = dec_varint(&bytes[pos..])?;
                pos += used;
                counter = v as u32;
            }
            (3, WIRE_VARINT) =>
            {
                let (v, used) = dec_varint(&bytes[pos..])?;
                pos += used;
                previous_counter = v as u32;
            }
            (4, WIRE_LEN) =>
            {
                let (len, used) = dec_varint(&bytes[pos..])?;
                pos += used;
                let end = pos + len as usize;
                ciphertext = bytes.get(pos..end)
                    .ok_or_else(|| SignalMessageError::InvalidFormat("truncated ciphertext".into()))?
                    .to_vec();
                pos = end;
            }
            (_, WIRE_VARINT) =>
            {
                let (_, used) = dec_varint(&bytes[pos..])?;
                pos += used;
            }
            (_, WIRE_LEN) =>
            {
                let (len, used) = dec_varint(&bytes[pos..])?;
                pos += used;
                pos = pos.saturating_add(len as usize).min(bytes.len());
            }
            (_, w) =>
            {
                return Err(SignalMessageError::InvalidFormat(format!("unsupported wire type {}", w)));
            }
        }
    }

    if ratchet_key.is_empty() || ciphertext.is_empty()
    {
        return Err(SignalMessageError::InvalidFormat("missing required field".into()));
    }

    return Ok(SignalMessage { ratchet_key_wire: ratchet_key, counter, previous_counter, ciphertext });
}

fn encode_prekey_proto(m: &PreKeySignalMessage) -> Vec<u8>
{
    let mut out = Vec::new();
    if let Some(pkid) = m.pre_key_id { enc_uint(1, pkid, &mut out); }
    enc_bytes(2, &m.base_key_wire, &mut out);
    enc_bytes(3, &m.identity_key_wire, &mut out);
    enc_bytes(4, &m.inner_message_bytes, &mut out);
    enc_uint(5, m.registration_id, &mut out);
    enc_uint(6, m.signed_pre_key_id, &mut out);
    return out;
}

fn decode_prekey_proto(bytes: &[u8]) -> Result<PreKeySignalMessage, SignalMessageError>
{
    let mut pos = 0;
    let mut pre_key_id = None;
    let mut base_key = Vec::new();
    let mut identity_key = Vec::new();
    let mut inner = Vec::new();
    let mut registration_id = 0u32;
    let mut signed_pre_key_id = 0u32;

    while pos < bytes.len()
    {
        let (tag, used) = dec_varint(&bytes[pos..])?;
        pos += used;
        let field = (tag >> 3) as u32;
        let wire = (tag & 0x07) as u8;

        match (field, wire)
        {
            (1, WIRE_VARINT) =>
            {
                let (v, used) = dec_varint(&bytes[pos..])?;
                pos += used;
                pre_key_id = Some(v as u32);
            }
            (2, WIRE_LEN) =>
            {
                let (len, used) = dec_varint(&bytes[pos..])?;
                pos += used;
                let end = pos + len as usize;
                base_key = bytes.get(pos..end)
                    .ok_or_else(|| SignalMessageError::InvalidFormat("truncated base_key".into()))?
                    .to_vec();
                pos = end;
            }
            (3, WIRE_LEN) =>
            {
                let (len, used) = dec_varint(&bytes[pos..])?;
                pos += used;
                let end = pos + len as usize;
                identity_key = bytes.get(pos..end)
                    .ok_or_else(|| SignalMessageError::InvalidFormat("truncated identity_key".into()))?
                    .to_vec();
                pos = end;
            }
            (4, WIRE_LEN) =>
            {
                let (len, used) = dec_varint(&bytes[pos..])?;
                pos += used;
                let end = pos + len as usize;
                inner = bytes.get(pos..end)
                    .ok_or_else(|| SignalMessageError::InvalidFormat("truncated inner".into()))?
                    .to_vec();
                pos = end;
            }
            (5, WIRE_VARINT) =>
            {
                let (v, used) = dec_varint(&bytes[pos..])?;
                pos += used;
                registration_id = v as u32;
            }
            (6, WIRE_VARINT) =>
            {
                let (v, used) = dec_varint(&bytes[pos..])?;
                pos += used;
                signed_pre_key_id = v as u32;
            }
            (_, WIRE_VARINT) =>
            {
                let (_, used) = dec_varint(&bytes[pos..])?;
                pos += used;
            }
            (_, WIRE_LEN) =>
            {
                let (len, used) = dec_varint(&bytes[pos..])?;
                pos += used;
                pos = pos.saturating_add(len as usize).min(bytes.len());
            }
            (_, w) =>
            {
                return Err(SignalMessageError::InvalidFormat(format!("unsupported wire type {}", w)));
            }
        }
    }

    if base_key.is_empty() || identity_key.is_empty() || inner.is_empty()
    {
        return Err(SignalMessageError::InvalidFormat("missing prekey field".into()));
    }

    return Ok(PreKeySignalMessage
    {
        registration_id,
        pre_key_id,
        signed_pre_key_id,
        base_key_wire: base_key,
        identity_key_wire: identity_key,
        inner_message_bytes: inner,
    });
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn varint_roundtrip()
    {
        for v in [0u64, 1, 127, 128, 0xff, 0x7fff, 0xffff_ffff_ffff_ffff]
        {
            let mut buf = Vec::new();
            enc_varint(v, &mut buf);
            let (decoded, used) = dec_varint(&buf).unwrap();
            assert_eq!(decoded, v);
            assert_eq!(used, buf.len());
        }
    }

    #[test]
    fn whisper_message_proto_roundtrip()
    {
        let m = SignalMessage
        {
            ratchet_key_wire: vec![5; 33],
            counter: 7,
            previous_counter: 42,
            ciphertext: b"hello".to_vec(),
        };
        let bytes = encode_whisper_proto(&m);
        let m2 = decode_whisper_proto(&bytes).unwrap();
        assert_eq!(m, m2);
    }

    #[test]
    fn signal_message_envelope_roundtrip_and_mac()
    {
        let mac_key = [9u8; 32];
        let alice = [DJB_TYPE; 33];
        let bob = [0x06u8; 33];

        let m = SignalMessage
        {
            ratchet_key_wire: wire_key_33(&[1u8; 32]).to_vec(),
            counter: 0,
            previous_counter: 0,
            ciphertext: b"deadbeef".to_vec(),
        };

        let env = encode_signal_message(&mac_key, &alice, &bob, &m);
        let parsed = decode_signal_message(&env, &mac_key, &alice, &bob).unwrap();
        assert_eq!(parsed, m);
    }

    #[test]
    fn signal_message_rejects_bad_mac()
    {
        let mac_key = [9u8; 32];
        let alice = [DJB_TYPE; 33];
        let bob = [0x06u8; 33];
        let m = SignalMessage
        {
            ratchet_key_wire: wire_key_33(&[1u8; 32]).to_vec(),
            counter: 0,
            previous_counter: 0,
            ciphertext: b"x".to_vec(),
        };

        let mut env = encode_signal_message(&mac_key, &alice, &bob, &m);
        let last = env.len() - 1;
        env[last] ^= 1;
        assert!(decode_signal_message(&env, &mac_key, &alice, &bob).is_err());
    }

    #[test]
    fn prekey_message_envelope_roundtrip()
    {
        let inner = vec![0x33; 64];
        let m = PreKeySignalMessage
        {
            registration_id: 12345,
            pre_key_id: Some(7),
            signed_pre_key_id: 1,
            base_key_wire: wire_key_33(&[2u8; 32]).to_vec(),
            identity_key_wire: wire_key_33(&[3u8; 32]).to_vec(),
            inner_message_bytes: inner.clone(),
        };
        let bytes = encode_prekey_message(&m);
        let parsed = decode_prekey_message(&bytes).unwrap();
        assert_eq!(m, parsed);
    }

    #[test]
    fn derive_message_keys_lengths()
    {
        let (c, m, i) = derive_message_keys(&[5u8; 32]).unwrap();
        assert_eq!(c.len(), 32);
        assert_eq!(m.len(), 32);
        assert_eq!(i.len(), 16);
    }

    #[test]
    fn aes256_cbc_roundtrip()
    {
        let key = [11u8; 32];
        let iv = [22u8; 16];
        let pt = [9u8; 32]; // 32-byte OMEMO inner key+tag
        let ct = aes256_cbc_encrypt(&key, &iv, &pt);
        assert_eq!(ct.len(), 48); // padded out to next 16-byte block
        let pt2 = aes256_cbc_decrypt(&key, &iv, &ct).unwrap();
        assert_eq!(pt2, pt);
    }

    #[test]
    fn strip_djb_prefix_accepts_both()
    {
        let mut raw33 = vec![DJB_TYPE];
        raw33.extend_from_slice(&[1u8; 32]);
        assert_eq!(strip_djb_prefix(&raw33).unwrap(), [1u8; 32]);
        assert_eq!(strip_djb_prefix(&[1u8; 32]).unwrap(), [1u8; 32]);
        assert!(strip_djb_prefix(&[1u8; 31]).is_err());
    }

    #[test]
    fn version_byte_is_0x33()
    {
        assert_eq!(VERSION_BYTE, 0x33);
    }
}
