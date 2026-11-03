//! The `AgentSession` channel API (Spec R06, §5.6; ADR-0004): the
//! `ait` frontend sends an [`AgentRequest`] — a task, the caller's
//! `Settings` snapshot and the terminal context — on one `mpsc`
//! channel and drains [`AgentEvent`]s on another during its app tick,
//! mirroring the [`CommandSuggester`](crate::CommandSuggester)
//! precedent. Internally the session owns a tokio current-thread
//! runtime on a dedicated worker thread, so `tokio` never enters the
//! `ait` binary's dependency tree or its synchronous poll loop.
//!
//! Per task the worker resolves
//! `Settings::resolve_model("default", ModelRole::Coding, store)` —
//! the `coding_model` → `base_model` fallback lives inside
//! `resolve_model` (§4.2) — so a hot reload that swapped `Settings`
//! app-side takes effect on the next task with no plumbing here. It
//! then assembles the per-schema request ([`Conversation`]): the
//! system prompt carries cwd/OS/shell, the nearest `AGENTS.md` found
//! walking up from cwd, and the last
//! [`OUTPUT_LINE_BUDGET`](crate::OUTPUT_LINE_BUDGET) lines of redacted
//! terminal output ([`assemble_system_prompt`]). Text deltas stream
//! back as [`AgentEvent::TextDelta`]s — raw, not sanitized: panel text
//! renders through ratatui and is never typed into the user's input
//! line, so the never-execute guarantee does not apply at this seam
//! (CODING_STANDARDS §13 governs PTY-bound text only).
//!
//! The agent loop: assemble → stream → accumulate streamed tool-call
//! deltas → gate + execute sequentially → append per-schema result
//! items → re-request. The **interim permission gate** (Spec R06's
//! ask-everything rule, until Round 7's §5.7 profiles land): every
//! tool call suspends the loop on an
//! [`AgentEvent::ApprovalRequest`] — carrying the call and a
//! renderable argument summary — until the frontend's
//! [`AgentSession::approve`] answers with an [`ApprovalVerdict`]:
//! *allow once*, *allow this tool for the rest of the task*, or
//! *deny* (the denial text becomes the tool result). Prompts never
//! time out (us-9); Esc still cancels the whole task, and
//! `ask_user_question` is exempt by design — its own card is the ask
//! (us-10): a call to it suspends on an [`AgentEvent::AskRequest`] —
//! carrying the question and its choices-or-free-text — until the
//! frontend's [`AgentSession::answer`] replies, and the answer text
//! becomes the call's tool result. The gate sits at the same
//! consult-point §5.7's profile engine will drive — Round 7 swaps
//! the policy, not the plumbing.
//!
//! Calls in one turn execute sequentially in model order (us-33),
//! each gated. Every tool result passes [`redact`](crate::redact)
//! before entering the conversation — §5.10.3's rule extends from
//! terminal output to everything the agent touches. The run ends on
//! a no-tool-calls end turn with no steering queued, cancellation
//! (the shared `CancellationToken`, the frontend's Esc path), or the
//! [`STEP_BUDGET`] model-turn cap. A steering message queued mid-run
//! ([`AgentSession::steer`]) is appended to the conversation as a
//! user message and rides the next request; it never interrupts an
//! in-flight stream.
//!
//! Every failure mode (unconfigured model, resolution/transport/stream
//! error, provider error event, cancelled request) maps to a terminal
//! [`AgentEvent::Error`] or [`AgentEvent::Cancelled`]; the worker never
//! panics and never wedges — a poisoned task answers `Error`, then the
//! next queued task is served.

use std::collections::HashSet;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ait_config::{ModelRole, OsKeyring, SecretStore, Settings};
use ait_llm::{Endpoint, EndpointConfig, EndpointRequest, EndpointStream, LlmError};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::agent_context::{assemble_system_prompt, find_agents_md, AgentContext};
use crate::conversation::Conversation;
use crate::endpoint_bridge::{extract, llm_schema, slot, Extracted};
use crate::files::{ToolCwd, ToolOutput};
use crate::redact::redact;
use crate::tool_call::{CallAccumulator, ToolCall, ToolStatus};
use crate::tools;

/// The execution profile whose model roles agent tasks resolve
/// against (§4.2 — the only profile Round 6 wires; per-profile roles
/// are Round 7). The `CommandSuggester` precedent resolves on the same
/// profile.
const PROFILE: &str = "default";

