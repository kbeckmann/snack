//! `libxmpp` is a small async XMPP client library built on Tokio.
//!
//! It supports STARTTLS, SASL, MUC rooms, one-to-one chat, plus a small
//! escape-hatch surface (`send_raw`, `send_iq_*`, PubSub helpers, OMEMO
//! stanza emission and parsing) used by the snack OMEMO module.

use tokio::sync::{ Notify, oneshot };
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use std::sync::{ Arc, Mutex };
use std::collections::{ HashMap, HashSet };
use std::time::Duration;

mod tcp_stream;
mod xml_framer;
pub mod stanza;
mod xmpp;

use stanza::Stanza;
use xml_framer::XmlFramer;

pub use stanza::iq::{ Iq, pubsub_publish_payload, pubsub_publish_open_payload, pubsub_items_payload };
pub use stanza::omemo::{
    OmemoEncrypted, OmemoKey, OmemoChatMessage,
    DeviceList, Bundle, PreKey,
    OMEMO_NS, OMEMO_DEVICELIST_NODE, OMEMO_BUNDLE_NODE_PREFIX,
};

/// Error returned for failed IQ requests.
#[derive(Debug, Clone)]
pub struct IqError
{
    pub condition: String,
    pub text: Option<String>,
}

impl std::fmt::Display for IqError
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result
    {
        match &self.text
        {
            Some(t) => write!(f, "{}: {}", self.condition, t),
            None => write!(f, "{}", self.condition),
        }
    }
}

impl std::error::Error for IqError {}

/// One item delivered inside a PubSub items result or event notification.
#[derive(Debug, Clone)]
pub struct PubSubItem
{
    pub id: Option<String>,
    pub payload_xml: String,
}

#[derive(Debug, Clone)]
pub enum XmppEvent
{
    Connecting,
    EstablishingTls,
    Authenticating,
    Connected,
    RoomJoined { room: String, members: Vec<RoomMember> },
    RoomLeft(String),
    MemberJoined { room: String, member: RoomMember },
    MemberLeft { room: String, nick: String },
    RoomMessage { room: String, nick: String, body: String, timestamp: Option<String> },
    RoomSubject { room: String, subject: String },
    PresenceError { from: String, error_type: String, condition: String, text: Option<String> },
    DirectMessage { from: String, body: String, timestamp: Option<String> },
    /// Inbound XEP-0384 (legacy axolotl) encrypted message.
    EncryptedDirectMessage
    {
        from: String,
        sid: u32,
        keys: Vec<OmemoKey>,
        iv_b64: String,
        payload_b64: Option<String>,
        timestamp: Option<String>,
    },
    /// PubSub/PEP event notification (XEP-0060 §12.1).
    PubSubEvent
    {
        from: String,
        node: String,
        items: Vec<PubSubItem>,
    },
    /// IQ request sent _to_ us (e.g. by a peer). We must respond.
    IncomingIq
    {
        id: String,
        iq_type: String,
        from: Option<String>,
        payload_xml: String,
    },
}

#[derive(Debug, Clone)]
pub struct RoomMember
{
    pub jid: Option<String>,
    pub nick: String,
    pub affiliation: String,
    pub role: String,
    pub show: Option<String>,
    pub status: Option<String>,
}

pub(crate) type IqWaiters =
    Arc<Mutex<HashMap<String, oneshot::Sender<Result<String, IqError>>>>>;

pub struct XmppClient
{
    shutdown: Arc<Notify>,
    task: JoinHandle<()>,
    writer: tcp_stream::TcpWriter,
    bound_jid: String,
    iq_waiters: IqWaiters,
    iq_counter: std::sync::atomic::AtomicU64,
}

impl XmppClient
{
    pub async fn new(jid: &str, password: &str) -> Result<(Self, mpsc::Receiver<XmppEvent>), String>
    {
        let (event_tx, event_rx) = mpsc::channel(32);

        let (bound_jid, tcp) = xmpp::setup_connection(&event_tx, jid, password).await?;

        let (mut reader, mut writer) = tcp.split()?;

        writer.write(b"<presence/>").await?;

        let shutdown = Arc::new(Notify::new());
        let shutdown_clone = shutdown.clone();
        let event_tx_loop = event_tx.clone();

        let iq_waiters: IqWaiters = Arc::new(Mutex::new(HashMap::new()));
        let iq_waiters_loop = iq_waiters.clone();

        let task = tokio::spawn(async move
        {
            let mut framer = XmlFramer::new_opened();
            let mut pending_joins: HashMap<String, Vec<RoomMember>> = HashMap::new();
            let mut pending_messages: HashMap<String, Vec<XmppEvent>> = HashMap::new();
            let mut joined_rooms: HashSet<String> = HashSet::new();

            loop
            {
                while let Some(stanza_xml) = framer.try_next()
                {
                    log::debug!("Received stanza: {}", stanza_xml);
                    xmpp::process_stanza(
                        &stanza_xml,
                        &event_tx_loop,
                        &mut pending_joins,
                        &mut pending_messages,
                        &mut joined_rooms,
                        &iq_waiters_loop,
                    ).await;
                }

                tokio::select!
                {
                    result = reader.read() =>
                    {
                        match result
                        {
                            Ok(data) =>
                            {
                                framer.feed(&data);
                            }
                            Err(e) =>
                            {
                                log::error!("Read error: {}", e);
                                break;
                            }
                        }
                    }
                    _ = shutdown_clone.notified() =>
                    {
                        break;
                    }
                }
            }
        });

        let _ = event_tx.send(XmppEvent::Connected).await;

        return Ok((
            Self
            {
                shutdown,
                task,
                writer,
                bound_jid,
                iq_waiters,
                iq_counter: std::sync::atomic::AtomicU64::new(1),
            },
            event_rx,
        ));
    }

