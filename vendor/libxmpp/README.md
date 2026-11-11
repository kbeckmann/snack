# libxmpp

An async XMPP client library for Rust, built on Tokio.

## Features

- TCP + STARTTLS connection handling
- SASL authentication (SCRAM-SHA-512, SCRAM-SHA-256, SCRAM-SHA-1, PLAIN)
- Resource binding
- One-to-one chat messages
- Explicit incoming stanza recipients are checked against the bound account,
  preserving username/domain aliases and other resources of that account.
  Forwarded archive recipients are checked separately from their outer envelope.
- Multi-User Chat (MUC) rooms: join, leave, send and receive group messages
- Bare-room text is delivered as `RoomServiceMessage`, separately from occupant
  messages, including normal-type notices from rooms tracked in this connection.
  Received service text shares entry buffering and remains routable after departure.
- Room privacy notices (`RoomPrivacy`): publicly logged and non-anonymous room
  settings reported by confirmed self-presence or live room-service changes.
  Unknown settings stay unknown; delayed history cannot change current settings.
- Presence updates and presence-error reporting
- XEP-0199 ping for keep-alives and connection liveness probing, and
  automatically answering pings from other entities
- XEP-0030 service discovery (querying other entities, and automatically
  answering info and items queries from others)
- XEP-0198 stream management (automatically negotiated when offered by the
  server, answering the server's acknowledgement requests, and confirming
  delivery of sent stanzas via `request_ack`)
- XEP-0115 entity capabilities (advertising the client's disco info as a
  compact `<c/>` hash in outgoing presence, and surfacing peers' advertised
  capabilities as `EntityCapabilities` events)
- XEP-0203 delayed delivery (recognising the `<delay/>` element on incoming
  messages — MUC history replay, offline messages — and surfacing its stamp,
  originating entity, and reason as the `delay` field of `RoomMessage` and
  `DirectMessage` events)

## Public API

The crate exposes these top-level items:

- [`XmppClient`](src/lib.rs) — the client handle. Created with
  `XmppClient::new(jid, password).await`, which returns the client
  and an `mpsc::Receiver<XmppEvent>` for incoming events.
- [`XmppEvent`](src/lib.rs) — the events delivered on that receiver
  (connection lifecycle, room joins/leaves, messages, presence
  errors).
- [`RoomMember`](src/lib.rs) — describes an occupant of a MUC room.
- [`DiscoInfo`](src/lib.rs), [`DiscoIdentity`](src/lib.rs) and
  [`DiscoItem`](src/lib.rs) — the results of XEP-0030 service
  discovery queries.
- [`Delay`](src/lib.rs) — XEP-0203 delayed-delivery information
  (stamp, originating entity, and reason) attached to a `RoomMessage`
  or `DirectMessage` that was stored and delivered later.

`XmppClient` provides:

| Method | Purpose |
| --- | --- |
| `new(jid, password)` | Connect, upgrade to TLS, authenticate, and bind a resource. |
| `get_jid()` | Return the full JID assigned by the server. |
| `join_room(room_jid, nick)` | Join a MUC room. |
| `leave_room(room_jid, nick)` | Leave a MUC room. |
| `send_room_message(room_jid, body)` | Send a group chat message. |
| `send_message(to, body)` | Send a one-to-one chat message. |
| `ping(to, timeout)` | Send an XEP-0199 ping and await the reply, returning the round-trip time. `to = None` pings the user's own server. |
| `disco_info(to, node, timeout)` | Send an XEP-0030 service discovery info query, returning the entity's advertised identities and features. |
| `disco_items(to, node, timeout)` | Send an XEP-0030 service discovery items query, returning the items the entity hosts. |
| `set_disco_info(info)` | Replace the identities and features advertised when answering incoming service discovery info queries. |
| `add_disco_feature(var)` | Advertise an additional feature namespace in replies to service discovery info queries. |
| `set_disco_items(items)` | Replace the items advertised when answering incoming service discovery items queries. |
| `set_caps_node(node)` | Set the URI advertised as the `node` of the XEP-0115 entity-capabilities element. |
| `caps_ver()` | The current XEP-0115 verification hash (`ver`) for the advertised capabilities. |
| `send_presence()` | Broadcast available presence advertising the client's current entity capabilities (re-advertise after changing disco info or the caps node). |
| `is_stream_management_enabled()` | Whether XEP-0198 stream management was negotiated for this session. |
| `request_ack(timeout)` | Request a stream-management acknowledgement and wait until the server confirms it has handled every stanza sent so far. |
| `close()` | Shut down the reader task and close the socket. |

Incoming service discovery queries and XEP-0199 pings from other
entities are answered automatically by the background reader task. Disco
replies use the advertised configuration above; by default a client
reports a single `client`/`bot` identity and the features it answers
(the two service-discovery namespaces, `urn:xmpp:ping`, and the entity
capabilities namespace).

On connect the client broadcasts available presence carrying an XEP-0115
`<c/>` element whose `ver` hash is derived from that same disco
configuration, so peers can cache the client's capabilities without a
disco round-trip. Peers' own capabilities arrive as `EntityCapabilities`
events, carrying the `node`/`ver` that can be resolved with `disco_info`.

See the rustdoc on `src/lib.rs` for detailed documentation of each
item, including the meaning of every `XmppEvent` variant.

## Example

```rust
use xmpp::{XmppClient, XmppEvent};

#[tokio::main]
async fn main() -> Result<(), xmpp::XmppError>
{
    let (mut client, mut events) =
        XmppClient::new("user@example.com", "password").await?;

    client.join_room("room@conference.example.com", "my-nick").await?;

    while let Some(event) = events.recv().await
    {
        match event
        {
            XmppEvent::RoomJoined { room, members, .. } =>
            {
                println!("joined {} ({} members)", room, members.len());
            }
            XmppEvent::RoomMessage { room, nick, body, .. } =>
            {
                println!("[{}] {}: {}", room, nick, body);
            }
            XmppEvent::RoomServiceMessage { room, body, .. } =>
            {
                println!("[{}] room service: {}", room, body);
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
