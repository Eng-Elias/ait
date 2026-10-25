//! Helpers shared by the testkit's integration tests: unique temp dirs
//! (removed on drop) and the recording-path convention re-spelled so
//! tests can predict where a step's `*.jsonl` lands.
//!
//! Not every test target uses every helper — dead code is expected per
//! binary, so it's allowed module-wide rather than per item.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A unique `ait-e2e-test-<pid>-<n>` directory under the system temp
/// dir, removed on drop — the crate-internal `TempDir` recipe spelled
/// for test crates (it is `pub(crate)`, unreachable from here).
pub(crate) struct TestDir(PathBuf);

impl TestDir {
    /// Create a fresh dir.
    pub(crate) fn fresh() -> Self {
        for _ in 0..100 {
            let path = std::env::temp_dir().join(format!(
                "ait-e2e-test-{}-{}",
                process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("could not create a fresh temp dir {}: {e}", path.display()),
            }
        }
        panic!("could not create a fresh temp dir after 100 tries");
    }

    /// The directory's path.
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    /// Write `name` inside the dir with `body`, returning its path —
    /// how tests plant scenario files and stub-driver scripts.
    pub(crate) fn write(&self, name: &str, body: &str) -> PathBuf {
        let file = self.0.join(name);
        fs::write(&file, body).expect("write test file");
        file
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The `<step>.jsonl` path the recording convention produces for the
/// calling test under `root` — `recording::test_dir`'s `::`→`/`
/// mapping re-spelled (the convention is crate-internal too).
pub(crate) fn step_file(root: &Path, step: usize) -> PathBuf {
    let test = std::thread::current()
        .name()
        .expect("test threads are named")
        .replace("::", "/");
    root.join(test).join(format!("{step:02}.jsonl"))
}
