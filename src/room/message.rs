use chrono::{ DateTime, Utc };

pub enum EventKind
{
    Joined,
    Left,
    StatusChanged(Option<String>),
}

pub enum Message
{
    Chat
    {
        // Row id in the persistent history DB. Stable identity used to highlight
        // a searched-for message and to track the loaded window's bounds.
        id: i64,
        from: String,
        body: String,
        received: DateTime<Utc>,
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
