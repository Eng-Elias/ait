//! Live-PTY tests at the public `LiveSession` seam (Spec R01: a small second
//! tier covering spawn/resize/exit paths only — everything else is byte-fed
//! in `session.rs`/`keys.rs`). These spawn real processes in a real PTY, so
//! every wait is bounded by a timeout. The scripted tests are unix-only
//! (`/bin/sh` fixtures); the ConPTY passthrough spike is Windows-only —
//! a platform with no test in this file compiles to an empty suite.

use std::ffi::OsString;
use std::time::{Duration, Instant};

use ait_term::{LiveEvent, LiveSession, LiveSessionOptions};

#[cfg(unix)]
use std::sync::Mutex;

#[cfg(unix)]
use ait_term::{ExitStatus, Key, KeyEvent, Modifiers, SessionOptions, Size};

#[cfg(windows)]
use ait_term::{PromptMarkKind, SessionEvent};

/// Serializes the tests that mutate the process-wide `SHELL` variable.
#[cfg(unix)]
static SHELL_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Generous bound for every wait below: even a loaded CI runner finishes
/// these tiny programs in well under this.
const TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(unix)]
fn spawn_shell_script(script: &str) -> LiveSession {
    spawn(
        "/bin/sh",
        &[OsString::from("-c"), OsString::from(script)],
        LiveSessionOptions::default(),
    )
}

fn spawn(program: &str, args: &[OsString], mut options: LiveSessionOptions) -> LiveSession {
    options.program = Some(OsString::from(program));
    options.args = args.to_vec();
    LiveSession::spawn(options).expect("spawn should succeed")
}

