//! `ait-testkit` agent-layer self-tests (Spec E01, ticket E01-T2): the
//! record/replay spine driving a plain interactive `/bin/sh` with a stub
//! [`Driver`] — resolve → record → replay → drift-fail round trips, all
//! without `ait` or a model. Recordings are written under fresh temp
//! dirs via `set_recordings_dir` (committed ones live under each
//! consuming crate's `tests/recordings/`).

mod common;

use std::fs;
use std::time::{Duration, Instant};

use ait_testkit::{
    text_hash, CursorState, Driver, E2e, Input, Op, ScreenState, Size, SpawnConfig, Verdict,
};
use common::{step_file, TestDir};

const SIZE: Size = Size {
    columns: 80,
    lines: 24,
};

/// Interactive `/bin/sh` (pinned `PS1="$ "`) at the default size.
fn sh() -> E2e {
    sh_at(SIZE)
}

fn sh_at(size: Size) -> E2e {
    E2e::spawn(SpawnConfig::new("/bin/sh", &[], size)).expect("spawn /bin/sh")
}

/// Wait for the `$ ` prompt — the shell is up and idle.
fn wait_for_prompt(app: &mut E2e) {
    app.wait_for_screen("the shell prompt", |app| app.screen_text().contains('$'));
}

/// A stub `Driver` — the stand-in for T3's Command backing and any
/// model: returns canned inputs for `act`, a canned verdict for
/// `assert`.
struct StubDriver {
    inputs: Vec<Input>,
    verdict: Verdict,
}

impl StubDriver {
    fn acting(inputs: Vec<Input>) -> Self {
        Self {
            inputs,
            verdict: Verdict::Pass,
        }
    }
}

impl Driver for StubDriver {
    fn act(&mut self, _goal: &str, _screen: &ScreenState) -> Vec<Input> {
        self.inputs.clone()
    }

    fn assert(&mut self, _claim: &str, _screen: &ScreenState) -> Verdict {
        self.verdict.clone()
    }
}

/// `echo MARK` typed literally plus the `Enter` a terminal sends.
fn echo_mark() -> Vec<Input> {
    vec![Input::text("echo AGENT-MARK"), Input::key("Enter")]
}

