//! `Store` facade tests (Spec R08 "Testing Decisions"): the pure seam
//! against a tempdir database — migrations from scratch, the
//! conversation create→append→load round-trip, list ordering, delete
//! cascades, plan upsert, and cross-`Store` persistence under WAL.
//! Everything here goes through the public API; no assertion reaches
//! inside the schema beyond behavior.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use ait_store::{NewBlock, NewConversation, NewTranscriptItem, Store};

/// A tempdir + the `store.db` path inside it; the dir owns cleanup.
fn temp_db() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ait").join("store.db");
    (dir, path)
}

fn item<'a>(kind: &'a str, payload: &'a str) -> NewTranscriptItem<'a> {
    NewTranscriptItem {
        kind,
        payload_json: payload,
    }
}

#[test]
fn open_creates_parent_directory_and_file() {
    let (_dir, path) = temp_db();
    assert!(!path.exists(), "fixture should start with no database");
    let store = Store::open(&path).unwrap();
    assert!(path.exists(), "open should create the database file");
    assert_eq!(store.path(), path.as_path());
}

#[test]
fn writes_wal_segments_for_concurrent_readers() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    store
        .create_conversation(NewConversation {
            title: "t",
            cwd: "/w",
            status: "running",
        })
        .unwrap();
    // journal_mode=WAL shows up on disk as -wal/-shm sidecars while the
    // connection lives — the observable half of "concurrent instances
    // share the database safely" (Spec R08 us-3).
    let wal = path.with_file_name("store.db-wal");
    assert!(wal.exists(), "WAL mode should produce a -wal sidecar");
}

#[test]
fn reopen_is_idempotent_and_preserves_rows() {
    let (_dir, path) = temp_db();
    let created = {
        let store = Store::open(&path).unwrap();
        store
            .create_conversation(NewConversation {
                title: "first",
                cwd: "/work",
                status: "running",
            })
            .unwrap()
    };
    // Second Store on the same file: migrations must not rerun or
    // clash, and the first connection's rows must be visible.
    let store = Store::open(&path).unwrap();
    let listed = store.list_conversations().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, created.id);
    assert_eq!(listed[0].title, "first");
}

