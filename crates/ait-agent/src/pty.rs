//! The agent-owned PTY behind `write_to_pty` (Spec R06, §5.6 / issue
//! #90): an interactive terminal session the agent drives by repeated
//! call↔read — REPLs, `ssh`, `sudo` prompts, installers — while
//! `run_command` stays aimed at the user's own shell.
//!
//! [`AgentPty`] is lazy: nothing spawns until the first approved call
//! reaches it, so a task that never needs interactivity costs no
//! process — and a denied call can never have spawned one (the gate
//! resolves before [`tools::execute`](crate::tools) dispatches here).
//! The spawn goes through `ait-shell`'s injection planner — the same
//! plan + materialize + argv/env rewrite `ait`'s `integration::inject`
//! applies, replicated below because cores cannot depend on the
//! frontend (CODING_STANDARDS §9) — so a supported shell comes up
//! `Integrated` end to end; an unsupported one plans to `None` and
//! spawns plain (§5.5's graceful degrade), still fully writable and
//! readable. The [`Materialized`] guard lives beside the session:
//! its drop removes the per-session `0700` snippet dir.
//!
//! Each call writes the argument's bytes verbatim — the model
//! supplies its own newlines and control characters — then waits a
//! bounded settle: the loop ends when drained output has been quiet
//! for [`QUIET_WINDOW`], the child exits, cancellation lands, or the
//! hard [`SETTLE_BOUND`] trips, so a `yes`-style producer can never
//! stall the loop. The result text is the output produced since the
//! previous call, tracked on the session's own grid: `drain_events`
//! feeds every PTY byte through the emulator, and the absolute rows
//! (scrollback + viewport index) at or below the cursor that were not
//! reported yet are the answer — already escape-stripped by emulation,
//! which also keeps the injected shell's OSC 7700/133 chatter out of
//! the model's context. The last reported row is re-examined each call
//! so a half-written prompt line keeps reporting as it completes.
//!
//! The worker thread owns the `AgentPty` and pumps it — the frontend
//! never touches it. Drop is the teardown: [`LiveSession`]'s drop
//! kills the child (SIGHUP escalating to SIGKILL) and joins its
//! threads bounded, so a cancelled task, a session end or a closing
//! tab leaves no orphaned child. A session whose shell `exit`s
//! mid-task is dropped on notice and respawned by the next call — an
//! `exit` never bricks the tool.

use std::ffi::OsString;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ait_term::{
    ExitStatus, GridView, LiveEvent, LiveSession, LiveSessionOptions, PromptMarkKind, SessionEvent,
    SessionState,
};
use tokio_util::sync::CancellationToken;

use crate::files::ToolOutput;

/// One call's hard cap on the output settle — a program that never
/// goes quiet still answers inside this bound (Spec R06: "waits a
/// bounded settle for output to go quiet").
const SETTLE_BOUND: Duration = Duration::from_secs(3);

/// Output counts as settled after this much drained silence — long
/// enough for a REPL to answer, short enough for the call↔read rhythm
/// to stay responsive.
const QUIET_WINDOW: Duration = Duration::from_millis(500);

/// Once an `OSC 133;A` prompt mark has surfaced during this call —
/// the injected shell finished the command our input ran and is
/// drawing a fresh prompt — only this much silence is needed to call
/// the burst settled. A fraction of [`QUIET_WINDOW`], and
/// deterministic: the mark rides the same byte stream as the output,
/// so scheduling lag can delay but never truncate it. A shell
/// without prompt marks simply never takes this path.
const PROMPT_GRACE: Duration = Duration::from_millis(100);

/// The settle loop's poll cadence — drains are non-blocking, so the
/// loop sleeps between rounds instead of busy-spinning.
const SETTLE_POLL: Duration = Duration::from_millis(25);

/// The largest output fragment one call returns — a flood's tail,
/// never its whole scrollback.
const OUTPUT_CAP: usize = 8 * 1024;

