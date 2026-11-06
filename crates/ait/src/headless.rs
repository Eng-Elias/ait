//! Headless mode (REQUIREMENTS.md §5.10.5, Spec R11 / issue #140):
//! `ait --headless "<task>" [--profile <name>]` runs one agent task
//! with no TUI — the [`HiddenSession`] half (R11-T4, user stories
//! 18/19/30) supplies the shell, the [`run`] driver half (R11-T5,
//! user stories 17/20–29) supplies the single-task event loop.
//!
//! The hidden shell is a [`LiveSession`] spawned off-screen through
//! the same integration materialization a visible tab gets — rc
//! snippets land, a vetted `init_shell`/`bootstrapped` proves
//! `Integrated` — with a dedicated **drain thread** standing in for
//! the app tick: `drain_events` → [`Tab::observe_event`] →
//! [`Tab::service_bridge`] on a fixed cadence.
//!
//! The session IS a real [`Tab`]: the bookkeeping [`Tab::observe_event`]
//! folds — integration state, the `__ait_run` accumulator, the remote
//! context stack, block assembly, `dropped_lines` — is literally the
//! tab's, so the bridge's absolute-line answers and integration
//! semantics cannot drift from the TUI's (standard 17). Nothing
//! renders: no TUI, no painter, no input path touches it.
//!
//! The [`ShellBridge`] consumers get is the same channel-proxy shape
//! `TabBridge` uses in the TUI — `shell_bridge::pair()` — answered on
//! the drain thread instead of a `TabManager` tick. `run_in_band`
//! resolves through the hidden session's in-band accumulator,
//! `send_bytes` writes its PTY, `grid_lines`/`output_tail` snapshot its
//! capture surfaces, `remote_state` reads its `ssh()` context, and
//! `subscribe` forwards its decoded events. Every failure mode is a
//! [`BridgeError`], never a wedge: a dropped drain disconnects the
//! service channel and `ask` reports `Gone`.
//!
//! The driver loads [`Settings`] itself (the app boot's own
//! `ait_config::load`), scans both skills roots and `mcp.json` like a
//! TUI task submit, submits ONE [`AgentRequest`] bound to the hidden
//! bridge and folds every drained [`AgentEvent`] through
//! [`effect_of`]: assistant text to stdout, tool/status activity to
//! stderr, `ApprovalRequest` the auto-deny, `AskRequest` the canned
//! no-interactive answer, the terminal events the exit codes. SIGINT
//! calls [`AgentSession::cancel`] — Esc's semantics — and the
//! transcript folds through the same conversation machinery the TUI's
//! `drain_transcripts` uses, so a headless task is a normal resumable
//! conversation row.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
#[cfg(unix)]
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use ait_agent::{
    AgentContext, AgentEvent, AgentRequest, AgentSession, ApprovalVerdict, BridgeError,
    SkillsCatalog, ToolStatus,
};
use ait_config::{ModelRole, Settings};
use ait_term::{LiveSessionOptions, SpawnError};

use crate::config::NoSecrets;
use crate::remote::SshSpawn;
use crate::shell_bridge::{self, TabBridge};
use crate::status::Toast;
use crate::store::StoreHandle;
use crate::tabs::{title_for, SessionTemplate, Tab};
use crate::transcript::Transcript;

/// How often the drain thread pumps the session — the same order as
/// the app's ~8 ms input tick that bounds `TabBridge` latency in the
/// TUI (tighter is wasted cycles; the hidden session has no render
/// pass to wait on).
const DRAIN_POLL: Duration = Duration::from_millis(5);

/// `AIT_SESSION_ID`s for hidden sessions, allocated downward from
/// `u64::MAX` so they can never collide with `TabManager`'s upward tab
/// ids — the ctl surface (Spec R11) addresses tabs by this id, and a
/// hidden session must never shadow a visible one.
static NEXT_HIDDEN_ID: AtomicU64 = AtomicU64::new(u64::MAX);

/// A hidden, fully integrated shell an `AgentSession` can drive without
/// the TUI (Spec R11 §5.10.5): owns the spawned `Tab` on its drain
/// thread; hands out [`TabBridge`] clones (`ShellBridge` impls) that
/// reach the thread's service queue. `Drop` stops the drain — the
/// `Tab`'s own drop then kills the child, joins the session's
/// reader/waiter threads and frees the snippet dir (us-30).
pub(crate) struct HiddenSession {
    /// The `ShellBridge` handle every consumer clones —
    /// `AgentRequest::bridge`'s `Box<dyn ShellBridge>` source.
    bridge: TabBridge,
    /// The drain thread's stop flag — `Drop` raises it, then joins.
    stop: Arc<AtomicBool>,
    /// The dedicated drain thread — the hidden session's app tick.
    drain: Option<JoinHandle<()>>,
    /// The `AIT_SESSION_ID` allocated at spawn — hook payloads echo it
    /// back, and the `ssh()` wrapper's hop ids prefix it.
    session_id: String,
}

impl HiddenSession {
    /// Spawn `options` as a hidden, integration-materialized session
    /// and start its drain thread. `options.session.size` is the grid
    /// size as given (no window shapes it); `options.cwd` unset means
    /// the process's own cwd. `ssh` is the armed `[ssh]` policy — the
    /// same `AIT_SSH_*` env a tab spawn renders (Spec R09-T2). A
    /// `None` injection plan or materialize failure still spawns the
    /// session — the bridge then reports `NotIntegrated`, the
    /// standard-1 degrade.
    pub(crate) fn spawn(options: LiveSessionOptions, ssh: &SshSpawn) -> Result<Self, SpawnError> {
        let cwd = options
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let id = NEXT_HIDDEN_ID.fetch_sub(1, Ordering::Relaxed);
        let session_id = id.to_string();
        let template = SessionTemplate {
            shell_integration: true,
            ssh: ssh.clone(),
            ..SessionTemplate::default()
        };
        let mut tab = Tab::spawn(options, cwd, id, &template)?;
        let (bridge, service) = shell_bridge::pair();
        tab.bridge = Some(service);
        let stop = Arc::new(AtomicBool::new(false));
        let drain = std::thread::Builder::new()
            .name("ait-headless-drain".to_string())
            .spawn({
                let stop = Arc::clone(&stop);
                move || drain_loop(tab, stop)
            })
            .map_err(SpawnError::Thread)?;
        Ok(Self {
            bridge,
            stop,
            drain: Some(drain),
            session_id,
        })
    }

    /// A `ShellBridge` handle for this hidden session — cloneable and
    /// `Send`, serviced by the drain thread; `Box<dyn ShellBridge>` it
    /// into `AgentRequest::bridge` and `run_command`, `write_to_pty`
    /// (via `send_bytes`), `grid_lines`/`output_tail` and
    /// `remote_state` all answer against the hidden session.
    pub(crate) fn bridge(&self) -> TabBridge {
        self.bridge.clone()
    }

