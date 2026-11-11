//! Room state shared by outgoing requests and the incoming stanza reader.
use std::{collections::{HashMap, HashSet}, sync::{Arc, Mutex}};
use crate::{RoomMember, XmppEvent, stanza::muc::{MucPresence, PresenceErrorStanza}};

pub(crate) type Rooms = Arc<Mutex<Registry>>;

pub(crate) const MAX_JOIN_BUFFER_EVENTS: usize = 256;
pub(crate) const MAX_JOIN_BUFFER_BYTES: usize = 4 * 1024 * 1024;

// The reader owns event delivery. Retired entry buffers stay here until it
// forwards them, including when an outgoing request is cancelled mid-write.
#[derive(Default)]
pub(crate) struct Registry
{
    states: HashMap<String, Room>,
    known_rooms: HashSet<String>,
    ready: Vec<XmppEvent>,
    changed: Arc<tokio::sync::Notify>,
    closing: bool,
}

impl std::ops::Deref for Registry
{
    type Target = HashMap<String, Room>;
    fn deref(&self) -> &Self::Target { &self.states }
}
impl std::ops::DerefMut for Registry
{
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.states }
}

impl Registry
{
    pub fn new(changed: Arc<tokio::sync::Notify>) -> Self { Self { changed, ..Self::default() } }

    // Membership can end before an already-routed legacy PM arrives. Remember
    // confirmed room identities for this connection without reviving membership.
    pub fn is_room(&self, jid: &str) -> bool
    {
        self.known_rooms.contains(jid) || key(&self.states, jid).is_some()
            || self.known_rooms.iter().any(|known| crate::stanza::jid_matches(known, jid))
    }

    pub fn is_closing(&self) -> bool { self.closing }
    pub fn take_ready(&mut self) -> Vec<XmppEvent> { std::mem::take(&mut self.ready) }

    pub fn begin_close(&mut self)
    {
        self.closing = true;
        for (jid, room) in self.states.iter_mut() {
            self.ready.extend(room.take_for_early_delivery().into_iter().map(|event| contextual_event(jid, room.context.as_deref(), event)));
        }
        self.changed.notify_one();
    }

    pub fn defer_buffered(&mut self, jid: &str)
    {
        let Some(room) = self.states.get_mut(jid) else { return; };
        if room.messages.is_empty() { return; }
        self.ready.extend(room.take_for_early_delivery().into_iter().map(|event| contextual_event(jid, room.context.as_deref(), event)));
        self.changed.notify_one();
    }
}

pub(crate) fn take_ready(rooms: &Rooms) -> Vec<XmppEvent>
{
    rooms.lock().unwrap().take_ready()
}

pub(crate) fn drain_buffered(rooms: &Rooms) -> Vec<XmppEvent>
{
    let mut rooms = rooms.lock().unwrap();
    let mut events = std::mem::take(&mut rooms.ready);
    for (jid, room) in rooms.states.iter_mut() {
        room.changed.notify_one();
        events.extend(room.take_messages().into_iter().map(|event| contextual_event(jid, room.context.as_deref(), event)));
    }
    events
}

