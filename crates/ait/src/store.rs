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

use ait_store::{NewBlock, Store, StoreError};

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
}
