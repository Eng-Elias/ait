//! Table-driven tests at the `redact` pure seam (Spec R04, user stories
//! 18, 26; standard 11 — no PTY, model or TTY): each §5.10.3 pattern
//! family gets one positive case collapsing to `[REDACTED]` and one
//! non-secret pass-through, plus a multi-secret line and a PEM block
//! spanning lines. Prior art: `ait-config`'s table-driven parse tests.

use ait_agent::redact;

/// (pattern family, input, expected): one positive and one non-secret
/// pass-through row per §5.10.3 credential family.
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