// Preserve the caller's registered key in events and request guards, while
// accepting equivalent DNS case and IPv6 representations from the service.
pub(crate) fn key<'a>(rooms: &'a HashMap<String, Room>, jid: &str) -> Option<&'a str>
{
    rooms.get_key_value(jid).map(|(key, _)| key.as_str())
        .or_else(|| rooms.keys().find(|key| crate::stanza::jid_matches(key, jid)).map(String::as_str))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase { Joining, Joined, Leaving }

struct PendingEntry {
    request_id: String,
    context: Option<String>,
    deadline: Option<tokio::time::Instant>,
    confirmed: bool,
}

pub(crate) struct Room
{
    pub request_id: String,
    pub nick: String,
    pub privacy: crate::RoomPrivacy,
    pub phase: Phase,
    pub members: crate::roster::Roster,
    pub messages: Vec<XmppEvent>,
    buffered_bytes: usize,
    // Latest observed subject, including one already delivered to the caller.
    flushed_subject: Option<String>,
    pub deadline: Option<tokio::time::Instant>,
    pub leave_target: Option<String>,
    pending_entry: Option<PendingEntry>,
    // A confirmed member can be renamed while departure suppresses member
    // events. Retain the published nick so a rejected leave can reconcile it.
    joined_nick_before_leave: Option<String>,
    // Keep a snapshot only during departure. Reconciliation storage follows
    // roster size rather than the duration or event count of a blocked write.
    members_before_leave: Option<crate::roster::Roster>,
    pub context: Option<String>,
    pub changed: Arc<tokio::sync::Notify>,
}

// Account for owned text as well as the event structures. Character counts
// undercount UTF-8; IDs, delay metadata and repeated room keys also occupy RAM.
fn buffered_event_bytes(event: &XmppEvent) -> usize
{
    let optional = |value: &Option<String>| value.as_ref().map_or(0, String::len);
    let text = match event {
        XmppEvent::RoomSubject { room, subject } => room.len() + subject.len(),
        XmppEvent::RoomMessage { room, nick, body, delay, stanza_id, origin_id } => {
            room.len() + nick.len() + body.len() + optional(stanza_id) + optional(origin_id)
                + delay.as_ref().map_or(0, |delay| optional(&delay.stamp) + optional(&delay.from) + optional(&delay.reason))
        }
        XmppEvent::RoomServiceMessage { room, body, delay, stanza_id, origin_id } => {
            room.len() + body.len() + optional(stanza_id) + optional(origin_id)
                + delay.as_ref().map_or(0, |delay| optional(&delay.stamp) + optional(&delay.from) + optional(&delay.reason))
        }
        _ => 0, // Only room subjects and occupant/service text enter the buffer.
    };
    std::mem::size_of::<XmppEvent>() + text
}

impl Room
{
    pub fn take_messages(&mut self) -> Vec<XmppEvent>
    {
        self.buffered_bytes = 0;
        std::mem::take(&mut self.messages)
    }

    pub fn remember_flushed_subject(&mut self, events: &[XmppEvent])
    {
        if let Some(subject) = events.iter().rev().find_map(|event| match event {
            XmppEvent::RoomSubject { subject, .. } => Some(subject),
            _ => None,
        }) { self.flushed_subject = Some(subject.clone()); }
    }

    pub fn take_for_early_delivery(&mut self) -> Vec<XmppEvent>
    {
        let events = self.take_messages();
        self.remember_flushed_subject(&events);
        events
    }

    #[cfg(test)]
    pub fn restore_messages(&mut self, messages: Vec<XmppEvent>)
    {
        self.buffered_bytes = messages.iter().map(buffered_event_bytes).sum();
        self.messages = messages;
    }

    // Return a complete batch to the reader when buffering would exceed either
    // budget. Delivery uses the bounded event channel and never implies a join.
    pub fn buffer_messages(&mut self, mut events: Vec<XmppEvent>) -> Option<Vec<XmppEvent>>
    {
        let bytes: usize = events.iter().map(buffered_event_bytes).sum();
        if self.messages.len().saturating_add(events.len()) > MAX_JOIN_BUFFER_EVENTS
            || self.buffered_bytes.saturating_add(bytes) > MAX_JOIN_BUFFER_BYTES {
            let mut ready = self.take_messages();
            ready.append(&mut events);
            self.remember_flushed_subject(&ready);
            Some(ready)
        } else {
            self.buffered_bytes += bytes;
            self.messages.append(&mut events);
            None
        }
    }

    pub fn departing(request_id: String, context: Option<&str>, previous: &Self) -> Self {
        let mut room = Self::joining(request_id, previous.nick.clone());
        room.phase = Phase::Leaving;
        room.privacy = previous.privacy;
        room.context = context.map(str::to_string).or_else(|| previous.context.clone());
        room.flushed_subject = previous.messages.iter().rev().find_map(|event| match event {
            XmppEvent::RoomSubject { subject, .. } => Some(subject.clone()), _ => None,
        }).or_else(|| previous.flushed_subject.clone());
        room.members = previous.members.clone();
        if previous.phase == Phase::Joined {
            room.joined_nick_before_leave = Some(previous.nick.clone());
            room.members_before_leave = Some(previous.members.clone());
        }
        if previous.phase == Phase::Joining {
            room.pending_entry = Some(PendingEntry { request_id: previous.request_id.clone(), context: previous.context.clone(),
                deadline: previous.deadline, confirmed: false });
        }
        room
    }

    fn joined_events(&mut self, jid: &str) -> Vec<XmppEvent> {
        let mut events = vec![XmppEvent::RoomJoined { room: jid.into(), members: self.members.snapshot(), own_nick: self.nick.clone() }];
        if let Some(subject) = self.flushed_subject.take() { events.push(XmppEvent::RoomSubject { room: jid.into(), subject }); }
        events.extend(self.take_messages());
        self.remember_flushed_subject(&events);
        events
    }

    // A failed departure resumes the original entry, whose self confirmation
    // may have arrived while departure was pending. Never infer it from failure.
    fn resume_entry(&mut self, jid: &str) -> Option<Vec<XmppEvent>> {
        let entry = self.pending_entry.take()?;
        self.request_id = entry.request_id;
        self.context = entry.context;
        self.leave_target = None;
        self.phase = if entry.confirmed { Phase::Joined } else { Phase::Joining };
        self.deadline = if entry.confirmed { None } else { entry.deadline };
        let mut events = if entry.confirmed { self.joined_events(jid) } else { Vec::new() };
        if entry.confirmed && self.privacy != crate::RoomPrivacy::default() {
            events.insert(1, XmppEvent::RoomPrivacy { room: jid.into(), privacy: self.privacy });
        }
        Some(events)
    }

    pub fn joining(request_id: String, nick: String) -> Self
    {
        Self { request_id, nick, privacy: Default::default(), phase: Phase::Joining, members: crate::roster::Roster::default(), messages: Vec::new(), buffered_bytes: 0, flushed_subject: None, deadline: None, leave_target: None, pending_entry: None, joined_nick_before_leave: None, members_before_leave: None, context: None, changed: Arc::new(tokio::sync::Notify::new()) }
    }
}

pub(crate) struct Cleanup(pub Rooms);
impl Drop for Cleanup
{
    fn drop(&mut self) {
        let mut rooms = self.0.lock().unwrap_or_else(|e| e.into_inner());
        for room in rooms.values() { room.changed.notify_one(); }
        rooms.clear();
        rooms.known_rooms.clear();
    }
}

pub(crate) async fn wait_for_departure(rooms: &Rooms, sm: &crate::SharedSmState, jid: &str, timeout: std::time::Duration) -> Result<(), crate::XmppError>
{
    let deadline = tokio::time::Instant::now() + timeout;
    let mut waiting = false;
    loop {
        // Register before checking the state: notify_waiters does not retain a
        // permit, and room cleanup can be delayed by incoming event delivery.
        let connection_changed = sm.acked.notified();
        tokio::pin!(connection_changed);
        connection_changed.as_mut().enable();
        if sm.closed.load(std::sync::atomic::Ordering::SeqCst) || sm.closing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(crate::XmppError::Disconnected);
        }
        let changed = {
            let rooms = rooms.lock().unwrap();
            match key(&rooms, jid).and_then(|key| rooms.get(key)) {
                Some(room) if room.phase == Phase::Leaving => room.changed.clone(),
                Some(_) if waiting => return Err(crate::XmppError::Protocol("Previous room departure was not confirmed.".into())),
                _ => return Ok(()),
            }
        };
        waiting = true;
        // One client operation can wait here at a time (&mut XmppClient).
        // notify_one retains a permit if departure precedes this await.
        tokio::select! {
            _ = connection_changed => {},
            result = tokio::time::timeout_at(deadline, changed.notified()) => {
                if result.is_err() {
                    return Err(crate::XmppError::Timeout("waiting for previous room departure".into()));
                }
            }
        }
    }
}

