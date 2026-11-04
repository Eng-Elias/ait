//! The §5.7 permission engine (Spec R07, ticket R07-T1): a pure
//! verdict seam — [`evaluate`] maps `(active profile, tool call,
//! gate memory)` to [`Verdict`], and the gate's consult point inside
//! `gate_and_execute` consumes it. Round 6's interim ask-everything
//! gate is not deleted; it is what a missing profile evaluates to —
//! the fail-safe asks rather than silently loosening.
//!
//! In scope this ticket: the tool → profile-field mapping, the
//! `always_allow`/`always_ask`/`never` levels and the
//! `ask_user_question` special case (any non-`never` level behaves
//! as before — its own card IS the ask). `agent_decides` and
//! `ask_on_first_write` degrade to `always_ask` until the safety
//! flag (T2) and the first-write memory (T5) land; the command
//! allow/deny lists stay parsed-but-unconsumed (T3) while the
//! `directory_allowlist` (T4) widens confinement at the `ToolCwd`
//! seam, not here — an allowlist expands what file tools may
//! touch, never a verdict; `mcp_*` fields and
//! `context_window_limit` stay inert (Rounds 10/8).

use std::collections::HashSet;

use ait_config::{ExecutionProfile, PermissionLevel};

use crate::tool_call::ToolCall;
use crate::tools::ASK_USER_QUESTION;

/// What the gate does with one tool call — the vocabulary the
/// interim gate's `needs_verdict` boolean could not express.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Run without asking: the call's card opens `Running` (an
    /// `always_allow` level or an allow-for-task repeat). For
    /// `ask_user_question`, `Allow` reaches the ask-card flow — its
    /// own card is the consent (us-10).
    Allow,
    /// Park on the three-option approve card — *allow once*,
    /// *allow this tool for the rest of the task*, *deny* — exactly
    /// as the interim gate behaved.
    Ask,
    /// Refuse outright: the text becomes the call's tool result and
    /// the card renders `Denied`, so the model reads the refusal and
    /// can tell the user to switch profiles. The tool stays declared.
    Deny(String),
}

/// One call's verdict under the task's active execution profile.
///
/// `profile` is `None` when the profile name resolves to nothing —
/// missing, renamed or zero profiles declared — and the fail-safe is
/// the Round 6 gate verbatim: every tool asks.
/// `allowed_for_task` is the per-task memory the user's
/// *allow this tool for the rest of the task* verdicts populate —
/// per tool name, never blanket (us-7).
///
/// Decision order (R07-T1): `never` denies first — the hard floor no
/// verdict memory crosses; `ask_user_question` at a non-`never`
/// level allows through to its own card; an allow-for-task memory
/// allows; then the mapped level decides — `always_allow` allows,
/// everything else asks (`always_ask`, an unset field, an unmapped
/// tool name, and `agent_decides`/`ask_on_first_write` degrading
/// until their consumers land).
pub(crate) fn evaluate(
    profile: Option<&ExecutionProfile>,
    profile_name: &str,
    call: &ToolCall,
    allowed_for_task: &HashSet<String>,
) -> Verdict {
    let level = profile.and_then(|profile| level_for(profile, &call.name));
    if level == Some(PermissionLevel::Never) {
        return Verdict::Deny(format!("denied by execution profile {profile_name}"));
    }
    // `ask_user_question` is never gated and never executed — any
    // non-`never` level sends it to the ask card (us-10).
    if call.name == ASK_USER_QUESTION {
        return Verdict::Allow;
    }
    if allowed_for_task.contains(&call.name) {
        return Verdict::Allow;
    }
    match level {
        Some(PermissionLevel::AlwaysAllow) => Verdict::Allow,
        _ => Verdict::Ask,
    }
}