/// The step budget (Spec R06, glossary): at most this many model turns
/// per task. Configurability is a later round's decision; the constant
/// names the termination condition the agent loop checks before every
/// request — tool-call turns included.
pub const STEP_BUDGET: usize = 50;

/// How often a gate wait wakes to check the cancellation token — the
/// only reason the wait is a poll at all (us-9: prompts never time
/// out into a verdict; Esc answers them instead).
const GATE_POLL: Duration = Duration::from_millis(100);

/// The tool-result text when the user denies a call without typing a
/// reason — the panel sends it, and the worker substitutes it for an
/// empty `Deny`, so the model always reads a sentence.
const DEFAULT_DENIAL: &str = "the user denied this call";

/// One submitted task — the value the frontend sends on the session's
/// request channel. It carries the task text verbatim plus the
/// terminal-side context the model may see (§5.6) and the caller's
/// current `Settings` snapshot: the session never reads
/// `settings.toml` itself, so a hot reload takes effect on the next
/// task without any plumbing here.
pub struct AgentRequest {
    /// The caller's live `Settings` — `coding_model` (with the
    /// `base_model` fallback) is resolved per task, so the latest
    /// hot-reloaded binding applies immediately.
    pub settings: Settings,
    /// The task text — sent verbatim, never redacted (the
    /// typed-request rule, §5.4 us-19).
    pub task: String,
    /// The terminal-side context — cwd, shell, OS and recent output
    /// lines; assembled into the system prompt by
    /// [`assemble_system_prompt`]. `cwd` also binds the file tools:
    /// every path the agent reads or searches stays under it.
    pub context: AgentContext,
}

/// The user's answer to an [`AgentEvent::ApprovalRequest`] — the
/// interim gate's whole vocabulary (Spec R06's three-way card).
/// Round 7's profile engine will compute the same verdict itself;
/// the channel shape survives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalVerdict {
    /// Run this call once; ask again next time.
    AllowOnce,
    /// Run this call and every later call to the same tool for the
    /// rest of the task — the suppression is per tool, never blanket
    /// (us-7).
    AllowForTask,
    /// Refuse the call; the text becomes its tool result so the model
    /// can pick a different approach (us-8). An empty reason reads as
    /// [`DEFAULT_DENIAL`].
    Deny(String),
}

/// One outcome of an [`AgentRequest`], drained with
/// [`Receiver::try_recv`] once per app-loop tick.
/// [`Done`](AgentEvent::Done), [`Cancelled`](AgentEvent::Cancelled),
/// [`BudgetReached`](AgentEvent::BudgetReached) and
/// [`Error`](AgentEvent::Error) are terminal for the task.
#[derive(Debug, PartialEq, Eq)]
pub enum AgentEvent {
    /// A fragment of assistant text to append to the conversation
    /// panel — raw model text (newlines included); it is rendered, not
    /// typed, so no sanitizer applies.
    /// Concatenating every `TextDelta` of a task yields the full
    /// streamed reply.
    TextDelta(String),
    /// The gate suspended on a call: nothing executes until the
    /// frontend answers [`AgentSession::approve`] with an
    /// [`ApprovalVerdict`] for `call.id`. The panel renders the
    /// approve card — `summary` is the renderable argument summary
    /// (command text, file paths, the `edit_files` diff; us-5/22).
    ApprovalRequest {
        /// The complete call awaiting a verdict.
        call: ToolCall,
        /// The call's renderable argument summary (command text,
        /// file paths — the `edit_files` unified diff, us-22).
        summary: String,
    },
    /// A call began executing without a gate wait — an
    /// allow-for-task repeat or a gate-exempt tool. The panel opens
    /// its tool card already `Running`.
    ToolCall {
        /// The call now executing.
        call: ToolCall,
        /// The call's renderable argument summary.
        summary: String,
    },
    /// `ask_user_question`'s own card (us-10 — gate-exempt because
    /// the card IS the ask): the loop is suspended until the
    /// frontend answers [`AgentSession::answer`] for `call.id`; the
    /// answer text becomes the call's tool result. The panel renders
    /// `question` plus `choices` — an empty list means the answer is
    /// free-text.
    AskRequest {
        /// The complete call awaiting an answer.
        call: ToolCall,
        /// The question to render.
        question: String,
        /// The model's offered answer choices; empty means the user
        /// answers in free text.
        choices: Vec<String>,
    },
    /// A tool card's status transition (us-5): `Running` after an
    /// allow verdict, `Done`/`Failed` after execution (the card's
    /// result preview or error rides in `detail`), `Denied` after a
    /// deny verdict (the denial text in `detail`).
    ToolUpdate {
        /// The `call.id` this update applies to.
        call_id: String,
        /// The new status.
        status: ToolStatus,
        /// A one-line detail for the card — result preview or error.
        detail: Option<String>,
    },
    /// The task finished cleanly: the model ended its turn with no
    /// tool calls and no steering queued. Terminal.
    Done,
    /// The user cancelled (Esc): the in-flight stream or gate wait
    /// stopped and the task ended. Terminal.
    Cancelled,
    /// The [`STEP_BUDGET`] model-turn cap tripped before the task
    /// finished. Terminal.
    BudgetReached,
    /// A terminal failure rendered for the panel: unconfigured model,
    /// endpoint down, transport/stream failure, provider-reported
    /// error. Terminal.
    Error(String),
}

