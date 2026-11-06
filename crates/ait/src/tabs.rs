//! Tabs: multiple sessions, one viewport state each (Spec R02 tabs seam).
//!
//! A **tab** is one [`LiveSession`] plus its viewport state — scroll
//! offset, mouse selection, the cwd its shell last reported via OSC 7
//! (fallback: the cwd it was spawned in); see `GLOSSARY.md`.
//! [`TabManager`] owns the ordered list and the active index; `app`
//! drives it from the keymap (`Alt+T` new, `Alt+W` close,
//! `Ctrl+PageUp/PageDown` cycle, `Alt+1..9` jump).
//!
//! Transition rules:
//!
//! - a spawned tab becomes active;
//! - a new tab's `$SHELL` spawns in the active tab's tracked cwd
//!   (the launch cwd — where `ait` itself started — when none was ever
//!   reported);
//! - closing drops the active tab and activates whatever shifted into
//!   its slot (clamped to the last tab when the closed one was last);
//!   closing the LAST tab reports `false` so the caller exits `ait`
//!   rather than indexing an empty list (standard 6);
//! - an exited session KEEPS its tab: [`Tab::exit_status`] records the
//!   status, the caller drops input to it, and `Alt+W` removes it
//!   without confirmation;
//! - every transition away from the active tab (spawn, cycle, jump)
//!   drops its selection highlight — a range on an off-screen tab is
//!   stale state — while its scroll offset persists;
//! - every tab is drained and resized each pass — background grids stay
//!   current and their exits/OSC events surface.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use ait_agent::{AgentRequest, ApprovalVerdict, ProgramIndex};
use ait_config::Settings;
use ait_term::{
    Cursor, ExitStatus, GridView, LiveEvent, LiveSession, LiveSessionOptions, PromptMarkKind,
    SessionEvent, Size, SpawnError,
};
use crossterm::event::{KeyCode, KeyEvent};

use crate::agent_panel::{context_for, edit_input, AgentPanel};
use crate::blocks::{
    row_hash, seal_fingerprint, Block, BlockAssembler, BlockStatus, PersistedBlock,
};
use crate::config::mcp_error_text;
use crate::inband::{InBandError, InBandExec, RunRequest};
use crate::integration::{self, IntegrationState};
use crate::remote::RemoteContext;
use crate::render::session_size;
use crate::scroll::ScrollState;
use crate::selection::{extract, GridPoint, Selection, SelectionRange};
use crate::shell_bridge::{self, BridgeRequest, BridgeService};
use crate::skills::{skipped_text, LiveSkills};
use crate::status::Toast;
use crate::store::StoreHandle;
use crate::suggest::InputShadow;

/// Title shown when the resolved program has no usable basename — the
/// resolved program is `$SHELL` or `ait-term`'s `/bin/sh` fallback, so
/// this only guards pathological paths (e.g. `/`).
const FALLBACK_TITLE: &str = "sh";

/// How many prompt marks a tab retains — four arrive per shell
/// command (A/B/C/D), so this covers ~1000 commands of history; the
/// oldest drop off the front, same rule as scrollback.
pub(crate) const PROMPT_MARK_CAP: usize = 4096;

/// One OSC 133 mark recorded against the line it landed on (Spec R01
/// user story 14). `line` indexes the "scrollback then viewport" row
/// sequence — the same coordinates [`Tab::grid`]'s `scrollback_rows` +
/// `visible_rows` iterate — and stays valid as scrollback grows because
/// new history only ever appends below it. Two caveats for Round 5's
/// block assembly (the consumer): `line` is the cursor's row once the
/// feeding chunk has fully landed, so a mark mid-burst can sit a row
/// late; and once scrollback is full and evicts rows, indices recorded
/// before the eviction drift — block assembly maps `line` into an
/// absolute space on the eviction signal that IS observable
/// ([`crate::blocks`]). Round 5's block assembly reads these instead of
/// re-parsing output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptMark {
    /// Which boundary the mark delimits.
    pub kind: PromptMarkKind,
    /// The mark's payload, if present — e.g. the exit code on
    /// `OSC 133;D;0`.
    pub payload: Option<String>,
    /// The mark's line in scrollback-then-viewport coordinates.
    pub line: usize,
    /// The cursor's column when the mark landed — on `CommandStart` it
    /// is the command's first column, which is what lets the `B→C`
    /// echo slice skip the prompt as the `command` fallback (Spec R05).
    pub column: usize,
    /// When the drain recorded the mark — block duration is
    /// `D.at − C.at` (falling back to `B.at` when `C` never arrives).
    pub at: Instant,
    /// The wall clock captured beside `at` (Spec R08): `Instant` can
    /// never convert to `SystemTime`, so the persisted `blocks` row's
    /// `started_at`/`ended_at` bounds come from the marks' paired
    /// wall times, anchored at seal.
    pub wall: SystemTime,
}

/// One tab: a [`LiveSession`] and the viewport state that belongs to it.
///
/// The session is public — the frontend legitimately drives its whole
/// API (keys, bytes, damage) — but the scroll offset and selection are
/// owned data: callers go through the methods so the grid-math they
/// need (`scrollback` bounds, page size) can't drift out of sync.
pub struct Tab {
    /// The tab's stable identity — a [`TabManager`] counter handed out
    /// at spawn and never reused. It is the locator in-flight
    /// suggestion pins re-resolve through (Spec R04 user story 14):
    /// neither the list index (shifts on every close) nor the child
    /// pid (`None` on pid-less platforms, recycled by the OS) can be
    /// trusted to keep pointing at the same session.
    pub(crate) id: u64,
    /// The live PTY session this tab drives.
    pub session: LiveSession,
    /// How far above the live viewport this tab's view sits.
    /// `pub(crate)`: tests reach the raw state to stage offsets the
    /// clamping methods can't produce; production callers use the
    /// `scroll_*`/`page_*`/`snap_to_bottom` methods.
    pub(crate) scroll: ScrollState,
    /// This tab's mouse-selection state — same `pub(crate)` test seam;
    /// production callers use the `*_selection` methods.
    pub(crate) selection: Selection,
    /// The shadow of this tab's printable input since the last line
    /// submission — what the `# `+Enter and `Alt+Enter` triggers verify
    /// against the grid's echoed line (Spec R04, user story 29). Same
    /// `pub(crate)` test seam; production drives it through
    /// `crate::suggest` from the keypress path.
    pub(crate) shadow: InputShadow,
    /// The §5.10.4 program index — this session's shell builtins
    /// merged with the executable basenames on `$PATH`, harvested ONCE
    /// at spawn (Spec R11 us-15): the classifier's per-Enter check is
    /// a set lookup, never a `$PATH` walk. Keyed on the same program
    /// basename the title resolves. Same `pub(crate)` test seam as
    /// `shadow` — tests stage `ProgramIndex::with_path_entries`.
    pub(crate) program_index: ProgramIndex,
    /// The cwd the shell last reported via OSC 7 — the spawn cwd until
    /// then. It is what a new tab inherits.
    cwd: PathBuf,
    /// Whether the shell has EVER reported a cwd via OSC 7 — when it
    /// hasn't, `cwd` is still the spawn directory and
    /// `/proc/<pid>/cwd` is the better answer for context (Spec R04
    /// user story 15).
    cwd_reported: bool,
    /// The child's exit status once `LiveEvent::Exited` surfaced;
    /// `Some` means the session is dead but the tab stays open.
    exit: Option<ExitStatus>,
    /// Shell basename for the tab bar (`bash`, `sh`, …).
    title: String,
    /// OSC 133 marks recorded at drain time (Spec R01 us-14) — the
    /// observable-and-kept half of "marks recorded in the grid".
    prompt_marks: VecDeque<PromptMark>,
    /// How much shell integration this tab's session has reported
    /// (Spec R05): `drain_events` drives it via
    /// [`IntegrationState::observe`]; `__ait_run` gating and the
    /// status line consume it in later tickets.
    integration: IntegrationState,
    /// Assembled command blocks (Spec R05 / issue #73): `drain_events`
    /// feeds marks + decoded hook events; [`blocks()`](Self::blocks)
    /// resolves them against the live grid.
    blocks: BlockAssembler,
    /// Scrollback lines evicted since spawn — the offset that maps a
    /// mark's record-time `line` into the assembler's absolute line
    /// space (`line + dropped_lines`). Two eviction signals feed it:
    /// scrollback *decrease* (truncate, `clear_history`, reflow shrink)
    /// and rotation at the cap, which `observe_scrollback` measures by
    /// tracking an anchor window of the newest scrollback rows —
    /// `GridView` itself exposes no eviction counter.
    dropped_lines: usize,
    /// Scrollback depth at the previous drain event — the delta
    /// `dropped_lines` accumulates.
    last_scrollback: usize,
    /// Row fingerprints of the newest scrollback rows (`anchor[0]` is
    /// `line(-1)`, the row that most recently entered history),
    /// refreshed by [`observe_scrollback`](Self::observe_scrollback).
    /// At a stable nonzero depth scrollback content only changes
    /// through rotation, so the window's displacement IS the eviction
    /// count scrollback depth alone can't expose (Spec R05 us-20).
    anchor: Vec<u64>,
    /// The `AIT_SESSION_ID` this tab's spawn allocated (`id` as text)
    /// — `Some` only when the injection materialized. Hook payloads
    /// must echo it before [`IntegrationState`] honors them.
    /// `pub(crate)`: same test seam as `scroll`/`selection` — tests
    /// stage a vetted stream without a real injected shell.
    pub(crate) session_id: Option<String>,
    /// The configured scrollback cap (`SessionOptions::scrollback`).
    /// `GridView` doesn't expose it, and [`observe_scrollback`]
    /// (Self::observe_scrollback) needs it: a gap that lands AT the cap
    /// with an empty anchor may have rotated invisibly, while any
    /// depth below it provably evicted nothing.
    scrollback_cap: usize,
    /// The §5.5.1 in-band exec seam: per-tab `__ait_run` request ids,
    /// the frame accumulator and the `id → waiter` map
    /// (`crate::inband`, Spec R05 / issue #75). `drain_events` feeds it
    /// every observed OSC; `pub(crate)` so tests inspect pending
    /// state — production callers go through [`run_in_shell`](Self::run_in_shell).
    pub(crate) inband: InBandExec,
    /// The materialized injection the session was spawned with —
    /// holding the guard here ties the per-session `0700` snippet dir's
    /// lifetime to the tab's (drop removes it best-effort, Spec R05
    /// us-30). `None` when the spawn wasn't injected: opt-out, unknown
    /// shell or a materialize failure.
    #[allow(dead_code)] // held for its Drop alone
    materialized: Option<ait_shell::Materialized>,
    /// The `terminal_sessions` row `app` opened for this tab at spawn
    /// (Spec R08) — `blocks` rows and the `ended_at` stamp key on it.
    /// `None` while the store is off or its insert failed: the sealed-
    /// block queue still drains so an unpersisted tab never grows it.
    store_session: Option<i64>,
    /// The `conversations` row this tab's transcript appends to
    /// (Spec R08-T3) — created by the first transcript flush, which
    /// is the first task: every later task in the same tab appends to
    /// the SAME row. `None` while the store is off or no task ran —
    /// the transcript queue still drains so an unpersisted tab never
    /// grows it.
    conversation: Option<i64>,
    /// This tab's agent conversation panel (Spec R06 / issue #85):
    /// `None` until the first `Alt+A` creates it — lazily, so a tab
    /// that never opens the panel never pays for the worker thread.
    /// One conversation at most, bound to this tab: the `Tab` drop
    /// drops the `AgentSession` inside, which is what cancels and
    /// joins the agent worker when the tab closes (standard 14 — the
    /// conversation's identity IS the tab's; no pin needed). Same
    /// `pub(crate)` seam as `scroll`/`shadow` — tests stage a panel;
    /// production drives it through the `agent_*` methods.
    pub(crate) agent: Option<AgentPanel>,
    /// The service side of this tab's `ShellBridge` (Spec R06-T4):
    /// created with the panel — `Alt+A` is the only thing that can
    /// spawn a worker wanting to reach the shell — and answered by
    /// `drain_events` every tick. `agent_key` clones its
    /// [`TabBridge`](crate::shell_bridge::TabBridge) handle into each
    /// submitted `AgentRequest`; `pub(crate)` so tests can attach a
    /// service to a staged tab.
    pub(crate) bridge: Option<BridgeService>,
    /// This tab's live remote-context stack (Spec R09-T4): nonempty
    /// while `ssh()`-wrapped sessions are open — `ssh_begin` pushes a
    /// level, a hop-attributed `ssh_end`/remote `exit_shell` closes
    /// it (and everything above it — a dead shell kills its nested
    /// sessions). The LAST level is the innermost: the status line,
    /// the bridge's `remote_state` and `blocks.host` all read it;
    /// `pub(crate)` so tests stage remote hooks without a real ssh.
    pub(crate) remote: Vec<RemoteContext>,
    /// The ait control dir this spawn armed (the template's at spawn
    /// time) — `RemoteContext::fold`'s "is this socket ours" check:
    /// only sockets under it retire on teardown.
    ssh_control_dir: Option<PathBuf>,
}

impl Drop for Tab {
    /// Retire every ait-managed ControlMaster socket still live at
    /// tab close (Spec R09-T4: `ssh -S <sock> -O exit` on `ssh_end` or
    /// tab close). Best-effort — the socket dies with `ControlPersist`
    /// regardless, and the retire never blocks the drop.
    fn drop(&mut self) {
        for context in self.remote.drain(..) {
            crate::remote::retire_control_socket(&context);
        }
    }
}

impl Tab {
    /// Spawn a session for `options` as a fresh `Tab` — the shared
    /// spawn half [`TabManager::spawn_tab`] and the headless driver
    /// (Spec R11 §5.10.5) both use: `options.cwd` is set to `cwd`,
    /// the grid takes `options.session.size` as given (the manager
    /// overrides it with the content band before calling; the hidden
    /// session passes its own), and when the template allows it an
    /// `ait_shell` injection is planned + materialized on the resolved
    /// program — its `Materialized` guard rides on the `Tab` so the
    /// `0700` snippet dir dies with it (Spec R05 us-30).
    /// `AIT_SESSION_ID` is `id`'s text: hook payloads echo it back so
    /// events stay attributable (CODING_STANDARDS.md 14 — never an
    /// index or a pid). Opt-out, unknown shells and materialize
    /// failures all degrade to a plain spawn (standard 1).
    pub(crate) fn spawn(
        mut options: LiveSessionOptions,
        cwd: PathBuf,
        id: u64,
        template: &SessionTemplate,
    ) -> Result<Self, SpawnError> {
        options.cwd = Some(cwd.clone());
        let title = title_for(&options);
        let materialized = template
            .shell_integration
            .then(|| integration::inject(&mut options, &id.to_string(), &template.ssh))
            .flatten();
        // §5.10.5: `ait ctl` inside a spawned session drives this
        // instance — the bound control socket rides the same
        // `options.env` seam as `AIT_SESSION_ID`, and lands on EVERY
        // spawn (uninjected shells too: the ctl verbs need no
        // integration).
        if let Some(socket) = &template.ctl_socket {
            options.env.push((
                OsString::from(crate::ctl::ENV_CTL_SOCKET),
                socket.clone().into_os_string(),
            ));
        }
        let scrollback_cap = options.session.scrollback;
        let session = LiveSession::spawn(options)?;
        Ok(Self {
            id,
            session,
            scroll: ScrollState::new(),
            selection: Selection::default(),
            shadow: InputShadow::new(),
            // §5.10.4: one `$PATH` walk per session spawn (us-15) —
            // keyed on the resolved program's basename, the same name
            // the tab title and request context report.
            program_index: ProgramIndex::harvest(&title),
            cwd,
            cwd_reported: false,
            exit: None,
            title,
            prompt_marks: VecDeque::new(),
            integration: IntegrationState::Absent,
            blocks: BlockAssembler::new(),
            dropped_lines: 0,
            last_scrollback: 0,
            anchor: Vec::new(),
            session_id: materialized.as_ref().map(|_| id.to_string()),
            scrollback_cap,
            inband: InBandExec::new(),
            materialized,
            store_session: None,
            conversation: None,
            agent: None,
            bridge: None,
            remote: Vec::new(),
            ssh_control_dir: template.ssh.control_dir().map(Path::to_path_buf),
        })
    }

    /// The cwd this tab tracks: OSC-7 reported, else the spawn cwd.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Whether the shell has ever reported a cwd via OSC 7 — the
    /// signal that [`cwd()`](Self::cwd) holds a real answer rather than
    /// the spawn directory. Callers that want the shell's actual cwd
    /// when no report ever arrived fall back to `/proc/<pid>/cwd`
    /// (Spec R04 user story 15).
    pub fn cwd_reported(&self) -> bool {
        self.cwd_reported
    }

    /// The child's exit status, or `None` while the session still runs.
    pub fn exit_status(&self) -> Option<&ExitStatus> {
        self.exit.as_ref()
    }

    /// The OSC 133 marks recorded so far, oldest first (Spec R01 user
    /// story 14). Capped at [`PROMPT_MARK_CAP`] — the newest win.
    /// Only tests read them today; Round 5's block assembly is the
    /// consumer this accessor exists for.
    #[allow(dead_code)]
    pub fn prompt_marks(&self) -> impl Iterator<Item = &PromptMark> {
        self.prompt_marks.iter()
    }

    /// The tab-bar title: the spawned program's basename.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// This tab's command blocks, oldest first, resolved against the
    /// live grid (Spec R05 §5.5): each `range` is clamped to current
    /// scrollback-then-viewport coordinates — fully-evicted blocks drop
    /// out entirely — and `command` is the `preexec` hook's text when
    /// the shell reported one, else the `B→C` echo slice. A command
    /// still running shows `completed: false` as the last block; a tab
    /// whose shell emits nothing simply has none (§5.5 graceful
    /// degrade). The status line, block select/copy and the
    /// suggester's context assembly (Spec R05 us-31) consume this.
    pub fn blocks(&self) -> Vec<Block> {
        let grid = self.grid();
        let extent = self.scrollback() + grid.lines();
        self.blocks
            .blocks()
            .filter_map(|entry| entry.resolve(&grid, self.dropped_lines, extent))
            .collect()
    }

    /// The text of `block`'s output rows — `output_start..=range.end`
    /// in the live coordinates [`blocks()`](Self::blocks) resolves,
    /// honoring the mid-row `D` column bound so the prompt and next
    /// echo that share `D`'s row stay out — extracted with the same
    /// copy semantics [`select_block`](Self::select_block) programs
    /// (Spec R11 `last-block`). Empty when the block's `C` mark never
    /// arrived: nothing is then *known* to be output, and a
    /// range-clamped `start > end` means the output rows left the grid
    /// entirely.
    pub(crate) fn block_output(&self, block: &Block) -> String {
        let Some(start) = block.output_start() else {
            return String::new();
        };
        let end = *block.range().end();
        if start > end {
            return String::new();
        }
        let grid = self.grid();
        let scrollback = i32::try_from(self.scrollback()).unwrap_or(i32::MAX);
        let to_grid = |line: usize| i32::try_from(line).unwrap_or(i32::MAX) - scrollback;
        // `end_column` narrows `D`'s mid-row landing; absent it the row
        // is selected whole (extract trims its trailing blanks).
        let end_column = block
            .end_column()
            .unwrap_or_else(|| grid.columns().saturating_sub(1));
        extract(
            &grid,
            SelectionRange {
                start: GridPoint::new(to_grid(start), 0),
                end: GridPoint::new(to_grid(end), end_column),
            },
        )
        .trim_end_matches('\n')
        .to_string()
    }

    /// The shell-integration state this tab's session has reported
    /// (Spec R05): `Absent` until hooks or marks arrive, `MarksOnly`
    /// when `OSC 133` arrives without `init_shell`, `Integrated` once
    /// `init_shell`/`bootstrapped` lands. `__ait_run` gating, the
    /// status line and the suggester's `/proc` cwd fallback (Spec R05,
    /// issue #76) consume it.
    pub fn integration_state(&self) -> IntegrationState {
        self.integration
    }

