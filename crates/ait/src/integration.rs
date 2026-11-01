//! Per-tab shell-integration state and the spawn-time injection seam
//! (REQUIREMENTS.md §5.5, Spec R05 / issue #72).
//!
//! Two pieces live here so `app.rs` stays presentation-only
//! (CODING_STANDARDS.md 9):
//!
//! - [`IntegrationState`] — the per-tab state machine
//!   `TabManager::drain_events` folds every observed [`SessionEvent`]
//!   into: `Absent` → `MarksOnly` on an `OSC 133` mark, a *vetted*
//!   `init_shell` / `bootstrapped` hook → `Integrated`, a vetted
//!   `exit_shell` → `Absent`. `SessionEvent::Private` arrives already
//!   decoded by the drain (one `ait_shell::decode` per sequence, shared
//!   with the block and in-band observers); malformed sequences decode
//!   to `None` and move nothing — the state machine never panics on
//!   garbage bytes.
//! - [`inject`] — the spawn-time half: plan + materialize an
//!   `ait_shell` injection on the resolved program and apply its
//!   argv/env rewrite to [`LiveSessionOptions`]. Every degrade path —
//!   opt-out handled by the caller, unknown shells and filesystem
//!   failures here — returns `None` and the session spawns exactly as
//!   it would without integration (standard 1).

use std::path::Path;

use ait_shell::{HookName, Materialized, ShellEvent};
use ait_term::{LiveSessionOptions, SessionEvent};

/// How much shell integration a tab's session has reported (GLOSSARY:
/// "integration state"). `__ait_run` gating, the status line's block
/// segment and the suggester's `/proc` cwd fallback consume it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IntegrationState {
    /// No integration signals — a shell `ait` doesn't inject (or an
    /// opted-out spawn), or a session that already sent `exit_shell`.
    /// The terminal behaves exactly as it did before Round 5.
    #[default]
    Absent,
    /// `OSC 133` marks arrive but no `init_shell` hook — a shell with
    /// its own semantic-prompt integration (Spec R05 us-19: marks still
    /// pay off) or a stray emitter. AIT-private hooks are not live.
    MarksOnly,
    /// `init_shell` or `bootstrapped` arrived *with the tab's
    /// `AIT_SESSION_ID` echoed back* — the rc snippet is live and hook
    /// payloads stay attributable to this tab.
    Integrated,
}

impl IntegrationState {
    /// Fold one observed session event into the state — the per-tab
    /// bookkeeping `TabManager::drain_events` applies beside its other
    /// bookkeeping. `decoded` is the drain's shared `ait_shell::decode`
    /// of `event` (`None` when it isn't a `Private` sequence or didn't
    /// decode); `session_id` is the tab's allocated `AIT_SESSION_ID`.
    ///
    /// Vetting (Spec R05 us-20): a lifecycle hook is honored only when
    /// its echoed `session_id` matches the allocated one — `init_shell`
    /// bytes on a PTY must never promote an arbitrary stream to
    /// `Integrated` and unlock `__ait_run`. An unvetted stream that
    /// speaks hooks lands in `MarksOnly`, the spec's state for
    /// integration `ait` can't account for; `OSC 133` marks still work
    /// regardless (us-19). Non-hook decodes, undecodable `Private`
    /// sequences and everything else move nothing.
    pub fn observe(
        &mut self,
        event: &SessionEvent,
        decoded: Option<&ShellEvent>,
        session_id: Option<&str>,
    ) {
        match event {
            // Any OSC 133 without a prior init_shell: the shell speaks
            // prompt marks on its own — marks-only integration.
            SessionEvent::PromptMark { .. } if *self == Self::Absent => {
                *self = Self::MarksOnly;
            }
            SessionEvent::Private { .. } => match decoded {
                Some(ShellEvent::Hook {
                    name: HookName::InitShell | HookName::Bootstrapped,
                    value,
                }) => {
                    // The echoed id must equal the allocated one —
                    // `Some(id) == expected` needs both present.
                    if ait_shell::session_id(value)
                        .as_deref()
                        .is_some_and(|id| session_id.is_some_and(|expected| expected == id))
                    {
                        *self = Self::Integrated;
                    } else if *self == Self::Absent {
                        // An unvetted stream — hooks we didn't inject.
                        *self = Self::MarksOnly;
                    }
                }
                Some(ShellEvent::Hook {
                    name: HookName::ExitShell,
                    value,
                }) if ait_shell::session_id(value)
                    .as_deref()
                    .is_some_and(|id| session_id.is_some_and(|expected| expected == id)) =>
                {
                    *self = Self::Absent;
                }
                // precmd/preexec/unknown hooks, the 7701 run channel,
                // reserved codes, unvetted exit_shells and undecodable
                // Private sequences don't move the state.
                _ => {}
            },
            _ => {}
        }
    }
}