/// [`AgentSession::submit`], [`AgentSession::steer`] or
/// [`AgentSession::approve`] could not queue the value — the worker
/// thread is gone (the only failure mode, so the error is
/// payload-free).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentError;

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("agent session is unavailable")
    }
}

impl std::error::Error for AgentError {}

/// A gate wait's outcome — what the worker does next.
enum GateOutcome {
    /// The frontend sent a verdict for the awaited call.
    Verdict(ApprovalVerdict),
    /// Esc: the cancellation token fired mid-wait.
    Cancelled,
    /// The verdict channel closed — the handle is gone; reporting
    /// anything is pointless.
    Gone,
}

/// An ask wait's outcome — [`GateOutcome`]'s sibling for the
/// `ask_user_question` card.
enum AskOutcome {
    /// The frontend sent the user's answer for the awaited call.
    Answered(String),
    /// Esc: the cancellation token fired mid-wait.
    Cancelled,
    /// The answer channel closed — the handle is gone.
    Gone,
}

/// The synchronous handle the frontend holds — one agent conversation
/// per `Tab` (Spec R06 us: per-tab ownership). One in-flight task at a
/// time is the frontend's contract; queued tasks run in order on the
/// single worker.
pub struct AgentSession {
    requests: Option<Sender<AgentRequest>>,
    steering: Option<Sender<String>>,
    /// Approve-card answers — `(call_id, verdict)` pairs; the worker
    /// matches `call_id` so a stale verdict (answered after its card
    /// was already resolved or its task ended) can never decide a
    /// different call.
    verdicts: Option<Sender<(String, ApprovalVerdict)>>,
    /// Ask-card answers — `(call_id, answer)` pairs, matched on
    /// `call_id` the same way the verdicts are.
    answers: Option<Sender<(String, String)>>,
    events: Receiver<AgentEvent>,
    /// The in-flight task's cancellation token — shared so
    /// [`cancel`](AgentSession::cancel) (the frontend's Esc) reaches
    /// into the worker's `block_on` from any thread.
    in_flight: Arc<Mutex<Option<CancellationToken>>>,
    thread: Option<JoinHandle<()>>,
}

impl AgentSession {
    /// Start the worker thread with the production secret store —
    /// [`OsKeyring`] (§4.1: `api_key_env` first, OS keyring second, no
    /// `Authorization` header when both miss).
    pub fn new() -> Self {
        Self::with_secret_store(OsKeyring)
    }

    /// [`new`](Self::new) with an explicit [`SecretStore`] — the seam
    /// tests inject a stub through instead of touching a real OS
    /// keyring.
    pub fn with_secret_store(store: impl SecretStore + Send + 'static) -> Self {
        let (requests_tx, requests_rx) = mpsc::channel::<AgentRequest>();
        let (steering_tx, steering_rx) = mpsc::channel::<String>();
        let (verdicts_tx, verdicts_rx) = mpsc::channel::<(String, ApprovalVerdict)>();
        let (answers_tx, answers_rx) = mpsc::channel::<(String, String)>();
        let (events_tx, events_rx) = mpsc::channel::<AgentEvent>();
        let in_flight = Arc::new(Mutex::new(None));
        let thread = thread::spawn({
            let in_flight = Arc::clone(&in_flight);
            move || {
                worker(
                    requests_rx,
                    steering_rx,
                    verdicts_rx,
                    answers_rx,
                    events_tx,
                    in_flight,
                    Box::new(store),
                )
            }
        });
        Self {
            requests: Some(requests_tx),
            steering: Some(steering_tx),
            verdicts: Some(verdicts_tx),
            answers: Some(answers_tx),
            events: events_rx,
            in_flight,
            thread: Some(thread),
        }
    }

