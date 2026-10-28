//! The `CommandSuggester` channel API (Spec R04, user stories 20–23,
//! 25; ADR-0004): the `ait` frontend sends a [`SuggestRequest`] on one
//! `mpsc` channel and drains [`SuggestEvent`]s on another during its
//! 8 ms tick — mirroring `ait-config`'s `SettingsWatcher` precedent.
//! Internally the suggester owns a tokio current-thread runtime on a
//! dedicated worker thread, so `tokio` never enters the `ait` binary's
//! dependency tree or its synchronous poll loop.
//!
//! Per request the worker resolves
//! `Settings::resolve_model("default", ModelRole::Command, store)` —
//! the `command_model` → `base_model` fallback lives inside
//! `resolve_model` (§4.2, us-21) — assembles the single-turn request
//! through [`assemble_request`], and streams via
//! `Endpoint::stream(request, CancellationToken)`. Every failure mode
//! (unconfigured model, resolution/transport/stream error, cancelled
//! request) becomes a [`SuggestEvent::Error`] for the status-line toast
//! (us-22, §6 degrade-gracefully); the worker never panics and never
//! wedges — a poisoned request answers `Error`, then the next queued
//! request is served.
//!
//! The `Delta` text is already sanitized: raw reply fragments pass
//! through [`ReplySanitizer`] *inside* the worker, so the never-execute
//! guarantee (us-4/5 — no `\n`/`\r` ever crosses the channel) is
//! enforced at this seam and cannot be bypassed by a caller.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use ait_config::{ModelRole, OsKeyring, SecretStore, Settings};
use ait_llm::{
    anthropic_messages, openai_chat_completions, openai_responses, Endpoint, EndpointConfig,
    EndpointRequest, EndpointSchema, EndpointStream, EndpointStreamEvent,
};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::{assemble_request, ReplySanitizer, SuggestContext};

/// The execution profile whose model roles command requests resolve
/// against (§4.2 — the only profile Round 4 wires; per-profile roles
/// are Round 7).
const PROFILE: &str = "default";

/// One `# `/`Alt+Enter` request — the value the frontend sends on the
/// suggester's channel. It carries the terminal-side context the model
/// may see (§5.4) plus the caller's current `Settings` snapshot: the
/// suggester never reads `settings.toml` itself, so a hot reload that
/// swapped `Settings` app-side (us-23) takes effect on the next request
/// without any plumbing here.
pub struct SuggestRequest {
    /// The caller's live `Settings` — resolved per request, so the
    /// latest hot-reloaded `command_model` applies immediately.
    pub settings: Settings,
    /// The tab's working directory (OSC 7 with a `/proc` fallback).
    pub cwd: String,
    /// The shell name — `$SHELL`'s basename, e.g. `bash` (us-17).
    pub shell: String,
    /// The OS name — `std::env::consts::OS`, e.g. `linux` (us-17).
    pub os: String,
    /// Recent logical output lines, oldest first. Only the last
    /// [`OUTPUT_LINE_BUDGET`](crate::OUTPUT_LINE_BUDGET) are sent and
    /// each is redacted at assembly (us-16, 18) — send the raw lines,
    /// redaction is [`assemble_request`]'s job.
    pub output_lines: Vec<String>,
    /// The typed request — sent verbatim, never redacted (us-19).
    pub request: String,
}

/// One outcome of a [`SuggestRequest`], drained with
/// [`Receiver::try_recv`] once per app-loop tick.
#[derive(Debug, PartialEq, Eq)]
pub enum SuggestEvent {
    /// Already-sanitized reply text to append to the input line —
    /// contains no `\n`/`\r`, so appending can never execute (us-4/5).
    /// Concatenating every `Delta` of a request yields the full
    /// `sanitize_reply` output.
    Delta(String),
    /// The reply finished cleanly. Terminal for the request.
    Done,
    /// Resolution or stream failure, rendered for a status toast
    /// (us-22): unconfigured model, endpoint down, provider error,
    /// cancelled request. Terminal for the request.
    Error(String),
}

/// [`CommandSuggester::suggest`] could not queue the request — the
/// worker thread is gone (the only failure mode, so the error is
/// payload-free; the frontend's toast needs nothing more).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuggestError;

impl std::fmt::Display for SuggestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("command suggester is unavailable")
    }
}

impl std::error::Error for SuggestError {}

/// The synchronous handle the frontend holds (us-25). One in-flight
/// request at a time is the frontend's contract; queued requests run in
/// order on the single worker.
pub struct CommandSuggester {
    requests: Option<Sender<SuggestRequest>>,
    events: Receiver<SuggestEvent>,
    /// The in-flight stream's cancellation token — shared so
    /// [`cancel`](CommandSuggester::cancel) (the frontend's Esc, T5)
    /// reaches into the worker's `block_on` from any thread.
    in_flight: Arc<Mutex<Option<CancellationToken>>>,
    thread: Option<JoinHandle<()>>,
}

