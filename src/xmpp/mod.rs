use iced::futures::SinkExt;
use iced::stream;
use log::{ debug, error };
use std::sync::{ Arc, Mutex };
use std::time::Duration;

// Upper bound on connecting + authenticating + binding. A reconnect fired right
// after wake (before WireGuard/the network is back) would otherwise block here
// forever; bounding it lets the app's reconnect backoff retry until the network
// returns.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum XmppCommand
{
    JoinRoom(String),
    LeaveRoom { room: String, nick: String },
    SendRoomMessage { room: String, body: String },
    SendDirectMessage { to: String, body: String },
}

struct ChannelInner
{
    rx: Option<tokio::sync::mpsc::Receiver<XmppCommand>>,
    jid: String,
    password: String,
}

#[derive(Clone)]
pub struct CommandChannel(Arc<Mutex<ChannelInner>>);

impl PartialEq for CommandChannel
{
    fn eq(&self, other: &Self) -> bool
    {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for CommandChannel {}

impl std::hash::Hash for CommandChannel
{
    fn hash<H: std::hash::Hasher>(&self, state: &mut H)
    {
        (Arc::as_ptr(&self.0) as usize).hash(state);
    }
}

pub fn new_command_channel(jid: String, password: String) -> (tokio::sync::mpsc::Sender<XmppCommand>, CommandChannel)
{
    let (tx, rx) = tokio::sync::mpsc::channel(100);
    (tx, CommandChannel(Arc::new(Mutex::new(ChannelInner { rx: Some(rx), jid, password }))))
}

#[derive(Debug, Clone)]
pub enum XmppEvent
{
    Connected,
    Disconnected(String),
    RoomJoined { room: String, members: Vec<::xmpp::RoomMember> },
    RoomJoinFailed { room: String, reason: String },
    RoomLeft(String),
    MemberJoined { room: String, member: ::xmpp::RoomMember },
    MemberLeft { room: String, nick: String },
    RoomMessage
    {
        room: String,
        nick: String,
        body: String,
        timestamp: chrono::DateTime<chrono::Utc>,
        // True when the stanza carried an XEP-0203 `<delay>`, i.e. it is replayed
        // history (MUC join backlog) rather than a live message. Drives dedup.
        delayed: bool,
    },
    RoomSubject
    {
        room: String,
        subject: String,
    },
    PresenceError
    {
        from: String,
        condition: String,
        text: Option<String>,
    },
    DirectMessage
    {
        from: String,
        body: String,
        timestamp: chrono::DateTime<chrono::Utc>,
        delayed: bool,
    },
}

pub fn connect(cmd: CommandChannel) -> impl iced::futures::Stream<Item = XmppEvent>
{
    stream::channel(100, async move |mut output|
    {
        let (cmd_rx, jid, password) =
        {
            let mut inner = cmd.0.lock().unwrap();
            let rx = inner.rx.take();
            let jid = std::mem::take(&mut inner.jid);
            let password = std::mem::take(&mut inner.password);
            (rx, jid, password)
        };

        let mut cmd_rx = match cmd_rx
        {
            Some(rx) => rx,
            None =>
            {
                error!("Command channel receiver already consumed");
                let _ = output.send(XmppEvent::Disconnected("Internal error: command channel already consumed.".to_string())).await;

                return;
            }
        };

        let nick = jid.split('@').next().unwrap_or("user").to_string();

        // Bridge between iced's async executor and tokio: libxmpp requires a
        // tokio runtime for TCP, TLS, and spawned tasks.
        let (bridge_tx, mut bridge_rx) = tokio::sync::mpsc::channel::<XmppEvent>(100);

        std::thread::spawn(move ||
        {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");

            rt.block_on(async move
            {
                // Connection setup (TCP connect, TLS, SASL, resource bind) has no
                // internal timeout, and the wake-from-sleep heartbeat below only
                // starts once setup succeeds. So if we reconnect right after waking
                // — before WireGuard/the network is back — the setup reads would
                // block forever with no detection and no retry. Bound it so a
                // stalled attempt fails and the app's backoff loop tries again.
                let setup = ::xmpp::XmppClient::new(&jid, &password);
                let (mut client, mut event_rx) = match tokio::time::timeout(SETUP_TIMEOUT, setup).await
                {
                    Ok(Ok(x)) => x,
                    Ok(Err(e)) =>
                    {
                        let _ = bridge_tx.send(XmppEvent::Disconnected(e.to_string())).await;
                        return;
                    }
                    Err(_) =>
                    {
                        let _ = bridge_tx.send(XmppEvent::Disconnected(
                            format!("Connection attempt timed out after {}s.", SETUP_TIMEOUT.as_secs())
                        )).await;
                        return;
                    }
                };

                // Heartbeat used purely to detect that the machine was
                // suspended (e.g. laptop lid closed). libxmpp 0.1.3 exposes no
                // ping/keepalive, so when the host sleeps the TCP socket goes
                // half-open: reads block forever and no Disconnected is ever
                // emitted, leaving the UI "connected" to a dead socket. By
                // comparing wall-clock time between ticks we notice the gap on
                // wake and tear the connection down so the app can reconnect.
                let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(30));
                heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut last_beat = std::time::SystemTime::now();

                loop
                {
                    tokio::select!
                    {
                        _ = heartbeat.tick() =>
                        {
                            // The first tick fires immediately; later ticks are
                            // ~30s apart. A much larger wall-clock gap means we
                            // were suspended.
                            let now = std::time::SystemTime::now();
                            let elapsed = now.duration_since(last_beat).unwrap_or_default();
                            last_beat = now;

                            if elapsed > std::time::Duration::from_secs(90)
                            {
                                log::warn!(
                                    "Wall-clock jumped {}s between heartbeats; assuming wake from sleep.",
                                    elapsed.as_secs()
                                );
                                let _ = bridge_tx.send(XmppEvent::Disconnected("Connection lost after sleep.".to_string())).await;
                                break;
                            }
                        }
                        event = event_rx.recv() =>
                        {
                            match event
                            {
                                Some(ev) =>
                                {
                                    debug!("libxmpp event: {:?}", ev);
                                    let mapped = match ev
                                    {
                                        ::xmpp::XmppEvent::Connected => Some(XmppEvent::Connected),
                                        ::xmpp::XmppEvent::RoomJoined { room, members } =>
                                        {
                                            Some(XmppEvent::RoomJoined { room, members })
                                        }
                                        ::xmpp::XmppEvent::RoomLeft(room) =>
                                        {
                                            Some(XmppEvent::RoomLeft(room))
                                        }
                                        ::xmpp::XmppEvent::MemberJoined { room, member } =>
                                        {
                                            Some(XmppEvent::MemberJoined { room, member })
                                        }
                                        ::xmpp::XmppEvent::MemberLeft { room, nick } =>
                                        {
                                            Some(XmppEvent::MemberLeft { room, nick })
                                        }
                                        ::xmpp::XmppEvent::RoomMessage { room, nick, body, timestamp } =>
                                        {
                                            let delayed = timestamp.is_some();
                                            let ts = timestamp
                                                .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                                                .map(|dt| dt.with_timezone(&chrono::Utc))
                                                .unwrap_or_else(chrono::Utc::now);
                                            Some(XmppEvent::RoomMessage { room, nick, body, timestamp: ts, delayed })
                                        }
                                        ::xmpp::XmppEvent::RoomSubject { room, subject } =>
                                        {
                                            Some(XmppEvent::RoomSubject { room, subject })
                                        }
                                        ::xmpp::XmppEvent::PresenceError { from, error_type: _, condition, text } =>
                                        {
                                            Some(XmppEvent::PresenceError { from, condition, text })
                                        }
                                        ::xmpp::XmppEvent::DirectMessage { from, body, timestamp } =>
                                        {
                                            let delayed = timestamp.is_some();
                                            let ts = timestamp
                                                .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                                                .map(|dt| dt.with_timezone(&chrono::Utc))
                                                .unwrap_or_else(chrono::Utc::now);
                                            Some(XmppEvent::DirectMessage { from, body, timestamp: ts, delayed })
                                        }
                                        _ => None,
                                    };

                                    if let Some(evt) = mapped
                                    {
                                        if bridge_tx.send(evt).await.is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                                None =>
                                {
                                    let _ = bridge_tx.send(XmppEvent::Disconnected("Connection closed.".to_string())).await;
                                    break;
                                }
                            }
                        }
                        cmd = cmd_rx.recv() =>
                        {
                            // Each command boils down to a socket write. A write
                            // error means the transport is gone, so surface it as
                            // a disconnect and stop — the UI then reconnects
                            // rather than silently dropping the command.
                            let write_err = match cmd
                            {
                                Some(XmppCommand::JoinRoom(room_jid)) =>
                                {
                                    debug!("XmppCommand::JoinRoom room={} nick={}", room_jid, nick);
                                    match client.join_room(&room_jid, &nick).await
                                    {
                                        Ok(()) => None,
                                        Err(e) =>
                                        {
                                            error!("join_room({}) failed: {}", room_jid, e);
                                            let _ = bridge_tx.send(XmppEvent::RoomJoinFailed
                                            {
                                                room: room_jid,
                                                reason: e.to_string(),
                                            }).await;
                                            Some(e)
                                        }
                                    }
                                }
                                Some(XmppCommand::SendRoomMessage { room, body }) =>
                                {
                                    debug!("XmppCommand::SendRoomMessage room={} len={}", room, body.len());
                                    client.send_room_message(&room, &body).await.err().inspect(|e|
                                    {
                                        error!("send_room_message({}) failed: {}", room, e);
                                    })
                                }
                                Some(XmppCommand::LeaveRoom { room, nick }) =>
                                {
                                    debug!("XmppCommand::LeaveRoom room={} nick={}", room, nick);
                                    client.leave_room(&room, &nick).await.err().inspect(|e|
                                    {
                                        error!("leave_room({}) failed: {}", room, e);
                                    })
                                }
                                Some(XmppCommand::SendDirectMessage { to, body }) =>
                                {
                                    debug!("XmppCommand::SendDirectMessage to={} len={}", to, body.len());
                                    client.send_message(&to, &body).await.err().inspect(|e|
                                    {
                                        error!("send_message({}) failed: {}", to, e);
                                    })
                                }
                                None => break,
                            };

                            if let Some(e) = write_err
                            {
                                error!("Command write failed, connection lost: {}", e);
                                let _ = bridge_tx.send(XmppEvent::Disconnected(format!("Connection lost: {}", e))).await;
                                break;
                            }
                        }
                    }
                }

                client.close().await;
            });
        });

        while let Some(event) = bridge_rx.recv().await
        {
            let is_disconnect = matches!(&event, XmppEvent::Disconnected(_));
            let _ = output.send(event).await;

            if is_disconnect
            {
                return;
            }
        }
    })
}
