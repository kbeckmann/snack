//! Double Ratchet (Signal Double Ratchet spec rev. 2016-11-20).
//!
//! State machine:
//!
//! - `root_key` (RK) is updated each time we perform a DH ratchet step.
//! - `sending_chain_key` (CKs) advances per message we send.
//! - `receiving_chain_key` (CKr) advances per message we receive.
//! - When a peer sends a new DH public key, we run a DH ratchet step:
//!   the new RK and a fresh receiving chain are derived from the new DH
//!   output; we then generate our own DH key and derive a new sending
//!   chain.
//!
//! Skipped message keys are cached in `skipped_keys` so out-of-order
//! messages can still decrypt.

use serde::{ Deserialize, Serialize };
use std::collections::HashMap;
use zeroize::Zeroize;

use super::crypto::{ hkdf_sha256, hmac_sha256, CryptoError, X25519KeyPair };

pub const RATCHET_INFO: &[u8] = b"WhisperRatchet";
pub const CHAIN_INFO: &[u8] = b"WhisperMessageKeys";

const MAX_SKIP: u32 = 1000;

#[derive(Debug, thiserror::Error)]
pub enum RatchetError
{
    #[error("crypto: {0}")]
    Crypto(#[from] CryptoError),
    #[error("too many skipped messages")]
    TooManySkipped,
    #[error("message authentication failed (mac mismatch)")]
    AuthFailure,
    #[error("invalid state")]
    InvalidState,
}

/// The serializable Double Ratchet state.
#[derive(Serialize, Deserialize, Clone)]
pub struct RatchetState
{
    pub root_key: [u8; 32],
    /// Our current DH key pair (private+public). Rotated each DH step.
    pub dh_self_private: [u8; 32],
    pub dh_self_public: [u8; 32],
    /// Peer's most recently received DH public key. `None` only on the
    /// initiator side prior to receiving Bob's first reply.
    pub dh_remote: Option<[u8; 32]>,
    pub sending_chain_key: Option<[u8; 32]>,
    pub receiving_chain_key: Option<[u8; 32]>,
    pub n_send: u32,
    pub n_recv: u32,
    pub pn: u32,
    /// Skipped message keys, keyed by `(dh_remote_pub, n)`.
    pub skipped_keys: HashMap<SkippedKey, [u8; 32]>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash)]
pub struct SkippedKey
{
    pub dh_pub: [u8; 32],
    pub n: u32,
}

impl Drop for RatchetState
{
    fn drop(&mut self)
    {
        self.root_key.zeroize();
        self.dh_self_private.zeroize();
        if let Some(ck) = self.sending_chain_key.as_mut() { ck.zeroize(); }
        if let Some(ck) = self.receiving_chain_key.as_mut() { ck.zeroize(); }
        for v in self.skipped_keys.values_mut() { v.zeroize(); }
    }
}

/// Output of `RatchetState::encrypt`: the message key plus the header
/// (sending DH public + counters) that must accompany the ciphertext.
pub struct EncryptStep
{
    pub message_key: [u8; 32],
    pub header_dh_pub: [u8; 32],
    pub n: u32,
    pub pn: u32,
}

impl RatchetState
{
    /// Initialize as the initiator after X3DH. We immediately perform our
    /// first DH ratchet step using the responder's signed pre-key as the
    /// "remote DH" so the first message goes out under a fresh sending
    /// chain.
    pub fn init_initiator(
        shared_secret: [u8; 32],
        responder_dh_public: [u8; 32],
    ) -> Result<Self, RatchetError>
    {
        let dh_self = X25519KeyPair::generate();
        let dh_out = dh_self.diffie_hellman(&responder_dh_public);
        let (rk, ck) = kdf_rk(&shared_secret, &dh_out)?;

        return Ok(Self
        {
            root_key: rk,
            dh_self_private: dh_self.private_bytes(),
            dh_self_public: dh_self.public_bytes(),
            dh_remote: Some(responder_dh_public),
            sending_chain_key: Some(ck),
            receiving_chain_key: None,
            n_send: 0,
            n_recv: 0,
            pn: 0,
            skipped_keys: HashMap::new(),
        });
    }

