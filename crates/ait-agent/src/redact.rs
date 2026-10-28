//! The §5.10.3 credential-redaction seam (Spec R04, user stories 18,
//! 26): one compiled-once pattern set covering the spec's built-in
//! credential list, applied to terminal output before context assembly.
//! Optional PII patterns, user-added regexes and per-pattern toggles
//! are Round 10 scope — out of scope here.

use regex::Regex;
use std::borrow::Cow;
use std::sync::OnceLock;

/// The marker every matched credential collapses to.
const REPLACEMENT: &str = "[REDACTED]";

/// The §5.10.3 built-in credential patterns. Ordering matters:
/// Anthropic's `sk-ant-` and Stripe's `sk_live_`/`sk_test_` precede
/// OpenAI's generic `sk-…`, whose character class would otherwise also
/// match those keys. Each pattern is leading-`\b`-anchored so prefixes
/// inside a larger word (`task-…`, `key=…`) never match; the trailing
/// end stays unanchored — over-redacting a glued-on suffix beats
/// leaking a partial key.
const PATTERNS: &[&str] = &[
    // Anthropic API keys (`sk-ant-…`) — before the generic `sk-`.
    r"\bsk-ant-[A-Za-z0-9_-]{20,}",
    // Stripe live + test secret keys (`sk_live_…`, `sk_test_…`).
    r"\bsk_(?:live|test)_[A-Za-z0-9]{16,}",
    // OpenAI API keys (`sk-…`, incl. `sk-proj-`/`sk-svcacct-`).
    r"\bsk-[A-Za-z0-9_-]{20,}",
    // AWS access key IDs: `AKIA` + 16 chars.
    r"\bAKIA[0-9A-Z]{16}",
    // GitHub personal/OAuth/user-to-server/server-to-server tokens and
    // fine-grained PATs (`ghp_`/`gho_`/`ghu_`/`ghs_`, `github_pat_`).
    r"\b(?:ghp|gho|ghu|ghs)_[A-Za-z0-9]{20,}",
    r"\bgithub_pat_[A-Za-z0-9_]{20,}",
    // Slack tokens (`xoxb`/`xoxp`/`xoxa`/`xoxr`/`xoxs`/…) and
    // app-level `xapp-`.
    r"\bxox[a-z]-[A-Za-z0-9-]{10,}",
    r"\bxapp-[A-Za-z0-9-]{10,}",
    // JWTs: three base64url segments; the header always encodes `{"`,
    // hence the `eyJ` anchor.
    r"\beyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}",
    // Private-key PEM blocks — `[\s\S]` lets the match span lines, so
    // a whole `-----BEGIN …-----`…`-----END …-----` block collapses
    // to one marker.
    r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
];

/// The compiled-once pattern set: a `RegexSet` only reports which
/// pattern matched, so per-pattern `Regex`es are kept instead —
/// `replace_all` needs the match spans, and a PEM match spans lines.
fn patterns() -> &'static [Regex] {
    static COMPILED: OnceLock<Vec<Regex>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        PATTERNS
            .iter()
            .map(|pattern| Regex::new(pattern).expect("built-in redaction patterns must compile"))
            .collect()
    })
}

/// Replace every built-in credential in `text` with `[REDACTED]`
/// (REQUIREMENTS.md §5.10.3). Pure — same input, same output, no I/O —
/// and non-secret text passes through untouched.
pub fn redact(text: &str) -> String {
    let mut redacted = Cow::Borrowed(text);
    for pattern in patterns() {
        if let Cow::Owned(replaced) = pattern.replace_all(&redacted, REPLACEMENT) {
            redacted = Cow::Owned(replaced);
        }
    }
    redacted.into_owned()
}