    /// The `AIT_SESSION_ID` this session's spawn allocated — the
    /// identity its hooks echo (standard 14).
    #[allow(dead_code)] // asserted by the tests; the driver needs no id.
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The §5.6 `AgentContext` of the hidden session — cwd, shell, OS
    /// and the same `output_lines` `context_for` assembles for a
    /// visible tab (Spec R11 us-30: the model sees its own commands'
    /// results the way it does in a tab). Answered on the drain like
    /// every bridge request; `Gone` once the session is dropped.
    pub(crate) fn context(&self) -> Result<AgentContext, BridgeError> {
        self.bridge.context()
    }
}

impl Drop for HiddenSession {
    /// Stop the drain thread and join it — bounded: the loop wakes at
    /// most `DRAIN_POLL` late, then the `Tab` it owns drops (kills the
    /// child, frees the snippet dir). Bridge clones outliving `self`
    /// report `Gone` on their next call — never a wedge.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(drain) = self.drain.take() {
            let _ = drain.join();
        }
    }
}

/// The drain thread's body — the hidden session's entire event loop:
/// feed PTY output through the tab's bookkeeping, drain the sealed
/// block queue so it can't grow unpersisted (`store_session` is always
/// `None` here — the same rule `TabManager::take_persisted_blocks`
/// documents), then answer every queued `ShellBridge` request. Runs
/// until `stop` (the owning `HiddenSession`'s drop). A session exit
/// keeps the loop alive — the bridge still answers
/// `integration_state`/`grid_lines`/`output_tail`/`remote_state` and
/// refuses `run_in_band`/`send_bytes` with `SessionExited`, exactly as
/// `drain_events` does on a dead tab.
fn drain_loop(mut tab: Tab, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        for event in tab.session.drain_events() {
            tab.observe_event(&event);
        }
        tab.take_persisted_blocks();
        tab.service_bridge();
        std::thread::sleep(DRAIN_POLL);
    }
}

/// The denial text every gated call gets — a headless run has nobody
/// to ask, so an `ApprovalRequest` never parks (us-20): the text
/// becomes the call's tool result, telling the model to pick an
/// approach it is allowed.
const DENY_TEXT: &str = "denied: `ait --headless` is non-interactive";

/// The canned `ask_user_question` answer — the model learns nobody is
/// home instead of parking on a card forever (us-21); it becomes the
/// call's tool result verbatim.
const ASK_ANSWER: &str = "no answer: `ait --headless` is non-interactive";

/// How often the event loop wakes to check the SIGINT flag — the
/// bound on Ctrl+C latency while the worker emits no events.
const SIGNAL_POLL: Duration = Duration::from_millis(50);

/// The exit-code vocabulary (us-26): `Done` is 0; `Error` reports 1;
/// the budget and the SIGINT cancel get their own non-zero codes —
/// 2 and the SIGINT-conventional 130 — so scripts can branch on the
/// outcome.
const EXIT_ERROR: u8 = 1;
const EXIT_BUDGET: u8 = 2;
const EXIT_CANCELLED: u8 = 130;

/// The reply a parked gate is owed — headless answers every ask
/// itself (us-20/21).
#[derive(Debug, PartialEq, Eq)]
enum Reply {
    /// Auto-deny the `ApprovalRequest` — the `call.id` to
    /// [`AgentSession::approve`] with [`DENY_TEXT`].
    Deny(String),
    /// Answer the `AskRequest` — the `call.id` to
    /// [`AgentSession::answer`] with [`ASK_ANSWER`].
    Answer(String),
}

/// One drained [`AgentEvent`]'s effect on the headless process — the
/// thin pure seam the table tests pin (Spec R11 §5.10.5 testing
/// decisions): what lands on stdout, what lands on stderr, what the
/// worker is owed back, and whether the run ends.
#[derive(Debug, Default, PartialEq, Eq)]
struct Effect {
    /// Assistant text appended to stdout verbatim — the pipe-safe
    /// answer stream (us-24); never a status byte.
    stdout: String,
    /// One activity line for stderr — tool calls, updates, gate
    /// decisions and terminal statuses (us-25). The writer adds the
    /// newline.
    stderr: Option<String>,
    /// The gate reply the event parks on — the auto-deny or the
    /// canned ask answer.
    reply: Option<Reply>,
    /// The process exit code a terminal event maps to.
    exit: Option<u8>,
}

/// A `ToolStatus` as lowercase log text — stderr's activity verbs.
fn status_text(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Running => "running",
        ToolStatus::Done => "done",
        ToolStatus::Failed => "failed",
        ToolStatus::Denied => "denied",
    }
}

/// The §5.10.5 event→stream map: `TextDelta` is the answer stream
/// (stdout); every tool/status signal is the inspectable activity log
/// (stderr); gates get the non-interactive replies; `Done` exits 0
/// and the other terminals map onto the non-zero vocabulary.
fn effect_of(event: &AgentEvent) -> Effect {
    match event {
        AgentEvent::TextDelta(text) => Effect {
            stdout: text.clone(),
            ..Effect::default()
        },
        AgentEvent::ToolCall { call, summary } => Effect {
            stderr: Some(format!("tool {}: {}", call.name, summary)),
            ..Effect::default()
        },
        AgentEvent::ToolUpdate {
            call_id,
            status,
            detail,
            ..
        } => Effect {
            stderr: Some(match detail {
                Some(detail) => format!("tool {call_id} {}: {detail}", status_text(*status)),
                None => format!("tool {call_id} {}", status_text(*status)),
            }),
            ..Effect::default()
        },
        AgentEvent::ApprovalRequest { call, summary } => Effect {
            stderr: Some(format!(
                "denying {} non-interactively: {}",
                call.name, summary
            )),
            reply: Some(Reply::Deny(call.id.clone())),
            ..Effect::default()
        },
        AgentEvent::AskRequest { call, question, .. } => Effect {
            stderr: Some(format!("answering non-interactively: {question}")),
            reply: Some(Reply::Answer(call.id.clone())),
            ..Effect::default()
        },
        AgentEvent::PlanUpdated { todos } => Effect {
            stderr: Some(format!("plan updated ({} items)", todos.len())),
            ..Effect::default()
        },
        AgentEvent::Summarized { .. } => Effect {
            stderr: Some("conversation summarized".to_owned()),
            ..Effect::default()
        },
        AgentEvent::Done => Effect {
            exit: Some(0),
            ..Effect::default()
        },
        AgentEvent::Error(message) => Effect {
            stderr: Some(format!("error: {message}")),
            exit: Some(EXIT_ERROR),
            ..Effect::default()
        },
        AgentEvent::BudgetReached => Effect {
            stderr: Some("step budget reached".to_owned()),
            exit: Some(EXIT_BUDGET),
            ..Effect::default()
        },
        AgentEvent::Cancelled => Effect {
            stderr: Some("cancelled".to_owned()),
            exit: Some(EXIT_CANCELLED),
            ..Effect::default()
        },
    }
}

