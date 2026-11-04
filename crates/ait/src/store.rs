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
use ait_store::{NewBlock, NewConversation, NewTranscriptItem, Store, StoreError};

use crate::blocks::PersistedBlock;
use crate::status::Toast;

/// The frontend-owned store handle. `inner: None` is an unpersisted
/// run — the terminal works exactly as before and a toast already
/// said why; it is reached on open failure, on the first write
/// failure, and in every test that never needed a database.
pub(crate) struct StoreHandle {
    inner: Option<Store>,
}

impl StoreHandle {
    /// Open the default store (`$XDG_DATA_HOME/ait/store.db`); `Err`
    /// carries the failure for the caller's toast.
    pub fn open() -> Result<Self, StoreError> {
        Store::open_default().map(|store| Self { inner: Some(store) })
    }

    /// An explicitly unpersisted run — what an `open` failure (and the
    /// test `Harness`) degrades to.
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// Wrap a caller-opened `Store` — the seam tests outside this
    /// module inject a tempdir database through instead of touching
    /// the real XDG path.
    #[cfg(test)]
    pub(crate) fn from_store(store: Store) -> Self {
        Self { inner: Some(store) }
    }

    /// One write through the degrade rule: a success returns `Some`,
    /// a disabled handle returns `None`, and the FIRST error turns
    /// into a toast and disables the handle — subsequent calls no-op
    /// silently instead of re-toasting every drain.
    fn write<T>(
        &mut self,
        toast: &mut Option<Toast>,
        f: impl FnOnce(&Store) -> Result<T, StoreError>,
    ) -> Option<T> {
        match self.inner.as_ref().map(f) {
            Some(Ok(value)) => Some(value),
            Some(Err(error)) => {
                *toast = Some(Toast::new(
                    format!("store write failed: {error}"),
                    Instant::now(),
                ));
                self.inner = None;
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
    /// same row. `status` opens as `"open"`: the picker/resume
    /// lifecycle vocabulary is the caller's to own, and nothing
    /// closes a conversation yet.
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
                status: "open",
            })
        })
        .map(|conversation| conversation.id)
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
        (dir, StoreHandle { inner: Some(store) })
    }

    fn block(command: &str) -> PersistedBlock {
        PersistedBlock {
            command: command.to_owned(),
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
        assert_eq!(blocks[0].command, "ls");
        assert_eq!(blocks[0].exit, Some(0));
    }

    /// A write error toasts ONCE and disables: the failed batch is
    /// dropped (never retried into a toast storm) and every later
    /// call is a silent no-op — the unpersisted run's contract.
    #[test]
    fn a_write_failure_toasts_once_and_disables() {
        let (_dir, mut handle) = handle();
        let mut toast = None;

        // A dangling session id is a foreign-key error — a write
        // failure without needing a broken disk.
        handle.insert_blocks(&mut toast, &[(999, block("x"))]);

        assert!(
            toast
                .as_ref()
                .is_some_and(|t| t.text().contains("store write failed")),
            "the failure surfaced as a toast"
        );
        assert!(handle.inner.is_none(), "the run continues unpersisted");

        toast.take();
        assert!(handle
            .session_started(&mut toast, "sh", Path::new("/w"))
            .is_none());
        handle.insert_blocks(&mut toast, &[(1, block("y"))]);
        assert!(toast.is_none(), "a disabled handle never re-toasts");
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
        // the handle goes silent (user story 25).
        handle.plan_upsert(&mut toast, 999, &first);
        assert!(
            toast
                .as_ref()
                .is_some_and(|t| t.text().contains("store write failed")),
            "the failure surfaced as a toast"
        );
        assert!(handle.inner.is_none(), "the run continues unpersisted");
    }

    /// A transcript append to a dangling conversation id is a
    /// foreign-key error — it degrades through the same toast-once-
    /// and-disable rule every other write obeys (user story 25).
    #[test]
    fn an_append_failure_toasts_once_and_disables() {
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
        assert!(handle.inner.is_none(), "the run continues unpersisted");

        toast.take();
        assert!(handle
            .conversation_started(&mut toast, "t", Path::new("/w"))
            .is_none());
        assert!(toast.is_none(), "a disabled handle never re-toasts");
    }
}