    /// Initialize as the responder before receiving the first message.
    /// Our DH key pair is the responder's signed pre-key (so the first
    /// initiator step's DH output is `DH(initiator_eph, our_spk)`).
    pub fn init_responder(
        shared_secret: [u8; 32],
        responder_dh_private: [u8; 32],
        responder_dh_public: [u8; 32],
    ) -> Self
    {
        return Self
        {
            root_key: shared_secret,
            dh_self_private: responder_dh_private,
            dh_self_public: responder_dh_public,
            dh_remote: None,
            sending_chain_key: None,
            receiving_chain_key: None,
            n_send: 0,
            n_recv: 0,
            pn: 0,
            skipped_keys: HashMap::new(),
        };
    }

    /// Derive the next message key for sending and advance the chain.
    pub fn encrypt(&mut self) -> Result<EncryptStep, RatchetError>
    {
        let ck = self.sending_chain_key.ok_or(RatchetError::InvalidState)?;
        let (next_ck, mk) = kdf_ck(&ck);
        self.sending_chain_key = Some(next_ck);
        let n = self.n_send;
        self.n_send += 1;

        return Ok(EncryptStep
        {
            message_key: mk,
            header_dh_pub: self.dh_self_public,
            n,
            pn: self.pn,
        });
    }

    /// Decrypt a message using the carried header information. Returns
    /// the message key for the receiver to use.
    pub fn decrypt(
        &mut self,
        header_dh_pub: [u8; 32],
        header_n: u32,
        header_pn: u32,
    ) -> Result<[u8; 32], RatchetError>
    {
        if let Some(mk) = self.skipped_keys.remove(&SkippedKey { dh_pub: header_dh_pub, n: header_n })
        {
            return Ok(mk);
        }

        if self.dh_remote != Some(header_dh_pub)
        {
            self.skip_message_keys(header_pn)?;
            self.dh_ratchet(header_dh_pub)?;
        }

        self.skip_message_keys(header_n)?;

        let ck = self.receiving_chain_key.ok_or(RatchetError::InvalidState)?;
        let (next_ck, mk) = kdf_ck(&ck);
        self.receiving_chain_key = Some(next_ck);
        self.n_recv += 1;

        return Ok(mk);
    }

    fn skip_message_keys(&mut self, until: u32) -> Result<(), RatchetError>
    {
        if self.receiving_chain_key.is_none() { return Ok(()); }

        if until.saturating_sub(self.n_recv) > MAX_SKIP
        {
            return Err(RatchetError::TooManySkipped);
        }

        while self.n_recv < until
        {
            let ck = self.receiving_chain_key.unwrap();
            let (next_ck, mk) = kdf_ck(&ck);
            self.receiving_chain_key = Some(next_ck);
            if let Some(remote) = self.dh_remote
            {
                self.skipped_keys.insert(
                    SkippedKey { dh_pub: remote, n: self.n_recv },
                    mk,
                );
            }
            self.n_recv += 1;
        }

        return Ok(());
    }

    fn dh_ratchet(&mut self, new_remote_pub: [u8; 32]) -> Result<(), RatchetError>
    {
        self.pn = self.n_send;
        self.n_send = 0;
        self.n_recv = 0;
        self.dh_remote = Some(new_remote_pub);

        let dh_self = X25519KeyPair::from_private_bytes(&self.dh_self_private);
        let dh_out_recv = dh_self.diffie_hellman(&new_remote_pub);
        let (new_rk, new_ck_recv) = kdf_rk(&self.root_key, &dh_out_recv)?;
        self.root_key = new_rk;
        self.receiving_chain_key = Some(new_ck_recv);

        let new_dh = X25519KeyPair::generate();
        let dh_out_send = new_dh.diffie_hellman(&new_remote_pub);
        let (new_rk2, new_ck_send) = kdf_rk(&self.root_key, &dh_out_send)?;
        self.root_key = new_rk2;
        self.dh_self_private = new_dh.private_bytes();
        self.dh_self_public = new_dh.public_bytes();
        self.sending_chain_key = Some(new_ck_send);

        return Ok(());
    }
}