    /// The destination token of this tab's live remote session —
    /// `Some("user@prod")` while an `ssh()`-wrapped ssh is open, `None`
    /// on a local shell (Spec R09-T4). The status line's `ssh:` segment
    /// renders it.
    pub fn remote_target(&self) -> Option<&str> {
        self.remote.last().map(RemoteContext::target)
    }

    /// Read-only view of the session's grid (viewport + scrollback) —
    /// what painting, selection extraction and scroll math all read.
    pub fn grid(&self) -> GridView<'_> {
        self.session.grid()
    }

    /// The grid's current scrollback depth — what every scroll/jump
    /// bound clamps against.
    pub fn scrollback(&self) -> usize {
        self.session.grid().scrollback()
    }

    /// The renderable cursor: viewport position and shape.
    pub fn cursor(&self) -> Cursor {
        self.session.cursor()
    }

    /// Ask this tab's live shell to run `command` invisibly and report
    /// `{merged output, exit_code}` back over `OSC 7701` — the §5.5.1
    /// in-band seam (Spec R05 / issue #75). Writes
    /// `__ait_run <id> <hex(cmd)>` to the PTY — which the shell echoes,
    /// so the request always leaves a visible trace — and returns the
    /// [`RunRequest`] the result arrives on.
    ///
    /// Refuses unless the tab is `Integrated` (a shell without the
    /// snippet can't answer, so nothing is typed) or already exited —
    /// Round 6's `ShellBridge` service calls it on behalf of the
    /// agent's `UserShellExecutor` (Spec R06-T4).
    pub fn run_in_shell(&mut self, command: &[u8]) -> Result<RunRequest, InBandError> {
        if self.exit.is_some() {
            return Err(InBandError::SessionExited);
        }
        self.inband.request(self.integration, command, |bytes| {
            self.session.send_bytes(bytes)
        })
    }

    /// Fold one drained [`LiveEvent`] into this tab's bookkeeping — the
    /// per-event half of [`TabManager::drain_events`], also the hidden
    /// session's drain body (Spec R11 §5.10.5). Order matters:
    /// `observe_scrollback` keeps `dropped_lines` current FIRST so a
    /// mark recorded mid-batch maps with a post-eviction offset; then
    /// cwd/exit/mark recording; then the shared `ait_shell::decode`
    /// feeds integration state, the block assembler, the remote
    /// context stack, the in-band accumulator and bridge subscribers.
    pub(crate) fn observe_event(&mut self, event: &LiveEvent) {
        // Keeps `dropped_lines` current — shrinkage directly, cap
        // rotation via the anchor window — so a mark recorded
        // mid-batch maps into absolute space with a post-eviction
        // offset.
        let scrollback = self.observe_scrollback();
        match event {
            LiveEvent::Osc(SessionEvent::CwdChanged(cwd)) => {
                self.cwd = cwd.clone();
                self.cwd_reported = true;
            }
            LiveEvent::Osc(SessionEvent::PromptMark { kind, payload }) => {
                let cursor = self.session.cursor();
                let mark = PromptMark {
                    kind: *kind,
                    payload: payload.clone(),
                    // The line the cursor sits on as the drain surfaces
                    // the mark — the whole feeding chunk is already in
                    // the grid — indexed in scrollback-then-viewport
                    // coordinates.
                    line: scrollback + cursor.line,
                    column: cursor.column,
                    at: Instant::now(),
                    wall: SystemTime::now(),
                };
                // Fingerprint the open block's echo span — final once
                // the command runs — so `resolve` can prove the
                // recorded range still covers its own content,
                // dropping (never mis-selecting) on unmeasured
                // rotation (us-20).
                let seal = self.blocks.open_seal_range().and_then(|range| {
                    seal_fingerprint(&self.session.grid(), self.dropped_lines, scrollback, range)
                        .map(|(hash, rows)| (range.0, rows, hash))
                });
                // blocks.host stamps at SEAL: the local blocks from
                // before ssh_begin keep NULL; the ones sealed under
                // the open context carry the target (Spec R09-T7).
                self.blocks.set_remote_host(
                    self.remote
                        .last()
                        .map(RemoteContext::target)
                        .map(str::to_owned),
                );
                self.blocks
                    .observe_mark_row(&mark, self.dropped_lines, seal);
                if self.prompt_marks.len() == PROMPT_MARK_CAP {
                    self.prompt_marks.pop_front();
                }
                self.prompt_marks.push_back(mark);
            }
            LiveEvent::Exited(status) => {
                self.exit = Some(status.clone());
                // Spec R06-T4: a dead session answers no `__ait_run` —
                // every pending waiter's channel disconnects so the
                // executor fails cleanly instead of hanging (us-34);
                // bridge subscribers see the exit too.
                self.inband.fail_pending();
                if let Some(service) = self.bridge.as_mut() {
                    service.publish(ait_agent::BridgeEvent::Exited);
                }
            }
            _ => {}
        }
        // Shell-integration bookkeeping (Spec R05): marks move `Absent`
        // → `MarksOnly`, *vetted* decoded 7700 hooks move to and from
        // `Integrated` — the state machine in `crate::integration`
        // never panics on malformed sequences. One `ait_shell::decode`
        // per `Private` event, shared by all three observers.
        if let LiveEvent::Osc(osc) = event {
            let decoded = match osc {
                SessionEvent::Private { code, params } => ait_shell::decode(code, params),
                _ => None,
            };
            self.integration
                .observe(osc, decoded.as_ref(), self.session_id.as_deref());
            if let Some(shell_event) = &decoded {
                self.blocks.observe_shell(shell_event);
                // §5.9 (Spec R09-T4): a vetted `ssh_begin` pushes a
                // level on this tab's remote stack; `ssh_end` or a
                // remote `exit_shell` closes the level its `hop`
                // names — and every closed level retires its
                // ait-managed control socket best-effort.
                if let ait_shell::ShellEvent::Hook { name, value } = shell_event {
                    // The tab's ALLOCATED id must exist to match —
                    // `None == None` would otherwise vet every
                    // ssh_begin on an uninjected tab (the
                    // IntegrationState vet's own rule).
                    if ait_shell::session_id(value).as_deref() == self.session_id.as_deref()
                        && self.session_id.is_some()
                    {
                        for closed in RemoteContext::fold(
                            &mut self.remote,
                            name,
                            value,
                            self.ssh_control_dir.as_deref(),
                        ) {
                            crate::remote::retire_control_socket(&closed);
                        }
                    }
                }
                // §5.5.1: `OSC 7701` frames resolve pending `__ait_run`
                // requests by their payload id.
                self.inband.observe(shell_event);
                // R06-T4: bridge subscribers (the typed tiers' feed)
                // see the same decoded event.
                if let Some(service) = self.bridge.as_mut() {
                    service.publish(ait_agent::BridgeEvent::Shell(shell_event.clone()));
                }
            }
            // R06-T5's feed: prompt marks reach subscribers with their
            // ABSOLUTE line — the record-time scrollback-then-viewport
            // index plus the lines dropped so far (the block
            // assembler's own space, `observe_mark_row`'s
            // `line + dropped`) — so a `B` recorded before a scroll
            // still names its row when `grid_lines` converts it back.
            if let (Some(service), SessionEvent::PromptMark { kind, payload }) =
                (self.bridge.as_mut(), osc)
            {
                service.publish(ait_agent::BridgeEvent::PromptMark {
                    kind: shell_bridge::mark_letter(*kind),
                    payload: payload.clone(),
                    line: scrollback + self.session.cursor().line + self.dropped_lines,
                });
            }
        }
    }

    /// Answer every queued `ShellBridge` request — the `drain_events`
    /// bookkeeping the agent worker's bridge calls land on (Spec
    /// R06-T4). `run_in_band` delegates to
    /// [`run_in_shell`](Self::run_in_shell) — the R05 seam's declared
    /// first consumer — and its reply carries the pending run's
    /// result channel straight back to the worker, which polls it on
    /// its own thread while this drain keeps resolving it. Every
    /// request is answered in place: the ~8 ms input tick bounds the
    /// latency, and a `None` service answers nothing.
    pub(crate) fn service_bridge(&mut self) {
        loop {
            let request = match &self.bridge {
                Some(service) => match service.try_recv() {
                    Some(request) => request,
                    None => return,
                },
                None => return,
            };
            self.answer_bridge(request);
        }
    }

    /// One bridge request serviced against this tab — the body of
    /// [`service_bridge`](Self::service_bridge). Replies are sends on
    /// a one-shot channel; a dead worker end (dropped reply receiver)
    /// silently discards the answer.
    fn answer_bridge(&mut self, request: BridgeRequest) {
        match request {
            BridgeRequest::State(reply) => {
                let _ = reply.send(self.integration.into());
            }
            BridgeRequest::SendBytes { bytes, reply } => {
                let result = if self.exit.is_some() {
                    Err(ait_agent::BridgeError::SessionExited)
                } else {
                    self.session
                        .send_bytes(&bytes)
                        .map_err(ait_agent::BridgeError::Write)
                };
                let _ = reply.send(result);
            }
            BridgeRequest::RunInBand { command, reply } => {
                let _ = reply.send(
                    self.run_in_shell(&command)
                        .map(RunRequest::into_receiver)
                        .map_err(ait_agent::BridgeError::from),
                );
            }
            BridgeRequest::Subscribe(reply) => {
                let receiver = self
                    .bridge
                    .as_mut()
                    .map(BridgeService::subscribe)
                    .expect("a queued request implies a service");
                let _ = reply.send(receiver);
            }
            BridgeRequest::CursorLine(reply) => {
                // The cursor's row in `PromptMark::line`'s absolute
                // space — `dropped + scrollback + viewport row`, the
                // same arithmetic the drain's publish applies.
                let line =
                    self.dropped_lines + self.grid().scrollback() + self.session.cursor().line;
                let _ = reply.send(line);
            }
            BridgeRequest::GridLines { start, end, reply } => {
                // `start`/`end` arrive in the marks' absolute line
                // space (Spec R06-T5) — the space `PromptMark::line`
                // publishes: record-time index plus `dropped_lines`.
                // Absolute → `GridView::line` subtracts the lines
                // dropped so far and the live scrollback depth; rows
                // that left the grid are absent from the answer.
                let grid = self.grid();
                let offset = self.dropped_lines as i64 + grid.scrollback() as i64;
                let lines = (start..end)
                    .filter_map(|index| {
                        i32::try_from(i64::from(index) - offset)
                            .ok()
                            .and_then(|line| grid.line(line))
                            .map(|row| row.text())
                    })
                    .collect();
                let _ = reply.send(lines);
            }
            BridgeRequest::OutputTail(reply) => {
                let _ = reply.send(self.session.output_tail().to_vec());
            }
            BridgeRequest::Remote(reply) => {
                // Spec R09-T5's tier input: the innermost live remote
                // context — target + negotiated control path while an
                // `ssh()` session is open, `Local` otherwise.
                // `bootstrapped` reports the remote shell's OWN
                // confirmed integration, never the tab's local state —
                // a remote shell that never integrated can't answer
                // `__ait_run`, and the executor's typed tiers need to
                // know that (us-18).
                let state = match self.remote.last() {
                    Some(context) => ait_agent::RemoteState::Remote {
                        target: context.target().to_owned(),
                        control_path: context.control_path().map(str::to_owned),
                        bootstrapped: context.integrated(),
                    },
                    None => ait_agent::RemoteState::Local,
                };
                let _ = reply.send(state);
            }
            BridgeRequest::Context(reply) => {
                // Spec R11 us-30: the headless driver's §5.6 context —
                // the same assembly the panel runs for a visible tab.
                let _ = reply.send(context_for(self));
            }
        }
    }

    /// The `N:title` label for the tab at position `index` (0-based) —
    /// the tab bar paints it, and toasts use it to attribute a
    /// background tab's notification. The 1-based position is what
    /// `Alt+N` jumps to.
    pub fn label(&self, index: usize) -> String {
        format!("{}:{}", index + 1, self.title())
    }

    /// Distance the view sits above the live viewport (`0` = bottom).
    pub fn scroll_offset(&self) -> usize {
        self.scroll.offset()
    }

    /// Scroll `lines` into history, clamped at the oldest line.
    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll.scroll_up(lines, self.scrollback());
    }

    /// Scroll `lines` toward the live viewport, clamped at the bottom.
    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll.scroll_down(lines);
    }

    /// Scroll one page (a viewport's height) into history.
    pub fn page_up(&mut self) {
        self.scroll_up(self.grid().lines());
    }

    /// Scroll one page toward the live viewport.
    pub fn page_down(&mut self) {
        let page = self.grid().lines();
        self.scroll.scroll_down(page);
    }

    /// Jump to the oldest scrollback line.
    pub fn scroll_to_top(&mut self) {
        self.scroll.jump_to_top(self.scrollback());
    }

    /// Snap back to the live viewport — what a keypress does.
    pub fn snap_to_bottom(&mut self) {
        self.scroll.snap_to_bottom();
    }

    /// `Alt+↑` (Spec R05 §5.5/us-16): scroll the view to the previous
    /// block's start line. "Previous" is measured against the view's
    /// top row in live coordinates: the highest block start above it —
    /// so a view sitting mid-block lands on that block's own start and
    /// repeated presses walk the starts upward. No-op when no block
    /// starts above the current top.
    pub fn prev_block_start(&mut self) {
        let top = self.viewport_top();
        let start = self
            .blocks()
            .iter()
            .map(|block| *block.range().start())
            .filter(|start| *start < top)
            .max();
        if let Some(line) = start {
            self.scroll_to_line(line);
        }
    }

    /// `Alt+↓`: scroll the view to the next block's start line — the
    /// lowest block start below the view's top row. A start inside the
    /// live viewport clamps to the bottom, which is where the user
    /// expects to land past the newest block anyway. No-op when no
    /// block starts below.
    pub fn next_block_start(&mut self) {
        let top = self.viewport_top();
        let start = self
            .blocks()
            .iter()
            .map(|block| *block.range().start())
            .filter(|start| *start > top)
            .min();
        if let Some(line) = start {
            self.scroll_to_line(line);
        }
    }

    /// `Alt+B` (Spec R05 §5.5/us-17): program the existing mouse-drag
    /// [`Selection`] over the *current* block's clamped range and
    /// return the finished range for the caller's copy path — `None`
    /// when the tab has no blocks.
    ///
    /// "Current" is the block the view's bottom row sits in: the last
    /// block starting at or above it — at the live viewport that is
    /// the region around the cursor; scrolled up it is the block the
    /// user is looking at. A view above every block start still
    /// selects the first block.
    pub fn select_block(&mut self) -> Option<SelectionRange> {
        let blocks = self.blocks();
        if blocks.is_empty() {
            return None;
        }
        let bottom = self.viewport_top() + self.grid().lines().saturating_sub(1);
        let index = blocks
            .iter()
            .rposition(|block| *block.range().start() <= bottom)
            .unwrap_or(0);
        let range = blocks[index].range();
        // Live → grid coordinates: `line − scrollback` — `0..lines` is
        // the viewport, negatives reach into scrollback (`GridPoint`'s
        // rule, shared with mouse hit-testing).
        let scrollback = i32::try_from(self.scrollback()).unwrap_or(i32::MAX);
        let to_grid = |line: usize| i32::try_from(line).unwrap_or(i32::MAX) - scrollback;
        // Whole lines: column 0 through the last column — the extract
        // path trims trailing blanks itself. A mid-row `D` narrows the
        // end column so the prompt + next echo on the `D` row stay out
        // of the copy (Spec R05 us-18).
        let last_column = self.grid().columns().saturating_sub(1);
        let end_column = blocks[index].end_column().unwrap_or(last_column);
        self.begin_selection(GridPoint::new(to_grid(*range.start()), 0));
        self.extend_selection(GridPoint::new(to_grid(*range.end()), end_column));
        self.finish_selection()
    }

    /// This tab's `terminal_sessions` row id, if `app` opened one at
    /// spawn (Spec R08) — what `blocks` rows and the `ended_at` stamp
    /// key on.
    pub fn store_session(&self) -> Option<i64> {
        self.store_session
    }

    /// Bind the `terminal_sessions` row `app` opened for this tab's
    /// session — called once, right after the spawn insert lands.
    pub(crate) fn set_store_session(&mut self, id: i64) {
        self.store_session = Some(id);
    }

    /// This tab's `conversations` row id, if a transcript flush has
    /// created one (Spec R08-T3) — what `transcript_items` rows and
    /// `plans` upserts key on, and what `status` closes on.
    pub fn conversation(&self) -> Option<i64> {
        self.conversation
    }

    /// Bind the `conversations` row the first transcript flush
    /// created — the same row every later task in this tab appends to.
    pub(crate) fn set_conversation(&mut self, id: i64) {
        self.conversation = Some(id);
    }

    /// Resume a stored conversation into this tab's panel (Spec
    /// R08-T6, us-6): the panel swaps to the loaded transcript —
    /// scrollback and pinned plan rebuilt, the mirror adopted
    /// already-flushed so the next task's `prior` IS this
    /// conversation — and `conversation` binds the row so every
    /// later flush appends to IT. The panel (and its shell bridge)
    /// is created like `Alt+A`'s first toggle when the tab never
    /// had one. Callers refuse while a task runs — a live run is
    /// never clobbered (us-10).
    pub(crate) fn resume_conversation(
        &mut self,
        id: i64,
        items: Vec<ait_agent::TranscriptItem>,
        plan: Option<Vec<ait_agent::PlanTodo>>,
    ) {
        if self.bridge.is_none() {
            self.bridge = Some(shell_bridge::pair().1);
        }
        let panel = self.agent.get_or_insert_with(AgentPanel::new);
        panel.restore(items, plan);
        self.conversation = Some(id);
    }

    /// The blocks sealed since the last call, as `blocks`-row-shaped
    /// [`PersistedBlock`]s — the store's batched insert (Spec R08).
    /// Each sealed entry resolves its command against the live grid
    /// right here (the `preexec` hook's text, else the `B→C` echo
    /// slice); the queue drains regardless of whether a store row
    /// exists, so an unpersisted tab's queue can't grow.
    pub(crate) fn take_persisted_blocks(&mut self) -> Vec<PersistedBlock> {
        let sealed = self.blocks.take_sealed();
        if sealed.is_empty() {
            return Vec::new();
        }
        let grid = self.grid();
        let extent = self.scrollback() + grid.lines();
        let cwd = self.cwd.to_string_lossy().into_owned();
        sealed
            .iter()
            .map(|entry| {
                // §5.9's blocks.host rides the entry — stamped at SEAL
                // by the assembler (Spec R09-T7), so a block carries
                // the remote session it ran under, NULL when local.
                entry.persisted(&grid, self.dropped_lines, extent, cwd.clone())
            })
            .collect()
    }

    /// The last finished block's exit code and duration — the status
    /// line's `✓/✗ <code> · <dur>` segment (Spec R05 us-15). `None`
    /// when no block has reported an `OSC 133;D` exit code yet.
    pub fn last_block_status(&self) -> Option<BlockStatus> {
        self.blocks.last_status()
    }

    /// The view's top row in live scrollback-then-viewport coordinates
    /// — the anchor block navigation and block selection measure
    /// against (`scrollback − offset`; the offset is clamped to the
    /// scrollback depth, so this never underflows).
    fn viewport_top(&self) -> usize {
        self.scrollback().saturating_sub(self.scroll.offset())
    }

    /// Scroll so live line `line` tops the viewport — `offset =
    /// scrollback − line`, reached through the same clamped
    /// `scroll_up`/`scroll_down` the page bindings use. Lines inside
    /// the live viewport clamp to the bottom (offset 0).
    fn scroll_to_line(&mut self, line: usize) {
        let target = self.scrollback().saturating_sub(line);
        let offset = self.scroll.offset();
        match target.cmp(&offset) {
            std::cmp::Ordering::Greater => self.scroll_up(target - offset),
            std::cmp::Ordering::Less => self.scroll_down(offset - target),
            std::cmp::Ordering::Equal => {}
        }
    }

    /// Begin a mouse-drag selection at `point` (grid coordinates).
    pub fn begin_selection(&mut self, point: GridPoint) {
        self.selection.begin(point);
    }

    /// Extend the in-flight selection to `point`; a no-op outside a
    /// drag.
    pub fn extend_selection(&mut self, point: GridPoint) {
        self.selection.extend(point);
    }

    /// Finish the drag and return the selected range, if any.
    pub fn finish_selection(&mut self) -> Option<SelectionRange> {
        self.selection.finish()
    }

    /// Drop the highlight — typing, scrolling or switching away does
    /// this; stale state never lingers.
    pub fn clear_selection(&mut self) {
        self.selection.clear();
    }

    /// The currently highlighted range, if any.
    pub fn selection_range(&self) -> Option<SelectionRange> {
        self.selection.range()
    }

    /// Follow new output: a pinned view rides scrollback growth, and a
    /// held selection shifts with it. Called once per drained batch.
    fn on_output(&mut self) {
        let scrollback = self.observe_scrollback();
        let grown = self.scroll.on_output(scrollback);
        self.selection.on_output(grown);
    }

    /// Whether the tab tracks any block — closed or still open. The
    /// block chords consult it: a tab with none passes `Alt+B`/`Alt+↑`/
    /// `Alt+↓` through to the PTY as ordinary input (Spec R05
    /// Out-of-Scope: no integration ⇒ no behavior change).
    pub fn has_blocks(&self) -> bool {
        !self.blocks.is_empty()
    }

    /// `Alt+A` (Spec R06 / issue #85): open the panel focused on a tab
    /// that never had one, refocus it when open but unfocused (the
    /// user Esc'd back to the shell), or close it when focused. The
    /// conversation — and any running task — survives a close; only
    /// the split goes away. The caller re-derives this tab's winsize
    /// through [`TabManager::resize_active`].
    pub fn toggle_agent_panel(&mut self) {
        match self.agent.as_mut() {
            Some(panel) => panel.toggle(),
            None => {
                // The bridge service is born with the panel — it is
                // the only thing that can spawn a worker wanting to
                // reach this shell (Spec R06-T4).
                self.bridge = Some(shell_bridge::pair().1);
                self.agent = Some(AgentPanel::new());
            }
        }
    }

    /// Whether the split is open on this tab — drives the layout and
    /// this tab's per-tab winsize derivation.
    pub fn agent_panel_open(&self) -> bool {
        self.agent.as_ref().is_some_and(AgentPanel::is_open)
    }

    /// Whether keystrokes route to the panel's input line — the
    /// `suggestion_here`-style gate `app`'s pass-through arm checks
    /// before PTY input (Spec R06: focused keys never reach the PTY).
    pub fn agent_focused(&self) -> bool {
        self.agent.as_ref().is_some_and(AgentPanel::is_focused)
    }

    /// The panel, if this tab owns one — the paint path reads it.
    pub fn agent_panel(&self) -> Option<&AgentPanel> {
        self.agent.as_ref()
    }

    /// One keystroke while the panel is focused (Spec R06): plain Esc
    /// cancels a running task or — while idle — returns focus to the
    /// terminal; Enter submits the input as a task, or queues it as a
    /// steering message while one runs; chars and Backspace edit the
    /// input line. Every other key is swallowed — focused keys never
    /// reach the PTY, never update the input shadow, and never snap
    /// the terminal's view.
    ///
    /// While the gate is parked on an approve card, `a`/`t`/`d` on an
    /// EMPTY input are the verdict keys (Spec R06's three options):
    /// *allow once*, *allow this tool for the rest of the task*, and
    /// *deny* — `d` does not fire a verdict, it opens the
    /// denial-reason mode (us-8): every key then edits the input, so
    /// the reason itself can hold `a`/`t`/`d` ("don't" is typable),
    /// and Enter answers `Deny` — an empty reason still falls back to
    /// the worker's default denial text. On a non-empty input the
    /// same letters type like any other, and Enter with text while
    /// parked still steers (never a verdict). Esc cancels the task
    /// from either state — the gate's own escape (us-9), reason mode
    /// included.
    ///
    /// While an `ask_user_question` card waits (us-10 — gate-exempt,
    /// its card IS the ask), a choices ask navigates with ↑/↓ and
    /// Enter answers the highlighted option — every other key is
    /// ignored, so typed text can never become a steering message
    /// mid-ask; a free-text ask edits the input line and Enter sends
    /// it as the answer. Esc still cancels the task from either card.
    ///
    /// `profile` is the app's active execution profile (Spec R07 §5.7):
    /// the next submitted task's [`AgentRequest`] snapshots it beside
    /// `settings` — it never rewrites a running task's stance.
    pub fn agent_key(
        &mut self,
        key: &KeyEvent,
        settings: &Settings,
        profile: &str,
        skills: &mut LiveSkills,
        toast: &mut Option<Toast>,
    ) {
        if !self.agent_focused() {
            return;
        }
        let panel = self.agent.as_mut().expect("focused implies a panel");
        match key.code {
            KeyCode::Esc if key.modifiers.is_empty() => panel.escape(),
            _ if panel.awaiting_answer() => {
                if panel.ask_has_choices() {
                    match key.code {
                        KeyCode::Up if key.modifiers.is_empty() => panel.ask_prev(),
                        KeyCode::Down if key.modifiers.is_empty() => panel.ask_next(),
                        KeyCode::Enter => panel.submit_answer(toast),
                        _ => {} // stray keys ignored while a choice waits
                    }
                } else {
                    match key.code {
                        KeyCode::Enter => panel.submit_answer(toast),
                        _ => {
                            edit_input(panel, key);
                        }
                    }
                }
            }
            // Denial-reason mode (us-8): EVERY key edits the reason —
            // the verdict letters type like any other, so "don't" is
            // typable — and Enter answers `Deny` with the input as
            // the reason (an empty one falls back to the worker's
            // default text). Esc above still cancels the task.
            _ if panel.deny_reason_active() => match key.code {
                KeyCode::Enter => {
                    let reason = panel.take_input();
                    panel.decide(ApprovalVerdict::Deny(reason), toast)
                }
                _ => {
                    edit_input(panel, key);
                }
            },
            // Any Enter submits — `Alt+Enter` already dispatched to the
            // suggester upstream, so no chord collides here. A parked
            // gate does not change this: Enter with text steers.
            KeyCode::Enter => {
                let text = panel.take_input().trim_end().to_string();
                if text.is_empty() {
                    return;
                }
                if panel.is_running() {
                    panel.steer(text, toast);
                } else {
                    self.submit_agent_task(settings, profile, text, skills, toast);
                }
            }
            // The parked approve card: `a`/`t` answer at once, `d`
            // opens the denial-reason mode — all only on an EMPTY
            // input, so a typed draft is never mistaken for a verdict
            // and a verdict never swallows a draft.
            _ if panel.awaiting_verdict() => match key.code {
                KeyCode::Char('a') if key.modifiers.is_empty() && panel.input_is_empty() => {
                    panel.decide(ApprovalVerdict::AllowOnce, toast)
                }
                KeyCode::Char('t') if key.modifiers.is_empty() && panel.input_is_empty() => {
                    panel.decide(ApprovalVerdict::AllowForTask, toast)
                }
                KeyCode::Char('d') if key.modifiers.is_empty() && panel.input_is_empty() => {
                    panel.begin_deny_reason()
                }
                _ => {
                    edit_input(panel, key);
                }
            },
            _ => {
                edit_input(panel, key);
            }
        }
    }

    /// Submit `task` as a fresh [`AgentRequest`] on this tab's panel —
    /// `agent_key`'s Enter arm while idle. Its own method because the
    /// request's context and shell binding read `self` fields the
    /// panel borrow can't overlap. `bridge` is a fresh handle into
    /// this tab's bridge service (Spec R06-T4) — `None` only when the
    /// panel exists without one (a staged test tab), where
    /// `run_command` answers a clean tool error.
    fn submit_agent_task(
        &mut self,
        settings: &Settings,
        profile: &str,
        task: String,
        skills: &mut LiveSkills,
        toast: &mut Option<Toast>,
    ) {
        // The conversation is the continuity unit (Spec R08-T4): the
        // panel's accumulated transcript rides `prior` so the worker
        // replays it before this task lands — task two continues
        // where task one left off. The panel scrollback is unchanged;
        // the send records the task item itself after submission, so
        // `prior` never double-carries it.
        let prior = self
            .agent
            .as_ref()
            .map(|panel| panel.transcript_items().to_vec())
            .unwrap_or_default();
        let context = context_for(self);
        // §5.10.2 (Spec R10-T5/T6): the skills catalog snapshot comes
        // from the app's live catalog — refreshed by the roots
        // watcher since the last task — re-resolved for THIS tab's
        // cwd so a session whose nearest .ait moved picks up its own
        // project layer. What the model sees is what the request
        // carried; a malformed SKILL.md is skipped and toasted,
        // never silent.
        let catalog = skills.snapshot(Path::new(&context.cwd));
        if !catalog.errors().is_empty() {
            *toast = Some(Toast::new(skipped_text(catalog.errors()), Instant::now()));
        }
        // §5.10.1 (Spec R10 us-4): the per-task `mcp.json` re-read is
        // the file's only reload seam — the worker's reconcile sees
        // the same `ConfigError` and records it in the catalog for
        // the prompt, but the user-facing toast is the frontend's
        // job: run the same load here so a malformed file surfaces
        // naming its path instead of silently reading as "no
        // servers". The catalog keeps last-good connections either
        // way (ADR-0006).
        if let Err(error) = ait_config::load_mcp(Path::new(&context.cwd)) {
            *toast = Some(Toast::new(mcp_error_text(&error), Instant::now()));
        }
        let request = AgentRequest {
            settings: settings.clone(),
            profile: profile.to_owned(),
            task,
            prior,
            context,
            skills: catalog,
            bridge: self
                .bridge
                .as_ref()
                .map(|service| Box::new(service.handle()) as _),
        };
        if let Some(panel) = self.agent.as_mut() {
            panel.submit(request, toast);
        }
    }

    /// The wheel inside the panel's rect scrolls its conversation
    /// rather than the terminal's scrollback.
    pub fn agent_scroll_up(&mut self, lines: usize) {
        if let Some(panel) = self.agent.as_mut() {
            panel.scroll_up(lines);
        }
    }

    /// The wheel's other half — landing back on the tail re-pins the
    /// conversation's follow mode.
    pub fn agent_scroll_down(&mut self, lines: usize) {
        if let Some(panel) = self.agent.as_mut() {
            panel.scroll_down(lines);
        }
    }

    /// Feed the grid's current scrollback depth into the bookkeeping
    /// that keeps mark/block absolute lines anchored, returning the
    /// depth read. Called once per drained event so a mark recorded
    /// mid-batch maps with a current `dropped_lines`.
    ///
    /// Scrollback *decrease* is a directly observable eviction (the top
    /// lines are gone). At a *stable nonzero* depth, history content
    /// changes only through cap rotation — the anchor window of the
    /// newest scrollback rows measures it: the window's displacement
    /// is the rotation count. An anchor that left the grid entirely
    /// means an unmeasurable rotation — every range the assembler
    /// tracks could then point at wrong-but-valid rows, so it is
    /// discarded wholesale (us-20: mis-select impossible; a dropped
    /// range is the spec-sanctioned worst case).
    fn observe_scrollback(&mut self) -> usize {
        let grid = self.session.grid();
        let scrollback = grid.scrollback();
        if scrollback < self.last_scrollback {
            // History shrank — truncate, clear_history or reflow: the
            // top lines are gone and recorded indices slide.
            self.dropped_lines += self.last_scrollback - scrollback;
        } else if scrollback > 0 && !self.prompt_marks.is_empty() {
            if self.anchor.is_empty() {
                // The window holds what the previous pass saw — empty
                // means scrollback was empty then, so this gap has no
                // reference. Below the cap nothing could have been
                // evicted anyway (safe skip); AT the cap the gap may
                // have rotated invisibly — the recorded ranges are
                // suspect, drop them rather than resolve stale.
                if scrollback == self.scrollback_cap {
                    self.blocks.discard_ranges();
                }
            } else {
                // Growth displaces the window exactly `grew` rows
                // without evicting; displacement BEYOND it is cap
                // rotation — the only way history content moves at a
                // stable-or-deeper depth. This also catches the
                // boundary-crossing gap that grew AND rotated (a burst
                // that fills the cap and keeps going).
                let grew = scrollback - self.last_scrollback;
                match self.measure_rotation(&grid, grew) {
                    Rotation::Steady => {}
                    Rotation::Measured(rotated) => self.dropped_lines += rotated,
                    Rotation::Lost => self.blocks.discard_ranges(),
                }
            }
        }
        self.last_scrollback = scrollback;
        // Field assignment, not a `&mut self` method — `grid` still
        // borrows `self.session`.
        self.anchor = anchor_window(&grid, scrollback);
        scrollback
    }

    /// Locate the anchor window deeper in the scrollback:
    /// `anchor[0]` was the newest scrollback row — after `d` combined
    /// growth+eviction it sits `d` rows deeper, so a `d ≥ floor` whose
    /// rows all still match is a candidate displacement and `d − floor`
    /// the rotation count. The match must be UNIQUE: repeated output
    /// (the same burst replayed) can verify the window at several
    /// depths at once, and any displacement picked among them may
    /// anchor recorded ranges on wrong-but-valid rows — ambiguous is
    /// unmeasurable, so `Lost`. `Steady` when the unique match sits at
    /// `floor` (unmoved, or growth alone); `Lost` when no `d` verifies —
    /// the window itself was evicted.
    fn measure_rotation(&self, grid: &GridView<'_>, floor: usize) -> Rotation {
        let scrollback = grid.scrollback();
        // Candidate displacements — the window's own rows are the
        // verifier. Positions that leave fewer than two rows to check
        // can't verify. Stop once two positions match: ambiguity is
        // all the second one has to prove.
        let mut found = None;
        for d in floor..scrollback {
            let overlap = self.anchor.len().min(scrollback - d);
            if overlap < self.anchor.len().min(2) {
                break;
            }
            let verified = (0..overlap).all(|j| {
                grid.line(-((d + j + 1) as i32))
                    .is_some_and(|row| row_hash(&row) == self.anchor[j])
            });
            if verified {
                if found.is_some() {
                    return Rotation::Lost;
                }
                found = Some(d);
            }
        }
        match found {
            Some(d) if d == floor => Rotation::Steady,
            Some(d) => Rotation::Measured(d - floor),
            None => Rotation::Lost,
        }
    }
}

