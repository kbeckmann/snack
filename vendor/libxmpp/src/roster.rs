//! Current room members, indexed by nickname without changing arrival order.
use std::collections::HashMap;
use crate::{RoomMember, XmppEvent};

#[derive(Clone, Default)]
pub(crate) struct Roster
{
    members: Vec<RoomMember>,
    by_nick: HashMap<String, usize>,
    // Local occupant identities survive anonymous nickname changes. A leave
    // and rejoin with the same nickname is a different occupant incarnation.
    identities: Vec<u64>,
    next_identity: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(nick: &str, status: &str) -> RoomMember {
        RoomMember { nick: nick.into(), jid: None, affiliation: "member".into(), role: "participant".into(), show: None, status: Some(status.into()) }
    }

    #[test]
    fn reconciliation_handles_rename_cycles_reused_names_and_self_renames() {
        for scenario in ["cycle", "vacated target", "rejoin", "self cycle"] {
            let mut original = Roster::default();
            original.insert(member("alice", "first occupant"));
            original.insert(member("bob", "second occupant"));
            original.insert(member("me", "own occupant"));
            let mut current = original.clone();
            match scenario {
                "cycle" => { current.rename("alice", "spare"); current.rename("bob", "alice"); current.rename("spare", "bob"); },
                "vacated target" => { current.remove("bob"); current.rename("alice", "bob"); },
                "rejoin" => { current.remove("alice"); current.insert(member("alice", "first occupant")); },
                "self cycle" => { current.rename("me", "spare"); current.rename("bob", "me"); current.rename("spare", "bob"); },
                _ => unreachable!(),
            }
            let mut visible: HashMap<_, _> = original.snapshot().into_iter().map(|member| (member.nick.clone(), member)).collect();
            let mut own_nick = "me".to_string();
            for event in current.changes_since(&original, "room@example", "me") {
                match event {
                    XmppEvent::MemberLeft { nick, .. } => { visible.remove(&nick); },
                    XmppEvent::MemberNicknameChanged { old_nick, new_nick, is_self, .. } => {
                        if is_self { own_nick = new_nick.clone(); }
                        let mut member = visible.remove(&old_nick).expect("rename must address an existing occupant");
                        member.nick = new_nick.clone();
                        assert!(visible.insert(new_nick, member).is_none(), "{scenario}: rename merged distinct occupants");
                    }
                    XmppEvent::MemberJoined { member, .. } => { visible.insert(member.nick.clone(), member); },
                    other => panic!("unexpected reconciliation event: {other:?}"),
                }
            }
            assert_eq!(visible.len(), current.members.len());
            for expected in current.snapshot() {
                assert_eq!(visible[&expected.nick].status, expected.status, "{scenario}");
            }
            assert_eq!(own_nick, if scenario == "self cycle" { "bob" } else { "me" });
        }
    }

    #[test]
    fn transient_roster_churn_is_not_retained_or_replayed() {
        let mut original = Roster::default();
        original.insert(member("alice", "present"));
        let mut current = original.clone();
        for index in 0..1000 {
            let nick = format!("transient-{index}");
            current.insert(member(&nick, "temporary"));
            current.remove(&nick);
            current.rename("alice", "renamed");
            current.rename("renamed", "alice");
        }
        assert_eq!(current.members.len(), 1);
        assert_eq!(current.identities.len(), 1);
        assert_eq!(current.by_nick.len(), 1);
        assert!(current.changes_since(&original, "room@example", "me").is_empty());
    }
}

impl Roster
{
    pub fn insert(&mut self, mut member: RoomMember)
    {
        if let Some(&index) = self.by_nick.get(&member.nick) {
            // Missing identity metadata does not revoke a previously known JID.
            if member.jid.is_none() { member.jid = self.members[index].jid.take(); }
            self.members[index] = member;
        } else {
            self.by_nick.insert(member.nick.clone(), self.members.len());
            self.members.push(member);
            self.identities.push(self.next_identity);
            self.next_identity = self.next_identity.wrapping_add(1);
        }
    }

