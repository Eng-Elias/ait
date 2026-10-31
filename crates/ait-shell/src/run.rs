//! The in-band `__ait_run` channel (REQUIREMENTS.md §5.5.1, ADR-0008).
//!
//! Wire shape:
//!
//! ```text
//! ait → PTY input : __ait_run <id> <hex(command)>          — `run_request`
//! shell → output  : ESC ] 7701 ; A BEL                     — `run_begin_sequence`
//!                   ESC ] 7701 ; R ; <len> ; <hex> BEL  ×N — `chunk_run_payload`
//!                   ESC ] 7701 ; B BEL                     — `run_end_sequence`
//! ```
//!
//! The `R` frames carry successive slices of one payload —
//! `<id>;<stdout+stderr>;<exit_code>` — each frame's `<len>` being the
//! decoded length of *that* slice. [`RunAccumulator`] concatenates the
//! slices between `A` and `B`, then splits the payload on its first and
//! last `;` (merged output may itself contain `;`).

use crate::codec::{hex_encode, OSC_RUN_CODE};
use crate::ShellEvent;

/// Decoded bytes per `R` frame. A frame's complete OSC is
/// `ESC]7701;R;<len>;<2×len hex>\x07` — 13 bytes of overhead when
/// `len ≤ 999` — so 500 decoded bytes produce a 1013-byte sequence,
/// under [`OSC_RAW_BUDGET`](crate::OSC_RAW_BUDGET) with margin.
const RUN_CHUNK_DECODED: usize = 500;

/// One `__ait_run` response, reassembled from its `R` frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    /// The request's `<id>`, echoed by the shell so responses stay
    /// attributable.
    pub id: String,
    /// The command's merged stdout+stderr bytes (uninterpreted).
    pub output: Vec<u8>,
    /// The command's exit code.
    pub exit_code: i32,
}

/// Reassembles one `A` … `R*` … `B` run into a [`RunResult`]. Feeding it
/// every decoded [`ShellEvent`] is safe: non-run events are ignored, so
/// hook traffic may interleave with a run (§5.5.1 responses are chunked
/// onto the same OSC stream as everything else).
#[derive(Debug, Default)]
pub struct RunAccumulator {
    active: bool,
    buf: Vec<u8>,
}

impl RunAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one decoded event. Returns `Some(RunResult)` exactly when a
    /// `RunEnd` completes a well-formed `<id>;<merged>;<exit_code>`
    /// payload; malformed payloads, stray frames and mid-run events all
    /// yield `None` — degrade, never panic.
    pub fn feed(&mut self, event: &ShellEvent) -> Option<RunResult> {
        match event {
            // A second `A` mid-run means the previous frame stream was
            // torn — drop it and start the new run cleanly.
            ShellEvent::RunBegin => {
                self.buf.clear();
                self.active = true;
                None
            }
            ShellEvent::RunChunk { payload, .. } if self.active => {
                self.buf.extend_from_slice(payload);
                None
            }
            ShellEvent::RunEnd if self.active => {
                self.active = false;
                parse_run_payload(&self.buf)
            }
            // Stray `R`/`B` outside a run, and every non-7701 event.
            _ => None,
        }
    }

    /// Whether a run is in flight (between `A` and `B`).
    pub fn is_active(&self) -> bool {
        self.active
    }
}

/// Split `<id>;<stdout+stderr>;<exit_code>`: the id ends at the first
/// `;`, the exit code starts after the last — everything between is
/// merged output, `;`s and all.
fn parse_run_payload(payload: &[u8]) -> Option<RunResult> {
    let first = payload.iter().position(|&b| b == b';')?;
    let last = payload.iter().rposition(|&b| b == b';')?;
    if last <= first {
        return None;
    }
    let id = std::str::from_utf8(&payload[..first]).ok()?.to_owned();
    let output = payload[first + 1..last].to_vec();
    let exit_code = std::str::from_utf8(&payload[last + 1..])
        .ok()?
        .parse()
        .ok()?;
    Some(RunResult {
        id,
        output,
        exit_code,
    })
}

/// The request line ait types into the PTY: `__ait_run <id> <hex(command)>`
/// plus a newline to submit it. Hex-encoding the command removes every
/// shell-quoting edge case (ADR-0008 amendment). The line stays visible in
/// the grid — in-band execution always leaves a trace (Spec R05 us-23).
pub fn run_request(id: &str, command: &[u8]) -> Vec<u8> {
    let mut line = Vec::with_capacity("__ait_run  \n".len() + id.len() + command.len() * 2);
    line.extend_from_slice(b"__ait_run ");
    line.extend_from_slice(id.as_bytes());
    line.push(b' ');
    line.extend_from_slice(&hex_encode(command));
    line.push(b'\n');
    line
}

/// `ESC ] 7701 ; A BEL` — opens a `__ait_run` response.
pub fn run_begin_sequence() -> Vec<u8> {
    osc_sequence(OSC_RUN_CODE, &[b"A"])
}

/// `ESC ] 7701 ; B BEL` — closes a `__ait_run` response.
pub fn run_end_sequence() -> Vec<u8> {
    osc_sequence(OSC_RUN_CODE, &[b"B"])
}

/// Split `payload` into complete `ESC ] 7701 ; R ; <len> ; <hex> BEL`
/// sequences, each no longer than [`OSC_RAW_BUDGET`](crate::OSC_RAW_BUDGET) total bytes — the
/// `vte` `MAX_OSC_RAW` guarantee even `no_std` parsers honor (Spec R05
/// us-26). An empty payload still yields one explicit `R;0;` frame.
pub fn chunk_run_payload(payload: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::with_capacity(payload.len() / RUN_CHUNK_DECODED + 1);
    for chunk in payload.chunks(RUN_CHUNK_DECODED) {
        let hex = hex_encode(chunk);
        let mut frame = Vec::with_capacity(4 + hex.len());
        frame.extend_from_slice(b"R;");
        frame.extend_from_slice(chunk.len().to_string().as_bytes());
        frame.push(b';');
        frame.extend_from_slice(&hex);
        frames.push(osc_sequence(OSC_RUN_CODE, &[frame.as_slice()]));
    }
    if frames.is_empty() {
        frames.push(osc_sequence(OSC_RUN_CODE, &[b"R;0;"]));
    }
    frames
}

/// Build a complete `ESC ] <code> ; <params joined on ;> BEL` sequence.
fn osc_sequence(code: u16, params: &[&[u8]]) -> Vec<u8> {
    let body_len: usize = params.iter().map(|p| p.len() + 1).sum();
    let mut out = Vec::with_capacity(2 + 4 + body_len + 1);
    out.extend_from_slice(b"\x1b]");
    out.extend_from_slice(code.to_string().as_bytes());
    for param in params {
        out.push(b';');
        out.extend_from_slice(param);
    }
    out.push(0x07);
    out
}
