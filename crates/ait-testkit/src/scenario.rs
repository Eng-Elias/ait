//! The declarative **scenario** format and its runner (Spec E01,
//! ticket E01-T4): acceptance criteria as TOML — a `feature` tag, an
//! optional `size`, ordered `[[steps]]` — executed through the same
//! [`E2e`] verbs code-first tests call. A scenario with no `act`/
//! `assert` steps is pure deterministic replay from day one.
//!
//! ```toml
//! feature = "tabs"               # required — the tag a later filtered-run
//!                                # layer maps changed paths to
//! size = [80, 24]                # optional, defaults to 80x24
//! args = ["--version"]           # optional argv for the spawned program
//! env = { PS1 = "E2E> " }        # optional pairs, applied after the
//!                                # pinned SHELL/PS1 (and so override them)
//!
//! [[steps]]                      # exactly one verb per step
//! wait_text = "· sh · — · running"
//!
//! [[steps]]
//! keys = ["Alt+T"]               # chord strings, the same parser as
//!                                # `E2e::press` ("Ctrl+PageDown", "F5", …)
//!
//! [[steps]]
//! expect_text = "2:sh"
//! ```
//!
//! ## Steps
//!
//! | key | value | effect |
//! |---|---|---|
//! | `keys` | `["Alt+T", …]` | press each chord in order |
//! | `text` | `"…"` | literal text to the program's stdin (`\n` stays `\n` — use `keys = ["Enter"]` for the `\r` terminals send) |
//! | `bytes` | `"1b5b…"` | raw bytes, lowercase hex pairs — the same spelling [`Input::Bytes`](crate::Input) uses (`od -tx1` style); whitespace ignored |
//! | `mouse` | `{event = "…", col = 9, row = 4}` | one SGR mouse event at the 0-based screen cell `(col, row)` — the same convention `expect_cursor` uses; the SGR encoding adds the 1 the wire requires — `event`: `"down"`/`"drag"`/`"up"`/`"wheel_up"`/`"wheel_down"`, plus optional `button`: `"left"` (default)/`"middle"`/`"right"` |
//! | `resize` | `[w, h]` | resize the PTY and grid |
//! | `wait_text` | `"…"` | bounded wait until the screen contains it |
//! | `expect_text` | `"…"` | assert the screen shows it (bounded wait — assertions on a live screen must tolerate render latency) |
//! | `expect_regex` | `"…"` | assert the screen text matches |
//! | `expect_cursor` | `[line, column]` | assert the renderable cursor's 0-based position |
//! | `expect_exit` | `0` | wait for the program to exit with this code |
//! | `expect_clipboard` | `"…"` | assert an OSC 52 `ClipboardWrite` carried exactly this text (the harness session opts into `osc52`) |
//! | `act` | `"…"` | agent step — resolved/recorded or replayed like `agent().act` |
//! | `assert` | `"…"` | agent step — replayed like `agent().assert` |
//!
//! `wait_text` gates progress ("wait until"); `expect_*` reads as the
//! assertion. Both wait boundedly — a missing expectation fails on
//! timeout with the screen in the message (Spec E01's "deterministic,
//! bounded waits" — the harness never sleeps fixed amounts).
//!
//! Malformed files fail at [`Scenario::load`] naming the file and the
//! step; a failing step fails its [`run`](Scenario::run) naming the
//! scenario and step — never a bare panic.

use std::ffi::OsString;
use std::fmt;
use std::path::Path;

use regex::Regex;
use serde::Deserialize;

use crate::chord::parse_chord;
use crate::driver::decode_hex;
use crate::evidence::panic_text;
use crate::harness::{E2e, SpawnConfig};
use crate::mouse::{MouseButton, MouseEvent};
use ait_term::Size;

/// The default spawn size — the standard 80x24 window the scenario map
/// was authored against.
const DEFAULT_SIZE: Size = Size {
    columns: 80,
    lines: 24,
};

