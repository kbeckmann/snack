//! Cryptographic primitives used by the OMEMO module.
//!
//! Wraps:
//!
//! - X25519 ECDH (`x25519-dalek`)
//! - XEdDSA signatures (Signal's curve25519 signature scheme, implemented
//!   on top of `curve25519-dalek` + `ed25519-dalek`)
//! - HKDF-SHA256, HMAC-SHA256 (`hkdf`, `hmac`)
//! - AES-128-GCM (`aes-gcm`)
//!
//! All keys are encoded compact-public-form (32 bytes for Curve25519).

use aes_gcm::aead::{ Aead, KeyInit };
use aes_gcm::{ Aes128Gcm, Nonce };
use curve25519_dalek::edwards::{ CompressedEdwardsY, EdwardsPoint };
use curve25519_dalek::scalar::Scalar;
use ed25519_dalek::{ Signature, VerifyingKey };
use hkdf::Hkdf;
use hmac::{ Hmac, Mac };
use rand::{ rngs::OsRng, RngCore };
use sha2::{ Digest, Sha256, Sha512 };
use x25519_dalek::{ PublicKey, StaticSecret };
use zeroize::Zeroize;

pub type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError
{
    #[error("invalid key length")]
    InvalidKeyLength,
    #[error("invalid signature")]
    InvalidSignature,
    #[error("aead failure: {0}")]
    Aead(String),
    #[error("invalid public key encoding")]
    InvalidPublicKey,
    #[error("hkdf failure")]
    Hkdf,
}

/// Generate `len` cryptographically secure random bytes.
pub fn random_bytes(len: usize) -> Vec<u8>
{
    let mut buf = vec![0u8; len];
    OsRng.fill_bytes(&mut buf);
    return buf;
}

/// Generate a random 32-bit integer suitable for OMEMO device IDs.
/// Device IDs must be non-zero (XEP-0384 §4.2).
pub fn random_device_id() -> u32
{
    loop
    {
        let n = OsRng.next_u32();
        if n != 0
        {
            return n;
        }
    }
}

pub fn random_uint(bound: u32) -> u32
{
    return OsRng.next_u32() % bound;
}

/// X25519 key pair using `x25519-dalek::StaticSecret`. The private key is
/// 32 bytes of uniformly random material clamped on use; the public key
/// is the X25519 derivation.
#[derive(Clone)]
pub struct X25519KeyPair
{
    secret: StaticSecret,
    public: PublicKey,
}

impl X25519KeyPair
{
    pub fn generate() -> Self
    {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret);
        return Self { secret, public };
    }

    pub fn from_private_bytes(bytes: &[u8; 32]) -> Self
    {
        let secret = StaticSecret::from(*bytes);
        let public = PublicKey::from(&secret);
        return Self { secret, public };
    }

    pub fn public_bytes(&self) -> [u8; 32]
    {
        return self.public.to_bytes();
    }

    pub fn private_bytes(&self) -> [u8; 32]
    {
        return self.secret.to_bytes();
    }

    pub fn diffie_hellman(&self, peer_public: &[u8; 32]) -> [u8; 32]
    {
        let peer = PublicKey::from(*peer_public);
        return self.secret.diffie_hellman(&peer).to_bytes();
    }
}

/// HKDF-SHA256 expand-extract. Returns `out_len` bytes.
pub fn hkdf_sha256(salt: &[u8], ikm: &[u8], info: &[u8], out_len: usize) -> Result<Vec<u8>, CryptoError>
{
    let h = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut out = vec![0u8; out_len];
    h.expand(info, &mut out).map_err(|_| CryptoError::Hkdf)?;
    return Ok(out);
}

/// HMAC-SHA256.
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32]
{
    use hmac::Mac as _;
    let mut mac = <HmacSha256 as hmac::Mac>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(data);
    let bytes = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    return out;
}