/// The current run's SIGINT flag, published to the handler as a raw
/// pointer — the handler does two atomic ops only (a load, then a
/// store through the pointer), the only discipline that is
/// async-signal-safe. A null slot means no run is armed: the handler
/// is a process-wide install, so the slot — not the flag — is the
/// arming.
#[cfg(unix)]
static SIGINT_FLAG: AtomicPtr<AtomicBool> = AtomicPtr::new(std::ptr::null_mut());

/// The SIGINT handler — Ctrl+C on a non-raw-mode process. Stores into
/// the armed run's flag; the event loop polls it and calls
/// [`AgentSession::cancel`], giving SIGINT the TUI's Esc semantics
/// (us-27).
#[cfg(unix)]
extern "C" fn catch_sigint(_signal: libc::c_int) {
    let flag = SIGINT_FLAG.load(Ordering::SeqCst);
    // `flag` points into the `Arc<AtomicBool>` the run holds alive for
    // its whole armed span — [`SigintArm`]'s Drop clears the slot
    // before the `Arc` can release the flag, so this can never store
    // into freed memory.
    if let Some(flag) = unsafe { flag.as_ref() } {
        flag.store(true, Ordering::SeqCst);
    }
}

/// One armed SIGINT flag — a run holds it for its task's span. Drop
/// disarms the slot BEFORE the flag can die, so the handler can never
/// store into a freed flag.
struct SigintArm {
    flag: Arc<AtomicBool>,
}

#[cfg(unix)]
fn arm_sigint() -> SigintArm {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Installed once per process — `signal`'s plain handler model
        // is all a flag store needs.
        unsafe {
            libc::signal(
                libc::SIGINT,
                catch_sigint as extern "C" fn(libc::c_int) as libc::sighandler_t,
            )
        };
    });
    let arm = SigintArm {
        flag: Arc::new(AtomicBool::new(false)),
    };
    SIGINT_FLAG.store(Arc::as_ptr(&arm.flag) as *mut AtomicBool, Ordering::SeqCst);
    arm
}

/// Non-unix builds have no SIGINT layer here — the flag stays clear
/// and Ctrl+C keeps its default kill (still a non-zero exit).
#[cfg(not(unix))]
fn arm_sigint() -> SigintArm {
    SigintArm {
        flag: Arc::new(AtomicBool::new(false)),
    }
}

impl Drop for SigintArm {
    fn drop(&mut self) {
        #[cfg(unix)]
        SIGINT_FLAG.store(std::ptr::null_mut(), Ordering::SeqCst);
    }
}

/// The startup contract (us-29): `profile` — the `--profile` value or
/// the TUI's launch default — resolves to the name the request arms,
/// or the stderr text explaining why the run can't start. An
/// EXPLICIT name must exist: a `--profile` typo fails rather than
/// silently running the worker's ask-everything fail-safe.
/// `default` against an empty profiles table is the TUI's own launch
/// stance — the sole configured model resolves implicitly (§4.2
/// us-23) — so it is the one undeclared name that still arms. Pure:
/// the caller feeds the loaded [`Settings`]; `NoSecrets` suffices for
/// resolution, which needs the binding to exist, not the secret.
fn resolve_start(settings: &Settings, profile: Option<&str>) -> Result<String, String> {
    let name = profile.unwrap_or(crate::profile::DEFAULT_PROFILE);
    let declared = &settings.agents.execution_profiles;
    if !declared.contains_key(name)
        && !(declared.is_empty() && name == crate::profile::DEFAULT_PROFILE)
    {
        return Err(format!("unknown execution profile {name:?}"));
    }
    match settings.resolve_model(name, ModelRole::Coding, &NoSecrets) {
        Ok(Some(_)) => Ok(name.to_owned()),
        Ok(None) => Err(format!(
            "no model configured for profile {name:?} — bind `coding_model` \
             or `base_model` in settings.toml"
        )),
        Err(error) => Err(error.to_string()),
    }
}

/// `ait --headless "<task>" [--profile <name>]` — the CLI arm: write
/// every startup failure to stderr and every answer byte to stdout,
/// exit by the task's terminal event (us-17/24/25/26/29).
pub(crate) fn run(task: &str, profile: Option<&str>) -> ExitCode {
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    ExitCode::from(run_inner(task, profile, &mut out, &mut err))
}

/// The launch sequence — each step is the TUI's own seam (us-23):
/// `ait_config::load` like the app boot, the skills roots and
/// `mcp.json` scans like a task submit, `config::session_template`'s
/// options like a tab spawn. Every failure writes only stderr —
/// stdout is always pipe-safe.
fn run_inner(task: &str, profile: Option<&str>, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    // Settings — malformed is a startup failure (us-29), never a
    // silent default.
    let settings = match ait_config::load() {
        Ok(settings) => settings,
        Err(error) => {
            let _ = writeln!(err, "ait: {error}");
            return EXIT_ERROR;
        }
    };
    // The armed profile plus a resolvable coding model — checked
    // before anything spawns so an unknown `--profile` and "no
    // endpoint" are startup failures, not mid-run task errors.
    let profile = match resolve_start(&settings, profile) {
        Ok(profile) => profile,
        Err(message) => {
            let _ = writeln!(err, "ait: {message}");
            return EXIT_ERROR;
        }
    };
    // The task's cwd — the hidden session inherits it (its options
    // keep `cwd` unset) and the file tools / `mcp.json` reconcile
    // bind to it.
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            let _ = writeln!(err, "ait: cannot determine the working directory: {error}");
            return EXIT_ERROR;
        }
    };
    // The per-task scans a TUI submit performs: the skills catalog
    // snapshots both roots once (no watcher — a one-shot process) and
    // `mcp.json`'s load errors surface on stderr where the TUI
    // toasts, keeping last-good MCP servers either way (ADR-0006).
    let catalog = SkillsCatalog::scan_roots(
        &ait_config::user_skills_dir(),
        ait_config::project_skills_dir(&cwd).as_deref(),
    );
    if !catalog.errors().is_empty() {
        let _ = writeln!(
            err,
            "ait: {}",
            crate::skills::skipped_text(catalog.errors())
        );
    }
    if let Err(error) = ait_config::load_mcp(&cwd) {
        let _ = writeln!(err, "ait: {}", crate::config::mcp_error_text(&error));
    }
    // The hidden shell — the same options a visible tab would spawn
    // with (`[terminal]` program/scrollback, `[ssh]` env), fully
    // integration-materialized (us-18/19/30). The shell name is read
    // before `options` moves into the spawn — the degraded context's
    // fallback.
    let template = crate::config::session_template(&settings);
    let shell = title_for(&template.options);
    let hidden = match HiddenSession::spawn(template.options, &template.ssh) {
        Ok(hidden) => hidden,
        Err(error) => {
            let _ = writeln!(err, "ait: the hidden shell failed to spawn: {error}");
            return EXIT_ERROR;
        }
    };
    // The §5.6 context the task's system prompt assembles — the
    // hidden session's own snapshot; a drain that died before
    // answering degrades to a cwd+shell context rather than failing
    // the run (its bridge errors reach the model as tool errors).
    let context = hidden.context().unwrap_or_else(|_| AgentContext {
        cwd: cwd.to_string_lossy().into_owned(),
        shell,
        os: std::env::consts::OS.to_owned(),
        output_lines: Vec::new(),
    });
    let session = AgentSession::new();
    let request = AgentRequest {
        settings,
        profile,
        task: task.to_owned(),
        prior: Vec::new(),
        skills: catalog,
        context,
        bridge: Some(Box::new(hidden.bridge())),
    };
    // The store opens like the app's: an open failure degrades to an
    // unpersisted run with one stderr line — persistence-when-
    // possible, never a hard dependency (us-28).
    let mut store = match StoreHandle::open() {
        Ok(store) => store,
        Err(error) => {
            let _ = writeln!(
                err,
                "ait: store unavailable — the run won't persist: {error}"
            );
            StoreHandle::disabled()
        }
    };
    let arm = arm_sigint();
    let code = drive(&session, request, &mut store, &cwd, out, err, &arm.flag);
    drop(arm);
    // `hidden` drops last of the run state — kills the child shell,
    // joins the drain thread and frees the snippet dir (us-30's
    // guard semantics).
    drop(hidden);
    code
}