#[test]
fn create_append_load_round_trips_items_in_order() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let convo = store
        .create_conversation(NewConversation {
            title: "fix the bug",
            cwd: "/repo",
            status: "running",
        })
        .unwrap();

    store
        .append_transcript_items(
            convo.id,
            &[
                item("task", r#"{"text":"fix the bug"}"#),
                item("tool_call", r#"{"id":"c1","name":"read_file"}"#),
            ],
        )
        .unwrap();
    // A second append continues seq where the first left off.
    store
        .append_transcript_items(
            convo.id,
            &[item("tool_result", r#"{"id":"c1","text":"…"}"#)],
        )
        .unwrap();

    let items = store.load_transcript(convo.id).unwrap();
    assert_eq!(
        items
            .iter()
            .map(|i| (i.seq, i.kind.as_str(), i.payload_json.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (0, "task", r#"{"text":"fix the bug"}"#),
            (1, "tool_call", r#"{"id":"c1","name":"read_file"}"#),
            (2, "tool_result", r#"{"id":"c1","text":"…"}"#),
        ]
    );
}

#[test]
fn append_to_unknown_conversation_is_an_error() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    // foreign_keys=ON turns the dangling FK into an error — a silent
    // insert would orphan items no conversation owns.
    assert!(store
        .append_transcript_items(999, &[item("task", "{}")])
        .is_err());
}

#[test]
fn list_conversations_is_newest_mutation_first() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let mut ids = Vec::new();
    for title in ["oldest", "middle", "newest"] {
        ids.push(
            store
                .create_conversation(NewConversation {
                    title,
                    cwd: "/w",
                    status: "done",
                })
                .unwrap()
                .id,
        );
        // Distinct updated_at values keep the ordering unambiguous even
        // on millisecond-granular clocks.
        thread::sleep(Duration::from_millis(2));
    }
    let listed: Vec<i64> = store
        .list_conversations()
        .unwrap()
        .iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(listed, vec![ids[2], ids[1], ids[0]]);
}

#[test]
fn mutation_bumps_updated_at_and_reorders_the_list() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let first = store
        .create_conversation(NewConversation {
            title: "a",
            cwd: "/w",
            status: "done",
        })
        .unwrap();
    thread::sleep(Duration::from_millis(2));
    let second = store
        .create_conversation(NewConversation {
            title: "b",
            cwd: "/w",
            status: "done",
        })
        .unwrap();
    thread::sleep(Duration::from_millis(2));

    // Appending to the older conversation makes it the freshest row.
    store
        .append_transcript_items(first.id, &[item("task", "{}")])
        .unwrap();
    let listed = store.list_conversations().unwrap();
    assert_eq!(listed[0].id, first.id);
    assert!(listed[0].updated_at > first.updated_at);
    assert_eq!(listed[1].id, second.id);
}

#[test]
fn set_conversation_status_updates_the_tag() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let convo = store
        .create_conversation(NewConversation {
            title: "t",
            cwd: "/w",
            status: "running",
        })
        .unwrap();
    store.set_conversation_status(convo.id, "done").unwrap();
    assert_eq!(store.list_conversations().unwrap()[0].status, "done");
}

#[test]
fn delete_cascades_transcript_items_and_plan() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let gone = store
        .create_conversation(NewConversation {
            title: "doomed",
            cwd: "/w",
            status: "done",
        })
        .unwrap();
    let kept = store
        .create_conversation(NewConversation {
            title: "kept",
            cwd: "/w",
            status: "done",
        })
        .unwrap();
    store
        .append_transcript_items(gone.id, &[item("task", "{}"), item("status", "{}")])
        .unwrap();
    store.upsert_plan(gone.id, r#"{"todos":[]}"#).unwrap();
    store
        .append_transcript_items(kept.id, &[item("task", "{}")])
        .unwrap();

    store.delete_conversation(gone.id).unwrap();

    assert_eq!(store.list_conversations().unwrap().len(), 1);
    // The rows are gone, not orphaned — cascade, not just the parent.
    assert!(store.load_transcript(gone.id).unwrap().is_empty());
    assert!(store.load_plan(gone.id).unwrap().is_none());
    assert_eq!(store.load_transcript(kept.id).unwrap().len(), 1);
}

#[test]
fn plan_upsert_keeps_only_the_latest_snapshot() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let convo = store
        .create_conversation(NewConversation {
            title: "t",
            cwd: "/w",
            status: "running",
        })
        .unwrap();

    assert!(store.load_plan(convo.id).unwrap().is_none());
    store
        .upsert_plan(
            convo.id,
            r#"{"todos":[{"content":"a","status":"pending"}]}"#,
        )
        .unwrap();
    store
        .upsert_plan(convo.id, r#"{"todos":[{"content":"a","status":"done"}]}"#)
        .unwrap();
    assert_eq!(
        store.load_plan(convo.id).unwrap().as_deref(),
        Some(r#"{"todos":[{"content":"a","status":"done"}]}"#)
    );
}

#[test]
fn sessions_and_blocks_survive_reopen() {
    let (_dir, path) = temp_db();
    let (session_id, started_at, ended_at);
    {
        let store = Store::open(&path).unwrap();
        let session = store.insert_terminal_session("zsh", "/home/u").unwrap();
        session_id = session.id;
        assert!(session.ended_at.is_none());
        started_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        ended_at = started_at + Duration::from_millis(1200);
        store
            .insert_block(NewBlock {
                session_id,
                command: "cargo test".to_owned(),
                exit: Some(0),
                duration: Some(Duration::from_millis(1200)),
                started_at: Some(started_at),
                ended_at: Some(ended_at),
            })
            .unwrap();
        // What the hooks never reported stays NULL.
        store
            .insert_block(NewBlock {
                session_id,
                command: "ctrl-c'd thing".to_owned(),
                exit: None,
                duration: None,
                started_at: Some(started_at),
                ended_at: None,
            })
            .unwrap();
    }

    // A second Store on the same file sees the first's rows.
    let store = Store::open(&path).unwrap();
    let session = store.terminal_session(session_id).unwrap().unwrap();
    assert_eq!(session.shell, "zsh");
    assert!(session.ended_at.is_none());

    let blocks = store.list_blocks(session_id).unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].seq, 0);
    assert_eq!(blocks[0].command, "cargo test");
    assert_eq!(blocks[0].exit, Some(0));
    assert_eq!(blocks[0].duration, Some(Duration::from_millis(1200)));
    assert_eq!(blocks[0].started_at, Some(started_at));
    assert_eq!(blocks[0].ended_at, Some(ended_at));
    assert_eq!(blocks[1].seq, 1);
    assert_eq!(blocks[1].exit, None);
    assert_eq!(blocks[1].duration, None);
    assert_eq!(blocks[1].ended_at, None);
}

/// A drain's worth of sealed blocks lands in ONE transaction: per-
/// session `seq` continues across batches, and a batch spanning two
/// sessions numbers each against its own session (Spec R08 — "writes
/// batched per drain in one transaction").
#[test]
fn insert_blocks_batches_per_session_seqs() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let first = store.insert_terminal_session("bash", "/w").unwrap();
    let second = store.insert_terminal_session("zsh", "/w").unwrap();
    let block = |session_id: i64, command: &str| NewBlock {
        session_id,
        command: command.to_owned(),
        exit: Some(0),
        duration: Some(Duration::from_millis(3)),
        started_at: None,
        ended_at: None,
    };

    // One batch spanning both sessions.
    store
        .insert_blocks(&[
            block(first.id, "a1"),
            block(second.id, "b1"),
            block(first.id, "a2"),
        ])
        .unwrap();
    // A later batch continues each session's numbering — the frontend
    // never tracks seq itself.
    store.insert_blocks(&[block(first.id, "a3")]).unwrap();

    let a_blocks = store.list_blocks(first.id).unwrap();
    let a: Vec<_> = a_blocks
        .iter()
        .map(|b| (b.seq, b.command.as_str()))
        .collect();
    assert_eq!(a, vec![(0, "a1"), (1, "a2"), (2, "a3")]);
    let b_blocks = store.list_blocks(second.id).unwrap();
    let b: Vec<_> = b_blocks
        .iter()
        .map(|b| (b.seq, b.command.as_str()))
        .collect();
    assert_eq!(b, vec![(0, "b1")]);
}

/// An empty batch is a no-op; a dangling `session_id` fails the whole
/// batch atomically — foreign_keys=ON turns it into an error, and the
/// one transaction means earlier rows of the same batch don't land
/// half-written.
#[test]
fn insert_blocks_is_atomic_and_empty_is_a_noop() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let session = store.insert_terminal_session("bash", "/w").unwrap();
    let block = |session_id: i64| NewBlock {
        session_id,
        command: "c".to_owned(),
        exit: None,
        duration: None,
        started_at: None,
        ended_at: None,
    };

    store.insert_blocks(&[]).unwrap();
    assert!(store
        .insert_blocks(&[block(session.id), block(999)])
        .is_err());
    assert!(
        store.list_blocks(session.id).unwrap().is_empty(),
        "the valid row rolled back with the batch"
    );
}

