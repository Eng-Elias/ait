//! The §5.10.3 credential-redaction seam (Spec R04, user stories 18,
//! 26; completed in Round 10): a named pattern table covering the
//! spec's built-in credential list plus the opt-in PII categories,
//! compiled into a [`Redactor`] from each request's
//! [`RedactionSettings`] snapshot and applied to terminal output and
//! tool results before they enter a provider conversation.
//!
//! The names themselves live in [`ait_config::pattern_names`] — the
//! `[redaction]` `enable`/`disable` lists validate against the same
//! constants this table keys on (CODING_STANDARDS §17), so the
//! vocabulary cannot drift between what the user may write and what
//! masks.

use ait_config::{pattern_names, RedactionSettings};
use regex::Regex;
use std::borrow::Cow;
use std::sync::OnceLock;

/// The marker every matched credential collapses to.
const REPLACEMENT: &str = "[REDACTED]";

/// One row of the §5.10.3 pattern table: the stable name
/// `[redaction]` toggles reference, whether the pattern runs by
/// default, and its regex.
struct NamedPattern {
    /// The `ait_config::pattern_names` constant — never a fresh
    /// literal (CODING_STANDARDS §17).
    name: &'static str,
    /// `false` for the opt-in PII categories: they over-match ordinary
    /// output (timestamps, versions, addresses) so they ship disabled.
    default_on: bool,
    /// The regex — built-in patterns are leading-`\b`-anchored so
    /// prefixes inside a larger word (`task-…`, `key=…`) never match;
    /// the trailing end stays unanchored — over-redacting a glued-on
    /// suffix beats leaking a partial key.
    pattern: &'static str,
}

