//! X3DH key agreement (Signal X3DH spec rev. 2016-11-04, adapted for
//! OMEMO).
//!
//! Two-party key agreement: Alice (initiator) wants to send Bob (responder)
//! an encrypted message. Bob has published a bundle containing his identity
//! key (IK_B), a signed pre-key (SPK_B), and optionally a one-time pre-key
//! (OPK_B). Alice generates an ephemeral key (EK_A) and computes the four
//! DH outputs:
//!
//! - DH1 = DH(IK_A, SPK_B)
//! - DH2 = DH(EK_A, IK_B)
//! - DH3 = DH(EK_A, SPK_B)
//! - DH4 = DH(EK_A, OPK_B)    // only if OPK_B is present
//!
//! The shared secret SK is HKDF over `KDF_PREFIX || DH1 || DH2 || DH3 [|| DH4]`.
//!
//! Bob runs the symmetric computation when he receives the first PreKey
//! message: he learns EK_A from the message and identifies which SPK and
//! OPK to use from the embedded ids.

use super::crypto::{ hkdf_sha256, xeddsa_verify, CryptoError, X25519KeyPair };
use super::signal_message::wire_key_33;

/// HKDF "info" string. OMEMO legacy follows libsignal which uses "WhisperText".
pub const KDF_INFO: &[u8] = b"WhisperText";

/// 32-byte F prefix prepended before the DH outputs to disambiguate
/// versions (Signal X3DH spec §3.3).
const KDF_PREFIX: [u8; 32] = [0xFFu8; 32];