/// The session id exported to an injected agent PTY as
/// `AIT_SESSION_ID` — distinct from the tab ids the frontend allocates
/// so hook payloads stay attributable (CODING_STANDARDS §14). Nothing
/// vets it this round; the injected shell echoes it regardless.
fn agent_session_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "agent-pty-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// The lazily-spawned interactive session `write_to_pty` drives —
/// one per agent worker, shared across that conversation's tasks so a
/// REPL or `ssh` session survives the task that opened it.
pub(crate) struct AgentPty {
    /// The live session — `None` until the first call and again after
    /// an exited session is torn down. Declared before `injection` so
    /// drop kills the child before the snippet dir it read vanishes.
    session: Option<LiveSession>,
    /// The injection's `0700` dir guard, held beside the session it
    /// rewrote so the snippet dir outlives the shell that sources it
    /// (Spec R05 us-30). `None` when the resolved program planned no
    /// injection — the degrade path that still spawns fine.
    injection: Option<ait_shell::Materialized>,
    /// Absolute row (`scrollback` + viewport index) of the last
    /// reported row — every row above it already sits in the model's
    /// context. Scrollback only grows until its cap, so the index is
    /// stable; overflow after that aliases harmlessly (bounded settle
    /// keeps floods from getting there in practice).
    reported_row: usize,
    /// The last reported row's text at report time — its growth since
    /// is the continuation a half-written prompt line produces.
    reported_tail: String,
    /// Program override: `None` resolves the user's configured shell
    /// (`$SHELL` → `/bin/sh`) through
    /// [`LiveSessionOptions::resolved_program`]; tests pin a known
    /// shell instead.
    program: Option<OsString>,
}

impl AgentPty {
    /// An unspawned agent PTY — the lazy state every worker starts in.
    pub(crate) fn new() -> Self {
        Self {
            session: None,
            injection: None,
            reported_row: 0,
            reported_tail: String::new(),
            program: None,
        }
    }

    /// `new` with the spawned program pinned — the test seam (CI
    /// `$SHELL`s vary; `/bin/sh` gives POSIX builtins everywhere).
    #[cfg(test)]
    fn with_program(program: &str) -> Self {
        Self {
            program: Some(OsString::from(program)),
            ..Self::new()
        }
    }

    /// One `write_to_pty` call: spawn lazily, write `input`'s bytes,
    /// settle bounded, return the output produced since the previous
    /// call (Spec R06 us-18). Every failure is a [`ToolOutput`] error —
    /// model-facing text, never a task failure.
    pub(crate) fn write(
        &mut self,
        args: &serde_json::Value,
        cwd: &Path,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let Some(input) = args.get("input").and_then(|v| v.as_str()) else {
            return ToolOutput::error(String::from("write_to_pty: `input` must be a string"));
        };
        // A shell that exited since the last call is torn down now —
        // the spawn below starts fresh rather than writing at a dead
        // PTY.
        if self.session_exited() {
            self.teardown();
        }
        if self.session.is_none() {
            if let Err(error) = self.spawn(cwd) {
                return ToolOutput::error(format!("write_to_pty: {error}"));
            }
        }
        let send_result = self
            .session
            .as_mut()
            .expect("spawned above")
            .send_bytes(input.as_bytes());
        if let Err(error) = send_result {
            // A write can fail on a child that died since the last
            // drain — surface the exit now so the next call respawns
            // instead of erroring at a dead PTY forever.
            if let Some(session) = self.session.as_mut() {
                let _ = session.drain_events();
            }
            if self.session_exited() {
                self.teardown();
            }
            return ToolOutput::error(format!(
                "write_to_pty: failed to write to the terminal: {error}"
            ));
        }
        let exited = self.settle(cancel);
        let mut text = self.collect_output();
        if let Some(status) = exited {
            // The session died under this call (the model typed
            // `exit`, the program quit): say so, drop the dead
            // session, let the next call respawn.
            text.push_str(&format!(
                "\n(the interactive session exited — {status:?}; \
                 a fresh one spawns on the next call)"
            ));
            self.teardown();
        }
        let text = text.trim_end();
        if text.is_empty() {
            ToolOutput::ok(String::from(
                "(the terminal produced no new output — it may still \
                 be starting; call again to read more)",
            ))
        } else if text.len() > OUTPUT_CAP {
            let mut start = text.len() - OUTPUT_CAP;
            while !text.is_char_boundary(start) {
                start += 1;
            }
            ToolOutput::ok(format!(
                "(output truncated — the last {OUTPUT_CAP} bytes follow)\n{}",
                &text[start..]
            ))
        } else {
            ToolOutput::ok(text.to_string())
        }
    }

