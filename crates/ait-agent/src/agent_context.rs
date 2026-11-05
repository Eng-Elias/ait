//! The agent system-prompt seam (Spec R06, §5.6): cwd, OS, shell, the
//! nearest `AGENTS.md` found by walking up from cwd, and the last
//! [`OUTPUT_LINE_BUDGET`] lines of terminal output — redacted through
//! the request's [`Redactor`] (§5.10.3) — assembled
//! into the fixed system prompt every agent request carries.
//!
//! [`assemble_system_prompt`] is pure (same inputs, same prompt);
//! [`find_agents_md`] is the only filesystem read, kept separate so
//! the assembly stays testable without touching disk.
//!
//! Written in-house (CODING_STANDARDS §10) — like the `context`
//! module's suggester prompt, nothing here quotes third-party prompt
//! text.

use std::fs;
use std::path::Path;

use crate::{Redactor, OUTPUT_LINE_BUDGET};

/// The terminal-side context one agent task carries — everything §5.6
/// lets the model see beyond the conversation itself. Fields are owned
/// because the value crosses the request channel to the worker.
#[derive(Debug, Clone)]
pub struct AgentContext {
    /// The tab's working directory (OSC 7 with a `/proc` fallback).
    pub cwd: String,
    /// The shell name — `$SHELL`'s basename, e.g. `bash`.
    pub shell: String,
    /// The OS name — `std::env::consts::OS`, e.g. `linux`.
    pub os: String,
    /// Recent logical output lines, oldest first. Only the last
    /// [`OUTPUT_LINE_BUDGET`] are sent and they pass through
    /// the request's [`Redactor`] as one joined block — send the raw
    /// lines, redaction is [`assemble_system_prompt`]'s job.
    pub output_lines: Vec<String>,
}

/// The fixed head of the agent system prompt (§5.6). Written in-house
/// (CODING_STANDARDS §10); the environment, `AGENTS.md` guidance and
/// redacted output tail follow it as labeled sections so the model
/// reads the context apart from the task in the user message.
const SYSTEM_PROMPT_HEAD: &str = "You are the agent built into ait, a terminal emulator. The user \
     hands you a task; you work on it step by step and report progress \
     as you go. Reply in plain text.";

/// The `AGENTS.md` file name searched walking up from cwd.
const AGENTS_MD: &str = "AGENTS.md";

/// The nearest `AGENTS.md` to `cwd` (§5.6): walk `cwd`, then each
/// ancestor, and return the contents of the first readable file by
/// that name. `None` when none exists or none reads — a missing or
/// unreadable file degrades the prompt, never the task.
pub(crate) fn find_agents_md(cwd: &Path) -> Option<String> {
    for dir in cwd.ancestors() {
        let candidate = dir.join(AGENTS_MD);
        if candidate.is_file() {
            if let Ok(contents) = fs::read_to_string(&candidate) {
                return Some(contents);
            }
        }
    }
    None
}

/// Assemble the system prompt for one task: the fixed head, the
/// environment block, the nearest `AGENTS.md` contents when
/// `agents_md` came back, the "MCP servers" section the task's
/// `mcp.json` reconcile rendered (`mcp_section` is `None` when no
/// servers are declared — Spec R10 us-6/7, §5.10.1), the "Available
/// skills" section the request's skills-catalog snapshot rendered
/// (`skills_section` is `None` when no skills are loaded — Spec R10
/// us-23, §5.10.2), and the redacted output tail. Pure — no I/O, no
/// clock; `find_agents_md`, the reconcile and the catalog scan run
/// in the caller.
///
/// Redaction ordering (§5.4, §5.10.3): the output tail passes through
/// the request's [`Redactor`] here, at assembly time — joined as one
/// block so a credential spanning lines (a PEM block) still matches.
/// The task text itself is never redacted; it travels verbatim in the
/// user message.
pub(crate) fn assemble_system_prompt(
    context: &AgentContext,
    agents_md: Option<&str>,
    redactor: &Redactor,
    mcp_section: Option<&str>,
    skills_section: Option<&str>,
) -> String {
    let mut prompt = String::from(SYSTEM_PROMPT_HEAD);
    prompt.push_str("\n\nEnvironment:\n");
    prompt.push_str(&format!("- Working directory: {}\n", context.cwd));
    prompt.push_str(&format!("- OS: {}\n", context.os));
    prompt.push_str(&format!("- Shell: {}\n", context.shell));
    if let Some(agents_md) = agents_md {
        prompt.push_str(
            "\nThe nearest AGENTS.md to the working directory — follow \
             its guidance:\n",
        );
        prompt.push_str(agents_md);
        if !agents_md.ends_with('\n') {
            prompt.push('\n');
        }
    }
    if let Some(mcp_section) = mcp_section {
        prompt.push('\n');
        prompt.push_str(mcp_section);
        if !mcp_section.ends_with('\n') {
            prompt.push('\n');
        }
    }
    if let Some(skills_section) = skills_section {
        prompt.push('\n');
        prompt.push_str(skills_section);
        if !skills_section.ends_with('\n') {
            prompt.push('\n');
        }
    }
    let tail_start = context
        .output_lines
        .len()
        .saturating_sub(OUTPUT_LINE_BUDGET);
    let tail = &context.output_lines[tail_start..];
    if !tail.is_empty() {
        prompt.push_str("\nRecent terminal output (oldest first, secrets redacted):\n");
        // Redact the joined tail, not each line: a credential spanning
        // grid lines only matches the assembled block — the
        // `SuggestContext` seam documents the same rule.
        prompt.push_str(&redactor.redact(&tail.join("\n")));
        prompt.push('\n');
    }
    prompt
}
