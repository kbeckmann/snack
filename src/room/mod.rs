pub mod user;
pub mod message;
pub mod chat;
pub mod backlog;

pub struct Room
{
    pub jid: String,
    pub title: String,
    pub topic: String,
    pub users: Vec<user::User>,
    pub messages: Vec<message::Message>,
    pub unread: bool,
    // Index of the first message that arrived while this room was not being watched.
    // Set when the window loses focus (for the active room) or when the user switches
    // away from this room. Cleared when the user switches to this room or the window
    // regains focus while this room is active.
    // `None` means no "new messages" divider should be shown.
    pub read_marker: Option<usize>,
    // Windowed-history bookkeeping; see room::backlog::Backlog.
    pub oldest_loaded_id: Option<i64>,
    pub has_older: bool,
    pub at_live_tail: bool,
    pub anchored_bottom: bool,
}

impl backlog::Backlog for Room
{
    fn messages(&self) -> &Vec<message::Message> { &self.messages }
    fn messages_mut(&mut self) -> &mut Vec<message::Message> { &mut self.messages }
    fn read_marker(&self) -> Option<usize> { self.read_marker }
    fn set_read_marker(&mut self, m: Option<usize>) { self.read_marker = m; }
    fn oldest_loaded_id(&self) -> Option<i64> { self.oldest_loaded_id }
    fn set_oldest_loaded_id(&mut self, id: Option<i64>) { self.oldest_loaded_id = id; }
    fn has_older(&self) -> bool { self.has_older }
    fn set_has_older(&mut self, v: bool) { self.has_older = v; }
    fn set_at_live_tail(&mut self, v: bool) { self.at_live_tail = v; }
    fn anchored_bottom(&self) -> bool { self.anchored_bottom }
    fn set_anchored_bottom(&mut self, v: bool) { self.anchored_bottom = v; }
}