/// Newest scrollback rows the rotation anchor fingerprints — a false
/// match needs that many consecutive repeated rows.
const ANCHOR_ROWS: usize = 8;

/// The newest scrollback rows' fingerprints — the window
/// [`measure_rotation`](Tab::measure_rotation) compares next time.
fn anchor_window(grid: &GridView<'_>, scrollback: usize) -> Vec<u64> {
    let depth = scrollback.min(ANCHOR_ROWS);
    (1..=depth)
        .filter_map(|i| grid.line(-(i as i32)))
        .map(|row| row_hash(&row))
        .collect()
}

/// The anchor window's verdict at a stable scrollback depth.
enum Rotation {
    /// The window's rows are unmoved — no rotation.
    Steady,
    /// The window sits `d` rows deeper — `d` lines were evicted.
    Measured(usize),
    /// The window left the grid — an unmeasurable rotation; every
    /// recorded range is suspect.
    Lost,
}

/// One [`LiveEvent`] tagged with the index of the tab that produced it.
///
/// The index is stable only within the drain that produced it — tab
/// removals shift positions.
#[derive(Debug)]
pub struct TabEvent {
    /// Position in the tab list when the event was drained — the toast
    /// uses it to attribute a notification to a background tab.
    pub tab: usize,
    /// What the session surfaced.
    pub event: LiveEvent,
}

/// The spawn-time template every new tab clones — the `[terminal]`
/// half of settings (Spec R03, Spec R05). Applying a template reaches
/// only sessions spawned from then on; live tabs keep their spawn-time
/// values (story 14).
#[derive(Debug, Clone)]
pub struct SessionTemplate {
    /// Options the spawned session is built from — `[terminal]`
    /// `shell`, `scrollback_lines` and `osc52` land here.
    pub options: LiveSessionOptions,
    /// Whether the spawn consults `ait_shell`'s injection planner —
    /// `[terminal] shell_integration` (Spec R05 us-3). `false` spawns
    /// plain terminals: no argv/env rewrite, no `AIT_SESSION_ID`, no
    /// snippet dir.
    pub shell_integration: bool,
    /// The armed `[ssh]` policy the spawn renders into `AIT_SSH_*`
    /// env (Spec R09-T1/T2) — the wrapper's allow/deny/reuse rules
    /// plus the remote bootstrap command.
    pub ssh: crate::remote::SshSpawn,
    /// The running instance's control socket (Spec R11 §5.10.5):
    /// spawned sessions get it exported as `AIT_CTL_SOCKET` so
    /// `ait ctl` inside a tab drives this instance. `None` while no
    /// ctl server is bound — instance state, not a setting, so
    /// [`set_session_template`](TabManager::set_session_template)
    /// carries it across reloads.
    pub ctl_socket: Option<PathBuf>,
}

impl Default for SessionTemplate {
    /// Integration is on when `[terminal] shell_integration` is unset
    /// (§5.5); `[ssh]` arms its documented defaults likewise (§5.9).
    fn default() -> Self {
        Self {
            options: LiveSessionOptions::default(),
            shell_integration: true,
            ssh: Default::default(),
            ctl_socket: None,
        }
    }
}

/// The ordered list of tabs plus the active index.
///
/// The empty state is reachable only through
/// [`close_active`](Self::close_active) reporting `false`; every accessor
/// returns `Option` so callers must guard before indexing (standard 6).
pub struct TabManager {
    tabs: Vec<Tab>,
    active: usize,
    /// Where `ait` itself was started — the cwd a tab inherits when the
    /// active tab never reported one.
    launch_cwd: PathBuf,
    /// The content-area PTY size every tab's session gets (chrome rows
    /// live outside it); updated by [`resize_all`](Self::resize_all) so
    /// tabs spawned later match.
    size: Size,
    /// The spawn template [`new_tab`](Self::new_tab) clones — where
    /// `[terminal]` `shell`/`scrollback_lines`/`shell_integration`
    /// land (Spec R03, Spec R05).
    session_template: SessionTemplate,
    /// The id the next spawned [`Tab`] gets — a plain counter rather
    /// than an index or pid so a tab's identity is immune to list
    /// shifts and pid-less platforms (Spec R04 user story 14).
    next_id: u64,
}

impl TabManager {
    /// Spawn the manager with its first tab in the directory `ait` was
    /// launched from: `template` — the app's `[terminal]`-derived
    /// template (Spec R03), `SessionTemplate::default()` for
    /// `$SHELL`-or-`/bin/sh` — shapes the first tab AND every
    /// `new_tab` after it.
    pub fn spawn_with(size: Size, template: SessionTemplate) -> Result<Self, SpawnError> {
        let launch_cwd = std::env::current_dir().unwrap_or_default();
        Self::spawn_at(size, launch_cwd, template)
    }

    /// `spawn_with` with an explicit launch cwd — the test seam.
    /// `template` spawns the first tab and becomes the template for
    /// later ones.
    fn spawn_at(
        size: Size,
        launch_cwd: PathBuf,
        template: SessionTemplate,
    ) -> Result<Self, SpawnError> {
        let mut manager = Self {
            tabs: Vec::new(),
            active: 0,
            launch_cwd,
            size,
            session_template: template,
            next_id: 0,
        };
        let options = manager.session_template.options.clone();
        manager.spawn_tab(options)?;
        Ok(manager)
    }

