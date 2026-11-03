//! The `ShellBridge` a `Tab` implements for `run_command` (Spec R06,
//! issue #87 — the TUI "implements the shell bridge", standard 9).
//!
//! The shape is a channel proxy, not a trait object over `Tab`
//! itself: the agent worker runs on its own thread while `Tab` lives
//! on the app loop's — so [`TabBridge`] holds a `Sender` of
//! [`BridgeRequest`]s, and [`TabManager::drain_events`] answers them
//! with `&mut Tab` on the next tick (the ~8 ms input poll bounds the
//! latency, the same cadence `drain_agent_events` rides). Every
//! method is a request the service answers; every failure mode is a
//! [`BridgeError`], never a wedge — a dropped receiver means the tab
//! (or app) is gone and reports `Gone`.
//!
//! The `Integrated` tier consumes only [`TabBridge::run_in_band`],
//! which delegates to [`Tab::run_in_shell`] — the R05 seam's
//! declared first consumer: the request line `__ait_run <id> <hex>`
//! stays visible in the grid (us-23), the `7701` frames resolve
//! through `InBandExec`'s shared `RunAccumulator` in the same drain,
//! and the finished `RunResult`'s `Receiver` is handed back to the
//! worker. `send_bytes`, `subscribe`, `grid_lines` and `output_tail`
//! are the typed tiers' surface (R06-T5): subscribers get decoded
//! `7700`-series events, `OSC 133` marks — published with their
//! absolute line, the coordinate `grid_lines` resolves — and the
//! exit, forwarded from `drain_events` where they are already
//! observed.

use std::sync::mpsc::{channel, Receiver, Sender};

use ait_agent::{BridgeError, BridgeEvent, ShellBridge, ShellIntegration};
use ait_shell::RunResult;
use ait_term::PromptMarkKind;

use crate::inband::InBandError;
use crate::integration::IntegrationState;

/// The bridge's end of the [`pair`]: a `ShellBridge` implementor the
/// `AgentRequest` carries to the agent worker. `Clone` because a
/// `Sender` clones — every clone reaches the same service queue.
#[derive(Clone)]
pub(crate) struct TabBridge {
    /// The service queue [`TabManager::drain_events`] answers.
    requests: Sender<BridgeRequest>,
}

impl ShellBridge for TabBridge {
    fn integration_state(&self) -> Result<ShellIntegration, BridgeError> {
        self.ask(BridgeRequest::State)
    }

    fn run_in_band(&self, command: &[u8]) -> Result<Receiver<RunResult>, BridgeError> {
        self.ask(|reply| BridgeRequest::RunInBand {
            command: command.to_vec(),
            reply,
        })?
    }

    fn send_bytes(&self, bytes: &[u8]) -> Result<(), BridgeError> {
        self.ask(|reply| BridgeRequest::SendBytes {
            bytes: bytes.to_vec(),
            reply,
        })?
    }

    fn subscribe(&self) -> Result<Receiver<BridgeEvent>, BridgeError> {
        self.ask(BridgeRequest::Subscribe)
    }

    fn grid_lines(&self, start: i32, end: i32) -> Result<Vec<String>, BridgeError> {
        self.ask(|reply| BridgeRequest::GridLines { start, end, reply })
    }

    fn output_tail(&self) -> Result<Vec<u8>, BridgeError> {
        self.ask(BridgeRequest::OutputTail)
    }
}

impl TabBridge {
    /// The request→reply round trip every bridge method performs:
    /// post the request, park on its reply channel — the frontend
    /// answers on the next drain tick; a dropped service side means
    /// the tab (or app) is gone.
    fn ask<T: Send + 'static>(
        &self,
        build: impl FnOnce(Sender<T>) -> BridgeRequest,
    ) -> Result<T, BridgeError> {
        let (reply, answer) = channel();
        self.requests
            .send(build(reply))
            .map_err(|_| BridgeError::Gone)?;
        answer.recv().map_err(|_| BridgeError::Gone)
    }
}

