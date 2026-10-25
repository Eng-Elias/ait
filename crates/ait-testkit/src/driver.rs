//! The [`Driver`] seam: the backing that resolves `agent` steps —
//! `act(goal, screen) -> inputs` and `assert(claim, screen) -> verdict`
//! (Spec E01, ticket E01-T2).
//!
//! Backings in delivery order: [`Authored`] (replay-only default — the
//! committed `*.jsonl` recordings are the authored answer, zero infra),
//! `Command` (E01-T3, `AIT_E2E_DRIVER` env points at a subprocess taking
//! `{goal|claim, screen}` JSON on stdin), and a deferred `ait-llm`
//! backing. Inject one with [`E2e::set_driver`](crate::E2e::set_driver).

use serde::{Deserialize, Serialize};

use crate::screen::ScreenState;

/// One semantic input — what a [`Driver`] chooses and a recording
/// stores. Replay resolves each through the harness's real encoding
/// path ([`E2e::press`](crate::E2e::press) → `parse_chord` →
/// `encode_key`, [`E2e::type_text`](crate::E2e::type_text),
/// [`E2e::send_bytes`](crate::E2e::send_bytes)), so recordings are
/// agent/human-writable assets (standard 10) and stay valid across
/// encoding changes.
///
/// Jsonl forms (one input per line):
///
/// ```json
/// {"key": "Alt+T"}
/// {"text": "echo hi"}
/// {"bytes": "1b5b353e"}
/// ```
///
/// `bytes` is lowercase hex — the raw-bytes escape hatch, spelled the
/// way `od -tx1` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Input {
    /// A named chord (`"Alt+T"`, `"Shift+PageUp"`), encoded the way a
    /// real legacy terminal sends it.
    Key {
        /// The chord string [`parse_chord`](crate::parse_chord) reads.
        key: String,
    },
    /// Literal text: its UTF-8 bytes to the program's stdin, exactly as
    /// written.
    Text {
        /// The text run.
        text: String,
    },
    /// Raw bytes, hex-encoded — anything a chord can't express (SGR
    /// mouse reports, bracketed paste, exotic sequences).
    Bytes {
        /// Lowercase hex pairs (`"1b5b353e"`).
        #[serde(with = "hex")]
        bytes: Vec<u8>,
    },
}

impl Input {
    /// A named-chord input (`Input::key("Alt+T")`).
    pub fn key(chord: impl Into<String>) -> Self {
        Self::Key { key: chord.into() }
    }

    /// A literal-text input.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// A raw-bytes input.
    pub fn bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self::Bytes {
            bytes: bytes.into(),
        }
    }
}

/// A [`Driver`]'s judgement of an `assert` claim against the screen.
///
/// Serialized into recordings as `"pass"` or
/// `{"fail": {"reason": "…"}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The claim holds on this screen.
    Pass,
    /// The claim does not hold; `reason` is the driver's explanation and
    /// becomes the test failure's evidence.
    Fail {
        /// Why the claim fails.
        reason: String,
    },
}

/// The backing that resolves `agent` steps at record time (Spec E01:
/// "BYO model — the agent is a trait; the backing brain is a config
/// detail"). Never invoked on the replay path — replay re-sends
/// recorded inputs and re-asserts recorded screen hashes with no driver
/// at all.
///
/// Both methods may panic: a driver that cannot resolve is a test
/// failure, and the panic message becomes the evidence.
pub trait Driver {
    /// Choose the inputs that achieve `goal` from `screen`.
    fn act(&mut self, goal: &str, screen: &ScreenState) -> Vec<Input>;

    /// Judge `claim` against `screen`.
    fn assert(&mut self, claim: &str, screen: &ScreenState) -> Verdict;
}

/// The default [`Driver`]: replay-only. Recordings under
/// `tests/recordings/` are authored assets — written by the authoring
/// agent (or a human) and committed — so a test using agent steps runs
/// with no configuration and no model (Spec E01). Any attempt to
/// *resolve* through `Authored` (record mode without an injected
/// driver) fails with instructions.
pub struct Authored;

impl Driver for Authored {
    fn act(&mut self, goal: &str, _screen: &ScreenState) -> Vec<Input> {
        panic!(
            "`AIT_E2E_RECORD` is set but the active Driver is `Authored`, which is \
             replay-only and cannot resolve the act goal {goal:?}.\n\
             Author the step's recording .jsonl by hand (it is a committed asset), \
             or inject a resolving driver with `E2e::set_driver` / `AIT_E2E_DRIVER`."
        )
    }

    fn assert(&mut self, claim: &str, _screen: &ScreenState) -> Verdict {
        panic!(
            "`AIT_E2E_RECORD` is set but the active Driver is `Authored`, which is \
             replay-only and cannot resolve the assert claim {claim:?}.\n\
             Author the step's recording .jsonl by hand (it is a committed asset), \
             or inject a resolving driver with `E2e::set_driver` / `AIT_E2E_DRIVER`."
        )
    }
}

/// Lowercase hex for [`Input::Bytes`]' jsonl form.
mod hex {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push_str(&format!("{b:02x}"));
        }
        s.serialize_str(&out)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        if text.len() % 2 != 0 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(serde::de::Error::custom(format!(
                "{text:?} is not an even number of hex digits"
            )));
        }
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(serde::de::Error::custom))
            .collect()
    }
}
