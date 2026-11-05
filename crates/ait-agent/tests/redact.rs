//! Table-driven tests at the `redact`/`Redactor` pure seam (Spec R04,
//! user stories 18, 26; Spec R10 §5.10.3 — standard 11, no PTY, model
//! or TTY): each built-in pattern family gets one positive case
//! collapsing to `[REDACTED]` and one non-secret pass-through, each
//! named pattern pins to its Round 4 specimen, the PII categories
//! mask only when enabled, and `extra`/`disable` drive the effective
//! set. Prior art: `ait-config`'s table-driven parse tests.

use ait_agent::{redact, Redactor};
use ait_config::RedactionSettings;

/// A `Redactor` with just the toggles a case needs — the rest of
/// `RedactionSettings` stays at its all-empty default.
fn redactor(extra: &[&str], enable: &[&str], disable: &[&str]) -> Redactor {
    Redactor::new(&RedactionSettings {
        extra: extra.iter().map(|s| (*s).to_owned()).collect(),
        enable: enable.iter().map(|s| (*s).to_owned()).collect(),
        disable: disable.iter().map(|s| (*s).to_owned()).collect(),
    })
}

/// (pattern family, input, expected): one positive and one non-secret
/// pass-through row per §5.10.3 credential family — the default
/// `redact` ships exactly the Round 4 built-ins.
const CASES: &[(&str, &str, &str)] = &[
    // AWS access key IDs: `AKIA` + 16 chars.
    (
        "aws access key id",
        "export AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE",
        "export AWS_ACCESS_KEY_ID=[REDACTED]",
    ),
    (
        "aws non-secret",
        "aws s3 ls lists buckets via the AWS CLI",
        "aws s3 ls lists buckets via the AWS CLI",
    ),
    // GitHub: ghp_/gho_/ghu_/ghs_ personal + OAuth tokens, github_pat_
    // fine-grained PATs.
    (
        "github classic pat",
        "token ghp_16C7e42F292c6912E7710c838347Ae178B4a leaked",
        "token [REDACTED] leaked",
    ),
    (
        "github oauth token",
        "gho_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789",
        "[REDACTED]",
    ),
    (
        "github fine-grained pat",
        "github_pat_11ABCDEFG0hijklmnopQRSTUVWXYZ_abcdef0123456789ABCDEFGHIJabcdef0123456789abcdef01234",
        "[REDACTED]",
    ),
    (
        "github non-secret",
        "gh docs explain ghp_ token scopes",
        "gh docs explain ghp_ token scopes",
    ),
    // OpenAI `sk-…`.
    (
        "openai api key",
        "OPENAI_API_KEY=sk-T9oFv3kJmQ2xR8yLpW5dNhBcUzA6sXqVgE7nM4jK",
        "OPENAI_API_KEY=[REDACTED]",
    ),
    (
        "openai non-secret",
        "sk-test is a prefix, not a key",
        "sk-test is a prefix, not a key",
    ),
    // Anthropic `sk-ant-…` — a more specific `sk-` prefix.
    (
        "anthropic api key",
        "sk-ant-api03-Ab1Cd2Ef3Gh4Ij5Kl6Mn7Op8Qr9St0Uv1Wx2Yz",
        "[REDACTED]",
    ),
    (
        "anthropic non-secret",
        "sk-ant- alone leaks nothing",
        "sk-ant- alone leaks nothing",
    ),
    // Stripe `sk_live_`/`sk_test_`.
    (
        "stripe live key",
        "sk_live_4eC39HqLyjWDarjtT1zdp7dc",
        "[REDACTED]",
    ),
    (
        "stripe test key",
        "sk_test_4eC39HqLyjWDarjtT1zdp7dc",
        "[REDACTED]",
    ),
    (
        "stripe non-secret",
        "sk_live_ and sk_test_ are stripe prefixes",
        "sk_live_ and sk_test_ are stripe prefixes",
    ),
    // Slack `xox…`/`xapp-`.
    (
        "slack bot token",
        "xoxb-1234567890123-1234567890123-AbCdEfGhIjKlMnOpQrStUvWx",
        "[REDACTED]",
    ),
    (
        "slack app-level token",
        "xapp-1-A01B2C3D4E-1234567890123-abcdef0123456789abcdef0123456789abcdef0123456789",
        "[REDACTED]",
    ),
    (
        "slack non-secret",
        "xox tokens start with xoxb- or xapp-",
        "xox tokens start with xoxb- or xapp-",
    ),
    // JWTs: three base64url segments, header always `eyJ` (`{"`).
    (
        "jwt",
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
        "[REDACTED]",
    ),
    (
        "jwt non-secret",
        "version 1.2.3 is a dotted triple, not a token",
        "version 1.2.3 is a dotted triple, not a token",
    ),
    // Private-key PEM blocks.
    (
        "pem non-secret",
        "-----BEGIN PUBLIC KEY----- is a public block",
        "-----BEGIN PUBLIC KEY----- is a public block",
    ),
    // A line carrying several secrets at once.
    (
        "multi-secret line",
        "AKIAIOSFODNN7EXAMPLE ghp_16C7e42F292c6912E7710c838347Ae178B4a sk-T9oFv3kJmQ2xR8yLpW5dNhBcUzA6sXqVgE7nM4jK xoxb-1234567890123-1234567890123-AbCdEfGhIjKlMnOpQrStUvWx",
        "[REDACTED] [REDACTED] [REDACTED] [REDACTED]",
    ),
];