/// The §5.10.3 pattern table. Built-in ordering matters and is
/// preserved verbatim from Round 4: Anthropic's `sk-ant-` and
/// Stripe's `sk_live_`/`sk_test_` precede OpenAI's generic `sk-…`,
/// whose character class would otherwise also match those keys.
/// A PEM match spans lines, so per-pattern `Regex`es (not a
/// `RegexSet` — which only reports which pattern matched, and cannot
/// express per-pattern toggles) carry the table (issue #58).
const PATTERN_TABLE: &[NamedPattern] = &[
    NamedPattern {
        name: pattern_names::ANTHROPIC_KEY,
        default_on: true,
        pattern: r"\bsk-ant-[A-Za-z0-9_-]{20,}",
    },
    NamedPattern {
        name: pattern_names::STRIPE_KEY,
        default_on: true,
        pattern: r"\bsk_(?:live|test)_[A-Za-z0-9]{16,}",
    },
    NamedPattern {
        name: pattern_names::OPENAI_KEY,
        default_on: true,
        pattern: r"\bsk-[A-Za-z0-9_-]{20,}",
    },
    NamedPattern {
        name: pattern_names::AWS_ACCESS_KEY,
        default_on: true,
        pattern: r"\bAKIA[0-9A-Z]{16}",
    },
    NamedPattern {
        name: pattern_names::GITHUB_TOKEN,
        default_on: true,
        pattern: r"\b(?:ghp|gho|ghu|ghs)_[A-Za-z0-9]{20,}",
    },
    NamedPattern {
        name: pattern_names::GITHUB_PAT,
        default_on: true,
        pattern: r"\bgithub_pat_[A-Za-z0-9_]{20,}",
    },
    NamedPattern {
        name: pattern_names::SLACK_TOKEN,
        default_on: true,
        pattern: r"\bxox[a-z]-[A-Za-z0-9-]{10,}",
    },
    NamedPattern {
        name: pattern_names::SLACK_APP,
        default_on: true,
        pattern: r"\bxapp-[A-Za-z0-9-]{10,}",
    },
    // JWTs: three base64url segments; the header always encodes `{"`,
    // hence the `eyJ` anchor.
    NamedPattern {
        name: pattern_names::JWT,
        default_on: true,
        pattern: r"\beyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}",
    },
    // Private-key PEM blocks — `[\s\S]` lets the match span lines, so
    // a whole `-----BEGIN …-----`…`-----END …-----` block collapses
    // to one marker. Trailing words after `PRIVATE KEY` (` BLOCK` in
    // `PGP PRIVATE KEY BLOCK`) stay inside the anchor.
    NamedPattern {
        name: pattern_names::PRIVATE_KEY_PEM,
        default_on: true,
        pattern: r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY[ A-Z0-9]*-----[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY[ A-Z0-9]*-----",
    },
    // PII — the opt-in categories (§5.10.3): precise enough to catch
    // their specimen, loose enough to over-match ordinary output,
    // which is why they ship disabled.
    NamedPattern {
        name: pattern_names::PII_IPV4,
        default_on: false,
        pattern: r"\b(?:(?:25[0-5]|2[0-4][0-9]|1?[0-9]?[0-9])\.){3}(?:25[0-5]|2[0-4][0-9]|1?[0-9]?[0-9])\b",
    },
    NamedPattern {
        name: pattern_names::PII_IPV6,
        default_on: false,
        // Full form, compressed (`…::…`), and `::`-leading forms.
        pattern: r"\b(?:[0-9A-Fa-f]{1,4}:){2,7}[0-9A-Fa-f]{1,4}\b|\b(?:[0-9A-Fa-f]{1,4}:){1,7}:(?:[0-9A-Fa-f]{1,4}:?){0,7}|::(?:[0-9A-Fa-f]{1,4}:?){0,7}\b",
    },
    NamedPattern {
        name: pattern_names::PII_EMAIL,
        default_on: false,
        pattern: r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b",
    },
    NamedPattern {
        name: pattern_names::PII_PHONE,
        default_on: false,
        // International/parenthesized 10-digit forms first — the
        // 7-digit tail alone must not clip a longer number. No
        // leading `\b`: `+` carries no word boundary, so anchoring
        // there would strand `+1 (415) …` prefixes.
        pattern: r"(?:\+[0-9]{1,3}[ .-]?)?(?:\([0-9]{2,5}\)[ .-]?)?[0-9]{3}[ .-][0-9]{3}[ .-][0-9]{4}\b|(?:\+[0-9]{1,3}[ .-]?)?(?:\([0-9]{2,5}\)[ .-]?)?[0-9]{3,4}[ .-][0-9]{4}\b",
    },
    NamedPattern {
        name: pattern_names::PII_MAC,
        default_on: false,
        pattern: r"\b(?:[0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}\b|\b(?:[0-9A-Fa-f]{2}-){5}[0-9A-Fa-f]{2}\b",
    },
];

/// The compiled §5.10.3 pattern set for one request — built from the
/// request's [`RedactionSettings`] snapshot (Spec R10): the default-on
/// built-ins in table order minus `disable`d names, then the
/// `enable`d PII categories in table order, then the `extra` user
/// regexes. Per-request rather than static, so a settings hot-reload
/// takes effect on the next task or suggestion with no plumbing.
pub struct Redactor {
    patterns: Vec<Regex>,
}

impl Redactor {
    /// Compile the effective set for `settings`. `Settings::from_toml`
    /// already validated the lists — an entry that still fails to
    /// compile or resolve (a `RedactionSettings` built by hand, not
    /// parsed) degrades to "pattern absent" rather than wedging the
    /// worker it is built on (§6).
    pub fn new(settings: &RedactionSettings) -> Self {
        let mut patterns = Vec::new();
        for entry in PATTERN_TABLE {
            let on = if entry.default_on {
                !settings.disable.iter().any(|name| name == entry.name)
            } else {
                settings.enable.iter().any(|name| name == entry.name)
            };
            if on {
                patterns.push(
                    Regex::new(entry.pattern).expect("built-in redaction patterns must compile"),
                );
            }
        }
        for pattern in &settings.extra {
            if let Ok(regex) = Regex::new(pattern) {
                patterns.push(regex);
            }
        }
        Self { patterns }
    }

    /// Replace every configured pattern's matches in `text` with
    /// `[REDACTED]` (§5.10.3). Pure — same input, same output, no
    /// I/O — and non-secret text passes through untouched.
    pub fn redact(&self, text: &str) -> String {
        let mut redacted = Cow::Borrowed(text);
        for pattern in &self.patterns {
            if let Cow::Owned(replaced) = pattern.replace_all(&redacted, REPLACEMENT) {
                redacted = Cow::Owned(replaced);
            }
        }
        redacted.into_owned()
    }
}

/// The default redactor — the built-ins with no toggles — shared
/// across [`redact`] calls so the Round 4 set stays compiled once.
fn default_redactor() -> &'static Redactor {
    static DEFAULT: OnceLock<Redactor> = OnceLock::new();
    DEFAULT.get_or_init(|| Redactor::new(&RedactionSettings::default()))
}

/// Replace every built-in credential in `text` with `[REDACTED]` —
/// [`Redactor::redact`] on the default settings. For callers with no
/// `Settings` snapshot; every request-time call site (context
/// assembly, tool results) must go through the request's own
/// [`Redactor`] instead so `[redaction]` toggles apply.
pub fn redact(text: &str) -> String {
    default_redactor().redact(text)
}