/// One queued request the owning `Tab` answers inside
/// [`TabManager::drain_events`] — the only thread allowed to touch
/// `Tab` state. Replies are plain channels: cheap, `Send`, and the
/// answer's payload (up to and including a pending run's `Receiver`)
/// moves straight to the worker.
pub(crate) enum BridgeRequest {
    /// `integration_state` — answers with the tab's live state.
    State(Sender<ShellIntegration>),
    /// `send_bytes` — write to the session's PTY; `SessionExited`
    /// refuses before a byte lands.
    SendBytes {
        /// The raw bytes to write.
        bytes: Vec<u8>,
        /// The write's outcome.
        reply: Sender<Result<(), BridgeError>>,
    },
    /// `run_in_band` — `Tab::run_in_shell`; the reply carries the
    /// pending run's result channel straight to the worker (or the
    /// refusal: not integrated, exited, write failed).
    RunInBand {
        /// The shell command, verbatim.
        command: Vec<u8>,
        /// The pending `RunResult`'s receiver, or the refusal.
        reply: Sender<Result<Receiver<RunResult>, BridgeError>>,
    },
    /// `subscribe` — install an event sender; the reply carries the
    /// subscription's receiver.
    Subscribe(Sender<Receiver<BridgeEvent>>),
    /// `grid_lines` — row text in the marks' absolute line space
    /// (`PromptMark::line`'s coordinates): the service subtracts
    /// `dropped_lines` and the live scrollback depth to land on
    /// `GridView::line` rows.
    GridLines {
        /// First absolute line.
        start: i32,
        /// One past the last absolute line.
        end: i32,
        /// The rows' text, in order, absent edges clamped away.
        reply: Sender<Vec<String>>,
    },
    /// `output_tail` — the session's raw output tail.
    OutputTail(Sender<Vec<u8>>),
}

/// The service side of a [`TabBridge`] — owned by its `Tab`, pumped
/// by [`TabManager::drain_events`]. Holds the request queue plus the
/// subscriber senders the drain forwards decoded events to.
pub(crate) struct BridgeService {
    /// The queue `drain_events` answers on each tick.
    requests: Receiver<BridgeRequest>,
    /// Live `subscribe` senders — dead ones (a dropped receiver, a
    /// finished tier) are pruned on the next published event.
    subscribers: Vec<Sender<BridgeEvent>>,
    /// A handle factory — `Tab::agent_key` clones it into each
    /// submitted `AgentRequest`.
    handle: TabBridge,
}

/// The two ends of a fresh bridge: the [`ShellBridge`] clone factory
/// the tab hands to `AgentRequest`s, and the service it drains.
pub(crate) fn pair() -> (TabBridge, BridgeService) {
    let (requests_tx, requests_rx) = channel();
    let handle = TabBridge {
        requests: requests_tx,
    };
    let service = BridgeService {
        requests: requests_rx,
        subscribers: Vec::new(),
        handle: handle.clone(),
    };
    (handle, service)
}

impl BridgeService {
    /// A fresh [`ShellBridge`] handle for this service — every clone
    /// reaches the same queue.
    pub(crate) fn handle(&self) -> TabBridge {
        self.handle.clone()
    }

    /// The next queued request, if any — `drain_events`' pump.
    pub(crate) fn try_recv(&self) -> Option<BridgeRequest> {
        self.requests.try_recv().ok()
    }

    /// Install a subscription: a fresh channel whose sender the drain
    /// forwards decoded events to.
    pub(crate) fn subscribe(&mut self) -> Receiver<BridgeEvent> {
        let (tx, rx) = channel();
        self.subscribers.push(tx);
        rx
    }

    /// Forward one decoded observation to every live subscriber,
    /// pruning senders whose receivers went away (a finished
    /// subscription is the norm — tiers drop their receivers).
    pub(crate) fn publish(&mut self, event: BridgeEvent) {
        self.subscribers
            .retain(|sender| sender.send(event.clone()).is_ok());
    }
}

/// `ait`'s `IntegrationState` → `ait-agent`'s `ShellIntegration` —
/// the same three names the spec uses (§5.5); the mapping is a
/// pure rename because the frontend owns the state machine and the
/// agent crate owns the vocabulary it tiers on (standard 9).
impl From<IntegrationState> for ShellIntegration {
    fn from(state: IntegrationState) -> Self {
        match state {
            IntegrationState::Absent => ShellIntegration::Absent,
            IntegrationState::MarksOnly => ShellIntegration::MarksOnly,
            IntegrationState::Integrated => ShellIntegration::Integrated,
        }
    }
}

/// `InBandExec`'s refusal vocabulary → the bridge's — the same
/// refusals, re-languaged for the agent side. `IntegrationState`
/// maps through [`From<IntegrationState>`].
impl From<InBandError> for BridgeError {
    fn from(error: InBandError) -> Self {
        match error {
            InBandError::NotIntegrated(state) => BridgeError::NotIntegrated(state.into()),
            InBandError::SessionExited => BridgeError::SessionExited,
            InBandError::Write(error) => BridgeError::Write(error),
        }
    }
}

/// `OSC 133` kind → the letter `BridgeEvent::PromptMark` carries —
/// the mark vocabulary stays text so `ait-agent` never needs
/// `ait-term` (the same rule that keeps `ait-shell` term-free).
pub(crate) fn mark_letter(kind: PromptMarkKind) -> char {
    match kind {
        PromptMarkKind::PromptStart => 'A',
        PromptMarkKind::CommandStart => 'B',
        PromptMarkKind::OutputStart => 'C',
        PromptMarkKind::CommandEnd => 'D',
    }
}