pub(crate) fn next_deadline(rooms: &Rooms) -> Option<tokio::time::Instant>
{
    rooms.lock().unwrap().values().filter_map(|room| room.deadline).min()
}

pub(crate) fn expire(rooms: &Rooms, now: tokio::time::Instant) -> Vec<(String, String, Option<String>, Vec<XmppEvent>)>
{
    let mut events = Vec::new();
    rooms.lock().unwrap().retain(|jid, room| {
        if room.deadline.is_some_and(|deadline| deadline <= now) {
            events.push((jid.clone(), room.nick.clone(), room.context.clone(), room.take_messages()));
            false
        } else { true }
    });
    events
}

/// A failed or cancelled write restores the preceding room state.
pub(crate) struct RequestGuard
{
    pub rooms: Rooms,
    pub room: String,
    pub request_id: Option<String>,
    pub previous: Option<Room>,
    pub leaving: bool,
}

impl Drop for RequestGuard
{
    fn drop(&mut self)
    {
        if let Some(id) = &self.request_id {
            let mut rooms = self.rooms.lock().unwrap_or_else(|e| e.into_inner());
            if rooms.get(&self.room).is_some_and(|room| room.request_id == *id) {
                rooms[&self.room].changed.notify_one();
                rooms.defer_buffered(&self.room);
                let mut current = rooms.remove(&self.room).unwrap();
                if let Some(mut previous) = self.previous.take() {
                    if self.leaving {
                        // Cancelling an outgoing request does not undo service
                        // updates received while it waited for the writer.
                        let old_nick = previous.nick.clone();
                        let old_privacy = previous.privacy;
                        let was_joined = previous.phase == Phase::Joined;
                        // A rejection may have already published these changes
                        // while the writer was still blocked. Replay only for
                        // a different restored context, or an unreported leave.
                        let replay_members = current.phase == Phase::Leaving || current.context != previous.context;
                        let member_changes = if was_joined && replay_members {
                            current.members.changes_since(&previous.members, &self.room, &old_nick)
                        } else { Vec::new() };
                        let resumed = current.resume_entry(&self.room);
                        if resumed.is_some() {
                            previous.phase = current.phase;
                            previous.deadline = current.deadline;
                            previous.members = std::mem::take(&mut current.members);
                            previous.flushed_subject = current.flushed_subject.take();
                        }
                        previous.members.rename(&old_nick, &current.nick);
                        previous.nick = current.nick;
                        previous.privacy.merge(current.privacy);
                        if was_joined {
                            previous.members = std::mem::take(&mut current.members);
                            let context = previous.context.as_deref();
                            if replay_members && old_nick != previous.nick
                                && !member_changes.iter().any(|event| matches!(event, XmppEvent::MemberNicknameChanged { is_self: true, .. })) {
                                rooms.ready.push(contextual_event(&self.room, context, XmppEvent::MemberNicknameChanged {
                                    room: self.room.clone(), old_nick, new_nick: previous.nick.clone(), is_self: true }));
                            }
                            rooms.ready.extend(member_changes.into_iter().map(|event| contextual_event(&self.room, context, event)));
                            if replay_members && old_privacy != previous.privacy {
                                rooms.ready.push(contextual_event(&self.room, context, XmppEvent::RoomPrivacy {
                                    room: self.room.clone(), privacy: previous.privacy }));
                            }
                            if let Some(subject) = current.flushed_subject.take() {
                                if current.context != previous.context && previous.flushed_subject.as_ref() != Some(&subject) {
                                    rooms.ready.push(contextual_event(&self.room, context, XmppEvent::RoomSubject {
                                        room: self.room.clone(), subject: subject.clone() }));
                                }
                                previous.flushed_subject = Some(subject);
                            }
                        }
                        if let Some(events) = resumed {
                            if previous.phase == Phase::Joined { rooms.known_rooms.insert(self.room.clone()); }
                            rooms.ready.extend(events.into_iter().map(|event| contextual_event(&self.room, previous.context.as_deref(), event)));
                        }
                    }
                    rooms.insert(self.room.clone(), previous);
                }
                rooms.changed.notify_one();
            }
        }
    }
}

