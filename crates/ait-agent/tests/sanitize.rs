//! Table-driven tests at the `sanitize_reply` pure seam (Spec R04, user
//! stories 5, 6, 28; standard 11 — no PTY, model or TTY). The hard
//! guarantee: cooked-mode line discipline makes a newline an execution,
//! so the output must never carry `\n` or `\r`.

use ait_agent::{sanitize_reply, ReplySanitizer};

/// (case name, model reply, expected command): fenced replies,
/// `$ `-prefixed, multi-line (first-line truncation), whitespace-padded
/// and already-clean per the spec's testing decision.
const CASES: &[(&str, &str, &str)] = &[
    ("already clean", "ls -la", "ls -la"),
    ("whitespace padded", "   ls -la   \n", "ls -la"),
    ("dollar prefixed", "$ ls -la", "ls -la"),
    ("padded dollar prefixed", "  $ ls -la  ", "ls -la"),
    ("fenced with language tag", "```bash\nls -la\n```", "ls -la"),
    ("fenced bare", "```\nls -la\n```", "ls -la"),
    (
        "fenced with dollar inside",
        "```sh\n$ ls -la\n```",
        "ls -la",
    ),
    ("multi line truncates", "ls -la\nrm -rf /", "ls -la"),
    ("crlf truncates", "ls -la\r\nrm -rf /", "ls -la"),
    (
        "bare carriage return truncates",
        "ls -la\rrm -rf /",
        "ls -la",
    ),
    (
        "fenced multi line keeps first",
        "```sh\nls\nrm -rf /\n```",
        "ls",
    ),
    ("fence only", "```bash\n```", ""),
    ("empty", "", ""),
    ("blank reply", "   \n\n  ", ""),
];

#[test]
fn model_reply_collapses_to_one_command_line() {
    for (name, reply, expected) in CASES {
        assert_eq!(&sanitize_reply(reply), expected, "case failed: {name}");
    }
}

/// The never-execute guarantee (us-5): whatever a model sends back —
/// newlines, carriage returns, fences — the command bytes can never
/// contain a line break the PTY would read as Enter.
#[test]
fn output_never_contains_a_line_break() {
    for reply in [
        "ls\nrm -rf /",
        "ls\r\nrm -rf /",
        "ls\rrm",
        "```\nls\n```",
        "\nls",
        "ls\n",
    ] {
        let command = sanitize_reply(reply);
        assert!(
            !command.contains(['\n', '\r']),
            "line break survived in {command:?} (reply {reply:?})"
        );
    }
}

/// Replies exercising every held-back prefix in the streaming
/// sanitizer — the batch CASES plus fragment-boundary-sensitive shapes.
const STREAM_REPLIES: &[&str] = &[
    "ls -la",
    "   ls -la   \n",
    "$ ls -la",
    "$  ls -la",
    "$ ",
    "$",
    "$\tls",
    "$x",
    "$ \nx",
    "```bash\nls -la\n```",
    "```\nls -la\n```",
    "```sh\n$ ls -la\n```",
    "```bash",
    "```",
    "``x",
    "`x",
    "`",
    "``",
    "`\nx",
    "```sh\nls\nrm -rf /\n```",
    "```sh\n\nls",
    "ls -la\nrm -rf /",
    "ls -la\r\nrm -rf /",
    "ls -la\rrm -rf /",
    "ls ```\nx",
    "ls -la   ",
    "ls -la   tail",
    "\n\n  ls -la",
    "  \n\n  ",
    "",
];

/// The streaming seam's contract: pushing the reply in any chunked
/// form yields, in concatenation, exactly `sanitize_reply`'s output —
/// every ambiguous prefix (`$ `, ``` ``` ```, trailing whitespace) must
/// resolve identically however the provider fragmented it.
#[test]
fn chunked_streaming_matches_batch_sanitize() {
    for reply in STREAM_REPLIES {
        // Every two-chunk split at a char boundary.
        for cut in 0..=reply.len() {
            if !reply.is_char_boundary(cut) {
                continue;
            }
            let mut sanitizer = ReplySanitizer::new();
            let mut streamed = String::new();
            for piece in [
                sanitizer.push(&reply[..cut]),
                sanitizer.push(&reply[cut..]),
                sanitizer.finish(),
            ] {
                assert!(
                    !piece.contains(['\n', '\r']),
                    "line break emitted mid-stream for {reply:?} split at {cut}"
                );
                streamed.push_str(&piece);
            }
            assert_eq!(
                &streamed,
                &sanitize_reply(reply),
                "streamed result differs for {reply:?} split at {cut}"
            );
        }
        // And a char-at-a-time feed — the worst-case fragmentation.
        let mut sanitizer = ReplySanitizer::new();
        let mut streamed = String::new();
        for ch in reply.chars() {
            let mut buf = [0; 4];
            streamed.push_str(&sanitizer.push(ch.encode_utf8(&mut buf)));
        }
        streamed.push_str(&sanitizer.finish());
        assert_eq!(
            &streamed,
            &sanitize_reply(reply),
            "char-at-a-time streamed result differs for {reply:?}"
        );
    }
}

/// The never-execute guarantee at the *streaming* seam: mid-reply
/// emitted fragments never contain a line break — `\n`/`\r` is dropped
/// before it can reach the input line, not merely filtered at the end.
#[test]
fn streamed_pieces_never_contain_a_line_break() {
    let mut sanitizer = ReplySanitizer::new();
    for fragment in ["ls -la\n", "rm -rf /", "```\n", "tail"] {
        let piece = sanitizer.push(fragment);
        assert!(
            !piece.contains(['\n', '\r']),
            "line break emitted in {piece:?}"
        );
    }
    assert!(!sanitizer.finish().contains(['\n', '\r']));
}