#[test]
fn each_pattern_family_redacts_and_passes_clean_text() {
    for (name, input, expected) in CASES {
        assert_eq!(&redact(input), expected, "case failed: {name}");
    }
}

/// A PEM block is the one pattern family whose match spans lines: the
/// whole `-----BEGIN …-----`…`-----END …-----` region must collapse to
/// a single `[REDACTED]` marker while surrounding lines survive.
#[test]
fn pem_block_spanning_lines_collapses_to_one_marker() {
    let input = "before\n\
                 -----BEGIN RSA PRIVATE KEY-----\n\
                 MIIEpAIBAAKCAQEA7qWJf+0AbCdEfGhIjKlMnOpQrStUvWxYz012345\n\
                 6789abcdefABCDEF+/=morebase64contentherethatspanslines\n\
                 -----END RSA PRIVATE KEY-----\n\
                 after";
    assert_eq!(redact(input), "before\n[REDACTED]\nafter");
}

/// The PGP armor variant ends in `KEY BLOCK`, not `PRIVATE KEY` — the
/// anchor must still catch it.
#[test]
fn pgp_private_key_block_collapses_to_one_marker() {
    let input = "before\n\
                 -----BEGIN PGP PRIVATE KEY BLOCK-----\n\
                 xVgEZ2FiY2RlZmdoaWprbG1ub3BxcnN0dXZ3eHl6MDEyMzQ1Njc4\n\
                 -----END PGP PRIVATE KEY BLOCK-----\n\
                 after";
    assert_eq!(redact(input), "before\n[REDACTED]\nafter");
}

// ---------------------------------------------------------------------------
// Spec R10-T1: the named pattern table and the settings-driven Redactor.
// ---------------------------------------------------------------------------

/// (pattern name, specimen) — one specimen per built-in that ONLY its
/// own pattern matches, so `disable`ing every other name still leaves
/// it masked: the pin that binds each §5.10.3 name to its Round 4
/// regex. `sk-ant-…` is the exception — OpenAI's generic `sk-` also
/// matches it, so its row proves the ordering instead (see
/// `ordering_keeps_specific_sk_prefixes_ahead_of_openai`).
const BUILTIN_SPECIMENS: &[(&str, &str)] = &[
    ("aws_access_key", "AKIAIOSFODNN7EXAMPLE"),
    ("github_token", "ghp_16C7e42F292c6912E7710c838347Ae178B4a"),
    (
        "github_pat",
        "github_pat_11ABCDEFG0hijklmnopQRSTUVWXYZ_abcdef0123456789ABCDEFGHIJabcdef0123456789abcdef01234",
    ),
    ("openai_key", "sk-T9oFv3kJmQ2xR8yLpW5dNhBcUzA6sXqVgE7nM4jK"),
    ("stripe_key", "sk_live_4eC39HqLyjWDarjtT1zdp7dc"),
    (
        "slack_token",
        "xoxb-1234567890123-1234567890123-AbCdEfGhIjKlMnOpQrStUvWx",
    ),
    (
        "slack_app",
        "xapp-1-A01B2C3D4E-1234567890123-abcdef0123456789abcdef0123456789abcdef0123456789",
    ),
    (
        "jwt",
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
    ),
    (
        "private_key_pem",
        "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA7qWJ\n-----END RSA PRIVATE KEY-----",
    ),
];

