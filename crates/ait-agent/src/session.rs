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
//! No tools yet: the loop emits requests and streams replies until an
//! end-of-turn with no steering queued, cancellation (the shared
//! `CancellationToken`, the frontend's Esc path), or the
//! [`STEP_BUDGET`] model-turn cap — tool calls, approvals and
//! `ask_user_question` arrive with the registry ticket. A steering
//! message queued mid-run ([`AgentSession::steer`]) is appended to the
//! conversation as a user message and rides the next request; it never
//! interrupts an in-flight stream.
//!
//! Every failure mode (unconfigured model, resolution/transport/stream
//! error, provider error event, cancelled request) maps to a terminal
//! [`AgentEvent::Error`] or [`AgentEvent::Cancelled`]; the worker never
//! panics and never wedges — a poisoned task answers `Error`, then the
//! next queued task is served.

use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use ait_config::{ModelRole, OsKeyring, SecretStore, Settings};
use ait_llm::{Endpoint, EndpointConfig, EndpointRequest, EndpointStream, LlmError};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::agent_context::{assemble_system_prompt, find_agents_md, AgentContext};
use crate::conversation::Conversation;
use crate::endpoint_bridge::{extract, llm_schema, slot, Extracted};

/// The execution profile whose model roles agent tasks resolve
/// against (§4.2 — the only profile Round 6 wires; per-profile roles
/// are Round 7). The `CommandSuggester` precedent resolves on the same
/// profile.
const PROFILE: &str = "default";

/// The step budget (Spec R06, glossary): at most this many model turns
/// per task. Configurability is a later round's decision; the constant
/// names the termination condition the agent loop checks before every
/// request — including the tool-call turns the registry ticket adds.
pub const STEP_BUDGET: usize = 50;

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
    /// [`assemble_system_prompt`].
    pub context: AgentContext,
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
    /// The task finished cleanly: the model ended its turn with no
    /// tool calls and no steering queued. Terminal.
    Done,
    /// The user cancelled (Esc): the in-flight stream stopped and the
    /// task ended. Terminal.
    Cancelled,
    /// The [`STEP_BUDGET`] model-turn cap tripped before the task
    /// finished. Terminal.
    BudgetReached,
    /// A terminal failure rendered for the panel: unconfigured model,
    /// endpoint down, transport/stream failure, provider-reported
    /// error. Terminal.
    Error(String),
}

/// [`AgentSession::submit`] or [`AgentSession::steer`] could not queue
/// the value — the worker thread is gone (the only failure mode, so
/// the error is payload-free).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentError;

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("agent session is unavailable")
    }
}

impl std::error::Error for AgentError {}

/// The synchronous handle the frontend holds — one agent conversation
/// per `Tab` (Spec R06 us: per-tab ownership). One in-flight task at a
/// time is the frontend's contract; queued tasks run in order on the
/// single worker.
pub struct AgentSession {
    requests: Option<Sender<AgentRequest>>,
    steering: Option<Sender<String>>,
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
        let (events_tx, events_rx) = mpsc::channel::<AgentEvent>();
        let in_flight = Arc::new(Mutex::new(None));
        let thread = thread::spawn({
            let in_flight = Arc::clone(&in_flight);
            move || {
                worker(
                    requests_rx,
                    steering_rx,
                    events_tx,
                    in_flight,
                    Box::new(store),
                )
            }
        });
        Self {
            requests: Some(requests_tx),
            steering: Some(steering_tx),
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

    /// The channel of agent events — drain with
    /// [`Receiver::try_recv`] once per tick, or block on
    /// [`Receiver::recv_timeout`] in tests.
    pub fn events(&self) -> &Receiver<AgentEvent> {
        &self.events
    }

    /// Cancel the in-flight task — the Esc path. The worker's
    /// `Endpoint::stream` sees the token promptly, mid-stream and
    /// mid-backoff; an [`AgentEvent::Cancelled`] follows on the event
    /// channel. No-op while idle, and a queued-but-unstarted task
    /// still runs.
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
        // Cancel whatever is streaming so `block_on` unwinds promptly,
        // then close both channels — the worker's `recv` ends and the
        // thread exits; join so no worker outlives its handle.
        self.cancel();
        self.requests.take();
        self.steering.take();
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
fn serve(
    runtime: &tokio::runtime::Runtime,
    request: AgentRequest,
    steering: &Receiver<String>,
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

    let token = CancellationToken::new();
    *slot(in_flight) = Some(token.clone());
    runtime.block_on(run_loop(
        &endpoint,
        &mut conversation,
        &system,
        steering,
        token,
        events,
    ));
    slot(in_flight).take();
}

/// The agent loop without tools: request → stream → emit, until the
/// model ends its turn with no steering queued (task `Done`), the
/// [`STEP_BUDGET`] trips, the task is cancelled, or a failure maps to
/// a terminal `Error`. Steering queued mid-run is drained into the
/// conversation at each turn boundary — the seam where tool results
/// will also be appended once the registry lands.
async fn run_loop(
    endpoint: &Endpoint,
    conversation: &mut Conversation,
    system: &str,
    steering: &Receiver<String>,
    cancel: CancellationToken,
    events: &Sender<AgentEvent>,
) {
    let mut turns = 0usize;
    loop {
        if turns >= STEP_BUDGET {
            emit(events, AgentEvent::BudgetReached);
            return;
        }
        let request = conversation.request(&endpoint.config().model, system);
        match stream_turn(endpoint, request, cancel.clone(), events).await {
            TurnOutcome::EndTurn(reply) => {
                turns += 1;
                conversation.push_assistant(reply);
                // A steering message queued during the stream becomes
                // a user message on the next request — the run
                // continues instead of finishing.
                if drain_steering(steering, conversation) {
                    continue;
                }
                emit(events, AgentEvent::Done);
                return;
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

/// How one streamed model turn ended.
enum TurnOutcome {
    /// The stream ended cleanly; carries the reply's concatenated text
    /// for the conversation.
    EndTurn(String),
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
/// provider error events and [`LlmError`]s into terminal `Error`s,
/// cancellation into [`TurnOutcome::Cancelled`]. Sends stop early if the
/// frontend's receiver is gone.
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
    while let Some(item) = stream.next().await {
        match item {
            Err(LlmError::Cancelled) => return TurnOutcome::Cancelled,
            Err(error) => {
                emit(events, AgentEvent::Error(format!("{error}")));
                return TurnOutcome::Ended;
            }
            Ok(event) => match extract(event) {
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
            },
        }
    }
    TurnOutcome::EndTurn(reply)
}

/// Send one event; `false` when the frontend's receiver is gone — the
/// worker then abandons the rest of the task rather than streaming
/// into the void.
fn emit(events: &Sender<AgentEvent>, event: AgentEvent) -> bool {
    events.send(event).is_ok()
}