/// Every `terminal_sessions` row, oldest first — the read side that
/// lets tests observe session persistence (the `list_blocks`
/// precedent).
#[test]
fn list_terminal_sessions_returns_every_row() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    assert!(store.list_terminal_sessions().unwrap().is_empty());
    let first = store.insert_terminal_session("bash", "/w").unwrap();
    let second = store.insert_terminal_session("sh", "/tmp").unwrap();
    store.finish_terminal_session(first.id).unwrap();

    let sessions = store.list_terminal_sessions().unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].id, first.id);
    assert_eq!(sessions[0].shell, "bash");
    assert_eq!(sessions[0].cwd, "/w");
    assert!(sessions[0].ended_at.is_some());
    assert_eq!(sessions[1].id, second.id);
    assert_eq!(sessions[1].shell, "sh");
    assert!(sessions[1].ended_at.is_none());
}

#[test]
fn finish_terminal_session_stamps_ended_at() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let session = store.insert_terminal_session("bash", "/w").unwrap();
    let finished = store.finish_terminal_session(session.id).unwrap().unwrap();
    assert!(finished.ended_at.is_some());
    // Unknown ids finish to None, not an error — a tab that closed
    // twice is a degrade, not a crash.
    assert!(store.finish_terminal_session(999).unwrap().is_none());
}

#[test]
fn open_default_honours_xdg_data_home() {
    // The only test that mutates the environment — it owns
    // XDG_DATA_HOME for its duration; no other test in this binary
    // touches it.
    let dir = tempfile::tempdir().unwrap();
    let previous = std::env::var_os("XDG_DATA_HOME");
    std::env::set_var("XDG_DATA_HOME", dir.path());
    let store = Store::open_default().unwrap();
    match previous {
        Some(value) => std::env::set_var("XDG_DATA_HOME", value),
        None => std::env::remove_var("XDG_DATA_HOME"),
    }
    assert_eq!(
        store.path(),
        dir.path().join("ait").join("store.db").as_path()
    );
    assert!(dir.path().join("ait").join("store.db").exists());
}

#[test]
fn timestamps_round_trip_through_the_row_shapes() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let before = SystemTime::now();
    let convo = store
        .create_conversation(NewConversation {
            title: "t",
            cwd: "/w",
            status: "running",
        })
        .unwrap();
    let after = SystemTime::now();
    // Millisecond storage can shave sub-ms precision off `after`; a
    // generous bound beats a flake.
    assert!(convo.created_at >= before - Duration::from_millis(1));
    assert!(convo.created_at <= after + Duration::from_millis(1));
}

#[test]
fn two_stores_share_the_file() {
    let (_dir, path) = temp_db();
    let first = Store::open(&path).unwrap();
    let second = Store::open(&path).unwrap();
    let convo = first
        .create_conversation(NewConversation {
            title: "shared",
            cwd: "/w",
            status: "running",
        })
        .unwrap();
    second
        .append_transcript_items(convo.id, &[item("task", "{}")])
        .unwrap();
    // WAL + busy_timeout: the second connection's write lands and the
    // first connection reads it — the us-3 sharing story end to end.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if first.load_transcript(convo.id).unwrap().len() == 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "second store's write never became visible"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
