//! The `ait` binary's hold on `ait-store` (Spec R08): opened once at
//! startup, every failure degrades to a toast plus an unpersisted run —
//! never a crash, never on the render path (user story 25; the
//! terminal stays a terminal first).
//!
//! [`StoreHandle`] is deliberately thin: the store knows SQL, the
//! tabs know what to persist, and this handle owns the degrade rule —
//! the first write error toasts and disables the handle so a broken
//! database can't toast-spam the status line every 8 ms tick.

use std::path::Path;
use std::time::Instant;

use ait_agent::TranscriptItem;
use ait_store::{
    ListedConversation, NewBlock, NewConversation, NewTranscriptItem, Store, StoreError,
};

use crate::blocks::PersistedBlock;
use crate::status::Toast;

/// The `conversations.status` vocabulary this handle owns (Spec
/// R08): `"open"` while a tab is bound to the row, `"closed"` when
/// the owning tab closed or the app exited — the same boundaries
/// `terminal_sessions.ended_at` marks. A closed row still lists and
/// resumes; resume re-opens it.
const STATUS_OPEN: &str = "open";
const STATUS_CLOSED: &str = "closed";

/// The frontend-owned store handle. `inner: None` is an unpersisted
/// run — the terminal works exactly as before and a toast already
/// said why; it is reached on open failure and in every test that
/// never needed a database. A WRITE failure degrades only the writes:
/// the store stays live for reads (user story 25's unpersisted-run
/// contract is about persistence, not about blinding the picker —
/// a transient write error must not empty `conversations()` forever).
pub(crate) struct StoreHandle {
    inner: Option<Store>,
    /// Whether writes still attempt — the first write error flips
    /// this off after one toast so a broken database can't toast-spam
    /// the status line every 8 ms tick. Reads never consult it.
    writes_enabled: bool,
}

impl StoreHandle {
    /// Open the default store (`$XDG_DATA_HOME/ait/store.db`); `Err`
    /// carries the failure for the caller's toast.
    pub fn open() -> Result<Self, StoreError> {
        Store::open_default().map(|store| Self {
            inner: Some(store),
            writes_enabled: true,
        })
    }

    /// An explicitly unpersisted run — what an `open` failure (and the
    /// test `Harness`) degrades to.
    pub fn disabled() -> Self {
        Self {
            inner: None,
            writes_enabled: false,
        }
    }

    /// Wrap a caller-opened `Store` — the seam tests outside this
    /// module inject a tempdir database through instead of touching
    /// the real XDG path.
    #[cfg(test)]
    pub(crate) fn from_store(store: Store) -> Self {
        Self {
            inner: Some(store),
            writes_enabled: true,
        }
    }

    /// One write through the degrade rule: a success returns `Some`,
    /// a disabled handle returns `None`, and the FIRST error turns
    /// into a toast and disables WRITES — subsequent calls no-op
    /// silently instead of re-toasting every drain, while reads keep
    /// the store alive underneath.
    fn write<T>(
        &mut self,
        toast: &mut Option<Toast>,
        f: impl FnOnce(&Store) -> Result<T, StoreError>,
    ) -> Option<T> {
        if !self.writes_enabled {
            return None;
        }
        match self.inner.as_ref().map(f) {
            Some(Ok(value)) => Some(value),
            Some(Err(error)) => {
                *toast = Some(Toast::new(
                    format!("store write failed: {error}"),
                    Instant::now(),
                ));
                self.writes_enabled = false;
                None
            }
            None => None,
        }
    }

    /// The `terminal_sessions` row for a freshly spawned tab; the
    /// caller binds the returned id to the tab (`Tab::set_store_session`).
    pub fn session_started(
        &mut self,
        toast: &mut Option<Toast>,
        shell: &str,
        cwd: &Path,
    ) -> Option<i64> {
        self.write(toast, |store| {
            store.insert_terminal_session(shell, &cwd.to_string_lossy())
        })
        .map(|session| session.id)
    }

    /// Stamp `ended_at` on a tab's session row — tab close and app
    /// exit both land here (Spec R08).
    pub fn session_ended(&mut self, toast: &mut Option<Toast>, id: i64) {
        self.write(toast, |store| store.finish_terminal_session(id).map(|_| ()));
    }

