//! The recording file format and its path convention (Spec E01, ticket
//! E01-T2).
//!
//! A **recording** is a committed authored asset: one `*.jsonl` per
//! agent step under the consuming test tree at
//! `tests/recordings/<test_name>/<step>.jsonl` (standard 10 — readable
//! and writable by agents and humans without running a model). Line 1
//! is the header; every later line is one semantic [`Input`].
//!
//! ```json
//! {"v":1,"step":0,"op":"act","text":"open a new tab","text_hash":"9a3f1c…","program":"/bin/sh","screen":{"columns":80,"lines":24,"rows":["$ echo hi","hi","$"],"cursor":{"line":2,"column":2,"shape":"Block"}},"screen_hash":"7f2ab9…"}
//! {"text":"echo hi"}
//! {"key":"Enter"}
//! ```
//!
//! The guard is exactly what the spec decided: `text_hash` (the step's
//! goal/claim text — editing it invalidates the recording) plus
//! `screen_hash` (the settled screen after the step — replay re-sends
//! the inputs and asserts it). `program` is provenance only and never
//! guarded: there is deliberately no binary hash, so a rebuild without
//! behavior change invalidates nothing.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::driver::{Input, Verdict};
use crate::screen::{fnv, ScreenState, FNV_OFFSET};

/// Recording format version, bumped on any incompatible change.
pub(crate) const FORMAT_VERSION: u32 = 1;

/// Which agent verb a step is (`act` resolves inputs, `assert` judges
/// the screen).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    /// `agent().act("…")`.
    Act,
    /// `agent().assert("…")`.
    Assert,
}

impl Op {
    /// Lowercase verb name for messages.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Act => "act",
            Self::Assert => "assert",
        }
    }
}

/// Line 1 of a recording: everything about the step except its inputs.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Header {
    /// [`FORMAT_VERSION`].
    pub v: u32,
    /// The step's index within its test.
    pub step: usize,
    /// `act` or `assert`.
    pub op: Op,
    /// The goal/claim text, kept verbatim for review and error messages.
    pub text: String,
    /// FNV-1a hex of `op + text` — the guard that makes editing a
    /// step's text invalidate the recording.
    pub text_hash: String,
    /// The program that ran when the step was recorded — provenance for
    /// reviewers, **never** guarded (no binary hash: a rebuild without
    /// behavior change must not invalidate, Spec E01).
    pub program: String,
    /// The settled screen after the step (cwd-redacted), stored verbatim
    /// so a drift failure can print the recorded-vs-actual diff.
    pub screen: ScreenState,
    /// `screen`'s hash — what replay re-asserts after re-sending inputs.
    pub screen_hash: String,
    /// The recorded verdict of an `assert` step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
}

/// A whole recording: header plus the semantic inputs to re-send.
pub(crate) struct Recording {
    /// Line 1.
    pub header: Header,
    /// Lines 2.. — sent in order on replay.
    pub inputs: Vec<Input>,
}

/// The step-text guard: FNV-1a over `op` and the goal/claim text, so
/// `act("x")` and `assert("x")` hash differently and any edit
/// invalidates the file.
pub fn text_hash(op: Op, text: &str) -> String {
    let mut h = FNV_OFFSET;
    fnv(&mut h, op.as_str().as_bytes());
    fnv(&mut h, b"\0");
    fnv(&mut h, text.as_bytes());
    format!("{h:016x}")
}

/// The `<step>.jsonl` path for step `n` of the calling test under
/// `root` — `root/<test_name>/<step>.jsonl`, where `<test_name>` is the
/// test thread's name with `::` mapped to `/` (cargo names test threads
/// after the test path, so module structure is preserved).
pub(crate) fn step_path(root: &Path, step: usize) -> PathBuf {
    root.join(test_dir()).join(format!("{step:02}.jsonl"))
}

/// Where recordings live: [`E2e::set_recordings_dir`](crate::E2e::set_recordings_dir)
/// wins (self-tests point at temp dirs), else `AIT_E2E_RECORDINGS`,
/// else `<consuming crate>/tests/recordings` via the `CARGO_MANIFEST_DIR`
/// cargo exports to test processes at run time — so a consuming crate's
/// recordings land in *its* test tree, not ait-testkit's.
pub(crate) fn recordings_root(override_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = override_dir {
        return dir.to_path_buf();
    }
    if let Some(dir) = std::env::var_os("AIT_E2E_RECORDINGS") {
        return PathBuf::from(dir);
    }
    match std::env::var("CARGO_MANIFEST_DIR") {
        Ok(dir) => PathBuf::from(dir).join("tests").join("recordings"),
        Err(_) => PathBuf::from("tests").join("recordings"),
    }
}

/// `<test_name>` = the test thread's name, `::` → `/`; `unnamed` when
/// the thread has no name.
fn test_dir() -> String {
    std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .replace("::", "/")
}

impl Recording {
    /// Load `path`; `None` when no file exists. A file that exists but
    /// doesn't parse panics with context — a corrupt recording is a test
    /// bug, not a missing one.
    pub(crate) fn load(path: &Path) -> Option<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => panic!("could not read recording {}: {e}", path.display()),
        };
        let mut lines = text.lines().filter(|line| !line.trim().is_empty());
        let header: Header = serde_json::from_str(lines.next().unwrap_or_default())
            .unwrap_or_else(|e| panic!("bad header line in recording {}: {e}", path.display()));
        assert!(
            header.v == FORMAT_VERSION,
            "recording {} is format v{}, this ait-testkit reads v{FORMAT_VERSION}",
            path.display(),
            header.v
        );
        let inputs = lines
            .map(|line| {
                serde_json::from_str(line).unwrap_or_else(|e| {
                    panic!(
                        "bad input line {line:?} in recording {}: {e}",
                        path.display()
                    )
                })
            })
            .collect();
        Some(Self { header, inputs })
    }

    /// Write the recording (header line + one input per line), creating
    /// the `<test_name>` directory as needed.
    pub(crate) fn save(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap_or_else(|e| {
                panic!("could not create recording dir {}: {e}", dir.display())
            });
        }
        let mut text = serde_json::to_string(&self.header).expect("header serializes");
        for input in &self.inputs {
            text.push('\n');
            text.push_str(&serde_json::to_string(input).expect("input serializes"));
        }
        text.push('\n');
        std::fs::write(path, text)
            .unwrap_or_else(|e| panic!("could not write recording {}: {e}", path.display()));
    }
}
