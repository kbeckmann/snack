# Vendored dependencies

`libxmpp/` is the complete, unmodified contents of the published `libxmpp` 0.3.1 crate at its import commit. Subsequent commits carry Snack's protocol extensions separately.

- Release: https://crates.io/crates/libxmpp/0.3.1
- Source: https://github.com/mikaelsahlstrom/libxmpp/tree/4415bd9e1bd0cea801257604aa62eb932661afd2
- Archive: https://static.crates.io/crates/libxmpp/libxmpp-0.3.1.crate
- Archive SHA-256: `100116e3e79e0271951bd710c6301e0408c4d67ea206ca82b81ac077b208a9c8`
- License: MIT; retained in `libxmpp/LICENSE`.

The packaged Cargo manifests, lockfile, source provenance, documentation, and tests are retained so the import can be compared directly with the release archive. Run library tests with a separate target directory:

```sh
cargo test --manifest-path vendor/libxmpp/Cargo.toml --locked --target-dir target/libxmpp
```

## Local changes

- XEP-0313 archive queries, archive events, and RSM pagination. Archive IQs use the shared counted writer; results and completion stanzas participate in XEP-0198 counters before archive routing.
- XEP-0359 stanza IDs on room and direct messages, selected by the assigning room/account JID for stable archive deduplication.

The vendored code also changes XML framing and namespace handling, TLS/SASL and binding validation, server stream-error handling, IQ reply authority and cancellation, room lifecycle management, discovery/capability replies, and stream-management acknowledgement validation. Message sends can wait for a covering stream acknowledgement or a legacy server ping round trip. These changes go beyond the two archive extensions above; compare against the import commit when reviewing or extracting patches.

The `Delay` representation is retained, but the public API is extended: message events carry stanza and origin IDs, additional lifecycle events and error variants are available, and contextual room and confirmed-send APIs are provided. Code that exhaustively matches enums or constructs event variants may require changes. The legacy ping fallback checks connectivity, not recipient delivery or stanza handling.

## TLS expiry policy

Snack retains its expired-server-certificate exception. The verifier repeats full chain and hostname validation at the certificate's expiry boundary and accepts only if that verification succeeds; TLS handshake signatures are still verified. Every accepted expired certificate produces a warning. This policy is kept in its own commit so consumers can omit it when strict current-time certificate validity is required.