    /// Replace the spawn template: sessions spawned from now on get
    /// `template` — the `[terminal]` half of a settings reload —
    /// while live tabs keep their spawn-time program and scrollback
    /// (Spec R03 user story 14). The instance's `ctl_socket` is not a
    /// setting: a reload builds its template with `None`, so the live
    /// one carries over rather than disarming `AIT_CTL_SOCKET` on
    /// later spawns (Spec R11).
    pub fn set_session_template(&mut self, mut template: SessionTemplate) {
        template.ctl_socket = template
            .ctl_socket
            .or_else(|| self.session_template.ctl_socket.clone());
        self.session_template = template;
    }

    /// The cwd a tab spawned right now inherits: the active tab's
    /// tracked cwd, else the launch cwd.
    fn spawn_cwd(&self) -> PathBuf {
        self.active()
            .map_or_else(|| self.launch_cwd.clone(), |tab| tab.cwd().to_path_buf())
    }

    /// Spawn a session for `options` and append it as the new active
    /// tab. `options.cwd` unset inherits [`spawn_cwd`](Self::spawn_cwd);
    /// `options.session.size` is always overridden — every tab's PTY is
    /// the content area, not the window (Spec R02 layout decision).
    fn spawn_tab(&mut self, mut options: LiveSessionOptions) -> Result<(), SpawnError> {
        // The outgoing tab's highlight ends when its tab loses focus —
        // even on a failed spawn, the keypress already left the tab.
        self.clear_active_selection();
        options.session.size = self.size;
        let cwd = options.cwd.take().unwrap_or_else(|| self.spawn_cwd());
        // Shell integration (Spec R05): when the template allows it,
        // `Tab::spawn` plans + materializes an injection on the
        // resolved program — `AIT_SESSION_ID` is the tab's stable id
        // (CODING_STANDARDS.md 14); opt-out, unknown shells and
        // materialize failures degrade to a plain spawn (standard 1).
        let id = self.next_id;
        let tab = Tab::spawn(options, cwd, id, &self.session_template)?;
        self.next_id += 1;
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        Ok(())
    }

    /// `Alt+T`: a fresh tab from the current spawn-options template, in
    /// the active tab's cwd.
    ///
    /// A spawn failure (e.g. the tracked cwd was deleted) leaves the tab
    /// set untouched and propagates — the caller surfaces it as a
    /// status-line toast.
    pub fn new_tab(&mut self) -> Result<(), SpawnError> {
        self.spawn_tab(self.session_template.options.clone())
    }

    /// `Alt+W`: drop the active tab. Returns `false` when that was the
    /// last tab — the tab list is then EMPTY and must not be indexed;
    /// the caller exits `ait` (standard 6).
    pub fn close_active(&mut self) -> bool {
        if self.is_empty() {
            return false;
        }
        self.tabs.remove(self.active);
        if self.is_empty() {
            return false;
        }
        // The tab that shifted into the freed slot becomes active;
        // closing the last tab wraps to the new last one.
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
        true
    }

    /// `Ctrl+PageDown`: the next tab, wrapping past the last.
    ///
    /// Switching drops the outgoing tab's selection — a highlight on a
    /// tab that is no longer on screen is stale state.
    pub fn activate_next(&mut self) {
        self.clear_active_selection();
        if !self.tabs.is_empty() {
            self.active = (self.active + 1) % self.tabs.len();
        }
    }

    /// `Ctrl+PageUp`: the previous tab, wrapping before the first.
    /// Drops the outgoing tab's selection like
    /// [`activate_next`](Self::activate_next).
    pub fn activate_prev(&mut self) {
        self.clear_active_selection();
        if !self.tabs.is_empty() {
            self.active = self.active.checked_sub(1).unwrap_or(self.tabs.len() - 1);
        }
    }

    /// `Alt+1..9`: jump straight to tab `n` (1-based). Out-of-range
    /// jumps are inert — the set may hold fewer tabs than the keymap
    /// offers digits — but the keypress still drops the outgoing tab's
    /// selection like every switch.
    pub fn jump_to(&mut self, n: usize) {
        self.clear_active_selection();
        if n >= 1 && n <= self.tabs.len() {
            self.active = n - 1;
        }
    }

    /// The active tab — `None` only on the empty set.
    pub fn active(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    /// The active tab, mutably.
    pub fn active_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    /// Index of the active tab (meaningless on the empty set).
    pub fn active_index(&self) -> usize {
        self.active
    }

    /// Drop the active tab's selection highlight — every transition
    /// away from a tab (spawn, cycle, jump) starts here. Inert on the
    /// empty set.
    fn clear_active_selection(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.clear_selection();
        }
    }

    /// All tabs, in tab-bar order.
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    /// The tab whose `AIT_SESSION_ID` is `session_id` — `ait ctl`'s
    /// `--tab` targeting (Spec R11 §5.10.5). The env-exported identity
    /// IS the stable [`Tab::id`] as text, so the lookup resolves the
    /// same value `ctl`'s `open-tab` response hands out, and reaches
    /// uninjected tabs too (they answer to their id even without the
    /// env var landing).
    pub(crate) fn session_tab(&self, session_id: &str) -> Option<&Tab> {
        self.tabs
            .iter()
            .find(|tab| tab.id.to_string() == session_id)
    }

    /// [`session_tab`](Self::session_tab), mutably — the `send` verb's
    /// target resolution.
    pub(crate) fn session_tab_mut(&mut self, session_id: &str) -> Option<&mut Tab> {
        self.tabs
            .iter_mut()
            .find(|tab| tab.id.to_string() == session_id)
    }

    /// How many tabs are open.
    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    /// Whether the tab list is empty (only after the last close).
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// Tab-bar labels in order: `1:bash`, `2:sh`, … — the 1-based
    /// position is what `Alt+N` jumps to.
    pub fn titles(&self) -> Vec<String> {
        self.tabs()
            .iter()
            .enumerate()
            .map(|(index, tab)| tab.label(index))
            .collect()
    }

    /// Forward a host resize to EVERY tab's session, not just the
    /// visible one (Spec R02 user story 17), and remember the band
    /// size for tabs spawned later. Each session gets the size its own
    /// layout implies — a tab with an open agent panel keeps its
    /// split, so `size` is the CONTENT BAND and [`session_size`]
    /// derives the per-tab winsize (Spec R06). Reflow invalidates the
    /// grid coordinates a selection range points at, so every tab's
    /// highlight is cleared. PTY-write failures on a just-dead child
    /// are tolerated — the session's `Exited` event carries that
    /// state.
    pub fn resize_all(&mut self, size: Size) {
        self.size = size;
        for tab in &mut self.tabs {
            tab.clear_selection();
            let _ = tab
                .session
                .resize(session_size(size, tab.agent_panel_open()));
        }
    }

    /// Re-derive the ACTIVE tab's PTY winsize from the stored band
    /// size — `Alt+A` opens or closes one tab's panel, so only its
    /// session shrinks or restores through the same [`session_size`]
    /// derivation [`resize_all`](Self::resize_all) applies.
    pub fn resize_active(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            let _ = tab
                .session
                .resize(session_size(self.size, tab.agent_panel_open()));
        }
    }

    /// Drain EVERY tab's session once — background tabs feed their grids
    /// too, so their output, OSC events and exits surface while another
    /// tab is on screen.
    ///
    /// Per-tab bookkeeping is applied here (`CwdChanged` updates the
    /// tab's tracked cwd, `Exited` records the status); the events are
    /// also returned for whatever else the caller renders — `app`
    /// toasts `Notification`s from this stream.
    ///
    /// After draining, each tab's pinned scroll view and selection are
    /// shifted by how much scrollback grew, so they stay on their lines
    /// (the same rule `app` applied to the single session).
    pub fn drain_events(&mut self) -> Vec<TabEvent> {
        let mut events = Vec::new();
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            for event in tab.session.drain_events() {
                // The tab's own bookkeeping — integration state, marks,
                // blocks, the remote stack, the in-band accumulator and
                // bridge publishes; the same fold the headless driver's
                // drain thread runs (Spec R11 §5.10.5).
                tab.observe_event(&event);
                events.push(TabEvent { tab: index, event });
            }
            tab.on_output();
            // R06-T4: the ShellBridge's service queue — the agent
            // worker's `run_in_band`/`send_bytes`/`subscribe` calls —
            // answers on the same tick the PTY drains.
            tab.service_bridge();
        }
        events
    }

    /// Every `(terminal_sessions id, persisted block)` pair sealed
    /// since the last call — the store's per-drain batch insert
    /// (Spec R08). Tabs without a session row (store off or the spawn
    /// insert failed) still drain their sealed queue — it can't grow
    /// behind an unpersisted run.
    pub fn take_persisted_blocks(&mut self) -> Vec<(i64, PersistedBlock)> {
        let mut rows = Vec::new();
        for tab in &mut self.tabs {
            let persisted = tab.take_persisted_blocks();
            if let Some(session) = tab.store_session {
                rows.extend(persisted.into_iter().map(|block| (session, block)));
            }
        }
        rows
    }

    /// Detach `id` from every tab bound to it (Spec R08-T6, us-11):
    /// the row was deleted — a later task must open a NEW row
    /// rather than append to a deleted one. The panel's scrollback
    /// and in-memory transcript stay: what the user sees is theirs
    /// to keep reading; only the row binding clears.
    pub fn detach_conversation(&mut self, id: i64) {
        for tab in &mut self.tabs {
            if tab.conversation == Some(id) {
                tab.conversation = None;
            }
        }
    }

    /// Drain every tab's agent session once (Spec R06): `AgentEvent`s
    /// land in each tab's conversation on the same tick the PTY
    /// sessions drain — a background tab's task keeps streaming too.
    /// Panels that were never opened cost nothing.
    pub fn drain_agent_events(&mut self) {
        for tab in &mut self.tabs {
            if let Some(panel) = tab.agent.as_mut() {
                panel.drain();
            }
        }
    }

    /// Flush every tab's accumulated transcript items into the store
    /// (Spec R08-T3, user story 28): the first flush creates the
    /// tab's `conversations` row — titled from its first task — and
    /// every later flush appends to the SAME row, so task two in a
    /// tab continues task one's transcript. The fold itself is
    /// [`StoreHandle::flush_transcript`] — the headless driver runs
    /// the same one over its single transcript; every write goes
    /// through the degrade rule, so a disabled store just drops the
    /// items — the panel's in-memory transcript keeps them
    /// regardless (T4's `prior`).
    pub fn drain_transcripts(&mut self, store: &mut StoreHandle, toast: &mut Option<Toast>) {
        for tab in &mut self.tabs {
            let items = match tab.agent.as_mut() {
                Some(panel) => panel.take_transcript_items(),
                None => continue,
            };
            // The title reads the batch's first task, falling back
            // through the panel's whole transcript — a post-detach
            // batch can start mid-conversation (Spec R08 us-5).
            let history = tab
                .agent
                .as_ref()
                .map(|panel| panel.transcript_items())
                .unwrap_or(&[]);
            if let Some(id) =
                store.flush_transcript(toast, tab.conversation, &items, history, &tab.cwd)
            {
                tab.set_conversation(id);
            }
        }
    }
}

