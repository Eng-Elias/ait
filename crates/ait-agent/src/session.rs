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
//! `Settings::resolve_model(&request.profile, ModelRole::Coding,
//! store)` — the task's armed execution profile picks the stance
//! (§5.7, Spec R07), the `coding_model` → `base_model` fallback lives
//! inside `resolve_model` (§4.2) — so a hot reload that swapped
//! `Settings` app-side takes effect on the next task with no plumbing
//! here. It
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
//! items → re-request. The **permission gate** (§5.7, Spec R07):
//! every tool call evaluates the task's active execution profile
//! through the pure `permissions::evaluate` seam — `Allow` runs the
//! call with its card already `Running`,
//! `Ask` suspends the loop on an [`AgentEvent::ApprovalRequest`] —
//! carrying the call and a renderable argument summary — until the
//! frontend's [`AgentSession::approve`] answers with an
//! [`ApprovalVerdict`]: *allow once*, *allow this tool for the rest
//! of the task*, or *deny* (the denial text becomes the tool
//! result), and `Deny` refuses the call outright — a
//! `"denied by execution profile <name>"` text becomes the tool
//! result and the card renders `Denied`. `agent_decides` tools
//! declare a required `safety` argument the model fills in per call
//! (R07-T2) — `"safe"` allows, `"unsafe"`/absent/malformed asks.
//! Prompts never time out
//! (us-9); Esc still cancels the whole task, and
//! `ask_user_question` at a non-`never` level keeps its exemption
//! by design — its own card is the ask (us-10): a call to it
//! suspends on an [`AgentEvent::AskRequest`] — carrying the
//! question and its choices-or-free-text — until the frontend's
//! [`AgentSession::answer`] replies, and the answer text becomes
//! the call's tool result. `update_plan` (Spec R08 us-16) is exempt
//! outright on the same logic: §5.7 maps no field to it, so every
//! stance allows and its [`AgentEvent::PlanUpdated`] snapshot IS the
//! effect. A profile that resolves to nothing —
//! missing, renamed or zero declared — evaluates to the Round 6
//! interim ask-everything rule: the fail-safe asks rather than
//! silently loosening. Round 7 swapped the policy, not the
//! plumbing (Spec R06 us-37).
//!
//! Calls in one turn execute sequentially in model order (us-33),
//! each gated. Every tool result passes the task's [`Redactor`]
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

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ait_config::{ExecutionProfile, McpServerOrigin, ModelRole, OsKeyring, SecretStore, Settings};
use ait_llm::{Endpoint, EndpointConfig, EndpointRequest, EndpointStream, LlmError};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::agent_context::{assemble_system_prompt, find_agents_md, AgentContext};
use crate::conversation::Conversation;
use crate::endpoint_bridge::{extract, llm_schema, reported_tokens, slot, Extracted};
use crate::executor::{CommandExecutor, ShellBridge};
use crate::files::{ToolCwd, ToolOutput};
use crate::mcp::McpPool;
use crate::permissions::{self, Verdict};
use crate::pty::AgentPty;
use crate::redact::Redactor;
use crate::skills::SkillsCatalog;
use crate::summarize;
use crate::tool_call::{CallAccumulator, ToolCall, ToolStatus};
use crate::tools;
use crate::transcript::{PlanTodo, TranscriptItem, TranscriptMirror};

/// The step budget (Spec R06, glossary): at most this many model turns
/// per task. Configurability is a later round's decision; the constant
/// names the termination condition the agent loop checks before every
/// request — tool-call turns included.
pub const STEP_BUDGET: usize = 50;

/// The context-limit fraction that triggers summarization (Spec R08
/// §5.8, ticket R08-T7): when the estimate — provider `usage` where
/// the stream reports it, else serialized-item chars over four —
/// reaches this share of the effective limit (the active profile's
/// `context_window_limit`, else the model's `context_window`), the
/// oldest half of the transcript is rewritten into a `summary`
/// marker by one extra call to the task's coding endpoint.
/// Configurability is a later round's decision, on the
/// [`STEP_BUDGET`] precedent.
pub const SUMMARIZE_AT: f64 = 0.8;

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
    /// The active execution profile's name (§5.7 / Spec R07 us-26):
    /// the stance the permission engine and the `coding_model` role
    /// both resolve against for THIS task. Snapshotted beside
    /// `settings`, so a frontend-side switch applies to the next task
    /// and can never rewrite an in-flight one's consent. A name that
    /// resolves to nothing arms the engine's ask-everything fail-safe.
    pub profile: String,
    /// The task text — sent verbatim, never redacted (the
    /// typed-request rule, §5.4 us-19).
    pub task: String,
    /// The conversation's accumulated transcript — the continuity
    /// record (Spec R08 §5.8): the worker replays it into the
    /// resolved schema's IR ([`Conversation::from_transcript`])
    /// before the task lands, so a second task in a tab continues
    /// where the first left off — the R06 per-task fresh context is
    /// superseded. An empty vec is exactly that old fresh
    /// conversation; a resumed conversation's stored items ride the
    /// same field — the worker never touches disk either way.
    pub prior: Vec<TranscriptItem>,
    /// The task's §5.10.2 skills snapshot (Spec R10-T5): the caller
    /// scans both skills roots at submit time and the catalog rides
    /// the request verbatim — the system prompt's "Available skills"
    /// section, `invoke_skill`'s answers and the skill directories
    /// the `ToolCwd` adds to its read roots all come from THIS
    /// value, so task-time behavior is deterministic no matter what
    /// changes on disk mid-run (live watching is R10-T6's).
    pub skills: SkillsCatalog,
    /// The terminal-side context — cwd, shell, OS and recent output
    /// lines; assembled into the system prompt by
    /// [`assemble_system_prompt`]. `cwd` also binds the file tools:
    /// every path the agent reads or searches stays under it —
    /// plus under any `directory_allowlist` root the active profile
    /// declares (§5.7, R07-T4) and any loaded skill's directory
    /// (§5.10.2).
    pub context: AgentContext,
    /// The shell binding `run_command` executes against (Spec R06-T4):
    /// the frontend hands each task the `ShellBridge` its `Tab`
    /// services. `None` means the task has no live shell — tests and
    /// detached conversations — and `run_command` answers a clean
    /// tool error rather than failing the task.
    pub bridge: Option<Box<dyn ShellBridge>>,
}