/// The full round trip: a stub driver resolves the step once (record
/// mode), the jsonl lands under `<dir>/<test>/00.jsonl` as semantic
/// inputs, and a fresh spawn replays it with *no driver at all* — the
/// `Authored` default — proving replay sends the recorded inputs
/// through the real encoding path.
#[test]
fn act_step_records_then_replays_driverless() {
    let dir = TestDir::fresh();
    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(true);
    app.set_driver(StubDriver::acting(echo_mark()));

    app.agent().act("show the agent mark");

    let file = step_file(dir.path(), 0);
    let recording = fs::read_to_string(&file).expect("the recording was written");
    let mut lines = recording.lines();
    let header = lines.next().expect("a header line");
    assert!(header.contains(r#""op":"act""#), "header: {header}");
    assert!(
        header.contains(r#""text":"show the agent mark""#),
        "header: {header}"
    );
    assert!(header.contains(r#""text_hash""#), "header: {header}");
    assert!(header.contains(r#""screen_hash""#), "header: {header}");
    let inputs: Vec<&str> = lines.collect();
    assert_eq!(
        inputs,
        [r#"{"text":"echo AGENT-MARK"}"#, r#"{"key":"Enter"}"#],
        "semantic inputs, one per line"
    );
    drop(app);

    // Replay: fresh PTY, default `Authored` driver, record mode off —
    // zero model involvement.
    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(false);

    app.agent().act("show the agent mark");

    assert!(
        app.screen_text().contains("AGENT-MARK"),
        "the recorded inputs were re-sent:\n{}",
        app.screen_text()
    );
}

/// `assert` steps record the driver's verdict and replay it the same
/// way — a recorded pass plus a matching screen hash is a pass.
#[test]
fn assert_step_records_its_verdict_and_replays() {
    let dir = TestDir::fresh();
    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(true);
    app.set_driver(StubDriver::acting(echo_mark()));

    app.agent().act("show the agent mark");
    app.agent().assert("the mark is on screen");

    let recording =
        fs::read_to_string(step_file(dir.path(), 1)).expect("the assert recording was written");
    let header = recording.lines().next().expect("a header line");
    assert!(header.contains(r#""op":"assert""#), "header: {header}");
    assert!(header.contains(r#""verdict":"pass""#), "header: {header}");
    drop(app);

    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(false);

    app.agent().act("show the agent mark");
    app.agent().assert("the mark is on screen");
}

/// `Authored` + no recording + no record flag → the failure tells the
/// author how to produce one.
#[test]
#[should_panic(expected = "AIT_E2E_RECORD")]
fn missing_recording_fails_with_guidance() {
    let dir = TestDir::fresh();
    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(false);

    app.agent().act("never recorded");
}

/// Editing a step's goal text invalidates its recording — the step-text
/// hash guard.
#[test]
#[should_panic(expected = "invalidates its recording")]
fn edited_step_text_invalidates_the_recording() {
    let dir = TestDir::fresh();
    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(true);
    app.set_driver(StubDriver::acting(echo_mark()));
    app.agent().act("show the agent mark");
    drop(app);

    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(false);

    app.agent().act("show a slightly different mark");
}

/// DRIFT: the app now prints extra output the recording never saw, so
/// the settled screen stops matching the recorded hash — a first-class
/// failure, not a warning.
#[test]
#[should_panic(expected = "DRIFTED")]
fn changed_output_drifts_the_recording() {
    let dir = TestDir::fresh();
    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(true);
    app.set_driver(StubDriver::acting(echo_mark()));
    app.agent().act("show the agent mark");
    drop(app);

    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(false);
    // Observable behavior changed: an extra line the recording lacks.
    app.type_text("echo EXTRA-LINE");
    app.press("Enter");
    app.wait_for_screen("the extra line", |app| {
        // `trim() ==` — the output row alone; the `echo EXTRA-LINE`
        // echo carries `$ echo ` and can't satisfy the wait (std.15).
        app.screen_text()
            .lines()
            .any(|line| line.trim() == "EXTRA-LINE")
    });

    app.agent().act("show the agent mark");
}

/// DRIFT on a different grid size: it can never heal, so replay fails
/// immediately with the recorded-vs-actual diff.
#[test]
#[should_panic(expected = "DRIFTED")]
fn changed_size_drifts_the_recording() {
    let dir = TestDir::fresh();
    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(true);
    app.set_driver(StubDriver::acting(echo_mark()));
    app.agent().act("show the agent mark");
    drop(app);

    let mut app = sh_at(Size::new(60, 20));
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(false);

    app.agent().act("show the agent mark");
}

/// A recording is an authored asset: one written by hand — the way an
/// authoring agent would, with a literal predicted [`ScreenState`] —
/// parses and replays.
#[test]
fn a_hand_authored_recording_replays() {
    let dir = TestDir::fresh();
    let text = "show the hand mark";

    // The settled screen after `echo HAND-MARK` at the prompt, predicted
    // literally: prompt+echo on row 0, output on row 1, fresh prompt on
    // row 2 (trailing `$ ` trims to `$`), cursor after it.
    let mut rows = vec![String::new(); SIZE.lines];
    rows[0] = "$ echo HAND-MARK".to_string();
    rows[1] = "HAND-MARK".to_string();
    rows[2] = "$".to_string();
    let screen = ScreenState {
        columns: SIZE.columns,
        lines: SIZE.lines,
        rows,
        cursor: CursorState {
            line: 2,
            column: 2,
            shape: "Block".to_string(),
        },
    };

    let file = step_file(dir.path(), 0);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(
        &file,
        format!(
            "{}\n{}\n{}\n",
            serde_json::json!({
                "v": 1,
                "step": 0,
                "op": "act",
                "text": text,
                "text_hash": text_hash(Op::Act, text),
                "program": "/bin/sh",
                "screen": screen,
                "screen_hash": screen.hash_hex(),
            }),
            r#"{"text":"echo HAND-MARK"}"#,
            r#"{"key":"Enter"}"#,
        ),
    )
    .unwrap();

    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(false);

    app.agent().act(text);

    assert!(app.screen_text().contains("HAND-MARK"));
}

/// The recording's `program` field is provenance only: there is no
/// binary hash, so a different binary path — and by extension a rebuilt
/// binary without behavior change — invalidates nothing.
#[test]
fn program_field_is_provenance_not_a_guard() {
    let dir = TestDir::fresh();
    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(true);
    app.set_driver(StubDriver::acting(echo_mark()));
    app.agent().act("show the agent mark");
    drop(app);

    // Rewrite the header as if a different binary had been recorded.
    let file = step_file(dir.path(), 0);
    let recording = fs::read_to_string(&file).unwrap();
    fs::write(
        &file,
        recording.replacen(
            r#""program":"/bin/sh""#,
            r#""program":"/elsewhere/ait-2f4a""#,
            1,
        ),
    )
    .unwrap();

    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(false);

    app.agent().act("show the agent mark");
}

/// Record mode re-resolves through the driver and rewrites an existing
/// recording — even a corrupt one.
#[test]
fn record_mode_rewrites_the_recording() {
    let dir = TestDir::fresh();
    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(true);
    app.set_driver(StubDriver::acting(echo_mark()));
    app.agent().act("show the agent mark");
    drop(app);

    fs::write(step_file(dir.path(), 0), "this is not json\n").unwrap();

    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(true);
    app.set_driver(StubDriver::acting(echo_mark()));

    app.agent().act("show the agent mark");

    let recording = fs::read_to_string(step_file(dir.path(), 0)).unwrap();
    assert!(
        recording.contains(r#""op":"act""#),
        "the file was rewritten: {recording}"
    );
}

/// A `fail` verdict at record time is a test failure carrying the
/// driver's reason.
#[test]
#[should_panic(expected = "no such thing on screen")]
fn failing_verdict_fails_at_record_time() {
    let dir = TestDir::fresh();
    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(true);
    app.set_driver(StubDriver {
        inputs: Vec::new(),
        verdict: Verdict::Fail {
            reason: "no such thing on screen".to_string(),
        },
    });

    app.agent().assert("an impossible claim");
}

/// `AIT_E2E_RECORD` is the env flag that flips a spawn into record
/// mode (`set_record_mode` exists so tests needn't race on env).
#[test]
fn record_env_flag_enables_record_mode() {
    // The only env-mutating test here; every other test sets record
    // mode explicitly, so the brief window can't corrupt them.
    std::env::set_var("AIT_E2E_RECORD", "1");
    let app = sh();
    std::env::remove_var("AIT_E2E_RECORD");
    assert!(app.record_mode(), "AIT_E2E_RECORD=1 at spawn");

    std::env::set_var("AIT_E2E_RECORD", "0");
    let app = sh();
    std::env::remove_var("AIT_E2E_RECORD");
    assert!(!app.record_mode(), "AIT_E2E_RECORD=0 means replay");
}

/// The spawn's fresh temp cwd is redacted to `[cwd]` inside
/// `ScreenState`, so cwd-printing apps still produce stable recordings.
#[test]
fn screen_state_redacts_the_spawn_cwd() {
    let mut app = sh();
    wait_for_prompt(&mut app);
    app.type_text("pwd");
    app.press("Enter");
    let cwd = app.cwd().to_string_lossy().into_owned();
    app.wait_for_screen("the printed cwd", |app| app.screen_text().contains(&cwd));

    let screen = app.screen_state();

    assert!(screen.rows.iter().any(|row| row.contains("[cwd]")));
    assert!(!screen.rows.iter().any(|row| row.contains(&cwd)));
}

/// Regression: a screen that changes on every poll must still return
/// from the record-time settle at its overall budget — the deadline is
/// checked before the churn/stability tracking, so churn can't starve
/// the budget check into an infinite loop.
#[test]
fn record_time_settle_bounds_a_churning_screen() {
    let dir = TestDir::fresh();
    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(true);
    app.set_driver(StubDriver::acting(Vec::new()));
    // An ever-incrementing counter redraws the screen faster than the
    // poll interval, so it never stays unchanged long enough to count
    // as settled — only the settle budget can end the wait.
    app.type_text("i=0; while true; do i=$((i+1)); echo $i; done");
    app.press("Enter");
    let started = Instant::now();

    app.agent().assert("the counter is climbing");

    assert!(
        started.elapsed() < Duration::from_secs(10),
        "settle returned at its budget, not via stability"
    );
}