    /// Queue `request` for the agent loop. Fails only if the worker is
    /// gone — an [`AgentError`] the panel can surface.
    pub fn submit(&self, request: AgentRequest) -> Result<(), AgentError> {
        match &self.requests {
            Some(sender) => sender.send(request).map_err(|_| AgentError),
            None => Err(AgentError),
        }
    }

    /// Queue a steering message (Spec R06: frontend input during a run
    /// enqueues a user message the worker injects at the next model
    /// turn; it never interrupts an in-flight request mid-stream). A
    /// message queued while no task runs joins the next task's
    /// conversation. Fails only if the worker is gone.
    pub fn steer(&self, message: String) -> Result<(), AgentError> {
        match &self.steering {
            Some(sender) => sender.send(message).map_err(|_| AgentError),
            None => Err(AgentError),
        }
    }

    /// Answer the pending [`AgentEvent::ApprovalRequest`] for `call_id`
    /// (Spec R06's approve card — *allow once* / *allow this tool for
    /// the rest of the task* / *deny*). The worker ignores a verdict
    /// whose `call_id` is not the call it suspended on, so a stale
    /// answer can never resolve a different call. Fails only if the
    /// worker is gone.
    pub fn approve(&self, call_id: &str, verdict: ApprovalVerdict) -> Result<(), AgentError> {
        match &self.verdicts {
            Some(sender) => sender
                .send((call_id.to_string(), verdict))
                .map_err(|_| AgentError),
            None => Err(AgentError),
        }
    }

    /// Answer the pending [`AgentEvent::AskRequest`] for `call_id`
    /// (Spec R06 us-10's ask card — a selected choice or the typed
    /// free-text reply). `answer` travels verbatim and becomes the
    /// call's tool result, like a denial reason. The worker ignores
    /// an answer whose `call_id` is not the call it suspended on, so
    /// a stale answer can never resolve a different ask. Fails only
    /// if the worker is gone.
    pub fn answer(&self, call_id: &str, answer: String) -> Result<(), AgentError> {
        match &self.answers {
            Some(sender) => sender
                .send((call_id.to_string(), answer))
                .map_err(|_| AgentError),
            None => Err(AgentError),
        }
    }

    /// The channel of agent events — drain with
    /// [`Receiver::try_recv`] once per tick, or block on
    /// [`Receiver::recv_timeout`] in tests.
    pub fn events(&self) -> &Receiver<AgentEvent> {
        &self.events
    }

    /// Cancel the in-flight task — the Esc path. The worker's
    /// `Endpoint::stream` sees the token promptly, mid-stream and
    /// mid-backoff, and a gate wait notices it within
    /// [`GATE_POLL`]; an [`AgentEvent::Cancelled`] follows on the
    /// event channel. No-op while idle, and a queued-but-unstarted
    /// task still runs.
    pub fn cancel(&self) {
        if let Some(token) = slot(&self.in_flight).as_ref() {
            token.cancel();
        }
    }
}

impl Default for AgentSession {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for AgentSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSession")
            .field("events", &self.events)
            .finish_non_exhaustive()
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        // Cancel whatever is streaming or gated so `block_on` unwinds
        // promptly, then close every channel — the worker's `recv`
        // ends and the thread exits; join so no worker outlives its
        // handle.
        self.cancel();
        self.requests.take();
        self.steering.take();
        self.verdicts.take();
        self.answers.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The worker thread: own the current-thread runtime and serve tasks
/// in order. A runtime that cannot be built degrades every task to an
/// `Error` event rather than wedging the frontend (§6).
fn worker(
    requests: Receiver<AgentRequest>,
    steering: Receiver<String>,
    verdicts: Receiver<(String, ApprovalVerdict)>,
    answers: Receiver<(String, String)>,
    events: Sender<AgentEvent>,
    in_flight: Arc<Mutex<Option<CancellationToken>>>,
    store: Box<dyn SecretStore + Send>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            while requests.recv().is_ok() {
                emit(
                    &events,
                    AgentEvent::Error(format!("failed to start the tokio runtime: {error}")),
                );
            }
            return;
        }
    };
    while let Ok(request) = requests.recv() {
        serve(
            &runtime,
            request,
            &steering,
            &verdicts,
            &answers,
            &events,
            &in_flight,
            store.as_ref(),
        );
    }
}