/// The user's answer to an [`AgentEvent::ApprovalRequest`] — the
/// ask verdict's whole vocabulary (Spec R06's three-way card).
/// The §5.7 engine decides when the card appears; the card's
/// verdicts and the channel shape survive it.
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
    /// A call proceeded without a gate wait — the engine allowed it
    /// (`always_allow`, an allow-for-task repeat) or refused it (a
    /// profile `Deny` opens this card then flips it `Denied` with
    /// the next [`ToolUpdate`](AgentEvent::ToolUpdate)). The panel
    /// opens its tool card already `Running`.
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
        /// The call's full result text — `Some` on the terminal
        /// updates (`Done`/`Failed`/`Denied`), carrying verbatim what
        /// entered the model's conversation (already redacted): the
        /// transcript mirror (Spec R08) persists it while the card
        /// keeps `detail`'s clipped preview. `None` on `Running`,
        /// a card transition rather than a result.
        result: Option<String>,
    },
    /// An `update_plan` call's snapshot (Spec R08 us-13/14): the
    /// whole TODO list — replace semantics, never a diff — carried
    /// in the transcript vocabulary's own [`PlanTodo`] shape. The
    /// panel swaps its pinned plan block for this list; the store
    /// upserts it as the conversation's `plans` row; the transcript
    /// mirror records a `plan` item. An empty list clears the plan.
    PlanUpdated {
        /// The call's full TODO list, in model order.
        todos: Vec<PlanTodo>,
    },
    /// The worker rewrote the conversation's oldest half into a
    /// summary marker (Spec R08 us-18/22): the estimate crossed
    /// [`SUMMARIZE_AT`] of the effective limit, one summarize call
    /// to the task's coding endpoint produced `text` (the current
    /// plan rides verbatim inside it), and the covered span was
    /// replaced in the live conversation. Non-terminal — the task
    /// continues on the compacted context. The transcript mirror
    /// records the `summary` item; the panel shows a "conversation
    /// summarized" entry so the rewrite is never silent.
    Summarized {
        /// The persisted transcript's seq domain: how many leading
        /// items (`0..covers_seq`) the summary replaces. Counted on
        /// the worker's own [`TranscriptMirror`](crate::TranscriptMirror),
        /// which folds the same events by the same rule as the
        /// frontend's — the domains agree unless a steering message
        /// raced a tool-turn boundary (see the mirror's doc), and
        /// replay tolerates either edge.
        covers_seq: u64,
        /// The marker's stored text — the model's summary with the
        /// current plan appended verbatim.
        text: String,
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

/// A parked wait's outcome — the approve card's verdict wait and
/// the ask card's answer wait share one shape: what the worker does
/// next once a reply lands (or never can).
enum ReplyOutcome<T> {
    /// The frontend sent a reply for the awaited call.
    Replied(T),
    /// Esc: the cancellation token fired mid-wait.
    Cancelled,
    /// The reply channel closed — the handle is gone; reporting
    /// anything is pointless.
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

/// The event channel plus the worker's transcript mirror in one
/// wrapper: every emitted [`AgentEvent`] folds into the mirror by
/// the same rule the frontend's drain applies (R08-T3's mirror,
/// shared as [`TranscriptMirror`]), so the worker's seq domain is
/// the persisted transcript's — which is what makes a
/// [`AgentEvent::Summarized`]'s `covers_seq` count correctly. The
/// mirror lives behind a `RefCell` so the immutable `&EventSink`
/// every loop function already takes can fold as it sends.
struct EventSink<'a> {
    /// The frontend's channel.
    sender: &'a Sender<AgentEvent>,
    /// The worker's transcript mirror — seeded from
    /// `AgentRequest.prior`, grown send-side and per emitted event.
    mirror: RefCell<TranscriptMirror>,
}

impl EventSink<'_> {
    /// Fold `event` into the mirror, then send it — `false` when the
    /// frontend's receiver is gone (the [`emit`] rule verbatim).
    fn emit(&self, event: AgentEvent) -> bool {
        self.mirror.borrow_mut().apply(&event);
        self.sender.send(event).is_ok()
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
    // The agent-owned PTY `write_to_pty` drives (Spec R06-T7): lazy —
    // nothing spawns until a call reaches it — and shared across this
    // conversation's tasks so an interactive session outlives the task
    // that opened it. It drops when the worker exits, which is what
    // makes a closing tab kill the spawned child: `AgentSession`'s
    // Drop closes these channels and joins this thread.
    let mut agent_pty = AgentPty::new();
    // The §5.10.1 MCP pool (Spec R10-T3): like `agent_pty` it lives
    // on this worker across the session's tasks — `serve` reconciles
    // it against the merged mcp.json per task, so an unchanged
    // user-tier server keeps its connection between tasks while a
    // failed one retries. Every rmcp value is created and dropped
    // inside `runtime.block_on` (rmcp's child cleanup spawns a
    // task), which is why the pool drains inside the final
    // `block_on` below rather than dropping bare.
    let mut mcp_pool = McpPool::new();
    while let Ok(request) = requests.recv() {
        // Spec R09-T3: the agent-owned shell's `ssh()` reads the
        // task's armed `[ssh]` policy — refreshed per request so a
        // hot-reloaded settings file reaches the next spawn.
        agent_pty.arm_ssh(&request.settings.ssh);
        serve(
            &runtime,
            request,
            &steering,
            &verdicts,
            &answers,
            &events,
            &in_flight,
            store.as_ref(),
            &mut agent_pty,
            &mut mcp_pool,
        );
    }
    // The request channel closed — shut the pool down inside the
    // runtime so every child process dies cleanly before the
    // worker's thread (and its runtime) exits.
    runtime.block_on(mcp_pool.drain());
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
    agent_pty: &mut AgentPty,
    mcp_pool: &mut McpPool,
) {
    // The worker's transcript mirror (R08-T7): seeded from `prior`
    // verbatim — those items were folded when first recorded — then
    // grown send-side and per emitted event through the sink, so
    // summarization counts `covers_seq` in the persisted
    // transcript's seq domain. `events` shadows the sender for the
    // rest of the task; every emit folds then sends.
    let mut mirror = TranscriptMirror::default();
    for item in &request.prior {
        mirror.record(item.clone());
    }
    let events = EventSink {
        sender: events,
        mirror: RefCell::new(mirror),
    };
    // `resolve_model` takes `&(impl SecretStore + ?Sized)` — the boxed
    // store's `&dyn` reference flows straight through. The task's own
    // profile resolves the role (§4.2/§5.7): the selection the
    // frontend armed when it submitted, never a global.
    let resolved = match request
        .settings
        .resolve_model(&request.profile, ModelRole::Coding, store)
    {
        Ok(Some(resolved)) => resolved,
        Ok(None) => {
            events.emit(AgentEvent::Error(format!(
                "profile {:?} has no coding_model configured \
                     (and no base_model fallback) — set it in settings.toml",
                request.profile
            )));
            return;
        }
        Err(error) => {
            events.emit(AgentEvent::Error(format!("{error}")));
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
            events.emit(AgentEvent::Error(format!("{error}")));
            return;
        }
    };

    // §5.10.1 (Spec R10-T3): the per-task MCP reconcile — the merged
    // mcp.json for the task's cwd is re-read, unchanged user-tier
    // connections persist, dropped declarations shut down, and new
    // or previously-failed user servers connect eagerly (spawn +
    // handshake + enumeration under one connect timeout). The
    // catalog it returns feeds the system prompt's "MCP servers"
    // section; a malformed file or a failed connect degrades to the
    // catalog, never to a task error.
    let mcp_catalog = runtime.block_on(mcp_pool.reconcile(Path::new(&request.context.cwd)));
    // The task's §5.10.3 redactor (Spec R10): built once from the
    // request's `[redaction]` snapshot, applied at context assembly
    // and to every tool result — a settings hot-reload takes effect
    // on the next task with no plumbing here.
    let redactor = Redactor::new(&request.settings.redaction);
    let system = assemble_system_prompt(
        &request.context,
        find_agents_md(Path::new(&request.context.cwd)).as_deref(),
        &redactor,
        mcp_catalog.prompt_section().as_deref(),
        request.skills.prompt_section().as_deref(),
    );
    // The §5.7 stance the permission engine consults on every tool
    // call — the task's armed profile missing, renamed or never
    // declared resolves to `None`, and the engine's fail-safe asks
    // for every call (the Round 6 interim gate verbatim).
    let profile = request
        .settings
        .agents
        .execution_profiles
        .get(request.profile.as_str());
    // `declare` is profile-aware: the `agent_decides` tools of this
    // stance gain the required `safety` argument in their schemas
    // (R07-T2) — per task, so a profile change applies next task.
    // The conversation is the continuity unit (Spec R08): the
    // transcript on `prior` replays into THIS resolved schema's IR —
    // a mid-conversation endpoint switch strands nothing — then the
    // new task appends on top. An empty `prior` is the R06 fresh
    // conversation verbatim.
    let mut conversation =
        Conversation::from_transcript(endpoint.schema(), &request.prior, profile);
    events.mirror.borrow_mut().task(&request.task);
    conversation.push_user(request.task);
    // Steering that queued while the worker was idle joins this task's
    // conversation rather than being dropped.
    drain_steering(steering, &mut conversation, &mut events.mirror.borrow_mut());
    // The task's file tools bind to the session's cwd ∪ the active
    // profile's normalized `directory_allowlist` roots (§5.7,
    // R07-T4): a missing profile or an empty list confines to cwd
    // exactly (Spec R06's confinement rule). On a LOCAL task every
    // loaded skill's directory joins the union too (Spec R10 us-25)
    // — `read_files` reaches a skill's bundled files with no
    // allowlist editing; on a remote task they stay out, since the
    // read tools would look for them on the wrong machine (skills
    // are local-only this round). The union only widens what the
    // tools may touch — the engine's verdicts, cards and diffs are
    // unaffected.
    let mut allowlist: Vec<String> = profile
        .map(|profile| profile.directory_allowlist.clone())
        .unwrap_or_default();
    // §5.9 (Spec R09-T5/T6): the bridge's remote state is the task's
    // one read of where commands land — `Remote` confines the file
    // tools lexically (their commands run through the executor) and
    // `select_executor` picks the ControlMaster → in-band → detached
    // preference order.
    let remote = request
        .bridge
        .as_deref()
        .and_then(|bridge| bridge.remote_state().ok())
        .is_some_and(|state| matches!(state, crate::RemoteState::Remote { .. }));
    let cwd = if remote {
        ToolCwd::new_remote(&request.context.cwd, &allowlist)
    } else {
        allowlist.extend(request.skills.skill_dirs());
        ToolCwd::new(&request.context.cwd, &allowlist)
    };

    let token = CancellationToken::new();
    *slot(in_flight) = Some(token.clone());
    // `run_command`'s §5.9 executor (Spec R06-T4): `select_executor`
    // walks §5.9's preference order — a `Remote` + live ControlMaster
    // socket answers out-of-band, every other bridged session takes
    // the user-shell tiers, a bridgeless task the detached no-op.
    let mut executor = crate::select_executor(request.bridge, token.clone());
    // The summarization threshold's denominator (R08-T7, us-19):
    // the profile's `context_window_limit` when it is >0, else the
    // resolved model's `context_window`, else `None` — with no
    // limit configured nothing ever fires.
    let context_limit = summarize::effective_limit(
        profile
            .map(|profile| profile.context_window_limit)
            .unwrap_or(0),
        resolved.model.context_window,
    );
    runtime.block_on(run_loop(
        &endpoint,
        &mut conversation,
        &system,
        steering,
        verdicts,
        answers,
        &cwd,
        profile,
        &request.profile,
        context_limit,
        agent_pty,
        executor.as_mut(),
        mcp_pool,
        &request.skills,
        remote,
        &redactor,
        token,
        &events,
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
///
/// Before every model request the summarization check (Spec R08-T7):
/// when `context_limit` resolved to a real cap, the estimate —
/// the last stream's provider `usage` where one was reported, else
/// the serialized-item chars/4 heuristic — is measured against
/// [`SUMMARIZE_AT`]; crossing it fires one summarize call and the
/// oldest half of the transcript becomes a `summary` marker.
#[allow(clippy::too_many_arguments)]
async fn run_loop(
    endpoint: &Endpoint,
    conversation: &mut Conversation,
    system: &str,
    steering: &Receiver<String>,
    verdicts: &Receiver<(String, ApprovalVerdict)>,
    answers: &Receiver<(String, String)>,
    cwd: &ToolCwd,
    profile: Option<&ExecutionProfile>,
    profile_name: &str,
    context_limit: Option<usize>,
    agent_pty: &mut AgentPty,
    executor: &mut dyn CommandExecutor,
    mcp_pool: &mut McpPool,
    skills: &SkillsCatalog,
    remote: bool,
    redactor: &Redactor,
    cancel: CancellationToken,
    events: &EventSink<'_>,
) {
    let mut turns = 0usize;
    // `context_limit` goes `None` on a failed summarize call —
    // retrying every turn would just spend a doomed request each
    // loop, so one failure disables summarization for the task.
    let mut context_limit = context_limit;
    // The last turn's provider-reported token count — the
    // estimate's preferred source, cleared by every rewrite so the
    // compacted conversation measures against the heuristic until
    // the next stream reports.
    let mut reported: Option<u64> = None;
    // Tools the user allowed for the rest of the task — the gate's
    // per-task memory; per name, never blanket (us-7). *Allow this
    // tool for the rest of the task* always records; under
    // `ask_on_first_write` so does *allow once* — the first write's
    // consent IS the interactive session's (us-11/12). Fresh per
    // task, so a new task asks on first write again.
    let mut allowed_for_task: HashSet<String> = HashSet::new();
    loop {
        if turns >= STEP_BUDGET {
            events.emit(AgentEvent::BudgetReached);
            return;
        }
        if let Some(limit) = context_limit {
            let estimate = match reported {
                Some(tokens) => tokens as usize,
                None => conversation.estimated_tokens(),
            };
            if estimate as f64 >= limit as f64 * SUMMARIZE_AT {
                match summarize_context(endpoint, conversation, profile, &cancel, events).await {
                    SummarizeOutcome::Summarized => reported = None,
                    SummarizeOutcome::NoSpan => {}
                    SummarizeOutcome::Unavailable => context_limit = None,
                    SummarizeOutcome::Cancelled => {
                        events.emit(AgentEvent::Cancelled);
                        return;
                    }
                }
            }
        }
        let request = conversation.request(&endpoint.config().model, system);
        match stream_turn(endpoint, request, cancel.clone(), events).await {
            TurnOutcome::EndTurn { text, calls, usage } => {
                turns += 1;
                if let Some(tokens) = usage {
                    reported = Some(tokens);
                }
                if calls.is_empty() {
                    conversation.push_assistant(text);
                    // A steering message queued during the stream
                    // becomes a user message on the next request —
                    // the run continues instead of finishing.
                    if drain_steering(steering, conversation, &mut events.mirror.borrow_mut()) {
                        continue;
                    }
                    events.emit(AgentEvent::Done);
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
                        events.emit(AgentEvent::Cancelled);
                        return;
                    }
                    match gate_and_execute(
                        call,
                        profile,
                        profile_name,
                        &mut allowed_for_task,
                        verdicts,
                        answers,
                        cwd,
                        agent_pty,
                        executor,
                        mcp_pool,
                        skills,
                        remote,
                        redactor,
                        &cancel,
                        events,
                    )
                    .await
                    {
                        Some(result) => results.push(result),
                        None => return,
                    }
                }
                conversation.push_tool_results(&calls, &results);
                // Steering queued during the tool turn rides the next
                // request beside the results.
                drain_steering(steering, conversation, &mut events.mirror.borrow_mut());
            }
            TurnOutcome::Cancelled => {
                events.emit(AgentEvent::Cancelled);
                return;
            }
            // The `Error` event was already emitted inside
            // `stream_turn` (or the receiver is gone — nothing left
            // to report to).
            TurnOutcome::Ended => return,
        }
    }
}

/// How one summarize attempt ended — `run_loop` maps it onto
/// continue / disable / cancel.
enum SummarizeOutcome {
    /// The marker recorded, [`AgentEvent::Summarized`] emitted, the
    /// live conversation rebuilt around it.
    Summarized,
    /// No pair-safe span exists yet — nothing fired; a later check
    /// may still find one as the transcript grows.
    NoSpan,
    /// The summarize call failed (transport, stream, provider error
    /// or an empty reply) — summarization disables for the task
    /// rather than retry-failing every turn. The task itself is
    /// unaffected: the rewrite is housekeeping, never load-bearing.
    Unavailable,
    /// Esc fired mid-call.
    Cancelled,
}

/// One summarize call (Spec R08 us-18/20): pick the covered span in
/// the mirror — the persisted transcript's seq domain — render it,
/// ask the task's coding endpoint to compact it, then record the
/// marker and emit [`AgentEvent::Summarized`] (the emit IS the
/// mirror's record — the shared fold). The live conversation then
/// rebuilds by replaying the mirror exactly like a resume would, so
/// persisted and live state stay identical by construction.
async fn summarize_context(
    endpoint: &Endpoint,
    conversation: &mut Conversation,
    profile: Option<&ExecutionProfile>,
    cancel: &CancellationToken,
    events: &EventSink<'_>,
) -> SummarizeOutcome {
    let (request, covers_seq) = {
        let mirror = events.mirror.borrow();
        let items = mirror.items();
        match summarize::covered_span(items) {
            Some(cut) => {
                let prompt = summarize::prompt(&summarize::render(&items[..cut]));
                (
                    summarize::request(endpoint.schema(), &endpoint.config().model, prompt),
                    cut,
                )
            }
            None => return SummarizeOutcome::NoSpan,
        }
    };
    let mut stream: EndpointStream = match endpoint.stream(request, cancel.clone()).await {
        Ok(stream) => stream,
        Err(LlmError::Cancelled) => return SummarizeOutcome::Cancelled,
        Err(_) => return SummarizeOutcome::Unavailable,
    };
    // The reply collects silently — the panel's record of the
    // rewrite is the status entry `Summarized` renders, not a
    // pseudo-assistant text stream.
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        match item {
            Err(LlmError::Cancelled) => return SummarizeOutcome::Cancelled,
            Err(_) => return SummarizeOutcome::Unavailable,
            Ok(event) => match extract(event) {
                Extracted::Text(fragment) => text.push_str(&fragment),
                Extracted::ProviderError(_) => return SummarizeOutcome::Unavailable,
                Extracted::Other => {}
            },
        }
    }
    if text.trim().is_empty() {
        return SummarizeOutcome::Unavailable;
    }
    // The current plan rides verbatim inside the marker's text
    // (us-20) — read it BEFORE the emit's fold records the item.
    let text = {
        let mirror = events.mirror.borrow();
        summarize::carrying_plan(&text, summarize::current_plan(mirror.items()))
    };
    if !events.emit(AgentEvent::Summarized {
        covers_seq: covers_seq as u64,
        text,
    }) {
        // The receiver is gone — abandon like every emit path.
        return SummarizeOutcome::Cancelled;
    }
    *conversation =
        Conversation::from_transcript(endpoint.schema(), events.mirror.borrow().items(), profile);
    SummarizeOutcome::Summarized
}

/// The §5.7 permission gate plus one call's execution (Spec R07):
/// the pure `permissions::evaluate` seam resolves the call's
/// `Verdict` under the task's active profile — `Ask` parks on the
/// unchanged three-option approve card, `Allow` runs it with the
/// card already `Running`, `Deny` refuses it outright — then the
/// executor runs whatever survived. Returns the text the tool
/// result carries — the executor's (redacted) output, or a denial
/// text — and `None` when the run must end (cancelled, or the
/// frontend's channels are gone).
#[allow(clippy::too_many_arguments)] // the gate's consult-point state is genuinely this wide — the profile engine swaps the policy, not the plumbing
async fn gate_and_execute(
    call: &ToolCall,
    profile: Option<&ExecutionProfile>,
    profile_name: &str,
    allowed_for_task: &mut HashSet<String>,
    verdicts: &Receiver<(String, ApprovalVerdict)>,
    answers: &Receiver<(String, String)>,
    cwd: &ToolCwd,
    agent_pty: &mut AgentPty,
    executor: &mut dyn CommandExecutor,
    mcp_pool: &mut McpPool,
    skills: &SkillsCatalog,
    remote: bool,
    redactor: &Redactor,
    cancel: &CancellationToken,
    events: &EventSink<'_>,
) -> Option<String> {
    // The consult point Spec R06 built the engine swap for: the
    // verdict, not the plumbing, is what profiles change.
    let verdict = permissions::evaluate(profile, profile_name, call, allowed_for_task);
    // `ask_user_question` at a non-`never` level never gates and
    // never executes — its own card IS the ask (us-10): it suspends
    // on the answer channel and the reply becomes the call's tool
    // result. `never` falls through to the profile denial below.
    if call.name == tools::ASK_USER_QUESTION && !matches!(verdict, Verdict::Deny(_)) {
        return ask_and_answer(call, answers, cancel, events);
    }
    // `update_plan` is exempt the same way (Spec R08 us-16): §5.7
    // maps no profile field to it, so `verdict` is always `Allow`
    // here — and its own snapshot IS the effect. The divert emits
    // the ordinary ungated card plus `PlanUpdated`; nothing reaches
    // the executor.
    if call.name == tools::UPDATE_PLAN {
        return update_plan(call, events);
    }
    // `edit_files` plans BEFORE the card (Spec R06 us-22/23): the
    // plan's unified diff is the summary the user approves, and the
    // same plan is what an allow verdict writes — the rendered diff
    // is byte-for-byte the applied change. A planning error still
    // rides the card (the paths plus why) and replays as the tool
    // result so the model reads what went wrong. On a REMOTE task
    // there is no local plan to render — remote writes are out of
    // scope (Spec R09-T6) and `execute`'s honest refusal is both the
    // card summary and the tool result.
    let plan = match (call.name.as_str(), remote) {
        ("edit_files", false) => Some(tools::plan_edit(call, cwd)),
        _ => None,
    };
    let summary = match &plan {
        Some(Ok(plan)) => plan.summary(),
        Some(Err(error)) => match tools::summarize_args(call).as_str() {
            "" => format!("edit_files: {error}"),
            paths => format!("{paths}\nedit_files: {error}"),
        },
        // Spec R10 us-9: the card names the layer that declared the
        // server — a project `.ait/` entry asks the user to run code
        // the repository shipped, which reads very differently from
        // a server the user's own config declared.
        None => match mcp_origin(call, mcp_pool) {
            Some(McpServerOrigin::Project) => format!(
                "{}\n(declared by the project .ait/mcp.json)",
                tools::summarize_args(call)
            ),
            _ => tools::summarize_args(call),
        },
    };
    // `Ask` parks on the three-option approve card; `Allow` and
    // `Deny` open an ungated card — `Deny`'s `Denied` update flips
    // it immediately after.
    let event = match &verdict {
        Verdict::Ask { .. } => AgentEvent::ApprovalRequest {
            call: call.clone(),
            summary,
        },
        Verdict::Allow | Verdict::Deny(_) => AgentEvent::ToolCall {
            call: call.clone(),
            summary,
        },
    };
    if !events.emit(event) {
        return None;
    }
    match verdict {
        // A profile denial mirrors a user denial (us-8): the card
        // renders `Denied` and the refusal text IS the tool result —
        // the model reads which profile refused it. The call never
        // runs: no `Running` update, nothing reaches the executor.
        Verdict::Deny(reason) => {
            events.emit(AgentEvent::ToolUpdate {
                call_id: call.id.clone(),
                status: ToolStatus::Denied,
                detail: Some(reason.clone()),
                result: Some(reason.clone()),
            });
            return Some(reason);
        }
        Verdict::Ask { first_write } => {
            match await_reply(verdicts, &call.id, cancel) {
                // `ask_on_first_write` (us-11/12): under it *allow
                // once* is the interactive session's consent — the
                // first write's allow covers the tool for the rest
                // of the task, recorded in the same per-tool memory
                // *allow for task* uses.
                ReplyOutcome::Replied(ApprovalVerdict::AllowOnce) => {
                    if first_write {
                        allowed_for_task.insert(call.name.clone());
                    }
                }
                ReplyOutcome::Replied(ApprovalVerdict::AllowForTask) => {
                    allowed_for_task.insert(call.name.clone());
                }
                ReplyOutcome::Replied(ApprovalVerdict::Deny(mut reason)) => {
                    if reason.is_empty() {
                        reason = DEFAULT_DENIAL.to_string();
                    }
                    events.emit(AgentEvent::ToolUpdate {
                        call_id: call.id.clone(),
                        status: ToolStatus::Denied,
                        detail: Some(reason.clone()),
                        result: Some(reason.clone()),
                    });
                    // The denial text IS the tool result (us-8) — the
                    // model reads it and picks a different approach.
                    return Some(reason);
                }
                ReplyOutcome::Cancelled => {
                    events.emit(AgentEvent::Cancelled);
                    return None;
                }
                ReplyOutcome::Gone => return None,
            }
            if !events.emit(AgentEvent::ToolUpdate {
                call_id: call.id.clone(),
                status: ToolStatus::Running,
                detail: None,
                result: None,
            }) {
                return None;
            }
        }
        Verdict::Allow => {}
    }
    // Apply the plan the card showed (edit_files); MCP calls ride the
    // session's pool — local to ait's own process even on a remote
    // task (Spec R10 us-16: MCP is not shell execution, the §5.9
    // executor tiers never see it); `invoke_skill` answers from the
    // request's catalog snapshot — a local read, no executor (Spec
    // R10 us-24); every other call dispatches through the executor.
    let output = match plan {
        Some(Ok(plan)) => plan.apply(),
        Some(Err(error)) => ToolOutput::error(error),
        None => match call.name.as_str() {
            tools::CALL_MCP_TOOL | tools::READ_MCP_RESOURCE => mcp_pool.execute(call).await,
            tools::INVOKE_SKILL => skills.invoke_call(call),
            _ => tools::execute(call, cwd, agent_pty, executor, remote, cancel),
        },
    };
    // A run cut short by Esc reports its own tool error — but the
    // task is over: emit `Cancelled` rather than re-entering the loop
    // for one more doomed request.
    if cancel.is_cancelled() {
        events.emit(AgentEvent::Cancelled);
        return None;
    }
    // §5.10.3, extended to everything the agent touches: the result
    // is redacted before it enters the conversation — and the card's
    // preview comes from the same redacted text.
    let text = redactor.redact(&output.text);
    events.emit(AgentEvent::ToolUpdate {
        call_id: call.id.clone(),
        status: if output.is_error {
            ToolStatus::Failed
        } else {
            ToolStatus::Done
        },
        detail: Some(card_detail(&call.name, &text)),
        result: Some(text.clone()),
    });
    Some(text)
}

/// The declaring layer of an MCP call's named server (Spec R10
/// us-9's card annotation) — `None` for non-MCP calls, undeclared
/// servers and unparseable arguments alike.
fn mcp_origin(call: &ToolCall, pool: &McpPool) -> Option<McpServerOrigin> {
    pool.origin_of(&tools::mcp_target(call)?.server)
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
    events: &EventSink<'_>,
) -> Option<String> {
    let (question, choices) = match tools::parse_question(call) {
        Ok(parsed) => parsed,
        Err(error) => {
            if !events.emit(AgentEvent::ToolCall {
                call: call.clone(),
                summary: tools::summarize_args(call),
            }) {
                return None;
            }
            events.emit(AgentEvent::ToolUpdate {
                call_id: call.id.clone(),
                status: ToolStatus::Failed,
                detail: Some(error.clone()),
                result: Some(error.clone()),
            });
            return Some(error);
        }
    };
    if !events.emit(AgentEvent::AskRequest {
        call: call.clone(),
        question,
        choices,
    }) {
        return None;
    }
    match await_reply(answers, &call.id, cancel) {
        ReplyOutcome::Replied(answer) => {
            // The reply IS the call's tool result — the transcript
            // records it as the send-side `answer` item (the
            // frontend records at send time; the worker at the
            // reply's arrival, which is the same position: no event
            // can interpose while the wait is parked).
            events.mirror.borrow_mut().answer(&answer);
            events.emit(AgentEvent::ToolUpdate {
                call_id: call.id.clone(),
                status: ToolStatus::Done,
                detail: Some(result_preview(&answer)),
                result: Some(answer.clone()),
            });
            Some(answer)
        }
        ReplyOutcome::Cancelled => {
            events.emit(AgentEvent::Cancelled);
            None
        }
        ReplyOutcome::Gone => None,
    }
}

/// The `update_plan` flow (Spec R08 us-13–16): gate-exempt — the
/// engine always `Allow`s it — so the call opens an ordinary ungated
/// card (us-15: its evolution stays auditable in scrollback), the
/// parsed TODO list rides [`AgentEvent::PlanUpdated`] to the panel's
/// pinned block and the store's `plans` upsert, and a fixed result
/// text re-enters the conversation like every tool result. A call
/// whose arguments carry no usable list is a tool error — model
/// input, never a wedge. Returns `None` only when the frontend's
/// channels are gone.
fn update_plan(call: &ToolCall, events: &EventSink<'_>) -> Option<String> {
    if !events.emit(AgentEvent::ToolCall {
        call: call.clone(),
        summary: tools::summarize_args(call),
    }) {
        return None;
    }
    let todos = match tools::parse_plan(call) {
        Ok(todos) => todos,
        Err(error) => {
            events.emit(AgentEvent::ToolUpdate {
                call_id: call.id.clone(),
                status: ToolStatus::Failed,
                detail: Some(error.clone()),
                result: Some(error.clone()),
            });
            return Some(error);
        }
    };
    // The model reads a confirmation, not the list back — the
    // snapshot itself just went out on the event.
    let text = match todos.len() {
        0 => String::from("plan cleared"),
        1 => String::from("plan updated — 1 todo"),
        n => format!("plan updated — {n} todos"),
    };
    if !events.emit(AgentEvent::PlanUpdated { todos }) {
        return None;
    }
    events.emit(AgentEvent::ToolUpdate {
        call_id: call.id.clone(),
        status: ToolStatus::Done,
        detail: Some(text.clone()),
        result: Some(text.clone()),
    });
    Some(text)
}

/// Park on a reply channel until the answer for `call_id` arrives —
/// indefinitely (us-9: no timeout into a default verdict or answer).
/// The [`GATE_POLL`] cadence exists only to notice the cancellation
/// token (Esc resolves a parked wait too) and stale replies —
/// answers to calls the loop is not waiting on — drop rather than
/// deciding the wrong call. One wait serves both cards: the
/// verdicts and the ask answers ride identical `(call_id, reply)`
/// channels.
fn await_reply<T>(
    replies: &Receiver<(String, T)>,
    call_id: &str,
    cancel: &CancellationToken,
) -> ReplyOutcome<T> {
    loop {
        if cancel.is_cancelled() {
            return ReplyOutcome::Cancelled;
        }
        match replies.recv_timeout(GATE_POLL) {
            Ok((id, reply)) if id == call_id => return ReplyOutcome::Replied(reply),
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return ReplyOutcome::Gone,
        }
    }
}

/// A card's `detail` per tool: `write_to_pty` previews the output
/// *tail* — its header already shows the bytes sent, so the card
/// reads input → latest output (Spec R06 us-20). Everything else
/// previews the first line.
fn card_detail(name: &str, text: &str) -> String {
    if name == "write_to_pty" {
        tail_preview(text)
    } else {
        result_preview(text)
    }
}

/// A card `detail`'s char cap — the last non-empty line clips from
/// the tail, the first line from the head, both to this width.
const PREVIEW_CHARS: usize = 120;

/// The last non-empty line of `text`, clipped to the preview width
/// from the tail — `write_to_pty`'s card detail.
fn tail_preview(text: &str) -> String {
    let last = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    if last.chars().count() > PREVIEW_CHARS {
        let tail: String = last
            .chars()
            .skip(last.chars().count() - PREVIEW_CHARS)
            .collect();
        format!("…{tail}")
    } else {
        last.to_string()
    }
}

/// A card's `detail`: the first line of the (already redacted) tool
/// result, clipped — enough to recognize what ran without turning
/// the scrollback into the output itself. The `ait` frontend's
/// resume rebuild clips stored `tool_result`s through this same fn.
pub fn result_preview(text: &str) -> String {
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
    /// signal is ≥1 accumulated call, not a `Finish` reason), plus
    /// the provider-reported token count `Finish` carried, if any —
    /// the summarization estimate's preferred source (R08-T7).
    EndTurn {
        /// The turn's concatenated reply text.
        text: String,
        /// The turn's accumulated tool calls.
        calls: Vec<ToolCall>,
        /// Provider `usage` from `Finish`, normalized to the turn's
        /// total tokens; `None` when the stream reported none.
        usage: Option<u64>,
    },
    /// The cancellation token fired (in-flight or pre-flight).
    Cancelled,
    /// Terminal: an `Error` was emitted, or the frontend's receiver is
    /// gone and reporting is pointless.
    Ended,
}

/// Drain queued steering messages into `conversation` as user
/// messages and into the mirror as `steering` items; `true` when at
/// least one was appended. The mirror records at drain time while
/// the frontend records at send time — the one ordering divergence
/// [`AgentEvent::Summarized`]'s `covers_seq` doc calls out.
fn drain_steering(
    steering: &Receiver<String>,
    conversation: &mut Conversation,
    mirror: &mut TranscriptMirror,
) -> bool {
    let mut drained = false;
    while let Ok(message) = steering.try_recv() {
        mirror.steering(&message);
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
    events: &EventSink<'_>,
) -> TurnOutcome {
    let mut stream: EndpointStream = match endpoint.stream(request, cancel).await {
        Ok(stream) => stream,
        Err(LlmError::Cancelled) => return TurnOutcome::Cancelled,
        Err(error) => {
            events.emit(AgentEvent::Error(format!("{error}")));
            return TurnOutcome::Ended;
        }
    };
    let mut reply = String::new();
    let mut accumulator = CallAccumulator::new();
    // The provider-reported token count on `Finish` — the
    // summarization estimate's preferred source (R08-T7); the last
    // report wins.
    let mut usage = None;
    while let Some(item) = stream.next().await {
        match item {
            Err(LlmError::Cancelled) => return TurnOutcome::Cancelled,
            Err(error) => {
                events.emit(AgentEvent::Error(format!("{error}")));
                return TurnOutcome::Ended;
            }
            Ok(event) => {
                if let Some(tokens) = reported_tokens(&event) {
                    usage = Some(tokens);
                }
                accumulator.push(&event);
                match extract(event) {
                    Extracted::Text(text) => {
                        reply.push_str(&text);
                        if !events.emit(AgentEvent::TextDelta(text)) {
                            return TurnOutcome::Ended;
                        }
                    }
                    Extracted::ProviderError(message) => {
                        events.emit(AgentEvent::Error(message));
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
        usage,
    }
}

/// Send one event; `false` when the frontend's receiver is gone — the
/// worker then abandons the rest of the task rather than streaming
/// into the void.
fn emit(events: &Sender<AgentEvent>, event: AgentEvent) -> bool {
    events.send(event).is_ok()
}
