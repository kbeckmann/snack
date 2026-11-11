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
// * Deduplication is needed because the same message reaches us through several
//   paths: live delivery, MUC history replay on (re)join, and the MAM archive
//   sweep. The primary key for this is the XEP-0359 stanza id, which is stable
//   and identical across all those copies; a fuzzy content+timestamp rule
//   remains as a fallback for servers (or rows) that lack one. See `insert`.

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
const CONTENT_INDEX_SQL: &str = "CREATE UNIQUE INDEX idx_msg_content ON messages(conversation, sender, body, ts) WHERE live = 0 AND stanza_id IS NULL AND outbox_id IS NULL AND unqueued_origin IS NULL";

pub struct History
{
    conn: Connection,
}

#[derive(Debug, Clone, Default)]
pub struct ArchiveSync {
    pub received: Option<DateTime<Utc>>,
    pub before: Option<String>,
    pub newest: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryState { Received, Queued(i64), Submitted(i64), Failed(i64), Unconfirmed, UnqueuedSubmitted, UnqueuedFailed }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub enum UnqueuedDelivery { Unconfirmed = 1, Submitted = 2, Failed = 3 }

// Keep delivery state tied to the exact outbox identity, including errors that
// arrive after acknowledgement and explicit discard. Incoming/legacy rows have
// no link and retain their original received status.
const MESSAGE_SELECT: &str = "SELECT id, sender, body, ts, messages.outbox_id,
    CASE WHEN messages.outbox_id IS NULL THEN CASE WHEN unqueued_origin IS NULL THEN 0 ELSE CASE unqueued_state WHEN 2 THEN 5 WHEN 3 THEN 6 ELSE 4 END END
      WHEN EXISTS (SELECT 1 FROM outgoing_messages o WHERE o.outbox_id = messages.outbox_id AND o.error IS NOT NULL) THEN 3
      WHEN EXISTS (SELECT 1 FROM outbox q WHERE q.id = messages.outbox_id) THEN 1
      WHEN EXISTS (SELECT 1 FROM outgoing_messages o WHERE o.outbox_id = messages.outbox_id) THEN 2
      ELSE 3 END FROM messages";

const UNQUEUED_SELECT: &str = "SELECT id, sender, body, ts, NULL,
    CASE unqueued_state WHEN 2 THEN 5 WHEN 3 THEN 6 ELSE 4 END, conversation FROM messages";

#[derive(Debug, Clone)]
pub struct StoredMessage
{
    pub delivery: DeliveryState,
    pub id: i64,
    // Empty is the reserved source key for room-service text, not an occupant.
    pub sender: String,
    pub body: String,
    pub received: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct SearchHit
{
    pub id: i64,
    pub conversation: String,
    pub received: DateTime<Utc>,
}

// A queued outgoing message awaiting delivery confirmation. Persisted so a send
// survives reconnects and app restarts; removed only once acknowledged.
#[derive(Debug, Clone)]
pub struct OutboxItem
{
    pub id: i64,
    pub origin_id: String,
    pub conversation: String,
    pub is_room: bool,
    pub body: String,
}

impl OutboxItem
{
    pub fn accepts_error_from(&self, from: Option<&str>, own_jid: &str) -> bool
    {
        let bare = |jid: &str| jid.split('/').next().unwrap_or(jid).to_string();
        let own = bare(own_jid);
        let domain = |jid: &str| jid.split('@').next_back().unwrap_or(jid).to_string();
        let target_bare = bare(&self.conversation);
        // Only the authenticated server can omit 'from' or use our bare
        // account JID; it may reject traffic on the account's behalf.
        let Some(from) = from else { return true; };
        if from.is_empty() { return false; }
        if ::xmpp::jid_matches(from, &own) { return true; }
        if ::xmpp::jid_matches(from, &domain(&own)) || ::xmpp::jid_matches(from, &domain(&target_bare)) { return true; }
        if self.is_room || self.conversation.contains('/') { ::xmpp::jid_matches(from, &self.conversation) }
        else { ::xmpp::jid_matches(&bare(from), &self.conversation) }
    }
}

fn to_dt(ms: i64, column: usize) -> rusqlite::Result<DateTime<Utc>>
{
    return Utc.timestamp_millis_opt(ms).single()
        .ok_or(rusqlite::Error::IntegralValueOutOfRange(column, ms));
}

fn row_to_stored(row: &rusqlite::Row) -> rusqlite::Result<StoredMessage>
{
    let outbox_id: Option<i64> = row.get(4)?;
    let delivery = match (outbox_id, row.get::<_, i64>(5)?) {
        (Some(id), 1) => DeliveryState::Queued(id),
        (Some(id), 2) => DeliveryState::Submitted(id),
        (Some(id), 3) => DeliveryState::Failed(id),
        (None, 4) => DeliveryState::Unconfirmed,
        (None, 5) => DeliveryState::UnqueuedSubmitted,
        (None, 6) => DeliveryState::UnqueuedFailed,
        _ => DeliveryState::Received,
    };
    return Ok(StoredMessage
    {
        delivery,
        id: row.get(0)?,
        sender: row.get(1)?,
        body: row.get(2)?,
        received: to_dt(row.get::<_, i64>(3)?, 3)?,
    });
}

fn row_to_outbox(row: &rusqlite::Row) -> rusqlite::Result<OutboxItem>
{
    Ok(OutboxItem { id: row.get(0)?, conversation: row.get(1)?, is_room: row.get::<_, i64>(2)? != 0,
        body: row.get(3)?, origin_id: row.get(4)? })
}

#[cfg(unix)]
fn protect_history_files(path: &std::path::Path) -> std::io::Result<()>
{
    use std::fs::{self, OpenOptions};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::os::unix::ffi::OsStrExt;
    // Precreate the main file with 0600 before SQLite can create a journal.
    // SQLite's Unix VFS derives new journal/WAL permissions from the database.
    match OpenOptions::new().write(true).create_new(true).mode(0o600).open(path) {
        Ok(_) => {},
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {},
        Err(e) => return Err(e),
    }
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut filename = path.as_os_str().to_os_string();
        filename.push(suffix);
        let candidate = std::path::PathBuf::from(filename);
        // Older releases allowed near-limit database filenames. A journal
        // suffix can exceed the component limit even when its WAL fits; such
        // a journal cannot exist and must not prevent opening the existing DB.
        if !suffix.is_empty() && candidate.file_name().is_some_and(|name| name.as_bytes().len() > 255) { continue; }
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) => {
                if !metadata.is_file() {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "history path must be a regular file"));
                }
                if let Err(e) = fs::set_permissions(candidate, fs::Permissions::from_mode(0o600)) {
                    // Another SQLite connection may remove its sidecars on close.
                    if suffix.is_empty() || e.kind() != std::io::ErrorKind::NotFound { return Err(e); }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !suffix.is_empty() => {},
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

impl History
{
    pub fn open(path: &std::path::Path) -> Result<Self, String>
    {
        if let Some(parent) = path.parent()
        {
            super::create_directory(parent).map_err(|e| e.to_string())?;
        }

        // Resolve directory aliases such as macOS /var -> /private/var while
        // leaving the database filename unresolved for SQLite's NOFOLLOW check.
        let resolved;
        let path = if let (Some(parent), Some(filename)) = (path.parent(), path.file_name()) {
            let parent = if parent.as_os_str().is_empty() { std::path::Path::new(".") } else { parent };
            resolved = parent.canonicalize().map_err(|e| e.to_string())?.join(filename);
            resolved.as_path()
        } else { path };
        #[cfg(unix)]
        protect_history_files(path).map_err(|e| e.to_string())?;
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::default() | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW)
            .map_err(|e| e.to_string())?;

        // This database also owns the outgoing queue. A successful commit must
        // request durable storage before the composer is cleared or a send is
        // attempted; WAL/NORMAL can roll back commits after power loss.
        #[cfg(target_os = "macos")]
        conn.execute_batch("PRAGMA fullfsync = ON;").map_err(|e| e.to_string())?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;")
            .map_err(|e| e.to_string())?;
        Self::apply_schema(&conn).map_err(|e| e.to_string())?;

        return Ok(Self { conn });
    }

    fn has_legacy_content_constraint(conn: &Connection) -> rusqlite::Result<bool>
    {
        // Inspect actual table constraints, not their SQL spelling or text
        // inside a default value. Modern partial dedup indexes are separate.
        let indexes = conn.prepare(r#"SELECT name FROM pragma_index_list('messages')
            WHERE "unique" = 1 AND origin = 'u' AND partial = 0"#)?
            .query_map([], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for index in indexes {
            let columns = conn.prepare("SELECT name FROM pragma_index_info(?1)")?
                .query_map([index], |row| row.get::<_, Option<String>>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            if columns.len() == 4 && ["conversation", "sender", "body", "ts"].iter().all(|expected|
                columns.iter().any(|column| column.as_deref().is_some_and(|name| name.eq_ignore_ascii_case(expected)))) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn apply_schema(conn: &Connection) -> rusqlite::Result<()>
    {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 conversation  TEXT NOT NULL,
                 sender        TEXT NOT NULL,
                 body          TEXT NOT NULL,
                 ts            INTEGER NOT NULL,
                 live          INTEGER NOT NULL,
                 stanza_id     TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_msg_conv_ts ON messages(conversation, ts, id);
             CREATE TABLE IF NOT EXISTS outbox (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 conversation  TEXT NOT NULL,
                 is_room       INTEGER NOT NULL,
                 body          TEXT NOT NULL,
                 created_ts    INTEGER NOT NULL
             );",
        )?;

        // Assign each pending send a durable wire identity. Migration is atomic
        // and only fills missing values, so reopening never changes a retry ID.
        {
            let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
            let columns = tx.prepare("PRAGMA table_info(outbox)")?
                .query_map([], |row| row.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
            if !columns.iter().any(|column| column.eq_ignore_ascii_case("origin_id")) {
                tx.execute_batch("ALTER TABLE outbox ADD COLUMN origin_id TEXT;")?;
            }
            // Whether a send may have reached the server. Unknown (existing rows,
            // or rows an older version inserts) counts as attempted.
            if !columns.iter().any(|column| column.eq_ignore_ascii_case("attempted")) {
                tx.execute_batch("ALTER TABLE outbox ADD COLUMN attempted INTEGER NOT NULL DEFAULT 1;")?;
            }
            tx.execute_batch("CREATE TABLE IF NOT EXISTS outgoing_messages (
                origin_id TEXT PRIMARY KEY NOT NULL, outbox_id INTEGER NOT NULL UNIQUE,
                conversation TEXT NOT NULL, is_room INTEGER NOT NULL, body TEXT NOT NULL,
                created_ts INTEGER NOT NULL, error TEXT);")?;
            let context_columns = tx.prepare("PRAGMA table_info(outgoing_messages)")?
                .query_map([], |row| row.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
            if !context_columns.iter().any(|name| name.eq_ignore_ascii_case("history_sender")) {
                tx.execute_batch("ALTER TABLE outgoing_messages ADD COLUMN history_sender TEXT;")?;
            }
            Self::fill_outbox_origins(&tx)?;
            tx.execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS idx_outbox_origin ON outbox(origin_id);")?;
            tx.commit()?;
        }

        // Migrate databases created before the stanza_id column existed.
        let has_stanza_id = conn
            .prepare("PRAGMA table_info(messages)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .filter_map(Result::ok)
            .any(|name| name.eq_ignore_ascii_case("stanza_id"));
        if !has_stanza_id
        {
            conn.execute_batch("ALTER TABLE messages ADD COLUMN stanza_id TEXT;")?;
        }

        // Older schemas treated identical content/timestamps as globally unique,
        // even when two distinct server stanza IDs proved they were different.
        {
            let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
            if Self::has_legacy_content_constraint(&tx)? {
                let columns = tx.prepare("PRAGMA table_info(messages)")?
                    .query_map([], |row| row.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
                let outbox = if columns.iter().any(|name| name.eq_ignore_ascii_case("outbox_id")) { "outbox_id" } else { "NULL" };
                let origin = if columns.iter().any(|name| name.eq_ignore_ascii_case("unqueued_origin")) { "unqueued_origin" } else { "NULL" };
                let state = if columns.iter().any(|name| name.eq_ignore_ascii_case("unqueued_state")) { "unqueued_state" } else { "0" };
                // Rebuilding the table must preserve delivery identities and the
                // sequence even when its highest allocated row was deleted.
                let sequence = tx.query_row("SELECT seq FROM sqlite_sequence WHERE name = 'messages' COLLATE NOCASE", [], |row| row.get::<_, i64>(0))
                    .optional()?.unwrap_or(0);
                tx.execute_batch(&format!("CREATE TABLE messages_new (
                        id INTEGER PRIMARY KEY AUTOINCREMENT, conversation TEXT NOT NULL,
                        sender TEXT NOT NULL, body TEXT NOT NULL, ts INTEGER NOT NULL,
                        live INTEGER NOT NULL, stanza_id TEXT, outbox_id INTEGER,
                        unqueued_origin TEXT, unqueued_state INTEGER NOT NULL DEFAULT 0);
                    INSERT INTO messages_new SELECT id, conversation, sender, body, ts, live, stanza_id, {outbox}, {origin}, {state} FROM messages;
                    DROP TABLE messages;
                    ALTER TABLE messages_new RENAME TO messages;"))?;
                tx.execute("UPDATE sqlite_sequence SET seq = max(seq, ?1) WHERE name = 'messages' COLLATE NOCASE", [sequence])?;
                tx.execute("INSERT INTO sqlite_sequence (name, seq) SELECT 'messages', ?1
                    WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'messages' COLLATE NOCASE)", [sequence])?;
            }
            tx.commit()?;
        }
        let has_outbox_id = conn.prepare("PRAGMA table_info(messages)")?
            .query_map([], |row| row.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?
            .iter().any(|name| name.eq_ignore_ascii_case("outbox_id"));
        if !has_outbox_id { conn.execute_batch("ALTER TABLE messages ADD COLUMN outbox_id INTEGER;")?; }
        let message_columns = conn.prepare("PRAGMA table_info(messages)")?
            .query_map([], |row| row.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
        if !message_columns.iter().any(|name| name.eq_ignore_ascii_case("unqueued_origin")) {
            conn.execute_batch("ALTER TABLE messages ADD COLUMN unqueued_origin TEXT;")?;
        }
        if !message_columns.iter().any(|name| name.eq_ignore_ascii_case("unqueued_state")) {
            conn.execute_batch("ALTER TABLE messages ADD COLUMN unqueued_state INTEGER NOT NULL DEFAULT 0;")?;
        }
        // This app-owned index has a fixed definition. Comparing the whole
        // definition avoids accepting a legacy predicate that only mentions a
        // new column in a comment or in the wrong condition. Replace atomically.
        {
            let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
            let index_sql: Option<String> = tx.query_row("SELECT sql FROM sqlite_master WHERE name = 'idx_msg_content' COLLATE NOCASE", [], |row| row.get(0)).optional()?;
            if index_sql.as_deref() != Some(CONTENT_INDEX_SQL) {
                tx.execute_batch("DROP INDEX IF EXISTS idx_msg_content;")?;
                tx.execute(CONTENT_INDEX_SQL, [])?;
            }
            tx.commit()?;
        }
        conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_msg_conv_ts ON messages(conversation, ts, id);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_msg_unqueued_origin ON messages(unqueued_origin) WHERE unqueued_origin IS NOT NULL;
            CREATE UNIQUE INDEX IF NOT EXISTS idx_msg_outbox ON messages(outbox_id) WHERE outbox_id IS NOT NULL;")?;

        // XEP-0359 dedup key: unique per conversation, but only where present.
        // Older rows and stanza-id-less servers leave it NULL, and SQLite treats
        // NULLs as distinct, so a partial index lets those coexist freely.
        conn.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_msg_stanza
                 ON messages(conversation, stanza_id) WHERE stanza_id IS NOT NULL;",
        )?;

        conn.execute_batch("CREATE TABLE IF NOT EXISTS message_stanza_aliases (
            conversation TEXT NOT NULL, stanza_id TEXT NOT NULL, message_id INTEGER NOT NULL,
            PRIMARY KEY(conversation, stanza_id));
            CREATE INDEX IF NOT EXISTS idx_msg_alias_target ON message_stanza_aliases(message_id);
            CREATE TABLE IF NOT EXISTS message_receipts (
                receipt_id TEXT PRIMARY KEY NOT NULL, message_id INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS idx_msg_receipt_target ON message_receipts(message_id);")?;

        // Bootstrap once from existing server-dated history, never our receipt
        // clock. Reopening must not mistake a partially saved new page for a
        // completed sync or advance the boundary past an unfinished gap.
        let tx = conn.unchecked_transaction()?;
        let existed: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='archive_sync')", [], |row| row.get(0))?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS archive_sync (
            conversation TEXT PRIMARY KEY NOT NULL, received INTEGER, before_id TEXT, newest INTEGER);")?;
        if !existed {
            tx.execute_batch("INSERT INTO archive_sync(conversation, received)
                SELECT conversation, max(CASE WHEN live = 0 THEN ts END) FROM messages GROUP BY conversation;")?;
        }
        tx.commit()?;

        return Ok(());
    }

    // A legacy database is claimed once; another account must use its own DB.
    pub fn claim_account(&self, account: &str) -> Result<bool, String>
    {
        self.conn.execute_batch("CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
            .map_err(|e| e.to_string())?;
        self.conn.execute("INSERT OR IGNORE INTO metadata (key, value) VALUES ('account', ?1)", params![account])
            .map_err(|e| e.to_string())?;
        let owner: String = self.conn.query_row("SELECT value FROM metadata WHERE key = 'account'", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        Ok(owner == account)
    }

    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Self
    {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        Self::apply_schema(&conn).expect("schema");
        return Self { conn };
    }

    #[cfg(test)]
    pub(crate) fn disable_writes(&self)
    {
        self.conn.execute_batch("PRAGMA query_only = ON").expect("read-only test database");
    }

    #[cfg(test)]
    pub(crate) fn enable_writes(&self)
    {
        self.conn.execute_batch("PRAGMA query_only = OFF").expect("writable test database");
    }

    #[cfg(test)]
    pub(crate) fn disable_reads(&self)
    {
        self.conn.execute_batch("ALTER TABLE messages RENAME TO unreadable_messages").expect("hide test history");
    }

    #[cfg(test)]
    pub(crate) fn enable_reads(&self)
    {
        self.conn.execute_batch("ALTER TABLE unreadable_messages RENAME TO messages").expect("restore test history");
    }

    #[cfg(test)]
    pub(crate) fn disable_outbox_reads(&self)
    {
        self.conn.execute_batch("ALTER TABLE outbox RENAME TO unreadable_outbox").expect("hide test outbox");
    }

    #[cfg(test)]
    pub(crate) fn enable_outbox_reads(&self)
    {
        self.conn.execute_batch("ALTER TABLE unreadable_outbox RENAME TO outbox").expect("restore test outbox");
    }

    #[cfg(test)]
    pub(crate) fn disable_outgoing_reads(&self)
    {
        self.conn.execute_batch("ALTER TABLE outgoing_messages RENAME TO unreadable_outgoing").unwrap();
    }

    #[cfg(test)]
    pub(crate) fn enable_outgoing_reads(&self)
    {
        self.conn.execute_batch("ALTER TABLE unreadable_outgoing RENAME TO outgoing_messages").unwrap();
    }

    // ----- Outbox: persistent retry queue for outgoing messages -----------

    // Queue an outgoing message for delivery; returns its outbox row id (used as
    // the negative temp id of the optimistic on-screen entry and as the send
    // command's correlation id). None only if the DB write fails.
    #[cfg(test)]
    pub fn enqueue_outbox(&self, conversation: &str, is_room: bool, body: &str) -> Option<i64>
    {
        self.enqueue_outbox_message(conversation, is_room, body).map(|item| item.id)
    }

    fn fill_outbox_origins(conn: &Connection) -> rusqlite::Result<()>
    {
        let missing = conn.prepare("SELECT id FROM outbox WHERE origin_id IS NULL OR origin_id = ''")?
            .query_map([], |row| row.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for id in missing {
            conn.execute("UPDATE outbox SET origin_id = ?1 WHERE id = ?2 AND (origin_id IS NULL OR origin_id = '')",
                params![uuid::Uuid::new_v4().to_string(), id])?;
        }
        let missing_context = conn.prepare("SELECT q.id FROM outbox q LEFT JOIN outgoing_messages o ON o.origin_id = q.origin_id WHERE o.origin_id IS NULL")?
            .query_map([], |row| row.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for id in missing_context {
            conn.execute("INSERT OR IGNORE INTO outgoing_messages (origin_id, outbox_id, conversation, is_room, body, created_ts)
                SELECT origin_id, id, conversation, is_room, body, created_ts FROM outbox WHERE id = ?1", params![id])?;
        }
        Ok(())
    }

    pub fn enqueue_outbox_message(&self, conversation: &str, is_room: bool, body: &str) -> Option<OutboxItem>
    {
        let tx = self.conn.unchecked_transaction().ok()?;
        let item = Self::enqueue_message(&tx, conversation, is_room, body, None).ok()?;
        tx.commit().ok()?;
        Some(item)
    }

    fn enqueue_message(conn: &Connection, conversation: &str, is_room: bool, body: &str, history_sender: Option<&str>) -> rusqlite::Result<OutboxItem>
    {
        let origin_id = uuid::Uuid::new_v4().to_string();
        conn.execute("INSERT INTO outbox (conversation, is_room, body, created_ts, origin_id, attempted) VALUES (?1, ?2, ?3, ?4, ?5, 0)",
            params![conversation, is_room as i64, body, Utc::now().timestamp_millis(), origin_id])?;
        let id = conn.last_insert_rowid();
        conn.execute("INSERT INTO outgoing_messages (origin_id, outbox_id, conversation, is_room, body, created_ts)
            SELECT origin_id, id, conversation, is_room, body, created_ts FROM outbox WHERE id = ?1", params![id])?;
        conn.execute("UPDATE outgoing_messages SET history_sender = ?1 WHERE outbox_id = ?2", params![history_sender, id])?;
        Ok(OutboxItem { id, origin_id, conversation: conversation.into(), is_room, body: body.into() })
    }

    pub fn enqueue_direct_message(&self, conversation: &str, sender: &str, body: &str) -> rusqlite::Result<(OutboxItem, StoredMessage)>
    {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        let item = Self::enqueue_message(&tx, conversation, false, body, Some(sender))?;
        tx.execute("INSERT INTO messages (conversation, sender, body, ts, live, outbox_id)
            SELECT conversation, ?1, body, created_ts, 1, id FROM outbox WHERE id = ?2", params![sender, item.id])?;
        let id = tx.last_insert_rowid();
        let stored = tx.query_row(&format!("{MESSAGE_SELECT} WHERE id = ?1"), params![id], row_to_stored)?;
        tx.commit()?;
        Ok((item, stored))
    }

    // Queue the recovery metadata even when writing the history table fails.
    // Legacy unlinked messages have no sender marker and are never guessed at.
    pub fn enqueue_direct_retry(&self, conversation: &str, sender: &str, body: &str) -> rusqlite::Result<OutboxItem>
    {
        let tx = self.conn.unchecked_transaction()?;
        let item = Self::enqueue_message(&tx, conversation, false, body, Some(sender))?;
        tx.commit()?;
        Ok(item)
    }

    // Recover a missing history copy without changing the durable wire identity.
    // The queue can remain available while the messages table temporarily fails.
    pub fn store_direct_message(&self, conversation: &str, sender: &str, body: &str,
        received: DateTime<Utc>, outbox_id: i64) -> rusqlite::Result<StoredMessage>
    {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("INSERT OR IGNORE INTO messages (conversation, sender, body, ts, live, outbox_id) VALUES (?1, ?2, ?3,
            COALESCE((SELECT created_ts FROM outgoing_messages WHERE outbox_id = ?5), ?4), 1, ?5)",
            params![conversation, sender, body, received.timestamp_millis(), outbox_id])?;
        let stored = tx.query_row(&format!("{MESSAGE_SELECT} WHERE outbox_id = ?1"), params![outbox_id], row_to_stored)?;
        tx.commit()?;
        Ok(stored)
    }

    pub fn store_unqueued_direct(&self, origin: &str, conversation: &str, sender: &str,
        body: &str, received: DateTime<Utc>, state: UnqueuedDelivery) -> rusqlite::Result<StoredMessage>
    {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("INSERT OR IGNORE INTO messages (conversation, sender, body, ts, live, unqueued_origin, unqueued_state)
            VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)", params![conversation, sender, body, received.timestamp_millis(), origin, state as i64])?;
        // A routing rejection is terminal even if an earlier transport result is late.
        tx.execute("UPDATE messages SET unqueued_state = ?1 WHERE unqueued_origin = ?2 AND unqueued_state != 3 AND (?1 != 1 OR unqueued_state = 1)", params![state as i64, origin])?;
        let stored = tx.query_row(&format!("{UNQUEUED_SELECT} WHERE unqueued_origin = ?1"), params![origin], row_to_stored)?;
        tx.commit()?;
        Ok(stored)
    }

    pub fn unqueued_direct(&self, origin: &str) -> rusqlite::Result<Option<(String, StoredMessage)>>
    {
        self.conn.query_row(&format!("{UNQUEUED_SELECT} WHERE unqueued_origin = ?1"), params![origin], |row| {
            let stored = row_to_stored(row)?;
            let conversation = row.get(6)?;
            Ok((conversation, stored))
        }).optional()
    }

    // All queued messages, oldest-first, for replay on (re)connect / launch.
    pub fn load_outbox(&self) -> rusqlite::Result<Vec<OutboxItem>>
    {
        // An older running version may still insert rows without this column.
        // Persist an identity before such a row can be sent by this version.
        Self::fill_outbox_origins(&self.conn)?;
        let mut stmt = self.conn.prepare("SELECT q.id, q.conversation, q.is_room, q.body, q.origin_id FROM outbox q
            JOIN outgoing_messages o ON o.origin_id = q.origin_id WHERE o.error IS NULL ORDER BY q.id ASC")?;
        let rows = stmt.query_map([], row_to_outbox);
        return rows?.collect();
    }

    // A detached retry may reopen a store after a file was moved or removed.
    // An empty database cannot prove that a known send was confirmed/discarded.
    pub fn has_outbox_context(&self, id: i64) -> rusqlite::Result<bool>
    {
        self.conn.query_row("SELECT EXISTS(SELECT 1 FROM outgoing_messages WHERE outbox_id = ?1 UNION ALL SELECT 1 FROM outbox WHERE id = ?1)",
            params![id], |row| row.get(0))
    }

    // Keep origin/context after transport acknowledgement: a routing error can
    // arrive later. Preserve context separately so older app versions still
    // see only pending messages in the original outbox table.
    pub fn confirm_outbox(&self, id: i64) -> rusqlite::Result<()>
    {
        let tx = self.conn.unchecked_transaction()?;
        Self::fill_outbox_origins(&tx)?;
        let needs_history: bool = tx.query_row("SELECT EXISTS (SELECT 1 FROM outgoing_messages WHERE outbox_id = ?1 AND history_sender IS NOT NULL)", params![id], |row| row.get(0))?;
        if needs_history {
            // Retire the queue only together with a durable local history copy.
            // This also recovers a DM after a restart lost its in-memory retry.
            tx.execute("INSERT OR IGNORE INTO messages (conversation, sender, body, ts, live, outbox_id)
                SELECT conversation, history_sender, body, created_ts, 1, outbox_id FROM outgoing_messages WHERE outbox_id = ?1", params![id])?;
        }
        tx.execute("DELETE FROM outbox WHERE id = ?1", params![id])?;
        tx.commit()
    }

    // Record before handing a send to the connection that it may be delivered.
    pub fn mark_outbox_attempted(&self, id: i64) -> rusqlite::Result<()>
    {
        self.conn.execute("UPDATE outbox SET attempted = 1 WHERE id = ?1 AND attempted = 0", params![id]).map(|_| ())
    }

    // Queued sends for this conversation that may already have been delivered.
    pub fn attempted_queued(&self, conversation: &str, is_room: bool) -> rusqlite::Result<Vec<i64>>
    {
        self.conn.prepare("SELECT id FROM outbox WHERE conversation = ?1 AND is_room = ?2 AND attempted != 0")?
            .query_map(params![conversation, is_room as i64], |row| row.get(0))?.collect()
    }

    // When the oldest send still queued for this conversation was created.
    pub fn oldest_queued(&self, conversation: &str, is_room: bool) -> rusqlite::Result<Option<DateTime<Utc>>>
    {
        let ts: Option<i64> = self.conn.query_row("SELECT min(created_ts) FROM outbox WHERE conversation = ?1 AND is_room = ?2",
            params![conversation, is_room as i64], |row| row.get(0))?;
        Ok(ts.and_then(DateTime::from_timestamp_millis))
    }

    // Whether a room send with this origin is still awaiting confirmation.
    // Its origin ID is unguessable before delivery, so a copy of it in the
    // room's history proves an unacknowledged attempt reached the room.
    pub fn has_queued_room_origin(&self, room: &str, body: &str, origin: &str) -> rusqlite::Result<bool>
    {
        self.conn.query_row("SELECT EXISTS(SELECT 1 FROM outbox WHERE origin_id = ?1 AND conversation = ?2 AND body = ?3 AND is_room = 1)",
            params![origin, room, body], |row| row.get(0))
    }

    pub fn outgoing_by_origin(&self, origin: &str) -> rusqlite::Result<Option<OutboxItem>>
    {
        self.conn.query_row("SELECT outbox_id, conversation, is_room, body, origin_id FROM outgoing_messages WHERE origin_id = ?1",
            params![origin], row_to_outbox).optional()
    }

    pub fn reject_outbox(&self, id: i64, reason: &str) -> rusqlite::Result<()>
    {
        if self.conn.execute("UPDATE outgoing_messages SET error = ?1 WHERE outbox_id = ?2", params![reason, id])? == 0 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        Ok(())
    }

    pub fn load_rejected_outbox(&self) -> rusqlite::Result<Vec<(OutboxItem, String)>>
    {
        self.conn.prepare("SELECT outbox_id, conversation, is_room, body, origin_id, error FROM outgoing_messages WHERE error IS NOT NULL ORDER BY outbox_id ASC")?
            .query_map([], |row| Ok((row_to_outbox(row)?, row.get(5)?)))?.collect()
    }

    // Drop a queued message once it has been acknowledged.
    pub fn remove_outbox(&self, id: i64) -> rusqlite::Result<()>
    {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM outbox WHERE id = ?1", params![id])?;
        tx.execute("DELETE FROM outgoing_messages WHERE outbox_id = ?1", params![id])?;
        tx.commit()
    }

    // Persist a chat message, deduplicating against prior copies. Returns the new
    // row id when the message was genuinely new (caller should display it), or
    // None when it was a duplicate that should be dropped. Storage errors are
    // returned separately so the caller never discards a message as a duplicate.
    //
    // `delayed` is true when `received` came from a validated server delay stamp.
    // Replays with invalid stamps retain receipt-time provenance. `stanza_id` is the XEP-0359
    // stable id when the server supplied one.
    pub fn insert(
        &self,
        conversation: &str,
        sender: &str,
        body: &str,
        received: DateTime<Utc>,
        delayed: bool,
        stanza_id: Option<&str>,
    ) -> rusqlite::Result<Option<i64>>
    {
        self.store_message(conversation, sender, body, received, delayed, stanza_id, true, None)
            .map(|(stored, inserted)| inserted.then_some(stored.id))
    }

    // A local receipt distinguishes separate arrivals even when their text and
    // millisecond timestamp match. Reuse it only when retrying this arrival.
    pub fn store_received(&self, receipt: uuid::Uuid, conversation: &str, sender: &str, body: &str,
        received: DateTime<Utc>, delayed: bool, stanza_id: Option<&str>, allow_outgoing_fuzzy: bool)
        -> rusqlite::Result<(StoredMessage, bool)>
    {
        self.store_message(conversation, sender, body, received, delayed, stanza_id, allow_outgoing_fuzzy, Some(&receipt.to_string()))
    }

    pub fn store_with_policy(&self, conversation: &str, sender: &str, body: &str,
        received: DateTime<Utc>, delayed: bool, stanza_id: Option<&str>, allow_outgoing_fuzzy: bool) -> rusqlite::Result<StoredMessage>
    {
        self.store_message(conversation, sender, body, received, delayed, stanza_id, allow_outgoing_fuzzy, None).map(|(stored, _)| stored)
    }

    fn store_message(
        &self, conversation: &str, sender: &str, body: &str,
        received: DateTime<Utc>, delayed: bool, stanza_id: Option<&str>, allow_outgoing_fuzzy: bool, receipt: Option<&str>,
    ) -> rusqlite::Result<(StoredMessage, bool)>
    {
        // Take the write reservation before reading candidates. Otherwise two
        // connections can both select the same live twin, and the later update
        // silently discards a distinct archived message after the first claims it.
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        let prior = if let Some(receipt) = receipt {
            match tx.query_row("SELECT r.message_id, m.conversation FROM message_receipts r JOIN messages m ON m.id = r.message_id
                WHERE receipt_id = ?1", [receipt], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))).optional()? {
                Some((id, owner)) if owner == conversation => Some(id),
                Some(_) => return Err(rusqlite::Error::InvalidQuery),
                None => None,
            }
        } else if !delayed && stanza_id.is_none() {
            // Legacy import/retry helpers have no local receipt. Keep their
            // exact-time fallback; fresh UI arrivals use store_received.
            tx.query_row("SELECT id FROM messages WHERE conversation=?1 AND sender=?2 AND body=?3 AND ts=?4
                AND stanza_id IS NULL AND outbox_id IS NULL AND unqueued_origin IS NULL ORDER BY id LIMIT 1",
                params![conversation, sender, body, received.timestamp_millis()], |row| row.get::<_, i64>(0)).optional()?
        } else { None };
        let (id, inserted) = if let Some(id) = prior { (id, false) }
            else { Self::insert_with_id(&tx, conversation, sender, body, received, delayed, stanza_id, allow_outgoing_fuzzy)? };
        if let Some(receipt) = receipt.filter(|_| prior.is_none()) {
            tx.execute("INSERT INTO message_receipts (receipt_id, message_id) VALUES (?1, ?2)", params![receipt, id])?;
        }
        let stored = tx.query_row(&format!("{MESSAGE_SELECT} WHERE id = ?1"), params![id], row_to_stored)?;
        tx.commit()?;
        Ok((stored, inserted))
    }

    fn insert_with_id(
        conn: &Connection,
        conversation: &str,
        sender: &str,
        body: &str,
        received: DateTime<Utc>,
        delayed: bool,
        stanza_id: Option<&str>,
        allow_outgoing_fuzzy: bool,
    ) -> rusqlite::Result<(i64, bool)>
    {
        let ts = received.timestamp_millis();
        let live = if delayed { 0 } else { 1 };

        // Primary rule: the XEP-0359 stanza id is identical on every copy of a
        // message (live delivery, MUC history replay, MAM archive), so it dedups
        // exactly — even when a message delivered late on reconnect was stored
        // with our local clock while its archive copy carries the real send time,
        // a gap far wider than the fuzzy window below could absorb.
        if let Some(sid) = stanza_id
        {
            let existing: Option<(i64, i64, bool)> = conn.query_row(
                "SELECT id, live, (outbox_id IS NOT NULL OR unqueued_origin IS NOT NULL) FROM messages WHERE conversation = ?1 AND stanza_id = ?2
                 UNION SELECT m.id, m.live, (m.outbox_id IS NOT NULL OR m.unqueued_origin IS NOT NULL) FROM message_stanza_aliases a JOIN messages m ON m.id = a.message_id
                 WHERE a.conversation = ?1 AND a.stanza_id = ?2 LIMIT 1",
                params![conversation, sid],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).optional()?;

            if let Some((existing_id, existing_live, outgoing)) = existing
            {
                // The stored copy was live (our local clock) and this is the
                // authoritative archived copy: adopt its real timestamp so the
                // message settles into its true place in the log.
                if delayed && existing_live == 1
                {
                    conn.execute(
                        "UPDATE OR IGNORE messages SET ts = ?1, live = 0 WHERE id = ?2",
                        params![ts, existing_id],
                    )?;
                }
                else if delayed && outgoing {
                    conn.execute("UPDATE messages SET ts = min(ts, ?1) WHERE id = ?2", params![ts, existing_id])?;
                }
                return Ok((existing_id, false));
            }
        }

        // Fallback rule: a delayed history/archive replay of something we already
        // stored live without a matching stanza id (rows predating XEP-0359
        // support, or our own messages persisted before the server echoed them).
        // Restricting it to live-provenance rows keeps two genuinely distinct
        // history lines with identical text from ever collapsing.
        if delayed
        {
            let exact: Option<i64> = conn.query_row(
                "SELECT id FROM messages WHERE conversation = ?1 AND sender = ?2 AND body = ?3 AND ts = ?4
                 AND (?5 IS NULL OR stanza_id IS NULL OR stanza_id = ?5)
                 AND (?5 IS NULL OR NOT EXISTS (SELECT 1 FROM message_stanza_aliases WHERE message_id = messages.id))
                 AND (?6 OR (outbox_id IS NULL AND unqueued_origin IS NULL)) ORDER BY live, id LIMIT 1",
                params![conversation, sender, body, ts, stanza_id, allow_outgoing_fuzzy], |r| r.get(0),
            ).optional()?;
            if let Some(id) = exact {
                conn.execute("UPDATE OR IGNORE messages SET live = 0, stanza_id = COALESCE(stanza_id, ?1) WHERE id = ?2",
                    params![stanza_id, id])?;
                return Ok((id, false));
            }
            // Only match a twin that has NO stanza id of its own. If this message
            // carries a stanza id and an otherwise-identical live row carries a
            // *different* one, they are genuinely distinct messages (e.g. a bot
            // firing the same line twice) and must both be kept — the exact
            // stanza-id rule above already handled the true-duplicate case.
            let twin: Option<i64> = conn.query_row(
                "SELECT id FROM messages
                 WHERE conversation = ?1 AND sender = ?2 AND body = ?3
                   AND live = 1 AND stanza_id IS NULL AND ts BETWEEN ?4 AND ?5
                   AND NOT EXISTS (SELECT 1 FROM message_stanza_aliases WHERE message_id = messages.id)
                   AND (?7 OR (outbox_id IS NULL AND unqueued_origin IS NULL))
                 ORDER BY abs(ts - ?6), id LIMIT 1",
                params![conversation, sender, body, ts - LIVE_DEDUP_TOLERANCE_MS, ts + LIVE_DEDUP_TOLERANCE_MS, ts, allow_outgoing_fuzzy],
                |row| row.get(0),
            ).optional()?;

            if let Some(twin_id) = twin
            {
                // Backfill the stanza id onto the live twin so any further copy
                // dedups exactly, and adopt the authoritative archive timestamp.
                conn.execute(
                    "UPDATE OR IGNORE messages SET ts = ?1, live = 0, stanza_id = COALESCE(stanza_id, ?2) WHERE id = ?3",
                    params![ts, stanza_id, twin_id],
                )?;
                return Ok((twin_id, false));
            }
        }

        // Content/time uniqueness only backs up archived rows without IDs.
        // Separate live receipts can share a timestamp and must remain distinct.
        let changed = conn.execute(
            "INSERT OR IGNORE INTO messages (conversation, sender, body, ts, live, stanza_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![conversation, sender, body, ts, live, stanza_id],
        )?;

        if changed == 0
        {
            let id = if let Some(sid) = stanza_id {
                conn.query_row("SELECT id FROM messages WHERE conversation = ?1 AND stanza_id = ?2",
                    params![conversation, sid], |row| row.get(0))?
            } else {
                conn.query_row("SELECT id FROM messages WHERE conversation = ?1 AND sender = ?2 AND body = ?3 AND ts = ?4
                    AND stanza_id IS NULL AND outbox_id IS NULL AND unqueued_origin IS NULL",
                    params![conversation, sender, body, ts], |row| row.get(0))?
            };
            return Ok((id, false));
        }

        return Ok((conn.last_insert_rowid(), true));
    }

    // Retry callers need the canonical row even if another delivery has already
    // stored it. Return its ID and authoritative timestamp to replace temp IDs.
    pub fn store(
        &self, conversation: &str, sender: &str, body: &str,
        received: DateTime<Utc>, delayed: bool, stanza_id: Option<&str>,
    ) -> rusqlite::Result<StoredMessage>
    {
        self.store_message(conversation, sender, body, received, delayed, stanza_id, true, None).map(|(stored, _)| stored)
    }

    // Caller validates its own sender and conversation. Live self-chat echoes
    // retain local timestamps/live status; archive copies carry server time.
    pub fn reconcile_own_direct_copy(&self, conversation: &str, body: &str, origin: &str,
        received: DateTime<Utc>, stanza_id: Option<&str>, delayed: bool) -> rusqlite::Result<Option<(StoredMessage, Vec<i64>)>>
    {
        self.reconcile_known_origin(conversation, body, origin, received, stanza_id, None, delayed)
            .map(|found| found.map(|(stored, merged, _)| (stored, merged)))
    }

    // A current live room echo is authority; an old nickname in history is not.
    pub fn reconcile_own_room_origin(&self, room: &str, nick: &str, body: &str, origin: &str,
        received: DateTime<Utc>, stanza_id: Option<&str>) -> rusqlite::Result<Option<(StoredMessage, Vec<i64>, bool)>>
    {
        self.reconcile_known_origin(room, body, origin, received, stanza_id, Some(nick), false)
    }

    fn reconcile_known_origin(&self, conversation: &str, body: &str, origin: &str,
        received: DateTime<Utc>, stanza_id: Option<&str>, room_nick: Option<&str>, delayed: bool)
        -> rusqlite::Result<Option<(StoredMessage, Vec<i64>, bool)>>
    {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        let mut created = false;
        if let Some(nick) = room_nick {
            // The caller verified a live echo from its current room identity.
            created = tx.execute("INSERT OR IGNORE INTO messages (conversation, sender, body, ts, live, outbox_id)
                SELECT conversation, ?4, body, ?5, 1, outbox_id FROM outgoing_messages
                WHERE origin_id = ?1 AND conversation = ?2 AND body = ?3 AND is_room = 1",
                params![origin, conversation, body, nick, received.timestamp_millis()])? > 0;
        } else {
            // A prior history outage may have left only the durable DM retry context.
            tx.execute("INSERT OR IGNORE INTO messages (conversation, sender, body, ts, live, outbox_id)
                SELECT conversation, history_sender, body, created_ts, 1, outbox_id FROM outgoing_messages
                WHERE origin_id = ?1 AND conversation = ?2 AND body = ?3 AND is_room = 0 AND history_sender IS NOT NULL",
                params![origin, conversation, body])?;
        }
        let target: Option<(i64, String, Option<i64>)> = tx.query_row("SELECT m.id, m.sender, m.outbox_id FROM messages m
            LEFT JOIN outgoing_messages o ON o.outbox_id = m.outbox_id
            WHERE m.conversation = ?1 AND m.body = ?2 AND ((?4 = 0 AND m.unqueued_origin = ?3) OR (o.origin_id = ?3 AND o.is_room = ?4))",
            params![conversation, body, origin, room_nick.is_some()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional()?;
        let Some((id, sender, outbox_id)) = target else { return Ok(None); };
        let mut merged = Vec::new();
        let mut server_ts = received.timestamp_millis();
        if let Some(sid) = stanza_id {
            let existing = tx.prepare("SELECT id FROM messages WHERE conversation = ?1 AND stanza_id = ?2
                UNION SELECT a.message_id FROM message_stanza_aliases a JOIN messages m ON m.id = a.message_id
                WHERE a.conversation = ?1 AND a.stanza_id = ?2")?
                .query_map(params![conversation, sid], |row| row.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            for other in existing.into_iter().filter(|other| *other != id) {
                let (duplicate, live, timestamp): (bool, bool, i64) = tx.query_row("SELECT (sender = ?1 OR sender = ?4) AND body = ?2 AND outbox_id IS NULL AND unqueued_origin IS NULL, live, ts
                    FROM messages WHERE id = ?3", params![sender, body, other, room_nick.unwrap_or(&sender)], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
                if !duplicate { return Err(rusqlite::Error::InvalidQuery); }
                if delayed && !live { server_ts = server_ts.min(timestamp); }
                tx.execute("UPDATE message_stanza_aliases SET message_id = ?1 WHERE message_id = ?2", params![id, other])?;
                tx.execute("INSERT OR IGNORE INTO message_stanza_aliases (conversation, stanza_id, message_id)
                    SELECT conversation, stanza_id, ?1 FROM messages WHERE id = ?2 AND stanza_id IS NOT NULL", params![id, other])?;
                tx.execute("UPDATE message_receipts SET message_id = ?1 WHERE message_id = ?2", params![id, other])?;
                tx.execute("DELETE FROM messages WHERE id = ?1", params![other])?;
                merged.push(other);
            }
            tx.execute("INSERT INTO message_stanza_aliases (conversation, stanza_id, message_id) VALUES (?1, ?2, ?3)
                ON CONFLICT(conversation, stanza_id) DO UPDATE SET message_id = excluded.message_id", params![conversation, sid, id])?;
        }
        // Adopt the first authoritative stamp, then keep the earliest server
        // copy when retransmissions arrive out of order. Local clock skew is irrelevant.
        tx.execute("UPDATE messages SET ts = CASE WHEN ?3 = 0 THEN ts WHEN live = 1 THEN ?1 ELSE min(ts, ?1) END,
            live = CASE WHEN ?3 THEN 0 ELSE live END,
            unqueued_state = CASE WHEN unqueued_origin IS NOT NULL AND unqueued_state != 3 THEN 2 ELSE unqueued_state END WHERE id = ?2",
            params![server_ts, id, delayed])?;
        if let Some(outbox_id) = outbox_id { tx.execute("DELETE FROM outbox WHERE id = ?1", params![outbox_id])?; }
        let stored = tx.query_row(&format!("{MESSAGE_SELECT} WHERE id = ?1"), params![id], row_to_stored)?;
        tx.commit()?;
        Ok(Some((stored, merged, created)))
    }

    // Chronological ordering is by (ts, id): `ts` is the message time and `id`
    // breaks ties. We must NOT order by `id` alone, because messages back-filled
    // from the server archive (MAM) are chronologically older yet are inserted
    // later and so receive higher ids. `(ts, id)` keeps the archive interleaved
    // in the right place.

    // The most recent `limit` messages of a conversation, oldest-first (ready to
    // append straight into a render buffer).
    pub fn load_tail(&self, conversation: &str, limit: usize) -> rusqlite::Result<Vec<StoredMessage>>
    {
        return self.query_desc_then_reverse(
            &format!("{MESSAGE_SELECT}
             WHERE conversation = ?1
             ORDER BY ts DESC, id DESC LIMIT ?2"),
            params![conversation, limit as i64],
        );
    }

    // The `limit` messages immediately older than the (ts, id) cursor, oldest-first.
    pub fn load_older(&self, conversation: &str, before_ts: i64, before_id: i64, limit: usize) -> rusqlite::Result<Vec<StoredMessage>>
    {
        return self.query_desc_then_reverse(
            &format!("{MESSAGE_SELECT}
             WHERE conversation = ?1 AND (ts < ?2 OR (ts = ?2 AND id < ?3))
             ORDER BY ts DESC, id DESC LIMIT ?4"),
            params![conversation, before_ts, before_id, limit as i64],
        );
    }

    // Timestamp corrections can move an already-rendered row past a frozen
    // paging frontier. Skip those identities in SQL rather than consuming the
    // page budget (and UI updates) on duplicate-only pages.
    pub fn load_unloaded_older(&self, conversation: &str, cursor: Option<(i64, i64)>, loaded: &[i64], limit: usize)
        -> rusqlite::Result<Vec<StoredMessage>>
    {
        let filter = Self::unloaded_filter(cursor, loaded);
        self.query_desc_then_reverse(&format!("{MESSAGE_SELECT}{filter} ORDER BY ts DESC, id DESC LIMIT ?4"),
            params![conversation, cursor.map(|key| key.0), cursor.map(|key| key.1), limit as i64])
    }

    pub fn has_unloaded_older(&self, conversation: &str, cursor: Option<(i64, i64)>, loaded: &[i64]) -> rusqlite::Result<bool>
    {
        let filter = Self::unloaded_filter(cursor, loaded);
        self.exists(&format!("SELECT 1 FROM messages{filter} LIMIT ?4"),
            params![conversation, cursor.map(|key| key.0), cursor.map(|key| key.1), 1])
    }

    fn unloaded_filter(cursor: Option<(i64, i64)>, loaded: &[i64]) -> String
    {
        let mut filter = String::from(" WHERE conversation = ?1");
        if cursor.is_some() { filter.push_str(" AND (ts < ?2 OR (ts = ?2 AND id < ?3))"); }
        if !loaded.is_empty() {
            // These are typed integer IDs, never user text. Numeric literals
            // avoid SQLite's bind-variable limit for a full rendered window.
            let ids = loaded.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
            filter.push_str(&format!(" AND id NOT IN ({ids})"));
        }
        filter
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
    ) -> rusqlite::Result<Vec<StoredMessage>>
    {
        let Some(center_ts) = self.message_ts(conversation, center_id)? else { return Ok(Vec::new()); };

        let mut out = self.load_older(conversation, center_ts, center_id, before)?;

        let newer = self.query_asc(
            &format!("{MESSAGE_SELECT}
             WHERE conversation = ?1 AND (ts > ?2 OR (ts = ?2 AND id >= ?3))
             ORDER BY ts ASC, id ASC LIMIT ?4"),
            params![conversation, center_ts, center_id, (after + 1) as i64],
        )?;
        out.extend(newer);
        return Ok(out);
    }

    // Only delayed/archive provenance can bound a server-time query. A live
    // row's timestamp is our receipt clock and may be arbitrarily skewed.
    pub fn archived_received(&self, conversation: &str, id: i64) -> rusqlite::Result<Option<DateTime<Utc>>>
    {
        self.conn.query_row("SELECT ts FROM messages WHERE conversation = ?1 AND id = ?2 AND live = 0",
            params![conversation, id], |row| row.get::<_, i64>(0)).optional()?
            .map(|ts| to_dt(ts, 0)).transpose()
    }

    pub fn archive_sync(&self, conversation: &str) -> rusqlite::Result<ArchiveSync> {
        self.conn.query_row("SELECT received, before_id, newest FROM archive_sync WHERE conversation = ?1", [conversation], |row| {
            let received: Option<i64> = row.get(0)?;
            let newest: Option<i64> = row.get(2)?;
            Ok(ArchiveSync { received: received.map(|ts| to_dt(ts, 0)).transpose()?, before: row.get(1)?,
                newest: newest.map(|ts| to_dt(ts, 2)).transpose()? })
        }).optional().map(|state| state.unwrap_or_default())
    }

    pub fn save_archive_sync(&self, conversation: &str, state: &ArchiveSync) -> rusqlite::Result<()> {
        self.conn.execute("INSERT INTO archive_sync(conversation, received, before_id, newest) VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(conversation) DO UPDATE SET received=excluded.received, before_id=excluded.before_id, newest=excluded.newest",
            params![conversation, state.received.map(|stamp| stamp.timestamp_millis()), state.before,
                state.newest.map(|stamp| stamp.timestamp_millis())])?;
        Ok(())
    }

    // Timestamp of the most recent stored message. A local receipt clock is
    // not a server sync boundary; archive_sync owns that checkpoint separately.
    pub fn newest_received(&self, conversation: &str) -> rusqlite::Result<Option<DateTime<Utc>>>
    {
        return self.conn
            .query_row(
                "SELECT max(ts) FROM messages WHERE conversation = ?1",
                params![conversation],
                |r| r.get::<_, Option<i64>>(0),
            )
            .and_then(|ts| ts.map(|ts| to_dt(ts, 0)).transpose());
    }

    fn message_ts(&self, conversation: &str, id: i64) -> rusqlite::Result<Option<i64>>
    {
        return self.conn
            .query_row("SELECT ts FROM messages WHERE conversation = ?1 AND id = ?2", params![conversation, id], |r| r.get(0))
            .optional();
    }

    pub fn has_older(&self, conversation: &str, before_ts: i64, before_id: i64) -> rusqlite::Result<bool>
    {
        return self.exists(
            "SELECT 1 FROM messages
             WHERE conversation = ?1 AND (ts < ?2 OR (ts = ?2 AND id < ?3)) LIMIT 1",
            params![conversation, before_ts, before_id],
        );
    }

    pub fn has_newer(&self, conversation: &str, after_ts: i64, after_id: i64) -> rusqlite::Result<bool>
    {
        return self.exists(
            "SELECT 1 FROM messages
             WHERE conversation = ?1 AND (ts > ?2 OR (ts = ?2 AND id > ?3)) LIMIT 1",
            params![conversation, after_ts, after_id],
        );
    }

    // Substring search (case-insensitive over ASCII). Keep the most recent
    // `limit` hits, returned oldest-first so find starts at the latest match.
    pub fn search(&self, conversation: Option<&str>, query: &str, limit: usize) -> rusqlite::Result<Vec<SearchHit>>
    {
        let needle = format!("%{}%", escape_like(query));
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        if let Some(conv) = conversation {
            self.query_search(
                "SELECT id, conversation, ts FROM messages
                 WHERE conversation = ?2 AND body LIKE ?1 ESCAPE '\\'
                 ORDER BY ts DESC, id DESC LIMIT ?3", params![needle, conv, limit])
        } else {
            self.query_search(
                "SELECT id, conversation, ts FROM messages
                 WHERE body LIKE ?1 ESCAPE '\\'
                 ORDER BY ts DESC, id DESC LIMIT ?2", params![needle, limit])
        }
    }

    // Apply the open-conversation scope before LIMIT. Filtering a capped
    // global result afterwards lets closed conversations crowd out real hits.
    pub fn search_conversations(&self, conversations: &[&str], query: &str, limit: usize) -> rusqlite::Result<Vec<SearchHit>>
    {
        if conversations.is_empty() { return Ok(Vec::new()); }
        let placeholders = vec!["?"; conversations.len()].join(",");
        let sql = format!("SELECT id, conversation, ts FROM messages
            WHERE conversation IN ({}) AND body LIKE ? ESCAPE '\\'
            ORDER BY ts DESC, id DESC LIMIT ?", placeholders);
        let needle = format!("%{}%", escape_like(query));
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut parameters: Vec<&dyn rusqlite::ToSql> = conversations.iter().map(|jid| jid as &dyn rusqlite::ToSql).collect();
        parameters.push(&needle);
        parameters.push(&limit);
        self.query_search(&sql, &parameters)
    }

    fn query_search(&self, sql: &str, parameters: &[&dyn rusqlite::ToSql]) -> rusqlite::Result<Vec<SearchHit>>
    {
        let mut stmt = self.conn.prepare(sql)?;
        let mut hits = stmt.query_map(parameters, |row| Ok(SearchHit {
            id: row.get(0)?, conversation: row.get(1)?, received: to_dt(row.get(2)?, 2)?,
        }))?.collect::<rusqlite::Result<Vec<_>>>()?;
        hits.reverse();
        Ok(hits)
    }

    fn query_desc_then_reverse(&self, sql: &str, p: &[&dyn rusqlite::ToSql]) -> rusqlite::Result<Vec<StoredMessage>>
    {
        let mut v = self.query_asc(sql, p)?;
        v.reverse();
        return Ok(v);
    }

    fn query_asc(&self, sql: &str, p: &[&dyn rusqlite::ToSql]) -> rusqlite::Result<Vec<StoredMessage>>
    {
        let mut stmt = self.conn.prepare(sql)?;
        return stmt.query_map(p, row_to_stored)?.collect();
    }

    fn exists(&self, sql: &str, p: &[&dyn rusqlite::ToSql]) -> rusqlite::Result<bool>
    {
        return self.conn.query_row(sql, p, |_| Ok(())).optional().map(|row| row.is_some());
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
    fn local_receipts_are_distinct_atomic_and_survive_reopening()
    {
        let directory = std::env::temp_dir().join(format!("snack-receipts-{}", uuid::Uuid::new_v4()));
        let path = directory.join("history.sqlite");
        let h = History::open(&path).unwrap();
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        let c = uuid::Uuid::new_v4();
        let one = h.store_received(a, "alice", "alice", "same", at(0), false, None, false).unwrap();
        let two = h.store_received(b, "alice", "alice", "same", at(0), false, None, false).unwrap();
        assert!(one.1 && two.1);
        assert_ne!(one.0.id, two.0.id);
        h.conn.execute_batch("CREATE TRIGGER fail_receipt BEFORE INSERT ON message_receipts BEGIN SELECT RAISE(ABORT, 'receipt unavailable'); END;").unwrap();
        assert!(h.store_received(c, "alice", "alice", "same", at(0), false, None, false).is_err());
        assert_eq!(h.load_tail("alice", 10).unwrap().len(), 2);
        let replay = h.store_received(a, "alice", "alice", "same", at(0), false, None, false).unwrap();
        assert_eq!(replay.0.id, one.0.id);
        assert!(!replay.1);
        h.conn.execute_batch("DROP TRIGGER fail_receipt;").unwrap();
        let three = h.store_received(c, "alice", "alice", "same", at(0), false, None, false).unwrap();
        assert!(three.1);
        assert!(h.store_received(a, "mallory", "alice", "same", at(0), false, None, false).is_err());
        assert!(h.load_tail("mallory", 10).unwrap().is_empty());
        drop(h);
        let h = History::open(&path).unwrap();
        for (receipt, id) in [(a, one.0.id), (b, two.0.id), (c, three.0.id)] {
            let (stored, inserted) = h.store_received(receipt, "alice", "alice", "same", at(0), false, None, false).unwrap();
            assert_eq!(stored.id, id);
            assert!(!inserted);
        }
        assert_eq!(h.load_tail("alice", 10).unwrap().len(), 3);
        drop(h);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn receipt_retries_follow_canonical_origin_merges()
    {
        let h = History::open_in_memory();
        let (outgoing, original) = h.enqueue_direct_message("alice", "me", "same").unwrap();
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        let first = h.store_received(a, "alice", "me", "same", original.received, false, Some("server"), false).unwrap();
        let second = h.store_received(b, "alice", "me", "same", original.received, false, Some("server"), false).unwrap();
        assert_eq!(first.0.id, second.0.id);
        assert!(first.1 && !second.1);
        let (canonical, merged) = h.reconcile_own_direct_copy("alice", "same", &outgoing.origin_id, at(1), Some("server"), false).unwrap().unwrap();
        assert_eq!(canonical.id, original.id);
        assert_eq!(merged, [first.0.id]);
        for receipt in [a, b] {
            let (stored, inserted) = h.store_received(receipt, "alice", "me", "same", original.received, false, Some("server"), false).unwrap();
            assert_eq!(stored.id, original.id);
            assert_eq!(stored.delivery, DeliveryState::Submitted(outgoing.id));
            assert!(!inserted);
        }
        assert_eq!(h.load_tail("alice", 10).unwrap().len(), 1);
    }

    #[test]
    fn content_index_upgrade_is_atomic_and_idempotent()
    {
        for predicate in [
            "stanza_id IS NULL AND outbox_id IS NULL AND unqueued_origin IS NULL",
            "stanza_id IS NULL /* unqueued_origin */ AND outbox_id IS NULL",
            "stanza_id IS NULL AND outbox_id IS NULL AND unqueued_origin IS NOT NULL",
        ] {
            let h = History::open_in_memory();
            h.conn.execute_batch(&format!("DROP INDEX idx_msg_content; CREATE UNIQUE INDEX idx_msg_content ON messages(conversation, sender, body, ts) WHERE {predicate};")).unwrap();
            History::apply_schema(&h.conn).unwrap();
            let one = h.store_received(uuid::Uuid::new_v4(), "r", "u", "same", at(0), false, None, false).unwrap();
            let two = h.store_received(uuid::Uuid::new_v4(), "r", "u", "same", at(0), false, None, false).unwrap();
            assert_ne!(one.0.id, two.0.id);
            assert!(h.insert("r", "u", "history", at(1), true, None).unwrap().is_some());
            assert!(h.insert("r", "u", "history", at(1), true, None).unwrap().is_none());
            let version = h.conn.query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0)).unwrap();
            History::apply_schema(&h.conn).unwrap();
            assert_eq!(h.conn.query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0)).unwrap(), version);
        }
        let h = History::open_in_memory();
        h.conn.execute_batch("DROP INDEX idx_msg_content; CREATE INDEX idx_msg_content ON messages(body);
            INSERT INTO messages(conversation,sender,body,ts,live) VALUES('r','u','same',1000,0),('r','u','same',1000,0);").unwrap();
        assert!(History::apply_schema(&h.conn).is_err());
        assert_eq!(h.load_tail("r", 10).unwrap().len(), 2);
        assert_eq!(h.conn.query_row("SELECT sql FROM sqlite_master WHERE name='idx_msg_content'", [], |row| row.get::<_, String>(0)).unwrap(),
            "CREATE INDEX idx_msg_content ON messages(body)");
    }

    #[test]
    fn file_backed_history_requests_durable_commits_on_every_open()
    {
        let directory = std::env::temp_dir().join(format!("snack-durable-history-{}", uuid::Uuid::new_v4()));
        let path = directory.join("history.sqlite");
        let check = |history: &History| {
            assert_eq!(history.conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0)).unwrap(), "wal");
            assert_eq!(history.conn.query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0)).unwrap(), 2,
                "queued sends require a synchronized WAL commit");
            #[cfg(target_os = "macos")]
            assert_eq!(history.conn.query_row("PRAGMA fullfsync", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
        };
        let first = History::open(&path).unwrap();
        check(&first);
        let (outgoing, stored) = first.enqueue_direct_message("alice@example", "me", "keep this queued").unwrap();
        // The old connection policy must not carry into a reopened account DB.
        first.conn.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA fullfsync = OFF;").unwrap();
        drop(first);
        let reopened = History::open(&path).unwrap();
        check(&reopened);
        let pending = reopened.load_outbox().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].origin_id, outgoing.origin_id);
        assert_eq!(reopened.load_tail("alice@example", 10).unwrap()[0].id, stored.id);
        drop(reopened);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn history_files_are_private_on_creation_and_existing_sidecars_are_repaired()
    {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!("snack-private-history-{}", uuid::Uuid::new_v4()));
        let path = directory.join("history.sqlite");
        let first = History::open(&path).unwrap();
        first.insert("alice", "alice", "private content", at(1), false, None).unwrap();
        let mode = |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&directory), 0o700);
        let files = [path.clone(), directory.join("history.sqlite-wal"), directory.join("history.sqlite-shm")];
        for file in &files {
            assert_eq!(mode(file), 0o600);
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let second = History::open(&path).unwrap();
        for file in &files { assert_eq!(mode(file), 0o600); }
        assert_eq!(second.load_tail("alice", 1).unwrap()[0].body, "private content");
        drop(second);
        drop(first);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn history_refuses_symlinked_database_and_sidecars_without_changing_the_target()
    {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let directory = std::env::temp_dir().join(format!("snack-history-link-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let target = directory.join("unrelated");
        std::fs::write(&target, b"leave this file alone").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let path = directory.join("history.sqlite");
        symlink(&target, &path).unwrap();
        assert!(History::open(&path).is_err());
        std::fs::remove_file(&path).unwrap();
        symlink(&target, directory.join("history.sqlite-wal")).unwrap();
        assert!(History::open(&path).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"leave this file alone");
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o644);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn direct_message_delivery_survives_reopen_and_late_errors()
    {
        let directory = std::env::temp_dir().join(format!("snack-dm-delivery-{}", uuid::Uuid::new_v4()));
        let path = directory.join("history.sqlite");
        let h = History::open(&path).unwrap();
        let (outgoing, message) = h.enqueue_direct_message("alice", "me", "queued").unwrap();
        assert_eq!(message.delivery, DeliveryState::Queued(outgoing.id));
        drop(h);
        let h = History::open(&path).unwrap();
        assert_eq!(h.load_tail("alice", 10).unwrap()[0].delivery, DeliveryState::Queued(outgoing.id));
        assert_eq!(h.load_window_around("alice", message.id, 0, 0).unwrap()[0].delivery, DeliveryState::Queued(outgoing.id));
        h.confirm_outbox(outgoing.id).unwrap();
        assert_eq!(h.load_tail("alice", 10).unwrap()[0].delivery, DeliveryState::Submitted(outgoing.id));
        h.reject_outbox(outgoing.id, "late routing failure").unwrap();
        assert_eq!(h.load_tail("alice", 10).unwrap()[0].delivery, DeliveryState::Failed(outgoing.id));
        h.confirm_outbox(outgoing.id).unwrap();
        assert_eq!(h.load_tail("alice", 10).unwrap()[0].delivery, DeliveryState::Failed(outgoing.id));
        h.remove_outbox(outgoing.id).unwrap();
        assert_eq!(h.load_tail("alice", 10).unwrap()[0].delivery, DeliveryState::Failed(outgoing.id));
        drop(h);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn direct_message_queue_and_history_are_atomic_and_identical_sends_keep_distinct_ids()
    {
        let h = History::open_in_memory();
        h.disable_outgoing_reads();
        assert!(h.enqueue_direct_message("alice", "me", "failed transaction").is_err());
        h.enable_outgoing_reads();
        assert!(h.load_outbox().unwrap().is_empty());
        assert!(h.load_tail("alice", 10).unwrap().is_empty());
        let (first, first_message) = h.enqueue_direct_message("alice", "me", "same body").unwrap();
        let (second, second_message) = h.enqueue_direct_message("alice", "me", "same body").unwrap();
        // Force the exact same timestamp; outgoing identity must distinguish them.
        h.conn.execute("UPDATE messages SET ts = ?1 WHERE id = ?2", params![first_message.received.timestamp_millis(), second_message.id]).unwrap();
        assert_ne!(first_message.id, second_message.id);
        assert_ne!(first.origin_id, second.origin_id);
        h.confirm_outbox(second.id).unwrap();
        let messages = h.load_tail("alice", 10).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].delivery, DeliveryState::Queued(first.id));
        assert_eq!(messages[1].delivery, DeliveryState::Submitted(second.id));
    }

    #[test]
    fn acknowledged_dm_recovers_history_after_restart_before_retiring_its_queue()
    {
        let directory = std::env::temp_dir().join(format!("snack-dm-recovery-{}", uuid::Uuid::new_v4()));
        let path = directory.join("history.sqlite");
        let h = History::open(&path).unwrap();
        h.disable_reads();
        let queued = h.enqueue_direct_retry("alice", "original sender", "retained history").unwrap();
        let timestamp: i64 = h.conn.query_row("SELECT created_ts FROM outgoing_messages WHERE outbox_id = ?1", params![queued.id], |row| row.get(0)).unwrap();
        assert!(h.confirm_outbox(queued.id).is_err());
        assert_eq!(h.load_outbox().unwrap()[0].origin_id, queued.origin_id);
        h.enable_reads();
        drop(h); // No in-memory history retry survives this restart.
        let h = History::open(&path).unwrap();
        assert!(h.load_tail("alice", 10).unwrap().is_empty());
        h.confirm_outbox(queued.id).unwrap();
        assert!(h.load_outbox().unwrap().is_empty());
        let messages = h.load_tail("alice", 10).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].sender, "original sender");
        assert_eq!(messages[0].received.timestamp_millis(), timestamp);
        assert_eq!(messages[0].delivery, DeliveryState::Submitted(queued.id));
        h.confirm_outbox(queued.id).unwrap();
        assert_eq!(h.load_tail("alice", 10).unwrap().len(), 1);
        drop(h);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unmarked_legacy_queues_are_not_heuristically_added_to_history()
    {
        let h = History::open_in_memory();
        let legacy = h.enqueue_outbox_message("alice", false, "old message").unwrap();
        let room = h.enqueue_outbox_message("room", true, "room message").unwrap();
        h.disable_reads();
        h.confirm_outbox(legacy.id).unwrap();
        h.confirm_outbox(room.id).unwrap();
        h.enable_reads();
        assert!(h.load_tail("alice", 10).unwrap().is_empty());
        assert!(h.load_tail("room", 10).unwrap().is_empty());
    }

    #[test]
    fn unqueued_attempts_keep_exact_delivery_identity_after_reopen()
    {
        let directory = std::env::temp_dir().join(format!("snack-unqueued-{}", uuid::Uuid::new_v4()));
        let path = directory.join("history.sqlite");
        let h = History::open(&path).unwrap();
        let first = uuid::Uuid::new_v4().to_string();
        let second = uuid::Uuid::new_v4().to_string();
        let one = h.store_unqueued_direct(&first, "alice", "me", "same", at(0), UnqueuedDelivery::Unconfirmed).unwrap();
        let two = h.store_unqueued_direct(&second, "alice", "me", "same", at(0), UnqueuedDelivery::Unconfirmed).unwrap();
        assert_ne!(one.id, two.id);
        drop(h);
        let h = History::open(&path).unwrap();
        assert_eq!(h.unqueued_direct(&first).unwrap().unwrap().1.delivery, DeliveryState::Unconfirmed);
        h.store_unqueued_direct(&second, "alice", "me", "same", at(0), UnqueuedDelivery::Submitted).unwrap();
        let messages = h.load_tail("alice", 10).unwrap();
        assert_eq!(messages[0].delivery, DeliveryState::Unconfirmed);
        assert_eq!(messages[1].delivery, DeliveryState::UnqueuedSubmitted);
        h.store_unqueued_direct(&second, "alice", "me", "same", at(0), UnqueuedDelivery::Failed).unwrap();
        h.store_unqueued_direct(&second, "alice", "me", "same", at(0), UnqueuedDelivery::Submitted).unwrap();
        assert_eq!(h.unqueued_direct(&second).unwrap().unwrap().1.delivery, DeliveryState::UnqueuedFailed);
        assert!(h.load_outbox().unwrap().is_empty());
        drop(h);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn content_conflict_returns_the_incoming_row_instead_of_an_independent_outgoing_attempt()
    {
        let h = History::open_in_memory();
        let outgoing = h.store_unqueued_direct("origin", "alice", "me", "same", at(0), UnqueuedDelivery::Unconfirmed).unwrap();
        let incoming = h.insert("alice", "me", "same", at(0), false, None).unwrap().unwrap();
        assert_ne!(outgoing.id, incoming);
        assert_eq!(h.store("alice", "me", "same", at(0), false, None).unwrap().id, incoming);
    }

    #[test]
    fn live_direct_origin_copies_keep_local_time_and_live_status() {
        let h = History::open_in_memory();
        let (outgoing, original) = h.enqueue_direct_message("me@example", "me", "note").unwrap();
        let (stored, _) = h.reconcile_own_direct_copy("me@example", "note", &outgoing.origin_id,
            original.received + chrono::Duration::hours(2), Some("live-copy"), false).unwrap().unwrap();
        assert_eq!(stored.id, original.id);
        assert_eq!(stored.received, original.received);
        assert_eq!(h.conn.query_row("SELECT live FROM messages WHERE id=?1", [stored.id], |row| row.get::<_, i64>(0)).unwrap(), 1);
        assert!(h.load_outbox().unwrap().is_empty());
        h.reconcile_own_direct_copy("me@example", "note", &outgoing.origin_id, at(5), Some("archive-copy"), true).unwrap().unwrap();
        assert_eq!(h.load_tail("me@example", 10).unwrap()[0].received, at(5));
        assert_eq!(h.conn.query_row("SELECT live FROM messages WHERE id=?1", [stored.id], |row| row.get::<_, i64>(0)).unwrap(), 0);
    }

    #[test]
    fn own_origins_reconcile_queued_and_best_effort_archives_and_keep_every_server_alias()
    {
        for queued in [false, true] {
            let h = History::open_in_memory();
            let (origin, original) = if queued {
                let (outgoing, stored) = h.enqueue_direct_message("alice", "me", "same").unwrap();
                (outgoing.origin_id, stored)
            } else {
                let origin = uuid::Uuid::new_v4().to_string();
                let stored = h.store_unqueued_direct(&origin, "alice", "me", "same", at(100_000), UnqueuedDelivery::Unconfirmed).unwrap();
                (origin, stored)
            };
            let duplicate = h.insert("alice", "me", "same", at(10), true, Some("copy-A")).unwrap().unwrap();
            let (stored, merged) = h.reconcile_own_direct_copy("alice", "same", &origin, at(20), Some("copy-A"), true).unwrap().unwrap();
            assert_eq!(stored.id, original.id);
            assert_eq!(merged, [duplicate]);
            assert_eq!(stored.received, at(10));
            h.reconcile_own_direct_copy("alice", "same", &origin, at(5), Some("copy-B"), true).unwrap().unwrap();
            assert_eq!(h.load_tail("alice", 10).unwrap().len(), 1);
            assert_eq!(h.load_tail("alice", 10).unwrap()[0].received, at(5));
            for sid in ["copy-A", "copy-B"] {
                assert!(h.insert("alice", "me", "same", at(999_999), true, Some(sid)).unwrap().is_none());
            }
            assert!(h.load_outbox().unwrap().is_empty());
        }
    }

    #[test]
    fn origin_reconciliation_checks_conversation_body_and_distinct_outgoing_identity()
    {
        let h = History::open_in_memory();
        let (one, first) = h.enqueue_direct_message("alice", "me", "same").unwrap();
        let (two, second) = h.enqueue_direct_message("alice", "me", "same").unwrap();
        assert!(h.reconcile_own_direct_copy("bob", "same", &one.origin_id, at(0), Some("x"), true).unwrap().is_none());
        assert!(h.reconcile_own_direct_copy("alice", "changed body", &one.origin_id, at(0), Some("x"), true).unwrap().is_none());
        h.reconcile_own_direct_copy("alice", "same", &one.origin_id, at(0), Some("one"), true).unwrap();
        h.reconcile_own_direct_copy("alice", "same", &two.origin_id, at(0), Some("two"), true).unwrap();
        assert!(h.reconcile_own_direct_copy("alice", "same", &one.origin_id, at(0), Some("two"), true).is_err());
        let messages = h.load_tail("alice", 10).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].id, first.id);
        assert_eq!(messages[1].id, second.id);
        assert_eq!(h.store("alice", "me", "same", at(0), true, Some("two")).unwrap().id, second.id);
    }

    #[test]
    fn reconciled_server_aliases_survive_database_reopen()
    {
        let directory = std::env::temp_dir().join(format!("snack-archive-alias-{}", uuid::Uuid::new_v4()));
        let path = directory.join("history.sqlite");
        let h = History::open(&path).unwrap();
        let (outgoing, local) = h.enqueue_direct_message("alice", "me", "one logical message").unwrap();
        for sid in ["first-server-copy", "retried-server-copy"] {
            h.reconcile_own_direct_copy("alice", "one logical message", &outgoing.origin_id, at(0), Some(sid), true).unwrap().unwrap();
        }
        drop(h);
        let h = History::open(&path).unwrap();
        for sid in ["first-server-copy", "retried-server-copy"] {
            let stored = h.store("alice", "me", "one logical message", at(100_000), true, Some(sid)).unwrap();
            assert_eq!(stored.id, local.id);
        }
        assert_eq!(h.load_tail("alice", 10).unwrap().len(), 1);
        drop(h);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn verified_room_echoes_keep_one_history_identity_and_all_aliases_after_reopen()
    {
        let directory = std::env::temp_dir().join(format!("snack-room-origin-{}", uuid::Uuid::new_v4()));
        let path = directory.join("history.sqlite");
        let h = History::open(&path).unwrap();
        let outgoing = h.enqueue_outbox_message("room", true, "same").unwrap();
        let (first, _, created) = h.reconcile_own_room_origin("room", "me", "same", &outgoing.origin_id, at(0), Some("first")).unwrap().unwrap();
        assert!(created);
        let (again, _, created) = h.reconcile_own_room_origin("room", "new nick", "same", &outgoing.origin_id, at(10), Some("retry")).unwrap().unwrap();
        assert!(!created);
        assert_eq!(first.id, again.id);
        assert_eq!(again.received, at(0));
        assert!(h.load_outbox().unwrap().is_empty());
        drop(h);
        let h = History::open(&path).unwrap();
        h.store_with_policy("room", "me", "same", at(5), true, Some("first"), false).unwrap();
        let stored = h.store_with_policy("room", "new nick", "same", at(2), true, Some("retry"), false).unwrap();
        assert_eq!(stored.id, first.id);
        assert_eq!(stored.received, at(2));
        assert_eq!(h.load_tail("room", 10).unwrap().len(), 1);
        // A different server ID is not a content-based twin of a known identity.
        assert!(h.insert("room", "me", "same", at(3), true, Some("independent")).unwrap().is_some());
        drop(h);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn history_reads_distinguish_failures_from_empty_results()
    {
        let h = History::open_in_memory();
        assert!(h.load_tail("room", 10).unwrap().is_empty());
        assert!(h.newest_received("room").unwrap().is_none());
        assert!(!h.has_older("room", 0, 0).unwrap());
        let id = h.insert("room", "alice", "saved", at(0), false, None).unwrap().unwrap();
        h.disable_reads();
        assert!(h.load_tail("room", 10).is_err());
        assert!(h.load_older("room", i64::MAX, i64::MAX, 10).is_err());
        assert!(h.load_window_around("room", id, 3, 3).is_err());
        assert!(h.newest_received("room").is_err());
        assert!(h.has_older("room", 0, 0).is_err());
        assert!(h.has_newer("room", 0, 0).is_err());
        assert!(h.search(Some("room"), "saved", 10).is_err());
        assert!(h.search(None, "saved", 10).is_err());
        h.enable_reads();
        assert_eq!(h.load_tail("room", 10).unwrap()[0].body, "saved");
    }

    #[test]
    fn history_queries_do_not_silently_skip_undecodable_rows()
    {
        let h = History::open_in_memory();
        h.insert("room", "alice", "valid", at(0), false, None).unwrap();
        let bad = h.insert("room", "alice", "invalid", at(1), false, None).unwrap().unwrap();
        h.conn.execute("UPDATE messages SET body = x'ff' WHERE id = ?1", params![bad]).unwrap();
        assert!(h.load_tail("room", 10).is_err());
        assert!(h.load_older("room", i64::MAX, i64::MAX, 10).is_err());
        assert!(h.load_window_around("room", bad, 3, 3).is_err());
    }

    #[test]
    fn global_search_does_not_silently_skip_undecodable_hits()
    {
        for corrupt_time in [false, true] {
            let h = History::open_in_memory();
            h.insert("room", "alice", "needle", at(0), false, None).unwrap();
            let bad = h.insert("other", "alice", "needle", at(1), false, None).unwrap().unwrap();
            if corrupt_time {
                h.conn.execute("UPDATE messages SET ts = ?1 WHERE id = ?2", params![i64::MAX, bad]).unwrap();
                assert!(h.search(Some("other"), "needle", 10).is_err());
                assert!(h.search_conversations(&["other"], "needle", 10).is_err());
            } else {
                h.conn.execute("UPDATE messages SET conversation = x'ff' WHERE id = ?1", params![bad]).unwrap();
            }
            assert!(h.search(None, "needle", 10).is_err());
            assert_eq!(h.search(Some("room"), "needle", 10).unwrap().len(), 1);
        }
    }

    #[test]
    fn search_caps_keep_recent_matches_and_scope_before_limiting()
    {
        let h = History::open_in_memory();
        let latest = h.insert("open", "alice", "needle", at(2), false, None).unwrap().unwrap();
        h.insert("open", "alice", "needle", at(0), false, None).unwrap();
        let middle = h.insert("open", "alice", "needle", at(1), false, None).unwrap().unwrap();
        assert_eq!(h.search(Some("open"), "needle", 2).unwrap().iter().map(|hit| hit.id).collect::<Vec<_>>(), vec![middle, latest]);
        for i in 3..6 { h.insert("closed", "alice", "needle", at(i), false, None).unwrap(); }
        assert_eq!(h.search_conversations(&["open"], "needle", 2).unwrap().iter().map(|hit| hit.id).collect::<Vec<_>>(), vec![middle, latest]);
        assert!(h.search_conversations(&[], "needle", 2).unwrap().is_empty());
        assert!(h.search_conversations(&["open"], "needle", 0).unwrap().is_empty());
        let quoted = "room'); DROP TABLE messages; --";
        h.insert(quoted, "alice", "100%_literal", at(9), false, None).unwrap();
        assert_eq!(h.search_conversations(&[quoted], "100%_", 2).unwrap().len(), 1);
        assert_eq!(h.search(Some("open"), "needle", 2).unwrap().len(), 2);
    }

    #[test]
    fn outbox_reads_report_sql_and_row_errors_without_returning_partial_queues()
    {
        let h = History::open_in_memory();
        h.enqueue_outbox("alice@example", false, "valid").unwrap();
        let bad = h.enqueue_outbox("alice@example", false, "bad").unwrap();
        h.disable_outbox_reads();
        assert!(h.load_outbox().is_err());
        h.enable_outbox_reads();
        assert_eq!(h.load_outbox().unwrap().len(), 2);
        h.conn.execute("UPDATE outbox SET body = x'ff' WHERE id = ?1", params![bad]).unwrap();
        assert!(h.load_outbox().is_err());
    }

    #[test]
    fn legacy_outbox_rows_count_as_attempted_and_new_rows_do_not_until_marked()
    {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE outbox (id INTEGER PRIMARY KEY AUTOINCREMENT,
            conversation TEXT NOT NULL, is_room INTEGER NOT NULL, body TEXT NOT NULL, created_ts INTEGER NOT NULL);
            INSERT INTO outbox (conversation, is_room, body, created_ts) VALUES ('room@example', 1, 'legacy', 1);").unwrap();
        History::apply_schema(&conn).unwrap();
        let h = History { conn };
        let legacy = h.load_outbox().unwrap()[0].id;
        assert_eq!(h.attempted_queued("room@example", true).unwrap(), vec![legacy]);
        let fresh = h.enqueue_outbox("room@example", true, "fresh").unwrap();
        assert_eq!(h.attempted_queued("room@example", true).unwrap(), vec![legacy]);
        h.mark_outbox_attempted(fresh).unwrap();
        History::apply_schema(&h.conn).unwrap();
        assert_eq!(h.attempted_queued("room@example", true).unwrap(), vec![legacy, fresh]);
        assert!(h.attempted_queued("room@example", false).unwrap().is_empty());
        assert_eq!(h.oldest_queued("room@example", true).unwrap(), DateTime::from_timestamp_millis(1));
    }

    #[test]
    fn legacy_outbox_rows_receive_unique_stable_origin_ids()
    {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE outbox (id INTEGER PRIMARY KEY AUTOINCREMENT,
            conversation TEXT NOT NULL, is_room INTEGER NOT NULL, body TEXT NOT NULL, created_ts INTEGER NOT NULL);
            INSERT INTO outbox (conversation, is_room, body, created_ts) VALUES ('alice@example', 0, 'same', 1), ('alice@example', 0, 'same', 2);").unwrap();
        History::apply_schema(&conn).unwrap();
        let h = History { conn };
        let first = h.load_outbox().unwrap();
        assert_ne!(first[0].origin_id, first[1].origin_id);
        for row in &first { assert_eq!(uuid::Uuid::parse_str(&row.origin_id).unwrap().get_version_num(), 4); }
        History::apply_schema(&h.conn).unwrap();
        // A concurrently running older version can still insert an old row.
        h.conn.execute("INSERT INTO outbox (conversation, is_room, body, created_ts) VALUES ('alice@example', 0, 'older writer', 3)", []).unwrap();
        let next = h.load_outbox().unwrap();
        assert_eq!(next.len(), 3);
        assert_eq!(first[0].origin_id, next[0].origin_id);
        assert_eq!(first[1].origin_id, next[1].origin_id);
        assert_eq!(uuid::Uuid::parse_str(&next[2].origin_id).unwrap().get_version_num(), 4);
        h.disable_writes();
        assert_eq!(h.load_outbox().unwrap()[2].origin_id, next[2].origin_id);
    }

    #[test]
    fn queued_origin_ids_survive_reopening_the_database()
    {
        let dir = std::env::temp_dir().join(format!("snack-origin-{}", uuid::Uuid::new_v4()));
        let path = dir.join("history.sqlite");
        let h = History::open(&path).unwrap();
        let first = h.enqueue_outbox_message("alice@example", false, "retry me").unwrap();
        let second = h.enqueue_outbox_message("alice@example", false, "retry me").unwrap();
        assert_ne!(first.origin_id, second.origin_id);
        drop(h);
        let h = History::open(&path).unwrap();
        let rows = h.load_outbox().unwrap();
        assert_eq!(rows[0].origin_id, first.origin_id);
        assert_eq!(rows[1].origin_id, second.origin_id);
        drop(h);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn origin_migration_failure_rolls_back_without_changing_queued_messages()
    {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE outbox (id INTEGER PRIMARY KEY AUTOINCREMENT,
            conversation TEXT NOT NULL, is_room INTEGER NOT NULL, body TEXT NOT NULL, created_ts INTEGER NOT NULL);
            INSERT INTO outbox (conversation, is_room, body, created_ts) VALUES ('alice@example', 0, 'keep me', 1);
            CREATE TRIGGER fail_origin_migration BEFORE UPDATE ON outbox BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
        assert!(History::apply_schema(&conn).is_err());
        let columns = conn.prepare("PRAGMA table_info(outbox)").unwrap()
            .query_map([], |row| row.get::<_, String>(1)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert!(!columns.iter().any(|column| column == "origin_id"));
        assert_eq!(conn.query_row("SELECT body FROM outbox", [], |row| row.get::<_, String>(0)).unwrap(), "keep me");
        conn.execute_batch("DROP TRIGGER fail_origin_migration").unwrap();
        History::apply_schema(&conn).unwrap();
        assert_eq!(History { conn }.load_outbox().unwrap()[0].body, "keep me");
    }

    #[test]
    fn routing_errors_survive_confirmation_in_either_order_and_database_reopen()
    {
        for error_first in [false, true] {
            let dir = std::env::temp_dir().join(format!("snack-error-{}", uuid::Uuid::new_v4()));
            let path = dir.join("history.sqlite");
            let h = History::open(&path).unwrap();
            let message = h.enqueue_outbox_message("alice@example", false, "recover me").unwrap();
            if error_first { h.reject_outbox(message.id, "forbidden").unwrap(); }
            h.confirm_outbox(message.id).unwrap();
            // Old versions query the raw table without the new filter.
            assert_eq!(h.conn.query_row("SELECT count(*) FROM outbox", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
            if !error_first { h.reject_outbox(message.id, "forbidden").unwrap(); }
            drop(h);
            let h = History::open(&path).unwrap();
            assert!(h.load_outbox().unwrap().is_empty());
            let failed = h.load_rejected_outbox().unwrap();
            assert_eq!(failed.len(), 1);
            assert_eq!(failed[0].0.body, "recover me");
            assert_eq!(failed[0].0.origin_id, message.origin_id);
            h.remove_outbox(message.id).unwrap();
            assert!(h.outgoing_by_origin(&message.origin_id).unwrap().is_none());
            drop(h);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn message_error_sources_are_checked_against_the_actual_target()
    {
        let mut message = OutboxItem { id: 1, origin_id: "id".into(), conversation: "alice@remote/phone".into(),
            is_room: false, body: "body".into() };
        for from in [None, Some("me@local"), Some("local"), Some("remote"), Some("alice@remote/phone")] {
            assert!(message.accepts_error_from(from, "me@local/device"));
        }
        for from in ["", "mallory@remote/phone", "alice@remote/other", "alice@remote", "evil"] {
            assert!(!message.accepts_error_from(Some(from), "me@local/device"));
        }
        message.conversation = "alice@remote".into();
        assert!(message.accepts_error_from(Some("alice@remote/phone"), "me@local/device"));
        message.conversation = "room@conference".into();
        message.is_room = true;
        assert!(message.accepts_error_from(Some("room@conference"), "me@local/device"));
        assert!(message.accepts_error_from(Some("conference"), "me@local/device"));
        assert!(!message.accepts_error_from(Some("room@conference/mallory"), "me@local/device"));
        assert!(!message.accepts_error_from(Some("other@conference"), "me@local/device"));
    }

    #[test]
    fn message_error_authority_supports_equivalent_domains() {
        let mut message = OutboxItem { id: 1, origin_id: "id".into(), conversation: "alice@[2001:db8::1]/Phone".into(),
            is_room: false, body: "body".into() };
        for from in ["alice@[2001:0db8:0:0:0:0:0:1]/Phone", "[2001:0db8:0:0:0:0:0:1]", "LOCAL.example"] {
            assert!(message.accepts_error_from(Some(from), "me@local.EXAMPLE/device"));
        }
        for from in ["alice@[2001:db8::1]/phone", "Alice@[2001:db8::1]/Phone", "[2001:db8::2]"] {
            assert!(!message.accepts_error_from(Some(from), "me@local.example/device"));
        }
        message.conversation = "room@CONFERENCE.example".into();
        message.is_room = true;
        assert!(message.accepts_error_from(Some("room@conference.EXAMPLE"), "me@local/device"));
        assert!(!message.accepts_error_from(Some("room@conference.example/occupant"), "me@local/device"));
    }

    #[test]
    fn invalid_history_timestamps_are_errors_instead_of_the_current_time()
    {
        let h = History::open_in_memory();
        let id = h.insert("room", "alice", "saved", at(0), false, None).unwrap().unwrap();
        h.conn.execute("UPDATE messages SET ts = ?1 WHERE id = ?2", params![i64::MAX, id]).unwrap();
        assert!(h.load_tail("room", 10).is_err());
        assert!(h.load_window_around("room", id, 3, 3).is_err());
        assert!(h.newest_received("room").is_err());
        h.conn.execute("UPDATE messages SET ts = ?1 WHERE id = ?2", params![at(0).timestamp_millis(), id]).unwrap();
        assert_eq!(h.load_tail("room", 10).unwrap()[0].received, at(0));
    }

    #[test]
    fn search_windows_require_a_center_in_the_requested_conversation()
    {
        let h = History::open_in_memory();
        let other = h.insert("other", "alice", "other", at(0), false, None).unwrap().unwrap();
        h.insert("room", "alice", "room", at(1), false, None).unwrap();
        assert!(h.load_window_around("room", other, 3, 3).unwrap().is_empty());
    }

    #[test]
    fn retries_return_the_exact_row_selected_by_deduplication()
    {
        let h = History::open_in_memory();
        let sid_row = h.insert("room", "alice", "same text", at(0), false, Some("sid")).unwrap().unwrap();
        let no_sid_row = h.insert("room", "alice", "same text", at(0), false, None).unwrap().unwrap();
        assert_ne!(sid_row, no_sid_row);
        assert_eq!(h.store("room", "alice", "same text", at(0), false, None).unwrap().id, no_sid_row);
        assert_eq!(h.store("room", "alice", "same text", at(10), true, Some("sid")).unwrap().id, sid_row);
        assert_eq!(h.store("room", "alice", "same text", at(0), true, None).unwrap().id, no_sid_row);
        assert_eq!(h.store("room", "alice", "same text", at(10), false, Some("sid")).unwrap().received, at(10));
    }

    #[test]
    fn concurrent_archives_cannot_both_claim_the_same_unstamped_live_message()
    {
        use std::{cell::RefCell, sync::mpsc, time::Duration};
        thread_local! {
            static BLOCKED: RefCell<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>> = const { RefCell::new(None) };
        }
        let directory = std::env::temp_dir().join(format!("snack-history-race-{}", uuid::Uuid::new_v4()));
        let path = directory.join("history.sqlite");
        let first = History::open(&path).unwrap();
        let original = first.insert("room", "alice", "same text", at(0), false, None).unwrap().unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (start_tx, start_rx) = mpsc::channel();
        let (blocked_tx, blocked_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut second = History::open(&path).unwrap();
            BLOCKED.with(|slot| *slot.borrow_mut() = Some((blocked_tx, resume_rx)));
            second.conn.busy_handler(Some(|_| BLOCKED.with(|slot| {
                let Some((notify, resume)) = slot.borrow_mut().take() else { return false; };
                notify.send(()).is_ok() && resume.recv_timeout(Duration::from_secs(5)).is_ok()
            }))).unwrap();
            ready_tx.send(()).unwrap();
            start_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            second.insert("room", "alice", "same text", at(10), true, Some("archive-B"))
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        first.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        start_tx.send(()).unwrap();
        blocked_rx.recv_timeout(Duration::from_secs(5)).expect("second writer must wait for the first");
        // The first writer claims the live twin while the second is waiting.
        first.conn.execute("UPDATE messages SET stanza_id = 'archive-A', ts = ?1, live = 0 WHERE id = ?2",
            params![at(10).timestamp_millis(), original]).unwrap();
        first.conn.execute_batch("COMMIT").unwrap();
        resume_tx.send(()).unwrap();
        let inserted = worker.join().unwrap().unwrap();
        assert!(inserted.is_some(), "archive-B is distinct and must not be dropped as a twin of archive-A");
        assert_eq!(first.load_tail("room", 10).unwrap().len(), 2);
        assert_eq!(first.store("room", "alice", "same text", at(10), true, Some("archive-A")).unwrap().id, original);
        drop(first);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn insertion_failure_is_not_a_duplicate()
    {
        let h = History::open_in_memory();
        h.conn.execute_batch("PRAGMA query_only = ON").unwrap();
        assert!(h.insert("room", "alice", "hello", at(0), false, None).is_err());
        assert!(h.load_tail("room", 10).unwrap().is_empty());
        h.conn.execute_batch("PRAGMA query_only = OFF").unwrap();
        assert!(h.insert("room", "alice", "hello", at(0), false, None).unwrap().is_some());
        assert!(h.insert("room", "alice", "hello", at(0), false, None).unwrap().is_none());
    }

    #[test]
    fn deduplication_update_failure_is_reported()
    {
        let h = History::open_in_memory();
        h.insert("room", "alice", "hello", at(0), false, Some("id")).unwrap();
        h.conn.execute_batch("PRAGMA query_only = ON").unwrap();
        assert!(h.insert("room", "alice", "hello", at(1), true, Some("id")).is_err());
    }

    #[test]
    fn exact_duplicate_is_dropped()
    {
        let h = History::open_in_memory();
        assert!(h.insert("room", "alice", "hello", at(0), false, None).unwrap().is_some());
        // Same conversation/sender/body/ts -> dropped.
        assert!(h.insert("room", "alice", "hello", at(0), false, None).unwrap().is_none());
    }

    #[test]
    fn history_replay_with_same_stamp_dedupes()
    {
        let h = History::open_in_memory();
        // First join: history message with a server delay stamp.
        assert!(h.insert("room", "bob", "gm", at(100), true, None).unwrap().is_some());
        // Rejoin replays the identical stamped message -> exact dedup.
        assert!(h.insert("room", "bob", "gm", at(100), true, None).unwrap().is_none());
    }

    #[test]
    fn live_then_replayed_history_dedupes_within_tolerance()
    {
        let h = History::open_in_memory();
        // Stored live at our local clock.
        assert!(h.insert("room", "carol", "deploying now", at(500), false, None).unwrap().is_some());
        // Server replays it after a reconnect with its (slightly different)
        // original stamp; fuzzy live-provenance rule collapses it.
        assert!(h.insert("room", "carol", "deploying now", at(503), true, None).unwrap().is_none());
        // Outside the tolerance window it is treated as a separate message.
        assert!(h.insert("room", "carol", "deploying now", at(5000), true, None).unwrap().is_some());
    }

    #[test]
    fn two_distinct_history_messages_same_text_both_kept()
    {
        let h = History::open_in_memory();
        // Two genuinely distinct history lines with identical text but different
        // (exact) stamps must both survive — the fuzzy rule only matches
        // live-provenance rows, so it never collapses these.
        assert!(h.insert("room", "dave", "+1", at(10), true, None).unwrap().is_some());
        assert!(h.insert("room", "dave", "+1", at(11), true, None).unwrap().is_some());
        assert_eq!(h.load_tail("room", 10).unwrap().len(), 2);
    }

    #[test]
    fn repeated_live_messages_are_kept()
    {
        let h = History::open_in_memory();
        assert!(h.insert("dm", "me", "ok", at(0), false, None).unwrap().is_some());
        assert!(h.insert("dm", "me", "ok", at(30), false, None).unwrap().is_some());
        assert_eq!(h.load_tail("dm", 10).unwrap().len(), 2);
    }

    #[test]
    fn tail_and_paging_walk_the_whole_log()
    {
        let h = History::open_in_memory();
        for i in 0..10
        {
            h.insert("room", "u", &format!("m{}", i), at(i), false, None).unwrap();
        }

        let tail = h.load_tail("room", 3).unwrap();
        assert_eq!(tail.iter().map(|m| m.body.clone()).collect::<Vec<_>>(), vec!["m7", "m8", "m9"]);
        let cur = &tail[0];
        assert!(h.has_older("room", cur.received.timestamp_millis(), cur.id).unwrap());

        let older = h.load_older("room", cur.received.timestamp_millis(), cur.id, 3).unwrap();
        assert_eq!(older.iter().map(|m| m.body.clone()).collect::<Vec<_>>(), vec!["m4", "m5", "m6"]);
        let last = &tail[2];
        assert!(!h.has_newer("room", last.received.timestamp_millis(), last.id).unwrap());
    }

    #[test]
    fn archive_backfill_orders_by_time_not_id()
    {
        let h = History::open_in_memory();
        // Live messages arrive in time order (ids 1,2,3 == times 100,101,102).
        h.insert("room", "u", "live-a", at(100), false, None).unwrap();
        h.insert("room", "u", "live-b", at(101), false, None).unwrap();
        h.insert("room", "u", "live-c", at(102), false, None).unwrap();

        // Now back-fill OLDER history from the server archive: inserted later, so
        // these get higher ids (4,5) despite older timestamps.
        let old1 = h.insert("room", "u", "old-1", at(50), true, None).unwrap().unwrap();
        let _old2 = h.insert("room", "u", "old-2", at(51), true, None).unwrap().unwrap();
        assert!(old1 > 3, "backfilled rows get higher ids");

        // The tail must still be the newest BY TIME, not by id.
        let tail = h.load_tail("room", 3).unwrap();
        assert_eq!(
            tail.iter().map(|m| m.body.clone()).collect::<Vec<_>>(),
            vec!["live-a", "live-b", "live-c"],
        );

        // Paging older than the tail front yields the backfilled history in
        // chronological order.
        let front = &tail[0];
        let older = h.load_older("room", front.received.timestamp_millis(), front.id, 10).unwrap();
        assert_eq!(
            older.iter().map(|m| m.body.clone()).collect::<Vec<_>>(),
            vec!["old-1", "old-2"],
        );
        assert!(!h.has_older("room", older[0].received.timestamp_millis(), older[0].id).unwrap());
    }

    #[test]
    fn window_around_centres_on_a_hit()
    {
        let h = History::open_in_memory();
        let mut ids = Vec::new();
        for i in 0..20
        {
            ids.push(h.insert("room", "u", &format!("m{}", i), at(i), false, None).unwrap().unwrap());
        }

        let window = h.load_window_around("room", ids[10], 3, 3).unwrap();
        let bodies: Vec<_> = window.iter().map(|m| m.body.clone()).collect();
        assert_eq!(bodies, vec!["m7", "m8", "m9", "m10", "m11", "m12", "m13"]);
    }

    #[test]
    fn stanza_id_dedupes_across_a_wide_timestamp_gap()
    {
        let h = History::open_in_memory();
        // The reconnect-duplication bug: a message delivered late on reconnect is
        // stored live with our clock; the MAM sweep re-fetches it with its real
        // (much earlier) archive stamp. The gap far exceeds the fuzzy window, so
        // only the shared stanza id can recognise them as the same message.
        let live_id = h.insert("room", "bot", "ping!", at(700), false, Some("arch-x")).unwrap().unwrap();
        assert!(h.insert("room", "bot", "ping!", at(0), true, Some("arch-x")).unwrap().is_none());
        assert_eq!(h.load_tail("room", 10).unwrap().len(), 1);
        // The authoritative archive timestamp was adopted onto the live row.
        let only = &h.load_tail("room", 10).unwrap()[0];
        assert_eq!(only.id, live_id);
        assert_eq!(only.received, at(0));
    }

    #[test]
    fn distinct_stanza_ids_with_same_text_both_kept()
    {
        let h = History::open_in_memory();
        // A bot genuinely posting "ping!" twice: two distinct messages,
        // hence two distinct stanza ids — both must survive.
        assert!(h.insert("room", "bot", "ping!", at(0), false, Some("arch-a")).unwrap().is_some());
        assert!(h.insert("room", "bot", "ping!", at(1), false, Some("arch-b")).unwrap().is_some());
        assert_eq!(h.load_tail("room", 10).unwrap().len(), 2);
    }

    #[test]
    fn distinct_stanza_id_in_fuzzy_window_is_not_collapsed()
    {
        let h = History::open_in_memory();
        // A live "ping!" with its own stanza id, then a *different* archived
        // "ping!" close enough in time to land inside the fuzzy window. Because
        // the live row carries a stanza id, the fuzzy fallback must not treat the
        // distinct-id archived copy as a duplicate.
        assert!(h.insert("room", "bot", "ping!", at(500), false, Some("arch-a")).unwrap().is_some());
        assert!(h.insert("room", "bot", "ping!", at(503), true, Some("arch-b")).unwrap().is_some());
        assert_eq!(h.load_tail("room", 10).unwrap().len(), 2);
    }

    #[test]
    fn stanza_id_backfilled_onto_unstamped_live_twin()
    {
        let h = History::open_in_memory();
        // A live copy stored before we had a stanza id (e.g. our own send), then
        // the archive copy arrives within the fuzzy window carrying the id: the
        // fallback rule dedups it and adopts the id so any further copy — even one
        // far outside the window — also dedups.
        h.insert("dm", "me", "hi", at(500), false, None).unwrap().unwrap();
        assert!(h.insert("dm", "me", "hi", at(503), true, Some("arch-z")).unwrap().is_none());
        // A later replay far outside the fuzzy window now dedups by stanza id.
        assert!(h.insert("dm", "me", "hi", at(99999), true, Some("arch-z")).unwrap().is_none());
        assert_eq!(h.load_tail("dm", 10).unwrap().len(), 1);
    }

    #[test]
    fn exact_timestamp_replay_still_backfills_server_id()
    {
        let h = History::open_in_memory();
        h.insert("r", "u", "hello", at(0), false, None).unwrap();
        assert!(h.insert("r", "u", "hello", at(0), true, Some("a")).unwrap().is_none());
        assert!(h.insert("r", "u", "hello", at(1000), true, Some("a")).unwrap().is_none());
        assert_eq!(h.load_tail("r", 10).unwrap().len(), 1);
    }

    #[test]
    fn distinct_server_ids_survive_identical_content_and_timestamps()
    {
        let h = History::open_in_memory();
        assert!(h.insert("r", "u", "ok", at(0), false, Some("a")).unwrap().is_some());
        assert!(h.insert("r", "u", "ok", at(0), true, Some("b")).unwrap().is_some());
        assert_eq!(h.load_tail("r", 10).unwrap().len(), 2);
    }

    #[test]
    fn legacy_unique_constraint_migrates_without_losing_messages()
    {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, conversation TEXT NOT NULL,
            sender TEXT NOT NULL, body TEXT NOT NULL, ts INTEGER NOT NULL, live INTEGER NOT NULL,
            UNIQUE(conversation, sender, body, ts));
            INSERT INTO messages VALUES (7, 'r', 'u', 'old', 1000, 1);
            INSERT INTO messages VALUES (1000, 'r', 'u', 'deleted', 2000, 1);
            DELETE FROM messages WHERE id = 1000;").unwrap();
        History::apply_schema(&conn).unwrap();
        let h = History { conn };
        assert_eq!(h.load_tail("r", 10).unwrap()[0].id, 7);
        let next = h.insert("r", "u", "ok", at(0), false, Some("a")).unwrap().unwrap();
        assert!(next > 1000, "migration reused a previously allocated message ID: {next}");
        assert!(h.insert("r", "u", "ok", at(0), true, Some("b")).unwrap().is_some());
        assert_eq!(h.load_tail("r", 10).unwrap().len(), 3);
    }

    #[test]
    fn legacy_unique_migration_recognizes_equivalent_constraint_definitions()
    {
        for constraint in [
            "UNIQUE(conversation,sender,body,ts)",
            "unique ( conversation , sender , body , ts )",
            "CONSTRAINT old_content UNIQUE ([conversation], `sender`, \"body\", ts)",
            "UNIQUE (ts, body, sender, conversation)",
        ] {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(&format!("CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT,
                conversation TEXT NOT NULL, sender TEXT NOT NULL, body TEXT NOT NULL,
                ts INTEGER NOT NULL, live INTEGER NOT NULL, {constraint});
                INSERT INTO messages VALUES (7, 'r', 'u', 'old', 1000, 1);")).unwrap();
            History::apply_schema(&conn).unwrap();
            let h = History { conn };
            let one = h.store("r", "u", "same", at(1), false, Some("one")).unwrap();
            let two = h.store("r", "u", "same", at(1), true, Some("two")).unwrap();
            assert_ne!(one.id, two.id, "constraint={constraint}");
            assert_eq!(h.load_tail("r", 10).unwrap().len(), 3);
        }
    }

    #[test]
    fn uniqueness_text_in_a_default_does_not_trigger_a_destructive_rebuild()
    {
        let h = History::open_in_memory();
        h.conn.execute_batch("ALTER TABLE messages ADD COLUMN retained_note TEXT DEFAULT 'UNIQUE(conversation, sender, body, ts)';").unwrap();
        h.insert("r", "u", "keep", at(0), false, Some("one")).unwrap();
        h.conn.execute("UPDATE messages SET retained_note = 'preserve this value'", []).unwrap();
        History::apply_schema(&h.conn).unwrap();
        assert_eq!(h.conn.query_row("SELECT retained_note FROM messages", [], |row| row.get::<_, String>(0)).unwrap(), "preserve this value");
        let one = h.store("r", "u", "same", at(1), false, Some("two")).unwrap();
        let two = h.store("r", "u", "same", at(1), true, Some("three")).unwrap();
        assert_ne!(one.id, two.id);
    }

    #[test]
    fn legacy_unique_migration_keeps_the_sequence_when_all_rows_were_deleted()
    {
        for table in ["messages", "MESSAGES"] {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(&format!("CREATE TABLE {table} (id INTEGER PRIMARY KEY AUTOINCREMENT, conversation TEXT NOT NULL,
                sender TEXT NOT NULL, body TEXT NOT NULL, ts INTEGER NOT NULL, live INTEGER NOT NULL,
                UNIQUE(conversation, sender, body, ts));
                INSERT INTO {table} VALUES (1000, 'r', 'u', 'deleted', 1000, 1);
                DELETE FROM {table};")).unwrap();
            History::apply_schema(&conn).unwrap();
            let h = History { conn };
            assert!(h.insert("r", "u", "new", at(0), false, None).unwrap().unwrap() > 1000);
        }
    }

    #[test]
    fn failed_legacy_unique_rebuild_rolls_back_and_can_be_retried()
    {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, conversation TEXT NOT NULL,
            sender TEXT NOT NULL, body TEXT NOT NULL, ts INTEGER NOT NULL, live INTEGER NOT NULL,
            UNIQUE(conversation, sender, body, ts));
            INSERT INTO messages VALUES (7, 'r', 'u', 'preserved', 1000, 1);
            CREATE TABLE messages_new (sentinel TEXT);
            INSERT INTO messages_new VALUES ('leave alone');").unwrap();
        assert!(History::apply_schema(&conn).is_err());
        assert!(conn.is_autocommit(), "failed rebuild left a transaction open");
        assert_eq!(conn.query_row("SELECT body FROM messages WHERE id=7", [], |row| row.get::<_, String>(0)).unwrap(), "preserved");
        assert_eq!(conn.query_row("SELECT sentinel FROM messages_new", [], |row| row.get::<_, String>(0)).unwrap(), "leave alone");
        conn.execute_batch("DROP TABLE messages_new;").unwrap();
        History::apply_schema(&conn).unwrap();
        let h = History { conn };
        assert_eq!(h.load_tail("r", 10).unwrap()[0].id, 7);
    }

    #[test]
    fn migration_recognizes_existing_mixed_case_columns()
    {
        let h = History::open_in_memory();
        let (queued, original) = h.enqueue_direct_message("alice", "me", "queued text").unwrap();
        let failed = h.store_unqueued_direct("best-effort-origin", "alice", "me", "failed text", at(1), UnqueuedDelivery::Failed).unwrap();
        for (table, column) in [
            ("outbox", "origin_id"), ("outgoing_messages", "history_sender"),
            ("messages", "stanza_id"), ("messages", "outbox_id"),
            ("messages", "unqueued_origin"), ("messages", "unqueued_state"),
        ] {
            h.conn.execute_batch(&format!("ALTER TABLE {table} RENAME COLUMN {column} TO casing_temp;
                ALTER TABLE {table} RENAME COLUMN casing_temp TO {};", column.to_ascii_uppercase())).unwrap();
        }
        h.conn.execute_batch("DROP INDEX idx_msg_content;
            CREATE UNIQUE INDEX IDX_MSG_CONTENT ON messages(conversation, sender, body, ts)
            WHERE STANZA_ID IS NULL AND OUTBOX_ID IS NULL;").unwrap();
        for _ in 0..2 {
            History::apply_schema(&h.conn).unwrap();
            assert_eq!(h.load_outbox().unwrap()[0].origin_id, queued.origin_id);
            assert_eq!(h.load_tail("alice", 10).unwrap().iter().find(|row| row.id == original.id).unwrap().delivery, DeliveryState::Queued(queued.id));
            let restored = h.unqueued_direct("best-effort-origin").unwrap().unwrap().1;
            assert_eq!(restored.id, failed.id);
            assert_eq!(restored.delivery, DeliveryState::UnqueuedFailed);
            let one = h.store_unqueued_direct("attempt-one", "alice", "me", "identical", at(2), UnqueuedDelivery::Unconfirmed).unwrap();
            let two = h.store_unqueued_direct("attempt-two", "alice", "me", "identical", at(2), UnqueuedDelivery::Unconfirmed).unwrap();
            assert_ne!(one.id, two.id, "case-variant legacy index still collapsed distinct attempts");
        }
    }

    #[test]
    fn legacy_unique_migration_preserves_existing_delivery_metadata()
    {
        for uppercase in [false, true] {
            let h = History::open_in_memory();
            let (queued, original) = h.enqueue_direct_message("alice", "me", "queued text").unwrap();
            let unqueued = h.store_unqueued_direct("best-effort-origin", "alice", "me", "failed text", at(1), UnqueuedDelivery::Failed).unwrap();
            h.conn.execute("INSERT INTO message_stanza_aliases VALUES ('alice', 'queued-alias', ?1)", [original.id]).unwrap();
            // A legacy uniqueness constraint must not make known delivery columns
            // disposable when the table is rebuilt.
            let columns = if uppercase { "OUTBOX_ID INTEGER, UNQUEUED_ORIGIN TEXT, UNQUEUED_STATE INTEGER NOT NULL DEFAULT 0" }
                else { "outbox_id INTEGER, unqueued_origin TEXT, unqueued_state INTEGER NOT NULL DEFAULT 0" };
            h.conn.execute_batch(&format!("ALTER TABLE messages RENAME TO prior_messages;
                CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, conversation TEXT NOT NULL,
                    sender TEXT NOT NULL, body TEXT NOT NULL, ts INTEGER NOT NULL, live INTEGER NOT NULL,
                    stanza_id TEXT, {columns},
                    UNIQUE(conversation, sender, body, ts));
                INSERT INTO messages SELECT * FROM prior_messages;
                DROP TABLE prior_messages;")).unwrap();
            History::apply_schema(&h.conn).unwrap();
            let rows = h.load_tail("alice", 10).unwrap();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows.iter().find(|row| row.id == original.id).unwrap().delivery, DeliveryState::Queued(queued.id));
            let (conversation, restored) = h.unqueued_direct("best-effort-origin").unwrap().unwrap();
            assert_eq!(conversation, "alice");
            assert_eq!(restored.id, unqueued.id);
            assert_eq!(restored.delivery, DeliveryState::UnqueuedFailed);
            assert_eq!(h.load_outbox().unwrap()[0].origin_id, queued.origin_id);
            assert_eq!(h.store("alice", "me", "queued text", original.received, false, Some("queued-alias")).unwrap().id, original.id);
            // Reopening the migrated schema must leave these identities intact.
            History::apply_schema(&h.conn).unwrap();
            assert_eq!(h.unqueued_direct("best-effort-origin").unwrap().unwrap().1.id, unqueued.id);
        }
    }

    #[test]
    fn account_ownership_cannot_change()
    {
        let h = History::open_in_memory();
        assert!(h.claim_account("alice@example").unwrap());
        assert!(!h.claim_account("bob@example").unwrap());
        assert!(h.claim_account("alice@example").unwrap());
    }

    #[test]
    fn fuzzy_replays_consume_distinct_live_twins_once()
    {
        let h = History::open_in_memory();
        h.insert("room", "u", "ok", at(100), false, None).unwrap();
        h.insert("room", "u", "ok", at(120), false, None).unwrap();
        assert!(h.insert("room", "u", "ok", at(101), true, None).unwrap().is_none());
        assert!(h.insert("room", "u", "ok", at(121), true, None).unwrap().is_none());
        assert!(h.insert("room", "u", "ok", at(101), true, None).unwrap().is_none());
        assert_eq!(h.load_tail("room", 10).unwrap().iter().map(|m| m.received).collect::<Vec<_>>(), vec![at(101), at(121)]);
        // A third archived message must survive rather than reusing a live twin.
        assert!(h.insert("room", "u", "ok", at(130), true, None).unwrap().is_some());
    }

    #[test]
    fn outbox_enqueue_load_remove()
    {
        let h = History::open_in_memory();
        let a = h.enqueue_outbox("room@c", true, "hello").unwrap();
        let b = h.enqueue_outbox("bob@x", false, "hi").unwrap();

        let items = h.load_outbox().unwrap();
        assert_eq!(items.len(), 2);
        // Oldest-first.
        assert_eq!(items[0].id, a);
        assert_eq!(items[0].is_room, true);
        assert_eq!(items[0].body, "hello");
        assert_eq!(items[1].id, b);
        assert_eq!(items[1].is_room, false);

        h.remove_outbox(a).unwrap();
        let items = h.load_outbox().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, b);
    }

    #[test]
    fn archive_boundaries_require_stored_archive_provenance_and_valid_time()
    {
        let h = History::open_in_memory();
        let live = h.insert("room", "alice", "live", at(0), false, Some("live-id")).unwrap().unwrap();
        let archived = h.insert("room", "alice", "archived", at(1), true, Some("archive-id")).unwrap().unwrap();
        let (queued, outgoing) = h.enqueue_direct_message("room", "me", "outgoing").unwrap();
        assert_eq!(h.archived_received("room", live).unwrap(), None);
        assert_eq!(h.archived_received("room", outgoing.id).unwrap(), None);
        assert_eq!(h.archived_received("room", archived).unwrap(), Some(at(1)));
        assert_eq!(h.archived_received("other", archived).unwrap(), None);
        assert_eq!(h.archived_received("room", i64::MAX).unwrap(), None);
        h.insert("room", "alice", "live", at(2), true, Some("live-id")).unwrap();
        assert_eq!(h.archived_received("room", live).unwrap(), Some(at(2)));
        h.reconcile_own_direct_copy("room", "outgoing", &queued.origin_id, at(3), None, true).unwrap().unwrap();
        assert_eq!(h.archived_received("room", outgoing.id).unwrap(), Some(at(3)));
        h.conn.execute("UPDATE messages SET ts = ?1 WHERE id = ?2", params![i64::MAX, archived]).unwrap();
        assert!(h.archived_received("room", archived).is_err());
    }

    #[test]
    fn search_scoped_and_global()
    {
        let h = History::open_in_memory();
        h.insert("roomA", "u", "the deploy is green", at(0), false, None).unwrap();
        h.insert("roomA", "u", "lunch?", at(1), false, None).unwrap();
        h.insert("roomB", "u", "deploy rollback done", at(2), false, None).unwrap();

        let scoped = h.search(Some("roomA"), "deploy", 100).unwrap();
        assert_eq!(scoped.len(), 1);

        let global = h.search(None, "deploy", 100).unwrap();
        assert_eq!(global.len(), 2);

        // Wildcards are treated literally.
        h.insert("roomA", "u", "100% sure", at(3), false, None).unwrap();
        assert_eq!(h.search(Some("roomA"), "100%", 100).unwrap().len(), 1);
        assert_eq!(h.search(Some("roomA"), "0% s", 100).unwrap().len(), 1);
    }
}
