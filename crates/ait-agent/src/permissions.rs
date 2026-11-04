//! The §5.7 permission engine (Spec R07, ticket R07-T1): a pure
//! verdict seam — [`evaluate`] maps `(active profile, tool call,
//! gate memory)` to [`Verdict`], and the gate's consult point inside
//! `gate_and_execute` consumes it. Round 6's interim ask-everything
//! gate is not deleted; it is what a missing profile evaluates to —
//! the fail-safe asks rather than silently loosening.
//!
//! In scope so far: the tool → profile-field mapping, the
//! `always_allow`/`always_ask`/`never` levels, the `ask_user_question`
//! special case (any non-`never` level behaves as before — its own
//! card IS the ask), the `agent_decides` self-assessment flag
//! (T2): a call whose own arguments carry `safety: "safe"` allows —
//! `"unsafe"`, an absent flag or a malformed one asks (the flag's
//! schema is what `tools::declare` injects into the governed tools'
//! declarations — [`is_agent_decided`]), and `run_command`'s
//! `command_denylist`/`command_allowlist` consult (T3).
//! `ask_on_first_write` degrades to `always_ask` until its
//! first-write memory (T5) lands; the `directory_allowlist` (T4)
//! widens confinement at the `ToolCwd` seam, not here — an
//! allowlist expands what file tools may touch, never a verdict;
//! `mcp_*` fields and
//! `context_window_limit` stay inert (Rounds 10/8).

use std::collections::HashSet;

use ait_config::{ExecutionProfile, PermissionLevel};
use regex::Regex;

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
/// Decision order: `never` denies first — the hard floor no list,
/// verdict memory or flag crosses; `ask_user_question` at a
/// non-`never` level allows through to its own card; for `run_command`
/// the profile's `command_denylist` then `command_allowlist` consult
/// the verbatim `command` argument (unanchored `regex::is_match`)
/// — a denylist hit refuses with the same profile-denial text
/// `never` produces, an allowlist hit allows; an allow-for-task
/// memory allows; then the mapped level decides — `always_allow`
/// allows, `agent_decides` consults the call's own `safety` flag
/// (`"safe"` allows; `"unsafe"`, absent or malformed asks), and
/// everything else asks (`always_ask`, an unset field, an unmapped
/// tool name, and `ask_on_first_write` degrading until T5 lands).
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
    // `run_command` alone consults the command lists (Spec R07, T3)
    // — after the `never` floor, before the memory and the level.
    if call.name == "run_command" {
        if let Some((command, profile)) = command_arg(call).zip(profile) {
            if listed(&profile.command_denylist, &command) {
                return Verdict::Deny(format!("denied by execution profile {profile_name}"));
            }
            if listed(&profile.command_allowlist, &command) {
                return Verdict::Allow;
            }
        }
    }
    if allowed_for_task.contains(&call.name) {
        return Verdict::Allow;
    }
    match level {
        Some(PermissionLevel::AlwaysAllow) => Verdict::Allow,
        // `agent_decides` (T2): the model's self-assessment rides the
        // call's own arguments — `"safe"` runs unasked, anything else
        // asks. The `never` check above already outranks the flag: a
        // `safe`-flagged call to a forbidden tool still denies.
        Some(PermissionLevel::AgentDecides) if flagged_safe(call) => Verdict::Allow,
        _ => Verdict::Ask,
    }
}

/// `true` when `tool`'s mapped level under `profile` is
/// `agent_decides` — the question the declaration transform asks:
/// `tools::declare` injects the required `safety` argument into
/// exactly these tools' parameter schemas (Spec R07-T2). A `None`
/// profile — the missing-profile fail-safe — declares nothing extra:
/// every call asks anyway.
pub(crate) fn is_agent_decided(profile: Option<&ExecutionProfile>, tool: &str) -> bool {
    profile.and_then(|profile| level_for(profile, tool)) == Some(PermissionLevel::AgentDecides)
}

