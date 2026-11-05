//! Tests at the watcher seam (Spec R03, user stories 10–11): `watch`/
//! `watch_from` subscribe a `settings.toml` path and push
//! `Result<Settings, ConfigError>` events on a standard channel —
//! `Ok` on a good save, `Err` on a bad one, ~300 ms-debounced, with the
//! watcher surviving errors and late file creation. Real `notify`, real
//! filesystem, tempdir fixtures — no TTY, no network (standard 11).

use std::env;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use ait_config::{
    watch, watch_dirs, watch_from, ConfigError, DirsWatcher, Settings, SettingsWatcher,
};

/// Generous bound on "the event eventually arrives" — far past the ~300 ms
/// debounce so a loaded CI machine cannot flake.
const EVENT_WAIT: Duration = Duration::from_secs(10);

/// Bound for "no event should be pending": long enough that a *missing*
/// debounce (emitting per raw fs event) is caught, short enough to keep
/// the suite fast.
const QUIET_CHECK: Duration = Duration::from_secs(2);

fn write(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
}

/// Serializes the `watch()` test that mutates `XDG_CONFIG_HOME` — the
/// settings path is process environment, so it must not overlap another
/// env-touching test in this binary.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Runs `body` with `XDG_CONFIG_HOME` pointed at `xdg`, restoring the
/// previous value after — even on panic.
fn with_xdg<T>(xdg: &Path, body: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap();
    let saved = env::var_os("XDG_CONFIG_HOME");
    env::set_var("XDG_CONFIG_HOME", xdg);

    struct Restore(Option<OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => env::set_var("XDG_CONFIG_HOME", value),
                None => env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }
    let _restore = Restore(saved);

    body()
}

/// The next event on a watcher's channel, unwrapped — a missing event is a
/// test failure with context, not a hang.
fn next_event(watcher: &SettingsWatcher) -> Result<Settings, ConfigError> {
    watcher
        .receiver()
        .recv_timeout(EVENT_WAIT)
        .expect("no event arrived within the wait bound")
}

#[test]
fn valid_edit_emits_reloaded_settings() {
    // Story 10: editing settings.toml while ait runs yields a reloaded
    // Settings — the consumer swaps it in without a restart.
    let dirs = tempfile::tempdir().unwrap();
    let path = dirs.path().join("settings.toml");
    write(&path, "[terminal]\nshell = \"/bin/zsh\"\n");

    let _watcher = watch_from(&path).unwrap();

    write(&path, "[terminal]\nshell = \"/bin/fish\"\n");
    let settings = next_event(&_watcher).expect("valid edit should emit Settings, not an error");
    assert_eq!(
        settings.terminal.shell.as_deref(),
        Some("/bin/fish"),
        "the event must carry the post-edit settings"
    );
}

#[test]
fn invalid_edit_emits_config_error_and_watcher_recovers() {
    // Story 11: a broken save surfaces as a ConfigError event (the caller
    // keeps last-good), and the watcher itself is unharmed — a later good
    // save still reloads. The watcher must never panic on bad saves.
    let dirs = tempfile::tempdir().unwrap();
    let path = dirs.path().join("settings.toml");
    write(&path, "[terminal]\nshell = \"/bin/zsh\"\n");

    let _watcher = watch_from(&path).unwrap();

    write(&path, "[terminal]\nbogus = 1\n");
    let err = next_event(&_watcher).expect_err("invalid edit should emit a ConfigError");
    assert!(
        err.to_string().starts_with("terminal.bogus:"),
        "the error must name the offending key, got: {err}"
    );

    write(&path, "[terminal]\nshell = \"/bin/fish\"\n");
    let settings = next_event(&_watcher)
        .expect("watcher must keep working after an error: valid edit should emit Settings");
    assert_eq!(settings.terminal.shell.as_deref(), Some("/bin/fish"));
}

#[test]
fn syntactically_broken_toml_emits_config_error() {
    // A half-written file (invalid TOML syntax, not just a bad key) is
    // also just a channel payload — never a panic.
    let dirs = tempfile::tempdir().unwrap();
    let path = dirs.path().join("settings.toml");
    write(&path, "[terminal\nshell = \n");

    let _watcher = watch_from(&path).unwrap();

    write(&path, "still = [broken\n");
    next_event(&_watcher).expect_err("broken TOML should emit a ConfigError");
}

