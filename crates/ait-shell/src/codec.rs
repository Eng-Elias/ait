//! The pure OSC 7700-series codec: `SessionEvent::Private{code,params}`
//! in, typed [`ShellEvent`] out (Spec R05 / issue #70).
//!
//! `decode` takes primitives — not ait-term's `SessionEvent` — so the crate
//! stays free of the terminal-core dependency (CODING_STANDARDS.md 9). The
//! caller pattern-matches `Private` and hands over `&code`/`&params`
//! verbatim; this module owns the entire 770x grammar.

use serde_json::Value;

/// OSC code carrying hex-encoded JSON hook events (§5.5, ADR-0008).
pub const OSC_HOOK_CODE: u16 = 7700;
/// OSC code for the in-band `__ait_run` channel (§5.5.1).
pub const OSC_RUN_CODE: u16 = 7701;
/// First reserved 770x code — `7702` (completions) and `7703` (grid reset)
/// are protocol-fixed but unconsumed this round; `7704..=7709` are
/// unassigned. All decode to [`ShellEvent::Reserved`].
const OSC_RESERVED_FIRST: u16 = 7702;
/// Last code of the reserved `770x` family.
const OSC_RESERVED_LAST: u16 = 7709;

/// The raw-byte budget every emitted OSC must stay under: `vte`'s
/// `MAX_OSC_RAW` (1024) — the largest OSC buffer a conforming parser is
/// required to provide. `OSC_RAW_BUDGET` counts the *complete* sequence
/// (`ESC ]` … `BEL`), so chunk sizing stays safe even under `no_std`
/// `ArrayVec` parsers (Spec R05 us-26).
pub const OSC_RAW_BUDGET: usize = 1024;

/// A decoded AIT-private OSC event.
#[derive(Debug, Clone, PartialEq)]
pub enum ShellEvent {
    /// `OSC 7700 ; <hex({"hook":…,"value":…})>` — a shell hook fired.
    /// `value` is the hook's JSON payload verbatim; interpreting it
    /// (`command` on `preexec`, `session_id`, …) is the consumer's job.
    Hook {
        /// Which hook fired — a known name or a forward-compatible unknown.
        name: HookName,
        /// The `"value"` field of the hook JSON.
        value: Value,
    },
    /// `OSC 7701 ; A` — a `__ait_run` response begins.
    RunBegin,
    /// `OSC 7701 ; R ; <len> ; <hex>` — one response frame. The wire
    /// `<len>` is checked against the decoded payload during decode, so
    /// what survives here always measured up.
    RunChunk {
        /// This frame's slice of the `<id>;<merged>;<exit_code>` stream.
        payload: Vec<u8>,
    },
    /// `OSC 7701 ; B` — the response ends; the [`crate::RunAccumulator`]
    /// splits the reassembled payload.
    RunEnd,
    /// A well-formed OSC in the reserved `770x` family (`7702`, `7703`,
    /// unassigned `7704..=7709`): protocol-recognized, deliberately
    /// unconsumed.
    Reserved {
        /// The OSC code (e.g. `7702`).
        code: u16,
    },
}

/// The seven hook names the snippets emit, plus a forward-compatible
/// unknown bucket (a newer snippet on an older ait still decodes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookName {
    /// Integration just went live (snippet sourced).
    InitShell,
    /// A remote/nested shell reported back as integrated (§5.5.1; Round 9
    /// will consume it — emitted-ready now).
    Bootstrapped,
    /// About to render a prompt.
    Precmd,
    /// About to run the command named in `value.command`.
    Preexec,
    /// The integrated shell is exiting.
    ExitShell,
    /// The `ssh()` wrapper is about to exec an integrated `ssh`
    /// (Spec R09 §5.9) — `value` names the destination and the
    /// negotiated ControlPath; the frontend opens the tab's remote
    /// context.
    SshBegin,
    /// The wrapped `ssh` returned — the remote session ended (cleanly
    /// or by a dropped connection); the frontend closes the remote
    /// context and retires the control socket.
    SshEnd,
    /// A hook name this version doesn't know.
    Unknown(String),
}

