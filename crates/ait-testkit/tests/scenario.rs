//! `ait-testkit` scenario-layer self-tests (Spec E01, ticket E01-T4):
//! TOML scenario files parsed and run against a plain `/bin/sh`, so the
//! runner's verbs are proven without `ait` involved — deterministic
//! steps, malformed-file diagnostics and the `expect_clipboard` event
//! verb included. Agent-step scenarios are exercised end to end in
//! `crates/ait/tests/scenarios.rs`.

mod common;

use ait_testkit::{E2e, Scenario, Size};
use common::TestDir;

const SIZE: Size = Size {
    columns: 80,
    lines: 24,
};

/// Interactive `/bin/sh` with `toml`'s scenario run against it — the
/// file-on-disk shape the runner consumes (load errors name it), kept
/// inside a [`TestDir`] so it is read eagerly and removed on drop.
fn run(toml: &str) -> E2e {
    let dir = TestDir::fresh();
    let file = dir.write("case.toml", toml);
    let scenario = Scenario::load(&file).expect("scenario parses");
    let mut app = E2e::spawn(scenario.spawn_config("/bin/sh")).expect("spawn /bin/sh");
    scenario.run(&mut app);
    app
}

/// The text/keys/wait/expect spine end to end: `text` types literally,
/// `keys` presses a named chord, `wait_text` gates, `expect_regex` and
/// `expect_cursor` assert on the rendered grid.
#[test]
fn deterministic_verbs_drive_a_shell_scenario() {
    let mut app = run(r#"
feature = "selftest"
size = [80, 24]

[[steps]]
wait_text = "$"

[[steps]]
text = "echo MARKER"

[[steps]]
keys = ["Enter"]

[[steps]]
wait_text = "MARKER"

[[steps]]
expect_text = "MARKER"

[[steps]]
text = "printf 'A\\tB'; exit"

[[steps]]
keys = ["Enter"]

[[steps]]
expect_regex = "(?m)^A\\t\\s*B$"

[[steps]]
expect_cursor = [3, 9]
"#);
    // `printf` left the cursor right of `B` (tab → column 8); `exit`
    // then ended the shell mid-scenario.
    let status = app.wait_for_exit();
    assert!(status.success());
}

/// `resize` forwards the new winsize mid-scenario.
#[test]
fn resize_verb_reaches_the_inner_pty() {
    run(r#"
feature = "selftest"

[[steps]]
resize = [100, 40]

[[steps]]
text = "stty size; exit\n"

[[steps]]
expect_text = "40 100"
"#);
}

/// The `bytes` verb takes lowercase hex — the same spelling recording
/// inputs use (`od -tx1` style).
#[test]
fn bytes_verb_decodes_hex() {
    run(r#"
feature = "selftest"

[[steps]]
text = "read x; echo saw:$x; exit\n"

[[steps]]
wait_text = "read x"

[[steps]]
bytes = "68656c6c6f0a"

[[steps]]
expect_text = "saw:hello"
"#);
}

/// `mouse` writes the SGR bytes the wire spells: a wheel-up tick at
/// the 0-based cell (9,4) — `col`/`row`, the same convention as
/// `expect_cursor` — encodes to the wire's 1-based (10,5):
/// `ESC [ < 6 4 ; 1 0 ; 5 M` — eleven bytes, hex-dumped by `od`.
#[test]
fn mouse_verb_writes_sgr_sequences() {
    run(r#"
feature = "selftest"

[[steps]]
text = "stty -icanon -echo; printf READY; dd bs=1 count=11 2>/dev/null | od -An -tx1; exit\n"

[[steps]]
wait_text = "READY"

[[steps]]
mouse = { event = "wheel_up", col = 9, row = 4 }

[[steps]]
expect_text = "1b 5b 3c 36 34 3b 31 30 3b 35 4d"
"#);
}

/// `expect_exit` waits for the child and checks its code.
#[test]
fn expect_exit_checks_the_child_status() {
    run(r#"
feature = "selftest"

[[steps]]
text = "exit 7\n"

[[steps]]
expect_exit = 7
"#);
}

/// `expect_clipboard` sees an OSC 52 copy as an event: the shell
/// `printf`s the OSC 52 write and the harness's opted-in session
/// surfaces the decoded contents (`SEVMTE8tU01BQ0s=` is
/// `HELLO-SMACK`).
#[test]
fn expect_clipboard_observes_osc52_writes() {
    run(r#"
feature = "selftest"

[[steps]]
text = "printf '\\033]52;c;SEVMTE8tU01BQ0s=\\007'; exit\n"

[[steps]]
expect_clipboard = "HELLO-SMACK"
"#);
}

/// A scenario `env` table lands in the child after the pinned vars, so
/// a scenario can pin a different prompt.
#[test]
fn env_pairs_override_the_pinned_prompt() {
    run(r#"
feature = "selftest"

[env]
PS1 = "SCENARIO> "

[[steps]]
expect_text = "SCENARIO>"
"#);
}

/// Every scenario carries its `feature` tag and `size` through to the
/// parsed value; `size` defaults to the standard 80x24.
#[test]
fn feature_tag_and_default_size_parse() {
    let dir = TestDir::fresh();
    let file = dir.write(
        "case.toml",
        r#"
feature = "scrollback"

[[steps]]
wait_text = "x"
"#,
    );
    let scenario = Scenario::load(&file).unwrap();
    assert_eq!(scenario.feature, "scrollback");
    assert_eq!(scenario.size, SIZE);
}

/// Malformed input fails naming the file and the offending step — not
/// a bare toml panic.
#[test]
fn malformed_scenarios_name_file_and_step() {
    let dir = TestDir::fresh();
    // Step 1 has no verb at all.
    let file = dir.write(
        "case.toml",
        r#"
feature = "bad"

[[steps]]
wait_text = "x"

[[steps]]
bogus_verb = 3
"#,
    );
    let err = Scenario::load(&file).expect_err("bogus verb rejected");
    let message = err.to_string();
    assert!(message.contains("case.toml"), "names the file: {message}");
    assert!(message.contains("step 1"), "names the step: {message}");
    assert!(message.contains("bogus_verb"), "names the verb: {message}");

    // Two verbs in one step is ambiguous.
    let file = dir.write(
        "case.toml",
        r#"
feature = "bad"

[[steps]]
keys = ["Enter"]
text = "x"
"#,
    );
    let err = Scenario::load(&file).expect_err("two verbs rejected");
    assert!(err.to_string().contains("step 0"), "{}", err);

    // A bad chord is caught at load, not mid-run.
    let file = dir.write(
        "case.toml",
        r#"
feature = "bad"

[[steps]]
keys = ["Ctrl+Bogus"]
"#,
    );
    let err = Scenario::load(&file).expect_err("bad chord rejected at load");
    assert!(err.to_string().contains("step 0"), "{}", err);

    // Bad hex in `bytes`.
    let file = dir.write(
        "case.toml",
        r#"
feature = "bad"

[[steps]]
bytes = "zz"
"#,
    );
    let err = Scenario::load(&file).expect_err("bad hex rejected");
    assert!(err.to_string().contains("step 0"), "{}", err);

    // Bad regex.
    let file = dir.write(
        "case.toml",
        r#"
feature = "bad"

[[steps]]
expect_regex = "(["
"#,
    );
    let err = Scenario::load(&file).expect_err("bad regex rejected");
    assert!(err.to_string().contains("step 0"), "{}", err);

    // A missing feature tag is a load error, not a panic.
    let file = dir.write(
        "case.toml",
        r#"
[[steps]]
wait_text = "x"
"#,
    );
    Scenario::load(&file).expect_err("feature is required");
}

/// A failing step reports the scenario name and step index — the
/// runner catches the verb's panic and re-raises it in context.
/// (Step 1's wait fails fast: the child already exited at step 0.)
#[test]
#[should_panic(expected = "case step 1")]
fn a_failing_step_reports_its_index() {
    let dir = TestDir::fresh();
    let file = dir.write(
        "case.toml",
        r#"
feature = "selftest"

[[steps]]
text = "exit\n"

[[steps]]
wait_text = "never-appears"
"#,
    );
    let scenario = Scenario::load(&file).unwrap();
    let mut app = E2e::spawn(scenario.spawn_config("/bin/sh")).expect("spawn");
    scenario.run(&mut app);
}