/// The single-task event loop: submit `request`, then fold each
/// drained [`AgentEvent`] through [`effect_of`] — stdout gets the
/// text stream, stderr the activity log, the parked gates their
/// non-interactive replies, and the store the same conversation rows
/// a tab's `drain_transcripts` writes (us-28 — a headless task is a
/// normal resumable conversation). `cancel` is the SIGINT flag,
/// polled every [`SIGNAL_POLL`]: set, it calls
/// [`AgentSession::cancel`] once — Esc's semantics (us-27). Returns
/// the terminal event's exit code.
fn drive(
    session: &AgentSession,
    request: AgentRequest,
    store: &mut StoreHandle,
    cwd: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
    cancel: &AtomicBool,
) -> u8 {
    // The send-side record lands before the worker's first event —
    // the same send order the panel's submit uses — so the
    // conversation row's title reads the task item.
    let mut transcript = Transcript::default();
    transcript.task(&request.task);
    if session.submit(request).is_err() {
        let _ = writeln!(err, "ait: agent session is unavailable");
        return EXIT_ERROR;
    }
    let mut conversation = None;
    let mut cancelled = false;
    loop {
        if !cancelled && cancel.load(Ordering::SeqCst) {
            session.cancel();
            cancelled = true;
        }
        let event = match session.events().recv_timeout(SIGNAL_POLL) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                // A dead worker emitted no terminal event — the run
                // ends as an error, not a hang.
                let _ = writeln!(err, "ait: agent session ended unexpectedly");
                flush_conversation(store, &mut conversation, &mut transcript, cwd, err);
                close_conversation(store, conversation, err);
                return EXIT_ERROR;
            }
        };
        transcript.apply(&event);
        let effect = effect_of(&event);
        if let Some(line) = &effect.stderr {
            let _ = writeln!(err, "ait: {line}");
        }
        if !effect.stdout.is_empty() {
            let _ = out.write_all(effect.stdout.as_bytes());
            let _ = out.flush();
        }
        match effect.reply {
            Some(Reply::Deny(call_id)) => {
                let _ = session.approve(&call_id, ApprovalVerdict::Deny(DENY_TEXT.to_owned()));
            }
            Some(Reply::Answer(call_id)) => {
                let _ = session.answer(&call_id, ASK_ANSWER.to_owned());
                // The send-side answer records like the panel's
                // `submit_answer` — the transcript's ask+answer pair
                // instead of an interrupted ask.
                transcript.answer(ASK_ANSWER);
            }
            None => {}
        }
        flush_conversation(store, &mut conversation, &mut transcript, cwd, err);
        if let Some(code) = effect.exit {
            close_conversation(store, conversation, err);
            return code;
        }
    }
}

/// Append the transcript's new items to the store — the headless
/// `drain_transcripts` (Spec R08 us-28). The fold itself is the
/// shared [`StoreHandle::flush_transcript`] the tab drain runs;
/// here a store-write toast lands on stderr where the TUI would
/// toast its status row.
fn flush_conversation(
    store: &mut StoreHandle,
    conversation: &mut Option<i64>,
    transcript: &mut Transcript,
    cwd: &Path,
    err: &mut dyn Write,
) {
    let items = transcript.take_new();
    let mut toast = None;
    *conversation =
        store.flush_transcript(&mut toast, *conversation, &items, transcript.items(), cwd);
    render_toast(&mut toast, err);
}

/// `status` → `"closed"` — the headless process's exit is the owning
/// tab's close in Spec R08's status vocabulary; a closed row still
/// lists and resumes.
fn close_conversation(store: &mut StoreHandle, conversation: Option<i64>, err: &mut dyn Write) {
    if let Some(id) = conversation {
        let mut toast = None;
        store.conversation_closed(&mut toast, id);
        render_toast(&mut toast, err);
    }
}

/// The headless status line — a store-write toast lands on stderr
/// where the TUI would toast its status row.
fn render_toast(toast: &mut Option<Toast>, err: &mut dyn Write) {
    if let Some(toast) = toast.take() {
        let _ = writeln!(err, "ait: {}", toast.text());
    }
}

#[cfg(test)]
mod tests {
    //! The integration seam the ticket pins: a REAL spawned session —
    //! injected `/bin/bash`, real PTY, real `7701` frames — driven only
    //! through the `ShellBridge` surface. No TUI, no renderer (prior
    //! art: `tabs.rs`'s `bridge_*` tests, which stage the same service
    //! against `TabManager`'s drain).
    use super::*;
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::mpsc::{Receiver, RecvTimeoutError};
    use std::time::Instant;

    use ait_agent::{BridgeEvent, RemoteState, ShellBridge, ShellIntegration, TranscriptItem};

    /// Generous bound for every wait below (mirrors `tabs.rs`'s live
    /// tests): tiny `/bin/bash` runs finish far under it even on a
    /// loaded CI runner.
    const TIMEOUT: Duration = Duration::from_secs(15);

    /// Options spawning an interactive `/bin/bash` — the injected real
    /// shell the ticket requires.
    fn bash_options() -> LiveSessionOptions {
        LiveSessionOptions {
            program: Some(OsString::from("/bin/bash")),
            ..Default::default()
        }
    }

    /// A hidden bash session, or `None` where bash isn't installed.
    fn spawn_bash() -> Option<HiddenSession> {
        Path::new("/bin/bash")
            .exists()
            .then(|| HiddenSession::spawn(bash_options(), &SshSpawn::default()).unwrap())
    }

