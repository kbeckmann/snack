# Snack

Snack is a desktop XMPP client built with Iced and powered by libxmpp.

## Features

- XMPP login with SRV server discovery, STARTTLS and SASL, optional saved credentials, and automatic reconnects.
- Multi-user rooms, direct messages, nickname completion, and desktop notifications.
- Persistent, searchable room and direct-message history, isolated by account.
- Local history paging and XEP-0313 server archive paging and offline catch-up.
- Stable XEP-0359 stanza-ID deduplication, with content/timestamp fallback for older messages and servers.
- Bounded history windows that keep long-running conversations responsive.
- Per-conversation drafts restored across switches and restarts.
- A persistent outgoing queue replayed after reconnects and application restarts.
- Undo/redo in the composer, free text selection across messages, and whole-message range copying.
- A native macOS application bundle and Dock icon, including terminal-launched builds.

## Keyboard and selection

Use Cmd on macOS and Ctrl on other platforms for the primary shortcuts.

| Action | Shortcut or gesture |
| --- | --- |
| Send | Enter |
| Insert newline | Shift-Enter or Alt-Enter (also Ctrl-Enter on macOS) |
| Undo / redo input | Cmd/Ctrl-Z / Cmd/Ctrl-Shift-Z (also Ctrl-Y on Windows and Linux) |
| Find in the current conversation | Cmd/Ctrl-F from any focused widget |
| Search all open conversations | Enable **All rooms** in the find bar |
| Next / previous find match | Enter / Shift-Enter in the find bar |
| Previous / next conversation | Alt-Up / Alt-Down |
| Close a direct conversation | Cmd/Ctrl-W, from the composer or the transcript; rooms are only left with their **Leave** button |
| Return to typing from the transcript | Tab |
| Select part of one or more messages | Drag through message bodies |
| Select whole messages | Cmd/Ctrl-click a message to show its checked checkbox; then use checkboxes and Shift-click to adjust the range |
| Quote a message in the composer | Double-click its body, timestamp, or nickname |
| Copy selected messages or text | Cmd/Ctrl-C; whole-message copies include timestamp and nickname and exit selection mode |
| Clear selection or close find | Escape |
| Open a URL | Click it; dragging across it selects text instead |

### Editing in the composer

The composer follows each platform's text-field conventions, so the usual
system shortcuts work. Alt-Up/Down stay reserved for switching conversations.

| Action | macOS | Windows and Linux |
| --- | --- | --- |
| Move / select by word | Alt-Left/Right (+Shift) | Ctrl-Left/Right (+Shift) |
| Line start / end | Cmd-Left/Right (+Shift) | Home / End (+Shift) |
| Start / end of the message | Cmd-Up/Down (+Shift) | Ctrl-Home/End (+Shift) |
| Extend the selection to a point | Shift-click | Shift-click |
| Select to start / end | Shift-Up on the first line, Shift-Down on the last, Shift-Home/End | Ctrl-Shift-Home/End |
| Paragraph | Alt-Shift-Up/Down selects | Ctrl-Up/Down (+Shift) |
| Delete word | Alt-Backspace / Alt-Delete | Ctrl-Backspace / Ctrl-Delete |
| Delete to line start | Cmd-Backspace | Shift-Home, then Backspace |
| Clipboard | Cmd-X/C/V | Ctrl-X/C/V, Shift-Delete, Ctrl-Insert, Shift-Insert |

On macOS, Home/End and Page Up/Down scroll without moving the caret, as in
other Cocoa text fields.

## Persistence and delivery behavior

Snack runs one instance per configuration directory. Launching it again restores and focuses the existing window before any second login or storage access. An operating-system lock protects the shared outbox and drafts and is released automatically after exit or a crash.

Open direct conversations are restored after a restart. Closing one removes it from the sidebar without losing it: closed conversations are listed, most recent first, under a collapsed **Earlier** section in the Chats list, and clicking one reopens it with its history. A new message from a closed contact reopens the conversation. History from before this list existed is scanned once to fill it.