    /// The `conversations` row a tab's first task creates (Spec
    /// R08-T3) — the caller binds the returned id to the tab
    /// (`Tab::set_conversation`) so every later task appends to the
    /// same row. `status` opens as `"open"` — the handle owns the
    /// lifecycle vocabulary: `"open"` while a tab is bound,
    /// [`conversation_closed`](Self::conversation_closed) /
    /// [`conversation_reopened`](Self::conversation_reopened)
    /// transition it after.
    pub fn conversation_started(
        &mut self,
        toast: &mut Option<Toast>,
        title: &str,
        cwd: &Path,
    ) -> Option<i64> {
        self.write(toast, |store| {
            store.create_conversation(NewConversation {
                title,
                cwd: &cwd.to_string_lossy(),
                status: STATUS_OPEN,
            })
        })
        .map(|conversation| conversation.id)
    }

    /// `status` → `"closed"` — the owning tab closed or the app
    /// exited (the same places `terminal_sessions.ended_at` gets
    /// stamped). A closed row can still list and resume.
    pub fn conversation_closed(&mut self, toast: &mut Option<Toast>, id: i64) {
        self.write(toast, |store| {
            store.set_conversation_status(id, STATUS_CLOSED)
        });
    }

    /// `status` → `"open"` — a resume re-bound the row to a live tab.
    pub fn conversation_reopened(&mut self, toast: &mut Option<Toast>, id: i64) {
        self.write(toast, |store| {
            store.set_conversation_status(id, STATUS_OPEN)
        });
    }

    /// One drain's worth of [`TranscriptItem`]s appended in the
    /// store's single transaction (Spec R08 — "writes batched per
    /// drain in one transaction"): each item serializes to the
    /// `(kind, payload_json)` pair the row stores — the payload is
    /// the item's whole serde form, opaque to the store.
    pub fn append_transcript(
        &mut self,
        toast: &mut Option<Toast>,
        conversation_id: i64,
        items: &[TranscriptItem],
    ) {
        if items.is_empty() {
            return;
        }
        // Serialization cannot fail — every field is plain data —
        // but an item that somehow didn't serialize is dropped here
        // rather than panicking on the degrade seam.
        let serialized: Vec<(String, String)> = items
            .iter()
            .filter_map(|item| {
                serde_json::to_string(item)
                    .ok()
                    .map(|payload| (item.kind().to_owned(), payload))
            })
            .collect();
        let rows: Vec<NewTranscriptItem> = serialized
            .iter()
            .map(|(kind, payload_json)| NewTranscriptItem { kind, payload_json })
            .collect();
        self.write(toast, |store| {
            store.append_transcript_items(conversation_id, &rows)
        });
    }

    /// The latest `update_plan` snapshot for a conversation (Spec
    /// R08-T5, us-17): `plans` keeps one row — each call's snapshot
    /// replaces it — so `load_plan` always reads the newest. The
    /// item serializes exactly like a `transcript_items` payload:
    /// its whole serde form, opaque to the store.
    pub fn plan_upsert(
        &mut self,
        toast: &mut Option<Toast>,
        conversation_id: i64,
        item: &TranscriptItem,
    ) {
        debug_assert!(
            matches!(item, TranscriptItem::Plan { .. }),
            "plan_upsert stores plan items only"
        );
        // Serialization cannot fail — the item is plain data — but
        // a payload that somehow didn't serialize is dropped here
        // rather than panicking on the degrade seam.
        let Ok(payload) = serde_json::to_string(item) else {
            return;
        };
        self.write(toast, |store| store.upsert_plan(conversation_id, &payload));
    }

    /// The `Alt+R` picker's rows (Spec R08-T6, us-5): every
    /// conversation newest-mutation first with its item count. A
    /// read — `Err` is the caller's to toast WITHOUT disabling the
    /// handle (only writes trip the degrade rule); a disabled handle
    /// lists nothing, which the caller reads as "no stored
    /// conversations".
    pub fn conversations(&self) -> Result<Vec<ListedConversation>, StoreError> {
        match &self.inner {
            Some(store) => store.list_conversations_with_counts(),
            None => Ok(Vec::new()),
        }
    }