/// One task: resolve `coding_model` → build the endpoint → assemble
/// the system prompt and per-schema conversation → run the model
/// loop. Every failure maps to a single terminal event; `Done` is
/// emitted only when nothing failed.
#[allow(clippy::too_many_arguments)]
fn serve(
    runtime: &tokio::runtime::Runtime,
    request: AgentRequest,
    steering: &Receiver<String>,
    verdicts: &Receiver<(String, ApprovalVerdict)>,
    answers: &Receiver<(String, String)>,
    events: &Sender<AgentEvent>,
    in_flight: &Mutex<Option<CancellationToken>>,
    store: &dyn SecretStore,
) {
    // `resolve_model` takes `&(impl SecretStore + ?Sized)` — the boxed
    // store's `&dyn` reference flows straight through.
    let resolved = match request
        .settings
        .resolve_model(PROFILE, ModelRole::Coding, store)
    {
        Ok(Some(resolved)) => resolved,
        Ok(None) => {
            emit(
                events,
                AgentEvent::Error(format!(
                    "profile {PROFILE:?} has no coding_model configured \
                     (and no base_model fallback) — set it in settings.toml"
                )),
            );
            return;
        }
        Err(error) => {
            emit(events, AgentEvent::Error(format!("{error}")));
            return;
        }
    };

    let schema = llm_schema(resolved.endpoint.schema);
    let endpoint = match Endpoint::new(EndpointConfig {
        base_url: resolved.endpoint.base_url.clone(),
        model: resolved.model.name.clone(),
        secret: resolved.secret().map(str::to_owned),
        schema,
    }) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            emit(events, AgentEvent::Error(format!("{error}")));
            return;
        }
    };

    let system = assemble_system_prompt(
        &request.context,
        find_agents_md(Path::new(&request.context.cwd)).as_deref(),
    );
    let mut conversation = Conversation::new(endpoint.schema());
    conversation.push_user(request.task);
    // Steering that queued while the worker was idle joins this task's
    // conversation rather than being dropped.
    drain_steering(steering, &mut conversation);
    // The task's file tools bind to the session's cwd — every path
    // they touch stays under it (Spec R06's confinement rule).
    let cwd = ToolCwd::new(&request.context.cwd);

    let token = CancellationToken::new();
    *slot(in_flight) = Some(token.clone());
    runtime.block_on(run_loop(
        &endpoint,
        &mut conversation,
        &system,
        steering,
        verdicts,
        answers,
        &cwd,
        token,
        events,
    ));
    slot(in_flight).take();
}