pub(crate) fn presence(rooms: &Rooms, presence: &MucPresence) -> Vec<XmppEvent>
{
    let Some((jid, _)) = presence.room_and_nick() else { return Vec::new(); };
    let mut rooms = rooms.lock().unwrap();
    // Read the context and generate events under the same lock. Looking it up
    // later in a forwarding task would label queued old events as a new join.
    // A replacement's confirmation must follow data retired from its preceding
    // entry, even if this stanza was already buffered when the request changed.
    let mut events = std::mem::take(&mut rooms.ready);
    let Some(jid) = key(&rooms, jid).map(str::to_string) else { return events; };
    let context = rooms.get(&jid).and_then(|room| room.context.clone());
    let previous_privacy = rooms[&jid].privacy;
    let mut incoming = presence_inner(&mut rooms, presence, &jid);
    let joined = incoming.iter().any(|event| matches!(event, XmppEvent::RoomJoined { .. }));
    if let Some(state) = rooms.get(&jid) {
        if state.phase == Phase::Joined && state.privacy != crate::RoomPrivacy::default()
            && (joined || state.privacy != previous_privacy) {
            let at = if joined { 1 } else { incoming.len() };
            incoming.insert(at, XmppEvent::RoomPrivacy { room: jid.clone(), privacy: state.privacy });
        }
    }
    if (joined || rooms.get(&jid).is_some_and(|room| room.pending_entry.as_ref().is_some_and(|entry| entry.confirmed)))
        && !rooms.known_rooms.iter().any(|known| crate::stanza::jid_matches(known, &jid)) {
        rooms.known_rooms.insert(jid.clone());
    }
    events.extend(incoming.into_iter().map(|event| contextual_event(&jid, context.as_deref(), event)));
    events
}