/// The verbs a `[[steps]]` table may carry — exactly one per step.
const VERBS: &[&str] = &[
    "keys",
    "text",
    "bytes",
    "mouse",
    "resize",
    "wait_text",
    "expect_text",
    "expect_regex",
    "expect_cursor",
    "expect_exit",
    "expect_clipboard",
    "act",
    "assert",
];

/// A declarative test of user-visible behavior: parsed TOML, ready to
/// [`run`](Self::run) against a spawned program.
#[derive(Debug)]
pub struct Scenario {
    /// The `feature` tag — metadata a later filtered-run layer maps
    /// changed paths to (Spec E01: tags land now, the map is deferred).
    pub feature: String,
    /// The spawn size (`size = [w, h]`, default 80x24).
    pub size: Size,
    /// `argv[1..]` for the spawned program (`args`).
    pub args: Vec<OsString>,
    /// Environment pairs applied after the pinned `SHELL`/`PS1`
    /// (`env = { … }`).
    pub env: Vec<(OsString, OsString)>,
    /// The ordered steps.
    pub steps: Vec<Step>,
    /// The scenario's name — its file stem, used in failure messages
    /// and (via the runner's thread naming) dump/recording dirs.
    name: String,
    /// Where it was loaded from, for error messages.
    source: String,
}

/// One `[[steps]]` entry — one user action or one assertion.
#[derive(Debug)]
pub enum Step {
    /// `keys` — named chords pressed in order.
    Keys(Vec<String>),
    /// `text` — literal text.
    Text(String),
    /// `bytes` — raw bytes (decoded from hex at load).
    Bytes(Vec<u8>),
    /// `mouse` — one SGR event at the 0-based screen cell (the same
    /// convention `expect_cursor` uses; the SGR wire encoding adds the
    /// 1 the protocol requires).
    Mouse {
        /// The event to send.
        event: MouseEvent,
        /// 0-based screen column (`col` in the file).
        column: u16,
        /// 0-based screen row (`row` in the file).
        row: u16,
    },
    /// `resize` — new PTY size.
    Resize(Size),
    /// `wait_text` — gate until the screen contains it.
    WaitText(String),
    /// `expect_text` — assert the screen contains it.
    ExpectText(String),
    /// `expect_regex` — assert the screen matches.
    ExpectRegex {
        /// The pattern (for messages).
        pattern: String,
        /// Compiled at load so a bad pattern fails then, not mid-run.
        regex: Regex,
    },
    /// `expect_cursor` — assert the cursor's 0-based position.
    ExpectCursor {
        /// Viewport row.
        line: usize,
        /// Column.
        column: usize,
    },
    /// `expect_exit` — wait for exit and check the code.
    ExpectExit(u32),
    /// `expect_clipboard` — assert an OSC 52 write carried this text.
    ExpectClipboard(String),
    /// `act` — an agent step (recording replay or record-time resolve).
    Act(String),
    /// `assert` — an agent step.
    Assert(String),
}