/// Every built-in name still masks its Round 4 specimen — including
/// when every OTHER built-in is disabled, which pins each name to its
/// own pattern rather than to the set as a whole (Spec R10's
/// regression pin).
#[test]
fn each_named_builtin_masks_its_specimen_on_its_own() {
    for (name, specimen) in BUILTIN_SPECIMENS {
        let others: Vec<&str> = BUILTIN_SPECIMENS
            .iter()
            .map(|(other, _)| *other)
            .filter(|other| other != name)
            .chain(["anthropic_key"])
            .collect();
        let redactor = redactor(&[], &[], &others);
        assert_eq!(
            redactor.redact(specimen),
            "[REDACTED]",
            "{name} alone must mask its specimen"
        );
    }
    // `anthropic_key` is the one name the table above cannot isolate —
    // the generic `sk-` catches its specimen too.
    let only_anthropic = redactor(
        &[],
        &[],
        &[
            "aws_access_key",
            "github_token",
            "github_pat",
            "openai_key",
            "stripe_key",
            "slack_token",
            "slack_app",
            "jwt",
            "private_key_pem",
        ],
    );
    assert_eq!(
        only_anthropic.redact("sk-ant-api03-Ab1Cd2Ef3Gh4Ij5Kl6Mn7Op8Qr9St0Uv1Wx2Yz"),
        "[REDACTED]",
        "anthropic_key alone must mask its specimen"
    );
}

/// Ordering preserved verbatim from Round 4 (§5.10.3): Anthropic's
/// `sk-ant-` and Stripe's `sk_live_`/`sk_test_` run ahead of the
/// generic `sk-`, so a more specific key collapses to a single marker
/// — never a partial match on the `sk-` prefix alone.
#[test]
fn ordering_keeps_specific_sk_prefixes_ahead_of_openai() {
    for specimen in [
        "sk-ant-api03-Ab1Cd2Ef3Gh4Ij5Kl6Mn7Op8Qr9St0Uv1Wx2Yz",
        "sk_live_4eC39HqLyjWDarjtT1zdp7dc",
    ] {
        assert_eq!(redact(specimen), "[REDACTED]", "{specimen}");
    }
    // With `openai_key` disabled the specifics still mask; with them
    // disabled too, the generic `sk-` alone masks them — proof the
    // table applies in declared order, not one combined set.
    let no_openai = redactor(&[], &[], &["openai_key"]);
    assert_eq!(
        no_openai.redact("sk-ant-api03-Ab1Cd2Ef3Gh4Ij5Kl6Mn7Op8Qr9St0Uv1Wx2Yz"),
        "[REDACTED]",
        "anthropic_key must still mask without openai_key"
    );
    let no_specifics = redactor(&[], &[], &["anthropic_key", "stripe_key"]);
    assert_eq!(
        no_specifics.redact("sk-ant-api03-Ab1Cd2Ef3Gh4Ij5Kl6Mn7Op8Qr9St0Uv1Wx2Yz"),
        "[REDACTED]",
        "the generic sk- must catch what the specifics would have"
    );
}