/// The agent loop (Spec R06): request → stream → accumulate tool
/// calls → gate + execute sequentially in model order → append
/// per-schema assistant-call and tool-result items → re-request.
/// Terminates on a no-tool-calls end turn with no steering queued,
/// the [`STEP_BUDGET`] tripping, cancellation, or a failure mapped to
/// a terminal `Error`. Steering queued mid-run is drained into the
/// conversation at each turn boundary — model turn or tool turn alike.
#[allow(clippy::too_many_arguments)]
async fn run_loop(
    endpoint: &Endpoint,
    conversation: &mut Conversation,
    system: &str,
    steering: &Receiver<String>,
    verdicts: &Receiver<(String, ApprovalVerdict)>,
    answers: &Receiver<(String, String)>,
    cwd: &ToolCwd,
    cancel: CancellationToken,
    events: &Sender<AgentEvent>,
) {
    let mut turns = 0usize;
    // Tools the user allowed for the rest of the task — the interim
    // gate's only memory; per name, never blanket (us-7).
    let mut allowed_for_task: HashSet<String> = HashSet::new();
    loop {
        if turns >= STEP_BUDGET {
            emit(events, AgentEvent::BudgetReached);
            return;
        }
        let request = conversation.request(&endpoint.config().model, system);
        match stream_turn(endpoint, request, cancel.clone(), events).await {
            TurnOutcome::EndTurn { text, calls } => {
                turns += 1;
                if calls.is_empty() {
                    conversation.push_assistant(text);
                    // A steering message queued during the stream
                    // becomes a user message on the next request —
                    // the run continues instead of finishing.
                    if drain_steering(steering, conversation) {
                        continue;
                    }
                    emit(events, AgentEvent::Done);
                    return;
                }
                // A tool turn: the assistant item lands first — the
                // conversation records the calls regardless of the
                // verdicts — then each call gates and executes in
                // model order.
                conversation.push_tool_calls(&text, &calls);
                let mut results = Vec::with_capacity(calls.len());
                for call in &calls {
                    if cancel.is_cancelled() {
                        emit(events, AgentEvent::Cancelled);
                        return;
                    }
                    match gate_and_execute(
                        call,
                        &mut allowed_for_task,
                        verdicts,
                        answers,
                        cwd,
                        &cancel,
                        events,
                    ) {
                        Some(result) => results.push(result),
                        None => return,
                    }
                }
                conversation.push_tool_results(&calls, &results);
                // Steering queued during the tool turn rides the next
                // request beside the results.
                drain_steering(steering, conversation);
            }
            TurnOutcome::Cancelled => {
                emit(events, AgentEvent::Cancelled);
                return;
            }
            // The `Error` event was already emitted inside
            // `stream_turn` (or the receiver is gone — nothing left
            // to report to).
            TurnOutcome::Ended => return,
        }
    }
}

/// The interim permission gate plus one call's execution (Spec R06):
/// emit the call's card event, suspend on an
/// [`ApprovalVerdict`] when the tool is not exempt and was not
/// allowed for the task, then run the executor. Returns the text the
/// tool result carries — the executor's (redacted) output, or the
/// denial text — and `None` when the run must end (cancelled, or the
/// frontend's channels are gone).
fn gate_and_execute(
    call: &ToolCall,
    allowed_for_task: &mut HashSet<String>,
    verdicts: &Receiver<(String, ApprovalVerdict)>,
    answers: &Receiver<(String, String)>,
    cwd: &ToolCwd,
    cancel: &CancellationToken,
    events: &Sender<AgentEvent>,
) -> Option<String> {
    // `ask_user_question` never gates and never executes — its own
    // card IS the ask (us-10): it suspends on the answer channel and
    // the reply becomes the call's tool result.
    if call.name == tools::ASK_USER_QUESTION {
        return ask_and_answer(call, answers, cancel, events);
    }
    // `edit_files` plans BEFORE the card (Spec R06 us-22/23): the
    // plan's unified diff is the summary the user approves, and the
    // same plan is what an allow verdict writes — the rendered diff
    // is byte-for-byte the applied change. A planning error still
    // rides the card (the paths plus why) and replays as the tool
    // result so the model reads what went wrong.
    let plan = match call.name.as_str() {
        "edit_files" => Some(tools::plan_edit(call, cwd)),
        _ => None,
    };
    let summary = match &plan {
        Some(Ok(plan)) => plan.summary(),
        Some(Err(error)) => match tools::summarize_args(call).as_str() {
            "" => format!("edit_files: {error}"),
            paths => format!("{paths}\nedit_files: {error}"),
        },
        None => tools::summarize_args(call),
    };
    let needs_verdict = !tools::gate_exempt(&call.name) && !allowed_for_task.contains(&call.name);
    let event = if needs_verdict {
        AgentEvent::ApprovalRequest {
            call: call.clone(),
            summary,
        }
    } else {
        AgentEvent::ToolCall {
            call: call.clone(),
            summary,
        }
    };
    if !emit(events, event) {
        return None;
    }
    if needs_verdict {
        match await_verdict(verdicts, &call.id, cancel) {
            GateOutcome::Verdict(ApprovalVerdict::AllowOnce) => {}
            GateOutcome::Verdict(ApprovalVerdict::AllowForTask) => {
                allowed_for_task.insert(call.name.clone());
            }
            GateOutcome::Verdict(ApprovalVerdict::Deny(mut reason)) => {
                if reason.is_empty() {
                    reason = DEFAULT_DENIAL.to_string();
                }
                emit(
                    events,
                    AgentEvent::ToolUpdate {
                        call_id: call.id.clone(),
                        status: ToolStatus::Denied,
                        detail: Some(reason.clone()),
                    },
                );
                // The denial text IS the tool result (us-8) — the
                // model reads it and picks a different approach.
                return Some(reason);
            }
            GateOutcome::Cancelled => {
                emit(events, AgentEvent::Cancelled);
                return None;
            }
            GateOutcome::Gone => return None,
        }
        if !emit(
            events,
            AgentEvent::ToolUpdate {
                call_id: call.id.clone(),
                status: ToolStatus::Running,
                detail: None,
            },
        ) {
            return None;
        }
    }
    // Apply the plan the card showed (edit_files); every other call
    // dispatches through the executor.
    let output = match plan {
        Some(Ok(plan)) => plan.apply(),
        Some(Err(error)) => ToolOutput::error(error),
        None => tools::execute(call, cwd),
    };
    // §5.10.3, extended to everything the agent touches: the result
    // is redacted before it enters the conversation — and the card's
    // preview comes from the same redacted text.
    let text = redact(&output.text);
    emit(
        events,
        AgentEvent::ToolUpdate {
            call_id: call.id.clone(),
            status: if output.is_error {
                ToolStatus::Failed
            } else {
                ToolStatus::Done
            },
            detail: Some(preview(&text)),
        },
    );
    Some(text)
}