impl Step {
    /// The verb's TOML key, for failure messages.
    fn verb(&self) -> &'static str {
        match self {
            Self::Keys(_) => "keys",
            Self::Text(_) => "text",
            Self::Bytes(_) => "bytes",
            Self::Mouse { .. } => "mouse",
            Self::Resize(_) => "resize",
            Self::WaitText(_) => "wait_text",
            Self::ExpectText(_) => "expect_text",
            Self::ExpectRegex { .. } => "expect_regex",
            Self::ExpectCursor { .. } => "expect_cursor",
            Self::ExpectExit(_) => "expect_exit",
            Self::ExpectClipboard(_) => "expect_clipboard",
            Self::Act(_) => "act",
            Self::Assert(_) => "assert",
        }
    }

    /// Execute the step against `e2e`.
    fn run(&self, e2e: &mut E2e) {
        match self {
            Self::Keys(chords) => {
                for chord in chords {
                    e2e.press(chord);
                }
            }
            Self::Text(text) => e2e.type_text(text),
            Self::Bytes(bytes) => e2e.send_bytes(bytes),
            // Scenario cells are 0-based like the grid view and
            // `expect_cursor`; the wire (`send_mouse`) wants 1-based.
            Self::Mouse { event, column, row } => {
                e2e.send_mouse(*event, column.saturating_add(1), row.saturating_add(1))
            }
            Self::Resize(size) => e2e.resize(*size),
            Self::WaitText(text) => {
                e2e.wait_for_screen(&format!("wait_text {text:?}"), |e2e| {
                    e2e.screen_text().contains(text.as_str())
                });
            }
            Self::ExpectText(text) => {
                e2e.wait_for_screen(&format!("expect_text {text:?}"), |e2e| {
                    e2e.screen_text().contains(text.as_str())
                });
            }
            Self::ExpectRegex { pattern, regex } => {
                e2e.wait_for_screen(&format!("expect_regex {pattern:?}"), |e2e| {
                    regex.is_match(&e2e.screen_text())
                });
            }
            Self::ExpectCursor { line, column } => {
                e2e.wait_for_screen(&format!("expect_cursor [{line}, {column}]"), |e2e| {
                    let cursor = e2e.cursor();
                    cursor.line == *line && cursor.column == *column
                });
            }
            Self::ExpectExit(code) => {
                let status = e2e.wait_for_exit();
                assert_eq!(
                    status.code, *code,
                    "expect_exit {code}: the program exited {status:?}"
                );
            }
            Self::ExpectClipboard(text) => {
                e2e.wait_for_screen(&format!("expect_clipboard {text:?}"), |e2e| {
                    e2e.clipboard_writes().iter().any(|write| write == text)
                });
            }
            Self::Act(goal) => e2e.agent().act(goal),
            Self::Assert(claim) => e2e.agent().assert(claim),
        }
    }
}

/// Why a scenario file failed to load — file and (when known) step.
#[derive(Debug)]
pub struct ScenarioError {
    file: String,
    step: Option<usize>,
    message: String,
}

impl ScenarioError {
    fn file(file: &str, message: impl Into<String>) -> Self {
        Self {
            file: file.to_string(),
            step: None,
            message: message.into(),
        }
    }

    fn step(file: &str, step: usize, message: impl Into<String>) -> Self {
        Self {
            file: file.to_string(),
            step: Some(step),
            message: message.into(),
        }
    }
}

impl fmt::Display for ScenarioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.step {
            Some(step) => write!(f, "{}: step {step}: {}", self.file, self.message),
            None => write!(f, "{}: {}", self.file, self.message),
        }
    }
}

impl std::error::Error for ScenarioError {}

/// The file's top level, parsed loosely so every diagnostic can name
/// the step it came from rather than toml's internal path.
#[derive(Deserialize)]
struct RawScenario {
    feature: String,
    #[serde(default)]
    size: Option<[usize; 2]>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: toml::Table,
    #[serde(default)]
    steps: Vec<toml::Table>,
}

/// The `mouse = { … }` inline table — `col`/`row` are the 0-based
/// screen cell, matching `expect_cursor`'s convention.
#[derive(Deserialize)]
struct RawMouse {
    event: String,
    col: u16,
    row: u16,
    #[serde(default)]
    button: Option<String>,
}