impl HookName {
    /// Map a wire name to a hook; unrecognized names land in `Unknown`.
    fn from_wire(name: &str) -> Self {
        match name {
            "init_shell" => Self::InitShell,
            "bootstrapped" => Self::Bootstrapped,
            "precmd" => Self::Precmd,
            "preexec" => Self::Preexec,
            "exit_shell" => Self::ExitShell,
            "ssh_begin" => Self::SshBegin,
            "ssh_end" => Self::SshEnd,
            other => Self::Unknown(other.to_owned()),
        }
    }
}

/// The `session_id` a hook `value` carries, normalized to a plain
/// string: the snippets emit a quoted JSON string, but a numeric
/// literal from an older or foreign emitter normalizes to its digits —
/// either way consumers compare against one allocated id. Absent,
/// null, empty or non-scalar values yield `None`.
pub fn session_id(value: &Value) -> Option<String> {
    match value.get("session_id") {
        Some(Value::String(id)) if !id.is_empty() => Some(id.clone()),
        Some(Value::Number(id)) => Some(id.to_string()),
        _ => None,
    }
}

/// Decode a raw private OSC — `SessionEvent::Private{code,params}` — into
/// a [`ShellEvent`]. Returns `Some` for the `770x` family (including
/// [`ShellEvent::Reserved`]) and `None` for foreign codes and every
/// malformed/truncated/over-long shape. Never panics.
pub fn decode(code: &[u8], params: &[Vec<u8>]) -> Option<ShellEvent> {
    let code = parse_code(code)?;
    let params: Vec<&[u8]> = params.iter().map(Vec::as_slice).collect();
    match code {
        OSC_HOOK_CODE => decode_hook(&params),
        OSC_RUN_CODE => decode_run(&params),
        OSC_RESERVED_FIRST..=OSC_RESERVED_LAST => Some(ShellEvent::Reserved { code }),
        _ => None,
    }
}

/// `7700 ; <hex({"hook":…,"value":…})>` — exactly one hex param carrying
/// a JSON object with a string `"hook"` and any `"value"`.
fn decode_hook(params: &[&[u8]]) -> Option<ShellEvent> {
    let [hex_payload] = params else {
        return None;
    };
    let bytes = hex_decode(hex_payload)?;
    let text = std::str::from_utf8(&bytes).ok()?;
    let json: Value = serde_json::from_str(text).ok()?;
    let object = json.as_object()?;
    let name = HookName::from_wire(object.get("hook")?.as_str()?);
    let value = object.get("value")?.clone();
    Some(ShellEvent::Hook { name, value })
}

/// `7701 ; A|B` brackets, `7701 ; R ; <len> ; <hex>` frames. `<len>` is
/// the decoded byte length of *that* frame — a mismatch makes the frame
/// malformed.
fn decode_run(params: &[&[u8]]) -> Option<ShellEvent> {
    match params {
        [b"A"] => Some(ShellEvent::RunBegin),
        [b"B"] => Some(ShellEvent::RunEnd),
        [b"R", len, hex_payload] => {
            let len = parse_decimal(len)?;
            let payload = hex_decode(hex_payload)?;
            if payload.len() != len {
                return None;
            }
            Some(ShellEvent::RunChunk { payload })
        }
        _ => None,
    }
}

/// Parse a numeric OSC code (`b"7700"` → `7700`); non-numeric codes and
/// u16 overflow both return `None` — neither can be `770x`.
fn parse_code(code: &[u8]) -> Option<u16> {
    let text = std::str::from_utf8(code).ok()?;
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Parse an ASCII-decimal field (`<len>`); leading zeros are legal.
fn parse_decimal(bytes: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(bytes).ok()?;
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Strict hex decode: even length, all hex digits (upper or lower case).
/// Anything else returns `None` — a malformed payload degrades to no
/// event rather than a partial decode.
pub(crate) fn hex_decode(hex: &[u8]) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_chunks::<2>().0 {
        let high = hex_value(pair[0])?;
        let low = hex_value(pair[1])?;
        out.push(high << 4 | low);
    }
    Some(out)
}

/// Lowercase hex encode — the single alphabet the snippets and this
/// crate emit.
pub(crate) fn hex_encode(bytes: &[u8]) -> Vec<u8> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize]);
        out.push(DIGITS[(byte & 0xF) as usize]);
    }
    out
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
