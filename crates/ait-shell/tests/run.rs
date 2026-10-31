//! Tests for the in-band `__ait_run` channel (§5.5.1): the emit side —
//! `run_request`, the A/B bracket sequences and the R-frame chunker — and
//! the `RunAccumulator` that reassembles chunked responses on the receive
//! side. The payload grammar is `<id>;<stdout+stderr>;<exit_code>` —
//! merged output may itself contain `;`, so split points are the first
//! and last separators only.

use ait_shell::{
    chunk_run_payload, decode, run_begin_sequence, run_end_sequence, run_request, RunAccumulator,
    RunResult, ShellEvent, OSC_RAW_BUDGET,
};

/// Parse one complete `ESC ] <code> ; <params…> BEL` sequence back into
/// the `(code, params)` pair `decode` consumes.
fn split_osc(sequence: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    assert!(sequence.starts_with(b"\x1b]"), "not an OSC: {sequence:?}");
    assert!(
        sequence.ends_with(b"\x07"),
        "unterminated OSC: {sequence:?}"
    );
    let body = &sequence[2..sequence.len() - 1];
    let mut parts = body.splitn(2, |&b| b == b';');
    let code = parts.next().unwrap().to_vec();
    let params = parts
        .next()
        .map(|rest| rest.split(|&b| b == b';').map(|p| p.to_vec()).collect())
        .unwrap_or_default();
    (code, params)
}

#[test]
fn run_request_is_the_hex_command_line() {
    // `__ait_run <id> <hex(command)>` — hex so the command needs no shell
    // quoting (ADR-0008 amendment); newline submits the line.
    assert_eq!(
        run_request("7", b"ls -la"),
        b"__ait_run 7 6c73202d6c61\n".to_vec(),
    );
}

#[test]
fn run_bracket_sequences() {
    assert_eq!(run_begin_sequence(), b"\x1b]7701;A\x07".to_vec());
    assert_eq!(run_end_sequence(), b"\x1b]7701;B\x07".to_vec());
}

#[test]
fn chunker_emits_complete_oscs_under_the_raw_budget() {
    let payload: Vec<u8> = (0..5_000).map(|i| b'0' + (i % 10) as u8).collect();
    let frames = chunk_run_payload(&payload);

    assert!(frames.len() > 1, "5 KB must need several frames");
    for frame in &frames {
        assert!(
            frame.len() <= OSC_RAW_BUDGET,
            "frame over budget: {frame:?}"
        );
        assert!(frame.starts_with(b"\x1b]7701;R;") && frame.ends_with(b"\x07"));

        let (code, params) = split_osc(frame);
        let event = decode(&code, &params).expect("emitted frame must decode");
        assert!(matches!(event, ShellEvent::RunChunk { .. }));
    }

    // Reassembly is lossless.
    let decoded: Vec<u8> = frames
        .iter()
        .flat_map(|frame| {
            let (code, params) = split_osc(frame);
            match decode(&code, &params) {
                Some(ShellEvent::RunChunk { payload, .. }) => payload,
                other => panic!("expected RunChunk, got {other:?}"),
            }
        })
        .collect();
    assert_eq!(decoded, payload);
}

#[test]
fn chunker_small_and_empty_payloads() {
    let one = chunk_run_payload(b"id;out;0");
    assert_eq!(one.len(), 1);
    assert_eq!(one[0], b"\x1b]7701;R;8;69643b6f75743b30\x07".to_vec(),);

    // An empty payload still gets one explicit empty frame.
    let empty = chunk_run_payload(b"");
    assert_eq!(empty.len(), 1);
    assert_eq!(empty[0], b"\x1b]7701;R;0;\x07".to_vec());
}

#[test]
fn accumulator_reassembles_a_chunked_response() {
    // Payload `42;first; second\nlast;0` split mid-token across frames.
    let mut acc = RunAccumulator::new();
    assert!(acc.feed(&ShellEvent::RunBegin).is_none());
    assert!(acc
        .feed(&ShellEvent::RunChunk {
            len: 9,
            payload: b"42;first;".to_vec(),
        })
        .is_none());
    assert!(acc
        .feed(&ShellEvent::RunChunk {
            len: 11,
            payload: b" second\nlast;".to_vec(),
        })
        .is_none());
    assert!(acc
        .feed(&ShellEvent::RunChunk {
            len: 1,
            payload: b"0".to_vec(),
        })
        .is_none());

    assert_eq!(
        acc.feed(&ShellEvent::RunEnd),
        Some(RunResult {
            id: "42".to_owned(),
            output: b"first; second\nlast".to_vec(),
            exit_code: 0,
        })
    );
}