impl Scenario {
    /// Read and validate the scenario at `path`. Every error names the
    /// file — and the step index when the problem sits inside a step.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ScenarioError> {
        let path = path.as_ref();
        let source = path.display().to_string();
        let text = std::fs::read_to_string(path)
            .map_err(|e| ScenarioError::file(&source, format!("could not read: {e}")))?;
        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "scenario".to_string());
        Self::parse(name, &source, &text)
    }

    /// Parse `text` as the scenario named `name` from `source`.
    fn parse(name: String, source: &str, text: &str) -> Result<Self, ScenarioError> {
        let raw: RawScenario = toml::from_str(text)
            .map_err(|e| ScenarioError::file(source, format!("invalid TOML: {e}")))?;
        let env = raw
            .env
            .into_iter()
            .map(|(key, value)| match value {
                toml::Value::String(value) => Ok((OsString::from(key), OsString::from(value))),
                other => Err(ScenarioError::file(
                    source,
                    format!("env.{key} must be a string, got {other}"),
                )),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let steps = raw
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| parse_step(source, index, step))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            feature: raw.feature,
            size: raw
                .size
                .map(|[columns, lines]| Size::new(columns, lines))
                .unwrap_or(DEFAULT_SIZE),
            args: raw.args.iter().map(OsString::from).collect(),
            env,
            steps,
            name,
            source: source.to_string(),
        })
    }

    /// A [`SpawnConfig`] for `program` honoring the scenario's `size`,
    /// `args` and `env` — what the runner hands `E2e::spawn`.
    pub fn spawn_config(&self, program: impl Into<OsString>) -> SpawnConfig {
        let mut config = SpawnConfig::new(program, &[], self.size);
        config.args = self.args.iter().map(OsString::from).collect();
        config.env = self.env.clone();
        config
    }

    /// Execute every step in order. A step's own panic (a mistyped
    /// chord, a timed-out wait, a drifted recording) is re-raised
    /// naming this scenario and the step index, so failure output
    /// always says *which* step of *which* file failed.
    ///
    /// # Panics
    /// On the first failing step, with context.
    pub fn run(&self, e2e: &mut E2e) {
        for (index, step) in self.steps.iter().enumerate() {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| step.run(e2e)));
            if let Err(payload) = outcome {
                panic!(
                    "{} step {index} ({}) failed: {}",
                    self.name,
                    step.verb(),
                    panic_text(payload.as_ref()),
                );
            }
        }
    }

    /// The scenario's name (its file stem).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The path it was loaded from.
    pub fn source(&self) -> &str {
        &self.source
    }
}