History lives in SQLite under the platform application-data directory. Drafts are saved in the platform configuration directory and flushed when switching conversations or closing the window. Existing history databases are migrated without changing message IDs; account ownership prevents another login from replaying the wrong outgoing queue. Retained history writes are retried in their owning account databases after switching accounts or disconnecting.

If saving settings or message updates fails when closing, Snack keeps the window open with retry controls. **Close without saving** is available after a failed close attempt; it can discard updates that have not reached disk. Durably queued messages can be retried after the next launch.

If saved settings cannot be read or parsed, Snack pauses login and prevents normal saves from overwriting the original file. Repair the file or its permissions and choose **Retry loading settings**, or choose **Back up settings and start fresh**. Starting fresh requires a completed backup of an existing settings file; Snack reports failures and the backup location.

Removing auto-login saves the disabled setting before deleting the saved password. If the keyring is locked or unavailable, Snack shows a warning and a **Retry password cleanup** button; pending cleanup is retained across restarts. A failure to save the disabled setting is reported separately. If the keyring cannot be read during startup, **Retry saved login** lets you try again after unlocking it; you can also enter your password manually. If an automatic login fails or is canceled, its restored password is cleared from the form. **Use saved login** retries the saved account.

Outgoing messages are durably queued before sending when local storage is available. The history and outbox database requests a disk sync for each commit, including the full device flush on macOS. Rooms replay queued messages after joining; direct messages replay after connecting. A successful socket write alone does not remove an entry: with XEP-0198, Snack waits for a server acknowledgement covering the message. Without stream management, it waits for a server ping round trip on the same connection. That fallback checks connectivity; it does not acknowledge message handling.

Neither confirmation proves recipient delivery or reading. Uncertain queued attempts remain available for retry, so a lost acknowledgement can cause a duplicate attempt. Stable message origins and archive IDs help reconcile copies, but do not guarantee exactly-once delivery. Correlated server errors mark messages as rejected instead of treating them as delivered.

If local storage is unavailable, an online send can proceed without a durable retry queue; its recovery guarantees are weaker. Offline sends retain the composer text when they cannot be queued.

Desktop alerts use short, literal text previews and a bounded background queue. A busy notification service can cause excess alerts to be skipped; message processing remains independent of the notification service.

## TLS policy

This branch retains Snack's exception for expired server certificates. After an expiry error, the verifier repeats full chain and hostname checks at the certificate's expiry boundary, still verifies TLS handshake signatures, and logs a warning on acceptance. This deliberately relaxes current-time certificate validity. The policy is isolated from the protocol extensions so a consumer requiring strict expiry enforcement can omit it.

## Building and testing

The committed lockfiles provide reproducible dependency resolution. The application requires Rust **1.89** or newer and uses a vendored libxmpp **0.3.1** with archive support and additional protocol and lifecycle fixes. See [vendor/README.md](vendor/README.md) for the pristine release checksum, patch scope, and TLS policy.

```sh
cargo test --locked
cargo test --manifest-path vendor/libxmpp/Cargo.toml --locked --target-dir target/libxmpp
cargo build --release --locked
```

The library suite includes local loopback socket tests; no XMPP account is needed. App tests cover history migration and deduplication, paging and reconnect recovery, draft preservation, undo keyboard dispatch, and cross-message pointer selection and rendering. Unit-test configuration is isolated from the user's saved account and drafts.

On macOS, package the release build with:

```sh
bash scripts/pack-macos-app.sh
open snack.app
```

The generated bundle is ignored by Git. Icon source files and generation scripts are under `icon/`. Debug builds optimize dependencies while keeping Snack itself unoptimized and debuggable.

## License

MIT. See [LICENSE](LICENSE); the vendored library's license is retained in [vendor/libxmpp/LICENSE](vendor/libxmpp/LICENSE).
