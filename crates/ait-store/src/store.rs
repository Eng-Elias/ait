//! The [`Store`] facade — the crate's one seam (Spec R08 "Testing
//! Decisions"): a synchronous `rusqlite` connection to `store.db` plus
//! the whole CRUD surface. SQL lives here and only here; every query is
//! a small insert or a per-parent list — nothing grows beyond "load a
//! transcript" (no search, no analytics).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

use crate::path::store_path;
use crate::{
    Block, Conversation, NewBlock, NewConversation, NewTranscriptItem, StoreError, TerminalSession,
    TranscriptItem,
};

/// `PRAGMA busy_timeout` — a second `ait` instance (or a stale WAL
/// lock) stalls a writer for at most this long before `SQLITE_BUSY`
/// surfaces (Spec R08, user story 3: concurrent instances share the
/// database safely).
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Sequential schema migrations applied in order and tracked by
/// `PRAGMA user_version` (user story 2): entry `i` upgrades version
/// `i` → `i + 1`. New schema versions are appended to the end — a
/// landed entry is never edited; the DB schema is a one-way door.
const MIGRATIONS: &[&str] = &[
    // Version 0 → 1: the Round-8 schema (Spec R08 "Implementation
    // Decisions"). Times are milliseconds since the Unix epoch;
    // `transcript_items.kind`/`payload_json` and `plans.payload_json`
    // hold the caller's vocabulary and JSON verbatim.
    "CREATE TABLE conversations (
        id          INTEGER PRIMARY KEY,
        title       TEXT NOT NULL,
        created_at  INTEGER NOT NULL,
        updated_at  INTEGER NOT NULL,
        cwd         TEXT NOT NULL,
        status      TEXT NOT NULL
    );
    CREATE TABLE transcript_items (
        id              INTEGER PRIMARY KEY,
        conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        seq             INTEGER NOT NULL,
        kind            TEXT NOT NULL,
        payload_json    TEXT NOT NULL,
        UNIQUE (conversation_id, seq)
    );
    CREATE TABLE plans (
        conversation_id INTEGER PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
        payload_json    TEXT NOT NULL,
        updated_at      INTEGER NOT NULL
    );
    CREATE TABLE terminal_sessions (
        id         INTEGER PRIMARY KEY,
        shell      TEXT NOT NULL,
        cwd        TEXT NOT NULL,
        started_at INTEGER NOT NULL,
        ended_at   INTEGER
    );
    CREATE TABLE blocks (
        id          INTEGER PRIMARY KEY,
        session_id  INTEGER NOT NULL REFERENCES terminal_sessions(id) ON DELETE CASCADE,
        seq         INTEGER NOT NULL,
        command     TEXT NOT NULL,
        exit        INTEGER,
        duration_ms INTEGER,
        started_at  INTEGER,
        ended_at    INTEGER,
        UNIQUE (session_id, seq)
    );",
];

/// A synchronous facade over the `store.db` SQLite database (Spec R08):
/// conversations, their transcripts and plans, and per-session command-
/// block metadata. `Store` is `!Sync` (one `rusqlite::Connection`) but
/// `Send`, so the owning `ait` side can hold or move it freely.
///
/// Opening applies `journal_mode=WAL`, a [`BUSY_TIMEOUT`] busy timeout
/// and `foreign_keys=ON` (the cascade deletes rely on it), then runs
/// every pending [`MIGRATIONS`] entry under `PRAGMA user_version`.
pub struct Store {
    conn: Connection,
    path: PathBuf,
}

impl Store {
    /// Open the store at [`store_path`] — `$XDG_DATA_HOME/ait/store.db`,
    /// falling back to `~/.local/share/ait/store.db` — creating the
    /// directory on first use (user story 1).
    pub fn open_default() -> Result<Self, StoreError> {
        Self::open(store_path())
    }