/// Plan and materialize shell integration for the session `options`
/// describes, applying the argv/env rewrite in place. Returns the
/// [`Materialized`] guard the owning `Tab` must hold — its drop removes
/// the per-session `0700` snippet dir (Spec R05 us-30).
///
/// `session_id` is exported to the shell as `AIT_SESSION_ID` and echoed
/// back in every hook payload; the caller supplies the tab's stable id
/// (CODING_STANDARDS.md 14 — never an index or a pid).
///
/// `None` is the graceful degrade (standard 1): the resolved program
/// isn't a supported shell, or the session dir couldn't be written —
/// `options` is left untouched and the spawn proceeds uninjected.
pub fn inject(options: &mut LiveSessionOptions, session_id: &str) -> Option<Materialized> {
    let injection = ait_shell::plan(Path::new(&options.resolved_program()))?;
    let mut materialized = injection.materialize().ok()?;
    materialized.set_session_id(session_id);
    // The planner's args lead (`bash --rcfile <wrapper> …`,
    // `fish --init-command …`) — the user's own args keep their slots.
    let mut args = materialized.args().to_vec();
    args.append(&mut options.args);
    options.args = args;
    // Env overrides layer on last so the injected vars win.
    options.env.extend(materialized.env().iter().cloned());
    Some(materialized)
}

#[cfg(test)]
mod tests {
    //! The pure seam: `SessionEvent`s are constructed in place — the
    //! real `sh`-spawned plumbing tests live in `tabs.rs`.
    use super::*;
    use ait_term::PromptMarkKind;
    use std::ffi::OsString;

    /// `OSC 7700 ; <hex>` as a `SessionEvent::Private` — the shape
    /// `feed_bytes` hands the drain.
    fn hook_event(json: &str) -> SessionEvent {
        let hex: String = json.bytes().map(|b| format!("{b:02x}")).collect();
        SessionEvent::Private {
            code: b"7700".to_vec(),
            params: vec![hex.into_bytes()],
        }
    }

    /// The drain's shared decode of `event` — what `observe` sees.
    fn decoded(event: &SessionEvent) -> Option<ShellEvent> {
        match event {
            SessionEvent::Private { code, params } => ait_shell::decode(code, params),
            _ => None,
        }
    }

    /// `observe` the way `drain_events` calls it: shared decode plus
    /// the tab's allocated session id.
    fn observe(state: &mut IntegrationState, event: &SessionEvent, session_id: Option<&str>) {
        state.observe(event, decoded(event).as_ref(), session_id);
    }

    fn mark() -> SessionEvent {
        SessionEvent::PromptMark {
            kind: PromptMarkKind::PromptStart,
            payload: None,
        }
    }

