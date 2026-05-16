//! OMEMO end-to-end encryption (XEP-0384, legacy `axolotl` namespace).
//!
//! This module implements:
//!
//! - Curve25519/X25519 identity, signed pre-key, one-time pre-keys
//! - X3DH key agreement (Signal X3DH spec)
//! - Double Ratchet (Signal Double Ratchet spec)
//! - AES-128-GCM payload encryption with a fresh random message key per
//!   stanza
//! - XEP-0384 v0.3 stanza format
//! - On-disk session storage rooted at the user's data directory
//!
//! Interop status: every primitive has unit tests, the protocol matches
//! the published Signal/OMEMO specs, but no live-wire interop test was
//! possible in the session this code was written in. See module-level
//! tests for the verifiable behaviour.

pub mod crypto;
pub mod x3dh;
pub mod ratchet;
pub mod session;
pub mod identity;
pub mod store;
pub mod protocol;
pub mod signal_message;

pub use identity::{ IdentityKeyPair, SignedPreKey, OneTimePreKey };
pub use session::{ Session, SessionError, EncryptedMessage, IncomingMessage };
pub use store::OmemoStore;
pub use protocol::{ OmemoController, OmemoError };

/// Length of the OMEMO inner message key (16 bytes for AES-128-GCM key,
/// 16 bytes appended GCM auth tag). The 32 bytes are what the Signal
/// ratchet wraps per recipient device.
pub const OMEMO_MESSAGE_KEY_LEN: usize = 32;