fn kdf_rk(rk: &[u8; 32], dh_out: &[u8; 32]) -> Result<([u8; 32], [u8; 32]), CryptoError>
{
    let out = hkdf_sha256(rk, dh_out, RATCHET_INFO, 64)?;
    let mut new_rk = [0u8; 32];
    let mut new_ck = [0u8; 32];
    new_rk.copy_from_slice(&out[..32]);
    new_ck.copy_from_slice(&out[32..]);
    return Ok((new_rk, new_ck));
}

fn kdf_ck(ck: &[u8; 32]) -> ([u8; 32], [u8; 32])
{
    let next_ck = hmac_sha256(ck, &[0x02]);
    let mk = hmac_sha256(ck, &[0x01]);
    return (next_ck, mk);
}

/// Derive AES-128-GCM key and nonce from the 32-byte ratchet message key.
///   key = HKDF(mk, info=CHAIN_INFO, 16 bytes)
///   nonce = HKDF(mk, info=CHAIN_INFO || "iv", 12 bytes)
///
/// (OMEMO legacy doesn't standardise this — different clients have used
/// different layouts. The shape used here is documented and tested for
/// self-consistency; cross-client compatibility for the AES key/IV layout
/// is part of the unverified surface.)
pub fn derive_message_aead(mk: &[u8; 32]) -> Result<([u8; 16], [u8; 12], [u8; 32]), CryptoError>
{
    let okm = hkdf_sha256(&[0u8; 32], mk, CHAIN_INFO, 60)?;
    let mut key = [0u8; 16];
    let mut nonce = [0u8; 12];
    let mut mac_key = [0u8; 32];
    key.copy_from_slice(&okm[..16]);
    nonce.copy_from_slice(&okm[16..28]);
    mac_key.copy_from_slice(&okm[28..60]);
    return Ok((key, nonce, mac_key));
}

#[cfg(test)]
mod tests
{
    use super::*;
    use crate::omemo::x3dh::{ initiator_x3dh, responder_x3dh, PeerBundle, ResponderInputs };
    use crate::omemo::crypto::{ xeddsa_sign, X25519KeyPair };

    /// End-to-end ratchet test: do an X3DH agreement, initialize both
    /// sides, and exchange several messages in both directions.
    #[test]
    fn ratchet_exchange_in_order()
    {
        let alice_id = X25519KeyPair::generate();
        let bob_id = X25519KeyPair::generate();
        let bob_spk = X25519KeyPair::generate();
        let bob_opk = X25519KeyPair::generate();
        let sig = xeddsa_sign(&bob_id.private_bytes(), &bob_spk.public_bytes());

        let bundle = PeerBundle
        {
            identity_key: bob_id.public_bytes(),
            signed_pre_key_id: 1,
            signed_pre_key: bob_spk.public_bytes(),
            signed_pre_key_sig: sig,
            one_time_pre_key: Some((1, bob_opk.public_bytes())),
        };

        let init = initiator_x3dh(&alice_id, &bundle).unwrap();

        let mut alice_state =
            RatchetState::init_initiator(init.shared_secret, bob_spk.public_bytes()).unwrap();

        let sk_bob = responder_x3dh(&ResponderInputs
        {
            identity: &bob_id,
            signed_pre_key: &bob_spk,
            one_time_pre_key: Some(&bob_opk),
            peer_identity_key: alice_id.public_bytes(),
            peer_ephemeral_key: init.ephemeral_key.public_bytes(),
        })
        .unwrap();

        let mut bob_state = RatchetState::init_responder(
            sk_bob,
            bob_spk.private_bytes(),
            bob_spk.public_bytes(),
        );

        // Alice -> Bob (first message under Alice's initial sending chain).
        let s1 = alice_state.encrypt().unwrap();
        let r1 = bob_state.decrypt(s1.header_dh_pub, s1.n, s1.pn).unwrap();
        assert_eq!(s1.message_key, r1);

        // Bob -> Alice (Bob must DH-ratchet first).
        let s2 = bob_state.encrypt().unwrap();
        let r2 = alice_state.decrypt(s2.header_dh_pub, s2.n, s2.pn).unwrap();
        assert_eq!(s2.message_key, r2);

        // Several more messages in both directions.
        for _ in 0..3
        {
            let s = alice_state.encrypt().unwrap();
            let r = bob_state.decrypt(s.header_dh_pub, s.n, s.pn).unwrap();
            assert_eq!(s.message_key, r);
        }

        for _ in 0..3
        {
            let s = bob_state.encrypt().unwrap();
            let r = alice_state.decrypt(s.header_dh_pub, s.n, s.pn).unwrap();
            assert_eq!(s.message_key, r);
        }
    }