    /// `true` once the event path has surfaced the child's exit.
    fn session_exited(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| matches!(s.state(), SessionState::Exited(_)))
    }

    /// Drop the dead session and its injection guard and reset the
    /// output watermark — the next call spawns and reports from a
    /// clean grid.
    fn teardown(&mut self) {
        self.session = None;
        self.injection = None;
        self.reported_row = 0;
        self.reported_tail.clear();
    }

    /// Spawn the resolved program in `cwd`, injected when the planner
    /// offers a plan. Replicates `ait`'s `integration::inject`
    /// rewrite (planner args lead, injected env layers on last) —
    /// cores cannot depend on the frontend for it (§9). A `None` plan
    /// or a failed materialize spawns the shell plain: §5.5's degrade
    /// rule — the tool still works, the session just is not
    /// `Integrated`.
    fn spawn(&mut self, cwd: &Path) -> Result<(), String> {
        let mut options = LiveSessionOptions {
            program: self.program.clone(),
            cwd: Some(cwd.to_path_buf()),
            ..Default::default()
        };
        self.injection = ait_shell::plan(Path::new(&options.resolved_program()))
            .and_then(|injection| injection.materialize().ok())
            .map(|mut materialized| {
                materialized.set_session_id(agent_session_id());
                let mut args = materialized.args().to_vec();
                args.append(&mut options.args);
                options.args = args;
                options.env.extend(materialized.env().iter().cloned());
                materialized
            });
        self.session = Some(
            LiveSession::spawn(options)
                .map_err(|error| format!("failed to spawn the interactive session: {error}"))?,
        );
        self.reported_row = 0;
        self.reported_tail.clear();
        Ok(())
    }

    /// Pump the session until drained output has been quiet for
    /// [`QUIET_WINDOW`] — bounded by [`SETTLE_BOUND`], an exit, or the
    /// cancellation token. An `OSC 133;A` prompt mark seen during this
    /// call tightens the quiet requirement to [`PROMPT_GRACE`]: the
    /// mark means the command our input ran already finished, so only
    /// the prompt burst is still settling. Returns the exit status
    /// when the child died under this call.
    fn settle(&mut self, cancel: &CancellationToken) -> Option<ExitStatus> {
        let session = self.session.as_mut().expect("settle on a live session");
        let deadline = Instant::now() + SETTLE_BOUND;
        let mut quiet_since = Instant::now();
        let mut exited = None;
        let mut at_prompt = false;
        loop {
            let mut produced = false;
            for event in session.drain_events() {
                match event {
                    LiveEvent::OutputReady => produced = true,
                    LiveEvent::Osc(SessionEvent::PromptMark {
                        kind: PromptMarkKind::PromptStart,
                        ..
                    }) => at_prompt = true,
                    LiveEvent::Exited(status) => exited = Some(status),
                    _ => {}
                }
            }
            if exited.is_some() || cancel.is_cancelled() {
                break;
            }
            let now = Instant::now();
            if produced {
                quiet_since = now;
            }
            let quiet_for = now.duration_since(quiet_since);
            let settled = if at_prompt {
                quiet_for >= PROMPT_GRACE
            } else {
                quiet_for >= QUIET_WINDOW
            };
            if settled || now >= deadline {
                break;
            }
            std::thread::sleep(SETTLE_POLL);
        }
        exited
    }

    /// The grid text produced since the previous call: every absolute
    /// row from [`reported_row`](Self::reported_row) through the
    /// cursor's row, with the first row diffed against its
    /// last-reported text so a growing prompt line reports only its
    /// continuation. Rows above the watermark are the model's context
    /// already — the same call never reports them twice.
    fn collect_output(&mut self) -> String {
        let Some(session) = &self.session else {
            return String::new();
        };
        let grid = session.grid();
        let scrollback = grid.scrollback();
        let cursor_line = session.cursor().line.min(grid.lines().saturating_sub(1));
        let end = scrollback + cursor_line;
        if end < self.reported_row {
            // The terminal moved backwards (an alt-screen redraw):
            // re-anchor instead of re-reporting or looping.
            self.reported_row = end;
            self.reported_tail = row_text(&grid, scrollback, end);
        }
        let mut out = String::new();
        for abs in self.reported_row..=end {
            let text = row_text(&grid, scrollback, abs);
            if abs == self.reported_row {
                match text.strip_prefix(self.reported_tail.as_str()) {
                    Some(added) if !added.trim().is_empty() => {
                        out.push_str(added);
                        out.push('\n');
                    }
                    Some(_) => {}
                    // The row was redrawn in place — its current text
                    // is the new state, report it whole.
                    None => {
                        out.push_str(&text);
                        out.push('\n');
                    }
                }
            } else {
                out.push_str(&text);
                out.push('\n');
            }
        }
        self.reported_row = end;
        self.reported_tail = row_text(&grid, scrollback, end);
        out
    }
}

