// Persistent, searchable chat-log storage backed by SQLite.
//
// Design notes
// ------------
// * One row per chat message, keyed by `conversation` (the bare JID of a room
//   or a 1:1 contact). Presence/join/leave events are deliberately NOT stored;
//   they are live session noise, not history worth searching or reloading.
// * The table is the "performant structure" the rest of the app pages against:
//   `idx_msg_conv_id` makes "the newest N messages of a conversation" and "the
//   N messages older than id X" both index-backed, O(log n) keyset lookups, so
//   the UI can extract any sub-window of an arbitrarily long log instantly
//   without ever holding the whole thing in memory.
// * Deduplication is needed because XMPP (without MAM/XEP-0359 stable ids here)
//   replays recent MUC history every time we (re)join a room. See `insert`.

use chrono::{ DateTime, TimeZone, Utc };
use rusqlite::{ params, Connection, OptionalExtension };

// A message we got LIVE was stored with our local clock (`Utc::now()`), because
// live stanzas carry no timestamp. When the server later replays that same
// message as history it arrives with its original server `<delay>` stamp, which
// differs from our local store time by network latency plus clock skew. When we
// see a delayed (history) copy we therefore treat any live-stored message with
// identical (sender, body) within this window as the same message and skip it.
// Restricting the fuzzy match to live-provenance rows keeps two genuinely
// distinct history messages (which carry exact, matching server stamps and so
// dedupe precisely) from ever being collapsed into one.
const LIVE_DEDUP_TOLERANCE_MS: i64 = 600_000; // 10 minutes

pub struct History
{
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct StoredMessage
{
    pub id: i64,
    pub sender: String,
    pub body: String,
    pub received: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct SearchHit
{
    pub id: i64,
    pub conversation: String,
}

fn to_dt(ms: i64) -> DateTime<Utc>
{
    return Utc.timestamp_millis_opt(ms).single().unwrap_or_else(Utc::now);
}

fn row_to_stored(row: &rusqlite::Row) -> rusqlite::Result<StoredMessage>
{
    return Ok(StoredMessage
    {
        id: row.get(0)?,
        sender: row.get(1)?,
        body: row.get(2)?,
        received: to_dt(row.get::<_, i64>(3)?),
    });
}

impl History
{
    pub fn open(path: &std::path::Path) -> Result<Self, String>
    {
        if let Some(parent) = path.parent()
        {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }

        let conn = Connection::open(path).map_err(|e| e.to_string())?;

        // WAL + NORMAL keeps writes cheap while staying crash-safe enough for a
        // chat log; we touch the DB synchronously from the UI loop so each op
        // must be fast.
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
            .map_err(|e| e.to_string())?;
        Self::apply_schema(&conn).map_err(|e| e.to_string())?;

        return Ok(Self { conn });
    }

    fn apply_schema(conn: &Connection) -> rusqlite::Result<()>
    {
        return conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 conversation  TEXT NOT NULL,
                 sender        TEXT NOT NULL,
                 body          TEXT NOT NULL,
                 ts            INTEGER NOT NULL,
                 live          INTEGER NOT NULL,
                 UNIQUE(conversation, sender, body, ts)
             );
             CREATE INDEX IF NOT EXISTS idx_msg_conv_ts ON messages(conversation, ts, id);",
        );
    }

    #[cfg(test)]
    fn open_in_memory() -> Self
    {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        Self::apply_schema(&conn).expect("schema");
        return Self { conn };
    }

    // Persist a chat message, deduplicating against prior copies. Returns the new
    // row id when the message was genuinely new (caller should display it), or
    // None when it was a duplicate that should be dropped.
    //
    // `delayed` is true when the stanza carried an XEP-0203 `<delay>` (i.e. it is
    // replayed history rather than a live message).
    pub fn insert(
        &self,
        conversation: &str,
        sender: &str,
        body: &str,
        received: DateTime<Utc>,
        delayed: bool,
    ) -> Option<i64>
    {
        let ts = received.timestamp_millis();

        // Fuzzy rule: a history replay of something we already stored live.
        if delayed
        {
            let exists: Option<i64> = self.conn.query_row(
                "SELECT id FROM messages
                 WHERE conversation = ?1 AND sender = ?2 AND body = ?3
                   AND live = 1 AND ts BETWEEN ?4 AND ?5
                 LIMIT 1",
                params![conversation, sender, body, ts - LIVE_DEDUP_TOLERANCE_MS, ts + LIVE_DEDUP_TOLERANCE_MS],
                |row| row.get(0),
            ).optional().unwrap_or(None);

            if exists.is_some()
            {
                return None;
            }
        }

        // Exact rule: the UNIQUE constraint collapses identical (conversation,
        // sender, body, ts) rows. History-vs-history replays share the same
        // server `<delay>` stamp, so they hit this and are ignored.
        let live = if delayed { 0 } else { 1 };
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO messages (conversation, sender, body, ts, live)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![conversation, sender, body, ts, live],
        ).unwrap_or(0);