    /// One conversation's transcript items, deserialized — resume's
    /// rebuild source for the panel scrollback and the next task's
    /// `prior` alike (Spec R08-T6). Payloads that fail to
    /// deserialize (the DB is local, plaintext and user-editable)
    /// drop out rather than failing the whole resume — the verbatim
    /// contract holds for everything the store wrote.
    pub fn load_transcript(&self, id: i64) -> Result<Vec<TranscriptItem>, StoreError> {
        match &self.inner {
            Some(store) => Ok(store
                .load_transcript(id)?
                .iter()
                .filter_map(|row| serde_json::from_str(&row.payload_json).ok())
                .collect()),
            None => Ok(Vec::new()),
        }
    }

    /// The conversation's latest `plans` snapshot, deserialized —
    /// the pinned block's restore source (Spec R08-T6, us-17). Same
    /// read-and-skip rule as [`load_transcript`](Self::load_transcript).
    pub fn load_plan(&self, id: i64) -> Result<Option<TranscriptItem>, StoreError> {
        match &self.inner {
            Some(store) => Ok(store
                .load_plan(id)?
                .and_then(|payload| serde_json::from_str(&payload).ok())),
            None => Ok(None),
        }
    }

    /// Delete a conversation — its items and plan cascade (Spec
    /// R08-T6, us-11). A write, so a failure toasts once and
    /// disables the handle like every other; `true` only when the
    /// delete actually landed.
    pub fn delete_conversation(&mut self, toast: &mut Option<Toast>, id: i64) -> bool {
        self.write(toast, |store| store.delete_conversation(id))
            .is_some()
    }

    /// The per-drain batch: every `(session id, sealed block)` pair in
    /// ONE transaction (Spec R08 — "writes batched per drain"). An
    /// empty batch costs nothing.
    pub fn insert_blocks(&mut self, toast: &mut Option<Toast>, rows: &[(i64, PersistedBlock)]) {
        if rows.is_empty() {
            return;
        }
        let blocks: Vec<NewBlock> = rows
            .iter()
            .map(|(session_id, block)| NewBlock {
                session_id: *session_id,
                command: block.command.clone(),
                cwd: Some(block.cwd.clone()),
                host: block.host.clone(),
                exit: block.exit,
                duration: block.duration,
                started_at: block.started_at,
                ended_at: block.ended_at,
            })
            .collect();
        self.write(toast, |store| store.insert_blocks(&blocks));
    }
}

#[cfg(test)]
mod tests {
    //! The degrade contract (Spec R08 user story 25): rows land when
    //! the store is live, and a failure produces exactly one toast and
    //! an unpersisted — but fully functional — handle.

    use super::*;
    use std::time::{Duration, SystemTime};