#[derive(Debug, thiserror::Error)]
pub enum X3dhError
{
    #[error("crypto: {0}")]
    Crypto(#[from] CryptoError),
    #[error("invalid signed prekey signature")]
    InvalidSignedPreKeySignature,
}

/// Public bundle of a responder device that an initiator needs to start
/// a session.
#[derive(Debug, Clone)]
pub struct PeerBundle
{
    pub identity_key: [u8; 32],
    pub signed_pre_key_id: u32,
    pub signed_pre_key: [u8; 32],
    pub signed_pre_key_sig: [u8; 64],
    pub one_time_pre_key: Option<(u32, [u8; 32])>,
}

/// Initiator-side X3DH output.
pub struct InitiatorAgreement
{
    pub shared_secret: [u8; 32],
    pub ephemeral_key: X25519KeyPair,
    pub used_signed_pre_key_id: u32,
    pub used_one_time_pre_key_id: Option<u32>,
}

/// Run X3DH as initiator. Verifies the signed pre-key signature first.
pub fn initiator_x3dh(
    identity: &X25519KeyPair,
    peer: &PeerBundle,
) -> Result<InitiatorAgreement, X3dhError>
{
    // Verify against the wire-form (DJB-prefixed) public key — that is
    // what other OMEMO clients sign and verify against.
    let spk_wire = wire_key_33(&peer.signed_pre_key);
    xeddsa_verify(&peer.identity_key, &spk_wire, &peer.signed_pre_key_sig)
        .map_err(|_| X3dhError::InvalidSignedPreKeySignature)?;

    let ephemeral = X25519KeyPair::generate();

    let dh1 = identity.diffie_hellman(&peer.signed_pre_key);
    let dh2 = ephemeral.diffie_hellman(&peer.identity_key);
    let dh3 = ephemeral.diffie_hellman(&peer.signed_pre_key);
    let dh4 = peer.one_time_pre_key.map(|(_, opk)| ephemeral.diffie_hellman(&opk));

    let mut ikm = Vec::with_capacity(32 * 5);
    ikm.extend_from_slice(&KDF_PREFIX);
    ikm.extend_from_slice(&dh1);
    ikm.extend_from_slice(&dh2);
    ikm.extend_from_slice(&dh3);
    if let Some(dh4) = dh4
    {
        ikm.extend_from_slice(&dh4);
    }

    let mut sk = [0u8; 32];
    let okm = hkdf_sha256(&[0u8; 32], &ikm, KDF_INFO, 32)?;
    sk.copy_from_slice(&okm);

    return Ok(InitiatorAgreement
    {
        shared_secret: sk,
        ephemeral_key: ephemeral,
        used_signed_pre_key_id: peer.signed_pre_key_id,
        used_one_time_pre_key_id: peer.one_time_pre_key.map(|(id, _)| id),
    });
}

/// Responder-side X3DH input. The responder uses its own identity key
/// plus the specific signed pre-key and (optionally) one-time pre-key
/// that the initiator referenced in the PreKey message.
pub struct ResponderInputs<'a>
{
    pub identity: &'a X25519KeyPair,
    pub signed_pre_key: &'a X25519KeyPair,
    pub one_time_pre_key: Option<&'a X25519KeyPair>,
    pub peer_identity_key: [u8; 32],
    pub peer_ephemeral_key: [u8; 32],
}

pub fn responder_x3dh(inputs: &ResponderInputs<'_>) -> Result<[u8; 32], X3dhError>
{
    // Note: ordering of arguments to diffie_hellman matches the initiator's
    // DH definitions so DH outputs are identical bytes.
    let dh1 = inputs.signed_pre_key.diffie_hellman(&inputs.peer_identity_key);
    let dh2 = inputs.identity.diffie_hellman(&inputs.peer_ephemeral_key);
    let dh3 = inputs.signed_pre_key.diffie_hellman(&inputs.peer_ephemeral_key);
    let dh4 = inputs.one_time_pre_key.map(|opk| opk.diffie_hellman(&inputs.peer_ephemeral_key));

    let mut ikm = Vec::with_capacity(32 * 5);
    ikm.extend_from_slice(&KDF_PREFIX);
    ikm.extend_from_slice(&dh1);
    ikm.extend_from_slice(&dh2);
    ikm.extend_from_slice(&dh3);
    if let Some(dh4) = dh4
    {
        ikm.extend_from_slice(&dh4);
    }

    let okm = hkdf_sha256(&[0u8; 32], &ikm, KDF_INFO, 32)?;
    let mut sk = [0u8; 32];
    sk.copy_from_slice(&okm);
    return Ok(sk);
}

#[cfg(test)]
mod tests
{
    use super::*;
    use crate::omemo::crypto::xeddsa_sign;

    fn synthetic_pair() -> (X25519KeyPair, X25519KeyPair, X25519KeyPair, X25519KeyPair, [u8; 64])
    {
        let identity = X25519KeyPair::generate();
        let signed_pre_key = X25519KeyPair::generate();
        let one_time_pre_key = X25519KeyPair::generate();
        let ephemeral = X25519KeyPair::generate(); // initiator's
        let spk_wire = wire_key_33(&signed_pre_key.public_bytes());
        let sig = xeddsa_sign(&identity.private_bytes(), &spk_wire);
        return (identity, signed_pre_key, one_time_pre_key, ephemeral, sig);
    }

    #[test]
    fn initiator_and_responder_agree_with_otpk()
    {
        let alice_id = X25519KeyPair::generate();
        let (bob_id, bob_spk, bob_opk, _, sig) = synthetic_pair();

        let peer = PeerBundle
        {
            identity_key: bob_id.public_bytes(),
            signed_pre_key_id: 1,
            signed_pre_key: bob_spk.public_bytes(),
            signed_pre_key_sig: sig,
            one_time_pre_key: Some((42, bob_opk.public_bytes())),
        };

        let init = initiator_x3dh(&alice_id, &peer).unwrap();

        let resp = ResponderInputs
        {
            identity: &bob_id,
            signed_pre_key: &bob_spk,
            one_time_pre_key: Some(&bob_opk),
            peer_identity_key: alice_id.public_bytes(),
            peer_ephemeral_key: init.ephemeral_key.public_bytes(),
        };

        let sk_bob = responder_x3dh(&resp).unwrap();
        assert_eq!(init.shared_secret, sk_bob);
        assert_eq!(init.used_one_time_pre_key_id, Some(42));
    }

    #[test]
    fn initiator_and_responder_agree_without_otpk()
    {
        let alice_id = X25519KeyPair::generate();
        let (bob_id, bob_spk, _, _, sig) = synthetic_pair();

        let peer = PeerBundle
        {
            identity_key: bob_id.public_bytes(),
            signed_pre_key_id: 1,
            signed_pre_key: bob_spk.public_bytes(),
            signed_pre_key_sig: sig,
            one_time_pre_key: None,
        };

        let init = initiator_x3dh(&alice_id, &peer).unwrap();
        let resp = ResponderInputs
        {
            identity: &bob_id,
            signed_pre_key: &bob_spk,
            one_time_pre_key: None,
            peer_identity_key: alice_id.public_bytes(),
            peer_ephemeral_key: init.ephemeral_key.public_bytes(),
        };
        let sk_bob = responder_x3dh(&resp).unwrap();
        assert_eq!(init.shared_secret, sk_bob);
        assert_eq!(init.used_one_time_pre_key_id, None);
    }

    #[test]
    fn initiator_rejects_bad_signature()
    {
        let alice_id = X25519KeyPair::generate();
        let (bob_id, bob_spk, _, _, _) = synthetic_pair();
        let attacker = X25519KeyPair::generate();
        let bad_sig = xeddsa_sign(&attacker.private_bytes(), &bob_spk.public_bytes());

        let peer = PeerBundle
        {
            identity_key: bob_id.public_bytes(),
            signed_pre_key_id: 1,
            signed_pre_key: bob_spk.public_bytes(),
            signed_pre_key_sig: bad_sig,
            one_time_pre_key: None,
        };

        assert!(initiator_x3dh(&alice_id, &peer).is_err());
    }
}
