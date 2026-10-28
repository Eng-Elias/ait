//! `ait-agent` — AIT's agent core (REQUIREMENTS.md §3.1, ADR-0003).
//!
//! Round 4 lands the crate's first seam: [`redact`], the §5.10.3
//! credential-redaction filter guarding every byte of terminal output
//! before it reaches a provider (Spec R04, user stories 18, 26). It is
//! pure — `&str` in, `String` out — so every pattern family is
//! testable without a PTY, model or TTY (CODING_STANDARDS.md §11).
//! The `CommandSuggester`, context assembly, tool registry, agent loop
//! and permission engine build on this crate in later tickets.

mod redact;

pub use redact::redact;