    pub fn get_jid(&self) -> &str
    {
        return &self.bound_jid;
    }

    pub async fn join_room(&mut self, room_jid: &str, nick: &str) -> Result<(), String>
    {
        let presence = stanza::muc::MucJoinPresence::new(room_jid.to_string(), nick.to_string());
        return self.writer.write(&presence.as_bytes()).await;
    }

    pub async fn leave_room(&mut self, room_jid: &str, nick: &str) -> Result<(), String>
    {
        let presence = stanza::muc::MucLeavePresence::new(room_jid.to_string(), nick.to_string());
        return self.writer.write(&presence.as_bytes()).await;
    }

    pub async fn send_room_message(&mut self, room_jid: &str, body: &str) -> Result<(), String>
    {
        let msg = stanza::muc::MucGroupMessage::new(room_jid.to_string(), body.to_string());
        return self.writer.write(&msg.as_bytes()).await;
    }

    pub async fn send_message(&mut self, to: &str, body: &str) -> Result<(), String>
    {
        let msg = stanza::chat::ChatMessage::new(to.to_string(), body.to_string());
        return self.writer.write(&msg.as_bytes()).await;
    }

    /// Write a raw XML stanza onto the stream. Caller is responsible for
    /// producing well-formed XML in the jabber:client namespace.
    pub async fn send_raw(&mut self, xml: &str) -> Result<(), String>
    {
        return self.writer.write(xml.as_bytes()).await;
    }

    /// Send an IQ request and await the response. `to == None` targets the
    /// user's own server (e.g. for PEP requests on the bound JID).
    pub async fn send_iq(
        &mut self,
        iq_type: &str,
        to: Option<&str>,
        payload_xml: &str,
    ) -> Result<String, IqError>
    {
        let id = self.next_iq_id();
        let iq = Iq
        {
            id: id.clone(),
            iq_type: iq_type.to_string(),
            to: to.map(|s| s.to_string()),
            payload_xml: payload_xml.to_string(),
        };

        let (tx, rx) = oneshot::channel();
        self.iq_waiters.lock().unwrap().insert(id.clone(), tx);

        if let Err(e) = self.writer.write(&iq.as_bytes()).await
        {
            self.iq_waiters.lock().unwrap().remove(&id);
            return Err(IqError { condition: "write-failed".into(), text: Some(e) });
        }

        let resp = match tokio::time::timeout(Duration::from_secs(30), rx).await
        {
            Ok(Ok(r)) => r,
            Ok(Err(_)) =>
            {
                self.iq_waiters.lock().unwrap().remove(&id);
                return Err(IqError { condition: "internal-error".into(), text: Some("waiter dropped".into()) });
            }
            Err(_) =>
            {
                self.iq_waiters.lock().unwrap().remove(&id);
                return Err(IqError { condition: "remote-server-timeout".into(), text: None });
            }
        };

        return resp;
    }

    pub async fn send_iq_get(&mut self, to: Option<&str>, payload_xml: &str) -> Result<String, IqError>
    {
        return self.send_iq("get", to, payload_xml).await;
    }

    pub async fn send_iq_set(&mut self, to: Option<&str>, payload_xml: &str) -> Result<String, IqError>
    {
        return self.send_iq("set", to, payload_xml).await;
    }

    /// Publish an item to a PubSub/PEP node. With `open = true`, the publish
    /// includes options to make the node readable to anyone, as required for
    /// OMEMO bundles and device lists.
    pub async fn pubsub_publish(
        &mut self,
        to: Option<&str>,
        node: &str,
        item_id: Option<&str>,
        item_xml: &str,
        open: bool,
    ) -> Result<(), IqError>
    {
        let payload = if open
        {
            pubsub_publish_open_payload(node, item_id, item_xml)
        }
        else
        {
            pubsub_publish_payload(node, item_id, item_xml)
        };

        let _ = self.send_iq_set(to, &payload).await?;
        return Ok(());
    }