/// Turn one raw `[[steps]]` table into a typed [`Step`] — or explain
/// why it isn't one, naming the file and step index.
fn parse_step(source: &str, index: usize, table: &toml::Table) -> Result<Step, ScenarioError> {
    let fail = |message: String| ScenarioError::step(source, index, message);

    let unknown: Vec<&String> = table
        .keys()
        .filter(|key| !VERBS.contains(&key.as_str()))
        .collect();
    if !unknown.is_empty() {
        let names: Vec<String> = unknown.iter().map(|key| format!("`{key}`")).collect();
        return Err(fail(format!(
            "unknown verb(s) {}; known verbs: {}",
            names.join(", "),
            VERBS.join(", ")
        )));
    }

    let verbs: Vec<&String> = table
        .keys()
        .filter(|key| VERBS.contains(&key.as_str()))
        .collect();
    if verbs.len() != 1 {
        let found = if verbs.is_empty() {
            "none".to_string()
        } else {
            verbs
                .iter()
                .map(|verb| format!("`{verb}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(fail(format!(
            "a step carries exactly one verb, found {found}"
        )));
    }
    let verb = verbs[0].as_str();
    let value = &table[verb];

    /// Deserialize one step's value, re-wording toml's error to name
    /// the verb.
    fn want<T: for<'de> Deserialize<'de>>(
        value: &toml::Value,
        verb: &str,
        what: &str,
    ) -> Result<T, String> {
        T::deserialize(value.clone()).map_err(|e| format!("`{verb}` must be {what}: {e}"))
    }

    match verb {
        "keys" => {
            let chords: Vec<String> = want(value, verb, "a list of chord strings").map_err(fail)?;
            for chord in &chords {
                parse_chord(chord).map_err(|e| fail(format!("`keys` entry {chord:?}: {e}")))?;
            }
            Ok(Step::Keys(chords))
        }
        "text" => Ok(Step::Text(want(value, verb, "a string").map_err(fail)?)),
        "bytes" => {
            let hex: String = want(value, verb, "a hex string").map_err(fail)?;
            Ok(Step::Bytes(
                decode_hex(&hex).map_err(|e| fail(format!("`bytes`: {e}")))?,
            ))
        }
        "mouse" => {
            let mouse: RawMouse = want(
                value,
                verb,
                "a table like \
                 `{ event = \"wheel_up\", col = 9, row = 4 }`",
            )
            .map_err(fail)?;
            let event = parse_mouse_event(&mouse).map_err(fail)?;
            Ok(Step::Mouse {
                event,
                column: mouse.col,
                row: mouse.row,
            })
        }
        "resize" => {
            let [columns, lines]: [usize; 2] =
                want(value, verb, "a `[columns, lines]` pair").map_err(fail)?;
            Ok(Step::Resize(Size::new(columns, lines)))
        }
        "wait_text" => Ok(Step::WaitText(want(value, verb, "a string").map_err(fail)?)),
        "expect_text" => Ok(Step::ExpectText(
            want(value, verb, "a string").map_err(fail)?,
        )),
        "expect_regex" => {
            let pattern: String = want(value, verb, "a regex string").map_err(fail)?;
            let regex = Regex::new(&pattern)
                .map_err(|e| fail(format!("`expect_regex` pattern {pattern:?}: {e}")))?;
            Ok(Step::ExpectRegex { pattern, regex })
        }
        "expect_cursor" => {
            let [line, column]: [usize; 2] =
                want(value, verb, "a `[line, column]` pair").map_err(fail)?;
            Ok(Step::ExpectCursor { line, column })
        }
        "expect_exit" => {
            let code: i64 = want(value, verb, "an exit code").map_err(fail)?;
            let code = u32::try_from(code).map_err(|_| {
                fail(format!(
                    "`expect_exit` must be a non-negative code, got {code}"
                ))
            })?;
            Ok(Step::ExpectExit(code))
        }
        "expect_clipboard" => Ok(Step::ExpectClipboard(
            want(value, verb, "a string").map_err(fail)?,
        )),
        "act" => Ok(Step::Act(want(value, verb, "a goal string").map_err(fail)?)),
        "assert" => Ok(Step::Assert(
            want(value, verb, "a claim string").map_err(fail)?,
        )),
        other => unreachable!("{other} passed the verb check"),
    }
}