/// One full burst scenario on a fresh tempdir and watcher — reported as
/// `Err` so the test can retry (see below).
fn burst_attempt() -> Result<(), String> {
    let dirs = tempfile::tempdir().unwrap();
    let path = dirs.path().join("settings.toml");
    write(&path, "[terminal]\nshell = \"/bin/zsh\"\n");

    let watcher = watch_from(&path).map_err(|e| format!("watch_from failed: {e}"))?;

    for shell in ["/bin/a", "/bin/b", "/bin/c", "/bin/fish"] {
        write(&path, &format!("[terminal]\nshell = \"{shell}\"\n"));
    }

    let event = watcher
        .receiver()
        .recv_timeout(EVENT_WAIT)
        .map_err(|_| String::from("no event arrived within the wait bound"))?;
    let settings = event.map_err(|e| format!("the burst emitted a ConfigError: {e}"))?;
    if settings.terminal.shell.as_deref() != Some("/bin/fish") {
        return Err(format!(
            "the debounced event carried intermediate content {:?}, not the burst's final write",
            settings.terminal.shell
        ));
    }
    match watcher.receiver().recv_timeout(QUIET_CHECK) {
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(()),
        other => Err(format!(
            "the burst did not collapse to one event: {other:?}"
        )),
    }
}

#[test]
fn burst_writes_collapse_to_one_event() {
    // Spec R03's ~300 ms debounce: an editor's multi-write save — and a
    // rapid sequence of whole-file saves — must not emit an event per raw
    // fs notification (each intermediate read could even see a truncated
    // file). One burst, one event, carrying the final content.
    //
    // Bounded retry on a fresh tempdir+watcher: a genuine debounce
    // regression (per-event emission, a dropped final write) fails every
    // attempt deterministically, but on a loaded CI box an inotify
    // delivery or a write can straddle the ~300 ms window and produce a
    // legitimate intermediate event — this flaked twice during R04. The
    // retry separates the two.
    const ATTEMPTS: usize = 3;
    let mut failure = String::new();
    for attempt in 1..=ATTEMPTS {
        match burst_attempt() {
            Ok(()) => return,
            Err(e) => failure = format!("attempt {attempt}/{ATTEMPTS}: {e}"),
        }
    }
    panic!("{failure}");
}

#[test]
fn sibling_file_noise_does_not_starve_the_debounce() {
    // Only events touching settings.toml may slide the quiet window —
    // otherwise a busy config dir (other tools' churn on sibling files)
    // starves the reload forever. Churn a sibling well past the debounce
    // window and confirm the event still lands on schedule.
    let dirs = tempfile::tempdir().unwrap();
    let path = dirs.path().join("settings.toml");
    write(&path, "[terminal]\nshell = \"/bin/zsh\"\n");
    let _watcher = watch_from(&path).unwrap();

    // ~2 s of sibling churn — far past the 300 ms window.
    let noise = dirs.path().join("unrelated.tmp");
    let churn = std::thread::spawn(move || {
        let stop = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < stop {
            write(&noise, "x");
            std::thread::sleep(Duration::from_millis(30));
        }
    });
    // Let the churn get going, then touch the target file.
    std::thread::sleep(Duration::from_millis(100));
    write(&path, "[terminal]\nshell = \"/bin/fish\"\n");

    // The event lands ~300 ms after the write — long before the churn
    // ends; a slide-on-anything window would starve past this bound.
    let settings = _watcher
        .receiver()
        .recv_timeout(Duration::from_millis(1500))
        .expect("sibling noise starved the debounce — no event arrived")
        .expect("a valid edit should emit Settings");
    assert_eq!(settings.terminal.shell.as_deref(), Some("/bin/fish"));
    churn.join().unwrap();
}

#[test]
fn atomic_save_via_rename_emits_settings() {
    // The editor-atomic-save landmine: write temp + rename replaces the
    // file's inode, which silences a naive file-level watch. Watching the
    // parent directory keeps emitting — this is the shape real editors
    // (vim, gedit, VS Code) save in.
    let dirs = tempfile::tempdir().unwrap();
    let path = dirs.path().join("settings.toml");
    write(&path, "[terminal]\nshell = \"/bin/zsh\"\n");

    let _watcher = watch_from(&path).unwrap();

    let temp = dirs.path().join(".settings.toml.tmp");
    write(&temp, "[terminal]\nshell = \"/bin/fish\"\n");
    std::fs::rename(&temp, &path).unwrap();

    let settings = next_event(&_watcher).expect("an atomic-save rename should emit Settings");
    assert_eq!(settings.terminal.shell.as_deref(), Some("/bin/fish"));
}

#[test]
fn deleting_the_file_emits_defaults() {
    // Removing settings.toml mid-session is a change like any other: the
    // file layer's "missing means defaults" rule flows through as an
    // `Ok(Settings::default())` event.
    let dirs = tempfile::tempdir().unwrap();
    let path = dirs.path().join("settings.toml");
    write(&path, "[terminal]\nshell = \"/bin/zsh\"\n");

    let _watcher = watch_from(&path).unwrap();

    std::fs::remove_file(&path).unwrap();

    let settings = next_event(&_watcher).expect("deletion should emit default Settings");
    assert_eq!(settings, Settings::default());
}

