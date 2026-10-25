//! The fresh working directory every spawned program gets (Spec E01's
//! pinned environment): unique per spawn, removed on drop — a test can
//! never leak state into a sibling's cwd.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{fs, io, process};

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A uniquely-named directory under the system temp dir.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    /// Create a fresh `ait-e2e-<pid>-<n>` directory.
    pub(crate) fn fresh() -> io::Result<Self> {
        for _ in 0..100 {
            let path = std::env::temp_dir().join(format!(
                "ait-e2e-{}-{}",
                process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a fresh ait-e2e temp dir after 100 tries",
        ))
    }

    /// The directory's path — the spawned program's cwd.
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
