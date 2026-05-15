//! Per-peer-device OMEMO session: orchestrates the ratchet to wrap and
//! unwrap the inner 32-byte message key.

use serde::{ Deserialize, Serialize };

use super::crypto::{ aes128_gcm_decrypt, aes128_gcm_encrypt, CryptoError };
use super::ratchet::{ derive_message_aead, EncryptStep, RatchetError, RatchetState };

#[derive(Debug, thiserror::Error)]
pub enum SessionError
{
    #[error("crypto: {0}")]
    Crypto(#[from] CryptoError),
    #[error("ratchet: {0}")]
    Ratchet(#[from] RatchetError),
    #[error("unexpected key length")]
    BadKeyLength,
}

/// Per-(peer_jid, peer_device_id) session.
#[derive(Serialize, Deserialize, Clone)]
pub struct Session
{
    pub ratchet: RatchetState,
    /// Set on the initiator side until the responder sends back its
    /// first ratchet message (after which all subsequent outbound
    /// messages are plain OMEMO messages, not pre-key messages).
    pub pending_pre_key: Option<PendingPreKeyData>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct PendingPreKeyData
{
    pub registration_id: u32,
    pub used_signed_pre_key_id: u32,
    pub used_one_time_pre_key_id: Option<u32>,
    pub identity_key_pub: [u8; 32],
    pub ephemeral_key_pub: [u8; 32],
}

/// Output of [`Session::encrypt`].
pub struct EncryptedMessage
{
    /// 32-byte message key plus the chosen AES-GCM tag wrapped inside.
    /// In OMEMO this is the `<key>` element's binary contents per
    /// recipient device.
    pub wrapped_key: Vec<u8>,
    /// Sender DH public — goes into the header for the receiver to know
    /// which DH ratchet step to use.
    pub header_dh_pub: [u8; 32],
    pub n: u32,
    pub pn: u32,
}

/// Output of [`Session::decrypt`]: the recovered 32-byte message key.
pub struct IncomingMessage
{
    pub message_key: [u8; 32],
}

impl Session
{
    /// Wrap the given 32-byte `payload_key || payload_tag` for transport
    /// to the peer. Returns the wrapped bytes plus the ratchet header
    /// info to ship in the `<key>` element.
    pub fn encrypt(&mut self, message_key_with_tag: &[u8]) -> Result<EncryptedMessage, SessionError>
    {
        if message_key_with_tag.len() != 32
        {
            return Err(SessionError::BadKeyLength);
        }

        let EncryptStep { message_key, header_dh_pub, n, pn } = self.ratchet.encrypt()?;
        let (aes_key, nonce, _mac_key) = derive_message_aead(&message_key)?;

        let wrapped = aes128_gcm_encrypt(&aes_key, &nonce, message_key_with_tag)?;

        return Ok(EncryptedMessage { wrapped_key: wrapped, header_dh_pub, n, pn });
    }

    /// Unwrap a 32-byte key carried in an OMEMO `<key rid='..'>` element.
    pub fn decrypt(
        &mut self,
        header_dh_pub: [u8; 32],
        n: u32,
        pn: u32,
        wrapped: &[u8],
    ) -> Result<[u8; 32], SessionError>
    {
        let mk = self.ratchet.decrypt(header_dh_pub, n, pn)?;
        let (aes_key, nonce, _mac_key) = derive_message_aead(&mk)?;
        let pt = aes128_gcm_decrypt(&aes_key, &nonce, wrapped)?;

        if pt.len() != 32
        {
            return Err(SessionError::BadKeyLength);
        }

        let mut out = [0u8; 32];
        out.copy_from_slice(&pt);
        // First successful decrypt clears pending pre-key state.
        self.pending_pre_key = None;
        return Ok(out);
    }
}

#[cfg(test)]
mod tests
{
    use super::*;
    use crate::omemo::ratchet::RatchetState;
    use crate::omemo::x3dh::{ initiator_x3dh, responder_x3dh, PeerBundle, ResponderInputs };
    use crate::omemo::crypto::{ xeddsa_sign, X25519KeyPair };

    fn make_pair() -> (Session, Session)
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
        let alice_state = RatchetState::init_initiator(init.shared_secret, bob_spk.public_bytes()).unwrap();

        let sk = responder_x3dh(&ResponderInputs
        {
            identity: &bob_id,
            signed_pre_key: &bob_spk,
            one_time_pre_key: None,
            peer_identity_key: alice_id.public_bytes(),
            peer_ephemeral_key: init.ephemeral_key.public_bytes(),
        }).unwrap();

        let bob_state = RatchetState::init_responder(sk, bob_spk.private_bytes(), bob_spk.public_bytes());

        let alice = Session
        {
            ratchet: alice_state,
            pending_pre_key: Some(PendingPreKeyData
            {
                registration_id: 1,
                used_signed_pre_key_id: 1,
                used_one_time_pre_key_id: None,
                identity_key_pub: alice_id.public_bytes(),
                ephemeral_key_pub: init.ephemeral_key.public_bytes(),
            }),
        };
        let bob = Session { ratchet: bob_state, pending_pre_key: None };
        return (alice, bob);
    }

    #[test]
    fn encrypt_decrypt_message_key()
    {
        let (mut alice, mut bob) = make_pair();
        let mk = [9u8; 32];
        let out = alice.encrypt(&mk).unwrap();
        let recovered = bob.decrypt(out.header_dh_pub, out.n, out.pn, &out.wrapped_key).unwrap();
        assert_eq!(recovered, mk);
    }

    #[test]
    fn pending_pre_key_cleared_after_responder_message()
    {
        let (mut alice, mut bob) = make_pair();
        let out = alice.encrypt(&[1u8; 32]).unwrap();
        let _ = bob.decrypt(out.header_dh_pub, out.n, out.pn, &out.wrapped_key).unwrap();

        let reply = bob.encrypt(&[2u8; 32]).unwrap();
        assert!(alice.pending_pre_key.is_some());
        let _ = alice.decrypt(reply.header_dh_pub, reply.n, reply.pn, &reply.wrapped_key).unwrap();
        assert!(alice.pending_pre_key.is_none());
    }
}
