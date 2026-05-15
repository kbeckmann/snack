# libxmpp

An async XMPP client library for Rust, built on Tokio.

## Features

- TCP + STARTTLS connection handling
- SASL authentication (PLAIN, SCRAM-SHA-1, SCRAM-SHA-256)
- Resource binding
- One-to-one chat messages
- Multi-User Chat (MUC) rooms: join, leave, send and receive group messages
- Presence updates and presence-error reporting

## Public API

The crate exposes three top-level items:

- [`XmppClient`](src/lib.rs) — the client handle. Created with
  `XmppClient::new(jid, password).await`, which returns the client
  and an `mpsc::Receiver<XmppEvent>` for incoming events.
- [`XmppEvent`](src/lib.rs) — the events delivered on that receiver
  (connection lifecycle, room joins/leaves, messages, presence
  errors).
- [`RoomMember`](src/lib.rs) — describes an occupant of a MUC room.

`XmppClient` provides:

| Method | Purpose |
| --- | --- |
| `new(jid, password)` | Connect, upgrade to TLS, authenticate, and bind a resource. |
| `get_jid()` | Return the full JID assigned by the server. |
| `join_room(room_jid, nick)` | Join a MUC room. |
| `leave_room(room_jid, nick)` | Leave a MUC room. |
| `send_room_message(room_jid, body)` | Send a group chat message. |
| `send_message(to, body)` | Send a one-to-one chat message. |
| `close()` | Shut down the reader task and close the socket. |

See the rustdoc on `src/lib.rs` for detailed documentation of each
item, including the meaning of every `XmppEvent` variant.

## Example

```rust
use xmpp::{XmppClient, XmppEvent};

#[tokio::main]
async fn main() -> Result<(), String>
{
    let (mut client, mut events) =
        XmppClient::new("user@example.com", "password").await?;

    client.join_room("room@conference.example.com", "my-nick").await?;

    while let Some(event) = events.recv().await
    {
        match event
        {
            XmppEvent::RoomJoined { room, members } =>
            {
                println!("joined {} ({} members)", room, members.len());
            }
            XmppEvent::RoomMessage { room, nick, body, .. } =>
            {
                println!("[{}] {}: {}", room, nick, body);
            }
            XmppEvent::DirectMessage { from, body, .. } =>
            {
                println!("{}: {}", from, body);
            }
            XmppEvent::PresenceError { from, condition, .. } =>
            {
                eprintln!("presence error from {}: {}", from, condition);
                break;
            }
            _ => {}
        }
    }

    client.close().await;
    return Ok(());
}
```

## Building

```sh
cargo build
cargo test
cargo doc --open
```

## License

See [LICENSE](LICENSE).