    /// A handle on a tempdir store — `open_default` stays untested
    /// here (ait-store's own tests cover the XDG path).
    fn handle() -> (tempfile::TempDir, StoreHandle) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("ait").join("store.db")).unwrap();
        (
            dir,
            StoreHandle {
                inner: Some(store),
                writes_enabled: true,
            },
        )
    }

    fn block(command: &str) -> PersistedBlock {
        PersistedBlock {
            command: Some(command.to_owned()),
            cwd: "/w".to_owned(),
            host: None,
            exit: Some(0),
            duration: Some(Duration::from_millis(4)),
            started_at: Some(SystemTime::now()),
            ended_at: Some(SystemTime::now()),
        }
    }

    /// Spawn → sealed blocks → close: the whole row lifecycle through
    /// the handle, verified against the store itself.
    #[test]
    fn the_row_lifecycle_round_trips() {
        let (_dir, mut handle) = handle();
        let mut toast = None;

        let id = handle
            .session_started(&mut toast, "sh", Path::new("/w"))
            .expect("the session insert lands");
        handle.insert_blocks(&mut toast, &[(id, block("ls"))]);
        handle.session_ended(&mut toast, id);

        assert!(toast.is_none(), "a healthy store never toasts");
        let store = handle.inner.as_ref().unwrap();
        let session = store.terminal_session(id).unwrap().unwrap();
        assert_eq!(session.shell, "sh");
        assert_eq!(session.cwd, "/w");
        assert!(session.ended_at.is_some(), "ended_at stamped");
        let blocks = store.list_blocks(id).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].command.as_deref(), Some("ls"));
        assert_eq!(blocks[0].cwd.as_deref(), Some("/w"));
        assert_eq!(blocks[0].exit, Some(0));
    }

    /// A write error toasts ONCE and disables WRITES: the failed batch
    /// is dropped (never retried into a toast storm) and every later
    /// write is a silent no-op — but the store stays live for reads,
    /// so a transient failure never blinds the picker or a resume.
    #[test]
    fn a_write_failure_toasts_once_disables_writes_and_keeps_reads() {
        let (_dir, mut handle) = handle();
        let mut toast = None;
        let conversation = handle
            .conversation_started(&mut toast, "fix the tests", Path::new("/w"))
            .expect("the conversation insert lands");

        // A dangling session id is a foreign-key error — a write
        // failure without needing a broken disk.
        handle.insert_blocks(&mut toast, &[(999, block("x"))]);

        assert!(
            toast
                .as_ref()
                .is_some_and(|t| t.text().contains("store write failed")),
            "the failure surfaced as a toast"
        );
        assert!(!handle.writes_enabled, "writes degrade…");
        assert!(handle.inner.is_some(), "…but the store stays live");

        // Reads survive the degraded write path — the picker still
        // lists what already landed.
        assert_eq!(handle.conversations().expect("reads keep working").len(), 1);
        assert_eq!(
            handle.load_transcript(conversation).unwrap().len(),
            0,
            "an empty transcript still loads empty"
        );

        toast.take();
        assert!(handle
            .session_started(&mut toast, "sh", Path::new("/w"))
            .is_none());
        handle.insert_blocks(&mut toast, &[(1, block("y"))]);
        assert!(toast.is_none(), "degraded writes never re-toast");
    }

    /// The `None` state itself: a disabled handle answers `None`,
    /// writes nothing, and toasts nothing.
    #[test]
    fn disabled_is_a_silent_noop() {
        let mut handle = StoreHandle::disabled();
        let mut toast = None;
        assert!(handle
            .session_started(&mut toast, "sh", Path::new("/w"))
            .is_none());
        handle.session_ended(&mut toast, 7);
        handle.insert_blocks(&mut toast, &[(1, block("z"))]);
        assert!(toast.is_none());
    }

    /// Spec R08-T3: a conversation row plus its transcript append —
    /// the `(kind, payload_json)` pairs land in `seq` order and every
    /// payload deserializes back into its item.
    #[test]
    fn conversation_rows_and_items_round_trip() {
        let (_dir, mut handle) = handle();
        let mut toast = None;

        let id = handle
            .conversation_started(&mut toast, "fix the tests", Path::new("/w"))
            .expect("the conversation insert lands");
        handle.append_transcript(
            &mut toast,
            id,
            &[
                TranscriptItem::Task {
                    text: "fix the tests".into(),
                },
                TranscriptItem::AssistantText {
                    text: "on it".into(),
                },
                TranscriptItem::Status {
                    text: "done".into(),
                },
            ],
        );
        assert!(toast.is_none(), "a healthy store never toasts");

        let store = handle.inner.as_ref().unwrap();
        let row = store
            .list_conversations()
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(row.id, id);
        assert_eq!(row.title, "fix the tests");
        assert_eq!(row.cwd, "/w");
        assert_eq!(row.status, "open");
        let items = store.load_transcript(id).unwrap();
        assert_eq!(
            items.iter().map(|item| item.seq).collect::<Vec<_>>(),
            [0, 1, 2],
            "seqs assigned contiguously in append order"
        );
        for (item, kind) in items.iter().zip(["task", "assistant_text", "status"]) {
            assert_eq!(item.kind, kind);
        }
        // The payload is the item's whole serde form — it reads back
        // into the same TranscriptItem (T4's verbatim contract).
        let back: TranscriptItem =
            serde_json::from_str(&items[0].payload_json).expect("payload deserializes");
        assert_eq!(
            back,
            TranscriptItem::Task {
                text: "fix the tests".into()
            }
        );
    }

    /// The `status` lifecycle (Spec R08): a row opens `"open"`,
    /// closes with its owning tab (or the app) and re-opens on
    /// resume — all through the handle's verbs.
    #[test]
    fn the_status_lifecycle_opens_closes_and_reopens() {
        let (_dir, mut handle) = handle();
        let mut toast = None;
        let id = handle
            .conversation_started(&mut toast, "lifecycle", Path::new("/w"))
            .expect("the conversation insert lands");

        let status = |handle: &StoreHandle| {
            handle
                .conversations()
                .unwrap()
                .iter()
                .find(|row| row.conversation.id == id)
                .map(|row| row.conversation.status.clone())
        };
        assert_eq!(status(&handle).as_deref(), Some("open"));
        handle.conversation_closed(&mut toast, id);
        assert_eq!(status(&handle).as_deref(), Some("closed"));
        handle.conversation_reopened(&mut toast, id);
        assert_eq!(status(&handle).as_deref(), Some("open"));
        assert!(toast.is_none(), "a healthy store never toasts");
    }

    /// Spec R08-T5/us-17: each `update_plan` snapshot replaces the
    /// `plans` row — the row count stays one and `load_plan` reads
    /// back the newest payload verbatim (the item's whole serde
    /// form, like a `transcript_items` payload). A dangling
    /// conversation id degrades through the same toast-once rule.
    #[test]
    fn plan_upserts_keep_only_the_latest_snapshot() {
        let (_dir, mut handle) = handle();
        let mut toast = None;
        let id = handle
            .conversation_started(&mut toast, "plan the work", Path::new("/w"))
            .expect("the conversation insert lands");

        let first = TranscriptItem::Plan {
            todos: vec![ait_agent::PlanTodo {
                content: "reproduce".into(),
                status: ait_agent::PlanTodoStatus::InProgress,
            }],
        };
        let second = TranscriptItem::Plan {
            todos: vec![ait_agent::PlanTodo {
                content: "reproduce".into(),
                status: ait_agent::PlanTodoStatus::Done,
            }],
        };
        handle.plan_upsert(&mut toast, id, &first);
        handle.plan_upsert(&mut toast, id, &second);
        assert!(toast.is_none(), "a healthy store never toasts");

        let store = handle.inner.as_ref().unwrap();
        let payload = store.load_plan(id).unwrap().expect("a plans row");
        // The newest snapshot, verbatim — and it deserializes back
        // into the plan item (the T6 restore contract).
        let back: TranscriptItem = serde_json::from_str(&payload).expect("payload");
        assert_eq!(back, second);

        // A dangling id is a foreign-key error — toast once, then
        // writes go silent (user story 25); reads stay live.
        handle.plan_upsert(&mut toast, 999, &first);
        assert!(
            toast
                .as_ref()
                .is_some_and(|t| t.text().contains("store write failed")),
            "the failure surfaced as a toast"
        );
        assert!(!handle.writes_enabled, "writes are degraded");
        assert!(handle.inner.is_some(), "reads keep the store");
    }

    /// A transcript append to a dangling conversation id is a
    /// foreign-key error — it degrades through the same toast-once-
    /// and-disable-writes rule every other write obeys (user story 25).
    #[test]
    fn an_append_failure_toasts_once_and_disables_writes() {
        let (_dir, mut handle) = handle();
        let mut toast = None;

        handle.append_transcript(
            &mut toast,
            999,
            &[TranscriptItem::Status { text: "x".into() }],
        );

        assert!(
            toast
                .as_ref()
                .is_some_and(|t| t.text().contains("store write failed")),
            "the failure surfaced as a toast"
        );
        assert!(!handle.writes_enabled, "writes are degraded");
        assert!(handle.inner.is_some(), "reads keep the store");

        toast.take();
        assert!(handle
            .conversation_started(&mut toast, "t", Path::new("/w"))
            .is_none());
        assert!(toast.is_none(), "degraded writes never re-toast");
    }

    /// Spec R08-T6/us-5: `conversations` returns the picker's rows —
    /// newest first with item counts — through the handle, and a
    /// disabled handle lists nothing rather than erroring.
    #[test]
    fn conversations_lists_the_pickers_rows() {
        let (_dir, mut handle) = handle();
        let mut toast = None;
        let id = handle
            .conversation_started(&mut toast, "fix the tests", Path::new("/w"))
            .expect("the conversation insert lands");
        handle.append_transcript(
            &mut toast,
            id,
            &[
                TranscriptItem::Task {
                    text: "fix the tests".into(),
                },
                TranscriptItem::Status {
                    text: "done".into(),
                },
            ],
        );

        let listed = handle.conversations().expect("the list lands");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].conversation.id, id);
        assert_eq!(listed[0].conversation.title, "fix the tests");
        assert_eq!(listed[0].items, 2);
        assert!(StoreHandle::disabled()
            .conversations()
            .expect("a disabled handle still answers")
            .is_empty());
    }

    /// Spec R08-T6: `load_transcript` returns the stored items
    /// deserialized in `seq` order — a payload that doesn't read back
    /// (a hand-edited row) drops out rather than failing the resume —
    /// and `load_plan` reads the latest snapshot the same way.
    #[test]
    fn load_transcript_and_plan_deserialize_the_stored_payloads() {
        let (_dir, mut handle) = handle();
        let mut toast = None;
        let id = handle
            .conversation_started(&mut toast, "plan the work", Path::new("/w"))
            .expect("the conversation insert lands");
        handle.append_transcript(
            &mut toast,
            id,
            &[
                TranscriptItem::Task {
                    text: "plan the work".into(),
                },
                TranscriptItem::AssistantText {
                    text: "on it".into(),
                },
            ],
        );
        let plan = TranscriptItem::Plan {
            todos: vec![ait_agent::PlanTodo {
                content: "fix".into(),
                status: ait_agent::PlanTodoStatus::InProgress,
            }],
        };
        handle.plan_upsert(&mut toast, id, &plan);

        assert_eq!(
            handle.load_transcript(id).expect("the transcript loads"),
            vec![
                TranscriptItem::Task {
                    text: "plan the work".into(),
                },
                TranscriptItem::AssistantText {
                    text: "on it".into(),
                },
            ]
        );
        assert_eq!(handle.load_plan(id).expect("the plan loads"), Some(plan));

        // A payload that doesn't deserialize drops out — the resume
        // still lands with everything legible.
        let store = handle.inner.as_ref().unwrap();
        store
            .append_transcript_items(
                id,
                &[ait_store::NewTranscriptItem {
                    kind: "bogus",
                    payload_json: "not json",
                }],
            )
            .unwrap();
        assert_eq!(
            handle.load_transcript(id).unwrap().len(),
            2,
            "the corrupt row drops rather than failing the load"
        );

        // Unknown ids read as empty, like the store itself.
        assert!(handle.load_transcript(999).unwrap().is_empty());
        assert!(handle.load_plan(999).unwrap().is_none());
    }

    /// Spec R08-T6/us-11: `delete_conversation` removes the row and
    /// cascades its items and plan — `true` on the landing write —
    /// while a disabled handle is a silent `false`.
    #[test]
    fn delete_conversation_cascades_items_and_plan() {
        let (_dir, mut handle) = handle();
        let mut toast = None;
        let id = handle
            .conversation_started(&mut toast, "to delete", Path::new("/w"))
            .expect("the conversation insert lands");
        handle.append_transcript(
            &mut toast,
            id,
            &[TranscriptItem::Task {
                text: "to delete".into(),
            }],
        );
        handle.plan_upsert(&mut toast, id, &TranscriptItem::Plan { todos: vec![] });

        assert!(handle.delete_conversation(&mut toast, id));
        assert!(toast.is_none(), "a healthy store never toasts");
        let store = handle.inner.as_ref().unwrap();
        assert!(store.list_conversations().unwrap().is_empty());
        assert!(store.load_transcript(id).unwrap().is_empty());
        assert!(store.load_plan(id).unwrap().is_none());

        // A disabled handle is the silent no-op (us-25).
        assert!(!StoreHandle::disabled().delete_conversation(&mut toast, id));
        assert!(toast.is_none());
    }
}