    pub fn rename(&mut self, old: &str, new: &str)
    {
        if old == new { return; }
        if let Some(&index) = self.by_nick.get(old) {
            self.members[index].nick = new.into();
            self.reindex();
        }
    }

    pub fn remove(&mut self, nick: &str)
    {
        if !self.by_nick.contains_key(nick) { return; }
        let mut index = 0;
        self.identities.retain(|_| {
            let keep = self.members[index].nick != nick;
            index += 1;
            keep
        });
        self.members.retain(|member| member.nick != nick);
        self.reindex();
    }

    fn reindex(&mut self)
    {
        self.by_nick.clear();
        for (index, member) in self.members.iter().enumerate() {
            // Preserve the prior first-match behavior if inconsistent service
            // data renames an occupant to a nickname already in the roster.
            self.by_nick.entry(member.nick.clone()).or_insert(index);
        }
    }

    #[cfg(test)]
    pub fn take(&mut self) -> Vec<RoomMember>
    {
        std::mem::take(self).members
    }

    pub fn snapshot(&self) -> Vec<RoomMember> { self.members.clone() }

    // Reconcile a retained membership after departure failed/cancelled without
    // fabricating another RoomJoined. Only the final state is replayed, so
    // churn during a slow write cannot create an unbounded event journal.
    pub fn changes_since(&self, previous: &Self, room: &str, old_own_nick: &str) -> Vec<XmppEvent>
    {
        let current: HashMap<_, _> = self.identities.iter().copied().zip(&self.members).collect();
        let old: HashMap<_, _> = previous.identities.iter().copied().zip(&previous.members).collect();
        // A target can still be occupied in the published snapshot even when
        // it is free now. Recreate those occupants after removing old names;
        // replaying a rename cycle directly would merge distinct occupants.
        let recreated: std::collections::HashSet<_> = previous.identities.iter().zip(&previous.members)
            .filter_map(|(identity, member)| {
                let next = current.get(identity)?;
                (member.nick != old_own_nick && member.nick != next.nick
                    && previous.by_nick.get(&next.nick).is_some_and(|index|
                        current.contains_key(&previous.identities[*index])))
                    .then_some(*identity)
            }).collect();
        let mut events = Vec::new();
        for (identity, member) in previous.identities.iter().zip(&previous.members) {
            if member.nick != old_own_nick && (!current.contains_key(identity) || recreated.contains(identity)) {
                events.push(XmppEvent::MemberLeft { room: room.into(), nick: member.nick.clone() });
            }
        }
        let mut own_rename = None;
        for (identity, member) in previous.identities.iter().zip(&previous.members) {
            if let Some(next) = current.get(identity).filter(|next| member.nick != next.nick && !recreated.contains(identity)) {
                let is_self = member.nick == old_own_nick;
                let event = XmppEvent::MemberNicknameChanged { room: room.into(), old_nick: member.nick.clone(), new_nick: next.nick.clone(), is_self };
                if is_self { own_rename = Some(event); } else { events.push(event); }
            }
        }
        events.extend(own_rename);
        for (identity, member) in self.identities.iter().zip(&self.members) {
            let changed = recreated.contains(identity) || old.get(identity).is_none_or(|old| old.jid != member.jid
                || old.affiliation != member.affiliation || old.role != member.role
                || old.show != member.show || old.status != member.status);
            if changed { events.push(XmppEvent::MemberJoined { room: room.into(), member: member.clone() }); }
        }
        events
    }

    #[cfg(test)]
    pub fn from_members(members: Vec<RoomMember>) -> Self
    {
        let mut roster = Self { identities: (0..members.len() as u64).collect(), next_identity: members.len() as u64, members, ..Self::default() };
        roster.reindex();
        roster
    }

    #[cfg(test)]
    pub fn as_slice(&self) -> &[RoomMember] { &self.members }
}