impl CommandSuggester {
    /// Start the worker thread with the production secret store —
    /// [`OsKeyring`] (§4.1: `api_key_env` first, OS keyring second,
    /// no `Authorization` header when both miss).
    pub fn new() -> Self {
        Self::with_secret_store(OsKeyring)
    }

    /// [`new`] with an explicit [`SecretStore`] — the seam tests inject
    /// a stub through instead of touching a real OS keyring.
    pub fn with_secret_store(store: impl SecretStore + Send + 'static) -> Self {
        let (requests_tx, requests_rx) = mpsc::channel::<SuggestRequest>();
        let (events_tx, events_rx) = mpsc::channel::<SuggestEvent>();
        let in_flight = Arc::new(Mutex::new(None));
        let thread = thread::spawn({
            let in_flight = Arc::clone(&in_flight);
            move || worker(requests_rx, events_tx, in_flight, Box::new(store))
        });
        Self {
            requests: Some(requests_tx),
            events: events_rx,
            in_flight,
            thread: Some(thread),
        }
    }

    /// Queue `request` for streaming. Fails only if the worker is gone
    /// — a [`SuggestError`] for the status toast.
    pub fn suggest(&self, request: SuggestRequest) -> Result<(), SuggestError> {
        match &self.requests {
            Some(sender) => sender.send(request).map_err(|_| SuggestError),
            None => Err(SuggestError),
        }
    }

    /// The channel of suggestion outcomes — drain with
    /// [`Receiver::try_recv`] once per tick, or block on
    /// [`Receiver::recv_timeout`] in tests.
    pub fn events(&self) -> &Receiver<SuggestEvent> {
        &self.events
    }

    /// Cancel the in-flight request — the Esc path (us-8, T5). The
    /// worker's `Endpoint::stream` sees the token promptly, mid-stream
    /// and mid-backoff; a [`SuggestEvent::Error`] reporting the
    /// cancellation follows on the event channel. No-op while idle, and
    /// a queued-but-unstarted request still runs (the frontend gates
    /// new triggers on "no suggestion in flight", us-9/10).
    pub fn cancel(&self) {
        if let Some(token) = slot(&self.in_flight).as_ref() {
            token.cancel();
        }
    }
}

impl Default for CommandSuggester {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for CommandSuggester {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandSuggester")
            .field("events", &self.events)
            .finish_non_exhaustive()
    }
}

impl Drop for CommandSuggester {
    fn drop(&mut self) {
        // Cancel whatever is streaming so `block_on` unwinds promptly,
        // then close the request channel — the worker's `recv` ends and
        // the thread exits; join so no worker outlives its handle.
        self.cancel();
        self.requests.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Lock the shared cancellation-token slot, recovering a poisoned
/// guard — a poisoned `Option<CancellationToken>` is still sound to
/// read (the token may simply be stale).
fn slot(
    mutex: &Mutex<Option<CancellationToken>>,
) -> std::sync::MutexGuard<'_, Option<CancellationToken>> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The worker thread: own the current-thread runtime and serve
/// requests in order. A runtime that cannot be built degrades every
/// request to an `Error` event rather than wedging the frontend (§6).
fn worker(
    requests: Receiver<SuggestRequest>,
    events: Sender<SuggestEvent>,
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
                    SuggestEvent::Error(format!("failed to start the tokio runtime: {error}")),
                );
            }
            return;
        }
    };
    while let Ok(request) = requests.recv() {
        serve(&runtime, request, &events, &in_flight, store.as_ref());
    }
}

/// One request: resolve → assemble → stream → emit. Every failure maps
/// to a single terminal [`SuggestEvent::Error`]; `Done` is emitted only
/// when nothing failed.
fn serve(
    runtime: &tokio::runtime::Runtime,
    request: SuggestRequest,
    events: &Sender<SuggestEvent>,
    in_flight: &Mutex<Option<CancellationToken>>,
    store: &dyn SecretStore,
) {
    // `resolve_model` takes `&impl SecretStore` (Sized); wrap the
    // worker's boxed store so the dyn reference fits the bound.
    let resolved =
        match request
            .settings
            .resolve_model(PROFILE, ModelRole::Command, &StoreRef(store))
        {
            Ok(Some(resolved)) => resolved,
            Ok(None) => {
                emit(
                    events,
                    SuggestEvent::Error(format!(
                        "profile {PROFILE:?} has no command_model configured \
                     (and no base_model fallback) — set it in settings.toml"
                    )),
                );
                return;
            }
            Err(error) => {
                emit(events, SuggestEvent::Error(format!("{error}")));
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
            emit(events, SuggestEvent::Error(format!("{error}")));
            return;
        }
    };
    let context = SuggestContext {
        cwd: &request.cwd,
        shell: &request.shell,
        os: &request.os,
        output_lines: &request.output_lines,
        request: &request.request,
    };
    // `assemble_request` takes the endpoint's own schema + wire model,
    // so the returned IR can never trip `LlmError::SchemaMismatch`.
    let endpoint_request = assemble_request(&context, endpoint.schema(), &endpoint.config().model);

    let token = CancellationToken::new();
    *slot(in_flight) = Some(token.clone());
    runtime.block_on(stream_and_emit(&endpoint, endpoint_request, token, events));
    slot(in_flight).take();
}