/// A row's text by absolute index (`scrollback` + viewport space —
/// `abs - scrollback` maps to [`GridView::line`]'s signed index),
/// trailing blanks trimmed. Out-of-range rows read empty.
fn row_text(grid: &GridView, scrollback: usize, abs: usize) -> String {
    grid.line(abs as i64 as i32 - scrollback as i32)
        .map(|row| row.text().trim_end().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    //! The agent PTY against a real `LiveSession` (Spec R06 testing
    //! decisions — a spawned shell needs no host TTY, the
    //! `ait-term/tests/live.rs` precedent). Scenarios pin `/bin/sh`:
    //! `read`/`echo`/`exit` are POSIX builtins on every CI box, and a
    //! non-injected spawn still exercises the whole write→settle→read
    //! path (injection is asserted separately where bash exists).

    use super::*;

    /// Bound on test waits well above the settle cap — a hang is a
    /// failure, not a slow pass.
    const BOUND: Duration = Duration::from_secs(10);

    /// A pty pinned to `/bin/sh`.
    fn pty() -> AgentPty {
        AgentPty::with_program("/bin/sh")
    }

    /// One call against `pty` in a tempdir cwd, never cancelled.
    fn write(pty: &mut AgentPty, input: &str, dir: &Path) -> ToolOutput {
        pty.write(
            &serde_json::json!({"input": input}),
            dir,
            &CancellationToken::new(),
        )
    }

    /// Spec R06 / #90: no PTY exists before the first call — the
    /// first call spawns it and later calls reuse the same one.
    #[test]
    fn spawn_is_lazy_until_the_first_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pty = pty();
        assert!(pty.session.is_none(), "no PTY before the first call");

        let out = write(&mut pty, "echo FIRST-SPAWN\n", dir.path());
        assert!(!out.is_error, "{:?}", out.text);
        let pid = pty.session.as_ref().expect("spawned").child_pid();
        assert!(pid.is_some());
        assert!(out.text.contains("FIRST-SPAWN"), "{:?}", out.text);

        // A second call reuses the same child — no respawn.
        let out = write(&mut pty, "echo SAME-CHILD\n", dir.path());
        assert_eq!(pty.session.as_ref().expect("alive").child_pid(), pid);
        assert!(out.text.contains("SAME-CHILD"), "{:?}", out.text);
    }

    /// Spec R06 / #90 (us-17/18): a program parked mid-command on
    /// `read` answers the next call's input — the call↔read loop the
    /// tool exists for.
    #[test]
    fn written_input_round_trips_through_a_repl() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pty = pty();

        // `read` produces no output of its own — the first call
        // settles quiet while the shell blocks on input.
        let out = write(&mut pty, "read v; echo READ-$v\n", dir.path());
        assert!(!out.is_error, "{:?}", out.text);

        let out = write(&mut pty, "hello\n", dir.path());
        assert!(!out.is_error, "{:?}", out.text);
        // `READ-hello` is the shell's answer, not the echoed input —
        // proof the bytes reached a live interactive session and its
        // response came back.
        assert!(out.text.contains("READ-hello"), "{:?}", out.text);
    }

    /// Spec R06 / #90 (us-18): each call returns output produced
    /// *since the previous call* — earlier rows never repeat.
    #[test]
    fn output_since_previous_call_is_a_delta() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pty = pty();

        let first = write(&mut pty, "echo FIRST-MARK\n", dir.path());
        assert!(first.text.contains("FIRST-MARK"), "{:?}", first.text);

        let second = write(&mut pty, "echo SECOND-MARK\n", dir.path());
        assert!(second.text.contains("SECOND-MARK"), "{:?}", second.text);
        assert!(
            !second.text.contains("FIRST-MARK"),
            "the first call's output must not repeat: {:?}",
            second.text
        );
    }

    /// Spec R06 / #90: the settle is bounded — a program that never
    /// goes quiet still answers inside the cap, with the flood capped
    /// to [`OUTPUT_CAP`] plus the truncation note.
    #[test]
    fn flood_still_answers_within_the_settle_bound() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pty = pty();
        let start = Instant::now();
        let out = write(&mut pty, "while :; do echo flood; done\n", dir.path());
        let elapsed = start.elapsed();
        // SIGINT the loop so the rest of the test runs a calm shell.
        let _ = write(&mut pty, "\x03", dir.path());

        assert!(
            elapsed < BOUND,
            "the settle cap must bound a flood: {elapsed:?}"
        );
        assert!(
            elapsed >= SETTLE_BOUND || out.text.contains("truncated"),
            "a flood only returns early via the cap: {elapsed:?}"
        );
        assert!(!out.is_error, "{:?}", out.text);
        assert!(
            out.text.len() <= OUTPUT_CAP + 128,
            "the returned tail stays bounded: {} bytes",
            out.text.len()
        );
    }

    /// An `exit` ends the session: the call reports it, the dead PTY
    /// is dropped, and the next call lazily spawns a fresh one —
    /// `exit` never bricks the tool.
    #[test]
    fn exit_reports_and_the_next_call_respawns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pty = pty();
        write(&mut pty, "echo WARMUP\n", dir.path());

        let out = write(&mut pty, "exit\n", dir.path());
        assert!(
            out.text.contains("session exited"),
            "the exit note rides the result: {:?}",
            out.text
        );
        assert!(pty.session.is_none(), "the dead session is dropped");

        let out = write(&mut pty, "echo BACK\n", dir.path());
        assert!(!out.is_error, "{:?}", out.text);
        assert!(out.text.contains("BACK"), "{:?}", out.text);
    }

    /// Spec R06 / #90: dropping the `AgentPty` kills the spawned
    /// child — no orphaned process survives a session end or tab
    /// close (`LiveSession`'s bounded SIGHUP→SIGKILL teardown).
    #[test]
    #[cfg(target_os = "linux")]
    fn drop_kills_the_child_no_orphan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pty = pty();
        write(&mut pty, "echo UP\n", dir.path());
        let pid = pty
            .session
            .as_ref()
            .expect("spawned")
            .child_pid()
            .expect("a pid");
        drop(pty);

        // The waiter's kill+reap makes `/proc/<pid>` vanish — join
        // inside Drop means it is normally already gone.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Path::new(&format!("/proc/{pid}")).exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "the agent PTY's child must not outlive it"
        );
    }

    /// A supported shell spawns through the injection planner (the
    /// `Materialized` guard + `0700` dir exist beside the session);
    /// `/bin/sh` plans to `None` and spawns plain — the degrade path
    /// the tool keeps working under.
    #[test]
    fn spawn_plans_injection_for_supported_shells_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut plain = pty();
        write(&mut plain, "echo PLAIN\n", dir.path());
        assert!(plain.injection.is_none(), "sh plans no injection");

        if Path::new("/bin/bash").exists() {
            let mut bash = AgentPty::with_program("/bin/bash");
            write(&mut bash, "echo INTEGRATED\n", dir.path());
            let injection = bash.injection.as_ref().expect("bash plans an injection");
            assert!(injection.dir().join("snippet.bash").exists());
        }
    }

    /// The injected shell's `OSC 133` marks give the settle its
    /// deterministic path: a command that answers in a burst resolves
    /// on the prompt mark + [`PROMPT_GRACE`], well under the blind
    /// quiet window — and the `read`-parked variant still waits for
    /// the mark rather than straddling a gap.
    #[test]
    fn prompt_marks_settle_an_injected_shell() {
        if !Path::new("/bin/bash").exists() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let mut bash = AgentPty::with_program("/bin/bash");

        let start = Instant::now();
        let out = write(&mut bash, "echo MARK-FAST\n", dir.path());
        assert!(
            out.text.contains("MARK-FAST"),
            "the burst is captured: {:?}",
            out.text
        );
        assert!(
            start.elapsed() < QUIET_WINDOW + SETTLE_POLL * 8,
            "prompt marks should settle far inside the quiet window: {:?}",
            start.elapsed()
        );

        // A `read`-parked line completes when the next call's input
        // lands — D+A marks make the wait deterministic under load.
        write(&mut bash, "read v; echo READ-$v\n", dir.path());
        let out = write(&mut bash, "MARKED\n", dir.path());
        assert!(out.text.contains("READ-MARKED"), "{:?}", out.text);
    }

    /// Malformed arguments are tool errors — before any spawn.
    #[test]
    fn missing_input_errors_without_spawning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pty = pty();
        let out = pty.write(
            &serde_json::json!({"wrong": "key"}),
            dir.path(),
            &CancellationToken::new(),
        );
        assert!(out.is_error, "{:?}", out.text);
        assert!(out.text.contains("input"), "{:?}", out.text);
        assert!(pty.session.is_none(), "a bad call never spawns");
    }
}