/// The `agent_decides` flag check: `true` only when the call's own
/// arguments parse and carry `safety: "safe"` verbatim — the
/// required parameter the declaration injects for `agent_decides`
/// tools. `"unsafe"`, an absent flag, a non-string value and
/// unparseable arguments all fail back to the ask path — the flag is
/// opt-in, never opt-out.
fn flagged_safe(call: &ToolCall) -> bool {
    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    args.get("safety").and_then(|v| v.as_str()) == Some("safe")
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

/// The verbatim `command` argument of a `run_command` call — the
/// text the §5.7 lists match against. A call whose arguments are
/// not JSON or carry no string `command` has nothing the lists can
/// match: the level decides and the executor reports the malformed
/// argument as a tool error, like every other bad call.
fn command_arg(call: &ToolCall) -> Option<String> {
    let args = serde_json::from_str::<serde_json::Value>(&call.arguments).ok()?;
    args.get("command")?.as_str().map(str::to_owned)
}

/// `true` when any entry in `list` — each a regex — matches
/// `command`. `Regex::is_match` is unanchored: an entry's own
/// `^`/`$` anchors decide where it binds (§5.7's shipped denylist
/// anchors its entries itself). Entries compile per call —
/// [`evaluate`] stays a pure function of the parsed `Settings`
/// snapshot, whose `Vec<String>` lists can hold no precompiled
/// form — and `Settings::from_toml` compiles every entry at load,
/// so an uncompilable pattern can only arrive through a hand-built
/// profile; it simply never matches, degrading to the level rather
/// than silently allowing or denying.
fn listed(list: &[String], command: &str) -> bool {
    list.iter()
        .any(|pattern| Regex::new(pattern).is_ok_and(|regex| regex.is_match(command)))
}

#[cfg(test)]
mod tests {
    //! The verdict seam (Spec R07 testing decisions): table-driven
    //! coverage of the level × tool matrix, the `never` deny text,
    //! the missing-profile fail-safe and the gate memory.

    use super::*;

    /// A call carrying `name` — the verdict reads the `arguments`
    /// too now (T2's flag check; T3 gives them a second consumer).
    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: String::from("call_1"),
            name: name.into(),
            arguments: String::from("{}"),
        }
    }

    /// A call carrying `name` and explicit `arguments` JSON — the
    /// `agent_decides` flag lives inside the call's own arguments.
    fn call_with(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: String::from("call_1"),
            name: name.into(),
            arguments: String::from(arguments),
        }
    }

    /// A `run_command` call carrying the verbatim `command` — what
    /// the T3 allow/deny lists match against.
    fn run_command_call(command: &str) -> ToolCall {
        ToolCall {
            id: String::from("call_1"),
            name: String::from("run_command"),
            arguments: format!(
                "{{\"command\":{}}}",
                serde_json::to_string(command).expect("a JSON string literal")
            ),
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
    /// `agent_decides` here carries no `safety` flag, so it asks;
    /// `ask_on_first_write` still degrades to ask until T5, and
    /// unset fields ask.
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

    /// `agent_decides` trusts the model's declared self-assessment
    /// (Spec R07 us-9/10): `safety: "safe"` inside the call's own
    /// arguments allows; `"unsafe"`, an absent flag, a non-string
    /// value, a differently-cased flag and unparseable arguments all
    /// ask — the flag is opt-in, never opt-out.
    #[test]
    fn agent_decides_reads_the_safety_flag() {
        let profile = profile_with("read_files", Some(PermissionLevel::AgentDecides));
        let empty = HashSet::new();
        assert_eq!(
            evaluate(
                Some(&profile),
                "default",
                &call_with("read_files", r#"{"paths":["a.rs"],"safety":"safe"}"#),
                &empty
            ),
            Verdict::Allow
        );
        for args in [
            r#"{"paths":["a.rs"],"safety":"unsafe"}"#,
            r#"{"paths":["a.rs"]}"#,
            r#"{"paths":["a.rs"],"safety":true}"#,
            r#"{"paths":["a.rs"],"safety":"Safe"}"#,
            "{not json",
        ] {
            assert_eq!(
                evaluate(
                    Some(&profile),
                    "default",
                    &call_with("read_files", args),
                    &empty
                ),
                Verdict::Ask,
                "{args}"
            );
        }
    }

    /// The flag never crosses the `never` floor: a `safe`-flagged
    /// call to a forbidden tool still denies.
    #[test]
    fn agent_decides_flag_cannot_cross_never() {
        let profile = profile_with("read_files", Some(PermissionLevel::Never));
        assert!(matches!(
            evaluate(
                Some(&profile),
                "default",
                &call_with("read_files", r#"{"paths":["a.rs"],"safety":"safe"}"#),
                &HashSet::new()
            ),
            Verdict::Deny(_)
        ));
    }

    /// `is_agent_decided` answers the declaration transform's
    /// question — `true` only for a tool whose mapped level is
    /// `agent_decides`; a `None` profile and every other level are
    /// `false`, so their declarations ship unchanged.
    #[test]
    fn is_agent_decided_marks_only_agent_decides_tools() {
        let profile = ExecutionProfile {
            read_files: Some(PermissionLevel::AgentDecides),
            execute_commands: Some(PermissionLevel::AlwaysAsk),
            ..Default::default()
        };
        for tool in ["read_files", "grep", "glob"] {
            assert!(is_agent_decided(Some(&profile), tool), "{tool}");
        }
        for tool in [
            "run_command",
            "edit_files",
            "write_to_pty",
            ASK_USER_QUESTION,
        ] {
            assert!(!is_agent_decided(Some(&profile), tool), "{tool}");
        }
        assert!(!is_agent_decided(None, "read_files"));
        let profile = profile_with("read_files", Some(PermissionLevel::AlwaysAllow));
        assert!(!is_agent_decided(Some(&profile), "read_files"));
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

    /// `run_command`'s list consult sits between `never` and the
    /// memory/level consult (Spec R07, T3): a denylist hit refuses
    /// outright — beating `always_allow`, an allowlist hit and the
    /// allow-for-task memory alike — an allowlist hit runs without
    /// asking, and no hit falls through to memory then level like
    /// every other call. The `directory_allowlist` (T4) widens
    /// `ToolCwd` confinement, never this verdict, and the
    /// `mcp_*`/`context_window_limit` fields stay parsed-but-unconsumed
    /// (Rounds 10/8): `mcp_permissions = "never"` forbids nothing the
    /// registry declares today.
    #[test]
    fn the_command_lists_decide_before_memory_and_level() {
        let empty = HashSet::new();
        // Denylist beats `always_allow`, the allowlist and memory.
        let profile = ExecutionProfile {
            execute_commands: Some(PermissionLevel::AlwaysAllow),
            command_allowlist: vec![String::from(".*")],
            command_denylist: vec![String::from("^rm")],
            ..Default::default()
        };
        let mut memory = HashSet::from([String::from("run_command")]);
        assert_eq!(
            evaluate(
                Some(&profile),
                "default",
                &run_command_call("rm -rf /"),
                &memory
            ),
            Verdict::Deny(String::from("denied by execution profile default"))
        );
        // An allowlist hit beats an `always_ask` level — no card.
        let profile = ExecutionProfile {
            execute_commands: Some(PermissionLevel::AlwaysAsk),
            command_allowlist: vec![String::from("^git ")],
            ..Default::default()
        };
        assert_eq!(
            evaluate(
                Some(&profile),
                "default",
                &run_command_call("git status"),
                &empty
            ),
            Verdict::Allow
        );
        // `never` still floors an allowlist hit.
        let profile = ExecutionProfile {
            execute_commands: Some(PermissionLevel::Never),
            command_allowlist: vec![String::from(".*")],
            ..Default::default()
        };
        assert_eq!(
            evaluate(
                Some(&profile),
                "default",
                &run_command_call("git status"),
                &empty
            ),
            Verdict::Deny(String::from("denied by execution profile default"))
        );
        // No list hit: the memory, then the level, decide unchanged.
        let profile = ExecutionProfile {
            execute_commands: Some(PermissionLevel::AlwaysAsk),
            command_allowlist: vec![String::from("^git ")],
            ..Default::default()
        };
        assert_eq!(
            evaluate(
                Some(&profile),
                "default",
                &run_command_call("cargo test"),
                &memory
            ),
            Verdict::Allow,
            "an unmatched allowlist falls to the gate memory"
        );
        memory.clear();
        assert_eq!(
            evaluate(
                Some(&profile),
                "default",
                &run_command_call("cargo test"),
                &memory
            ),
            Verdict::Ask,
            "and to the level when the memory is empty"
        );
    }

    /// Matching is `regex::Regex::is_match` — unanchored, on the
    /// verbatim command: a bare `rm` matches `sudo rm -rf /`
    /// mid-string and `echo ok\nrm -rf /` past a line break, while
    /// an anchored entry binds only where the author anchored it —
    /// `^git status$` must not allow a chained `&& rm`, and
    /// `^rm(\s.*)?$` must not swallow a multiline payload (`.` never
    /// crosses `\n`, `$` binds only the end). Hostile text the
    /// author's anchors do not cover falls through to the level —
    /// which still asks — never to a silent allow.
    #[test]
    fn command_list_matching_is_unanchored_on_the_verbatim_command() {
        let profile = ExecutionProfile {
            execute_commands: Some(PermissionLevel::AlwaysAllow),
            command_denylist: vec![String::from("rm")],
            ..Default::default()
        };
        let empty = HashSet::new();
        for command in ["sudo rm -rf /", "echo ok\nrm -rf /"] {
            assert_eq!(
                evaluate(
                    Some(&profile),
                    "default",
                    &run_command_call(command),
                    &empty
                ),
                Verdict::Deny(String::from("denied by execution profile default")),
                "{command:?}"
            );
        }
        let profile = ExecutionProfile {
            execute_commands: Some(PermissionLevel::AlwaysAsk),
            command_allowlist: vec![String::from("^git status$")],
            command_denylist: vec![String::from("^rm(\\s.*)?$")],
            ..Default::default()
        };
        for command in ["git status && rm -rf /", "rm -rf /\necho done"] {
            assert_eq!(
                evaluate(
                    Some(&profile),
                    "default",
                    &run_command_call(command),
                    &empty
                ),
                Verdict::Ask,
                "{command:?} matches neither anchored entry"
            );
        }
    }

    /// The lists govern `run_command` alone: a catch-all denylist
    /// changes nothing for the read tools — their level still
    /// decides — and a `run_command` call with no usable `command`
    /// argument (bad JSON, a missing or non-string member) has
    /// nothing verbatim to match, so it falls through to
    /// memory/level and the executor reports the bad call like any
    /// other tool error.
    #[test]
    fn the_lists_govern_run_command_only() {
        let profile = ExecutionProfile {
            read_files: Some(PermissionLevel::AlwaysAllow),
            command_denylist: vec![String::from(".*")],
            ..Default::default()
        };
        let empty = HashSet::new();
        assert_eq!(
            evaluate(Some(&profile), "default", &call("read_files"), &empty),
            Verdict::Allow,
            "a command denylist never governs read_files"
        );
        let profile = ExecutionProfile {
            execute_commands: Some(PermissionLevel::AlwaysAsk),
            command_denylist: vec![String::from(".*")],
            ..Default::default()
        };
        for arguments in ["{}", "not json", "{\"command\":7}"] {
            let call = ToolCall {
                id: String::from("call_1"),
                name: String::from("run_command"),
                arguments: String::from(arguments),
            };
            assert_eq!(
                evaluate(Some(&profile), "default", &call, &empty),
                Verdict::Ask,
                "arguments {arguments:?} carry no matchable command"
            );
        }
    }

    /// An entry that fails to compile simply never matches:
    /// `Settings::from_toml` compiles every list entry at load
    /// (ait-config's validation), so an uncompilable pattern can
    /// only arrive through a hand-built profile — and degrading to
    /// the level (which still asks) is the safe fallback, never a
    /// silent allow or deny.
    #[test]
    fn an_uncompilable_list_entry_never_matches() {
        let profile = ExecutionProfile {
            execute_commands: Some(PermissionLevel::AlwaysAsk),
            command_allowlist: vec![String::from("(")],
            command_denylist: vec![String::from("[")],
            ..Default::default()
        };
        assert_eq!(
            evaluate(
                Some(&profile),
                "default",
                &run_command_call("rm -rf /"),
                &HashSet::new()
            ),
            Verdict::Ask
        );
    }

    /// `directory_allowlist`, the `mcp_*` fields and
    /// `context_window_limit` stay parsed-but-unconsumed (T4, Rounds
    /// 10/8): a populated directory list neither relaxes nor
    /// tightens `run_command` — it is a path list, not a command
    /// list — and `mcp_permissions = "never"` forbids nothing the
    /// registry declares today.
    #[test]
    fn directory_and_mcp_lists_stay_unconsumed() {
        let profile = ExecutionProfile {
            execute_commands: Some(PermissionLevel::AlwaysAsk),
            directory_allowlist: vec![String::from(".*"), String::from("/tmp")],
            mcp_permissions: Some(PermissionLevel::Never),
            mcp_allowlist: vec![String::from("*")],
            mcp_denylist: vec![String::from("*")],
            context_window_limit: 4096,
            ..Default::default()
        };
        let empty = HashSet::new();
        assert_eq!(
            evaluate(Some(&profile), "default", &run_command_call("ls"), &empty),
            Verdict::Ask,
            "directory_allowlist is not a command list — the level decides"
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