    #[test]
    fn ratchet_handles_out_of_order()
    {
        let alice_id = X25519KeyPair::generate();
        let bob_id = X25519KeyPair::generate();
        let bob_spk = X25519KeyPair::generate();
        let sig = xeddsa_sign(&bob_id.private_bytes(), &bob_spk.public_bytes());

        let bundle = PeerBundle
        {
            identity_key: bob_id.public_bytes(),
            signed_pre_key_id: 1,
            signed_pre_key: bob_spk.public_bytes(),
            signed_pre_key_sig: sig,
            one_time_pre_key: None,
        };

        let init = initiator_x3dh(&alice_id, &bundle).unwrap();
        let mut alice = RatchetState::init_initiator(init.shared_secret, bob_spk.public_bytes()).unwrap();

        let sk = responder_x3dh(&ResponderInputs
        {
            identity: &bob_id,
            signed_pre_key: &bob_spk,
            one_time_pre_key: None,
            peer_identity_key: alice_id.public_bytes(),
            peer_ephemeral_key: init.ephemeral_key.public_bytes(),
        }).unwrap();

        let mut bob = RatchetState::init_responder(sk, bob_spk.private_bytes(), bob_spk.public_bytes());

        // Alice sends three messages; Bob receives them out of order.
        let m1 = alice.encrypt().unwrap();
        let m2 = alice.encrypt().unwrap();
        let m3 = alice.encrypt().unwrap();

        // Out-of-order: 3, 1, 2.
        let r3 = bob.decrypt(m3.header_dh_pub, m3.n, m3.pn).unwrap();
        assert_eq!(r3, m3.message_key);

        let r1 = bob.decrypt(m1.header_dh_pub, m1.n, m1.pn).unwrap();
        assert_eq!(r1, m1.message_key);

        let r2 = bob.decrypt(m2.header_dh_pub, m2.n, m2.pn).unwrap();
        assert_eq!(r2, m2.message_key);
    }

    #[test]
    fn ratchet_derives_distinct_keys()
    {
        // Sanity: consecutive message keys differ.
        let alice_id = X25519KeyPair::generate();
        let bob_id = X25519KeyPair::generate();
        let bob_spk = X25519KeyPair::generate();
        let sig = xeddsa_sign(&bob_id.private_bytes(), &bob_spk.public_bytes());

        let bundle = PeerBundle
        {
            identity_key: bob_id.public_bytes(),
            signed_pre_key_id: 1,
            signed_pre_key: bob_spk.public_bytes(),
            signed_pre_key_sig: sig,
            one_time_pre_key: None,
        };

        let init = initiator_x3dh(&alice_id, &bundle).unwrap();
        let mut alice = RatchetState::init_initiator(init.shared_secret, bob_spk.public_bytes()).unwrap();

        let m1 = alice.encrypt().unwrap();
        let m2 = alice.encrypt().unwrap();
        assert_ne!(m1.message_key, m2.message_key);
    }
}