    /// Poll `f(bridge)` until it holds — read-only bridge calls only —
    /// or the timeout kills the test. The drain thread does all the
    /// work; the caller never touches the session.
    fn wait_until(bridge: &TabBridge, f: impl Fn(&TabBridge) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !f(bridge) {
            assert!(Instant::now() < deadline, "timed out waiting");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Wait until the hidden shell proves `Integrated` — a vetted
    /// `init_shell`/`bootstrapped` landed through the injected rc.
    fn wait_integrated(bridge: &TabBridge) {
        wait_until(bridge, |b| {
            matches!(b.integration_state(), Ok(ShellIntegration::Integrated))
        });
    }

    /// `OSC 7700 ; <hex(json)>` as a line the shell prints — the same
    /// hand-emit `tabs.rs` tests use.
    fn hook_line(json: &str) -> Vec<u8> {
        let hex: String = json.bytes().map(|b| format!("{b:02x}")).collect();
        format!("printf '\\033]7700;{hex}\\007'\n").into_bytes()
    }

    /// The drain collects `events` until `f` accepts one or the timeout
    /// kills the test.
    fn recv_until(events: &Receiver<BridgeEvent>, f: impl Fn(&BridgeEvent) -> bool) -> BridgeEvent {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match events.recv_timeout(Duration::from_millis(200)) {
                Ok(event) if f(&event) => return event,
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("subscription dropped unexpectedly")
                }
            }
            assert!(Instant::now() < deadline, "timed out waiting");
        }
    }

    /// Acceptance: the spawn reaches `Integrated` (rc snippets landed,
    /// `init_shell`/`bootstrapped` proved it) and `run_in_band` returns
    /// real `RunResult`s through the hidden session's in-band
    /// accumulator — the `__ait_run` trace and `7701` reply included.
    #[test]
    fn hidden_session_integrates_and_runs_in_band() {
        let Some(hidden) = spawn_bash() else { return };
        let bridge = hidden.bridge();
        wait_integrated(&bridge);

        let rx = bridge
            .run_in_band(b"printf 'agent-marker'")
            .expect("an Integrated hidden session accepts the request");
        let result = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the run resolved");
        assert_eq!(result.output, b"agent-marker".to_vec());
        assert_eq!(result.exit_code, 0);

        // A second run resolves by id — requests serialize on the wire.
        let rx = bridge
            .run_in_band(b"echo second")
            .expect("a second request");
        let result = rx.recv_timeout(Duration::from_secs(5)).expect("resolved");
        assert_eq!(result.id, "1");
        assert!(result.output.windows(6).any(|w| w == b"second"));
    }

    /// Acceptance: shell state persists across commands — a `cd` and an
    /// `export` typed through `send_bytes` carry into the next
    /// `run_in_band`, exactly as in a tab (us-18); and the typed input
    /// round-trips: `grid_lines`/`output_tail` reflect its output
    /// (us-19/30).
    #[test]
    fn send_bytes_state_persists_and_surfaces() {
        let Some(hidden) = spawn_bash() else { return };
        let bridge = hidden.bridge();
        wait_integrated(&bridge);

        // Typed input round-trips through the hidden PTY — standard 15:
        // wait on computed output, never the echoed command.
        bridge
            .send_bytes(b"export AIT_KEEP=41; cd /\n")
            .expect("send_bytes on a live session");
        bridge
            .send_bytes(b"echo HX$((100+23))\n")
            .expect("send_bytes");
        wait_until(&bridge, |b| {
            String::from_utf8_lossy(&b.output_tail().unwrap()).contains("HX123")
        });
        let lines = bridge.grid_lines(0, 10_000).expect("grid snapshot");
        assert!(
            lines.iter().any(|line| line.contains("HX123")),
            "the output reached the grid: {lines:?}"
        );

        // The typed `cd`/`export` persisted — `run_in_band` answers
        // against THAT shell's cwd and env.
        let rx = bridge
            .run_in_band(b"printf '%s:%s' \"$PWD\" \"$AIT_KEEP\"")
            .expect("run accepted");
        let result = rx.recv_timeout(Duration::from_secs(5)).expect("resolved");
        assert!(
            result.output.windows(4).any(|w| w == b"/:41"),
            "cd and export carried into the in-band run: {:?}",
            result.output
        );
        assert_eq!(result.exit_code, 0);
    }

    /// The subscription feed and `remote_state` mirror `TabBridge`: a
    /// vetted `ssh_begin` opens the context the bridge reports, the
    /// remote shell's own `init_shell` marks it `bootstrapped`,
    /// `ssh_end` closes it — and `OSC 133` marks reach subscribers with
    /// their absolute line.
    #[test]
    fn subscription_and_remote_state_mirror_tabbridge() {
        let Some(hidden) = spawn_bash() else { return };
        let bridge = hidden.bridge();
        wait_integrated(&bridge);
        let events = bridge.subscribe().expect("subscribe");
        assert!(
            matches!(bridge.remote_state(), Ok(RemoteState::Local)),
            "a fresh hidden session is local"
        );

        // A vetted ssh_begin: the session's allocated id must echo.
        let sid = hidden.session_id().to_owned();
        bridge
            .send_bytes(&hook_line(&format!(
                r#"{{"hook":"ssh_begin","value":{{"session_id":"{sid}","target":"u@prod","hop":"{sid}.1.1","control_path":"/tmp/ait-hl/%C"}}}}"#
            )))
            .unwrap();
        wait_until(&bridge, |b| {
            matches!(b.remote_state(), Ok(RemoteState::Remote { .. }))
        });
        // The remote shell's OWN integration lands through a
        // hop-attributed init_shell.
        bridge
            .send_bytes(&hook_line(&format!(
                r#"{{"hook":"init_shell","value":{{"session_id":"{sid}","hop":"{sid}.1.1"}}}}"#
            )))
            .unwrap();
        wait_until(&bridge, |b| {
            matches!(
                b.remote_state(),
                Ok(RemoteState::Remote {
                    bootstrapped: true,
                    ..
                })
            )
        });
        match bridge.remote_state().unwrap() {
            RemoteState::Remote {
                target,
                control_path,
                ..
            } => {
                assert_eq!(target, "u@prod");
                assert_eq!(control_path.as_deref(), Some("/tmp/ait-hl/%C"));
            }
            state => panic!("expected Remote, got {state:?}"),
        }

        // Marks reach the subscriber with their absolute line — feed
        // `grid_lines` the same coordinate space.
        bridge.send_bytes(b"printf '\\033]133;D;0\\007'\n").unwrap();
        let mark = recv_until(&events, |event| {
            matches!(event, BridgeEvent::PromptMark { kind: 'D', .. })
        });
        let BridgeEvent::PromptMark { line, payload, .. } = mark else {
            unreachable!()
        };
        assert_eq!(payload.as_deref(), Some("0"));
        let rows = bridge
            .grid_lines(line as i32, line as i32 + 1)
            .expect("the mark's row resolves");
        assert_eq!(rows.len(), 1, "the absolute line still resolves");

        // ssh_end closes the context — back to Local.
        bridge
            .send_bytes(&hook_line(&format!(
                r#"{{"hook":"ssh_end","value":{{"session_id":"{sid}","target":"u@prod","hop":"{sid}.1.1"}}}}"#
            )))
            .unwrap();
        wait_until(&bridge, |b| {
            matches!(b.remote_state(), Ok(RemoteState::Local))
        });
    }

