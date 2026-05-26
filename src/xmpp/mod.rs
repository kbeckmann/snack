use iced::futures::SinkExt;
use iced::stream;
use log::{ debug, error };
use std::sync::{ Arc, Mutex };

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
                let (mut client, mut event_rx) = match ::xmpp::XmppClient::new(&jid, &password).await
                {
                    Ok(x) => x,
                    Err(e) =>
                    {
                        let _ = bridge_tx.send(XmppEvent::Disconnected(e.to_string())).await;
                        return;
                    }
                };

                loop
                {
                    tokio::select!
                    {
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
                                            let ts = timestamp
                                                .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                                                .map(|dt| dt.with_timezone(&chrono::Utc))
                                                .unwrap_or_else(chrono::Utc::now);
                                            Some(XmppEvent::RoomMessage { room, nick, body, timestamp: ts })
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
                                            let ts = timestamp
                                                .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                                                .map(|dt| dt.with_timezone(&chrono::Utc))
                                                .unwrap_or_else(chrono::Utc::now);
                                            Some(XmppEvent::DirectMessage { from, body, timestamp: ts })
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
                            match cmd
                            {
                                Some(XmppCommand::JoinRoom(room_jid)) =>
                                {
                                    debug!("XmppCommand::JoinRoom room={} nick={}", room_jid, nick);
                                    if let Err(e) = client.join_room(&room_jid, &nick).await
                                    {
                                        error!("join_room({}) failed: {}", room_jid, e);
                                        let _ = bridge_tx.send(XmppEvent::RoomJoinFailed
                                        {
                                            room: room_jid,
                                            reason: e.to_string(),
                                        }).await;
                                    }
                                }
                                Some(XmppCommand::SendRoomMessage { room, body }) =>
                                {
                                    debug!("XmppCommand::SendRoomMessage room={} len={}", room, body.len());
                                    if let Err(e) = client.send_room_message(&room, &body).await
                                    {
                                        error!("send_room_message({}) failed: {}", room, e);
                                    }
                                }
                                Some(XmppCommand::LeaveRoom { room, nick }) =>
                                {
                                    debug!("XmppCommand::LeaveRoom room={} nick={}", room, nick);
                                    if let Err(e) = client.leave_room(&room, &nick).await
                                    {
                                        error!("leave_room({}) failed: {}", room, e);
                                    }
                                }
                                Some(XmppCommand::SendDirectMessage { to, body }) =>
                                {
                                    debug!("XmppCommand::SendDirectMessage to={} len={}", to, body.len());
                                    if let Err(e) = client.send_message(&to, &body).await
                                    {
                                        error!("send_message({}) failed: {}", to, e);
                                    }
                                }
                                None => break,
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