/// Drive the stream and emit events: raw text fragments through
/// [`ReplySanitizer`] into `Delta`s, provider error events and
/// [`LlmError`](ait_llm::LlmError)s into terminal `Error`s, clean EOF
/// into `Done`. Sends stop early if the frontend's receiver is gone.
async fn stream_and_emit(
    endpoint: &Endpoint,
    request: EndpointRequest,
    cancel: CancellationToken,
    events: &Sender<SuggestEvent>,
) {
    let mut stream: EndpointStream = match endpoint.stream(request, cancel).await {
        Ok(stream) => stream,
        Err(error) => {
            emit(events, SuggestEvent::Error(format!("{error}")));
            return;
        }
    };
    let mut sanitizer = ReplySanitizer::new();
    let mut failed = false;
    while let Some(item) = stream.next().await {
        match item {
            Err(error) => {
                emit(events, SuggestEvent::Error(format!("{error}")));
                failed = true;
                break;
            }
            Ok(event) => match extract(event) {
                Extracted::Text(text) => {
                    let sanitized = sanitizer.push(&text);
                    if !sanitized.is_empty() && !emit(events, SuggestEvent::Delta(sanitized)) {
                        return;
                    }
                }
                Extracted::ProviderError(message) => {
                    emit(events, SuggestEvent::Error(message));
                    failed = true;
                    break;
                }
                Extracted::Other => {}
            },
        }
    }
    if !failed {
        let tail = sanitizer.finish();
        if !tail.is_empty() && !emit(events, SuggestEvent::Delta(tail)) {
            return;
        }
        emit(events, SuggestEvent::Done);
    }
}

/// A sized reference to the worker's `dyn SecretStore` —
/// `Settings::resolve_model` is generic over `impl SecretStore`, which
/// a `dyn` reference cannot fill directly.
struct StoreRef<'a>(&'a dyn SecretStore);

impl SecretStore for StoreRef<'_> {
    fn get(&self, service: &str, account: &str) -> Option<String> {
        self.0.get(service, account)
    }
}

/// Send one event; `false` when the frontend's receiver is gone — the
/// worker then abandons the rest of the request rather than streaming
/// into the void.
fn emit(events: &Sender<SuggestEvent>, event: SuggestEvent) -> bool {
    events.send(event).is_ok()
}

/// What a schema-tagged [`EndpointStreamEvent`] means to the command
/// path — text worth streaming, a provider-reported error worth
/// toasting, or anything else (`Finish`, `ToolCallDelta`) a one-shot
/// command request ignores.
enum Extracted {
    /// Assistant text — pushed through `ReplySanitizer`.
    Text(String),
    /// A provider-reported error event, rendered for the toast.
    ProviderError(String),
    /// Everything else.
    Other,
}

/// Flatten the three per-schema event IRs (they stay distinct, §5.3 —
/// the flattening happens here at the channel edge, not in the IRs).
fn extract(event: EndpointStreamEvent) -> Extracted {
    match event {
        EndpointStreamEvent::OpenAiChatCompletions(event) => match event {
            openai_chat_completions::StreamEvent::TextDelta { text } => Extracted::Text(text),
            openai_chat_completions::StreamEvent::Error { message, code } => {
                Extracted::ProviderError(provider_error(message, code))
            }
            _ => Extracted::Other,
        },
        EndpointStreamEvent::OpenAiResponses(event) => match event {
            openai_responses::StreamEvent::TextDelta { text } => Extracted::Text(text),
            openai_responses::StreamEvent::Error { message, code } => {
                Extracted::ProviderError(provider_error(message, code))
            }
            _ => Extracted::Other,
        },
        EndpointStreamEvent::AnthropicMessages(event) => match event {
            anthropic_messages::StreamEvent::TextDelta { text } => Extracted::Text(text),
            anthropic_messages::StreamEvent::Error { message, code } => {
                Extracted::ProviderError(provider_error(message, code))
            }
            _ => Extracted::Other,
        },
    }
}

/// A provider error event for the toast: the message, with the
/// machine-readable code appended when the endpoint sent one.
fn provider_error(message: String, code: Option<String>) -> String {
    match code {
        Some(code) => format!("{message} ({code})"),
        None => message,
    }
}

/// `ait-config`'s schema enum and `ait-llm`'s are deliberately separate
/// types (the llm crate owns its wire vocabulary); bridge them here —
/// the same seam `ait-llm`'s `chat` example uses.
fn llm_schema(schema: ait_config::EndpointSchema) -> EndpointSchema {
    match schema {
        ait_config::EndpointSchema::OpenAiChatCompletions => EndpointSchema::OpenAiChatCompletions,
        ait_config::EndpointSchema::OpenAiResponses => EndpointSchema::OpenAiResponses,
        ait_config::EndpointSchema::AnthropicMessages => EndpointSchema::AnthropicMessages,
    }
}