/// The tab-bar title for a session spawned from `options`: the resolved
/// program's basename, with `sh` as the fallback when the path has no
/// usable name. The program itself comes from
/// [`LiveSessionOptions::resolved_program`] — the same resolution
/// [`LiveSession::spawn`] applies — so the title can never drift from
/// what actually spawned.
pub(crate) fn title_for(options: &LiveSessionOptions) -> String {
    Path::new(&options.resolved_program())
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| FALLBACK_TITLE.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// `XDG_CONFIG_HOME` is process-global — tests that point it at
    /// a fixture serialize on this so two writers never race it.
    static XDG: Mutex<()> = Mutex::new(());

    /// Take the env lock, surviving a poisoned guard.
    fn xdg_lock() -> std::sync::MutexGuard<'static, ()> {
        XDG.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Generous bound for every wait below (mirrors ait-term's live
    /// tests): tiny `/bin/sh` runs finish far under it even on a loaded
    /// CI runner.
    const TIMEOUT: Duration = Duration::from_secs(15);

    /// Options spawning an interactive `/bin/sh` — the cheap real
    /// session the ticket allows; PTY spawns need no host TTY.
    fn sh() -> LiveSessionOptions {
        LiveSessionOptions {
            program: Some(OsString::from("/bin/sh")),
            ..Default::default()
        }
    }

    /// Options spawning `/bin/sh -c <script>` — a tab that exits on its
    /// own for the exited-state tests.
    fn sh_script(script: &str) -> LiveSessionOptions {
        LiveSessionOptions {
            program: Some(OsString::from("/bin/sh")),
            args: vec![OsString::from("-c"), OsString::from(script)],
            ..Default::default()
        }
    }

    /// A manager with `tabs` `/bin/sh` tabs (the first carrying
    /// `launch_cwd`), all at a small size.
    fn manager(tabs: usize, launch_cwd: &str) -> TabManager {
        let mut manager = TabManager::spawn_at(
            Size::new(40, 10),
            PathBuf::from(launch_cwd),
            SessionTemplate {
                options: sh(),
                ..Default::default()
            },
        )
        .unwrap();
        for _ in 1..tabs {
            manager.spawn_tab(sh()).unwrap();
        }
        manager
    }

    /// All text in a session's grid (scrollback then viewport).
    fn grid_text(session: &LiveSession) -> String {
        let grid = session.grid();
        grid.scrollback_rows()
            .chain(grid.visible_rows())
            .map(|row| row.text().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Drain until `pred` holds, or the timeout kills the test.
    fn drain_until(manager: &mut TabManager, pred: impl Fn(&TabManager) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !pred(manager) {
            manager.drain_events();
            assert!(Instant::now() < deadline, "timed out waiting");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn spawn_starts_with_one_active_tab_in_launch_cwd() {
        let manager = manager(1, "/tmp");

        assert_eq!(manager.len(), 1);
        assert_eq!(manager.active_index(), 0);
        let tab = manager.active().unwrap();
        assert_eq!(tab.cwd(), Path::new("/tmp"));
        assert_eq!(tab.title(), "sh");
        assert_eq!(tab.exit_status(), None);
        assert_eq!(manager.titles(), vec!["1:sh".to_string()]);
    }

    /// Spec R02: a new tab spawns `$SHELL` in the CURRENT tab's tracked
    /// cwd — the launch cwd when OSC 7 never reported one.
    #[test]
    fn new_tab_falls_back_to_launch_cwd_without_osc7() {
        let mut manager = manager(1, "/tmp");

        manager.new_tab().unwrap();

        assert_eq!(manager.len(), 2);
        assert_eq!(manager.active_index(), 1, "a spawned tab is active");
        assert_eq!(manager.active().unwrap().cwd(), Path::new("/tmp"));
    }

    /// The full OSC-7 chain, driven for real: the shell `printf`s an
    /// `OSC 7 ; file://` sequence, drain picks up `CwdChanged`, and a
    /// tab spawned afterwards inherits it.
    #[test]
    fn new_tab_inherits_osc7_tracked_cwd() {
        let mut manager = manager(1, "/");

        manager
            .active()
            .unwrap()
            .session
            .send_bytes(b"printf '\\033]7;file:///tmp\\007\\n'\n")
            .unwrap();
        drain_until(&mut manager, |m| {
            m.active().unwrap().cwd() == Path::new("/tmp")
        });

        manager.new_tab().unwrap();
        assert_eq!(
            manager.active().unwrap().cwd(),
            Path::new("/tmp"),
            "the new tab spawned in the OSC-7 cwd"
        );
    }

    /// Per-tab viewport state: each tab keeps its own scroll offset —
    /// a switch leaves the background tab's view untouched. The
    /// selection is the exception: a switch drops the outgoing tab's
    /// highlight, since a range on an off-screen tab is stale state.
    #[test]
    fn viewport_state_is_per_tab() {
        let mut manager = manager(2, "/tmp");
        manager.jump_to(1);
        let first = manager.active_mut().unwrap();
        // Field access fixes a nonzero offset the public methods would
        // clamp against this tiny session's real scrollback.
        first.scroll.jump_to_top(7);
        first.begin_selection(crate::selection::GridPoint::new(0, 0));

        manager.activate_next();
        let second = manager.active().unwrap();
        assert_eq!(second.scroll_offset(), 0, "new tab starts at bottom");
        assert_eq!(second.selection_range(), None);

        manager.activate_prev();
        let first = manager.active().unwrap();
        assert_eq!(first.scroll_offset(), 7, "first tab kept its offset");
        assert!(
            first.selection_range().is_none(),
            "the switch dropped the outgoing highlight"
        );
    }

    /// Cycling wraps at both ends; `jump_to` is 1-based and ignores
    /// out-of-range targets.
    #[test]
    fn cycle_and_jump_transitions() {
        let mut manager = manager(3, "/tmp");
        assert_eq!(manager.active_index(), 2, "spawned tab is active");

        manager.activate_next();
        assert_eq!(manager.active_index(), 0, "wraps past the last");
        manager.activate_prev();
        assert_eq!(manager.active_index(), 2, "wraps before the first");
        manager.activate_prev();
        assert_eq!(manager.active_index(), 1);

        manager.jump_to(3);
        assert_eq!(manager.active_index(), 2);
        manager.jump_to(1);
        assert_eq!(manager.active_index(), 0);
        manager.jump_to(9);
        assert_eq!(manager.active_index(), 0, "out-of-range is inert");
        manager.jump_to(0);
        assert_eq!(manager.active_index(), 0, "zero is inert");
    }

    /// Closing activates whatever shifted into the freed slot — or the
    /// new last tab when the closed one was last. Session identity is
    /// tracked by child pid.
    #[test]
    fn close_active_removes_and_retargets() {
        let mut manager = manager(3, "/tmp");
        let pids: Vec<_> = manager
            .tabs()
            .iter()
            .map(|tab| tab.session.child_pid())
            .collect();

        // Close the middle tab: the old tab 3 shifts into slot 1.
        manager.jump_to(2);
        assert!(manager.close_active());
        assert_eq!(manager.len(), 2);
        assert_eq!(manager.active_index(), 1);
        assert_eq!(
            manager.active().unwrap().session.child_pid(),
            pids[2],
            "the right neighbor moved into the slot"
        );

        // Close the last tab: the index clamps to the new last.
        manager.jump_to(2);
        assert!(manager.close_active());
        assert_eq!(manager.active_index(), 0, "clamped to the new last");
        assert_eq!(manager.tabs()[0].session.child_pid(), pids[0]);
    }

    /// The last close reports `false` and the manager is then empty —
    /// `active()` returns `None` instead of indexing into nothing.
    #[test]
    fn closing_the_last_tab_leaves_a_guarded_empty_state() {
        let mut manager = manager(2, "/tmp");
        assert!(manager.close_active(), "one tab remains");
        assert!(!manager.close_active(), "the last close reports false");
        assert!(manager.is_empty());
        assert!(manager.active().is_none(), "guarded before indexing");
        assert!(manager.active_mut().is_none());

        // Every tab operation on the empty set is inert, not a panic.
        manager.activate_next();
        manager.activate_prev();
        manager.jump_to(1);
        assert!(!manager.close_active());
    }

    /// An exited session keeps its tab: `Exited` marks the status, the
    /// tab list is unchanged, and the close binding still removes it.
    #[test]
    fn exited_session_keeps_its_tab() {
        let mut manager = TabManager::spawn_at(
            Size::new(40, 10),
            PathBuf::from("/tmp"),
            SessionTemplate {
                options: sh_script("exit 3"),
                ..Default::default()
            },
        )
        .unwrap();

        drain_until(&mut manager, |m| {
            m.active().unwrap().exit_status().is_some()
        });

        assert_eq!(manager.len(), 1, "the tab persists after exit");
        let status = manager.active().unwrap().exit_status().unwrap();
        assert_eq!(status.code, 3);

        // …and closing it (no confirmation) reaches the guarded empty
        // state.
        assert!(!manager.close_active());
        assert!(manager.is_empty());
    }

    /// A failed spawn — a nonexistent program here — propagates a real
    /// `SpawnError` and leaves the tab set untouched; `app` toasts that
    /// error on `Alt+T`.
    #[test]
    fn failed_spawn_propagates_and_changes_nothing() {
        let mut manager = manager(1, "/tmp");
        let result = manager.spawn_tab(LiveSessionOptions {
            program: Some(OsString::from("/ait-definitely-nonexistent-program")),
            ..Default::default()
        });

        assert!(result.is_err(), "the spawn reports its failure");
        assert_eq!(manager.len(), 1, "no half-spawned tab was kept");
        assert_eq!(manager.active_index(), 0, "the active tab is unmoved");
    }

    /// Background tabs drain too: output lands in their grids and their
    /// exits surface while another tab is active.
    #[test]
    fn background_tabs_drain_events() {
        let mut manager = manager(1, "/tmp");
        manager
            .spawn_tab(sh_script("printf bg-done; exit 5"))
            .unwrap();
        manager.jump_to(1); // watch tab 1 while tab 2 runs

        drain_until(&mut manager, |m| m.tabs()[1].exit_status().is_some());

        assert_eq!(manager.active_index(), 0);
        assert_eq!(manager.tabs()[1].exit_status().unwrap().code, 5);
        assert!(grid_text(&manager.tabs()[1].session).contains("bg-done"));
    }

    /// OSC 133 marks are recorded on the tab, not dropped (Spec R01
    /// user story 14): each lands with its kind, payload and the
    /// scrollback-then-viewport line the cursor sat on.
    #[test]
    fn prompt_marks_are_recorded_on_the_tab() {
        let mut manager = manager(1, "/tmp");
        manager
            .spawn_tab(sh_script(
                "printf '\\033]133;A\\007prompt\\033]133;B\\007\\n\\033]133;D;0\\007'",
            ))
            .unwrap();

        drain_until(&mut manager, |m| m.tabs()[1].prompt_marks().count() >= 3);

        let marks: Vec<_> = manager.tabs()[1].prompt_marks().collect();
        assert_eq!(marks[0].kind, ait_term::PromptMarkKind::PromptStart);
        assert_eq!(marks[1].kind, ait_term::PromptMarkKind::CommandStart);
        assert_eq!(marks[2].kind, ait_term::PromptMarkKind::CommandEnd);
        assert_eq!(marks[2].payload.as_deref(), Some("0"));
        // Instants are monotonic in stream order (Spec R05:
        // `D.at − C.at` needs both). Column shares `line`'s caveat: it
        // is the post-chunk cursor column — here the burst ends after a
        // newline, so `0`.
        assert!(marks[0].at <= marks[1].at && marks[1].at <= marks[2].at);
        assert_eq!(marks[1].column, 0);
        let grid = manager.tabs()[1].grid();
        let total = grid.scrollback() + grid.lines();
        for mark in marks {
            assert!(mark.line < total, "mark line out of grid: {mark:?}");
        }
    }

    /// Spec R08: each sealed block surfaces once through
    /// `take_persisted_blocks` as a `(terminal_sessions id, row)` pair
    /// — the store's per-drain batch — and never repeats. The scripted
    /// `/bin/sh` emits two full `A B C D` quadruples with no `preexec`
    /// hook, so the persisted command is the `B→C` echo slice.
    #[test]
    fn sealed_blocks_surface_once_as_persisted_rows() {
        let mut manager = manager(1, "/tmp");
        // `sleep` splits the burst so each mark's `line`/`column`
        // records against its own chunk — mid-burst marks record the
        // post-chunk cursor (the `block_manager` fixture's trick).
        manager
            .spawn_tab(sh_script(
                "printf '\\033]133;A\\007$ \\033]133;B\\007'; sleep 0.2; \
                 printf 'first'; sleep 0.2; \
                 printf '\\n\\033]133;C\\007'; sleep 0.2; \
                 printf 'out1\\n'; sleep 0.2; \
                 printf '\\033]133;D;0\\007\\n'; sleep 0.2; \
                 printf '\\033]133;A\\007$ \\033]133;B\\007'; sleep 0.2; \
                 printf 'second'; sleep 0.2; \
                 printf '\\n\\033]133;C\\007'; sleep 0.2; \
                 printf 'out2\\n'; sleep 0.2; \
                 printf '\\033]133;D;7\\007'; sleep 0.5",
            ))
            .unwrap();
        drain_until(&mut manager, |m| {
            m.tabs()[1]
                .blocks()
                .iter()
                .filter(|b| b.completed())
                .count()
                == 2
        });
        // `app` binds the session row right after the spawn insert.
        manager.active_mut().unwrap().set_store_session(7);

        let rows = manager.take_persisted_blocks();
        assert_eq!(rows.len(), 2);
        let (id, first) = &rows[0];
        assert_eq!(*id, 7, "the row keys on the session's store id");
        assert_eq!(first.command.as_deref(), Some("first"));
        assert_eq!(first.cwd, manager.tabs()[1].cwd.to_string_lossy());
        assert_eq!(first.exit, Some(0));
        assert!(first.duration.is_some());
        assert!(first.started_at.is_some());
        assert!(first.ended_at.is_some());
        assert!(first.started_at <= first.ended_at);
        let (_, second) = &rows[1];
        assert_eq!(second.command.as_deref(), Some("second"));
        assert_eq!(second.exit, Some(7));
        // Sealed entries drain once — a second take finds nothing.
        assert!(manager.take_persisted_blocks().is_empty());
    }

    /// Spec R08's unpersisted-tab contract: a tab with no
    /// `terminal_sessions` row yields no pairs — but its sealed queue
    /// still drains, so the queue can't grow behind a disabled store.
    #[test]
    fn unpersisted_tabs_drain_sealed_without_rows() {
        let mut manager = manager(1, "/tmp");
        manager
            .spawn_tab(sh_script(
                "printf '\\033]133;A\\007$ \\033]133;B\\007cmd\\n'; \
                 printf '\\033]133;C\\007out\\n'; \
                 printf '\\033]133;D;0\\007'",
            ))
            .unwrap();
        // The `D` mark seals the block — waiting on marks (not
        // resolved blocks) keeps the burst timing out of it.
        drain_until(&mut manager, |m| m.tabs()[1].prompt_marks().count() >= 4);

        // No `set_store_session` — the manager yields no rows…
        assert!(manager.take_persisted_blocks().is_empty());
        // …and the seal is GONE, not deferred: binding a session row
        // afterwards can't resurrect it.
        manager.active_mut().unwrap().set_store_session(9);
        assert!(manager.take_persisted_blocks().is_empty());
    }

    /// A minimal `AgentRequest` for the transcript tests — the send
    /// succeeds (the channel is real); the unconfigured model's
    /// `Error` just sits in the event queue, undrained.
    fn agent_request(task: &str) -> ait_agent::AgentRequest {
        ait_agent::AgentRequest {
            settings: ait_config::Settings::default(),
            profile: "default".to_string(),
            task: task.to_string(),
            context: ait_agent::AgentContext {
                cwd: "/tmp".into(),
                shell: "sh".into(),
                os: "test".into(),
                output_lines: Vec::new(),
            },
            prior: Vec::new(),
            bridge: None,
            skills: ait_agent::SkillsCatalog::default(),
        }
    }

    /// Spec R08-T3: the first transcript flush creates the tab's
    /// `conversations` row titled from its first task; every later
    /// flush — a second task in the same tab — appends to the SAME
    /// row, seqs continuing contiguously (us-4's in-tab continuity
    /// unit persisted).
    #[test]
    fn drain_transcripts_creates_one_row_and_appends_the_next_task() {
        let dir = tempfile::tempdir().expect("fixture tempdir");
        let db = dir.path().join("store.db");
        let mut handle = StoreHandle::from_store(ait_store::Store::open(&db).unwrap());
        let mut toast = None;
        let mut manager = manager(1, "/tmp");
        manager.active_mut().unwrap().toggle_agent_panel();
        let panel = manager.active_mut().unwrap().agent.as_mut().unwrap();
        panel.submit(agent_request("fix the tests"), &mut toast);
        panel.apply(ait_agent::AgentEvent::TextDelta("on it".into()));
        panel.apply(ait_agent::AgentEvent::Done);
        manager.drain_transcripts(&mut handle, &mut toast);
        assert!(toast.is_none(), "a healthy store never toasts");

        // A second connection reads what the handle committed —
        // the same WAL-sharing rule the e2e relies on.
        let store = ait_store::Store::open(&db).unwrap();
        let rows = store.list_conversations().unwrap();
        assert_eq!(rows.len(), 1, "the first task created the row");
        assert_eq!(rows[0].title, "fix the tests", "titled from the task");
        let items = store.load_transcript(rows[0].id).unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| item.kind.as_str())
                .collect::<Vec<_>>(),
            ["task", "assistant_text", "status"],
            "send-side then drained items, in order"
        );

        // Task two in the same tab appends to the SAME row.
        let panel = manager.active_mut().unwrap().agent.as_mut().unwrap();
        panel.submit(agent_request("and the lint"), &mut toast);
        manager.drain_transcripts(&mut handle, &mut toast);
        let rows = store.list_conversations().unwrap();
        assert_eq!(rows.len(), 1, "no second conversation row");
        let items = store.load_transcript(rows[0].id).unwrap();
        assert_eq!(
            items.iter().map(|item| item.seq).collect::<Vec<_>>(),
            [0, 1, 2, 3],
            "seqs continue across flushes"
        );
        assert_eq!(items[3].kind, "task");
        // The tab bound the row — resume/delete address it (T6).
        assert_eq!(manager.active().unwrap().conversation(), Some(rows[0].id));
    }

    /// Spec R08-T5/us-17: `plan` items append to the transcript like
    /// every other kind AND upsert the conversation's `plans` row —
    /// the row always holds the latest snapshot while the transcript
    /// keeps every snapshot the calls produced.
    #[test]
    fn drain_transcripts_appends_plan_items_and_upserts_the_latest() {
        let dir = tempfile::tempdir().expect("fixture tempdir");
        let db = dir.path().join("store.db");
        let mut handle = StoreHandle::from_store(ait_store::Store::open(&db).unwrap());
        let mut toast = None;
        let mut manager = manager(1, "/tmp");
        manager.active_mut().unwrap().toggle_agent_panel();
        let panel = manager.active_mut().unwrap().agent.as_mut().unwrap();
        let todo = |content: &str, status| ait_agent::PlanTodo {
            content: content.into(),
            status,
        };
        panel.submit(agent_request("plan the work"), &mut toast);
        panel.apply(ait_agent::AgentEvent::PlanUpdated {
            todos: vec![todo("reproduce", ait_agent::PlanTodoStatus::InProgress)],
        });
        panel.apply(ait_agent::AgentEvent::PlanUpdated {
            todos: vec![
                todo("reproduce", ait_agent::PlanTodoStatus::Done),
                todo("fix", ait_agent::PlanTodoStatus::InProgress),
            ],
        });
        manager.drain_transcripts(&mut handle, &mut toast);
        assert!(toast.is_none(), "a healthy store never toasts");

        let store = ait_store::Store::open(&db).unwrap();
        let id = store.list_conversations().unwrap()[0].id;
        // Both snapshots land as transcript items — the audit trail.
        let items = store.load_transcript(id).unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| item.kind.as_str())
                .collect::<Vec<_>>(),
            ["task", "plan", "plan"]
        );
        // …and the `plans` row holds only the latest.
        let payload = store.load_plan(id).unwrap().expect("a plans row");
        let back: ait_agent::TranscriptItem =
            serde_json::from_str(&payload).expect("the payload deserializes");
        assert_eq!(
            back,
            ait_agent::TranscriptItem::Plan {
                todos: vec![
                    todo("reproduce", ait_agent::PlanTodoStatus::Done),
                    todo("fix", ait_agent::PlanTodoStatus::InProgress),
                ]
            }
        );
    }

    /// Spec R08-T6/us-6: `resume_conversation` binds the row and
    /// rebuilds the panel — a fresh flush then appends to the
    /// RESUMED row, and `detach_conversation` clears the binding so
    /// the next flush opens a new row instead of appending to a
    /// deleted one.
    #[test]
    fn resume_binds_the_row_and_detach_clears_it() {
        let dir = tempfile::tempdir().expect("fixture tempdir");
        let db = dir.path().join("store.db");
        let store = ait_store::Store::open(&db).unwrap();
        let stored = store
            .create_conversation(ait_store::NewConversation {
                title: "fix the tests",
                cwd: "/w",
                status: "open",
            })
            .unwrap();
        let items = vec![
            ait_agent::TranscriptItem::Task {
                text: "fix the tests".into(),
            },
            ait_agent::TranscriptItem::Status {
                text: "done".into(),
            },
        ];
        let mut handle = StoreHandle::from_store(store);
        let mut toast = None;
        let mut manager = manager(1, "/tmp");

        // Resume into a tab that never opened a panel — the panel
        // (and its bridge) is created like Alt+A's first toggle.
        let tab = manager.active_mut().unwrap();
        tab.resume_conversation(stored.id, items.clone(), None);
        assert_eq!(tab.conversation(), Some(stored.id));
        let panel = tab.agent_panel().expect("the resume built a panel");
        assert!(panel.is_open());
        assert_eq!(
            panel.transcript_items(),
            items.as_slice(),
            "the next task's prior IS the resumed transcript"
        );

        // A new task's flush appends to the RESUMED row — no new
        // conversation row is created.
        let panel = manager.active_mut().unwrap().agent.as_mut().unwrap();
        panel.submit(agent_request("and the lint"), &mut toast);
        manager.drain_transcripts(&mut handle, &mut toast);
        let store = ait_store::Store::open(&db).unwrap();
        assert_eq!(store.list_conversations().unwrap().len(), 1);
        let kinds: Vec<String> = store
            .load_transcript(stored.id)
            .unwrap()
            .iter()
            .map(|item| item.kind.clone())
            .collect();
        assert_eq!(kinds.last().unwrap(), "task");

        // Deleting that row detaches the binding: the next flush
        // opens a NEW row rather than appending to a deleted one.
        manager.detach_conversation(stored.id);
        assert_eq!(manager.active().unwrap().conversation(), None);
        let panel = manager.active_mut().unwrap().agent.as_mut().unwrap();
        panel.submit(agent_request("fresh work"), &mut toast);
        manager.drain_transcripts(&mut handle, &mut toast);
        let rows = store.list_conversations().unwrap();
        assert_eq!(rows.len(), 2, "the next task opened a new row");
        assert_eq!(rows[0].title, "fresh work");
        // …and its transcript holds only what followed the detach —
        // the deleted row's items never re-appended.
        assert_eq!(store.load_transcript(rows[0].id).unwrap()[0].kind, "task");
    }

    /// An unpersisted run (disabled handle) still drains every tab's
    /// accumulated items — the transcript queue can't grow behind a
    /// dead store, and the panel keeps the whole thing in memory
    /// regardless (T4's `prior` source).
    #[test]
    fn drain_transcripts_on_a_disabled_store_drops_and_moves_on() {
        let mut handle = StoreHandle::disabled();
        let mut toast = None;
        let mut manager = manager(1, "/tmp");
        manager.active_mut().unwrap().toggle_agent_panel();
        let panel = manager.active_mut().unwrap().agent.as_mut().unwrap();
        panel.submit(agent_request("fix the tests"), &mut toast);

        manager.drain_transcripts(&mut handle, &mut toast);
        assert!(toast.is_none(), "a disabled store is a silent no-op");
        assert_eq!(
            manager.active().unwrap().conversation(),
            None,
            "no row ever bound"
        );
        assert!(
            manager
                .active_mut()
                .unwrap()
                .agent
                .as_mut()
                .unwrap()
                .take_transcript_items()
                .is_empty(),
            "the batch drained rather than queueing"
        );
    }

    /// Resize reaches every tab's session, not just the visible one.
    #[test]
    fn resize_all_reaches_every_tab() {
        let mut manager = manager(3, "/tmp");
        manager.jump_to(2);

        manager.resize_all(Size::new(50, 15));

        for tab in manager.tabs() {
            assert_eq!(tab.session.size(), Size::new(50, 15));
        }
        // Tabs spawned later get the remembered size too.
        manager.spawn_tab(sh()).unwrap();
        assert_eq!(manager.active().unwrap().session.size(), Size::new(50, 15));
    }

    /// Resize clears every tab's selection — reflow moved the cells the
    /// range pointed at.
    #[test]
    fn resize_clears_selections() {
        let mut manager = manager(2, "/tmp");
        manager.jump_to(1);
        manager
            .active_mut()
            .unwrap()
            .selection
            .begin(crate::selection::GridPoint::new(0, 0));
        manager.jump_to(2);
        manager
            .active_mut()
            .unwrap()
            .selection
            .begin(crate::selection::GridPoint::new(1, 1));

        manager.resize_all(Size::new(40, 10));

        assert!(manager
            .tabs()
            .iter()
            .all(|tab| tab.selection.range().is_none()));
    }

    /// The title is the spawned program's basename; explicit programs
    /// win over `$SHELL`.
    #[test]
    fn titles_are_position_plus_program_basename() {
        let mut manager = manager(1, "/tmp");
        manager.spawn_tab(sh()).unwrap();
        assert_eq!(
            manager.titles(),
            vec!["1:sh".to_string(), "2:sh".to_string()]
        );

        manager
            .spawn_tab(LiveSessionOptions {
                program: Some(OsString::from("/bin/bash")),
                args: vec![OsString::from("-c"), OsString::from("sleep 60")],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            manager.titles(),
            vec!["1:sh".to_string(), "2:sh".to_string(), "3:bash".to_string()]
        );
    }

    /// Spec R03 stories 12–14: the session template shapes NEW
    /// tabs — the spawned program and the scrollback capacity — while a
    /// tab spawned before the template changed keeps its spawn-time
    /// values. `new_tab` used to hardcode `LiveSessionOptions::default()`;
    /// the template is what `[terminal]` writes land on at reload.
    #[test]
    fn session_template_shapes_new_tabs_only() {
        use ait_term::SessionOptions;

        // Tab 1 spawns under the default template — /bin/sh with the
        // 10_000-line scrollback. Flood it past a small cap so the
        // contrast with a reconfigured tab is observable.
        let mut manager = manager(1, "/tmp");
        manager
            .active()
            .unwrap()
            .session
            .send_bytes(b"i=0; while [ $i -lt 50 ]; do echo line$i; i=$((i+1)); done\n")
            .unwrap();
        drain_until(&mut manager, |m| m.tabs()[0].scrollback() > 5);

        // What a config reload does: swap the template, then Alt+T.
        manager.set_session_template(SessionTemplate {
            options: LiveSessionOptions {
                program: Some(OsString::from("/bin/cat")),
                session: SessionOptions {
                    scrollback: 3,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        });
        manager.new_tab().unwrap();

        let tab = manager.active().unwrap();
        assert_eq!(tab.title(), "cat", "the shell override spawned (story 12)");
        // cat echoes its input; flood it past the 3-line cap.
        let mut blob = Vec::new();
        for _ in 0..50 {
            blob.extend_from_slice(b"x\n");
        }
        tab.session.send_bytes(&blob).unwrap();
        drain_until(&mut manager, |m| m.tabs()[1].scrollback() >= 3);
        assert!(
            manager.tabs()[1].scrollback() <= 3,
            "scrollback_lines capped the new session (story 13)"
        );

        assert!(
            manager.tabs()[0].scrollback() > 5,
            "the pre-reload tab kept its spawn-time scrollback (story 14)"
        );
        assert_eq!(
            manager.tabs()[0].title(),
            "sh",
            "the pre-reload tab kept its spawn-time program"
        );
    }

    /// `OSC 7700 ; <hex(json)>` as a line an interactive shell prints —
    /// how the tests below hand-emit hook events through a real
    /// `/bin/sh` (the seam `prompt_marks_are_recorded_on_the_tab`
    /// established).
    fn hook_line(json: &str) -> Vec<u8> {
        let hex: String = json.bytes().map(|b| format!("{b:02x}")).collect();
        format!("printf '\\033]7700;{hex}\\007'\n").into_bytes()
    }

    /// Spec R05 / issue #72 + the session-id vetting fix: on a tab
    /// that allocated NO `AIT_SESSION_ID` (plain `/bin/sh` — no
    /// injection), `init_shell`/`bootstrapped`/`exit_shell` bytes are
    /// unvetted and never promote — the stream lands in `MarksOnly`,
    /// the spec's state for integration `ait` can't account for, while
    /// `OSC 133` marks still pay off identically (us-19/20).
    #[test]
    fn unvetted_hooks_never_promote() {
        let mut manager = manager(1, "/tmp");
        assert_eq!(
            manager.active().unwrap().integration_state(),
            IntegrationState::Absent,
            "a fresh tab starts Absent"
        );

        // Send a line to the interactive sh — a borrow-free helper so
        // `drain_until` can take `&mut manager` between sends.
        fn emit(manager: &TabManager, bytes: &[u8]) {
            manager.active().unwrap().session.send_bytes(bytes).unwrap();
        }

        // Forged init_shell bytes: no allocated id exists to match —
        // MarksOnly, never Integrated.
        emit(
            &manager,
            &hook_line(r#"{"hook":"init_shell","value":{"session_id":0,"shell":"sh"}}"#),
        );
        drain_until(&mut manager, |m| {
            m.active().unwrap().integration_state() == IntegrationState::MarksOnly
        });

        // An unvetted exit_shell is ignored too — the state doesn't move.
        emit(
            &manager,
            &hook_line(r#"{"hook":"exit_shell","value":{"session_id":0}}"#),
        );
        emit(&manager, b"echo after-exit\n");
        drain_until(&mut manager, |m| {
            grid_text(&m.tabs()[0].session).contains("after-exit")
        });
        assert_eq!(
            manager.active().unwrap().integration_state(),
            IntegrationState::MarksOnly
        );

        // OSC 133 marks still work regardless (us-19).
        emit(&manager, b"printf '\\033]133;A\\007'\n");
        drain_until(&mut manager, |m| m.tabs()[0].prompt_marks().count() == 1);
        assert_eq!(
            manager.active().unwrap().integration_state(),
            IntegrationState::MarksOnly
        );
    }

    /// The injected path's end of the vetting: a real injected
    /// interactive bash's own `init_shell` — which echoes the
    /// `AIT_SESSION_ID` the spawn exported — promotes the tab on its
    /// own (no hand-emitted hooks involved).
    #[test]
    fn injected_shell_integrates_itself() {
        if !Path::new("/bin/bash").exists() {
            return; // bash isn't installed everywhere tests run
        }
        let mut manager = TabManager::spawn_at(
            Size::new(40, 10),
            PathBuf::from("/tmp"),
            SessionTemplate {
                options: LiveSessionOptions {
                    program: Some(OsString::from("/bin/bash")),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();

        drain_until(&mut manager, |m| {
            m.active().unwrap().integration_state() == IntegrationState::Integrated
        });
    }

    /// A `sh -c` script emitting the `A B C D` sequence with `sleep`
    /// separators — the sleeps split the output into separate drains so
    /// each mark records its own cursor row (marks record post-chunk
    /// positions; an interactive shell produces the same shape between
    /// keystrokes, without the typed-line echo an interactive `send`
    /// would add).
    fn scripted_marks(preexec_hook: Option<&str>) -> LiveSessionOptions {
        let hook = preexec_hook
            .map(|json| {
                let hex: String = json.bytes().map(|b| format!("{b:02x}")).collect();
                format!("printf '\\033]7700;{hex}\\007'; sleep 0.2; ")
            })
            .unwrap_or_default();
        sh_script(&format!(
            "printf '\\033]133;A\\007$ \\033]133;B\\007'; sleep 0.2; \
             printf 'echo hi'; sleep 0.2; \
             {hook}\
             printf '\\n\\033]133;C\\007'; sleep 0.2; \
             printf 'hi\\n'; sleep 0.2; \
             printf '\\033]133;D;3\\007'; sleep 0.5"
        ))
    }

    /// Spec R05 / issue #73: a real `A B C D` mark stream plus a 7700
    /// `preexec` hook assembles a block on the tab — range, exit code,
    /// duration and the hook's command text. A tab whose shell emits
    /// nothing has none (§5.5 graceful degrade: `Absent` → no blocks).
    #[test]
    fn blocks_assemble_from_real_marks_and_hooks() {
        let mut manager = manager(1, "/tmp");
        assert!(
            manager.active().unwrap().blocks().is_empty(),
            "Absent: no marks → no blocks"
        );
        manager
            .spawn_tab(scripted_marks(Some(
                r#"{"hook":"preexec","value":{"session_id":1,"command":"echo hi"}}"#,
            )))
            .unwrap();

        drain_until(&mut manager, |m| {
            m.tabs()[1].blocks().iter().any(|b| b.completed())
        });

        let blocks = manager.tabs()[1].blocks();
        assert_eq!(blocks.len(), 1, "one quadruple → one block");
        let block = &blocks[0];
        assert!(block.completed());
        assert_eq!(block.exit(), Some(3));
        assert!(block.duration().is_some(), "D.at − C.at");
        assert_eq!(block.command(), "echo hi", "the preexec hook wins");
        // The range covers the echo row through the output row.
        let tab = &manager.tabs()[1];
        let grid = tab.grid();
        let range = block.range();
        let text: Vec<String> = (*range.start()..=*range.end())
            .map(|line| {
                grid.line(line as i32 - grid.scrollback() as i32)
                    .map(|row| row.text().trim_end().to_string())
                    .unwrap_or_default()
            })
            .collect();
        assert!(
            text[0].contains("echo hi"),
            "echo starts the block: {text:?}"
        );
        assert!(text.iter().any(|line| line == "hi"));
        assert_eq!(
            block.output_start(),
            Some(*range.start() + 1),
            "C marks the output's first row"
        );
    }

    /// Marks without hooks (us-19): the `B→C` echo slice supplies the
    /// command — the `B` mark's column strips the prompt.
    #[test]
    fn marks_only_blocks_slice_the_command_from_the_grid() {
        let mut manager = manager(1, "/tmp");
        manager.spawn_tab(scripted_marks(None)).unwrap();

        drain_until(&mut manager, |m| {
            m.tabs()[1].blocks().iter().any(|b| b.completed())
        });

        let tab = &manager.tabs()[1];
        assert_eq!(tab.integration_state(), IntegrationState::MarksOnly);
        let blocks = tab.blocks();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].command(), "echo hi", "echo slice, no prompt");
        assert_eq!(blocks[0].exit(), Some(3));
    }

    /// A two-block script on a 10-row grid: each `B→D` quadruple is
    /// separated by sleeps so every mark records its own row, and the
    /// padding between them pushes enough lines into scrollback that
    /// block navigation has somewhere to go.
    fn two_block_script() -> LiveSessionOptions {
        sh_script(
            "printf '\\033]133;A\\007$ \\033]133;B\\007'; sleep 0.2; \
             printf 'first'; sleep 0.2; \
             printf '\\n\\033]133;C\\007'; sleep 0.2; \
             printf 'out1\\n'; sleep 0.2; \
             printf '\\033]133;D;0\\007\\n'; sleep 0.2; \
             i=0; while [ $i -lt 14 ]; do echo pad$i; i=$((i+1)); done; sleep 0.2; \
             printf '\\033]133;A\\007$ \\033]133;B\\007'; sleep 0.2; \
             printf 'second'; sleep 0.2; \
             printf '\\n\\033]133;C\\007'; sleep 0.2; \
             printf 'out2\\n'; sleep 0.2; \
             printf '\\033]133;D;7\\007'; sleep 0.5",
        )
    }

    /// Spec R05 / issue #74: `Alt+B` programs the existing
    /// drag-selection machinery over the block's clamped range — the
    /// finished range is what `extract` (and so the OSC-52 copy path)
    /// reads, yielding command + output as the one copy granularity.
    #[test]
    fn select_block_programs_the_existing_selection() {
        let mut manager = manager(1, "/tmp");
        manager.spawn_tab(scripted_marks(None)).unwrap();
        drain_until(&mut manager, |m| {
            m.tabs()[1].blocks().iter().any(|b| b.completed())
        });

        let range = manager
            .active_mut()
            .unwrap()
            .select_block()
            .expect("a tab with blocks selects");

        let tab = manager.active().unwrap();
        let block = &tab.blocks()[0];
        let scrollback = i32::try_from(tab.scrollback()).unwrap();
        // The finished range IS the block's live range, re-expressed
        // in selection coordinates (`line − scrollback`) — and it
        // stays programmed as the tab's highlight.
        assert_eq!(range.start.line, *block.range().start() as i32 - scrollback);
        assert_eq!(range.end.line, *block.range().end() as i32 - scrollback);
        assert_eq!(tab.selection_range(), Some(range));
        let text = crate::selection::extract(&tab.grid(), range);
        assert!(text.contains("echo hi"), "the echo row: {text:?}");
        assert!(text.contains("\nhi"), "the output row: {text:?}");
    }

    /// The status segment's data source (Spec R05 us-15): the newest
    /// block that reported an exit code — here the only block, `D;3`.
    #[test]
    fn last_block_status_reports_the_newest_exit() {
        let mut manager = manager(1, "/tmp");
        manager.spawn_tab(scripted_marks(None)).unwrap();
        drain_until(&mut manager, |m| {
            m.tabs()[1].blocks().iter().any(|b| b.completed())
        });

        let status = manager.tabs()[1].last_block_status().unwrap();
        assert_eq!(status.exit, 3);
        assert!(status.duration > Duration::ZERO);
        assert!(
            manager.tabs()[0].last_block_status().is_none(),
            "the markless tab reports nothing (graceful degrade)"
        );
    }

    /// `Alt+↑`/`Alt+↓` scroll the view between block starts (Spec R05
    /// us-16): up from the bottom lands on the newest start above the
    /// view's top row, another press walks further back (or goes inert
    /// at the oldest), and down walks back toward the live viewport.
    /// A view parked mid-block jumps to that block's own start first.
    #[test]
    fn block_navigation_walks_block_starts() {
        let mut manager = TabManager::spawn_at(
            Size::new(40, 10),
            PathBuf::from("/tmp"),
            SessionTemplate {
                options: two_block_script(),
                ..Default::default()
            },
        )
        .unwrap();
        drain_until(&mut manager, |m| {
            m.active()
                .unwrap()
                .blocks()
                .iter()
                .filter(|b| b.completed())
                .count()
                == 2
                && m.active().unwrap().scrollback() > 0
        });

        let tab = manager.active_mut().unwrap();
        let scrollback = tab.scrollback();
        let starts: Vec<usize> = tab
            .blocks()
            .iter()
            .map(|block| *block.range().start())
            .collect();
        assert_eq!(starts.len(), 2, "two quadruples → two blocks");

        // Up from the bottom: the newest start above the view's top
        // row (`top = scrollback`) wins — that's the earlier block.
        let target = starts
            .iter()
            .copied()
            .filter(|start| *start < scrollback)
            .max()
            .expect("the first block sits in scrollback");
        tab.prev_block_start();
        assert_eq!(
            tab.scroll_offset(),
            scrollback - target,
            "view tops on the block's start line"
        );

        // Another press goes inert when no earlier start exists — or
        // walks further back when it does.
        let top = scrollback - tab.scroll_offset();
        let prior = starts.iter().copied().filter(|start| *start < top).max();
        let before = tab.scroll_offset();
        tab.prev_block_start();
        match prior {
            Some(line) => assert_eq!(tab.scroll_offset(), scrollback - line),
            None => assert_eq!(
                tab.scroll_offset(),
                before,
                "no earlier start — the press is inert"
            ),
        }

        // Down walks back toward the live viewport: the next start
        // below the top row.
        let top = scrollback - tab.scroll_offset();
        let next = starts
            .iter()
            .copied()
            .filter(|start| *start > top)
            .min()
            .expect("the second block is below");
        tab.next_block_start();
        assert_eq!(
            tab.scroll_offset(),
            scrollback.saturating_sub(next),
            "a start inside the live viewport clamps to the bottom"
        );

        // Mid-block anchor: park the view's top INSIDE the first
        // block's range — `Alt+↑` lands on that block's own start
        // rather than skipping it.
        let first = starts[0];
        let inside = *tab.blocks()[0].range().end();
        if inside > first && inside < scrollback {
            tab.snap_to_bottom();
            tab.scroll.scroll_up(scrollback - inside, scrollback);
            assert_eq!(scrollback - tab.scroll_offset(), inside, "fixture check");
            tab.prev_block_start();
            assert_eq!(
                tab.scroll_offset(),
                scrollback - first,
                "a mid-block view lands on the block's own start"
            );
        }
    }

    /// Blocks-only UX degrades quietly (§5.5): a tab with no marks
    /// scrolls nowhere, selects nothing, reports no block status.
    #[test]
    fn block_ux_is_inert_without_blocks() {
        let mut manager = manager(1, "/tmp");
        let tab = manager.active_mut().unwrap();

        tab.prev_block_start();
        tab.next_block_start();
        assert_eq!(tab.scroll_offset(), 0, "no blocks — the view stays");
        assert_eq!(tab.select_block(), None);
        assert!(tab.selection_range().is_none());
        assert!(tab.last_block_status().is_none());
    }

    /// Scrollback truncation (`ESC[3J`) drops the lines a block sat on:
    /// the range slides in absolute space and the fully-evicted block
    /// leaves `blocks()` cleanly — no panic, no phantom range.
    #[test]
    fn evicted_blocks_drop_out() {
        let mut manager = TabManager::spawn_at(
            Size::new(40, 10),
            PathBuf::from("/tmp"),
            SessionTemplate {
                options: LiveSessionOptions {
                    program: Some(OsString::from("/bin/sh")),
                    args: vec![
                        OsString::from("-c"),
                        OsString::from(
                            "printf '\\033]133;B\\007old\\n\\033]133;C\\007done\\n\\033]133;D;0\\007'; \
                             sleep 0.3; \
                             i=0; while [ $i -lt 25 ]; do echo line$i; i=$((i+1)); done; \
                             sleep 0.3; \
                             printf '\\033[3J'; sleep 0.5",
                        ),
                    ],
                    session: ait_term::SessionOptions {
                        scrollback: 10,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();

        drain_until(&mut manager, |m| m.active().unwrap().blocks().len() == 1);
        drain_until(&mut manager, |m| m.active().unwrap().scrollback() == 10);

        // `ESC[3J` clears the 10-line history — the recorded block's
        // whole absolute range falls below `dropped`.
        drain_until(&mut manager, |m| {
            m.active().unwrap().scrollback() == 0 && m.active().unwrap().blocks().is_empty()
        });
    }

    /// An unknown program's spawn is untouched: `sh` plans to `None` —
    /// no materialized dir, no argv/env rewrite — and a session that
    /// emits nothing stays `Absent` (§5.5 graceful degrade).
    #[test]
    fn unknown_shell_stays_absent_and_uninjected() {
        let mut manager = TabManager::spawn_at(
            Size::new(40, 10),
            PathBuf::from("/tmp"),
            SessionTemplate {
                options: sh_script("echo plain-done"),
                ..Default::default()
            },
        )
        .unwrap();

        drain_until(&mut manager, |m| {
            grid_text(&m.tabs()[0].session).contains("plain-done")
        });

        let tab = manager.tabs().first().unwrap();
        assert!(tab.materialized.is_none(), "sh plans to no injection");
        assert_eq!(tab.integration_state(), IntegrationState::Absent);
    }

    /// The injection's `AIT_SESSION_ID` really lands in the spawned
    /// shell's environment — and `shell_integration = false` opts the
    /// spawn out entirely (Spec R05 us-3): no env var, no dir. `bash -c`
    /// never reads the injected rcfile, so this also proves the rewrite
    /// is transparent to non-interactive arg forms.
    #[test]
    fn injected_env_reaches_the_shell_and_opt_out_skips_it() {
        if !Path::new("/bin/bash").exists() {
            return; // bash isn't installed everywhere tests run
        }
        let echo_sid = || LiveSessionOptions {
            program: Some(OsString::from("/bin/bash")),
            args: vec![
                OsString::from("-c"),
                OsString::from("echo SID=${AIT_SESSION_ID:-none}"),
            ],
            ..Default::default()
        };

        let mut manager = TabManager::spawn_at(
            Size::new(40, 10),
            PathBuf::from("/tmp"),
            SessionTemplate {
                options: echo_sid(),
                shell_integration: false,
                ssh: Default::default(),
                ctl_socket: None,
            },
        )
        .unwrap();
        drain_until(&mut manager, |m| {
            grid_text(&m.tabs()[0].session).contains("SID=")
        });
        let tab = manager.tabs().first().unwrap();
        assert!(tab.materialized.is_none(), "opt-out spawns no injection");
        assert!(
            grid_text(&tab.session).contains("SID=none"),
            "no AIT_SESSION_ID was exported: {:?}",
            grid_text(&tab.session)
        );

        // Flip the template — the reload path — and spawn again.
        manager.set_session_template(SessionTemplate {
            options: echo_sid(),
            ..Default::default()
        });
        manager.new_tab().unwrap();
        drain_until(&mut manager, |m| {
            grid_text(&m.tabs()[1].session).contains("SID=")
        });
        let tab = &manager.tabs()[1];
        let materialized = tab
            .materialized
            .as_ref()
            .expect("an injected spawn keeps its dir guard");
        assert!(materialized.dir().is_dir());
        assert!(
            grid_text(&tab.session).contains(&format!("SID={}", tab.id)),
            "AIT_SESSION_ID is the tab's stable id: {:?}",
            grid_text(&tab.session)
        );
    }

    /// The materialized `0700` dir lives exactly as long as its tab:
    /// closing the tab drops the guard and the dir goes away (Spec R05
    /// us-30 — integration leaves no litter).
    #[test]
    fn snippet_dir_dies_with_the_tab() {
        if !Path::new("/bin/bash").exists() {
            return;
        }
        let mut manager = TabManager::spawn_at(
            Size::new(40, 10),
            PathBuf::from("/tmp"),
            SessionTemplate {
                options: LiveSessionOptions {
                    program: Some(OsString::from("/bin/bash")),
                    args: vec![OsString::from("-c"), OsString::from("sleep 30")],
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        let dir = manager.tabs()[0]
            .materialized
            .as_ref()
            .expect("bash spawns injected")
            .dir()
            .to_path_buf();
        assert!(dir.is_dir());

        assert!(!manager.close_active(), "the last tab closes the app");
        assert!(
            !dir.exists(),
            "the session dir was removed with the tab: {dir:?}"
        );
    }

    /// A shell command line that prints `count` numbered lines starting
    /// at `start` — the flood that overflows a capped scrollback in the
    /// rotation regression test. Distinct `start`s keep every burst's
    /// rows unique: repeated content could verify the anchor at a
    /// smaller displacement (the window can't tell identical rows
    /// apart — a documented bound of content anchoring).
    fn flood_line(start: usize, count: usize) -> Vec<u8> {
        format!("seq {start} {}\n", start + count - 1).into_bytes()
    }

    /// Spec R05 / issue #75 — the §5.5.1 seam through a real
    /// interactive `/bin/sh`: `Absent`/`MarksOnly` requests are refused
    /// with NO bytes typed; once `Integrated`, `run_in_shell` types a
    /// visible `__ait_run <id> <hex>` line, and hand-emitted `7701`
    /// frames resolve the waiter's channel while their decoded payload
    /// never lands in the grid.
    #[test]
    fn inband_run_gates_then_round_trips() {
        let mut manager = manager(1, "/tmp");
        fn emit(manager: &TabManager, bytes: &[u8]) {
            manager.active().unwrap().session.send_bytes(bytes).unwrap();
        }

        // Absent: refused before anything is typed. The marker command
        // afterwards flushes the input stream — had the request leaked
        // bytes, its echo would precede the marker.
        let err = manager
            .active_mut()
            .unwrap()
            .run_in_shell(b"echo hi")
            .expect_err("an Absent tab refuses");
        assert!(
            matches!(err, InBandError::NotIntegrated(IntegrationState::Absent)),
            "{err:?}"
        );
        emit(&manager, b"echo gate-marker\n");
        drain_until(&mut manager, |m| {
            grid_text(&m.tabs()[0].session).contains("gate-marker")
        });
        assert!(
            !grid_text(&manager.tabs()[0].session).contains("__ait_run"),
            "a refused request typed nothing"
        );

        // MarksOnly — including forged init_shell bytes, which stay
        // unvetted on an unallocated tab — refuses the same way.
        emit(&manager, b"printf '\\033]133;A\\007'\n");
        emit(
            &manager,
            &hook_line(r#"{"hook":"init_shell","value":{"session_id":0,"shell":"sh"}}"#),
        );
        drain_until(&mut manager, |m| {
            m.active().unwrap().integration_state() == IntegrationState::MarksOnly
        });
        let err = manager
            .active_mut()
            .unwrap()
            .run_in_shell(b"echo hi")
            .expect_err("a MarksOnly tab refuses");
        assert!(
            matches!(err, InBandError::NotIntegrated(IntegrationState::MarksOnly)),
            "{err:?}"
        );

        // The round-trip needs a vetted `Integrated` tab: a real
        // injected bash integrates itself and defines `__ait_run`.
        if !Path::new("/bin/bash").exists() {
            return; // bash isn't installed everywhere tests run
        }
        manager
            .spawn_tab(LiveSessionOptions {
                program: Some(OsString::from("/bin/bash")),
                ..Default::default()
            })
            .unwrap();
        drain_until(&mut manager, |m| {
            m.tabs()[1].integration_state() == IntegrationState::Integrated
        });

        // The request types `__ait_run 0 <hex(echo hi)>` — visible in
        // the grid (us-23 transparency) — and the snippet's own reply
        // resolves the waiter: a real end-to-end in-band round-trip.
        let req = manager
            .active_mut()
            .unwrap()
            .run_in_shell(b"echo hi")
            .expect("an Integrated tab accepts the request");
        assert_eq!(req.id(), 0);
        drain_until(&mut manager, |m| !m.tabs()[1].inband.is_pending(0));
        let result = req.wait(Duration::from_secs(1)).unwrap();
        assert_eq!(result.id, "0");
        assert!(
            result.output.windows(2).any(|w| w == b"hi"),
            "the snippet's merged output: {:?}",
            result.output
        );
        assert_eq!(result.exit_code, 0);

        // A second request whose reply spans several `R` frames
        // (>500 decoded bytes) resolves the same way.
        let req = manager
            .active_mut()
            .unwrap()
            .run_in_shell(b"seq 1 400")
            .expect("a second request");
        assert_eq!(req.id(), 1);
        drain_until(&mut manager, |m| !m.tabs()[1].inband.is_pending(1));
        let result = req.wait(Duration::from_secs(1)).unwrap();
        assert_eq!(result.id, "1");
        assert!(
            result.output.windows(3).any(|w| w == b"400"),
            "the chunked reply reassembled: {} bytes",
            result.output.len()
        );
        assert_eq!(result.exit_code, 0);

        // The decoded payload rendered nowhere — it only ever crossed
        // the PTY hex-inside-OSC, and private OSCs never render. (The
        // typed `__ait_run` lines themselves are grid text — us-23.)
        let text = grid_text(&manager.tabs()[1].session);
        assert!(text.contains("__ait_run 0"), "the request's trace: {text}");
        assert!(
            !text.lines().any(|line| line.trim() == "hi"),
            "7701 output is invisible: {text}"
        );
    }

    /// Run one blocking `ShellBridge` call on a worker thread while
    /// this thread keeps pumping `drain_events` — the same split the
    /// production loop runs (the agent worker waits; the app tick
    /// services). `f` takes the bridge so `FnOnce` closures over
    /// `&TabManager` stay out of the thread.
    fn bridge_call<T, F>(
        manager: &mut TabManager,
        bridge: &crate::shell_bridge::TabBridge,
        f: F,
    ) -> T
    where
        T: Send + 'static,
        F: FnOnce(crate::shell_bridge::TabBridge) -> T + Send + 'static,
    {
        let handle = std::thread::spawn({
            let bridge = bridge.clone();
            move || f(bridge)
        });
        let deadline = Instant::now() + TIMEOUT;
        while !handle.is_finished() {
            manager.drain_events();
            assert!(
                Instant::now() < deadline,
                "timed out waiting for bridge service"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        handle.join().expect("bridge call panicked")
    }

    /// Attach a fresh bridge pair to `manager`'s active tab —
    /// production creates it inside `toggle_agent_panel`; tests stage
    /// it directly.
    fn attach_bridge(manager: &mut TabManager) -> (crate::shell_bridge::TabBridge, &Tab) {
        let (handle, service) = crate::shell_bridge::pair();
        let tab = manager.active_mut().expect("an active tab");
        tab.bridge = Some(service);
        (handle, manager.active().unwrap())
    }

    /// Spec R06-T4 (issue #87) through a REAL injected bash: the
    /// bridge's `run_in_band` request is serviced by `drain_events`,
    /// the shell's `7701` reply resolves the returned channel, and
    /// the typed `__ait_run` line stays visible while the merged
    /// output never renders (us-12, us-23).
    #[test]
    fn bridge_run_in_band_round_trips_through_drain() {
        if !Path::new("/bin/bash").exists() {
            return; // bash isn't installed everywhere tests run
        }
        let mut manager = TabManager::spawn_at(
            Size::new(40, 10),
            PathBuf::from("/tmp"),
            SessionTemplate {
                options: LiveSessionOptions {
                    program: Some(OsString::from("/bin/bash")),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        drain_until(&mut manager, |m| {
            m.active().unwrap().integration_state() == IntegrationState::Integrated
        });
        let (bridge, _) = attach_bridge(&mut manager);
        use ait_agent::ShellBridge;

        // The tier selector answers through the service too.
        let state = bridge_call(&mut manager, &bridge, |b| {
            b.integration_state().expect("state")
        });
        assert_eq!(state, ait_agent::ShellIntegration::Integrated);

        // The in-band run: request serviced by the drain, result on
        // the returned channel.
        let receiver = bridge_call(&mut manager, &bridge, |b| {
            b.run_in_band(b"printf 'agent-marker'")
                .expect("run accepted")
        });
        drain_until(&mut manager, |m| !m.tabs()[0].inband.is_pending(0));
        let result = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("the run resolved");
        assert_eq!(result.output, b"agent-marker".to_vec());
        assert_eq!(result.exit_code, 0);

        // The trace guarantee (us-23): the typed request line is grid
        // text; the 7701 output stays invisible.
        let text = grid_text(&manager.tabs()[0].session);
        assert!(text.contains("__ait_run"), "the request's trace: {text}");
        assert!(
            !text.contains("agent-marker"),
            "7701 output is invisible: {text}"
        );
    }

    /// Spec R09-T4/T7 through a REAL injected bash: a vetted
    /// `ssh_begin` opens the tab's remote context — the status
    /// accessor, the bridge's `remote_state` and the sealed block's
    /// `host` all read it — `ssh_end` closes it, and a `ssh_begin`
    /// carrying a foreign session id never opens anything.
    #[test]
    fn remote_context_follows_vetted_ssh_hooks() {
        if !Path::new("/bin/bash").exists() {
            return; // bash isn't installed everywhere tests run
        }
        let mut manager = TabManager::spawn_at(
            Size::new(40, 10),
            PathBuf::from("/tmp"),
            SessionTemplate {
                options: LiveSessionOptions {
                    program: Some(OsString::from("/bin/bash")),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        drain_until(&mut manager, |m| {
            m.active().unwrap().integration_state() == IntegrationState::Integrated
        });
        let sid = manager.tabs()[0].session_id.clone().unwrap();
        fn emit(manager: &TabManager, bytes: &[u8]) {
            manager.active().unwrap().session.send_bytes(bytes).unwrap();
        }

        // A foreign session id is unvetted: the hook never opens
        // context — drain past it on a marker echo.
        emit(
            &manager,
            &hook_line(
                r#"{"hook":"ssh_begin","value":{"session_id":"bogus","target":"evil@h","bootstrapped":true}}"#,
            ),
        );
        emit(&manager, b"echo BOGUSMARKER\n");
        drain_until(&mut manager, |m| {
            grid_text(&m.tabs()[0].session).contains("BOGUSMARKER")
        });
        assert_eq!(manager.active().unwrap().remote_target(), None);

        // The vetted begin opens the context — carrying its hop id.
        emit(
            &manager,
            &hook_line(&format!(
                r#"{{"hook":"ssh_begin","value":{{"session_id":"{sid}","target":"u@prod","hop":"{sid}.1.1","control_path":"/tmp/ait-ssh-foreign/%C"}}}}"#
            )),
        );
        drain_until(&mut manager, |m| {
            m.active().unwrap().remote_target() == Some("u@prod")
        });
        // `bootstrapped` is the REMOTE shell's own proof — a remote
        // `init_shell` attributed to this hop marks the context
        // integrated; the ssh_begin payload's `bootstrapped` field
        // only means the wrapper SENT the bootstrap.
        emit(
            &manager,
            &hook_line(&format!(
                r#"{{"hook":"init_shell","value":{{"session_id":"{sid}","hop":"{sid}.1.1"}}}}"#
            )),
        );
        drain_until(&mut manager, |m| {
            m.active()
                .unwrap()
                .remote
                .last()
                .is_some_and(RemoteContext::integrated)
        });

        // The bridge's remote_state answers the live context — the
        // executor tiers' selection input (Spec R09-T5).
        let (bridge, _) = attach_bridge(&mut manager);
        use ait_agent::ShellBridge;
        let state = bridge_call(&mut manager, &bridge, |b| {
            b.remote_state().expect("remote state")
        });
        let ait_agent::RemoteState::Remote {
            target,
            control_path,
            bootstrapped,
        } = state
        else {
            panic!("expected Remote, got {state:?}")
        };
        assert_eq!(target, "u@prod");
        assert_eq!(control_path.as_deref(), Some("/tmp/ait-ssh-foreign/%C"));
        assert!(bootstrapped);

        // A block sealed inside the session persists `blocks.host`
        // (Spec R09-T7): marks B C D seal it, the row carries
        // "u@prod". Wait on the D mark's own record — take_sealed
        // would consume the row the assert below needs.
        emit(
            &manager,
            b"printf '\\033]133;B\\007\\033]133;C\\007\\033]133;D;0\\007'\n",
        );
        drain_until(&mut manager, |m| {
            m.tabs()[0]
                .prompt_marks()
                .any(|mark| mark.kind == ait_term::PromptMarkKind::CommandEnd)
        });
        let rows = manager.active_mut().unwrap().take_persisted_blocks();
        // Stamped at SEAL: the local blocks from before ssh_begin
        // (the bogus hook line, the marker echo) keep NULL; the ones
        // sealed under the open context carry the target.
        assert_eq!(
            rows.last().map(|row| row.host.as_deref()),
            Some(Some("u@prod")),
            "the remote block carries its host: {rows:?}"
        );
        assert!(
            rows.iter()
                .filter(|row| row.host.as_deref() == Some("u@prod"))
                .count()
                < rows.len(),
            "blocks sealed before ssh_begin stayed local: {rows:?}"
        );

        // ssh_end closes the context — the status segment's source
        // and every downstream consumer go back to local.
        emit(
            &manager,
            &hook_line(&format!(
                r#"{{"hook":"ssh_end","value":{{"session_id":"{sid}","target":"u@prod","hop":"{sid}.1.1"}}}}"#
            )),
        );
        drain_until(&mut manager, |m| {
            m.active().unwrap().remote_target().is_none()
        });
        let state = bridge_call(&mut manager, &bridge, |b| {
            b.remote_state().expect("remote state")
        });
        assert_eq!(state, ait_agent::RemoteState::Local);
    }

    /// Refusals and exits are clean `BridgeError`s, never hangs: an
    /// `Absent` tab refuses `run_in_band` with nothing typed; an
    /// exited session refuses both the run and raw `send_bytes`
    /// (us-34).
    #[test]
    fn bridge_refusals_and_exited_sessions_error_cleanly() {
        use ait_agent::{BridgeError, ShellBridge, ShellIntegration};
        let mut manager = manager(1, "/tmp");
        let (bridge, _) = attach_bridge(&mut manager);

        // Absent: `run_in_band` refuses — the drain reports the live
        // state, and no `__ait_run` is typed.
        let state = bridge_call(&mut manager, &bridge, |b| {
            b.integration_state().expect("state")
        });
        assert_eq!(state, ShellIntegration::Absent);
        let error = bridge_call(&mut manager, &bridge, |b| {
            b.run_in_band(b"echo hi").expect_err("Absent refuses")
        });
        assert!(
            matches!(error, BridgeError::NotIntegrated(ShellIntegration::Absent)),
            "{error:?}"
        );
        manager
            .active()
            .unwrap()
            .session
            .send_bytes(b"echo refuse-marker\n")
            .unwrap();
        drain_until(&mut manager, |m| {
            grid_text(&m.tabs()[0].session).contains("refuse-marker")
        });
        assert!(
            !grid_text(&manager.tabs()[0].session).contains("__ait_run"),
            "a refused request typed nothing"
        );

        // `send_bytes` still works on the live shell — the typed
        // tiers' (R06-T5) primitive.
        bridge_call(&mut manager, &bridge, |b| {
            b.send_bytes(b"echo sent-marker\n").expect("send_bytes")
        });
        drain_until(&mut manager, |m| {
            grid_text(&m.tabs()[0].session).contains("sent-marker")
        });

        // Exit the session: the drain's Exited handling marks the
        // tab, drops pending waits, and later bridge calls refuse.
        manager
            .active()
            .unwrap()
            .session
            .send_bytes(b"exit\n")
            .unwrap();
        drain_until(&mut manager, |m| {
            m.active().unwrap().exit_status().is_some()
        });
        let error = bridge_call(&mut manager, &bridge, |b| {
            b.run_in_band(b"echo hi")
                .expect_err("an exited session refuses")
        });
        assert!(matches!(error, BridgeError::SessionExited), "{error:?}");
        let error = bridge_call(&mut manager, &bridge, |b| {
            b.send_bytes(b"x")
                .expect_err("send_bytes on a dead session")
        });
        assert!(matches!(error, BridgeError::SessionExited), "{error:?}");
    }

    /// The subscription feed — R06-T5's typed-tier input: marks,
    /// decoded `7700`-series events and the exit all arrive in drain
    /// order; `grid_lines`/`output_tail` snapshot the capture
    /// surfaces.
    #[test]
    fn bridge_subscription_forwards_marks_events_and_exit() {
        use ait_agent::{BridgeEvent, ShellBridge};
        let mut manager = manager(1, "/tmp");
        let (bridge, _) = attach_bridge(&mut manager);

        let events_rx = bridge_call(&mut manager, &bridge, |b| b.subscribe().expect("subscribe"));
        let mut seen = Vec::new();

        // An OSC 133 mark forwards as `PromptMark` with its payload
        // and recorded line.
        manager
            .active()
            .unwrap()
            .session
            .send_bytes(b"printf '\\033]133;D;0\\007'\n")
            .unwrap();
        drain_until(&mut manager, |m| m.tabs()[0].prompt_marks().count() >= 1);
        seen.extend(events_rx.try_iter());
        let mark_line = seen
            .iter()
            .find_map(|event| match event {
                BridgeEvent::PromptMark {
                    kind: 'D',
                    payload: Some(_),
                    line,
                } => Some(*line),
                _ => None,
            })
            .expect("a 133;D mark reached the subscriber");

        // The mark's published line is absolute — `grid_lines`
        // resolves it through the frontend's dropped/scrollback
        // offsets onto the very row the mark was recorded on.
        let row = bridge_call(&mut manager, &bridge, move |b| {
            b.grid_lines(mark_line as i32, mark_line as i32 + 1)
                .expect("grid lines")
        });
        assert_eq!(row.len(), 1, "the mark's own row resolves: {row:?}");

        // The capture surface answers a span of mark-space lines too.
        let lines = bridge_call(&mut manager, &bridge, |b| {
            b.grid_lines(0, 40).expect("grid lines")
        });
        assert!(!lines.is_empty());
        let tail = bridge_call(&mut manager, &bridge, |b| {
            b.output_tail().expect("output tail")
        });
        assert!(!tail.is_empty(), "the tail saw the session's output");

        // The exit forwards — and closes pending run waits.
        manager
            .active()
            .unwrap()
            .session
            .send_bytes(b"exit\n")
            .unwrap();
        drain_until(&mut manager, |m| {
            m.active().unwrap().exit_status().is_some()
        });
        let deadline = Instant::now() + TIMEOUT;
        loop {
            seen.extend(events_rx.try_iter());
            if seen.iter().any(|e| matches!(e, BridgeEvent::Exited)) {
                break;
            }
            manager.drain_events();
            assert!(Instant::now() < deadline, "Exited never arrived: {seen:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// `OSC 7701` bytes on a session's output never reach the grid:
    /// the emulator swallows private OSCs (the observer still decodes
    /// them — resolution coverage lives in the tests above). Printed
    /// mid-text, `PRE` and `POST` land contiguous.
    #[test]
    fn run_frames_never_render() {
        let mut manager = TabManager::spawn_at(
            Size::new(80, 24),
            PathBuf::from("/tmp"),
            SessionTemplate {
                options: sh_script(
                    "printf 'PRE\\033]7701;A\\007\\033]7701;R;3;616263\\007\\033]7701;B\\007POST\\n'",
                ),
                ..Default::default()
            },
        )
        .unwrap();

        drain_until(&mut manager, |m| {
            grid_text(&m.tabs()[0].session).contains("POST")
        });

        let text = grid_text(&manager.tabs()[0].session);
        assert!(
            text.contains("PREPOST"),
            "the OSCs rendered nothing: {text}"
        );
        assert!(!text.contains("7701"), "{text}");
        // `616263` decodes to `abc` — the payload is invisible too.
        assert!(!text.contains("abc"), "{text}");
    }

    /// The real round-trip: injected bash defines `__ait_run`, so a
    /// `run_in_shell` request executes inside the live shell — merged
    /// output and exit code come back over `7701` while the typed
    /// request line stays visible (us-23). The e2e-harness version of
    /// this is T9's; this covers the tab-level seam end to end.
    #[test]
    fn injected_bash_executes_run_requests_in_band() {
        if !Path::new("/bin/bash").exists() {
            return; // bash isn't installed everywhere tests run
        }
        let mut manager = TabManager::spawn_at(
            Size::new(80, 24),
            PathBuf::from("/tmp"),
            SessionTemplate {
                options: LiveSessionOptions {
                    program: Some(OsString::from("/bin/bash")),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        drain_until(&mut manager, |m| {
            m.active().unwrap().integration_state() == IntegrationState::Integrated
        });

        let req = manager
            .active_mut()
            .unwrap()
            .run_in_shell(b"printf 'inband-ok\\n'; (exit 7)")
            .expect("the integrated tab accepts");
        assert_eq!(req.id(), 0);
        drain_until(&mut manager, |m| !m.tabs()[0].inband.is_pending(0));

        let result = req.wait(Duration::from_secs(1)).unwrap();
        assert_eq!(result.id, "0");
        assert_eq!(result.exit_code, 7);
        assert!(
            String::from_utf8_lossy(&result.output).contains("inband-ok"),
            "{result:?}"
        );

        let text = grid_text(&manager.tabs()[0].session);
        assert!(
            text.contains("__ait_run 0"),
            "the request left a trace: {text}"
        );
        assert!(
            !text.contains("inband-ok"),
            "captured output stayed off the grid: {text}"
        );
    }

    /// Real bash, injected for real: the `--rcfile` wrapper sources the
    /// snippet, which emits `init_shell` — the tab reports `Integrated`
    /// without a single dotfile touched (Spec R05 us-1).
    #[test]
    fn injected_bash_reports_integrated() {
        if !Path::new("/bin/bash").exists() {
            return;
        }
        let mut manager = TabManager::spawn_at(
            Size::new(80, 24),
            PathBuf::from("/tmp"),
            SessionTemplate {
                options: LiveSessionOptions {
                    program: Some(OsString::from("/bin/bash")),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();

        drain_until(&mut manager, |m| {
            m.active().unwrap().integration_state() == IntegrationState::Integrated
        });
    }

    /// Spec R05 us-20 regression: a scrollback cap's rotation must never
    /// leave a block's range pointing at wrong-but-valid rows — the
    /// anchor window either measures the rotation (the range stays true)
    /// or reports itself lost and the block drops out. `Alt+B` can
    /// therefore never copy stale rows.
    #[test]
    fn capped_scrollback_never_mis_selects() {
        let mut manager = TabManager::spawn_at(
            Size::new(40, 10),
            PathBuf::from("/tmp"),
            SessionTemplate {
                options: LiveSessionOptions {
                    program: Some(OsString::from("/bin/sh")),
                    session: ait_term::SessionOptions {
                        scrollback: 40,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        fn emit(manager: &TabManager, bytes: &[u8]) {
            manager.active().unwrap().session.send_bytes(bytes).unwrap();
        }

        // Fill scrollback to the cap BEFORE the marks — the window then
        // exists to measure rotation (a first flood from empty scrollback
        // is the discard case, exercised below).
        emit(&manager, &flood_line(1, 50));
        drain_until(&mut manager, |m| m.active().unwrap().scrollback() == 40);

        // One real block via hand-emitted marks (each a separate drain).
        emit(
            &manager,
            b"printf '\\033]133;A\\007$ \\033]133;B\\007'; sleep 0.3\n",
        );
        emit(&manager, b"printf 'echo ANCHOR'; sleep 0.3\n");
        emit(&manager, b"printf '\\n\\033]133;C\\007'; sleep 0.3\n");
        emit(&manager, b"printf 'ANCHORED-OUT\\n'; sleep 0.3\n");
        emit(&manager, b"printf '\\033]133;D;0\\007'; sleep 0.3\n");
        drain_until(&mut manager, |m| {
            m.active().unwrap().blocks().iter().any(|b| b.completed())
        });

        // ~25 rotations at the cap — inside the anchor window's reach:
        // the block must stay anchored on its OWN rows, never on
        // whatever slid into its recorded indices.
        emit(&manager, &flood_line(100, 25));
        // Wait on a marker only EXECUTION produces — a plain echo's
        // input echo matches `grid_text` before the flood has run.
        emit(&manager, b"echo FD$((100+24))\n");
        drain_until(&mut manager, |m| {
            grid_text(&m.tabs()[0].session).contains("FD124")
        });

        let tab = manager.active_mut().unwrap();
        let blocks = tab.blocks();
        let block = blocks.first().expect("a measured rotation keeps the block");
        let range = block.range();
        let scrollback = i32::try_from(tab.scrollback()).unwrap();
        let text = crate::selection::extract(
            &tab.grid(),
            crate::selection::SelectionRange {
                start: crate::selection::GridPoint::new(*range.start() as i32 - scrollback, 0),
                end: crate::selection::GridPoint::new(*range.end() as i32 - scrollback, 39),
            },
        );
        assert!(
            text.contains("ANCHORED-OUT"),
            "the range still covers its own output: {text:?}"
        );
        assert!(
            !text.contains("FLOOD"),
            "no flood rows leaked into the block: {text:?}"
        );

        // More bursts, each inside the window's reach but together
        // rotating the block's own rows past the top: the range must
        // slide with them and then fall out of the grid cleanly — a
        // fully evicted block resolves to nothing, so `Alt+B` finds no
        // stale rows to copy.
        for _ in 0..4 {
            emit(&manager, &flood_line(100, 25));
        }
        emit(&manager, b"echo GD$((2+3))\n");
        drain_until(&mut manager, |m| {
            grid_text(&m.tabs()[0].session).contains("GD5")
        });
        let tab = manager.active_mut().unwrap();
        assert!(
            tab.blocks().is_empty(),
            "a rotated-out block resolves to nothing: {:?}",
            tab.blocks().iter().map(|b| b.range()).collect::<Vec<_>>()
        );
        assert_eq!(tab.select_block(), None, "no stale rows to select");
    }

    /// Spec R06 / issue #85: `Alt+A`'s toggle and a host resize share
    /// the band→session derivation — opening shrinks only THIS tab's
    /// session to the terminal side of the split, a resize keeps the
    /// split proportional, and closing restores the whole band.
    #[test]
    fn agent_panel_toggle_and_resize_share_the_split_math() {
        let mut manager = manager(1, "/tmp");

        manager.active_mut().unwrap().toggle_agent_panel();
        manager.resize_active();
        assert_eq!(
            manager.active().unwrap().session.size(),
            Size::new(24, 10),
            "open panel: two fifths of the 40-column band goes to the panel"
        );

        // A host resize re-derives the same tab's split — the panel
        // keeps its share rather than restoring full width.
        manager.resize_all(Size::new(80, 20));
        assert_eq!(manager.active().unwrap().session.size(), Size::new(48, 20));

        manager.active_mut().unwrap().toggle_agent_panel();
        manager.resize_active();
        assert_eq!(
            manager.active().unwrap().session.size(),
            Size::new(80, 20),
            "closed panel: the session owns the band again"
        );
    }

    /// Per-tab ownership: the split belongs to the tab, a fresh tab
    /// takes the whole band, and dropping the owning tab drops the
    /// `AgentSession` inside — cancelling and joining the worker even
    /// with a task queued (the Drop contract, not a special case).
    #[test]
    fn panels_are_per_tab_and_drop_with_the_tab() {
        let mut manager = manager(2, "/tmp");
        manager.active_mut().unwrap().toggle_agent_panel();
        assert!(manager.active().unwrap().agent_panel_open());
        // Queue real work so the worker is mid-task at close.
        let mut toast = None;
        manager
            .active_mut()
            .unwrap()
            .agent
            .as_mut()
            .unwrap()
            .submit(
                ait_agent::AgentRequest {
                    settings: ait_config::Settings::default(),
                    profile: "default".to_string(),
                    task: "fix it".to_string(),
                    context: ait_agent::AgentContext {
                        cwd: "/tmp".into(),
                        shell: "sh".into(),
                        os: "test".into(),
                        output_lines: Vec::new(),
                    },
                    prior: Vec::new(),
                    bridge: None,
                    skills: ait_agent::SkillsCatalog::default(),
                },
                &mut toast,
            );

        manager.spawn_tab(sh()).unwrap();
        assert!(
            !manager.active().unwrap().agent_panel_open(),
            "a fresh tab owns no panel"
        );

        assert!(manager.close_active(), "the spawned tab closes");
        assert_eq!(manager.len(), 2);
        assert!(
            manager.active().unwrap().agent_panel_open(),
            "the surviving tab kept its conversation"
        );
        // Closing THE owning tab drops the worker mid-task — Drop
        // cancels and joins; nothing to observe but the clean return.
        assert!(manager.close_active(), "the owning tab closes");
    }

    /// The approve card's keys (Spec R06): while the gate is parked,
    /// `a`/`t`/`d` on an EMPTY input are the verdict keys — `d`
    /// opens the denial-reason mode rather than firing a verdict —
    /// and the same letters type like any other once the input holds
    /// text. In reason mode every key edits (a reason can contain
    /// `a`/`t`/`d`) and Enter answers `Deny`. With no card pending
    /// the letters are ordinary input.
    #[test]
    fn approve_card_keys_answer_the_gate() {
        use crossterm::event::KeyEvent as CtKeyEvent;
        let mut manager = manager(1, "/tmp");
        let mut toast = None;
        let settings = Settings::default();
        let mut skills = LiveSkills::unwatched();
        let key = |code| CtKeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        let staged = |id: &str| ait_agent::AgentEvent::ApprovalRequest {
            call: ait_agent::ToolCall {
                id: id.into(),
                name: "read_files".into(),
                arguments: String::new(),
            },
            summary: "note.txt".into(),
        };

        let tab = manager.active_mut().unwrap();
        tab.toggle_agent_panel();

        // `a` answers allow-once — the wait clears.
        tab.agent.as_mut().unwrap().apply(staged("call_1"));
        assert!(tab.agent.as_ref().unwrap().awaiting_verdict());
        tab.agent_key(
            &key(KeyCode::Char('a')),
            &settings,
            "default",
            &mut skills,
            &mut toast,
        );
        assert!(!tab.agent.as_ref().unwrap().awaiting_verdict());
        assert!(toast.is_none());

        // `d` on an empty input opens reason mode — the card stays
        // parked and the reason can contain the verdict letters.
        tab.agent.as_mut().unwrap().apply(staged("call_2"));
        tab.agent_key(
            &key(KeyCode::Char('d')),
            &settings,
            "default",
            &mut skills,
            &mut toast,
        );
        {
            let panel = tab.agent.as_ref().unwrap();
            assert!(
                panel.awaiting_verdict(),
                "d opened the reason, not a verdict"
            );
            assert!(panel.deny_reason_active());
        }
        for c in "don't allow".chars() {
            tab.agent_key(
                &key(KeyCode::Char(c)),
                &settings,
                "default",
                &mut skills,
                &mut toast,
            );
        }
        {
            let panel = tab.agent.as_ref().unwrap();
            assert_eq!(panel.input(), "don't allow", "every key edited the reason");
            assert!(panel.awaiting_verdict(), "the card still waits");
        }
        // Enter answers `Deny` with the reason — the wait clears.
        tab.agent_key(
            &key(KeyCode::Enter),
            &settings,
            "default",
            &mut skills,
            &mut toast,
        );
        {
            let panel = tab.agent.as_ref().unwrap();
            assert!(!panel.awaiting_verdict());
            assert_eq!(panel.input(), "", "the reason was consumed");
        }

        // `d` again — reason mode — and Enter on an EMPTY reason
        // still denies (the worker substitutes its default text).
        tab.agent.as_mut().unwrap().apply(staged("call_3"));
        tab.agent_key(
            &key(KeyCode::Char('d')),
            &settings,
            "default",
            &mut skills,
            &mut toast,
        );
        tab.agent_key(
            &key(KeyCode::Enter),
            &settings,
            "default",
            &mut skills,
            &mut toast,
        );
        assert!(!tab.agent.as_ref().unwrap().awaiting_verdict());

        // With input held, the same letters are ordinary editing —
        // `a`/`d` type rather than verdict; the card stays parked
        // until the input is emptied and `a` pressed again.
        tab.agent.as_mut().unwrap().apply(staged("call_4"));
        tab.agent_key(
            &key(KeyCode::Char('x')),
            &settings,
            "default",
            &mut skills,
            &mut toast,
        );
        tab.agent_key(
            &key(KeyCode::Char('d')),
            &settings,
            "default",
            &mut skills,
            &mut toast,
        );
        tab.agent_key(
            &key(KeyCode::Char('a')),
            &settings,
            "default",
            &mut skills,
            &mut toast,
        );
        {
            let panel = tab.agent.as_ref().unwrap();
            assert!(panel.awaiting_verdict(), "non-empty input = editing");
            assert_eq!(panel.input(), "xda");
        }
        tab.agent.as_mut().unwrap().take_input();
        tab.agent_key(
            &key(KeyCode::Char('a')),
            &settings,
            "default",
            &mut skills,
            &mut toast,
        );
        assert!(!tab.agent.as_ref().unwrap().awaiting_verdict());

        // And with nothing pending, `d` edits the input like any
        // other character.
        tab.agent_key(
            &key(KeyCode::Char('d')),
            &settings,
            "default",
            &mut skills,
            &mut toast,
        );
        assert_eq!(tab.agent.as_ref().unwrap().input(), "d");
    }

    /// The ask card's keys (Spec R06 us-10 / issue #91): a choices
    /// ask navigates with ↑/↓ and Enter answers the highlighted
    /// option while stray keys are ignored — the typed input never
    /// becomes a steering message mid-ask. A free-text ask instead
    /// takes the input line on Enter.
    #[test]
    fn ask_card_keys_select_or_take_free_text() {
        use crossterm::event::KeyEvent as CtKeyEvent;
        let mut manager = manager(1, "/tmp");
        let mut toast = None;
        let settings = Settings::default();
        let mut skills = LiveSkills::unwatched();
        let key = |code| CtKeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        let ask = |id: &str, choices: &[&str]| ait_agent::AgentEvent::AskRequest {
            call: ait_agent::ToolCall {
                id: id.into(),
                name: "ask_user_question".into(),
                arguments: String::new(),
            },
            question: "Which?".into(),
            choices: choices.iter().map(|c| c.to_string()).collect(),
        };

        let tab = manager.active_mut().unwrap();
        tab.toggle_agent_panel();

        // A choices ask: ↓ then Enter answers the second option —
        // and stray keys move nothing.
        let panel = tab.agent.as_mut().unwrap();
        panel.apply(ask("call_1", &["a", "b"]));
        assert!(panel.awaiting_answer());
        tab.agent_key(
            &key(KeyCode::Char('x')),
            &settings,
            "default",
            &mut skills,
            &mut toast,
        );
        tab.agent_key(
            &key(KeyCode::Left),
            &settings,
            "default",
            &mut skills,
            &mut toast,
        );
        tab.agent_key(
            &key(KeyCode::Backspace),
            &settings,
            "default",
            &mut skills,
            &mut toast,
        );
        {
            let panel = tab.agent.as_ref().unwrap();
            assert!(panel.awaiting_answer(), "stray keys resolved nothing");
            assert_eq!(panel.input(), "", "stray keys never reached the input");
        }
        tab.agent_key(
            &key(KeyCode::Down),
            &settings,
            "default",
            &mut skills,
            &mut toast,
        );
        tab.agent_key(
            &key(KeyCode::Enter),
            &settings,
            "default",
            &mut skills,
            &mut toast,
        );
        {
            let panel = tab.agent.as_ref().unwrap();
            assert!(!panel.awaiting_answer());
            assert!(matches!(
                panel.entries().last(),
                Some(crate::agent_panel::Entry::Ask(card))
                    if card.answer.as_deref() == Some("b")
            ));
        }

        // A free-text ask: chars edit the input, Enter sends them.
        let panel = tab.agent.as_mut().unwrap();
        panel.apply(ask("call_2", &[]));
        assert!(!panel.ask_has_choices());
        for c in "why not".chars() {
            tab.agent_key(
                &key(KeyCode::Char(c)),
                &settings,
                "default",
                &mut skills,
                &mut toast,
            );
        }
        // ↓ on a free-text ask navigates nothing — the card keeps
        // waiting and the input is untouched.
        tab.agent_key(
            &key(KeyCode::Down),
            &settings,
            "default",
            &mut skills,
            &mut toast,
        );
        assert!(tab.agent.as_ref().unwrap().awaiting_answer());
        tab.agent_key(
            &key(KeyCode::Enter),
            &settings,
            "default",
            &mut skills,
            &mut toast,
        );
        {
            let panel = tab.agent.as_ref().unwrap();
            assert!(!panel.awaiting_answer());
            assert!(matches!(
                panel.entries().last(),
                Some(crate::agent_panel::Entry::Ask(card))
                    if card.answer.as_deref() == Some("why not")
            ));
            assert_eq!(panel.input(), "", "the answer consumed the input");
        }
    }

    /// Spec R10 us-4: a malformed `mcp.json` surfaces as a toast
    /// naming the offending path at the submit seam — the same
    /// per-task re-read the worker's reconcile runs (whose
    /// `load_error` is the model's view of the failure, kept in the
    /// prompt's catalog section).
    #[test]
    fn a_malformed_mcp_json_toasts_at_submit() {
        let _lock = xdg_lock();
        let dirs = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", dirs.path().join("xdg"));
        let cwd = dirs.path().join("work");
        let ait_dir = cwd.join(".ait");
        std::fs::create_dir_all(&ait_dir).unwrap();
        std::fs::write(ait_dir.join("mcp.json"), "{not json").unwrap();

        let mut manager = manager(1, cwd.to_str().unwrap());
        let mut toast = None;
        let settings = Settings::default();
        let mut skills = LiveSkills::unwatched();
        let tab = manager.active_mut().unwrap();
        tab.toggle_agent_panel();
        tab.submit_agent_task(
            &settings,
            "default",
            "do it".to_string(),
            &mut skills,
            &mut toast,
        );

        let toast = toast.expect("the malformed mcp.json toasted");
        assert!(
            toast.text().starts_with("mcp.json failed to load:"),
            "the toast labels the file: {}",
            toast.text()
        );
        assert!(
            toast.text().contains(".ait"),
            "the toast names the offending path: {}",
            toast.text()
        );
    }

    /// `--tab`'s resolution (Spec R11 §5.10.5): the session id is the
    /// tab's stable `id` as text — the same value `ctl`'s `open-tab`
    /// response hands out — resolving injected and uninjected tabs
    /// alike; an unknown id resolves to nothing.
    #[test]
    fn session_tab_resolves_by_tab_id() {
        let mut manager = manager(2, "/tmp");
        // `/bin/sh` spawns are never injected — the id still names the tab.
        assert!(manager.session_tab("0").is_some());
        assert!(manager.session_tab("1").is_some());
        assert!(manager.session_tab("9").is_none());
        let id = manager.session_tab("1").unwrap().id;
        assert_eq!(manager.session_tab_mut("1").unwrap().id, id);
    }

    /// Spec R11 §5.10.5: a template carrying `ctl_socket` exports it
    /// as `AIT_CTL_SOCKET` into every spawned session — the plain
    /// uninjected `/bin/sh` included, since the ctl verbs need no
    /// integration — and a settings reload (a fresh template built
    /// with `None`) keeps it armed rather than dropping the export.
    #[test]
    fn ctl_socket_exports_into_spawned_sessions() {
        let socket = PathBuf::from("/tmp/ait-test-ctl.sock");
        let mut manager = TabManager::spawn_at(
            Size::new(40, 10),
            PathBuf::from("/tmp"),
            SessionTemplate {
                options: sh(),
                ctl_socket: Some(socket.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        manager
            .active()
            .unwrap()
            .session
            .send_bytes(b"echo SOCK=${AIT_CTL_SOCKET:-none}\n")
            .unwrap();
        drain_until(&mut manager, |m| {
            grid_text(&m.tabs()[0].session).contains("SOCK=/tmp/ait-test-ctl.sock")
        });

        // What a config reload does: a fresh template with `None`.
        manager.set_session_template(SessionTemplate {
            options: sh(),
            ..Default::default()
        });
        manager.new_tab().unwrap();
        manager
            .active()
            .unwrap()
            .session
            .send_bytes(b"echo SOCK2=${AIT_CTL_SOCKET:-none}\n")
            .unwrap();
        drain_until(&mut manager, |m| {
            grid_text(&m.tabs()[1].session).contains("SOCK2=/tmp/ait-test-ctl.sock")
        });
    }

    /// `last-block`'s read-back (Spec R11): `block_output` yields the
    /// block's `C`→`D` rows only — the echo row and `D`'s own row stay
    /// out — in the same extraction semantics the copy path uses.
    #[test]
    fn block_output_slices_the_output_rows() {
        let mut manager = manager(1, "/tmp");
        manager.spawn_tab(scripted_marks(None)).unwrap();
        drain_until(&mut manager, |m| {
            m.tabs()[1].blocks().iter().any(|b| b.completed())
        });

        let tab = &manager.tabs()[1];
        let blocks = tab.blocks();
        let block = &blocks[0];
        assert_eq!(
            tab.block_output(block),
            "hi",
            "just the output rows — echo and D's row excluded"
        );
    }
}
