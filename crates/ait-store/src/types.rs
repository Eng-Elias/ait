//! The row shapes [`Store`] hands back and takes in. Transcript and
//! plan payloads are opaque JSON — the vocabulary lives in ait-agent
//! (Spec R08); this crate knows SQL and JSON text, nothing else.
//!
//! [`Store`]: crate::Store

use std::time::{Duration, SystemTime};

/// A `conversations` row — the agent's durable unit of work (GLOSSARY
/// "agent conversation", sharpened by Spec R08 to outlive its tab).
#[derive(Debug, Clone, PartialEq)]
pub struct Conversation {
    /// Row id — stable across restarts; what resume and delete address.
    pub id: i64,
    /// Display title — the first task's text, truncated by the caller.
    pub title: String,
    /// The working directory the conversation ran in.
    pub cwd: String,
    /// Opaque lifecycle tag the caller owns ("running", "done", …).
    pub status: String,
    /// When the row was created.
    pub created_at: SystemTime,
    /// Last mutation: create, transcript append, plan upsert or status
    /// change — what [`Store::list_conversations`] sorts by.
    ///
    /// [`Store::list_conversations`]: crate::Store::list_conversations
    pub updated_at: SystemTime,
}

/// A conversation to create — what
/// [`Store::create_conversation`](crate::Store::create_conversation)
/// takes; `id` and the timestamps are the store's to assign.
#[derive(Debug, Clone, Copy)]
pub struct NewConversation<'a> {
    /// Display title.
    pub title: &'a str,
    /// Working directory the conversation runs in.
    pub cwd: &'a str,
    /// Initial lifecycle tag (see [`Conversation::status`]).
    pub status: &'a str,
}

/// One `transcript_items` row — a neutral, schema-free record of one
/// conversation event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptItem {
    /// Ordering within the conversation — `0, 1, 2, …` assigned at
    /// append; a `summary` item's payload names the seqs it covers.
    pub seq: i64,
    /// The vocabulary tag (`task`, `tool_call`, `summary`, …) — opaque.
    pub kind: String,
    /// The kind's serialized body, stored and returned verbatim.
    pub payload_json: String,
}

/// A transcript item to append — what
/// [`Store::append_transcript_items`](crate::Store::append_transcript_items)
/// takes; `seq` is the store's to assign.
#[derive(Debug, Clone, Copy)]
pub struct NewTranscriptItem<'a> {
    /// The vocabulary tag (see [`TranscriptItem::kind`]).
    pub kind: &'a str,
    /// The serialized payload.
    pub payload_json: &'a str,
}

/// A `terminal_sessions` row — one per spawned tab (Spec R08): the
/// shell, its cwd and the session's wall-clock bounds.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalSession {
    /// Row id — stable for the tab's lifetime; what `blocks` rows key on.
    pub id: i64,
    /// The shell the tab spawned (`bash`, `zsh`, …).
    pub shell: String,
    /// The tab's working directory at spawn.
    pub cwd: String,
    /// When the session started (set at insert).
    pub started_at: SystemTime,
    /// When the tab closed — `Some` once
    /// [`Store::finish_terminal_session`](crate::Store::finish_terminal_session)
    /// ran.
    pub ended_at: Option<SystemTime>,
}

/// A `blocks` row — one sealed command block's *metadata* (Spec R08,
/// user stories 23–24): command, exit, duration and wall-clock bounds.
/// Fields the hooks never reported stay `None`; no output text or
/// scrollback is ever stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    /// Ordering within the session — `0, 1, 2, …` assigned at insert.
    pub seq: i64,
    /// The session this block ran in.
    pub session_id: i64,
    /// The command text as reported or resolved at seal.
    pub command: String,
    /// The exit code, if the block's `D` mark carried one.
    pub exit: Option<i64>,
    /// Execution time, if both bounds were known.
    pub duration: Option<Duration>,
    /// Wall-clock start (`Instant`→`SystemTime` at seal), if known.
    pub started_at: Option<SystemTime>,
    /// Wall-clock end, if known.
    pub ended_at: Option<SystemTime>,
}

/// A block to store — what
/// [`Store::insert_block`](crate::Store::insert_block) takes; `seq` is
/// the store's to assign.
#[derive(Debug, Clone)]
pub struct NewBlock {
    /// The session this block ran in — a [`TerminalSession::id`].
    pub session_id: i64,
    /// The command text (see [`Block::command`]).
    pub command: String,
    /// The exit code, if reported.
    pub exit: Option<i64>,
    /// Execution time, if known.
    pub duration: Option<Duration>,
    /// Wall-clock start, if known.
    pub started_at: Option<SystemTime>,
    /// Wall-clock end, if known.
    pub ended_at: Option<SystemTime>,
}
