pub mod user;
pub mod message;
pub mod chat;
pub mod backlog;

pub struct Room
{
    pub jid: String,
    pub title: String,
    pub topic: String,
    pub privacy: ::xmpp::RoomPrivacy,
    pub own_nick: String,
    pub users: Vec<user::User>,
    pub messages: Vec<message::Message>,
    // Messages without a DB copy that are outside the rendered history window.
    pub parked_messages: Vec<message::Message>,
    pub unread: bool,
    // Visible projection of read_boundary; the identity survives window changes.
    pub read_marker: Option<usize>,
    pub read_boundary: Option<backlog::ReadBoundary>,
    // One boundary for each reconnect, retained while its messages are loaded.
    pub history_markers: Vec<usize>,
    // Windowed-history bookkeeping; see room::backlog::Backlog.
    pub oldest_loaded_id: Option<i64>,
    pub local_cursor: Option<(i64, i64)>,
    pub has_older: bool,
    pub at_live_tail: bool,
    pub anchored_bottom: bool,
    pub mam_in_flight: bool,
    pub mam_exhausted: bool,
    pub mam_cursor: Option<String>,
}

impl backlog::Backlog for Room
{
    fn messages(&self) -> &Vec<message::Message> { &self.messages }
    fn messages_mut(&mut self) -> &mut Vec<message::Message> { &mut self.messages }
    fn parked_messages_mut(&mut self) -> &mut Vec<message::Message> { &mut self.parked_messages }
    fn read_boundary(&self) -> Option<backlog::ReadBoundary> { self.read_boundary }
    fn set_read_boundary(&mut self, boundary: Option<backlog::ReadBoundary>) { self.read_boundary = boundary; }
    fn read_marker(&self) -> Option<usize> { self.read_marker }
    fn set_read_marker(&mut self, m: Option<usize>) { self.read_marker = m; }
    fn reset_history_markers(&mut self) { self.history_markers.clear(); }
    fn shift_history_markers(&mut self, removed: usize, added: usize)
    {
        self.history_markers.retain(|m| *m >= removed);
        for marker in &mut self.history_markers { *marker = *marker - removed + added; }
    }
    fn remap_history_markers(&mut self, positions: &[usize])
    {
        for marker in &mut self.history_markers {
            *marker = positions.get(*marker).copied().unwrap_or(positions.len());
        }
    }
    fn oldest_loaded_id(&self) -> Option<i64> { self.oldest_loaded_id }
    fn set_oldest_loaded_id(&mut self, id: Option<i64>) { self.oldest_loaded_id = id; }
    fn local_cursor(&self) -> Option<(i64, i64)> { self.local_cursor }
    fn set_local_cursor(&mut self, cursor: Option<(i64, i64)>) { self.local_cursor = cursor; }
    fn has_older(&self) -> bool { self.has_older }
    fn set_has_older(&mut self, v: bool) { self.has_older = v; }
    fn set_at_live_tail(&mut self, v: bool) { self.at_live_tail = v; }
    fn anchored_bottom(&self) -> bool { self.anchored_bottom }
    fn set_anchored_bottom(&mut self, v: bool) { self.anchored_bottom = v; }
    fn mam_in_flight(&self) -> bool { self.mam_in_flight }
    fn set_mam_in_flight(&mut self, v: bool) { self.mam_in_flight = v; }
    fn mam_exhausted(&self) -> bool { self.mam_exhausted }
    fn set_mam_exhausted(&mut self, v: bool) { self.mam_exhausted = v; }
    fn mam_cursor(&self) -> Option<String> { self.mam_cursor.clone() }
    fn set_mam_cursor(&mut self, v: Option<String>) { self.mam_cursor = v; }
}

#[cfg(test)]
mod tests
{
    use super::*;
    use super::backlog::Backlog;

    #[test]
    fn reconnect_boundaries_follow_history_window_changes()
    {
        let mut room = Room
        {
            jid: String::new(), title: String::new(), topic: String::new(), privacy: Default::default(), own_nick: String::new(),
            users: Vec::new(), messages: Vec::new(), parked_messages: Vec::new(), unread: false,
            read_marker: None, read_boundary: None, history_markers: vec![1, 3, 5],
            oldest_loaded_id: None, local_cursor: None, has_older: false, at_live_tail: true,
            anchored_bottom: true, mam_in_flight: false, mam_exhausted: false,
            mam_cursor: None,
        };
        room.shift_history_markers(0, 2);
        assert_eq!(room.history_markers, vec![3, 5, 7]);
        room.shift_history_markers(5, 0);
        assert_eq!(room.history_markers, vec![0, 2]);
        room.set_window(Vec::new(), false, false);
        assert!(room.history_markers.is_empty());
        room.history_markers.push(0);
        room.seed_tail(Vec::new(), false);
        assert!(room.history_markers.is_empty());
    }
}
