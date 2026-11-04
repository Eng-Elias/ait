//! `ait-store` — AIT's local-first persistence crate (REQUIREMENTS.md
//! §5.8, Spec R08): the `ait-store` growth ADR-0003 anticipated.
//!
//! One synchronous [`Store`] facade over `rusqlite` (`bundled`) holds
//! everything the round persists under `$XDG_DATA_HOME/ait/store.db`:
//!
//! - **conversations** — the agent's durable unit of work; created per
//!   first task in a tab, listed newest-first for the `Alt+R` picker,
//!   resumed and deleted by id,
//! - **transcript_items** — each conversation's neutral, schema-free
//!   event log; replayed on resume into whatever IR the resolved model
//!   speaks (the vocabulary is ait-agent's — payloads are opaque JSON
//!   here),
//! - **plans** — one latest `update_plan` snapshot per conversation,
//! - **terminal_sessions** + **blocks** — per-tab command *metadata*
//!   (command, exit, duration, bounds); never output text or scrollback
//!   (Spec R08, user story 24).
//!
//! [`Store::open_default`] resolves the XDG path, creates the
//! directory, applies `journal_mode=WAL` + a busy timeout +
//! `foreign_keys=ON` (concurrent `ait` instances share the file
//! safely), and runs sequential migrations tracked by
//! `PRAGMA user_version`. The surface is deliberately CRUD-only —
//! nothing grows beyond "load a transcript".
//!
//! The crate stays synchronous (the `ait` binary is tokio-free,
//! ADR-0004) and depends on no `ait-*` crate: it knows SQL and opaque
//! JSON, nothing else.

mod error;
mod path;
mod store;
mod types;

pub use error::StoreError;
pub use path::store_path;
pub use store::Store;
pub use types::{
    Block, Conversation, NewBlock, NewConversation, NewTranscriptItem, TerminalSession,
    TranscriptItem,
};