/// The `ask_user_question` flow (us-10): emit the call's own card —
/// [`AgentEvent::AskRequest`] carrying the parsed question and
/// choices-or-free-text — and suspend on the answer channel; the
/// user's reply becomes the call's tool result verbatim (the user's
/// own words, like a denial reason, so no `redact` applies). A call
/// whose arguments carry no usable question degrades to a tool error
/// — model input, never a parked card the frontend cannot answer.
/// Returns `None` when the run must end (cancelled, or the
/// frontend's channels are gone).
fn ask_and_answer(
    call: &ToolCall,
    answers: &Receiver<(String, String)>,
    cancel: &CancellationToken,
    events: &Sender<AgentEvent>,
) -> Option<String> {
    let (question, choices) = match tools::parse_question(call) {
        Ok(parsed) => parsed,
        Err(error) => {
            if !emit(
                events,
                AgentEvent::ToolCall {
                    call: call.clone(),
                    summary: tools::summarize_args(call),
                },
            ) {
                return None;
            }
            emit(
                events,
                AgentEvent::ToolUpdate {
                    call_id: call.id.clone(),
                    status: ToolStatus::Failed,
                    detail: Some(error.clone()),
                },
            );
            return Some(error);
        }
    };
    if !emit(
        events,
        AgentEvent::AskRequest {
            call: call.clone(),
            question,
            choices,
        },
    ) {
        return None;
    }
    match await_answer(answers, &call.id, cancel) {
        AskOutcome::Answered(answer) => {
            emit(
                events,
                AgentEvent::ToolUpdate {
                    call_id: call.id.clone(),
                    status: ToolStatus::Done,
                    detail: Some(preview(&answer)),
                },
            );
            Some(answer)
        }
        AskOutcome::Cancelled => {
            emit(events, AgentEvent::Cancelled);
            None
        }
        AskOutcome::Gone => None,
    }
}

/// Park on the answer channel until the reply for `call_id` arrives —
/// indefinitely, on the [`GATE_POLL`] cadence, exactly like
/// [`await_verdict`]: answers addressed to other calls drop, Esc's
/// cancellation token resolves the wait, and a closed channel means
/// the handle is gone.
fn await_answer(
    answers: &Receiver<(String, String)>,
    call_id: &str,
    cancel: &CancellationToken,
) -> AskOutcome {
    loop {
        if cancel.is_cancelled() {
            return AskOutcome::Cancelled;
        }
        match answers.recv_timeout(GATE_POLL) {
            Ok((id, answer)) if id == call_id => return AskOutcome::Answered(answer),
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return AskOutcome::Gone,
        }
    }
}

