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