#[test]
fn watch_uses_the_resolved_settings_path() {
    // `watch()` — no path argument — subscribes to the same
    // `$XDG_CONFIG_HOME/ait/settings.toml` that `load()` reads.
    let dirs = tempfile::tempdir().unwrap();
    let ait_dir = dirs.path().join("ait");
    std::fs::create_dir_all(&ait_dir).unwrap();

    with_xdg(dirs.path(), || {
        let _watcher = watch().unwrap();
        write(
            &ait_dir.join("settings.toml"),
            "[terminal]\nshell = \"/bin/fish\"\n",
        );
        let settings = next_event(&_watcher).expect("watch() should emit Settings");
        assert_eq!(settings.terminal.shell.as_deref(), Some("/bin/fish"));
    });
}

#[test]
fn file_created_after_watch_starts_emits_settings() {
    // The watcher subscribes before settings.toml exists; a file that
    // appears later still surfaces as a Settings event.
    let dirs = tempfile::tempdir().unwrap();
    let path = dirs.path().join("settings.toml");

    let _watcher = watch_from(&path).unwrap();

    write(&path, "[terminal]\nscrollback_lines = 4_000\n");
    let settings = next_event(&_watcher).expect("created file should emit Settings, not an error");
    assert_eq!(settings.terminal.scrollback_lines, Some(4_000));
}

/// The next `()` on a directory watcher's channel — a missing event is
/// a test failure with context, not a hang.
fn next_dirs_event(watcher: &DirsWatcher) {
    watcher
        .receiver()
        .recv_timeout(EVENT_WAIT)
        .expect("no event arrived within the wait bound")
}

#[test]
fn directory_watch_emits_on_a_nested_file_write() {
    // §5.10.2 (Spec R10-T6): the skills watcher subscribes to a
    // directory RECURSIVELY — a `SKILL.md` write nested two levels
    // down (`<dir>/skills/demo/SKILL.md`) still settles into an event.
    let dirs = tempfile::tempdir().unwrap();
    let root = dirs.path().join("ait");
    std::fs::create_dir_all(&root).unwrap();

    let _watcher = watch_dirs(std::slice::from_ref(&root)).unwrap();

    let skill = root.join("skills").join("demo");
    std::fs::create_dir_all(&skill).unwrap();
    write(
        &skill.join("SKILL.md"),
        "---\nname: demo\ndescription: d\n---\n",
    );
    next_dirs_event(&_watcher);
}

#[test]
fn directory_watch_covers_every_listed_dir() {
    // Both skills roots watch in parallel — a change under EITHER
    // listed directory emits (the payload is "rescan now", never
    // attribution).
    let dirs = tempfile::tempdir().unwrap();
    let user = dirs.path().join("user-layer");
    let project = dirs.path().join("project-layer");
    std::fs::create_dir_all(&user).unwrap();
    std::fs::create_dir_all(&project).unwrap();

    let _watcher = watch_dirs(&[user, project.clone()]).unwrap();

    write(&project.join("touched"), "x");
    next_dirs_event(&_watcher);
}

#[test]
fn directory_watch_emits_on_delete_and_skips_missing_dirs() {
    // Deleting a file under a watched dir is a change like any other;
    // a listed dir that does not exist is skipped — a skills dir that
    // has not been created yet is simply nothing to watch, not a
    // watch failure (convergence is the caller's rescan).
    let dirs = tempfile::tempdir().unwrap();
    let present = dirs.path().join("present");
    std::fs::create_dir_all(&present).unwrap();
    let missing = dirs.path().join("missing");

    let _watcher = watch_dirs(&[missing, present.clone()]).unwrap();

    let file = present.join("SKILL.md");
    write(&file, "---\nname: demo\ndescription: d\n---\n");
    next_dirs_event(&_watcher);

    std::fs::remove_file(&file).unwrap();
    next_dirs_event(&_watcher);
}

#[test]
fn directory_watch_with_no_watchable_dirs_stays_inert() {
    // A set with no existing dirs yields a live-but-quiet watcher —
    // the caller needs no empty-set special case.
    let dirs = tempfile::tempdir().unwrap();

    let watcher = watch_dirs(&[dirs.path().join("absent")]).unwrap();

    watcher
        .receiver()
        .recv_timeout(QUIET_CHECK)
        .expect_err("an unwatched set must not emit");
}
