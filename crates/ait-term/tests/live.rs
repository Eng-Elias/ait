//! Live-PTY tests at the public `LiveSession` seam (Spec R01: a small second
//! tier covering spawn/resize/exit paths only — everything else is byte-fed
//! in `session.rs`/`keys.rs`). These spawn real processes in a real PTY, so
//! every wait is bounded by a timeout.

use std::ffi::OsString;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ait_term::{
    ExitStatus, Key, KeyEvent, LiveEvent, LiveSession, LiveSessionOptions, Modifiers,
    SessionOptions, Size,
};

/// Serializes the tests that mutate the process-wide `SHELL` variable.
static SHELL_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Generous bound for every wait below: even a loaded CI runner finishes
/// these tiny programs in well under this.
const TIMEOUT: Duration = Duration::from_secs(15);

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
fn wait_for_exit(session: &mut LiveSession) -> ExitStatus {
    match wait_for_event(session, |event| matches!(event, LiveEvent::Exited(_))) {
        LiveEvent::Exited(status) => status,
        other => panic!("expected Exited, got {other:?}"),
    }
}

/// All text in the grid: scrollback then viewport, rows trimmed and joined.
fn grid_text(session: &LiveSession) -> String {
    let grid = session.grid();
    grid.scrollback_rows()
        .chain(grid.visible_rows())
        .map(|row| row.text().trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

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
    let pid = session.child_pid().expect("spawned child has a pid");
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "child {pid} still exists after Exited"
    );
}

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
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "child {pid} survived teardown"
    );
}

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