/// `mouse.event`/`mouse.button` strings → a [`MouseEvent`].
fn parse_mouse_event(mouse: &RawMouse) -> Result<MouseEvent, String> {
    let button = match mouse.button.as_deref().unwrap_or("left") {
        "left" => MouseButton::Left,
        "middle" => MouseButton::Middle,
        "right" => MouseButton::Right,
        other => {
            return Err(format!(
                "mouse.button must be \"left\"|\"middle\"|\"right\", got {other:?}"
            ))
        }
    };
    match mouse.event.as_str() {
        "down" => Ok(MouseEvent::Down(button)),
        "drag" => Ok(MouseEvent::Drag(button)),
        "up" => Ok(MouseEvent::Up(button)),
        "wheel_up" => Ok(MouseEvent::WheelUp),
        "wheel_down" => Ok(MouseEvent::WheelDown),
        other => Err(format!(
            "mouse.event must be \"down\"|\"drag\"|\"up\"|\"wheel_up\"|\"wheel_down\", \
             got {other:?}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Scenario, ScenarioError> {
        Scenario::parse("case".to_string(), "case.toml", text)
    }

    /// The minimal file: `feature` alone, no steps.
    #[test]
    fn a_feature_only_scenario_parses() {
        let scenario = parse("feature = \"x\"\n").expect("parses");
        assert_eq!(scenario.feature, "x");
        assert_eq!(scenario.size, DEFAULT_SIZE);
        assert!(scenario.steps.is_empty());
    }

    /// `size`/`args`/`env` come through typed.
    #[test]
    fn header_fields_parse() {
        let scenario = parse(
            r#"
feature = "x"
size = [100, 40]
args = ["--version"]
env = { PS1 = "E2E> " }
"#,
        )
        .unwrap();
        assert_eq!(scenario.size, Size::new(100, 40));
        assert_eq!(scenario.args, [OsString::from("--version")]);
        assert_eq!(
            scenario.env,
            [(OsString::from("PS1"), OsString::from("E2E> "))]
        );
    }

    /// Every verb parses into its step.
    #[test]
    fn every_verb_parses() {
        let scenario = parse(
            r##"
feature = "x"
[[steps]]
keys = ["Alt+T", "Enter"]
[[steps]]
text = "hi\n"
[[steps]]
bytes = "1b 5b"
[[steps]]
mouse = { event = "drag", col = 3, row = 4 }
[[steps]]
mouse = { event = "up", col = 3, row = 4, button = "middle" }
[[steps]]
resize = [50, 15]
[[steps]]
wait_text = "a"
[[steps]]
expect_text = "b"
[[steps]]
expect_regex = "c+d"
[[steps]]
expect_cursor = [2, 9]
[[steps]]
expect_exit = 3
[[steps]]
expect_clipboard = "clip"
[[steps]]
act = "do a thing"
[[steps]]
assert = "the thing happened"
"##,
        )
        .expect("all verbs parse");
        assert_eq!(scenario.steps.len(), 14);
        assert!(matches!(
            scenario.steps[3],
            Step::Mouse {
                event: MouseEvent::Drag(MouseButton::Left),
                column: 3,
                row: 4
            }
        ));
        assert!(matches!(
            scenario.steps[4],
            Step::Mouse {
                event: MouseEvent::Up(MouseButton::Middle),
                ..
            }
        ));
        assert!(matches!(scenario.steps[10], Step::ExpectExit(3)));
    }

    /// A step with no verb and a step with two both fail naming the
    /// step index.
    #[test]
    fn step_verb_cardinality_is_enforced() {
        let err = parse("feature = \"x\"\n[[steps]]\nfoo = 1\n").unwrap_err();
        assert!(
            err.to_string().contains("step 0: unknown verb(s) `foo`"),
            "{err}"
        );
        let err = parse("feature = \"x\"\n[[steps]]\n").unwrap_err();
        assert!(err.to_string().contains("found none"), "{err}");
        let err =
            parse("feature = \"x\"\n[[steps]]\nkeys = [\"Enter\"]\ntext = \"x\"\n").unwrap_err();
        assert!(err
            .to_string()
            .contains("step 0: a step carries exactly one verb"));
    }

    /// Malformed values fail at load with the step index, not mid-run.
    #[test]
    fn bad_values_fail_at_load() {
        for (toml, needle) in [
            ("keys = 3", "`keys` must be a list of chord strings"),
            ("keys = [\"Bogus\"]", "invalid chord"),
            ("bytes = \"1b5\"", "not an even number of hex digits"),
            (
                "mouse = { event = \"sideways\", col = 1, row = 1 }",
                "mouse.event",
            ),
            ("resize = [80]", "`resize` must be"),
            ("expect_regex = \"([\"", "`expect_regex` pattern"),
            ("expect_exit = -1", "non-negative"),
        ] {
            let err = parse(&format!("feature = \"x\"\n[[steps]]\n{toml}\n")).unwrap_err();
            assert!(
                err.to_string().contains(needle),
                "{toml:?} → {err} should contain {needle:?}"
            );
        }
    }

    /// `wait_text` and `expect_text` share the bounded-wait machinery
    /// but stay distinct verbs (gate vs assertion, for the reader).
    #[test]
    fn wait_and_expect_are_distinct_steps() {
        let scenario = parse(
            "feature = \"x\"\n[[steps]]\nwait_text = \"a\"\n[[steps]]\nexpect_text = \"a\"\n",
        )
        .unwrap();
        assert!(matches!(scenario.steps[0], Step::WaitText(_)));
        assert!(matches!(scenario.steps[1], Step::ExpectText(_)));
    }
}