#[test]
fn accumulator_binary_output_and_nonzero_exit() {
    let mut acc = RunAccumulator::new();
    acc.feed(&ShellEvent::RunBegin);
    acc.feed(&ShellEvent::RunChunk {
        len: 9,
        payload: b"r-9;\xFF\x00x;127".to_vec(),
    });
    assert_eq!(
        acc.feed(&ShellEvent::RunEnd),
        Some(RunResult {
            id: "r-9".to_owned(),
            output: vec![0xFF, 0x00, b'x'],
            exit_code: 127,
        })
    );
}

#[test]
fn accumulator_ignores_events_outside_and_inside_a_run() {
    let mut acc = RunAccumulator::new();
    // Stray frames without a preceding A produce nothing.
    assert!(acc
        .feed(&ShellEvent::RunChunk {
            len: 1,
            payload: b"x".to_vec(),
        })
        .is_none());
    assert!(acc.feed(&ShellEvent::RunEnd).is_none());

    // A hook event mid-run is ignored, not an abort.
    acc.feed(&ShellEvent::RunBegin);
    acc.feed(&ShellEvent::Hook {
        name: ait_shell::HookName::Precmd,
        value: serde_json::json!({}),
    });
    acc.feed(&ShellEvent::RunChunk {
        len: 6,
        payload: b"1;o;0".to_vec(),
    });
    assert_eq!(
        acc.feed(&ShellEvent::RunEnd),
        Some(RunResult {
            id: "1".to_owned(),
            output: b"o".to_vec(),
            exit_code: 0,
        })
    );
}

#[test]
fn accumulator_malformed_payloads_yield_no_result() {
    let cases: Vec<Vec<u8>> = vec![
        b"no-separator".to_vec(),           // not `<id>;<out>;<exit>`
        b"id;out".to_vec(),                 // only one separator
        b"1;out;abc".to_vec(),              // non-numeric exit code
        b"1;out;".to_vec(),                 // empty exit code
        vec![0xFF, b';', b'o', b';', b'0'], // non-UTF-8 id
    ];
    for payload in cases {
        let mut acc = RunAccumulator::new();
        acc.feed(&ShellEvent::RunBegin);
        acc.feed(&ShellEvent::RunChunk {
            len: payload.len(),
            payload: payload.clone(),
        });
        assert_eq!(acc.feed(&ShellEvent::RunEnd), None, "payload {payload:?}");
    }
}

#[test]
fn accumulator_a_second_begin_restarts_the_run() {
    let mut acc = RunAccumulator::new();
    acc.feed(&ShellEvent::RunBegin);
    acc.feed(&ShellEvent::RunChunk {
        len: 7,
        payload: b"stale;x".to_vec(),
    });
    acc.feed(&ShellEvent::RunBegin); // torn frame: previous run dropped
    acc.feed(&ShellEvent::RunChunk {
        len: 5,
        payload: b"2;y;1".to_vec(),
    });
    assert_eq!(
        acc.feed(&ShellEvent::RunEnd),
        Some(RunResult {
            id: "2".to_owned(),
            output: b"y".to_vec(),
            exit_code: 1,
        })
    );
}

#[test]
fn emitted_sequence_feeds_back_through_decode_and_accumulator() {
    // Full emit→wire→decode→accumulate round-trip for a multi-frame run.
    let payload = b"17;"
        .iter()
        .chain(vec![b'x'; 3_000].iter())
        .chain(b";2".iter())
        .copied()
        .collect::<Vec<u8>>();

    let mut wire = run_begin_sequence();
    for frame in chunk_run_payload(&payload) {
        wire.extend_from_slice(&frame);
    }
    wire.extend_from_slice(&run_end_sequence());

    let mut acc = RunAccumulator::new();
    let mut result = None;
    // Split the wire back into complete OSCs on their BEL terminators.
    for sequence in wire.split(|&b| b == 0x07).filter(|s| !s.is_empty()) {
        let mut complete = sequence.to_vec();
        complete.push(0x07);
        let (code, params) = split_osc(&complete);
        if let Some(event) = decode(&code, &params) {
            result = acc.feed(&event).or(result);
        }
    }
    assert_eq!(
        result,
        Some(RunResult {
            id: "17".to_owned(),
            output: vec![b'x'; 3_000],
            exit_code: 2,
        })
    );
}