/// Park on the verdict channel until the answer for `call_id`
/// arrives — indefinitely (us-9: no timeout into a default verdict).
/// The [`GATE_POLL`] cadence exists only to notice the cancellation
/// token (Esc resolves a gate wait too) and stale verdicts — answers
/// to calls the loop is not waiting on — drop rather than deciding
/// the wrong call.
fn await_verdict(
    verdicts: &Receiver<(String, ApprovalVerdict)>,
    call_id: &str,
    cancel: &CancellationToken,
) -> GateOutcome {
    loop {
        if cancel.is_cancelled() {
            return GateOutcome::Cancelled;
        }
        match verdicts.recv_timeout(GATE_POLL) {
            Ok((id, verdict)) if id == call_id => return GateOutcome::Verdict(verdict),
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return GateOutcome::Gone,
        }
    }
}

/// A card's `detail`: the first line of the (already redacted) tool
/// result, clipped — enough to recognize what ran without turning
/// the scrollback into the output itself.
fn preview(text: &str) -> String {
    const PREVIEW_CHARS: usize = 120;
    let first = text.lines().next().unwrap_or_default();
    if first.chars().count() > PREVIEW_CHARS {
        format!("{}…", first.chars().take(PREVIEW_CHARS).collect::<String>())
    } else if text.lines().nth(1).is_some() {
        format!("{first} …")
    } else {
        first.to_string()
    }
}

/// How one streamed model turn ended.
enum TurnOutcome {
    /// The stream ended cleanly; carries the reply's concatenated
    /// text and the turn's accumulated tool calls (empty on a plain
    /// end turn — the cross-schema "the model asked for tools"
    /// signal is ≥1 accumulated call, not a `Finish` reason).
    EndTurn { text: String, calls: Vec<ToolCall> },
    /// The cancellation token fired (in-flight or pre-flight).
    Cancelled,
    /// Terminal: an `Error` was emitted, or the frontend's receiver is
    /// gone and reporting is pointless.
    Ended,
}

/// Drain queued steering messages into `conversation` as user
/// messages; `true` when at least one was appended.
fn drain_steering(steering: &Receiver<String>, conversation: &mut Conversation) -> bool {
    let mut drained = false;
    while let Ok(message) = steering.try_recv() {
        conversation.push_user(message);
        drained = true;
    }
    drained
}

/// Drive one request's stream and emit events: text fragments into
/// [`AgentEvent::TextDelta`]s (and the turn's accumulated reply),
/// tool-call fragments into the [`CallAccumulator`], provider error
/// events and [`LlmError`]s into terminal `Error`s, cancellation into
/// [`TurnOutcome::Cancelled`]. Sends stop early if the frontend's
/// receiver is gone.
async fn stream_turn(
    endpoint: &Endpoint,
    request: EndpointRequest,
    cancel: CancellationToken,
    events: &Sender<AgentEvent>,
) -> TurnOutcome {
    let mut stream: EndpointStream = match endpoint.stream(request, cancel).await {
        Ok(stream) => stream,
        Err(LlmError::Cancelled) => return TurnOutcome::Cancelled,
        Err(error) => {
            emit(events, AgentEvent::Error(format!("{error}")));
            return TurnOutcome::Ended;
        }
    };
    let mut reply = String::new();
    let mut accumulator = CallAccumulator::new();
    while let Some(item) = stream.next().await {
        match item {
            Err(LlmError::Cancelled) => return TurnOutcome::Cancelled,
            Err(error) => {
                emit(events, AgentEvent::Error(format!("{error}")));
                return TurnOutcome::Ended;
            }
            Ok(event) => {
                accumulator.push(&event);
                match extract(event) {
                    Extracted::Text(text) => {
                        reply.push_str(&text);
                        if !emit(events, AgentEvent::TextDelta(text)) {
                            return TurnOutcome::Ended;
                        }
                    }
                    Extracted::ProviderError(message) => {
                        emit(events, AgentEvent::Error(message));
                        return TurnOutcome::Ended;
                    }
                    Extracted::Other => {}
                }
            }
        }
    }
    TurnOutcome::EndTurn {
        text: reply,
        calls: accumulator.finish(),
    }
}

/// Send one event; `false` when the frontend's receiver is gone — the
/// worker then abandons the rest of the task rather than streaming
/// into the void.
fn emit(events: &Sender<AgentEvent>, event: AgentEvent) -> bool {
    events.send(event).is_ok()
}
