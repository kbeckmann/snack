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