/// `disable` turns a built-in off: its specimen passes through
/// verbatim while unrelated built-ins still mask (§5.10.3).
#[test]
fn a_disabled_builtin_no_longer_masks() {
    let no_jwt = redactor(&[], &[], &["jwt"]);
    assert_eq!(
        no_jwt.redact(
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"
        ),
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"
    );
    assert_eq!(
        no_jwt.redact("AKIAIOSFODNN7EXAMPLE"),
        "[REDACTED]",
        "unrelated built-ins still mask"
    );
    // `openai_key` is the ordering edge: disabling it exposes the
    // generic `sk-` catch — `sk-ant-`/`sk_live_` keys still mask
    // through their own patterns, but an ordinary OpenAI key leaks…
    let no_openai = redactor(&[], &[], &["openai_key"]);
    assert_eq!(
        no_openai.redact("sk-T9oFv3kJmQ2xR8yLpW5dNhBcUzA6sXqVgE7nM4jK"),
        "sk-T9oFv3kJmQ2xR8yLpW5dNhBcUzA6sXqVgE7nM4jK"
    );
}

/// The PII categories ship disabled (§5.10.3): ordinary addresses,
/// emails, phones and MACs pass through by default — and each masks
/// its specimen only once enabled.
#[test]
fn pii_categories_mask_only_when_enabled() {
    let cases: &[(&str, &str)] = &[
        ("pii_ipv4", "ping 192.168.1.1"),
        ("pii_ipv6", "route via fe80::1"),
        ("pii_email", "mail alerts@example.com"),
        ("pii_phone", "call +1 (415) 555-0132"),
        ("pii_mac", "arp 00:1B:44:11:3A:B7"),
    ];
    for (name, input) in cases {
        assert_eq!(
            redact(input),
            *input,
            "{name} must stay off by default: {input}"
        );
        let redactor = redactor(&[], &[name], &[]);
        assert!(
            redactor.redact(input).contains("[REDACTED]"),
            "{name} enabled must mask {input}"
        );
    }
}

/// PII specimens in detail: each category's own pattern produces the
/// single `[REDACTED]` the user asked for.
#[test]
fn each_pii_category_masks_its_specimen() {
    let cases: &[(&str, &str, &str)] = &[
        ("pii_ipv4", "ping 192.168.1.1", "ping [REDACTED]"),
        ("pii_ipv6", "route via fe80::1", "route via [REDACTED]"),
        (
            "pii_ipv6",
            "2001:db8::ff00:42:8329",
            "[REDACTED]",
        ),
        (
            "pii_email",
            "mail alerts@example.com",
            "mail [REDACTED]",
        ),
        (
            "pii_phone",
            "call +1 (415) 555-0132",
            "call [REDACTED]",
        ),
        ("pii_phone", "dial 415-555-0132", "dial [REDACTED]"),
        ("pii_mac", "arp 00:1B:44:11:3A:B7", "arp [REDACTED]"),
        ("pii_mac", "arp 00-1B-44-11-3A-B7", "arp [REDACTED]"),
    ];
    for (name, input, expected) in cases {
        let redactor = redactor(&[], &[name], &[]);
        assert_eq!(redactor.redact(input), *expected, "{name} on {input}");
    }
}

/// `extra` user regexes mask what the built-ins miss (§5.10.3,
/// Spec R10 story 30), alongside the unchanged built-ins.
#[test]
fn extra_patterns_mask_alongside_the_builtins() {
    let redactor = redactor(&[r"\btoken-[A-Z0-9]{8}"], &[], &[]);
    assert_eq!(
        redactor.redact("deploy token-A1B2C3D4 now"),
        "deploy [REDACTED] now"
    );
    assert_eq!(redactor.redact("AKIAIOSFODNN7EXAMPLE"), "[REDACTED]");
}

/// A `Redactor` built from the settings defaults is the Round 4 set —
/// `extra`/`enable`/`disable` empty means the same mask as `redact`.
#[test]
fn a_default_redactor_matches_the_static_redact() {
    let settings_driven = redactor(&[], &[], &[]);
    for (_, input, expected) in CASES {
        assert_eq!(&settings_driven.redact(input), expected, "case failed: {input}");
    }
}