/// AES-128-GCM encrypt. Returns `ciphertext || 16-byte tag`. Caller
/// supplies a 12-byte nonce (NIST SP 800-38D recommended size).
pub fn aes128_gcm_encrypt(key: &[u8; 16], nonce: &[u8; 12], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError>
{
    let cipher = Aes128Gcm::new_from_slice(key).map_err(|_| CryptoError::InvalidKeyLength)?;
    let ct = cipher.encrypt(Nonce::from_slice(nonce), plaintext)
        .map_err(|e| CryptoError::Aead(e.to_string()))?;
    return Ok(ct);
}

/// AES-128-GCM decrypt. `ciphertext_with_tag` must end with the GCM auth
/// tag (the standard `aes-gcm` crate format).
pub fn aes128_gcm_decrypt(
    key: &[u8; 16],
    nonce: &[u8; 12],
    ciphertext_with_tag: &[u8],
) -> Result<Vec<u8>, CryptoError>
{
    let cipher = Aes128Gcm::new_from_slice(key).map_err(|_| CryptoError::InvalidKeyLength)?;
    let pt = cipher.decrypt(Nonce::from_slice(nonce), ciphertext_with_tag)
        .map_err(|e| CryptoError::Aead(e.to_string()))?;
    return Ok(pt);
}

// =====================================================================
// XEdDSA signatures (Signal spec, "X-Ed25519 from Curve25519").
//
// The Signal Protocol uses XEdDSA to sign messages with a Curve25519
// long-term key. The public key carried on the wire is the Curve25519
// (Montgomery) form so the same key can be used for both DH and signing.
// =====================================================================

/// XEdDSA-sign a message with a Curve25519 private key (32 raw bytes).
/// Output: 64-byte signature.
pub fn xeddsa_sign(private_key_x25519: &[u8; 32], message: &[u8]) -> [u8; 64]
{
    // The conversion from a clamped Curve25519 scalar to an Ed25519 scalar:
    //   k = SHA512(private_key)[..32] would be the EdDSA derivation
    // but XEdDSA mandates direct use of the clamped scalar from the X25519
    // private key. We reproduce the spec algorithm here.

    // Step 1: clamp scalar
    let mut a_bytes = *private_key_x25519;
    a_bytes[0] &= 248;
    a_bytes[31] &= 127;
    a_bytes[31] |= 64;
    let a = Scalar::from_bytes_mod_order(a_bytes);

    // Step 2: derive public Edwards point A = a * G (basepoint).
    let big_a = EdwardsPoint::mul_base(&a);
    let mut big_a_bytes = big_a.compress().to_bytes();

    // Step 3: ensure A is on the positive Edwards side. XEdDSA encodes
    // the sign so that the receiver who only has the Montgomery x-coord
    // can pick the matching Edwards form. We force sign = 0 (high bit
    // clear) and remember whether we flipped, since the scalar must be
    // negated if so.
    let sign_bit = big_a_bytes[31] & 0x80;
    big_a_bytes[31] &= 0x7f;

    let a_signed = if sign_bit != 0 { -a } else { a };

    // Step 4: nonce r = SHA512(0xFE || 0xFF * 31 || a || M || Z)  where Z
    // is 64 random bytes (XEdDSA-randomized variant). We follow the
    // standard spec: r = SHA512(a || M || Z).
    let mut z = [0u8; 64];
    OsRng.fill_bytes(&mut z);

    let mut hasher = Sha512::new();
    // Domain separation prefix (per XEdDSA spec algorithm 1)
    hasher.update([0xfe]);
    hasher.update([0xff; 31]);
    hasher.update(a_signed.to_bytes());
    hasher.update(message);
    hasher.update(z);
    let r_hash = hasher.finalize();
    let mut r_buf = [0u8; 64];
    r_buf.copy_from_slice(&r_hash);
    let r = Scalar::from_bytes_mod_order_wide(&r_buf);

    // Step 5: R = r * G
    let big_r = EdwardsPoint::mul_base(&r);
    let big_r_bytes = big_r.compress().to_bytes();

    // Step 6: k = SHA512(R || A || M) mod q
    let mut hasher = Sha512::new();
    hasher.update(big_r_bytes);
    hasher.update(big_a_bytes);
    hasher.update(message);
    let k_hash = hasher.finalize();
    let mut k_buf = [0u8; 64];
    k_buf.copy_from_slice(&k_hash);
    let k = Scalar::from_bytes_mod_order_wide(&k_buf);

    // Step 7: s = r + k * a
    let s = r + k * a_signed;

    let mut sig = [0u8; 64];
    sig[..32].copy_from_slice(&big_r_bytes);
    sig[32..].copy_from_slice(s.as_bytes());

    a_bytes.zeroize();

    return sig;
}

/// Verify an XEdDSA signature over `message` made by the holder of the
/// X25519 private key whose public form is `public_key_x25519`.
pub fn xeddsa_verify(public_key_x25519: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> Result<(), CryptoError>
{
    // Convert Montgomery u-coordinate to a positive Edwards y-coordinate.
    let mont = curve25519_dalek::montgomery::MontgomeryPoint(*public_key_x25519);
    let edwards = mont
        .to_edwards(0)
        .ok_or(CryptoError::InvalidPublicKey)?;

    // Build the Ed25519 public key from the compressed Edwards bytes.
    let big_a_bytes = edwards.compress().to_bytes();
    let vk = VerifyingKey::from_bytes(&big_a_bytes).map_err(|_| CryptoError::InvalidPublicKey)?;

    let sig = Signature::from_bytes(signature);
    vk.verify_strict(message, &sig).map_err(|_| CryptoError::InvalidSignature)?;

    return Ok(());
}

/// Compute SHA-256 digest (used for fingerprint construction).
pub fn sha256(data: &[u8]) -> [u8; 32]
{
    let mut h = Sha256::new();
    h.update(data);
    let out = h.finalize();
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&out);
    return buf;
}

/// Constant-time compare of two byte slices of equal length.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool
{
    use subtle::ConstantTimeEq;
    return a.ct_eq(b).into();
}

/// Decompress a Curve25519 public key from its 32-byte X25519 form. Used
/// during XEdDSA signature verification.
#[allow(dead_code)]
pub fn x25519_public_to_edwards(public_key_x25519: &[u8; 32]) -> Option<EdwardsPoint>
{
    let mont = curve25519_dalek::montgomery::MontgomeryPoint(*public_key_x25519);
    return mont.to_edwards(0);
}

#[allow(dead_code)]
pub fn edwards_to_x25519(edwards: &EdwardsPoint) -> [u8; 32]
{
    return edwards.to_montgomery().to_bytes();
}

#[allow(dead_code)]
pub fn try_decompress_edwards(bytes: &[u8; 32]) -> Option<EdwardsPoint>
{
    return CompressedEdwardsY(*bytes).decompress();
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn dh_is_symmetric()
    {
        let alice = X25519KeyPair::generate();
        let bob = X25519KeyPair::generate();

        let ab = alice.diffie_hellman(&bob.public_bytes());
        let ba = bob.diffie_hellman(&alice.public_bytes());

        assert_eq!(ab, ba);
        assert_eq!(ab.len(), 32);
    }

    #[test]
    fn hkdf_deterministic()
    {
        let a = hkdf_sha256(b"salt", b"ikm", b"info", 64).unwrap();
        let b = hkdf_sha256(b"salt", b"ikm", b"info", 64).unwrap();
        assert_eq!(a, b);
        assert_ne!(
            a,
            hkdf_sha256(b"salt", b"ikm2", b"info", 64).unwrap()
        );
    }

    #[test]
    fn hmac_sha256_known_answer()
    {
        // RFC 4231 test vector 1:
        //   key = 20 bytes of 0x0b, data = "Hi There"
        //   expected = b0344c61d8db38535ca8afceaf0bf12b
        //              881dc200c9833da726e9376c2e32cff7
        let key = [0x0b_u8; 20];
        let mac = hmac_sha256(&key, b"Hi There");
        assert_eq!(
            mac,
            [
                0xb0, 0x34, 0x4c, 0x61, 0xd8, 0xdb, 0x38, 0x53,
                0x5c, 0xa8, 0xaf, 0xce, 0xaf, 0x0b, 0xf1, 0x2b,
                0x88, 0x1d, 0xc2, 0x00, 0xc9, 0x83, 0x3d, 0xa7,
                0x26, 0xe9, 0x37, 0x6c, 0x2e, 0x32, 0xcf, 0xf7,
            ]
        );
    }

    #[test]
    fn aes128_gcm_roundtrip()
    {
        let key = [42u8; 16];
        let nonce = [7u8; 12];
        let plaintext = b"hello omemo";
        let ct = aes128_gcm_encrypt(&key, &nonce, plaintext).unwrap();
        let pt = aes128_gcm_decrypt(&key, &nonce, &ct).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn aes128_gcm_tampering_fails()
    {
        let key = [42u8; 16];
        let nonce = [7u8; 12];
        let mut ct = aes128_gcm_encrypt(&key, &nonce, b"some message").unwrap();
        ct[0] ^= 1;
        assert!(aes128_gcm_decrypt(&key, &nonce, &ct).is_err());
    }

    #[test]
    fn xeddsa_sign_and_verify()
    {
        let kp = X25519KeyPair::generate();
        let priv_bytes = kp.private_bytes();
        let pub_bytes = kp.public_bytes();

        let msg = b"sign me";
        let sig = xeddsa_sign(&priv_bytes, msg);
        xeddsa_verify(&pub_bytes, msg, &sig).unwrap();
    }

    #[test]
    fn xeddsa_verify_rejects_wrong_message()
    {
        let kp = X25519KeyPair::generate();
        let sig = xeddsa_sign(&kp.private_bytes(), b"original");
        assert!(xeddsa_verify(&kp.public_bytes(), b"tampered", &sig).is_err());
    }

    #[test]
    fn xeddsa_verify_rejects_wrong_signer()
    {
        let signer = X25519KeyPair::generate();
        let other = X25519KeyPair::generate();
        let sig = xeddsa_sign(&signer.private_bytes(), b"hello");
        assert!(xeddsa_verify(&other.public_bytes(), b"hello", &sig).is_err());
    }

    #[test]
    fn xeddsa_sign_is_randomized_but_verifies()
    {
        let kp = X25519KeyPair::generate();
        let s1 = xeddsa_sign(&kp.private_bytes(), b"x");
        let s2 = xeddsa_sign(&kp.private_bytes(), b"x");
        // Randomized signing should not produce identical signatures (with overwhelming probability).
        assert_ne!(s1, s2);
        xeddsa_verify(&kp.public_bytes(), b"x", &s1).unwrap();
        xeddsa_verify(&kp.public_bytes(), b"x", &s2).unwrap();
    }
}