// Bare room service text has no occupant identity. It shares entry buffering
// and immutable operation context, but never confirms membership.
pub(crate) fn service_message(rooms: &Rooms, message: &crate::stanza::muc::MucMessage) -> Vec<XmppEvent>
{
    let Some(from) = message.from.as_deref().filter(|from| !from.contains('/')) else { return Vec::new(); };
    if crate::xmpp::parse_address(from).is_err() { return Vec::new(); }
    let mut registry = rooms.lock().unwrap();
    let Some(room) = key(&registry, from)
        .or_else(|| registry.known_rooms.get(from).map(String::as_str))
        .or_else(|| registry.known_rooms.iter().find(|known| crate::stanza::jid_matches(known, from)).map(String::as_str))
        .map(str::to_string) else { return Vec::new(); };
    let mut events = Vec::new();
    if let Some(body) = &message.body {
        events.push(XmppEvent::RoomServiceMessage { room: room.clone(), body: body.clone(),
            delay: message.delay.clone().map(Into::into), stanza_id: message.stanza_id(&room).map(str::to_string), origin_id: message.origin_id.clone() });
    }
    let closing = registry.is_closing();
    let context = if let Some(state) = registry.get_mut(&room) {
        if let Some((_, subject)) = message.subject_change() {
            events.insert(0, XmppEvent::RoomSubject { room: room.clone(), subject: subject.into() });
        }
        if message.message_type.as_deref() == Some("groupchat") && message.delay.is_none() {
            let previous = state.privacy;
            state.privacy.merge(message.privacy);
            if state.phase == Phase::Joined && previous != state.privacy {
                events.insert(0, XmppEvent::RoomPrivacy { room: room.clone(), privacy: state.privacy });
            }
        }
        if state.phase == Phase::Joining {
            if !closing {
                let Some(batch) = state.buffer_messages(events) else { return Vec::new(); };
                events = batch;
            } else { state.remember_flushed_subject(&events); }
        } else {
            state.remember_flushed_subject(&events);
        }
        state.context.clone()
    } else { None };
    let mut ready = registry.take_ready();
    ready.extend(events.into_iter().map(|event| contextual_event(&room, context.as_deref(), event)));
    ready
}

pub(crate) fn contextual_event(room: &str, context: Option<&str>, event: XmppEvent) -> XmppEvent
{
    match context {
        Some(context) => XmppEvent::RoomEvent { room: room.into(), context: context.into(), event: Box::new(event) },
        None => event,
    }
}

fn presence_inner(rooms: &mut HashMap<String, Room>, presence: &MucPresence, jid: &str) -> Vec<XmppEvent>
{
    let Some((_, nick)) = presence.room_and_nick() else { return Vec::new(); };
    if !matches!(presence.presence_type.as_deref(), None | Some("unavailable")) { return Vec::new(); }
    let Some(room) = rooms.get_mut(jid) else { return Vec::new(); };
    let is_self = presence.is_self_presence();
    // The room service can assign a different nickname and signals it with
    // status 210. An unrelated occupant's self marker cannot confirm our join.
    if is_self && !room.nick.is_empty() && nick != room.nick
        && !presence.muc_user_x().is_some_and(|x| x.statuses().any(|s| s.code == "210")) {
        return Vec::new();
    }
    if presence.presence_type.as_deref() == Some("unavailable") {
        if let Some(x) = presence.muc_user_x() {
            if x.statuses().any(|status| status.code == "303") {
                let Some(new_nick) = x.items().next().and_then(|item| item.nick.as_deref())
                    .filter(|nick| crate::validate_room_target(jid, nick).is_ok()) else { return Vec::new(); };
                if is_self { room.nick = new_nick.into(); }
                room.members.rename(nick, new_nick);
                return if room.phase == Phase::Joined {
                    vec![XmppEvent::MemberNicknameChanged { room: jid.into(), old_nick: nick.into(), new_nick: new_nick.into(), is_self }]
                } else { Vec::new() };
            }
        }
        if is_self {
            let joining = room.phase == Phase::Joining;
            room.changed.notify_one();
            let mut events = room.take_messages();
            rooms.remove(jid);
            events.push(if joining {
                XmppEvent::RoomJoinFailed { room: jid.into(), reason: "Room entry ended before it was confirmed.".into() }
            } else { XmppEvent::RoomLeft(jid.into()) });
            return events;
        }
        room.members.remove(nick);
        return if room.phase == Phase::Joined { vec![XmppEvent::MemberLeft { room: jid.into(), nick: nick.into() }] } else { Vec::new() };
    }
    let Some(x) = presence.muc_user_x() else { return Vec::new(); };
    if is_self {
        let mut update = crate::RoomPrivacy::default();
        for status in x.statuses() {
            match status.code.as_str() {
                "100" => update.non_anonymous = Some(true),
                "170" => update.publicly_logged = Some(true),
                _ => {},
            }
        }
        room.privacy.merge(update);
    }
    let member = RoomMember {
        jid: x.jid().map(str::to_string), nick: nick.into(),
        affiliation: x.items().next().and_then(|i| i.affiliation.clone()).unwrap_or_default(),
        role: x.items().next().and_then(|i| i.role.clone()).unwrap_or_default(),
        show: presence.show().map(str::to_string), status: presence.status().map(str::to_string),
    };
    if room.phase == Phase::Leaving {
        if is_self && room.nick != nick { room.members.rename(&room.nick, nick); }
        room.members.insert(member);
        if let Some(entry) = &mut room.pending_entry {
            if is_self { entry.confirmed = true; }
        }
        if is_self { room.nick = nick.into(); }
        return Vec::new();
    }
    if room.phase == Phase::Joined {
        let mut events = Vec::new();
        if is_self && room.nick != nick {
            events.push(XmppEvent::MemberNicknameChanged { room: jid.into(), old_nick: room.nick.clone(), new_nick: nick.into(), is_self: true });
            room.members.rename(&room.nick, nick);
            room.nick = nick.into();
        }
        room.members.insert(member.clone());
        events.push(XmppEvent::MemberJoined { room: jid.into(), member });
        return events;
    }
    room.members.insert(member);
    if !is_self { return Vec::new(); }
    room.nick = nick.into();
    room.phase = Phase::Joined;
    room.deadline = None;
    room.joined_events(jid)
}