        if changed == 0
        {
            return None;
        }

        return Some(self.conn.last_insert_rowid());
    }

    // Chronological ordering is by (ts, id): `ts` is the message time and `id`
    // breaks ties. We must NOT order by `id` alone, because messages back-filled
    // from the server archive (MAM) are chronologically older yet are inserted
    // later and so receive higher ids. `(ts, id)` keeps the archive interleaved
    // in the right place.

    // The most recent `limit` messages of a conversation, oldest-first (ready to
    // append straight into a render buffer).
    pub fn load_tail(&self, conversation: &str, limit: usize) -> Vec<StoredMessage>
    {
        return self.query_desc_then_reverse(
            "SELECT id, sender, body, ts FROM messages
             WHERE conversation = ?1
             ORDER BY ts DESC, id DESC LIMIT ?2",
            params![conversation, limit as i64],
        );
    }

    // The `limit` messages immediately older than the (ts, id) cursor, oldest-first.
    pub fn load_older(&self, conversation: &str, before_ts: i64, before_id: i64, limit: usize) -> Vec<StoredMessage>
    {
        return self.query_desc_then_reverse(
            "SELECT id, sender, body, ts FROM messages
             WHERE conversation = ?1 AND (ts < ?2 OR (ts = ?2 AND id < ?3))
             ORDER BY ts DESC, id DESC LIMIT ?4",
            params![conversation, before_ts, before_id, limit as i64],
        );
    }

    // A window centred on `center_id`: up to `before` older messages, the centre
    // message, and up to `after` newer ones — all oldest-first. Used to jump to a
    // search hit deep in the archive.
    pub fn load_window_around(
        &self,
        conversation: &str,
        center_id: i64,
        before: usize,
        after: usize,
    ) -> Vec<StoredMessage>
    {
        let Some(center_ts) = self.message_ts(center_id) else { return Vec::new(); };

        let mut out = self.load_older(conversation, center_ts, center_id, before);

        let newer = self.query_asc(
            "SELECT id, sender, body, ts FROM messages
             WHERE conversation = ?1 AND (ts > ?2 OR (ts = ?2 AND id >= ?3))
             ORDER BY ts ASC, id ASC LIMIT ?4",
            params![conversation, center_ts, center_id, (after + 1) as i64],
        );
        out.extend(newer);
        return out;
    }

    // Timestamp of the most recent stored message for a conversation, used as
    // the `start` bound when catching up missed history from the server.
    pub fn newest_received(&self, conversation: &str) -> Option<DateTime<Utc>>
    {
        return self.conn
            .query_row(
                "SELECT max(ts) FROM messages WHERE conversation = ?1",
                params![conversation],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()
            .ok()
            .flatten()
            .flatten()
            .map(to_dt);
    }

    fn message_ts(&self, id: i64) -> Option<i64>
    {
        return self.conn
            .query_row("SELECT ts FROM messages WHERE id = ?1", params![id], |r| r.get(0))
            .optional()
            .unwrap_or(None);
    }

    pub fn has_older(&self, conversation: &str, before_ts: i64, before_id: i64) -> bool
    {
        return self.exists(
            "SELECT 1 FROM messages
             WHERE conversation = ?1 AND (ts < ?2 OR (ts = ?2 AND id < ?3)) LIMIT 1",
            params![conversation, before_ts, before_id],
        );
    }

    pub fn has_newer(&self, conversation: &str, after_ts: i64, after_id: i64) -> bool
    {
        return self.exists(
            "SELECT 1 FROM messages
             WHERE conversation = ?1 AND (ts > ?2 OR (ts = ?2 AND id > ?3)) LIMIT 1",
            params![conversation, after_ts, after_id],
        );
    }

    // Substring search (case-insensitive over ASCII). When `conversation` is Some
    // the search is scoped to that room/DM, otherwise it spans every
    // conversation. Hits are returned oldest-first and capped to bound work.
    pub fn search(&self, conversation: Option<&str>, query: &str, limit: usize) -> Vec<SearchHit>
    {
        let needle = format!("%{}%", escape_like(query));

        let sql = if conversation.is_some()
        {
            "SELECT id, conversation FROM messages
             WHERE conversation = ?2 AND body LIKE ?1 ESCAPE '\\'
             ORDER BY ts ASC, id ASC LIMIT ?3"
        }
        else
        {
            "SELECT id, conversation FROM messages
             WHERE body LIKE ?1 ESCAPE '\\'
             ORDER BY ts ASC, id ASC LIMIT ?2"
        };

        let mut stmt = match self.conn.prepare(sql)
        {
            Ok(s) => s,
            Err(e) => { log::warn!("history search prepare failed: {}", e); return Vec::new(); }
        };

        let map = |row: &rusqlite::Row| -> rusqlite::Result<SearchHit>
        {
            Ok(SearchHit
            {
                id: row.get(0)?,
                conversation: row.get(1)?,
            })
        };

        let rows = if let Some(conv) = conversation
        {
            stmt.query_map(params![needle, conv, limit as i64], map)
        }
        else
        {
            stmt.query_map(params![needle, limit as i64], map)
        };

        return match rows
        {
            Ok(iter) => iter.filter_map(Result::ok).collect(),
            Err(e) => { log::warn!("history search failed: {}", e); Vec::new() }
        };
    }

    fn query_desc_then_reverse(&self, sql: &str, p: &[&dyn rusqlite::ToSql]) -> Vec<StoredMessage>
    {
        let mut v = self.query_asc(sql, p);
        v.reverse();
        return v;
    }

    fn query_asc(&self, sql: &str, p: &[&dyn rusqlite::ToSql]) -> Vec<StoredMessage>
    {
        let mut stmt = match self.conn.prepare(sql)
        {
            Ok(s) => s,
            Err(e) => { log::warn!("history query prepare failed: {}", e); return Vec::new(); }
        };

        return match stmt.query_map(p, row_to_stored)
        {
            Ok(iter) => iter.filter_map(Result::ok).collect(),
            Err(e) => { log::warn!("history query failed: {}", e); Vec::new() }
        };
    }

    fn exists(&self, sql: &str, p: &[&dyn rusqlite::ToSql]) -> bool
    {
        return self.conn.query_row(sql, p, |_| Ok(())).optional().unwrap_or(None).is_some();
    }
}