/// Drain events until `pred` accepts one, or the timeout kills the test.
#[cfg(unix)]
fn wait_for_event(session: &mut LiveSession, pred: impl Fn(&LiveEvent) -> bool) -> LiveEvent {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        for event in session.drain_events() {
            if pred(&event) {
                return event;
            }
        }
        assert!(Instant::now() < deadline, "timed out waiting for event");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Wait for `LiveEvent::Exited` and return its status.
#[cfg(unix)]
fn wait_for_exit(session: &mut LiveSession) -> ExitStatus {
    match wait_for_event(session, |event| matches!(event, LiveEvent::Exited(_))) {
        LiveEvent::Exited(status) => status,
        other => panic!("expected Exited, got {other:?}"),
    }
}

/// Assert the child process is fully gone — no zombie, no orphan.
/// `kill(pid, 0)` is the portable existence probe (an `ESRCH` errno means
/// gone, `EPERM` would still mean it exists); unlike `/proc` it works on
/// every unix, and §6 lists macOS as supported.
#[cfg(unix)]
fn assert_child_gone(pid: Option<u32>, context: &str) {
    let pid = pid.expect("spawned child has a pid");
    let gone = unsafe { libc::kill(pid as i32, 0) } != 0
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    assert!(gone, "child {pid} {context}");
}

/// No portable existence probe exists in our dep tree outside unix; the
/// waiter-join contract is still covered by the event assertions above.
#[cfg(not(unix))]
fn assert_child_gone(_pid: Option<u32>, _context: &str) {}

/// All text in the grid: scrollback then viewport, rows trimmed and joined.
#[cfg(unix)]
fn grid_text(session: &LiveSession) -> String {
    let grid = session.grid();
    grid.scrollback_rows()
        .chain(grid.visible_rows())
        .map(|row| row.text().trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(unix)]
#[test]
fn spawned_program_output_lands_in_grid_and_exit_is_reported() {
    let mut session = spawn_shell_script("printf hello-ait; exit 3");

    let status = wait_for_exit(&mut session);

    assert_eq!(status.code, 3);
    assert!(status.signal.is_none());
    assert!(!status.success());
    assert!(grid_text(&session).contains("hello-ait"));
    assert_eq!(
        session.state(),
        ait_term::SessionState::Exited(status.clone())
    );

    // The waiter thread reaped the child before surfacing Exited: the
    // process must be fully gone — no zombie, no orphan.
    assert_child_gone(session.child_pid(), "still exists after Exited");
}

#[cfg(unix)]
#[test]
fn shell_env_program_is_spawned_when_set() {
    let _guard = SHELL_ENV_LOCK.lock().unwrap();
    let saved = std::env::var_os("SHELL");
    // `/bin/echo` as the "shell": spawned with our args, it prints them.
    std::env::set_var("SHELL", "/bin/echo");

    let result = LiveSession::spawn(LiveSessionOptions {
        args: vec![OsString::from("from-shell-env")],
        ..Default::default()
    });

    match saved {
        Some(value) => std::env::set_var("SHELL", value),
        None => std::env::remove_var("SHELL"),
    }

    let mut session = result.expect("spawn should succeed");
    let status = wait_for_exit(&mut session);
    assert!(status.success());
    assert!(grid_text(&session).contains("from-shell-env"));
}

#[cfg(unix)]
#[test]
fn missing_shell_env_falls_back_to_bin_sh() {
    let _guard = SHELL_ENV_LOCK.lock().unwrap();
    let saved = std::env::var_os("SHELL");
    std::env::remove_var("SHELL");

    let result = LiveSession::spawn(LiveSessionOptions {
        args: vec![
            OsString::from("-c"),
            OsString::from("printf fallback-ok; exit 0"),
        ],
        ..Default::default()
    });

    if let Some(value) = saved {
        std::env::set_var("SHELL", value);
    }

    // The fallback /bin/sh must run `-c` scripts — proving we got a shell.
    let mut session = result.expect("spawn should succeed");
    let status = wait_for_exit(&mut session);
    assert!(status.success());
    assert!(grid_text(&session).contains("fallback-ok"));
}

#[cfg(unix)]
#[test]
fn resize_forwards_the_new_winsize_to_the_pty() {
    // The shell waits for input before reporting `stty size`, so the
    // resize is guaranteed to land first.
    let mut session = spawn_shell_script("read x; stty size");

    session
        .resize(Size::new(100, 40))
        .expect("resize should succeed");
    assert_eq!(session.size(), Size::new(100, 40));
    session.send_bytes(b"\n").expect("input should be accepted");

    wait_for_exit(&mut session);
    // `stty size` prints "rows cols".
    assert!(grid_text(&session).contains("40 100"));
}

#[cfg(unix)]
#[test]
fn key_events_reach_the_shell() {
    let mut session = spawn_shell_script("read line; echo \"got:$line\"");

    for key in ['h', 'i'] {
        session
            .send_key(KeyEvent::new(Key::Char(key), Modifiers::NONE))
            .expect("key should be accepted");
    }
    session
        .send_key(KeyEvent::new(Key::Enter, Modifiers::NONE))
        .expect("key should be accepted");

    wait_for_exit(&mut session);
    assert!(grid_text(&session).contains("got:hi"));
}

#[cfg(unix)]
#[test]
fn an_output_flood_is_delivered_without_losing_bytes() {
    // 8000 numbered lines fit the default 10_000-line scrollback, so every
    // byte must land in the grid intact — the whole output is recoverable.
    let options = LiveSessionOptions {
        session: SessionOptions {
            size: Size::new(80, 24),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut session = spawn(
        "/bin/sh",
        &[OsString::from("-c"), OsString::from("seq 1 8000")],
        options,
    );

    let status = wait_for_exit(&mut session);
    assert!(status.success());

    let grid = session.grid();
    let received: Vec<String> = grid
        .scrollback_rows()
        .chain(grid.visible_rows())
        .map(|row| row.text().trim_end().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    let expected: Vec<String> = (1..=8000).map(|n| n.to_string()).collect();
    assert_eq!(received, expected);
}

#[cfg(unix)]
#[test]
fn teardown_under_flood_kills_the_child_and_joins_threads() {
    // `yes` floods forever; the bounded channel fills, the reader stalls on
    // `send`, and we drop without ever draining — teardown must still kill
    // the child and bring the threads down rather than deadlock.
    let session = spawn_shell_script("exec yes");
    let pid = session.child_pid().expect("spawned child has a pid");

    // Let the flood run against an undrained channel.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        session.state(),
        ait_term::SessionState::Running,
        "the flood should still be running"
    );

    drop(session);

    // The waiter reaped the killed child before drop returned: no orphan,
    // no zombie.
    assert_child_gone(Some(pid), "survived teardown");
}

#[cfg(unix)]
#[test]
fn input_can_still_be_sent_while_output_is_flooding() {
    // §6: a flood must not starve the input path.
    let mut session = spawn_shell_script("yes & bg=$!; read line; echo \"got:$line\"; kill $bg");

    session
        .send_bytes(b"still-works\n")
        .expect("input should be accepted during a flood");

    let status = wait_for_exit(&mut session);
    assert!(status.success());
    let mut grid_has_reply = false;
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        session.drain_events();
        if grid_text(&session).contains("got:still-works") {
            grid_has_reply = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(grid_has_reply, "shell never answered input under flood");
}

#[cfg(unix)]
#[test]
fn try_recv_event_surfaces_output_and_exit() {
    // `read` keeps the child alive, so the first event must be the
    // OutputReady carrying "polled" — `Exited` is only possible afterwards.
    let mut session = spawn_shell_script("printf polled; read x; printf done");

    let deadline = Instant::now() + TIMEOUT;
    loop {
        match session.try_recv_event() {
            Some(LiveEvent::OutputReady) => break,
            Some(_) => panic!("no other event can precede OutputReady here"),
            None => {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for OutputReady"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    assert!(grid_text(&session).contains("polled"));

    // Let the script finish; the session must then report Exited.
    session.send_bytes(b"\n").expect("input should be accepted");
    let status = loop {
        match session.try_recv_event() {
            Some(LiveEvent::Exited(status)) => break status,
            Some(_) => {}
            None => {
                assert!(Instant::now() < deadline, "timed out waiting for Exited");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    };

    assert!(status.success());
    assert!(grid_text(&session).contains("done"));
    // After the exit event nothing further is pending.
    assert!(session.try_recv_event().is_none());
}

#[cfg(unix)]
#[test]
fn output_tail_holds_the_raw_bytes_fed_into_the_grid() {
    let mut session = spawn_shell_script("printf tail-marker; read x");

    // The tail reports output the session has *consumed* (fed into the
    // grid), not output the reader has merely produced: bytes the child
    // already wrote but nobody drained yet are not in it.
    std::thread::sleep(Duration::from_millis(200));
    assert!(session.output_tail().is_empty());

    wait_for_event(&mut session, |event| {
        matches!(event, LiveEvent::OutputReady)
    });
    let tail = session.output_tail();
    assert!(
        String::from_utf8_lossy(tail).contains("tail-marker"),
        "tail should hold the raw output: {}",
        String::from_utf8_lossy(tail)
    );
}

#[cfg(unix)]
#[test]
fn output_tail_is_bounded_to_the_last_64_kib() {
    // ~700 KB of output — far past the documented 64 KiB bound.
    let mut session = spawn_shell_script("seq 1 100000");

    let status = wait_for_exit(&mut session);
    assert!(status.success());

    let tail = session.output_tail();
    assert_eq!(tail.len(), 64 * 1024, "the tail is capped at 64 KiB");
    // It is a *tail*: the last bytes the program emitted.
    assert!(
        String::from_utf8_lossy(tail).trim_end().ends_with("100000"),
        "the tail should end with the program's last output"
    );
}

#[cfg(unix)]
#[test]
fn osc_events_flow_through_the_live_channel() {
    // OSC 9 (notification) observed by the core while output is fed must
    // surface on the live event stream, in order, before Exited.
    let mut session = spawn_shell_script("printf '\\033]9;build done\\007'; printf tail; exit 0");

    let deadline = Instant::now() + TIMEOUT;
    let mut notification = None;
    let status = 'outer: loop {
        for event in session.drain_events() {
            match event {
                LiveEvent::Osc(ait_term::SessionEvent::Notification { message, .. }) => {
                    notification = Some(message);
                }
                LiveEvent::Exited(status) => break 'outer status,
                _ => {}
            }
        }
        assert!(Instant::now() < deadline, "timed out waiting for Exited");
        std::thread::sleep(Duration::from_millis(5));
    };

    assert!(status.success());
    assert_eq!(notification.as_deref(), Some("build done"));
    assert!(grid_text(&session).contains("tail"));
}

/// A `Write` shared with the test so the mirrored bytes can be
/// inspected: the mirror is consumed as `Box<dyn Write + Send>`, so a
/// plain buffer behind a Mutex is the simplest stand-in for a terminal.
#[cfg(unix)]
#[derive(Clone, Default)]
struct SharedBuf(std::sync::Arc<Mutex<Vec<u8>>>);

#[cfg(unix)]
impl std::io::Write for SharedBuf {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
#[test]
fn the_output_mirror_receives_bytes_as_the_reader_reads_them() {
    let mut session = spawn_shell_script("printf early-mark; read x; printf late-mark; read x");
    let mirror = SharedBuf::default();
    session.set_output_mirror(Some(Box::new(mirror.clone())));

    let deadline = Instant::now() + TIMEOUT;
    loop {
        session.drain_events();
        if grid_text(&session).contains("early-mark") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for early-mark"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        mirror
            .0
            .lock()
            .unwrap()
            .windows(b"early-mark".len())
            .any(|w| w == b"early-mark"),
        "the mirror missed the child's first output"
    );

    // Detaching stops the tap: the second print reaches the grid but
    // never the mirror.
    session.set_output_mirror(None);
    session.send_bytes(b"\n").expect("input should be accepted");

    let deadline = Instant::now() + TIMEOUT;
    loop {
        session.drain_events();
        if grid_text(&session).contains("late-mark") {
            break;
        }
        assert!(Instant::now() < deadline, "timed out waiting for late-mark");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !mirror
            .0
            .lock()
            .unwrap()
            .windows(b"late-mark".len())
            .any(|w| w == b"late-mark"),
        "the detached mirror still received output"
    );
}

/// A temp dir that removes itself on drop: the ConPTY spike's fixture
/// dir, spawned as the child's cwd so `type` needs no path quoting.
#[cfg(windows)]
struct FixtureDir(std::path::PathBuf);

#[cfg(windows)]
impl FixtureDir {
    fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("ait-term-conpty-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("fixture dir should create");
        Self(path)
    }
}

#[cfg(windows)]
impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Spec R16's gating spike (testing decisions): OSC bytes a ConPTY
/// child writes to its console must reach the session event stream
/// intact — ConPTY's passthrough of the AIT-private 7700 series is the
/// one unportable assumption the Windows round builds on. `cmd /c
/// type` replays a byte-exact fixture so the assertion never depends
/// on a shell's escape-string quoting.
#[cfg(windows)]
#[test]
fn conpty_passes_osc_bytes_through_to_session_events() {
    // The same byte families the unix snippets emit and the pwsh
    // snippet will (Spec R16-T2/T3): the private hook channel, prompt
    // marks — one carrying an exit-code payload — and a cwd report.
    let fixture = FixtureDir::new("osc");
    std::fs::write(
        fixture.0.join("osc.raw"),
        b"\x1b]7700;init_shell;pwsh\x07\x1b]133;A\x07\x1b]133;D;0\x07\x1b]7;C:\\ait-cwd\x07",
    )
    .expect("OSC fixture should write");

    let mut session = spawn(
        "cmd.exe",
        &[
            OsString::from("/c"),
            OsString::from("type"),
            OsString::from("osc.raw"),
        ],
        LiveSessionOptions {
            cwd: Some(fixture.0.clone()),
            ..Default::default()
        },
    );

    // Collect every observed OSC event until the child exits.
    let deadline = Instant::now() + TIMEOUT;
    let mut observed = Vec::new();
    let status = 'outer: loop {
        for event in session.drain_events() {
            match event {
                LiveEvent::Osc(event) => observed.push(event),
                LiveEvent::Exited(status) => break 'outer status,
                LiveEvent::OutputReady => {}
                _ => {}
            }
        }
        assert!(Instant::now() < deadline, "timed out waiting for Exited");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(status.success());
    assert_child_gone(session.child_pid(), "still exists after Exited");

    assert!(
        observed.iter().any(|event| matches!(
            event,
            SessionEvent::Private { code, params }
                if code == b"7700" && params.iter().any(|p| p == b"init_shell")
        )),
        "the 7700 hook channel never arrived: {observed:?}"
    );
    assert!(
        observed.iter().any(|event| matches!(
            event,
            SessionEvent::PromptMark {
                kind: PromptMarkKind::PromptStart,
                ..
            }
        )),
        "OSC 133;A never arrived: {observed:?}"
    );
    assert!(
        observed.iter().any(|event| matches!(
            event,
            SessionEvent::PromptMark {
                kind: PromptMarkKind::CommandEnd,
                payload,
            } if payload.as_deref() == Some("0")
        )),
        "OSC 133;D;0 never arrived: {observed:?}"
    );
    assert!(
        observed.iter().any(|event| matches!(
            event,
            SessionEvent::CwdChanged(path) if path.ends_with("ait-cwd")
        )),
        "OSC 7 never arrived: {observed:?}"
    );
}