/// The tool → profile-field mapping (Spec R07): `run_command` is
/// governed by `execute_commands`, the three read-only filesystem
/// tools share `read_files`, `edit_files` rides `apply_code_diffs`,
/// and `write_to_pty`/`ask_user_question` read their same-named
/// fields. An unmapped name has no level — [`evaluate`] fails safe
/// and asks.
fn level_for(profile: &ExecutionProfile, tool: &str) -> Option<PermissionLevel> {
    match tool {
        "run_command" => profile.execute_commands,
        "read_files" | "grep" | "glob" => profile.read_files,
        "edit_files" => profile.apply_code_diffs,
        "write_to_pty" => profile.write_to_pty,
        ASK_USER_QUESTION => profile.ask_user_question,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    //! The verdict seam (Spec R07 testing decisions): table-driven
    //! coverage of the level × tool matrix, the `never` deny text,
    //! the missing-profile fail-safe and the gate memory.

    use super::*;

    /// A call carrying `name` — the verdict reads only the name in
    /// T1 (T2/T3 give `arguments` consumers).
    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: String::from("call_1"),
            name: name.into(),
            arguments: String::from("{}"),
        }
    }

    /// A profile with `level` on the field `tool` maps to.
    fn profile_with(tool: &str, level: Option<PermissionLevel>) -> ExecutionProfile {
        match tool {
            "run_command" => ExecutionProfile {
                execute_commands: level,
                ..Default::default()
            },
            "read_files" | "grep" | "glob" => ExecutionProfile {
                read_files: level,
                ..Default::default()
            },
            "edit_files" => ExecutionProfile {
                apply_code_diffs: level,
                ..Default::default()
            },
            "write_to_pty" => ExecutionProfile {
                write_to_pty: level,
                ..Default::default()
            },
            ASK_USER_QUESTION => ExecutionProfile {
                ask_user_question: level,
                ..Default::default()
            },
            _ => panic!("no field for {tool}"),
        }
    }

    /// The full level × tool matrix: `always_allow` runs, `never`
    /// denies with the profile's name, `ask_user_question` reaches
    /// its own card on every non-`never` level — and the rest ask.
    /// `agent_decides`, `ask_on_first_write` and unset fields all
    /// degrade to ask (T2/T5 land their semantics).
    #[test]
    fn the_level_by_tool_matrix() {
        let gated = [
            "run_command",
            "read_files",
            "grep",
            "glob",
            "edit_files",
            "write_to_pty",
        ];
        let empty = HashSet::new();
        for tool in gated {
            for (level, expected) in [
                (Some(PermissionLevel::AlwaysAllow), Verdict::Allow),
                (Some(PermissionLevel::AlwaysAsk), Verdict::Ask),
                (Some(PermissionLevel::AgentDecides), Verdict::Ask),
                (Some(PermissionLevel::AskOnFirstWrite), Verdict::Ask),
                (None, Verdict::Ask),
                (
                    Some(PermissionLevel::Never),
                    Verdict::Deny(String::from("denied by execution profile default")),
                ),
            ] {
                let profile = profile_with(tool, level);
                assert_eq!(
                    evaluate(Some(&profile), "default", &call(tool), &empty),
                    expected,
                    "{tool} at {level:?}"
                );
            }
        }
        // `ask_user_question`: `never` denies; every other level —
        // and an unset field — allows through to the ask card.
        for level in [
            Some(PermissionLevel::AlwaysAllow),
            Some(PermissionLevel::AlwaysAsk),
            Some(PermissionLevel::AgentDecides),
            Some(PermissionLevel::AskOnFirstWrite),
            None,
        ] {
            let profile = profile_with(ASK_USER_QUESTION, level);
            assert_eq!(
                evaluate(Some(&profile), "default", &call(ASK_USER_QUESTION), &empty),
                Verdict::Allow,
                "ask_user_question at {level:?}"
            );
        }
        let profile = profile_with(ASK_USER_QUESTION, Some(PermissionLevel::Never));
        assert_eq!(
            evaluate(Some(&profile), "default", &call(ASK_USER_QUESTION), &empty),
            Verdict::Deny(String::from("denied by execution profile default"))
        );
    }

    /// The denial text names the profile that refused — the model
    /// can tell the user which stance to switch.
    #[test]
    fn never_denies_naming_the_profile() {
        let profile = profile_with("run_command", Some(PermissionLevel::Never));
        assert_eq!(
            evaluate(
                Some(&profile),
                "locked",
                &call("run_command"),
                &HashSet::new()
            ),
            Verdict::Deny(String::from("denied by execution profile locked"))
        );
    }

    /// A missing, renamed or absent profile — `None` — evaluates
    /// every tool as `always_ask`: the Round 6 interim gate
    /// verbatim, fail-safe never silent. `ask_user_question` keeps
    /// its card (non-`never` behaves as today).
    #[test]
    fn a_missing_profile_asks_everything() {
        let empty = HashSet::new();
        for tool in [
            "run_command",
            "read_files",
            "grep",
            "glob",
            "edit_files",
            "write_to_pty",
            "a_tool_that_does_not_exist",
        ] {
            assert_eq!(
                evaluate(None, "default", &call(tool), &empty),
                Verdict::Ask,
                "{tool} under no profile"
            );
        }
        assert_eq!(
            evaluate(None, "default", &call(ASK_USER_QUESTION), &empty),
            Verdict::Allow
        );
    }

    /// *Allow this tool for the rest of the task* suppresses asks
    /// for that tool only — per name, never blanket (us-7).
    #[test]
    fn allow_for_task_memory_is_per_tool() {
        let profile = ExecutionProfile::default(); // every field unset → ask
        let mut memory = HashSet::new();
        memory.insert(String::from("read_files"));
        assert_eq!(
            evaluate(Some(&profile), "default", &call("read_files"), &memory),
            Verdict::Allow
        );
        assert_eq!(
            evaluate(Some(&profile), "default", &call("grep"), &memory),
            Verdict::Ask
        );
    }

    /// `never` is the floor: even an allow-for-task memory cannot
    /// run what the profile forbids.
    #[test]
    fn never_beats_allow_for_task_memory() {
        let profile = profile_with("read_files", Some(PermissionLevel::Never));
        let mut memory = HashSet::new();
        memory.insert(String::from("read_files"));
        assert!(matches!(
            evaluate(Some(&profile), "default", &call("read_files"), &memory),
            Verdict::Deny(_)
        ));
    }

    /// An unmapped tool name under a permissive profile still asks —
    /// the engine never invents a level for a tool it does not know.
    #[test]
    fn an_unknown_tool_asks() {
        let profile = ExecutionProfile {
            read_files: Some(PermissionLevel::AlwaysAllow),
            execute_commands: Some(PermissionLevel::AlwaysAllow),
            ..Default::default()
        };
        assert_eq!(
            evaluate(
                Some(&profile),
                "default",
                &call("some_future_mcp_tool"),
                &HashSet::new()
            ),
            Verdict::Ask
        );
    }

    /// The command lists and the `mcp_*`/`context_window_limit`
    /// fields stay parsed-but-unconsumed here (T3, Rounds 10/8) —
    /// and the `directory_allowlist` (T4) widens `ToolCwd`
    /// confinement, never this verdict: a populated allowlist does
    /// not relax a call, a populated denylist does not tighten one,
    /// and `mcp_permissions = "never"` forbids nothing the registry
    /// declares today.
    #[test]
    fn lists_and_inert_fields_stay_unconsumed() {
        let profile = ExecutionProfile {
            execute_commands: Some(PermissionLevel::AlwaysAsk),
            command_allowlist: vec![String::from(".*")],
            command_denylist: vec![String::from(".*")],
            directory_allowlist: vec![String::from("/tmp")],
            mcp_permissions: Some(PermissionLevel::Never),
            mcp_allowlist: vec![String::from("*")],
            mcp_denylist: vec![String::from("*")],
            context_window_limit: 4096,
            ..Default::default()
        };
        let empty = HashSet::new();
        assert_eq!(
            evaluate(Some(&profile), "default", &call("run_command"), &empty),
            Verdict::Ask,
            "the lists must not relax or tighten a call in T1"
        );
        let profile = ExecutionProfile {
            read_files: Some(PermissionLevel::AlwaysAllow),
            mcp_permissions: Some(PermissionLevel::Never),
            ..Default::default()
        };
        assert_eq!(
            evaluate(Some(&profile), "default", &call("read_files"), &empty),
            Verdict::Allow,
            "mcp_permissions governs no declared tool"
        );
    }
}
