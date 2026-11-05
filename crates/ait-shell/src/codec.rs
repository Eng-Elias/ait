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
    /// (Spec R09 §5.9) — `value` names the destination, the
    /// wrapper-minted `hop` id and the negotiated ControlPath; the
    /// frontend pushes the tab's remote context.
    SshBegin,
    /// The wrapped `ssh` returned — the remote session ended (cleanly
    /// or by a dropped connection); `value.hop` attributes the close
    /// to one level of a possibly-nested chain.
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

#[cfg(test)]
mod tests {
    //! The wire contract's read side (Spec R05 us-25, Spec R09-T4):
    //! every hook name the snippets emit decodes to its `HookName`,
    //! the `ssh_begin`/`ssh_end` payloads — `hop`, `target`,
    //! `control_path`, `bootstrapped` — survive verbatim, and every
    //! malformed shape degrades to `None` instead of a partial event.

    use super::*;

    /// `7700 ; <hex(json)>` — the exact frame the snippets emit.
    fn hook_frame(json: &str) -> Option<ShellEvent> {
        decode(b"7700", &[hex_encode(json.as_bytes())])
    }

    /// Every emitted hook name round-trips to its variant — the
    /// decode side of the vocabulary standard 17 single-sources here.
    #[test]
    fn every_hook_name_decodes() {
        for (wire, expected) in [
            ("init_shell", HookName::InitShell),
            ("bootstrapped", HookName::Bootstrapped),
            ("precmd", HookName::Precmd),
            ("preexec", HookName::Preexec),
            ("exit_shell", HookName::ExitShell),
            ("ssh_begin", HookName::SshBegin),
            ("ssh_end", HookName::SshEnd),
        ] {
            let json = format!(r#"{{"hook":"{wire}","value":{{"session_id":"1"}}}}"#);
            assert_eq!(
                hook_frame(&json),
                Some(ShellEvent::Hook {
                    name: expected,
                    value: serde_json::json!({"session_id": "1"}),
                }),
                "{wire}"
            );
        }
    }

    /// `ssh_begin`'s payload — target, hop, bootstrapped, the
    /// negotiated ControlPath — arrives verbatim (Spec R09-T4: the
    /// frontend folds it into a remote context).
    #[test]
    fn ssh_begin_decodes_its_full_payload() {
        let Some(ShellEvent::Hook { name, value }) = hook_frame(
            r#"{"hook":"ssh_begin","value":{"session_id":"1","target":"prod",
             "hop":"1.4242.1","bootstrapped":true,"control_path":"/run/ait/ssh/%C"}}"#,
        ) else {
            panic!("ssh_begin must decode");
        };
        assert_eq!(name, HookName::SshBegin);
        assert_eq!(value["target"], "prod");
        assert_eq!(value["hop"], "1.4242.1");
        assert_eq!(value["bootstrapped"], true);
        assert_eq!(value["control_path"], "/run/ait/ssh/%C");
    }

    /// `ssh_end` carries target + hop — the attribution the nested
    /// fold needs.
    #[test]
    fn ssh_end_decodes_target_and_hop() {
        let Some(ShellEvent::Hook { name, value }) = hook_frame(
            r#"{"hook":"ssh_end","value":{"session_id":"1","target":"prod","hop":"1.4242.1"}}"#,
        ) else {
            panic!("ssh_end must decode");
        };
        assert_eq!(name, HookName::SshEnd);
        assert_eq!(value["target"], "prod");
        assert_eq!(value["hop"], "1.4242.1");
    }

    /// A remote shell's `exit_shell` carries its `hop` attribution —
    /// decoded the same as every other hook's value.
    #[test]
    fn a_remote_exit_shell_keeps_its_hop() {
        let Some(ShellEvent::Hook { name, value }) =
            hook_frame(r#"{"hook":"exit_shell","value":{"session_id":"1","hop":"1.4242.1"}}"#)
        else {
            panic!("exit_shell must decode");
        };
        assert_eq!(name, HookName::ExitShell);
        assert_eq!(value["hop"], "1.4242.1");
    }

    /// `session_id` normalizes string and numeric emitters to one
    /// comparison surface; empty/absent/non-scalar read as `None`.
    #[test]
    fn session_id_normalizes_emitters() {
        assert_eq!(
            session_id(&serde_json::json!({"session_id": "7"})),
            Some("7".to_owned())
        );
        assert_eq!(
            session_id(&serde_json::json!({"session_id": 7})),
            Some("7".to_owned())
        );
        for bad in [
            serde_json::json!({"session_id": ""}),
            serde_json::json!({"session_id": null}),
            serde_json::json!({"session_id": {"x": 1}}),
            serde_json::json!({}),
        ] {
            assert_eq!(session_id(&bad), None, "{bad}");
        }
    }

    /// A forward-compatible unknown name still decodes — a newer
    /// snippet's hooks degrade, never error.
    #[test]
    fn an_unknown_hook_decodes_as_unknown() {
        assert_eq!(
            hook_frame(r#"{"hook":"future_hook","value":{"x":1}}"#),
            Some(ShellEvent::Hook {
                name: HookName::Unknown("future_hook".to_owned()),
                value: serde_json::json!({"x": 1}),
            })
        );
    }

    /// Every malformed 7700 shape degrades to `None` — never a
    /// partial event.
    #[test]
    fn malformed_hook_frames_decode_to_none() {
        // Not hex, not UTF-8-after-hex, not JSON, not an object,
        // missing "hook", non-string "hook", missing "value".
        for params in [
            vec![b"zz".to_vec()],
            vec![hex_encode(b"\xff\xfe")],
            vec![hex_encode(b"not json")],
            vec![hex_encode(b"[]")],
            vec![hex_encode(br#"{"value":{}}"#)],
            vec![hex_encode(br#"{"hook":1,"value":{}}"#)],
            vec![hex_encode(br#"{"hook":"precmd"}"#)],
            // Two params — the frame carries exactly one.
            vec![hex_encode(br#"{"hook":"x","value":{}}"#), b"extra".to_vec()],
        ] {
            assert_eq!(decode(b"7700", &params), None, "{params:?}");
        }
    }

    /// The 7701 frames and the reserved family decode; a foreign
    /// code and a bad `<len>` do not.
    #[test]
    fn run_frames_and_reserved_codes() {
        assert_eq!(
            decode(b"7701", &[b"A".to_vec()]),
            Some(ShellEvent::RunBegin)
        );
        assert_eq!(decode(b"7701", &[b"B".to_vec()]), Some(ShellEvent::RunEnd));
        assert_eq!(
            decode(
                b"7701",
                &[b"R".to_vec(), b"4".to_vec(), hex_encode(b"abcd")],
            ),
            Some(ShellEvent::RunChunk {
                payload: b"abcd".to_vec()
            })
        );
        // A lying <len> is malformed, not truncated.
        assert_eq!(
            decode(
                b"7701",
                &[b"R".to_vec(), b"9".to_vec(), hex_encode(b"abcd")],
            ),
            None
        );
        assert_eq!(
            decode(b"7703", &[]),
            Some(ShellEvent::Reserved { code: 7703 })
        );
        assert_eq!(decode(b"133", &[]), None, "OSC 133 is not ours");
        assert_eq!(decode(b"7700x", &[]), None);
    }
}
