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

OMEMO is wired end-to-end inside snack: every primitive has unit tests,
the protocol matches the published Signal/OMEMO specs, and the bundle /
device-list PubSub publish-fetch cycle goes through real XMPP IQs.
Cross-client interop with other OMEMO implementations (Conversations,
Dino, Gajim, …) has **not** been verified on the wire — the wrapped-key
container format used per recipient is internal to this implementation
rather than libsignal's PreKeyWhisperMessage protobuf. Two snack peers
can exchange encrypted messages with each other today.

## License

This project is licensed under the MIT License. See [LICENSE](./LICENSE) for the full text.