pub(crate) fn error(rooms: &Rooms, error: PresenceErrorStanza) -> Vec<XmppEvent>
{
    if error.error_type == "continue" { return Vec::new(); }
    let sender = error.from.split('/').next().unwrap_or(&error.from);
    let mut rooms = rooms.lock().unwrap();
    let Some(jid) = key(&rooms, sender).map(str::to_string) else { return Vec::new(); };
    let room = rooms.get_mut(&jid).unwrap();
    if room.phase == Phase::Joined { return Vec::new(); }
    let resource = error.from.split_once('/').map(|(_, resource)| resource);
    if resource.is_some_and(|nick| nick != room.nick) { return Vec::new(); }
    // A leave can overtake the response to its original join. A correlated
    // rejection proves absence only until valid self presence confirms entry.
    // Report each operation under its own context and wake rejoin waiters.
    if room.phase == Phase::Leaving && room.pending_entry.as_ref().is_some_and(|entry|
        !entry.confirmed && error.id.as_deref() == Some(entry.request_id.as_str())) {
        let entry_context = room.pending_entry.as_ref().unwrap().context.clone();
        let departure_context = room.context.clone();
        let buffered = room.take_messages();
        room.changed.notify_one();
        rooms.remove(&jid);
        let mut events: Vec<_> = buffered.into_iter().map(|event| contextual_event(&jid, departure_context.as_deref(), event)).collect();
        events.push(contextual_event(&jid, entry_context.as_deref(), XmppEvent::PresenceError {
            from: resource.map_or_else(|| jid.clone(), |nick| format!("{jid}/{nick}")),
            error_type: error.error_type, condition: error.condition, text: error.text }));
        events.push(contextual_event(&jid, departure_context.as_deref(), XmppEvent::RoomLeft(jid.clone())));
        return events;
    }
    if error.id.as_ref().is_some_and(|id| id != &room.request_id) { return Vec::new(); }
    let mut events = room.take_messages();
    let context = room.context.clone();
    let room_jid = jid.to_string();
    if room.phase == Phase::Leaving {
        if let Some(resumed) = room.resume_entry(&jid) {
            room.changed.notify_one();
            let resumed_context = room.context.clone();
            let confirmed = room.phase == Phase::Joined;
            events.push(XmppEvent::PresenceError { from: resource.map_or_else(|| jid.clone(), |nick| format!("{jid}/{nick}")),
                error_type: error.error_type, condition: error.condition, text: error.text });
            let mut events: Vec<_> = events.into_iter().map(|event| contextual_event(&room_jid, context.as_deref(), event)).collect();
            events.extend(resumed.into_iter().map(|event| contextual_event(&room_jid, resumed_context.as_deref(), event)));
            if confirmed { rooms.known_rooms.insert(jid); }
            return events;
        }
    }
    let resumed_privacy = (room.phase == Phase::Leaving && room.privacy != crate::RoomPrivacy::default()).then_some(room.privacy);
    let resumed_members = room.members_before_leave.take().map(|previous|
        room.members.changes_since(&previous, &jid, room.joined_nick_before_leave.as_deref().unwrap_or(&room.nick))).unwrap_or_default();
    let resumed_nickname = room.joined_nick_before_leave.take().filter(|nick| nick != &room.nick)
        .map(|old_nick| XmppEvent::MemberNicknameChanged { room: jid.clone(), old_nick, new_nick: room.nick.clone(), is_self: true });
    room.changed.notify_one();
    if room.phase == Phase::Joining { rooms.remove(&jid); }
    else { room.phase = Phase::Joined; }
    events.push(XmppEvent::PresenceError { from: resource.map_or_else(|| jid.clone(), |nick| format!("{jid}/{nick}")), error_type: error.error_type, condition: error.condition, text: error.text });
    if let Some(nickname) = resumed_nickname.filter(|_| !resumed_members.iter().any(|event|
        matches!(event, XmppEvent::MemberNicknameChanged { is_self: true, .. }))) { events.push(nickname); }
    events.extend(resumed_members);
    // Service updates received while leaving were retained without emitting
    // membership events. Publish the full current snapshot when leaving fails.
    if let Some(privacy) = resumed_privacy { events.push(XmppEvent::RoomPrivacy { room: jid.clone(), privacy }); }
    events.into_iter().map(|event| contextual_event(&room_jid, context.as_deref(), event)).collect()
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn replacement_confirmation_follows_retired_data_with_its_original_context() {
        let rooms: Rooms = Default::default();
        {
            let mut registry = rooms.lock().unwrap();
            let mut original = Room::joining("first".into(), "me".into());
            original.context = Some("original".into());
            original.messages.push(XmppEvent::RoomMessage { room: "room@example".into(), nick: "alice".into(), body: "before replacement".into(), delay: None, stanza_id: None, origin_id: None });
            registry.insert("room@example".into(), original);
            registry.defer_buffered("room@example");
            let mut replacement = Room::joining("second".into(), "me".into());
            replacement.context = Some("replacement".into());
            registry.insert("room@example".into(), replacement);
        }
        let xml = "<presence xmlns='jabber:client' from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/></x></presence>";
        let events = presence(&rooms, &MucPresence::from_xml(xml).unwrap());
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], XmppEvent::RoomEvent { context, event, .. }
            if context == "original" && matches!(event.as_ref(), XmppEvent::RoomMessage { body, .. } if body == "before replacement")));
        assert!(matches!(&events[1], XmppEvent::RoomEvent { context, event, .. }
            if context == "replacement" && matches!(event.as_ref(), XmppEvent::RoomJoined { .. })));
        assert!(take_ready(&rooms).is_empty());
    }

    #[tokio::test]
    async fn departure_wait_has_a_deadline_and_a_rejected_departure_is_not_success() {
        let rooms: Rooms = Default::default();
        let sm = Arc::new(crate::SmState::new());
        let mut room = Room::joining("leave".into(), "me".into());
        room.phase = Phase::Leaving;
        let changed = room.changed.clone();
        rooms.lock().unwrap().insert("room@example".into(), room);
        let result = wait_for_departure(&rooms, &sm, "room@EXAMPLE", std::time::Duration::from_millis(20)).await;
        assert!(matches!(result, Err(crate::XmppError::Timeout(_))));
        assert!(rooms.lock().unwrap()["room@example"].phase == Phase::Leaving);
        let waiting = wait_for_departure(&rooms, &sm, "room@EXAMPLE", std::time::Duration::from_secs(1));
        tokio::pin!(waiting);
        // Poll the wait into Pending before simulating a correlated error.
        tokio::select! {
            biased;
            result = &mut waiting => panic!("departure completed unexpectedly: {result:?}"),
            _ = std::future::ready(()) => {},
        }
        rooms.lock().unwrap().get_mut("room@example").unwrap().phase = Phase::Joined;
        changed.notify_one();
        assert!(matches!(waiting.await, Err(crate::XmppError::Protocol(_))));
    }

    #[test]
    fn member_renames_preserve_rosters_and_membership_during_entry_and_after_joining() {
        for phase in [Phase::Joining, Phase::Joined] {
            let rooms: Rooms = Default::default();
            let mut state = Room::joining("request".into(), "me".into());
            state.phase = phase;
            state.members.insert(RoomMember { nick: "old".into(), jid: Some("alice@example".into()), role: String::new(), affiliation: String::new(), show: None, status: None });
            rooms.lock().unwrap().insert("room@example".into(), state);
            let xml = "<presence xmlns='jabber:client' type='unavailable' from='room@example/old'><x xmlns='http://jabber.org/protocol/muc#user'><item nick='new'/><status code='303'/></x></presence>";
            let events = presence(&rooms, &MucPresence::from_xml(xml).unwrap());
            if phase == Phase::Joined {
                assert!(matches!(&events[..], [XmppEvent::MemberNicknameChanged { old_nick, new_nick, is_self: false, .. }] if old_nick == "old" && new_nick == "new"));
            } else { assert!(events.is_empty()); }
            let rooms = rooms.lock().unwrap();
            assert!(rooms["room@example"].phase == phase);
            assert_eq!(rooms["room@example"].members.as_slice().len(), 1);
            assert_eq!(rooms["room@example"].members.as_slice()[0].nick, "new");
            assert_eq!(rooms["room@example"].members.as_slice()[0].jid.as_deref(), Some("alice@example"));
        }
    }

    #[test]
    fn service_assigned_nickname_updates_during_membership_reach_the_application() {
        let rooms: Rooms = Default::default();
        let mut state = Room::joining("request".into(), "OldNick".into());
        state.phase = Phase::Joined;
        rooms.lock().unwrap().insert("room@example".into(), state);
        let xml = "<presence xmlns='jabber:client' from='room@example/oldnick'><x xmlns='http://jabber.org/protocol/muc#user'><item/><status code='110'/><status code='210'/></x></presence>";
        let events = presence(&rooms, &MucPresence::from_xml(xml).unwrap());
        assert!(matches!(&events[..], [XmppEvent::MemberNicknameChanged { old_nick, new_nick, is_self: true, .. }, XmppEvent::MemberJoined { .. }] if old_nick == "OldNick" && new_nick == "oldnick"));
        assert_eq!(rooms.lock().unwrap()["room@example"].nick, "oldnick");
    }

    #[test]
    fn rejected_departure_coalesces_renames_and_starts_the_next_leave_from_the_current_nick() {
        for final_nick in ["old", "last"] {
            let rooms: Rooms = Default::default();
            let mut joined = Room::joining("entry".into(), "old".into());
            joined.phase = Phase::Joined;
            rooms.lock().unwrap().insert("room@example".into(), Room::departing("leave".into(), None, &joined));
            for (old, new) in [("old", "middle"), ("middle", final_nick)] {
                let xml = format!("<presence xmlns='jabber:client' type='unavailable' from='room@example/{old}'><x xmlns='http://jabber.org/protocol/muc#user'><item nick='{new}'/><status code='110'/><status code='303'/></x></presence>");
                assert!(presence(&rooms, &MucPresence::from_xml(&xml).unwrap()).is_empty());
            }
            let rejected = |id: &str| PresenceErrorStanza {
                id: Some(id.into()), from: format!("room@example/{final_nick}"),
                error_type: "cancel".into(), condition: "forbidden".into(), text: None,
            };
            assert!(error(&rooms, rejected("unrelated")).is_empty());
            let events = error(&rooms, rejected("leave"));
            assert!(matches!(&events[0], XmppEvent::PresenceError { .. }));
            if final_nick == "old" {
                assert_eq!(events.len(), 1, "returning to the published nick needs no rename");
            } else {
                assert!(matches!(&events[1..], [XmppEvent::MemberNicknameChanged { old_nick, new_nick, is_self: true, .. }]
                    if old_nick == "old" && new_nick == final_nick));
            }
            {
                let mut rooms = rooms.lock().unwrap();
                let previous = rooms.remove("room@example").unwrap();
                assert!(previous.phase == Phase::Joined);
                rooms.insert("room@example".into(), Room::departing("next-leave".into(), None, &previous));
            }
            assert!(matches!(&error(&rooms, rejected("next-leave"))[..], [XmppEvent::PresenceError { .. }]),
                "an unchanged retry must not republish the previous rename");
        }
    }

    #[test]
    fn incomplete_or_qualified_rename_nicknames_do_not_remove_the_room() {
        let rooms: Rooms = Default::default();
        let mut state = Room::joining("request".into(), "me".into());
        state.phase = Phase::Joined;
        rooms.lock().unwrap().insert("room@example".into(), state);
        for item in ["<item/>", "<item nick=''/>", "<item xmlns:e='extension' e:nick='forged'/>"] {
            let xml = format!("<presence xmlns='jabber:client' type='unavailable' from='room@example/me'><x xmlns='http://jabber.org/protocol/muc#user'>{item}<status code='110'/><status code='303'/></x></presence>");
            assert!(presence(&rooms, &MucPresence::from_xml(&xml).unwrap()).is_empty());
            let rooms = rooms.lock().unwrap();
            assert!(rooms["room@example"].phase == Phase::Joined);
            assert_eq!(rooms["room@example"].nick, "me");
        }
    }
}