    /// Retrieve items from a PubSub/PEP node.
    pub async fn pubsub_get_items(
        &mut self,
        to: &str,
        node: &str,
        max_items: Option<u32>,
    ) -> Result<Vec<PubSubItem>, IqError>
    {
        let payload = pubsub_items_payload(node, max_items);
        let result_xml = self.send_iq_get(Some(to), &payload).await?;
        return Ok(parse_pubsub_items(&result_xml, node));
    }

    /// Send an OMEMO encrypted chat message.
    pub async fn send_omemo_message(&mut self, msg: &OmemoChatMessage) -> Result<(), String>
    {
        return self.writer.write(&msg.as_bytes()).await;
    }

    /// Respond to an incoming IQ.
    pub async fn respond_iq_result(
        &mut self,
        id: &str,
        to: Option<&str>,
        payload_xml: &str,
    ) -> Result<(), String>
    {
        let to_attr = to.map(|t| format!(" to='{}'", t)).unwrap_or_default();
        let xml = format!("<iq type='result' id='{}'{}>{}</iq>", id, to_attr, payload_xml);
        return self.writer.write(xml.as_bytes()).await;
    }

    fn next_iq_id(&self) -> String
    {
        let n = self.iq_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return format!("snack-{}", n);
    }

    pub async fn close(mut self)
    {
        self.shutdown.notify_one();
        let _ = self.task.await;
        self.writer.shutdown().await;
    }
}

/// Extract `<item id='..'>...</item>` children out of a pubsub items result.
pub(crate) fn parse_pubsub_items(xml: &str, node: &str) -> Vec<PubSubItem>
{
    let mut out = Vec::new();
    let single = format!("node='{}'", node);
    let double = format!("node=\"{}\"", node);

    let items_open = match xml.find("<items ")
    {
        Some(p) =>
        {
            let close = xml[p..].find('>').unwrap_or(0) + p + 1;
            if xml[p..close].contains(&single) || xml[p..close].contains(&double)
            {
                close
            }
            else
            {
                return out;
            }
        }
        None => return out,
    };

    let items_close = xml[items_open..].find("</items>").map(|p| p + items_open).unwrap_or(xml.len());
    let body = &xml[items_open..items_close];

    let mut pos = 0;
    while let Some(item_start) = body[pos..].find("<item")
    {
        let abs_start = pos + item_start;
        let open_end = body[abs_start..].find('>').unwrap_or(0) + abs_start + 1;

        if body[abs_start..open_end].ends_with("/>")
        {
            pos = open_end;
            continue;
        }

        let close_pos = match body[open_end..].find("</item>")
        {
            Some(p) => p + open_end,
            None => break,
        };

        let id = extract_attr_in_tag(&body[abs_start..open_end], "id");
        let payload_xml = body[open_end..close_pos].trim().to_string();
        out.push(PubSubItem { id, payload_xml });

        pos = close_pos + "</item>".len();
    }

    return out;
}

pub(crate) fn extract_attr_in_tag(open_tag: &str, name: &str) -> Option<String>
{
    let patterns = [
        format!("{}='", name),
        format!("{}=\"", name),
    ];
    for (i, p) in patterns.iter().enumerate()
    {
        if let Some(pos) = open_tag.find(p)
        {
            let q = if i == 0 { '\'' } else { '"' };
            let start = pos + p.len();
            if let Some(end) = open_tag[start..].find(q)
            {
                return Some(open_tag[start..start + end].to_string());
            }
        }
    }
    return None;
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn parse_pubsub_items_extracts_each_item()
    {
        let xml = "<iq type='result' id='1'>\
            <pubsub xmlns='http://jabber.org/protocol/pubsub'>\
                <items node='eu.siacs.conversations.axolotl.devicelist'>\
                    <item id='current'><list xmlns='eu.siacs.conversations.axolotl'><device id='42'/></list></item>\
                </items>\
            </pubsub>\
        </iq>";

        let items = parse_pubsub_items(xml, "eu.siacs.conversations.axolotl.devicelist");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.as_deref(), Some("current"));
        assert!(items[0].payload_xml.contains("<device id='42'/>"));
    }

    #[test]
    fn parse_pubsub_items_returns_empty_for_other_node()
    {
        let xml = "<iq type='result' id='1'>\
            <pubsub xmlns='http://jabber.org/protocol/pubsub'>\
                <items node='other.node'><item id='x'><a/></item></items>\
            </pubsub>\
        </iq>";
        assert!(parse_pubsub_items(xml, "some.node").is_empty());
    }
}
