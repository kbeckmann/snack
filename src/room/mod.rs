pub mod user;
pub mod message;
pub mod chat;

pub struct Room
{
    pub jid: String,
    pub title: String,
    pub topic: String,
    pub users: Vec<user::User>,
    pub messages: Vec<message::Message>,
    pub unread: bool,
    // Index of the first message that arrived while this room was not being watched.
    pub read_marker: Option<usize>,
    // Index of the first message in each backlog the server replays after a
    // reconnect re-join, one entry per reconnect. These messages can repeat
    // ones already in `messages`, so a divider is shown at each index to
    // separate the old chat from that replayed "new history".
    pub history_markers: Vec<usize>,
}