    /// `context()` answers the §5.6 `AgentContext` — the same assembly
    /// a visible tab's panel runs (us-30): real cwd, the spawned
    /// program's basename, and the session's own output lines.
    #[test]
    fn context_reports_cwd_shell_and_output() {
        let Some(hidden) = spawn_bash() else { return };
        let bridge = hidden.bridge();
        wait_integrated(&bridge);
        bridge.send_bytes(b"echo CTX$((200+24))\n").unwrap();
        wait_until(&bridge, |b| {
            String::from_utf8_lossy(&b.output_tail().unwrap()).contains("CTX224")
        });

        let context = hidden.context().expect("context answers");
        assert!(!context.cwd.is_empty(), "a real cwd: {context:?}");
        assert_eq!(context.shell, "bash");
        assert_eq!(context.os, std::env::consts::OS);
        assert!(
            context
                .output_lines
                .iter()
                .any(|line| line.contains("CTX224")),
            "the session's own output feeds context: {:?}",
            context.output_lines
        );
    }

    /// Acceptance: a dead or exited session reports through
    /// `BridgeError` exactly as `TabBridge` does — subscribers get
    /// `Exited`, `run_in_band`/`send_bytes` refuse `SessionExited`, and
    /// a dropped `HiddenSession` leaves clones reporting `Gone` — never
    /// a wedge.
    #[test]
    fn exited_and_dropped_sessions_report_bridge_errors() {
        let Some(hidden) = spawn_bash() else { return };
        let bridge = hidden.bridge();
        wait_integrated(&bridge);
        let events = bridge.subscribe().expect("subscribe");

        bridge.send_bytes(b"exit\n").unwrap();
        recv_until(&events, |event| matches!(event, BridgeEvent::Exited));

        // The drain observed the exit — both typed paths refuse.
        wait_until(&bridge, |b| {
            matches!(b.run_in_band(b"echo hi"), Err(BridgeError::SessionExited))
        });
        assert!(
            matches!(bridge.send_bytes(b"x"), Err(BridgeError::SessionExited)),
            "send_bytes on a dead session"
        );

        // Snapshots still answer against the dead session's grid — the
        // drain keeps servicing until the owner drops.
        assert!(bridge.output_tail().is_ok());
        assert!(bridge.integration_state().is_ok());

        drop(hidden);
        assert!(
            matches!(bridge.integration_state(), Err(BridgeError::Gone)),
            "a dropped service reports Gone"
        );
        assert!(matches!(bridge.run_in_band(b"x"), Err(BridgeError::Gone)));
    }

    // ── The driver half (R11-T5) ────────────────────────────────────
    //
    // Two seams, per the spec's testing decisions: `effect_of`'s pure
    // event→stream map gets the table tests; `drive` gets real
    // `AgentSession` runs against the `ait_llm::testing` fixture
    // provider (`test-fixtures` — the same in-process SSE server
    // ait-agent's session tests use; NoSecrets, zero external network).

    use ait_llm::testing::{error_status, sse, Fixture, Script};

    /// A `settings.toml` snapshot binding `default`'s `coding_model`
    /// to the fixture endpoint, with `profile_fields` appended inside
    /// `[agents.execution_profiles.default]` (agent tests' shape).
    fn fixture_settings(base_url: &str, profile_fields: &str) -> Settings {
        Settings::from_toml(&format!(
            r#"
[agents.custom_endpoints.local]
name = "Fixture"
base_url = "{base_url}"
schema = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name = "test-model"
alias = "Test Model"
config_key = "coding-key"

[agents.execution_profiles.default]
coding_model = "coding-key"
{profile_fields}
"#
        ))
        .expect("test settings must parse")
    }

    /// A JSON string literal for hand-built chunk bodies.
    fn json_string(text: &str) -> String {
        serde_json::to_string(text).expect("json literal")
    }