    /// Open (creating if needed) the database file at `path`: the
    /// parent directory is created, pragmas applied and migrations run.
    /// `open_default` and tests-with-tempdirs both land here.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| StoreError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut conn = Connection::open(&path).map_err(open_err(&path))?;
        // Pragma failures share `Open`: a file we cannot put into WAL is
        // a file we could not usefully open.
        conn.busy_timeout(BUSY_TIMEOUT).map_err(open_err(&path))?;
        conn.query_row("PRAGMA journal_mode = WAL", [], |row| {
            row.get::<_, String>(0)
        })
        .map_err(open_err(&path))?;
        conn.execute_batch("PRAGMA foreign_keys = ON")
            .map_err(open_err(&path))?;
        migrate(&mut conn)?;
        Ok(Self { conn, path })
    }

    /// The database file this `Store` is bound to — for toasts and logs.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Insert a conversation row; `created_at`/`updated_at` are stamped
    /// now. Returns the stored row.
    pub fn create_conversation(
        &self,
        conversation: NewConversation<'_>,
    ) -> Result<Conversation, StoreError> {
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO conversations (title, created_at, updated_at, cwd, status)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                conversation.title,
                now,
                now,
                conversation.cwd,
                conversation.status
            ],
        )?;
        Ok(Conversation {
            id: self.conn.last_insert_rowid(),
            title: conversation.title.to_owned(),
            cwd: conversation.cwd.to_owned(),
            status: conversation.status.to_owned(),
            created_at: ms_to_system(now),
            updated_at: ms_to_system(now),
        })
    }

    /// Every conversation, newest-mutation first
    /// (`updated_at DESC, id DESC`) — the conversation picker's row
    /// order (Spec R08, user story 5).
    pub fn list_conversations(&self) -> Result<Vec<Conversation>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, created_at, updated_at, cwd, status
             FROM conversations ORDER BY updated_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Conversation {
                id: row.get(0)?,
                title: row.get(1)?,
                created_at: ms_to_system(row.get(2)?),
                updated_at: ms_to_system(row.get(3)?),
                cwd: row.get(4)?,
                status: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Update a conversation's lifecycle tag (and `updated_at`).
    pub fn set_conversation_status(&self, id: i64, status: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE conversations SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, status, now_ms()],
        )?;
        Ok(())
    }

    /// Delete a conversation; `transcript_items` and `plans` rows
    /// cascade away with it (the FKs were declared `ON DELETE CASCADE`
    /// and `foreign_keys=ON` is always set on this connection).
    pub fn delete_conversation(&self, id: i64) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM conversations WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Append transcript items to a conversation in one transaction:
    /// `seq` continues from the conversation's current maximum, and the
    /// conversation's `updated_at` bumps. An empty slice is a no-op;
    /// an unknown `conversation_id` is a foreign-key `Query` error.
    pub fn append_transcript_items(
        &self,
        conversation_id: i64,
        items: &[NewTranscriptItem<'_>],
    ) -> Result<(), StoreError> {
        if items.is_empty() {
            return Ok(());
        }
        // `unchecked_transaction` keeps this `&self`; `Store` never
        // nests transactions so the one-connection invariant holds.
        let tx = self.conn.unchecked_transaction()?;
        let next: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM transcript_items
             WHERE conversation_id = ?1",
            [conversation_id],
            |row| row.get(0),
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO transcript_items (conversation_id, seq, kind, payload_json)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (offset, item) in items.iter().enumerate() {
                stmt.execute(params![
                    conversation_id,
                    next + offset as i64,
                    item.kind,
                    item.payload_json
                ])?;
            }
        }
        tx.execute(
            "UPDATE conversations SET updated_at = ?2 WHERE id = ?1",
            params![conversation_id, now_ms()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Every transcript item of a conversation in `seq` order — the
    /// whole of "load a transcript", the largest query this crate runs.
    /// A conversation with no items (or an unknown id) loads empty.
    pub fn load_transcript(&self, conversation_id: i64) -> Result<Vec<TranscriptItem>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, kind, payload_json FROM transcript_items
             WHERE conversation_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map([conversation_id], |row| {
            Ok(TranscriptItem {
                seq: row.get(0)?,
                kind: row.get(1)?,
                payload_json: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Store the latest plan snapshot for a conversation — `plans` is
    /// keyed by `conversation_id`, so an upsert keeps exactly one row
    /// (the newest) and bumps both `plans.updated_at` and the
    /// conversation's `updated_at`.
    pub fn upsert_plan(&self, conversation_id: i64, payload_json: &str) -> Result<(), StoreError> {
        let now = now_ms();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO plans (conversation_id, payload_json, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (conversation_id)
             DO UPDATE SET payload_json = ?2, updated_at = ?3",
            params![conversation_id, payload_json, now],
        )?;
        tx.execute(
            "UPDATE conversations SET updated_at = ?2 WHERE id = ?1",
            params![conversation_id, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The latest stored plan snapshot for a conversation, verbatim —
    /// `None` when no `update_plan` ever landed (or the conversation
    /// was deleted).
    pub fn load_plan(&self, conversation_id: i64) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row(
                "SELECT payload_json FROM plans WHERE conversation_id = ?1",
                [conversation_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Insert a `terminal_sessions` row for a freshly spawned tab;
    /// `started_at` is stamped now, `ended_at` stays `NULL` until
    /// [`finish_terminal_session`](Self::finish_terminal_session).
    pub fn insert_terminal_session(
        &self,
        shell: &str,
        cwd: &str,
    ) -> Result<TerminalSession, StoreError> {
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO terminal_sessions (shell, cwd, started_at)
             VALUES (?1, ?2, ?3)",
            params![shell, cwd, now],
        )?;
        Ok(TerminalSession {
            id: self.conn.last_insert_rowid(),
            shell: shell.to_owned(),
            cwd: cwd.to_owned(),
            started_at: ms_to_system(now),
            ended_at: None,
        })
    }

    /// The stored `terminal_sessions` row — `None` for an unknown id.
    pub fn terminal_session(&self, id: i64) -> Result<Option<TerminalSession>, StoreError> {
        self.conn
            .query_row(
                "SELECT id, shell, cwd, started_at, ended_at
                 FROM terminal_sessions WHERE id = ?1",
                [id],
                |row| {
                    Ok(TerminalSession {
                        id: row.get(0)?,
                        shell: row.get(1)?,
                        cwd: row.get(2)?,
                        started_at: ms_to_system(row.get(3)?),
                        ended_at: row.get::<_, Option<i64>>(4)?.map(ms_to_system),
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Every `terminal_sessions` row, oldest first — symmetric with
    /// [`list_blocks`](Self::list_blocks), the read side that lets
    /// tests observe session persistence.
    pub fn list_terminal_sessions(&self) -> Result<Vec<TerminalSession>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, shell, cwd, started_at, ended_at
             FROM terminal_sessions ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(TerminalSession {
                id: row.get(0)?,
                shell: row.get(1)?,
                cwd: row.get(2)?,
                started_at: ms_to_system(row.get(3)?),
                ended_at: row.get::<_, Option<i64>>(4)?.map(ms_to_system),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Stamp `ended_at` on a terminal session (tab close/exit) and
    /// return the updated row — `None` if the id never existed.
    pub fn finish_terminal_session(&self, id: i64) -> Result<Option<TerminalSession>, StoreError> {
        self.conn.execute(
            "UPDATE terminal_sessions SET ended_at = ?2 WHERE id = ?1",
            params![id, now_ms()],
        )?;
        self.terminal_session(id)
    }

    /// Insert a sealed block's metadata against its session; `seq`
    /// continues from the session's current maximum. Returns the stored
    /// row. An unknown `session_id` is a foreign-key `Query` error.
    pub fn insert_block(&self, block: NewBlock) -> Result<Block, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM blocks WHERE session_id = ?1",
            [block.session_id],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO blocks
                 (session_id, seq, command, exit, duration_ms, started_at, ended_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                block.session_id,
                seq,
                block.command,
                block.exit,
                block.duration.map(duration_ms),
                block.started_at.map(system_to_ms),
                block.ended_at.map(system_to_ms),
            ],
        )?;
        tx.commit()?;
        Ok(Block {
            seq,
            session_id: block.session_id,
            command: block.command,
            exit: block.exit,
            duration: block.duration,
            started_at: block.started_at,
            ended_at: block.ended_at,
        })
    }

    /// Insert a drain's worth of sealed blocks in ONE transaction
    /// (Spec R08 — "writes batched per drain in one transaction"): a
    /// batch may span several sessions — each `seq` continues against
    /// its own `session_id` — and a single failure rolls the whole
    /// batch back. An empty slice is a no-op.
    pub fn insert_blocks(&self, blocks: &[NewBlock]) -> Result<(), StoreError> {
        if blocks.is_empty() {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        let mut next_seq =
            tx.prepare("SELECT COALESCE(MAX(seq) + 1, 0) FROM blocks WHERE session_id = ?1")?;
        let mut insert = tx.prepare(
            "INSERT INTO blocks
                 (session_id, seq, command, exit, duration_ms, started_at, ended_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        // `seq` continues per session across the batch: the first row
        // of each session queries its max, later rows increment it.
        let mut seqs: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        for block in blocks {
            let seq = match seqs.get_mut(&block.session_id) {
                Some(seq) => {
                    *seq += 1;
                    *seq
                }
                None => {
                    let seq: i64 = next_seq.query_row([block.session_id], |row| row.get(0))?;
                    seqs.insert(block.session_id, seq);
                    seq
                }
            };
            insert.execute(params![
                block.session_id,
                seq,
                block.command,
                block.exit,
                block.duration.map(duration_ms),
                block.started_at.map(system_to_ms),
                block.ended_at.map(system_to_ms),
            ])?;
        }
        drop(insert);
        drop(next_seq);
        tx.commit()?;
        Ok(())
    }

    /// Every stored block of a session in `seq` order — symmetric with
    /// [`load_transcript`](Self::load_transcript), and the read side
    /// that lets tests observe block persistence.
    pub fn list_blocks(&self, session_id: i64) -> Result<Vec<Block>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, session_id, command, exit, duration_ms, started_at, ended_at
             FROM blocks WHERE session_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map([session_id], |row| {
            Ok(Block {
                seq: row.get(0)?,
                session_id: row.get(1)?,
                command: row.get(2)?,
                exit: row.get(3)?,
                duration: row.get::<_, Option<i64>>(4)?.map(ms_to_duration),
                started_at: row.get::<_, Option<i64>>(5)?.map(ms_to_system),
                ended_at: row.get::<_, Option<i64>>(6)?.map(ms_to_system),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

/// Wrap an `open`-stage failure in [`StoreError::Open`] naming `path`.
fn open_err(path: &Path) -> impl Fn(rusqlite::Error) -> StoreError + '_ {
    move |source| StoreError::Open {
        path: path.to_path_buf(),
        source,
    }
}

/// Apply every pending [`MIGRATIONS`] entry, each in its own
/// transaction, and stamp `PRAGMA user_version` to match. Idempotent:
/// a database at the current version runs none.
fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let version: u32 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(StoreError::Query)?;
    for (index, ddl) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let target = index as u32 + 1;
        let tx = conn.transaction().map_err(|source| StoreError::Migrate {
            version: target,
            source,
        })?;
        tx.execute_batch(ddl)
            .and_then(|()| tx.pragma_update(None, "user_version", target))
            .map_err(|source| StoreError::Migrate {
                version: target,
                source,
            })?;
        tx.commit().map_err(|source| StoreError::Migrate {
            version: target,
            source,
        })?;
    }
    Ok(())
}

/// Milliseconds since the Unix epoch — the on-disk time representation.
fn now_ms() -> i64 {
    system_to_ms(SystemTime::now())
}

/// `SystemTime` → epoch milliseconds (saturating; pre-epoch times go
/// negative rather than wrapping).
fn system_to_ms(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        Err(e) => i64::try_from(e.duration().as_millis())
            .map(|ms| -ms)
            .unwrap_or(i64::MIN),
    }
}

/// Epoch milliseconds → `SystemTime`.
fn ms_to_system(ms: i64) -> SystemTime {
    if ms >= 0 {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        UNIX_EPOCH - Duration::from_millis(ms.unsigned_abs())
    }
}

/// `Duration` → whole milliseconds.
fn duration_ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// Milliseconds → `Duration`.
fn ms_to_duration(ms: i64) -> Duration {
    Duration::from_millis(ms.max(0) as u64)
}
