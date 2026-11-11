use chrono::{ DateTime, Utc };

pub enum EventKind
{
    Joined,
    Left,
    StatusChanged(Option<String>),
}

// Incoming messages are Confirmed. Room sends use temporary Sending/Pending
// states until their echo. DMs carry the durable outbox ID through Queued,
// Submitted (server acknowledgement, not a read/delivery receipt), or Failed.
// Slow room sends reveal a badge only after a grace period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatStatus
{
    Confirmed,
    Unconfirmed,
    Queued(i64),
    Submitted(i64),
    Failed(i64),
    Sending(i64),
    Pending(i64),
}

impl From<crate::storage::history::DeliveryState> for ChatStatus {
    fn from(delivery: crate::storage::history::DeliveryState) -> Self {
        use crate::storage::history::DeliveryState;
        match delivery {
            DeliveryState::Received | DeliveryState::UnqueuedSubmitted => Self::Confirmed,
            DeliveryState::Unconfirmed => Self::Unconfirmed,
            DeliveryState::UnqueuedFailed => Self::Failed(0),
            DeliveryState::Queued(id) => Self::Queued(id),
            DeliveryState::Submitted(id) => Self::Submitted(id),
            DeliveryState::Failed(id) => Self::Failed(id),
        }
    }
}

pub enum Message
{
    Chat
    {
        // Row id in the persistent history DB. Stable identity used to highlight
        // a searched-for message and to track the loaded window's bounds.
        id: i64,
        // Empty identifies room-service text; never an occupant or own echo.
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

// Empty is reserved for room-service text in history. A valid occupant JID
// always has a nonempty nickname, so it cannot collide with this source key.
pub fn sender_label(sender: &str) -> &str
{
    if sender.is_empty() { "Room service" } else { sender }
}

pub fn mentions(body: &str, nick: &str) -> bool
{
    if nick.is_empty()
    {
        return false;
    }

    use caseless::Caseless;
    use unicode_normalization::UnicodeNormalization;
    // Canonical caseless matching is NFD(casefold(NFD(text))). These keys
    // are only for mentions; never rewrite displayed text or protocol JIDs.
    let key = |text: &str| if text.is_ascii() { text.to_ascii_lowercase() }
        else { text.chars().nfd().default_case_fold().nfd().collect::<String>() };
    let body_folded = key(body);
    let nick_folded = key(nick);

    use unicode_properties::{UnicodeGeneralCategory, GeneralCategory};
    let word_character = |ch: char| ch.is_alphanumeric()
        || matches!(ch.general_category(), GeneralCategory::NonspacingMark | GeneralCategory::SpacingMark
            | GeneralCategory::EnclosingMark | GeneralCategory::ConnectorPunctuation)
        || matches!(ch, '\u{200c}' | '\u{200d}');

    // Track overlapping matches without repeatedly rescanning long prefixes.
    // `match_indices` skips a later valid mention when an earlier overlapping
    // occurrence has invalid word boundaries (e.g. "a a" in "ba a a").
    let pattern = nick_folded.as_bytes();
    let mut fallback = vec![0; pattern.len()];
    for index in 1..pattern.len() {
        let mut matched = fallback[index - 1];
        while matched > 0 && pattern[index] != pattern[matched] { matched = fallback[matched - 1]; }
        if pattern[index] == pattern[matched] { matched += 1; }
        fallback[index] = matched;
    }
    let mut matched = 0;
    for (index, &byte) in body_folded.as_bytes().iter().enumerate() {
        while matched > 0 && byte != pattern[matched] { matched = fallback[matched - 1]; }
        if byte == pattern[matched] { matched += 1; }
        if matched == pattern.len() {
            let end = index + 1;
            let start = end - pattern.len();
            // Complete matches of a valid UTF-8 string start and end on
            // character boundaries; inspect whole adjacent scalars, not bytes.
            let before_ok = body_folded[..start].chars().next_back().is_none_or(|ch| !word_character(ch));
            let after_ok = body_folded[end..].chars().next().is_none_or(|ch| !word_character(ch));
            if before_ok && after_ok { return true; }
            matched = fallback[matched - 1];
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::mentions;

    #[test]
    fn mentions_recognize_canonical_spellings_and_full_case_equivalence() {
        for (body, nick, expected) in [
            ("hello e\u{301}VA!", "Éva", true),
            ("hello ÉVA!", "e\u{301}va", true),
            ("hello οσ!", "ΟΣ", true),
            ("hello ος!", "οσ", true),
            ("hello STRASSE!", "Straße", true),
            ("hello Straße!", "STRASSE", true),
            ("hello \u{fb03}!", "ffi", true),
            ("hello ffi!", "\u{fb03}", true),
            ("hello e\u{301}\u{323}!", "e\u{323}\u{301}", true),
            ("é", "e", false),
            ("ß", "s", false),
            ("STRASSEaway", "Straße", false),
            ("Ａ", "a", false),
            ("ı", "i", false),
            ("İ", "i", false),
            ("İ", "i\u{307}", true),
        ] {
            assert_eq!(mentions(body, nick), expected, "body={body:?}, nick={nick:?}");
        }
    }

    #[test]
    fn mention_boundaries_follow_unicode_words() {
        for (body, nick, expected) in [
            ("hello Bob!", "bob", true),
            ("@BOB: hello", "Bob", true),
            ("bobby", "bob", false),
            ("ébob", "bob", false),
            ("bob猫", "bob", false),
            ("bob١", "bob", false),
            ("bob_away", "bob", false),
            ("bob\u{203f}away", "bob", false),
            ("bob\u{301}", "bob", false),
            ("x\u{301}bob", "bob", false),
            ("bob\u{200d}x", "bob", false),
            ("hi, 猫!", "猫", true),
            ("野猫", "猫", false),
            ("hello ÉVA", "éva", true),
            ("bob-bob", "bob", true),
            ("hello 😀!", "😀", true),
            ("anything", "", false),
        ] {
            assert_eq!(mentions(body, nick), expected, "body={body:?}, nick={nick:?}");
        }
    }

    #[test]
    fn rejected_candidates_do_not_hide_later_overlapping_mentions() {
        assert!(mentions("ba a a", "a a"));
        assert!(mentions("xa-a-a!", "a-a"));
        assert!(mentions("x猫-猫-猫!", "猫-猫"));
        assert!(!mentions("ba a ab", "a a"));
        assert!(mentions("ordinary text then bob", "bob"));
        assert!(!mentions(&"a".repeat(100_000), &"a".repeat(1023)));
    }
}