// Escape LIKE wildcards so a user searching for "100%" or "a_b" matches those
// literal characters rather than treating them as patterns.
fn escape_like(s: &str) -> String
{
    let mut out = String::with_capacity(s.len());
    for c in s.chars()
    {
        if c == '%' || c == '_' || c == '\\'
        {
            out.push('\\');
        }
        out.push(c);
    }
    return out;
}

#[cfg(test)]
mod tests
{
    use super::*;

    fn at(secs: i64) -> DateTime<Utc>
    {
        return Utc.timestamp_opt(1_700_000_000 + secs, 0).single().unwrap();
    }

    #[test]
    fn exact_duplicate_is_dropped()
    {
        let h = History::open_in_memory();
        assert!(h.insert("room", "alice", "hello", at(0), false).is_some());
        // Same conversation/sender/body/ts -> dropped.
        assert!(h.insert("room", "alice", "hello", at(0), false).is_none());
    }

    #[test]
    fn history_replay_with_same_stamp_dedupes()
    {
        let h = History::open_in_memory();
        // First join: history message with a server delay stamp.
        assert!(h.insert("room", "bob", "gm", at(100), true).is_some());
        // Rejoin replays the identical stamped message -> exact dedup.
        assert!(h.insert("room", "bob", "gm", at(100), true).is_none());
    }

    #[test]
    fn live_then_replayed_history_dedupes_within_tolerance()
    {
        let h = History::open_in_memory();
        // Stored live at our local clock.
        assert!(h.insert("room", "carol", "deploying now", at(500), false).is_some());
        // Server replays it after a reconnect with its (slightly different)
        // original stamp; fuzzy live-provenance rule collapses it.
        assert!(h.insert("room", "carol", "deploying now", at(503), true).is_none());
        // Outside the tolerance window it is treated as a separate message.
        assert!(h.insert("room", "carol", "deploying now", at(5000), true).is_some());
    }

