# Snack

Snack is a desktop XMPP client built with Iced and powered by libxmpp.

## Supported features

- Connect to an XMPP account with JID and password
- Join multi-user chat rooms by room JID
- TLS support
- SASL authentication
- OMEMO end-to-end encryption (XEP-0384 legacy `axolotl` namespace) for
  one-to-one messages, with X3DH key agreement, the Signal Double
  Ratchet, AES-128-GCM payload encryption, and on-disk session storage

### OMEMO interop status

OMEMO is wired end-to-end inside snack:

- Identity / signed pre-key / one-time pre-keys are emitted in
  libsignal wire format (DJB-prefixed 33-byte Curve25519 keys).
- The signed pre-key is XEdDSA-signed over the 33-byte wire form, so
  Conversations / Dino / Gajim accept it as a valid bundle.
- Each per-recipient `<key>` element carries a libsignal
  `PreKeyWhisperMessage` (first message in a session) or a
  `WhisperMessage` envelope: version byte `0x33`, the protobuf, then an
  8-byte HMAC-SHA256 MAC over
  `alice_ik || bob_ik || version_byte || protobuf` keyed by an HKDF of
  the ratchet message key. The inner ciphertext is AES-256-CBC.
- The outer OMEMO payload is AES-128-GCM with a random 12-byte IV; the
  16-byte AES key and 16-byte GCM tag are concatenated and wrapped by
  the libsignal envelope.

Live-wire interop with a real Conversations / Dino / Gajim peer was
**not** verified in the session this was written; the protocol matches
the specs and the encode→parse path round-trips end-to-end internally.
If a real peer can't decrypt, the debug log will show the failure mode.

## License

This project is licensed under the MIT License. See [LICENSE](./LICENSE) for the full text.
