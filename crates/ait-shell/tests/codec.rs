//! Table-driven tests for the OSC 7700-series codec seam (Spec R05 /
//! issue #70): `ait-term` hands raw `SessionEvent::Private{code,params}`
//! to `ait_shell::decode` and well-formed 7700/7701 sequences emerge as
//! typed `ShellEvent`s. Everything malformed, truncated or outside the
//! 770x family decodes to `None` — degrade, never panic (§5.5).

use ait_shell::{decode, HookName, ShellEvent};

/// Hex-encode a UTF-8 string the way the shell snippets emit it.
fn hex(text: &str) -> Vec<u8> {
    text.bytes()
        .flat_map(|b| [nibble(b >> 4), nibble(b & 0xF)])
        .collect()
}

fn nibble(value: u8) -> u8 {
    match value {
        0..=9 => b'0' + value,
        _ => b'a' + value - 10,
    }
}

fn params(params: &[&str]) -> Vec<Vec<u8>> {
    params.iter().map(|p| p.as_bytes().to_vec()).collect()
}

#[test]
fn osc7700_decodes_every_known_hook() {
    let cases = [
        ("init_shell", HookName::InitShell),
        ("bootstrapped", HookName::Bootstrapped),
        ("precmd", HookName::Precmd),
        ("preexec", HookName::Preexec),
        ("exit_shell", HookName::ExitShell),
    ];
    for (wire, expected) in cases {
        let json = format!("{{\"hook\":\"{wire}\",\"value\":{{\"session_id\":7}}}}");
        let event = decode(b"7700", &[hex(&json)]);
        assert_eq!(
            event,
            Some(ShellEvent::Hook {
                name: expected,
                value: serde_json::json!({"session_id": 7}),
            }),
            "hook {wire} should decode",
        );
    }
}

#[test]
fn osc7700_preexec_value_passes_through_verbatim() {
    let json = r#"{"hook":"preexec","value":{"command":"ls -la","session_id":3}}"#;
    let event = decode(b"7700", &[hex(json)]);
    assert_eq!(
        event,
        Some(ShellEvent::Hook {
            name: HookName::Preexec,
            value: serde_json::json!({"command": "ls -la", "session_id": 3}),
        })
    );
}

#[test]
fn osc7700_unknown_hook_name_stays_a_hook() {
    // Forward compatibility: a future hook name is still a well-formed
    // hook event, not a malformed frame.
    let json = r#"{"hook":"postexec","value":{}}"#;
    let event = decode(b"7700", &[hex(json)]);
    assert_eq!(
        event,
        Some(ShellEvent::Hook {
            name: HookName::Unknown("postexec".to_owned()),
            value: serde_json::json!({}),
        })
    );
}

#[test]
fn osc7700_extra_json_fields_are_tolerated() {
    let json = r#"{"hook":"precmd","value":{"cwd":"/tmp"},"v":1}"#;
    let event = decode(b"7700", &[hex(json)]);
    assert_eq!(
        event,
        Some(ShellEvent::Hook {
            name: HookName::Precmd,
            value: serde_json::json!({"cwd": "/tmp"}),
        })
    );
}

#[test]
fn osc7701_verbs_decode() {
    assert_eq!(decode(b"7701", &params(&["A"])), Some(ShellEvent::RunBegin));
    assert_eq!(decode(b"7701", &params(&["B"])), Some(ShellEvent::RunEnd));
}

#[test]
fn osc7701_r_frame_decodes_len_checked_payload() {
    // hex("abc;e") — the decoded payload itself may contain `;`.
    let event = decode(b"7701", &params(&["R", "5", "6162633b65"]));
    assert_eq!(
        event,
        Some(ShellEvent::RunChunk {
            len: 5,
            payload: b"abc;e".to_vec(),
        })
    );
}