    /// Spec R05 / #72: `init_shell` and `bootstrapped` integrate,
    /// `exit_shell` drops back to `Absent`, and an `OSC 133` mark
    /// without init reports `MarksOnly` — all gated on the hook
    /// payload's `session_id` matching the tab's allocated id.
    #[test]
    fn vetted_hook_events_drive_the_state() {
        let mut state = IntegrationState::Absent;

        observe(
            &mut state,
            &hook_event(r#"{"hook":"init_shell","value":{"session_id":"0","shell":"bash"}}"#),
            Some("0"),
        );
        assert_eq!(state, IntegrationState::Integrated);

        // Marks and the run channel don't move an integrated session.
        observe(&mut state, &mark(), Some("0"));
        state.observe(
            &SessionEvent::Private {
                code: b"7701".to_vec(),
                params: vec![b"A".to_vec()],
            },
            Some(&ShellEvent::RunBegin),
            Some("0"),
        );
        assert_eq!(state, IntegrationState::Integrated);

        observe(
            &mut state,
            &hook_event(r#"{"hook":"exit_shell","value":{"session_id":"0"}}"#),
            Some("0"),
        );
        assert_eq!(state, IntegrationState::Absent);

        // bootstrapped integrates from Absent too (Spec R05 decision:
        // `init_shell`/`bootstrapped` → `Integrated`).
        observe(
            &mut state,
            &hook_event(r#"{"hook":"bootstrapped","value":{"session_id":"0"}}"#),
            Some("0"),
        );
        assert_eq!(state, IntegrationState::Integrated);
    }

    /// The review's vetting fix: `init_shell`/`bootstrapped` whose
    /// `session_id` is missing, empty, or simply not this tab's never
    /// promotes — on an `Absent` tab the unvetted stream lands in
    /// `MarksOnly` (us-19/20), and a numeric `session_id` normalizes
    /// to the same comparison the quoted string form uses.
    #[test]
    fn only_the_allocated_session_id_integrates() {
        for json in [
            r#"{"hook":"init_shell","value":{"session_id":"99"}}"#, // wrong id
            r#"{"hook":"init_shell","value":{"session_id":99}}"#,   // wrong, numeric
            r#"{"hook":"init_shell","value":{"session_id":""}}"#,   // empty
            r#"{"hook":"init_shell","value":{}}"#,                  // absent
            r#"{"hook":"init_shell","value":{"session_id":null}}"#, // null
            r#"{"hook":"bootstrapped","value":{"session_id":"1"}}"#, // bootstrapped too
        ] {
            let mut state = IntegrationState::Absent;
            observe(&mut state, &hook_event(json), Some("0"));
            assert_eq!(state, IntegrationState::MarksOnly, "{json}");
            // A forged hook can't downgrade or bootstrap a session.
            let mut state = IntegrationState::Integrated;
            observe(&mut state, &hook_event(json), Some("0"));
            assert_eq!(state, IntegrationState::Integrated, "{json}");
        }

        // Numeric wire form matches the same allocated id.
        let mut state = IntegrationState::Absent;
        observe(
            &mut state,
            &hook_event(r#"{"hook":"init_shell","value":{"session_id":0}}"#),
            Some("0"),
        );
        assert_eq!(state, IntegrationState::Integrated);

        // No allocated id at all → nothing can ever vouch for the tab.
        let mut state = IntegrationState::Absent;
        observe(
            &mut state,
            &hook_event(r#"{"hook":"init_shell","value":{"session_id":"0"}}"#),
            None,
        );
        assert_eq!(state, IntegrationState::MarksOnly);

        // An unvetted exit_shell can't tear down a vetted session.
        let mut state = IntegrationState::Integrated;
        observe(
            &mut state,
            &hook_event(r#"{"hook":"exit_shell","value":{"session_id":"99"}}"#),
            Some("0"),
        );
        assert_eq!(state, IntegrationState::Integrated);
    }

    /// `OSC 133` alone upgrades `Absent` → `MarksOnly` but never moves
    /// an `Integrated` session; a vetted `init_shell` still wins from
    /// `MarksOnly` (Spec R05 us-19: marks pay off on any stream).
    #[test]
    fn marks_only_without_init_then_integrated() {
        let mut state = IntegrationState::Absent;
        observe(&mut state, &mark(), Some("0"));
        assert_eq!(state, IntegrationState::MarksOnly);
        observe(&mut state, &mark(), Some("0"));
        assert_eq!(state, IntegrationState::MarksOnly);

        observe(
            &mut state,
            &hook_event(r#"{"hook":"init_shell","value":{"session_id":"0"}}"#),
            Some("0"),
        );
        assert_eq!(state, IntegrationState::Integrated);

        // A late mark on an integrated session changes nothing.
        observe(&mut state, &mark(), Some("0"));
        assert_eq!(state, IntegrationState::Integrated);
    }

    /// Malformed hook bytes, reserved codes and foreign OSCs are all
    /// inert — decode's `None` and `Reserved` move nothing, and
    /// precmd/preexec are not lifecycle events.
    #[test]
    fn noise_never_moves_the_state() {
        for event in [
            // Odd-length hex → decode returns None.
            SessionEvent::Private {
                code: b"7700".to_vec(),
                params: vec![b"abc".to_vec()],
            },
            // Non-JSON hex payload → None.
            SessionEvent::Private {
                code: b"7700".to_vec(),
                params: vec![b"deadbeef".to_vec()],
            },
            // Reserved 770x code.
            SessionEvent::Private {
                code: b"7702".to_vec(),
                params: vec![],
            },
            // Foreign code — not ours at all.
            SessionEvent::Private {
                code: b"9999".to_vec(),
                params: vec![b"x".to_vec()],
            },
            // A known-but-not-lifecycle hook.
            hook_event(r#"{"hook":"preexec","value":{"session_id":"0","command":"ls"}}"#),
            SessionEvent::CwdChanged(Path::new("/tmp").to_path_buf()),
        ] {
            let mut state = IntegrationState::Absent;
            observe(&mut state, &event, Some("0"));
            assert_eq!(state, IntegrationState::Absent, "{event:?}");
        }
    }

    /// `inject` on a supported shell rewrites argv (planner args lead)
    /// and env (`AIT_SESSION_ID` lands), and returns the dir guard;
    /// an unsupported program plans to `None` and leaves the options
    /// untouched.
    #[test]
    fn inject_rewrites_supported_shells_only() {
        let mut options = LiveSessionOptions {
            program: Some(OsString::from("/bin/bash")),
            args: vec![OsString::from("-l")],
            ..Default::default()
        };
        let materialized = inject(&mut options, "7").expect("bash plans an injection");

        // `--rcfile <dir>/bashrc` leads, the caller's `-l` keeps its slot.
        assert_eq!(options.args.len(), 3);
        assert_eq!(options.args[0], OsString::from("--rcfile"));
        assert_eq!(
            Path::new(&options.args[1]),
            materialized.dir().join("bashrc")
        );
        assert_eq!(options.args[2], OsString::from("-l"));
        assert!(materialized.dir().join("snippet.bash").exists());
        assert!(options.env.contains(&(
            OsString::from(ait_shell::ENV_SESSION_ID),
            OsString::from("7")
        )));

        // Unknown shells leave everything untouched.
        let mut plain = LiveSessionOptions {
            program: Some(OsString::from("/bin/sh")),
            ..Default::default()
        };
        let snapshot = plain.clone();
        assert!(inject(&mut plain, "7").is_none());
        assert_eq!(plain.args, snapshot.args);
        assert_eq!(plain.env, snapshot.env);
    }
}
