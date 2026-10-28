//! Table-driven tests at the `sanitize_reply` pure seam (Spec R04, user
//! stories 5, 6, 28; standard 11 — no PTY, model or TTY). The hard
//! guarantee: cooked-mode line discipline makes a newline an execution,
//! so the output must never carry `\n` or `\r`.

use ait_agent::sanitize_reply;

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