#[test]
fn osc7701_r_frame_accepts_binary_and_empty_payloads() {
    let binary = vec![0xFF, 0x00, 0x80];
    let hex_payload: String = binary.iter().map(|b| format!("{b:02x}")).collect();
    let event = decode(
        b"7701",
        &[vec![b'R'], b"3".to_vec(), hex_payload.into_bytes()],
    );
    assert_eq!(
        event,
        Some(ShellEvent::RunChunk {
            len: 3,
            payload: binary,
        })
    );

    assert_eq!(
        decode(b"7701", &params(&["R", "0", ""])),
        Some(ShellEvent::RunChunk {
            len: 0,
            payload: Vec::new(),
        })
    );
}

#[test]
fn reserved_770x_codes_decode_to_reserved() {
    for code in [b"7702", b"7703", b"7704", b"7709"] {
        assert_eq!(
            decode(code, &params(&["A", "whatever"])),
            Some(ShellEvent::Reserved {
                code: std::str::from_utf8(code).unwrap().parse().unwrap(),
            }),
            "OSC {code:?} should surface as Reserved",
        );
    }
}

/// Everything that must decode to `None`: malformed 7700/7701, the
/// non-770x codes `SessionEvent::Private` also carries, and non-numeric
/// codes. Each case must never panic (§5.5 graceful degrade).
#[test]
fn malformed_or_foreign_sequences_decode_to_none() {
    let cases: Vec<(Vec<u8>, Vec<Vec<u8>>)> = vec![
        // OSC 7700 malformed shapes.
        (b"7700".to_vec(), vec![]),                      // missing payload
        (b"7700".to_vec(), params(&["6162", "63"])),     // extra param
        (b"7700".to_vec(), params(&["abc"])),            // odd hex length
        (b"7700".to_vec(), params(&["zz"])),             // non-hex digits
        (b"7700".to_vec(), params(&["ff00"])),           // valid hex, not UTF-8
        (b"7700".to_vec(), vec![hex("not json")]),       // valid UTF-8, not JSON
        (b"7700".to_vec(), vec![hex("[1,2]")]),          // JSON, not an object
        (b"7700".to_vec(), vec![hex("{\"value\":{}}")]), // missing "hook"
        (b"7700".to_vec(), vec![hex("{\"hook\":7,\"value\":{}})")]), // hook not a string
        (b"7700".to_vec(), vec![hex("{\"hook\":\"precmd\"}")]), // missing "value"
        // OSC 7701 malformed frames.
        (b"7701".to_vec(), vec![]),                      // missing verb
        (b"7701".to_vec(), params(&["X"])),              // unknown verb
        (b"7701".to_vec(), params(&["a"])),              // verbs are case-sensitive
        (b"7701".to_vec(), params(&["A", "extra"])),     // extra param on A
        (b"7701".to_vec(), params(&["R"])),              // truncated R frame
        (b"7701".to_vec(), params(&["R", "3"])),         // truncated R frame
        (b"7701".to_vec(), params(&["R", "x", "00"])),   // non-numeric len
        (b"7701".to_vec(), params(&["R", "2", "zz"])),   // non-hex payload
        (b"7701".to_vec(), params(&["R", "3", "6162"])), // len < decoded (3 != 1)
        (b"7701".to_vec(), params(&["R", "1", "6162"])), // len > decoded (1 != 1 byte? "61 62" = 2)
        (b"7701".to_vec(), params(&["R", "1", "61", "62"])), // extra param on R
        (b"7701".to_vec(), params(&["B", "9"])),         // extra param on B
        // Foreign OSC codes — Private carries every unrecognized sequence;
        // only the 770x family is ours.
        (b"7".to_vec(), params(&["file:///tmp"])),
        (b"133".to_vec(), params(&["A"])),
        (b"52".to_vec(), params(&["c", "eA=="])),
        (b"7710".to_vec(), params(&["A"])),
        (b"9999".to_vec(), params(&["x"])),
        (b"i".to_vec(), params(&["x"])),
        (b"".to_vec(), params(&["x"])),
        (b"77777".to_vec(), params(&["x"])), // u16 overflow
    ];
    for (code, params) in cases {
        assert_eq!(
            decode(&code, &params),
            None,
            "code {code:?} params {params:?} should decode to None",
        );
    }
}
