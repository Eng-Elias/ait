//! Failure evidence (Spec E01 "Failure evidence", ticket E01-T5): the
//! Playwright-trace equivalent — on any failed step the harness dumps
//! the final screen text, the raw PTY tail and the ordered action log
//! under [`dump_root()`]`/<test>/` so failures explain themselves. CI
//! uploads that directory as a workflow artifact.
//!
//! `E2e` writes a dump automatically when a test panics while it is
//! alive (harness waits/asserts *and* test-side panics — see
//! `E2e`'s `Drop`), naming the dir after the panicking test's thread;
//! scenario runners call `E2e::dump_evidence` with an explicit name.

use std::any::Any;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// Environment override for the dump root — handy for local reruns and
/// for a scenario runner that wants dumps somewhere else.
const DUMP_DIR_ENV: &str = "AIT_E2E_DUMP_DIR";

/// The files one dump writes: screen text, raw PTY tail, action log.
const SCREEN_FILE: &str = "screen.txt";
const TAIL_FILE: &str = "pty-tail.bin";
const ACTIONS_FILE: &str = "actions.log";

/// The root every failure dump lands under: `$AIT_E2E_DUMP_DIR` when
/// set, else `<workspace>/target/e2e-failures` — the path CI uploads.
pub fn dump_root() -> PathBuf {
    if let Some(dir) = std::env::var_os(DUMP_DIR_ENV) {
        return PathBuf::from(dir);
    }
    // `CARGO_MANIFEST_DIR` is this crate's dir (`crates/ait-testkit`),
    // and the crate always lives inside the workspace — so two levels
    // up is the workspace root regardless of which package's tests run.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("ait-testkit lives at <workspace>/crates/ait-testkit")
        .join("target/e2e-failures")
}

/// The dump dir name for an automatic (panic-triggered) dump: the
/// current thread's name — under `cargo test` that is the test's own
/// name — sanitized for the filesystem. Unnamed threads dump under
/// `unnamed`.
pub(crate) fn current_test_name() -> String {
    let thread = std::thread::current();
    sanitize(thread.name().unwrap_or("unnamed"))
}

/// A filesystem-safe test/scenario name: anything outside
/// `[A-Za-z0-9._-]` (e.g. the `::` in `mod::test`) becomes `_`.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// One ordered action-log entry: what the harness sent or waited for,
/// when (relative to spawn), and the screen-state hash at that moment.
pub(crate) struct ActionEntry {
    /// Time since the `E2e` was spawned.
    pub at: Duration,
    /// What happened, e.g. `press("Alt+T")` or `wait_for_screen("…")`.
    pub action: String,
    /// [`ScreenState::hash`](crate::ScreenState::hash) of the screen at
    /// log time — the same hash a recording's `screen_hash` field
    /// stores, so the log's `screen=` values compare directly to
    /// recorded steps (drift visibility for humans, not a
    /// cryptographic guard).
    pub screen_hash: u64,
}

/// Write the three dump files under `dump_root()/<test_name>/`,
/// replacing a stale dump from an earlier run of the same test, and
/// return the dump directory.
pub(crate) fn write_dump(
    test_name: &str,
    screen: &str,
    tail: &[u8],
    actions: &[ActionEntry],
) -> io::Result<PathBuf> {
    let dir = dump_root().join(sanitize(test_name));
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(SCREEN_FILE), screen)?;
    fs::write(dir.join(TAIL_FILE), tail)?;
    fs::write(dir.join(ACTIONS_FILE), render_actions(actions))?;
    Ok(dir)
}

/// `actions.log`: one `#<seq> [t+<ms>] <action> screen=<hash>` line
/// per entry, in the order steps ran.
fn render_actions(actions: &[ActionEntry]) -> String {
    let mut log = String::from(
        "# action log — inputs sent and waits performed, in order, \
         with the screen-state hash at each step\n",
    );
    for (seq, entry) in actions.iter().enumerate() {
        let _ = writeln!(
            log,
            "#{seq:03} [t+{:>6}ms] {} screen={:016x}",
            entry.at.as_millis(),
            entry.action,
            entry.screen_hash,
        );
    }
    log
}

/// The message inside a caught panic payload — for callers that
/// `catch_unwind` or join a thread and re-raise the text with context,
/// so the original evidence (screens, drift diffs) survives.
pub fn panic_text(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&'static str>() {
        text.to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "(non-string panic)".to_string()
    }
}

/// Render a byte run for the action log: ASCII-escaped and truncated,
/// so raw input stays readable without flooding the log.
pub(crate) fn describe_bytes(bytes: &[u8]) -> String {
    const MAX: usize = 96;
    let mut rendered = String::from("send_bytes(\"");
    for &byte in bytes.iter().take(MAX) {
        for c in std::ascii::escape_default(byte) {
            rendered.push(c as char);
        }
    }
    if bytes.len() > MAX {
        let _ = write!(rendered, "…(+{} bytes)", bytes.len() - MAX);
    }
    rendered.push_str("\")");
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_path_unsafe_characters() {
        assert_eq!(sanitize("e2e::tabs::opens"), "e2e__tabs__opens");
        assert_eq!(sanitize("plain-name_1.2"), "plain-name_1.2");
        assert_eq!(sanitize("a/b c"), "a_b_c");
    }

    #[test]
    fn describe_bytes_escapes_and_truncates() {
        assert_eq!(describe_bytes(b"hi\n"), "send_bytes(\"hi\\n\")");
        let long = vec![b'x'; 200];
        assert!(describe_bytes(&long).ends_with("…(+104 bytes)\")"));
    }
}