    /// A plain end-turn `openai_chat_completions` transcript.
    fn chat_text_turn(text: &str) -> String {
        format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}}}}]}}\n\ndata: [DONE]\n\n",
            json_string(text)
        )
    }

    /// A `chat.completion.chunk` `data:` line carrying a tool-call
    /// fragment — `id`/`name` on the announce fragment, bare
    /// `arguments` afterwards (agent tests' builder).
    fn sse_tool_fragment(index: u32, announce: Option<(&str, &str)>, args: &str) -> String {
        let tool_call = match announce {
            Some((id, name)) => serde_json::json!({
                "index": index,
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": args}
            }),
            None => serde_json::json!({
                "index": index,
                "function": {"arguments": args}
            }),
        };
        let chunk = serde_json::json!({
            "choices": [{"index": 0, "delta": {"tool_calls": [tool_call]}, "finish_reason": null}]
        });
        format!("data: {chunk}\n\n")
    }

    /// One tool turn calling `name` once with `args` (JSON).
    fn chat_tool_turn(call_id: &str, name: &str, args: &str) -> String {
        let mut body = sse_tool_fragment(0, Some((call_id, name)), "");
        body.push_str(&sse_tool_fragment(0, None, args));
        body.push_str(
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        );
        body.push_str("data: [DONE]\n\n");
        body
    }

    /// The task every drive test submits — `bridge: None` keeps the
    /// run shell-free (the hidden bridge's own tests cover the real
    /// session); the fixture endpoint stands in for the provider.
    fn drive_request(base_url: &str, profile_fields: &str, task_text: &str) -> AgentRequest {
        AgentRequest {
            settings: fixture_settings(base_url, profile_fields),
            profile: "default".to_owned(),
            task: task_text.to_owned(),
            prior: Vec::new(),
            skills: SkillsCatalog::default(),
            context: AgentContext {
                cwd: "/tmp".to_owned(),
                shell: "bash".to_owned(),
                os: "linux".to_owned(),
                output_lines: Vec::new(),
            },
            bridge: None,
        }
    }

    /// A `drive` run against `scripts`: the real `AgentSession` (stub
    /// secrets), `Vec<u8>` buffers for the two streams, a disabled
    /// store and a clear cancel flag — returns (exit, stdout, stderr,
    /// fixture) so the test can inspect captured request bodies.
    fn drive_fixture(scripts: Vec<Script>, profile_fields: &str) -> (u8, String, String, Fixture) {
        let fixture = Fixture::serve(scripts);
        let session = AgentSession::with_secret_store(NoSecrets);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut store = StoreHandle::disabled();
        let cancel = AtomicBool::new(false);
        let code = drive(
            &session,
            drive_request(&fixture.base_url, profile_fields, "do the thing"),
            &mut store,
            Path::new("/tmp"),
            &mut out,
            &mut err,
            &cancel,
        );
        (
            code,
            String::from_utf8(out).expect("stdout is utf-8"),
            String::from_utf8(err).expect("stderr is utf-8"),
            fixture,
        )
    }

    /// The pure seam's table — every `AgentEvent` variant onto its
    /// `Effect`: stdout only for text, stderr for the activity log,
    /// the gate replies, and the terminal exit vocabulary.
    #[test]
    fn effect_of_maps_the_whole_event_vocabulary() {
        let call = |id: &str, name: &str| ait_agent::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: "{}".into(),
        };

        assert_eq!(
            effect_of(&AgentEvent::TextDelta("hi\n".into())),
            Effect {
                stdout: "hi\n".into(),
                ..Effect::default()
            }
        );
        assert_eq!(
            effect_of(&AgentEvent::ToolCall {
                call: call("call_1", "run_command"),
                summary: "\"ls\"".into(),
            }),
            Effect {
                stderr: Some("tool run_command: \"ls\"".into()),
                ..Effect::default()
            }
        );
        assert_eq!(
            effect_of(&AgentEvent::ToolUpdate {
                call_id: "call_1".into(),
                status: ToolStatus::Done,
                detail: Some("listed".into()),
                result: Some("listed".into()),
            }),
            Effect {
                stderr: Some("tool call_1 done: listed".into()),
                ..Effect::default()
            }
        );
        assert_eq!(
            effect_of(&AgentEvent::ToolUpdate {
                call_id: "call_1".into(),
                status: ToolStatus::Running,
                detail: None,
                result: None,
            }),
            Effect {
                stderr: Some("tool call_1 running".into()),
                ..Effect::default()
            }
        );
        assert_eq!(
            effect_of(&AgentEvent::ApprovalRequest {
                call: call("call_2", "edit_files"),
                summary: "a.rs".into(),
            }),
            Effect {
                stderr: Some("denying edit_files non-interactively: a.rs".into()),
                reply: Some(Reply::Deny("call_2".into())),
                ..Effect::default()
            }
        );
        assert_eq!(
            effect_of(&AgentEvent::AskRequest {
                call: call("call_3", "ask_user_question"),
                question: "which?".into(),
                choices: vec!["a".into()],
            }),
            Effect {
                stderr: Some("answering non-interactively: which?".into()),
                reply: Some(Reply::Answer("call_3".into())),
                ..Effect::default()
            }
        );
        assert_eq!(
            effect_of(&AgentEvent::PlanUpdated {
                todos: vec![ait_agent::PlanTodo {
                    content: "x".into(),
                    status: ait_agent::PlanTodoStatus::Done,
                }],
            }),
            Effect {
                stderr: Some("plan updated (1 items)".into()),
                ..Effect::default()
            }
        );
        assert_eq!(
            effect_of(&AgentEvent::Summarized {
                covers_seq: 4,
                text: "s".into(),
            }),
            Effect {
                stderr: Some("conversation summarized".into()),
                ..Effect::default()
            }
        );
        // The terminal vocabulary: Done → 0, the rest non-zero and
        // distinct (us-26).
        assert_eq!(effect_of(&AgentEvent::Done).exit, Some(0));
        assert_eq!(effect_of(&AgentEvent::Cancelled).exit, Some(130));
        assert_eq!(effect_of(&AgentEvent::BudgetReached).exit, Some(2));
        assert_eq!(
            effect_of(&AgentEvent::Error("down".into())),
            Effect {
                stderr: Some("error: down".into()),
                exit: Some(1),
                ..Effect::default()
            }
        );
    }

    /// The startup contract (us-29): the implicit `default` arms like
    /// a TUI launch, an explicit declared name arms, an undeclared
    /// one errors naming it, and a settings file with no resolvable
    /// model is a startup failure — all before anything spawns.
    #[test]
    fn resolve_start_gates_profiles_and_models() {
        let declared = fixture_settings("http://unused", "");
        assert_eq!(resolve_start(&declared, None).as_deref(), Ok("default"));
        assert_eq!(
            resolve_start(&declared, Some("default")).as_deref(),
            Ok("default")
        );
        let err = resolve_start(&declared, Some("ghost")).unwrap_err();
        assert!(err.contains("ghost"), "{err}");

        // A declared non-default name arms; `default` undeclared
        // while other profiles exist is unknown too (an explicit
        // `--profile default` must name a real profile).
        let work = Settings::from_toml(
            r#"
[agents.custom_endpoints.local]
name = "Fixture"
base_url = "http://unused"
schema = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name = "test-model"
alias = "Test Model"
config_key = "coding-key"

[agents.execution_profiles.work]
coding_model = "coding-key"
"#,
        )
        .unwrap();
        assert_eq!(resolve_start(&work, Some("work")).as_deref(), Ok("work"));
        // `default` undeclared while profiles exist is unknown too —
        // for an explicit `--profile default`; the implicit arm
        // reaches resolve_model's own "unknown profile" error.
        assert!(resolve_start(&work, Some("default")).is_err());

        // No model bound → the startup failure (us-29's "missing
        // endpoint config").
        let unbound = Settings::from_toml("[agents.execution_profiles.default]\n").unwrap();
        let err = resolve_start(&unbound, None).unwrap_err();
        assert!(err.contains("no model configured"), "{err}");

        // Zero profiles at all: `default` still arms through the
        // implicit sole-model rule (§4.2 us-23).
        let implicit = Settings::from_toml(
            r#"
[agents.custom_endpoints.local]
name = "Fixture"
base_url = "http://unused"
schema = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name = "test-model"
alias = "Test Model"
config_key = "coding-key"
"#,
        )
        .unwrap();
        assert_eq!(resolve_start(&implicit, None).as_deref(), Ok("default"));
        // …but an explicit `--profile` still must exist.
        assert!(resolve_start(&implicit, Some("ghost")).is_err());
    }

    /// Acceptance (us-24/26): assistant text streams to stdout, the
    /// run exits 0 on `Done`, and stderr stays silent — stdout is the
    /// answer stream only.
    #[test]
    fn done_run_streams_stdout_and_exits_zero() {
        let (code, out, err, _fixture) =
            drive_fixture(vec![sse(&chat_text_turn("the answer"))], "");
        assert_eq!(code, 0);
        assert_eq!(out, "the answer");
        assert!(err.is_empty(), "stderr should be silent: {err}");
    }

    /// Acceptance (us-20/26): under `always_ask` every gated call
    /// auto-denies and the task still reaches `Done` — the denial
    /// text reaches the model as the tool result so it picks another
    /// approach; the activity lands on stderr.
    #[test]
    fn gated_calls_auto_deny_and_the_task_finishes() {
        let (code, out, err, fixture) = drive_fixture(
            vec![
                sse(&chat_tool_turn(
                    "call_1",
                    "read_files",
                    r#"{"paths":["note.txt"]}"#,
                )),
                sse(&chat_text_turn("denied — switched approach")),
            ],
            "read_files = \"always_ask\"",
        );
        assert_eq!(code, 0, "the task still reaches Done: {err}");
        assert_eq!(out, "denied — switched approach");
        assert!(err.contains("denying read_files"), "stderr: {err}");

        // The denial text IS the tool result in the next request —
        // the model reads why its call was refused.
        fixture.captured();
        let second = fixture.captured();
        assert!(second.body.contains(DENY_TEXT), "{}", second.body);
    }

    /// Acceptance (us-21): `ask_user_question` gets the canned
    /// no-interactive answer — the model reads it as the tool result
    /// and moves on instead of parking on a card nobody can answer.
    #[test]
    fn ask_user_question_gets_the_canned_answer() {
        let (code, _out, err, fixture) = drive_fixture(
            vec![
                sse(&chat_tool_turn(
                    "call_1",
                    "ask_user_question",
                    r#"{"question":"which approach?"}"#,
                )),
                sse(&chat_text_turn("understood")),
            ],
            "",
        );
        assert_eq!(code, 0, "{err}");
        assert!(err.contains("answering non-interactively"), "stderr: {err}");

        fixture.captured();
        let second = fixture.captured();
        assert!(second.body.contains(ASK_ANSWER), "{}", second.body);
    }

    /// Acceptance (us-26): a terminal `Error` exits non-zero with the
    /// message on stderr — stdout stays clean.
    #[test]
    fn an_endpoint_error_exits_nonzero_on_stderr() {
        // 400 is terminal — the adapter's bounded retry skips it, so
        // one scripted response is enough (agent tests' precedent).
        let (code, out, err, _fixture) = drive_fixture(
            vec![error_status(
                400,
                "{\"error\":{\"message\":\"provider down\"}}",
            )],
            "",
        );
        assert_ne!(code, 0, "Error is a non-zero exit");
        assert!(out.is_empty(), "an error never writes stdout: {out}");
        assert!(err.contains("error:"), "stderr: {err}");
    }

    /// Acceptance (us-28): the headless task persists exactly like a
    /// TUI conversation — one `conversations` row titled from the
    /// task, the transcript items appended in order, `status` closed
    /// when the run ends. A resume can reopen it.
    #[test]
    fn the_task_persists_a_resumable_conversation_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = ait_store::Store::open(dir.path().join("store.db")).unwrap();
        let fixture = Fixture::serve(vec![sse(&chat_text_turn("the answer"))]);
        let session = AgentSession::with_secret_store(NoSecrets);
        let mut handle = StoreHandle::from_store(store);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let cancel = AtomicBool::new(false);

        let code = drive(
            &session,
            drive_request(&fixture.base_url, "", "summarize the bug"),
            &mut handle,
            dir.path(),
            &mut out,
            &mut err,
            &cancel,
        );
        assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&err));

        let rows = handle.conversations().expect("list");
        assert_eq!(rows.len(), 1, "one conversation row");
        let row = &rows[0];
        assert_eq!(row.conversation.title, "summarize the bug");
        assert_eq!(row.conversation.status, "closed");
        let items = handle.load_transcript(row.conversation.id).unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| item.kind().to_owned())
                .collect::<Vec<_>>(),
            vec!["task", "assistant_text", "status"],
            "the TUI's own item vocabulary, in order: {items:?}"
        );
        assert_eq!(
            items[0],
            TranscriptItem::Task {
                text: "summarize the bug".into()
            }
        );
    }

    /// The SIGINT flag → `session.cancel()` path (us-27): a raised
    /// flag mid-stream ends the task with `Cancelled`'s exit code —
    /// the same semantics Esc has in the TUI. The flag stands in for
    /// a delivered signal here; the handler's own wiring is proved by
    /// `sigint_reaches_the_armed_flag`. `drive` itself runs on THIS
    /// thread — `AgentSession`'s event `Receiver` is `!Sync`, so a
    /// helper thread owns the drip feed and the flag raise instead.
    #[test]
    fn a_raised_cancel_flag_cancels_the_in_flight_task() {
        let (drip_tx, drip_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let fixture = Fixture::serve(vec![Script::DripSse(drip_rx)]);
        let session = AgentSession::with_secret_store(NoSecrets);
        let cancel = Arc::new(AtomicBool::new(false));
        // Share the stdout buffer so the helper can wait on the first
        // streamed delta — the flag must raise while a task is
        // provably in flight, not before the worker picked it up
        // (a queued-but-unstarted task still runs).
        let shared = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        struct Shared(Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("out lock").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut out = Shared(Arc::clone(&shared));
        let mut err = Vec::new();
        let mut store = StoreHandle::disabled();
        let request = drive_request(&fixture.base_url, "", "work on it");

        let helper = std::thread::spawn({
            let cancel = Arc::clone(&cancel);
            let shared = Arc::clone(&shared);
            move || {
                // One >8KiB write (tiny_http buffers ~8 KiB — a `:`
                // comment is spec-legal SSE padding) so the first
                // delta arrives, then hold the sender open: the
                // stream stalls.
                let mut chunk = format!(
                    "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}}}}]}}\n\n",
                    json_string("working on it")
                )
                .into_bytes();
                chunk.extend_from_slice(format!(": {}\n", "x".repeat(9 * 1024)).as_bytes());
                drip_tx.send(chunk).expect("drip feed");
                let deadline = Instant::now() + Duration::from_secs(10);
                while !shared
                    .lock()
                    .expect("out lock")
                    .starts_with(b"working on it")
                    && Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                // ALWAYS raise — a stalled delta must still unblock
                // `drive` so the test fails instead of hanging.
                cancel.store(true, Ordering::SeqCst);
                // …then hold the sender open a beat: dropping it ends
                // the SSE stream into `Done`, and a Done racing the
                // 50 ms flag poll makes the exit code nondeterministic.
                std::thread::sleep(Duration::from_millis(500));
            }
        });

        let code = drive(
            &session,
            request,
            &mut store,
            Path::new("/tmp"),
            &mut out,
            &mut err,
            &cancel,
        );
        helper.join().expect("the helper finished");
        assert_eq!(code, 130, "Cancelled maps to the SIGINT code");
        assert!(
            shared
                .lock()
                .expect("out lock")
                .starts_with(b"working on it"),
            "the flag raised with the task provably in flight"
        );
    }

    /// The handler's own wiring (us-27): an armed flag observes a
    /// delivered SIGINT through `catch_sigint`. One arm at a time —
    /// the slot is the arming.
    #[cfg(unix)]
    #[test]
    fn sigint_reaches_the_armed_flag() {
        let arm = arm_sigint();
        assert!(!arm.flag.load(Ordering::SeqCst));
        unsafe { libc::raise(libc::SIGINT) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !arm.flag.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "SIGINT never arrived");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
