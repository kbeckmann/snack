//! OMEMO long-term identity material: identity key, signed pre-key, and
//! one-time pre-keys.

use serde::{ Deserialize, Serialize };
use zeroize::Zeroize;

use super::crypto::{ random_device_id, xeddsa_sign, X25519KeyPair };
use super::signal_message::wire_key_33;

/// Long-term identity key plus the device id we present to peers. Kept on
/// disk in [`OmemoStore`](super::store::OmemoStore).
#[derive(Clone)]
pub struct IdentityKeyPair
{
    pub device_id: u32,
    pub keypair: X25519KeyPair,
}

impl IdentityKeyPair
{
    pub fn generate() -> Self
    {
        return Self
        {
            device_id: random_device_id(),
            keypair: X25519KeyPair::generate(),
        };
    }

    pub fn from_parts(device_id: u32, private_key: [u8; 32]) -> Self
    {
        return Self
        {
            device_id,
            keypair: X25519KeyPair::from_private_bytes(&private_key),
        };
    }

    pub fn public_bytes(&self) -> [u8; 32]
    {
        return self.keypair.public_bytes();
    }
}

/// Serializable on-disk form of [`IdentityKeyPair`]. The private key is
/// stored in cleartext within the user's data directory; OMEMO doesn't
/// mandate encrypted-at-rest storage but the file permissions are 0600.
#[derive(Serialize, Deserialize, Clone, Zeroize)]
pub struct StoredIdentity
{
    pub device_id: u32,
    pub private_key: [u8; 32],
}

impl Drop for StoredIdentity
{
    fn drop(&mut self)
    {
        self.zeroize();
    }
}

impl From<&IdentityKeyPair> for StoredIdentity
{
    fn from(k: &IdentityKeyPair) -> Self
    {
        return Self
        {
            device_id: k.device_id,
            private_key: k.keypair.private_bytes(),
        };
    }
}

impl From<StoredIdentity> for IdentityKeyPair
{
    fn from(s: StoredIdentity) -> Self
    {
        let kp = X25519KeyPair::from_private_bytes(&s.private_key);
        return Self { device_id: s.device_id, keypair: kp };
    }
}

/// A signed pre-key. The signature is an XEdDSA signature made by the
/// identity key over the signed pre-key public bytes.
#[derive(Clone)]
pub struct SignedPreKey
{
    pub id: u32,
    pub keypair: X25519KeyPair,
    pub signature: [u8; 64],
}

impl SignedPreKey
{
    pub fn generate(id: u32, identity: &IdentityKeyPair) -> Self
    {
        let kp = X25519KeyPair::generate();
        // Sign over the wire-form (DJB-prefixed) public key — this is
        // the layout libsignal expects so other OMEMO clients verify it.
        let wire = wire_key_33(&kp.public_bytes());
        let signature = xeddsa_sign(&identity.keypair.private_bytes(), &wire);
        return Self { id, keypair: kp, signature };
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct StoredSignedPreKey
{
    pub id: u32,
    pub private_key: [u8; 32],
    /// Stored as 64 bytes; kept as Vec since serde can't derive for `[u8; 64]`.
    #[serde(with = "serde_bytes")]
    pub signature: Vec<u8>,
}

impl From<&SignedPreKey> for StoredSignedPreKey
{
    fn from(spk: &SignedPreKey) -> Self
    {
        return Self
        {
            id: spk.id,
            private_key: spk.keypair.private_bytes(),
            signature: spk.signature.to_vec(),
        };
    }
}

impl From<StoredSignedPreKey> for SignedPreKey
{
    fn from(s: StoredSignedPreKey) -> Self
    {
        let mut sig = [0u8; 64];
        sig.copy_from_slice(&s.signature[..64]);
        return Self
        {
            id: s.id,
            keypair: X25519KeyPair::from_private_bytes(&s.private_key),
            signature: sig,
        };
    }
}

/// A one-time pre-key: consumed when a peer first establishes a session.
#[derive(Clone)]
pub struct OneTimePreKey
{
    pub id: u32,
    pub keypair: X25519KeyPair,
}

impl OneTimePreKey
{
    pub fn generate(id: u32) -> Self
    {
        return Self { id, keypair: X25519KeyPair::generate() };
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct StoredOneTimePreKey
{
    pub id: u32,
    pub private_key: [u8; 32],
}

impl From<&OneTimePreKey> for StoredOneTimePreKey
{
    fn from(p: &OneTimePreKey) -> Self
    {
        return Self { id: p.id, private_key: p.keypair.private_bytes() };
    }
}

impl From<StoredOneTimePreKey> for OneTimePreKey
{
    fn from(s: StoredOneTimePreKey) -> Self
    {
        return Self
        {
            id: s.id,
            keypair: X25519KeyPair::from_private_bytes(&s.private_key),
        };
    }
}

/// Number of one-time pre-keys to maintain per device (XEP-0384 §4.3
/// recommends maintaining around 100).
pub const OTPK_BATCH_SIZE: u32 = 100;

#[cfg(test)]
mod tests
{
    use super::*;
    use crate::omemo::crypto::xeddsa_verify;

    #[test]
    fn signed_pre_key_signature_verifies()
    {
        let id = IdentityKeyPair::generate();
        let spk = SignedPreKey::generate(1, &id);

        let wire = wire_key_33(&spk.keypair.public_bytes());
        xeddsa_verify(
            &id.keypair.public_bytes(),
            &wire,
            &spk.signature,
        )
        .expect("signed prekey signature should verify under the identity key (wire form)");
    }

    #[test]
    fn round_trip_identity()
    {
        let id = IdentityKeyPair::generate();
        let stored: StoredIdentity = (&id).into();
        let reloaded: IdentityKeyPair = stored.clone().into();
        assert_eq!(id.device_id, reloaded.device_id);
        assert_eq!(id.keypair.public_bytes(), reloaded.keypair.public_bytes());
    }

    #[test]
    fn round_trip_signed_pre_key()
    {
        let id = IdentityKeyPair::generate();
        let spk = SignedPreKey::generate(1, &id);
        let stored: StoredSignedPreKey = (&spk).into();
        let reloaded: SignedPreKey = stored.into();
        assert_eq!(spk.keypair.public_bytes(), reloaded.keypair.public_bytes());
        assert_eq!(spk.signature, reloaded.signature);
    }
}
