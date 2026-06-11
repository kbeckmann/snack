use chrono::{ DateTime, Utc };

pub enum EventKind
{
    Joined,
    Left,
    StatusChanged(Option<String>),
}

// Delivery state of a chat message. Most messages are `Confirmed` (received from
// the server or loaded from history). Our own room messages are shown optimistically
// the instant they're sent and only grow a status badge if the server echo is slow:
//   Sending  – just sent, within a short grace period; rendered with no badge, so a
//              normal (fast) round-trip never flickers an indicator.
//   Pending  – grace elapsed without an echo; rendered with a "sending…" badge.
//   Failed   – no echo arrived before the timeout; rendered with a "failed" badge.
//              A late echo still upgrades it back to Confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatStatus
{
    Confirmed,
    Sending,
    Pending,
    Failed,
}

pub enum Message
{
    Chat
    {
        // Row id in the persistent history DB. Stable identity used to highlight
        // a searched-for message and to track the loaded window's bounds. While a
        // message is `Sending`/`Failed` it has no DB row yet and carries a negative
        // temporary id instead.
        id: i64,
        from: String,
        body: String,
        received: DateTime<Utc>,
        status: ChatStatus,
    },
    Event
    {
        kind: EventKind,
        nick: String,
        received: DateTime<Utc>,
    },
}

pub fn mentions(body: &str, nick: &str) -> bool
{
    if nick.is_empty()
    {
        return false;
    }

    let body_lower = body.to_lowercase();
    let nick_lower = nick.to_lowercase();

    return body_lower.match_indices(&nick_lower).any(|(start, matched)|
    {
        let end = start + matched.len();
        let before_ok = start == 0
            || !body_lower.as_bytes()[start - 1].is_ascii_alphanumeric();
        let after_ok = end == body_lower.len()
            || !body_lower.as_bytes()[end].is_ascii_alphanumeric();
        return before_ok && after_ok;
    });
}