    #[test]
    fn two_distinct_history_messages_same_text_both_kept()
    {
        let h = History::open_in_memory();
        // Two genuinely distinct history lines with identical text but different
        // (exact) stamps must both survive — the fuzzy rule only matches
        // live-provenance rows, so it never collapses these.
        assert!(h.insert("room", "dave", "+1", at(10), true).is_some());
        assert!(h.insert("room", "dave", "+1", at(11), true).is_some());
        assert_eq!(h.load_tail("room", 10).len(), 2);
    }

    #[test]
    fn repeated_live_messages_are_kept()
    {
        let h = History::open_in_memory();
        assert!(h.insert("dm", "me", "ok", at(0), false).is_some());
        assert!(h.insert("dm", "me", "ok", at(30), false).is_some());
        assert_eq!(h.load_tail("dm", 10).len(), 2);
    }

    #[test]
    fn tail_and_paging_walk_the_whole_log()
    {
        let h = History::open_in_memory();
        for i in 0..10
        {
            h.insert("room", "u", &format!("m{}", i), at(i), false);
        }

        let tail = h.load_tail("room", 3);
        assert_eq!(tail.iter().map(|m| m.body.clone()).collect::<Vec<_>>(), vec!["m7", "m8", "m9"]);
        let cur = &tail[0];
        assert!(h.has_older("room", cur.received.timestamp_millis(), cur.id));

        let older = h.load_older("room", cur.received.timestamp_millis(), cur.id, 3);
        assert_eq!(older.iter().map(|m| m.body.clone()).collect::<Vec<_>>(), vec!["m4", "m5", "m6"]);
        let last = &tail[2];
        assert!(!h.has_newer("room", last.received.timestamp_millis(), last.id));
    }

    #[test]
    fn archive_backfill_orders_by_time_not_id()
    {
        let h = History::open_in_memory();
        // Live messages arrive in time order (ids 1,2,3 == times 100,101,102).
        h.insert("room", "u", "live-a", at(100), false);
        h.insert("room", "u", "live-b", at(101), false);
        h.insert("room", "u", "live-c", at(102), false);

        // Now back-fill OLDER history from the server archive: inserted later, so
        // these get higher ids (4,5) despite older timestamps.
        let old1 = h.insert("room", "u", "old-1", at(50), true).unwrap();
        let _old2 = h.insert("room", "u", "old-2", at(51), true).unwrap();
        assert!(old1 > 3, "backfilled rows get higher ids");

        // The tail must still be the newest BY TIME, not by id.
        let tail = h.load_tail("room", 3);
        assert_eq!(
            tail.iter().map(|m| m.body.clone()).collect::<Vec<_>>(),
            vec!["live-a", "live-b", "live-c"],
        );

        // Paging older than the tail front yields the backfilled history in
        // chronological order.
        let front = &tail[0];
        let older = h.load_older("room", front.received.timestamp_millis(), front.id, 10);
        assert_eq!(
            older.iter().map(|m| m.body.clone()).collect::<Vec<_>>(),
            vec!["old-1", "old-2"],
        );
        assert!(!h.has_older("room", older[0].received.timestamp_millis(), older[0].id));
    }

    #[test]
    fn window_around_centres_on_a_hit()
    {
        let h = History::open_in_memory();
        let mut ids = Vec::new();
        for i in 0..20
        {
            ids.push(h.insert("room", "u", &format!("m{}", i), at(i), false).unwrap());
        }

        let window = h.load_window_around("room", ids[10], 3, 3);
        let bodies: Vec<_> = window.iter().map(|m| m.body.clone()).collect();
        assert_eq!(bodies, vec!["m7", "m8", "m9", "m10", "m11", "m12", "m13"]);
    }

    #[test]
    fn search_scoped_and_global()
    {
        let h = History::open_in_memory();
        h.insert("roomA", "u", "the deploy is green", at(0), false);
        h.insert("roomA", "u", "lunch?", at(1), false);
        h.insert("roomB", "u", "deploy rollback done", at(2), false);

        let scoped = h.search(Some("roomA"), "deploy", 100);
        assert_eq!(scoped.len(), 1);

        let global = h.search(None, "deploy", 100);
        assert_eq!(global.len(), 2);

        // Wildcards are treated literally.
        h.insert("roomA", "u", "100% sure", at(3), false);
        assert_eq!(h.search(Some("roomA"), "100%", 100).len(), 1);
        assert_eq!(h.search(Some("roomA"), "0% s", 100).len(), 1);
    }
}
