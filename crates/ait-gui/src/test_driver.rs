//! The test-driver socket (Spec R15 / ticket R15-T1): a second,
//! test-only Unix socket beside the [`ait_app::ctl`] control socket —
//! the harness's way in. The whole module compiles only under the
//! off-by-default `test-driver` cargo feature, so release artifacts
//! carry none of this code, and even a `test-driver` build binds
//! nothing unless [`ENV_DRIVER_SOCKET`] names the path.
//!
//! The wire follows the ctl socket's conventions exactly: a
//! nonblocking `UnixListener`, one request per connection, requests
//! and responses as single-line JSON, accepted connections buffered
//! across drain passes so a slow client never wedges the UI, the
//! socket `0600` under its parent dir, unlinked on drop.
//!
//! Protocol v1 verbs — every request is `{"verb":…}`:
//!
//! - `{"verb":"key","chord":"Ctrl+Shift+E"}` — one press+release pair
//!   of `egui::Event::Key`s carrying the chord's *source* modifier
//!   bits (`ctrl`/`alt`/`shift`/`mac_cmd`; `command` is derived the
//!   way winit derives it), queued for the next frame's
//!   `dispatch_events` — everything downstream is production input
//!   code. `"twin":true` adds the chord's `Event::Text` twin between
//!   press and release — the shape twin-emitting platforms deliver
//!   for `Alt+<letter>` (the R13 leak); `"twin":"å"` gives the twin
//!   an explicit payload.
//! - `{"verb":"text","text":"…"}` — one `egui::Event::Text`, the
//!   composed-text twin of real typing.
//! - `{"verb":"paste","text":"…"}` — one `egui::Event::Paste`, the
//!   OS-clipboard shape a Ctrl+V delivers.
//! - `{"verb":"scroll","unit":"…","dx":…,"dy":…}` — one wheel
//!   gesture: `unit` is `line`, `pixel` (sub-line touchpad deltas —
//!   winit's `PixelDelta`) or `page`; `dx`/`dy` are in the unit.
//!   `over` (`grid`/`panel`) picks the hover region — `grid` is the
//!   default — and `x`/`y` give an explicit position in points.
//! - `{"verb":"resize","width":…,"height":…}` — a viewport resize
//!   request in points, sent as a real `ViewportCommand::InnerSize`
//!   so the winsize settle path runs end to end.
//! - `{"verb":"pointer","event":"…","x":…,"y":…}` — one pointer
//!   gesture piece (`move`/`press`/`release`/`modifiers`) delivered
//!   as a real `RawInput` event through [`DriverInputPlugin`]'s
//!   `input_hook` — the only seam upstream of egui's `PointerState`
//!   fold, so hover, press, click and drag state update exactly as
//!   they do for winit-delivered input. `target` names a hit
//!   registry entry (its rect's center, plus `dx`/`dy`); `x`/`y` is
//!   the raw escape hatch; `button` defaults `primary`; `modifiers`
//!   sets the held modifier set for the event (a `ModifiersChanged`
//!   lands first, real input's order).
//! - `{"verb":"click","target":"…"}` — one complete click (move +
//!   press + release in a single frame batch) on a hit-registry
//!   `target`, or on raw `x`/`y`. Unknown names answer `ok:false`
//!   with the registered names. `modifiers` holds the set for the
//!   click only — the held state restores on the next frame.
//! - `{"verb":"agent","event":"approve","name":"…","summary":"…"}` —
//!   stage an `AgentEvent::ApprovalRequest` on the active tab's
//!   panel through `AgentPanel::apply` — the same fold
//!   `drain_agent_events` feeds real worker events — so a staged
//!   approve card IS the panel's real approval flow.
//! - `{"verb":"dump"}` — a versioned JSON snapshot of what the user
//!   sees, sourced from the same [`Shared`] state `ui`'s paint pass
//!   reads: the active tab's visible grid rows + cursor, scroll
//!   position and selection, the pane host's selection, the find
//!   bar, the suggestion arm, the agent panel's open/focus/gate
//!   state, the window's content size, and the hit-target registry
//!   (`hits` — every interact site's name → rect).
//! - `{"verb":"quit"}` — the app takes its normal close path
//!   (`GuiApp::quit` → `ViewportCommand::Close`).
//!
//! Injected input joins the `dispatch_events` stream at frame start
//! via [`Shared::injected`]; pointer input rides
//! [`DriverInputPlugin`] instead — `dispatch_events` runs mid-pass,
//! after `InputState::begin_pass` already folded the frame's events
//! into `PointerState`, so only `RawInput` injection reaches the
//! hit-test. The socket is drained in the pump loop beside ctl
//! (`drain_pass`), where a `dump` answers from post-drain state. The
//! control socket's request enum and `ait ctl` are untouched — this
//! socket is the only path to input injection.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ait_app::ctl::REQUEST_CAP;
use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::app::Shared;
use crate::pane::{PaneId, SidePane};

/// The env var naming the test-driver socket's path — the harness
/// picks the path (inside its throwaway dir), exports it, and the app
/// binds exactly there. Unset or empty means no socket: ordinary runs,
/// even `test-driver` builds, never open it. The same string is
/// re-declared in `ait-testkit`'s `gui` module — no shared crate sits
/// between the server and its only client (CODING_STANDARDS §17's
/// duplication ban yields to the crate boundary; keep the two in
/// lockstep).
pub const ENV_DRIVER_SOCKET: &str = "AIT_TEST_DRIVER_SOCKET";

/// The dump's protocol version — clients gate field expectations on
/// it; additive changes stay v1, shape changes bump it.
pub const DUMP_VERSION: u32 = 1;

/// A half-open connection's lifetime — same bound as ctl's: a client
/// that connects but never completes a line is never serviced forever.
const PENDING_TTL: Duration = Duration::from_secs(30);

/// One request on the wire — internally tagged on `"verb"` like the
/// ctl protocol, so an unknown verb fails decode naming the variant.
#[derive(Debug, Deserialize)]
#[serde(tag = "verb", rename_all = "snake_case")]
pub enum Request {
    /// `key` — the chord string the scenario verbs and `GuiE2e::key`
    /// speak (`"Ctrl+Shift+E"`, `"Alt+A"`, `"Enter"`): parsed here into
    /// one press+release pair of `egui::Event::Key`s.
    Key {
        /// The chord to press and release.
        chord: String,
        /// The platform text twin — `true` derives the chord's
        /// character (`Alt+A` → `"a"`), a string is the payload
        /// verbatim. Emitted between press and release, the order
        /// egui-winit produces from one winit `KeyboardInput`.
        #[serde(default)]
        twin: Option<TwinSpec>,
    },
    /// `text` — one `egui::Event::Text` payload, composed text the way
    /// real typing delivers it.
    Text {
        /// The text to compose.
        text: String,
    },
    /// `paste` — one `egui::Event::Paste` payload, the OS-clipboard
    /// shape a Ctrl+V delivers.
    Paste {
        /// The clipboard payload.
        text: String,
    },
    /// `scroll` — one wheel gesture with an explicit hover region:
    /// real wheel events route on `pointer.hover_pos`, which injected
    /// events can't reach (egui consumed the pointer state at
    /// `begin_pass`), so the scroll's target is resolved at dispatch
    /// instead.
    Scroll {
        /// The delta's unit — `line`, `pixel` or `page`.
        unit: ScrollUnit,
        /// The horizontal delta in `unit`s — positive is right.
        #[serde(default)]
        dx: f32,
        /// The vertical delta in `unit`s — positive scrolls back, like
        /// a wheel flick toward the user.
        #[serde(default)]
        dy: f32,
        /// An explicit hover position in points — `x` and `y` travel
        /// together and win over `over`.
        #[serde(default)]
        x: Option<f32>,
        /// See `x`.
        #[serde(default)]
        y: Option<f32>,
        /// The region a bare scroll hovers — `grid` (the default) or
        /// `panel` (the agent conversation's region).
        #[serde(default)]
        over: Option<ScrollRegion>,
    },
    /// `resize` — a viewport resize request in points, sent as a real
    /// `ViewportCommand::InnerSize` so the window manager round trip
    /// and the winsize settle run for real.
    Resize {
        /// The requested inner width.
        width: f32,
        /// The requested inner height.
        height: f32,
    },
    /// `pointer` — one pointer gesture piece delivered as real
    /// `RawInput` events through [`DriverInputPlugin`]: egui's own
    /// `PointerState` folds them at `begin_pass`, so hover, press,
    /// click and drag all run the production hit-test path (the
    /// `Shared::injected` stream can't — `dispatch_events` runs
    /// mid-pass, after the fold).
    Pointer {
        /// The gesture piece — `move`, `press`, `release` or
        /// `modifiers` (a bare modifier-set change).
        event: PointerEventKind,
        /// A hit-registry name — resolves to its rect's center.
        /// `x`/`y` and `target` are alternatives.
        #[serde(default)]
        target: Option<String>,
        /// A raw position in points — the escape hatch. `x` and `y`
        /// travel together.
        #[serde(default)]
        x: Option<f32>,
        /// See `x`.
        #[serde(default)]
        y: Option<f32>,
        /// Points added to the resolved position — drags step away
        /// from a named target without knowing its pixels.
        #[serde(default)]
        dx: f32,
        /// See `dx`.
        #[serde(default)]
        dy: f32,
        /// The button for `press`/`release` — default `primary`.
        #[serde(default)]
        button: Option<PointerButtonKind>,
        /// The held modifier set for this event (`"Ctrl"`,
        /// `"Ctrl+Shift"`, `""` for none) — emitted as a
        /// `ModifiersChanged` first, real input's order. Absent means
        /// "whatever the batch stream left held".
        #[serde(default)]
        modifiers: Option<String>,
    },
    /// `click` — a complete click on a registry `target` (or raw
    /// `x`/`y`): one frame batch carrying the pointer move and the
    /// press+release pair, so egui's own click detection fires.
    Click {
        /// The hit-registry name — fails `ok:false` when unregistered.
        #[serde(default)]
        target: Option<String>,
        /// A raw position in points — `x`/`y` travel together.
        #[serde(default)]
        x: Option<f32>,
        /// See `x`.
        #[serde(default)]
        y: Option<f32>,
        /// Points added to the resolved position.
        #[serde(default)]
        dx: f32,
        /// See `dx`.
        #[serde(default)]
        dy: f32,
        /// The button — default `primary`.
        #[serde(default)]
        button: Option<PointerButtonKind>,
        /// The modifier set held for the click only — the held state
        /// restores on the next frame.
        #[serde(default)]
        modifiers: Option<String>,
    },
    /// `agent` — stage one `AgentEvent` on the active tab's panel
    /// through `AgentPanel::apply` — the real approval flow's fold.
    Agent {
        /// The staged event — `approve` parks an `ApprovalRequest`'s
        /// gate card.
        event: AgentEventKind,
        /// The tool's registry name — the card's title.
        name: String,
        /// The renderable argument summary the card shows.
        #[serde(default)]
        summary: String,
    },
    /// `dump` — answer with the versioned state snapshot.
    Dump,
    /// `quit` — close the window the normal way.
    Quit,
}

/// `pointer`'s `event` — one gesture piece per request (each lands
/// in its own frame batch, the way real input arrives).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerEventKind {
    /// `Event::PointerMoved`.
    Move,
    /// `Event::PointerButton{pressed: true}`.
    Press,
    /// `Event::PointerButton{pressed: false}`.
    Release,
    /// `Event::ModifiersChanged` alone — set (`"Ctrl"`) or lift
    /// (`""`/absent) the held set.
    Modifiers,
}

/// The pointer button names the wire speaks — egui's
/// `PointerButton` minus the exotic extras.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerButtonKind {
    /// The primary (usually left) button.
    Primary,
    /// The secondary (usually right) button.
    Secondary,
    /// The middle button.
    Middle,
}

/// `agent`'s `event` — the stageable `AgentEvent` vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentEventKind {
    /// `AgentEvent::ApprovalRequest` — the approve gate card.
    Approve,
}

/// `key`'s `twin` — the `Event::Text` payload twin-emitting platforms
/// send beside a `Key` event.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum TwinSpec {
    /// `true` derives the chord's own character (`Alt+A` → `"a"`,
    /// `Alt+Shift+A` → `"A"`); `false` is no twin, same as absent.
    Flag(bool),
    /// The literal payload — a layout whose `Alt+A` composes `å`
    /// writes `"twin":"å"`.
    Text(String),
}

/// `scroll`'s `unit` — egui's [`egui::MouseWheelUnit`] spelled the
/// winit way, so a scenario reads like the event it reproduces.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollUnit {
    /// Whole lines — winit's `LineDelta`.
    #[serde(alias = "line_delta")]
    Line,
    /// Physical pixels — winit's `PixelDelta`, the touchpad shape
    /// whose sub-line deltas were the R13 dead-zone.
    #[serde(alias = "pixel_delta")]
    Pixel,
    /// Whole pages — `PageUp`/`PageDown` semantics.
    Page,
}

/// `scroll`'s `over` — the named region an explicit `x`/`y` replaces.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollRegion {
    /// The terminal grid's pane.
    Grid,
    /// The agent panel's conversation region.
    Panel,
}

/// Where a `scroll` lands — resolved against `GuiApp`'s last-painted
/// rects at dispatch time (they're frontend-local, like the wheel
/// remainders).
#[derive(Debug)]
pub enum ScrollTarget {
    /// The grid pane's center — the bare `scroll`'s default.
    Grid,
    /// The panel region's center, falling back to the grid when no
    /// agent panel is up (the region is plain grid then).
    Panel,
    /// An explicit position in points.
    At(egui::Pos2),
}

/// One response — `{"ok":true}` on success (`dump` adds its payload),
/// `{"ok":false,"error":"…"}` naming the failure otherwise. A
/// malformed request line earns the error shape, never silence.
#[derive(Debug, Serialize)]
pub struct Response {
    /// `true` on success.
    pub ok: bool,
    /// The failure message — present iff `ok` is `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `dump`'s answer — present iff the verb was `dump` and it
    /// succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dump: Option<Dump>,
}

impl Response {
    /// `{"ok":true}`.
    fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            dump: None,
        }
    }

    /// `{"ok":true,"dump":{…}}`.
    fn dump(dump: Dump) -> Self {
        Self {
            ok: true,
            dump: Some(dump),
            ..Self::ok()
        }
    }

    /// `{"ok":false,"error":"…"}`.
    fn error(message: String) -> Self {
        Self {
            ok: false,
            error: Some(message),
            dump: None,
        }
    }
}

/// The versioned state snapshot a `dump` request answers — v1 carries
/// the grid the paint pass draws, the pane the side region shows, and
/// the window's size. Later tickets extend it (explorer/browser
/// models, overlays, hit targets); field additions stay v1, shape
/// changes bump [`DUMP_VERSION`].
#[derive(Debug, Serialize)]
pub struct Dump {
    /// Protocol version — [`DUMP_VERSION`].
    pub v: u32,
    /// The active tab's session grid — `null` only with no tabs.
    pub grid: Option<GridDump>,
    /// The side region's state.
    pub pane: PaneDump,
    /// The find bar while it's open — `Some` IS its visibility (the
    /// `Shared::find` rule the overlays and hit highlights share).
    pub find: Option<FindDump>,
    /// Whether a suggestion is armed — the status line's `suggesting`
    /// segment paints on exactly this.
    pub suggesting: bool,
    /// The window's content area in points.
    pub window: WindowDump,
    /// The active tab's agent panel — `null` while no tab has ever
    /// opened one (the `tab.agent` Option mirrors through).
    pub agent: Option<AgentDump>,
    /// The hit-target registry — name → rect for every interactive
    /// surface the last painted frame registered. `pointer`'s and
    /// `click`'s `target` resolve against exactly this map, so the
    /// dump answers "what can I click" with the same data the verbs
    /// resolve from.
    pub hits: BTreeMap<String, RectDump>,
}

/// The active tab's visible grid — the same `visible_rows` the paint
/// pass plans from, plus the renderable cursor and the session's
/// cell size.
#[derive(Debug, Serialize)]
pub struct GridDump {
    /// The session's cell width.
    pub columns: usize,
    /// The session's cell height.
    pub lines: usize,
    /// Rows scrolled back into scrollback — the `⤓` jump-to-bottom
    /// pill paints on exactly `scroll_offset > 0`.
    pub scroll_offset: usize,
    /// Rows held in scrollback above the viewport.
    pub scrollback: usize,
    /// Every visible row's text, top to bottom, trailing whitespace
    /// trimmed (the `screen_text` convention).
    pub rows: Vec<String>,
    /// The renderable cursor.
    pub cursor: CursorDump,
    /// The drag-selection's normalized range — during and after a
    /// drag (the highlight's lifetime), `null` when none.
    pub selection: Option<SelectionDump>,
}

/// The cursor's viewport position and shape.
#[derive(Debug, Serialize)]
pub struct CursorDump {
    /// Viewport row.
    pub line: usize,
    /// Viewport column.
    pub column: usize,
    /// `block` / `hollow_block` / `underline` / `beam` / `hidden`.
    pub shape: &'static str,
}

/// The side region's state — what shows and what's selected beneath
/// an agent claim.
#[derive(Debug, Serialize)]
pub struct PaneDump {
    /// What the region shows: `agent`, an aux pane's name
    /// (`explorer`/`browser`), or `null` when the region is closed.
    pub shown: Option<&'static str>,
    /// The app-global aux selection — `explorer`/`browser`/`null`,
    /// independent of the agent claim.
    pub selected: Option<&'static str>,
}

/// The find bar's visible state while it's open — the same query,
/// hit count and position the bar itself paints.
#[derive(Debug, Serialize)]
pub struct FindDump {
    /// The query typed so far.
    pub query: String,
    /// The number of hits the grid holds.
    pub hits: usize,
    /// The 1-based `(current, total)` position over the hits — `null`
    /// with no hits at all.
    pub position: Option<(usize, usize)>,
}

/// The window's content rect in points — the same `content_rect` the
/// `ui` pass layouts against.
#[derive(Debug, Serialize)]
pub struct WindowDump {
    /// Content width.
    pub width: f32,
    /// Content height.
    pub height: f32,
}

/// A rectangle in points — the hit-target registry's value shape and
/// the shape every future rect dump reuses.
#[derive(Debug, Serialize)]
pub struct RectDump {
    /// The left edge.
    pub x: f32,
    /// The top edge.
    pub y: f32,
    /// The width.
    pub w: f32,
    /// The height.
    pub h: f32,
}

impl From<egui::Rect> for RectDump {
    fn from(rect: egui::Rect) -> Self {
        Self {
            x: rect.min.x,
            y: rect.min.y,
            w: rect.width(),
            h: rect.height(),
        }
    }
}

/// A grid position — `line` is absolute (scrollback rows are
/// negative), `column` a cell column.
#[derive(Debug, Serialize)]
pub struct PointDump {
    /// The absolute line.
    pub line: i32,
    /// The cell column.
    pub column: usize,
}

/// The drag-selection's normalized range — `start` ≤ `end` in
/// reading order.
#[derive(Debug, Serialize)]
pub struct SelectionDump {
    /// The earlier endpoint.
    pub start: PointDump,
    /// The later endpoint.
    pub end: PointDump,
}

/// The active tab's agent panel — the open/focus split a `click`
/// scenario drives the pane affordances through, plus the gate card.
#[derive(Debug, Serialize)]
pub struct AgentDump {
    /// `AgentPanel::is_open` — the region claim.
    pub open: bool,
    /// `AgentPanel::is_focused` — key routing.
    pub focused: bool,
    /// The gate card while one parks — `approve`/`ask`/`null` (the
    /// same `panel::gate` the painter draws).
    pub gate: Option<GateDump>,
}

/// The gate card — tagged on `kind` like the ctl protocol's verbs.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GateDump {
    /// `Gate::Approve` — an approve card, possibly mid-deny-prompt.
    Approve {
        /// The tool's registry name.
        name: String,
        /// `denying` — the deny reason prompt replaced the buttons.
        denying: bool,
    },
    /// `Gate::Ask` — a multiple-choice card.
    Ask {
        /// The question the card asks.
        question: String,
        /// The answer choices the card shows.
        choices: Vec<String>,
        /// The highlighted row.
        selected: usize,
    },
}

impl Dump {
    /// Snapshot `shared` — the state `ui`'s paint pass reads — plus
    /// the window's current content rect and the hit registry the
    /// last painted frame produced.
    fn capture(ctx: &egui::Context, shared: &Shared) -> Self {
        let grid = shared.tabs.active().map(|tab| {
            let cursor = tab.cursor();
            let size = tab.session.size();
            GridDump {
                columns: size.columns,
                lines: size.lines,
                scroll_offset: tab.scroll_offset(),
                scrollback: tab.scrollback(),
                rows: tab
                    .grid()
                    .visible_rows()
                    .map(|row| row.text().trim_end().to_owned())
                    .collect(),
                cursor: CursorDump {
                    line: cursor.line,
                    column: cursor.column,
                    shape: shape_name(cursor.shape),
                },
                selection: tab.selection.range().map(|range| SelectionDump {
                    start: PointDump {
                        line: range.start.line,
                        column: range.start.column,
                    },
                    end: PointDump {
                        line: range.end.line,
                        column: range.end.column,
                    },
                }),
            }
        });
        let area = ctx.content_rect();
        Self {
            v: DUMP_VERSION,
            grid,
            pane: PaneDump {
                shown: shared.shown_pane().map(side_pane_name),
                selected: shared.panes.selected().map(pane_name),
            },
            find: shared.find.as_ref().map(|find| FindDump {
                query: find.query().to_owned(),
                hits: find.hits().len(),
                position: find.position(),
            }),
            suggesting: shared.suggestion.is_some(),
            window: WindowDump {
                width: area.width(),
                height: area.height(),
            },
            agent: shared
                .tabs
                .active()
                .and_then(|tab| tab.agent_panel())
                .map(|panel| AgentDump {
                    open: panel.is_open(),
                    focused: panel.is_focused(),
                    gate: crate::panel::gate(panel).map(|gate| match gate {
                        crate::panel::Gate::Approve { name, denying } => {
                            GateDump::Approve {
                                name: name.to_owned(),
                                denying,
                            }
                        }
                        crate::panel::Gate::Ask(card) => GateDump::Ask {
                            question: card.question.clone(),
                            choices: card.choices.clone(),
                            selected: card.selected,
                        },
                    }),
                }),
            hits: shared
                .hits
                .entries()
                .map(|(name, rect)| (name.clone(), RectDump::from(*rect)))
                .collect(),
        }
    }
}

/// `ait_term::CursorShape` → the wire's snake-case name.
fn shape_name(shape: ait_term::CursorShape) -> &'static str {
    match shape {
        ait_term::CursorShape::Block => "block",
        ait_term::CursorShape::HollowBlock => "hollow_block",
        ait_term::CursorShape::Underline => "underline",
        ait_term::CursorShape::Beam => "beam",
        ait_term::CursorShape::Hidden => "hidden",
    }
}

/// A [`SidePane`] → its dump name.
fn side_pane_name(pane: SidePane) -> &'static str {
    match pane {
        SidePane::Agent => "agent",
        SidePane::Aux(id) => pane_name(id),
    }
}

/// A [`PaneId`] → its dump name.
fn pane_name(id: PaneId) -> &'static str {
    match id {
        PaneId::Explorer => "explorer",
        PaneId::Browser => "browser",
    }
}

/// One item the socket queued for the UI thread — `dispatch_events`
/// drains [`Shared::injected`] at frame start and prepends the events
/// to the frame's native input.
#[derive(Debug)]
pub enum Injected {
    /// A native-shaped `egui::Event` — key press/release, composed
    /// text, a clipboard paste. Everything downstream is production
    /// code.
    Event(egui::Event),
    /// A wheel gesture with its target resolved at dispatch — real
    /// wheel events route on `pointer.hover_pos`, which the cloned
    /// event vec can't reach (egui consumed the pointer state at
    /// `begin_pass`), so `scroll` carries the hover region instead.
    Wheel {
        /// The delta's unit — line, sub-line pixel or page.
        unit: egui::MouseWheelUnit,
        /// The delta in `unit`s — `y` positive scrolls back.
        delta: egui::Vec2,
        /// Where the gesture hovers.
        target: ScrollTarget,
    },
    /// A viewport resize request in points — dispatched as a real
    /// `ViewportCommand::InnerSize`, so the winsize settle path the
    /// scenario asserts is the production one end to end.
    Resize(egui::Vec2),
    /// A staged `AgentEvent` — `AgentPanel::apply` runs it in
    /// `dispatch_events`, the same fold `drain_agent_events` feeds
    /// real worker events through, so a staged approve card IS the
    /// panel's real approval flow.
    AgentEvent(ait_agent::AgentEvent),
    /// A marker that wakes one frame — `pointer`/`click` batches queue
    /// on [`DriverInputPlugin`] (their events must fold into
    /// `RawInput` at `begin_pass`, upstream of everything
    /// `dispatch_events` can reach), so the verb pushes this marker
    /// to earn the frame that delivers them.
    Repaint,
    /// Close the window — `GuiApp::quit`'s flag path.
    Quit,
}

/// The egui plugin that injects pointer batches into `RawInput` at
/// `begin_pass` — the only seam where injected events update
/// `InputState`/`PointerState` (hover, press, click, drag) like
/// winit-delivered input. `dispatch_events`' `Shared::injected`
/// stream runs mid-pass, after the fold already happened, which is
/// why wheel went through the app-side `wheel` route — pointer
/// gestures need the real thing.
#[derive(Debug, Default)]
pub struct DriverInputPlugin {
    /// One `pointer`/`click` request's events per entry — each
    /// `input_hook` delivers exactly one batch so multi-batch
    /// gestures (a click's trailing modifier restore, a
    /// press→drag→release) unfold across frames the way real input
    /// arrives.
    queue: VecDeque<Vec<egui::Event>>,
    /// The modifier set the queued batch stream leaves held —
    /// tracked at push time (not hook time) so a `PointerButton`
    /// event's `modifiers` field can reflect the stream's own
    /// `ModifiersChanged` events even before they deliver.
    held: egui::Modifiers,
}

impl DriverInputPlugin {
    /// Queue `batch` for delivery on a coming frame; folds its
    /// `ModifiersChanged` events into `held`.
    pub fn push(&mut self, batch: Vec<egui::Event>) {
        for event in &batch {
            if let egui::Event::ModifiersChanged(mods) = event {
                self.held = *mods;
            }
        }
        self.queue.push_back(batch);
    }

    /// The modifier set the queue's batches leave held — a pointer
    /// event's `modifiers` when its request doesn't name a set.
    pub fn held(&self) -> egui::Modifiers {
        self.held
    }
}

impl egui::Plugin for DriverInputPlugin {
    fn debug_name(&self) -> &'static str {
        "ait-test-driver"
    }

    fn input_hook(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
        let Some(batch) = self.queue.pop_front() else {
            return;
        };
        // Front of the queue — the injected events fold ahead of any
        // natively produced ones for the frame.
        input.events.splice(0..0, batch);
        // A delivered batch changes pointer state — the next frame
        // paints it (and delivers any queued follow-up batch).
        ctx.request_repaint();
    }
}

/// The boot-degradation text a bind failure produces — mirrors
/// [`ait_app::ctl::socket_error_text`]: a bind-failed run simply
/// never gets its driver socket; the toast names why.
pub fn socket_error_text(error: impl std::fmt::Display) -> String {
    format!("test-driver socket unavailable: {error}")
}

/// The per-request dispatch: the input verbs push onto
/// `shared.injected` for the next frame's `dispatch_events`, `dump`
/// answers from `shared` — the same state the paint pass reads.
fn dispatch(ctx: &egui::Context, shared: &mut Shared, request: &Request) -> Response {
    match request {
        Request::Key { chord, twin } => match parse_chord(chord) {
            Ok((key, modifiers)) => {
                let twin = match twin_text(twin.as_ref(), chord, key, modifiers) {
                    Ok(twin) => twin,
                    Err(error) => return Response::error(error),
                };
                // Press, then the platform text twin, then release —
                // the order egui-winit produces from one winit
                // `KeyboardInput` event carrying `text` (the release
                // never carries one). The release is a no-op
                // downstream; the press does the work.
                shared.injected.push_back(Injected::Event(egui::Event::Key {
                    key,
                    physical_key: Some(key),
                    pressed: true,
                    repeat: false,
                    modifiers,
                }));
                if let Some(text) = twin {
                    shared
                        .injected
                        .push_back(Injected::Event(egui::Event::Text(text)));
                }
                shared.injected.push_back(Injected::Event(egui::Event::Key {
                    key,
                    physical_key: Some(key),
                    pressed: false,
                    repeat: false,
                    modifiers,
                }));
                Response::ok()
            }
            Err(error) => Response::error(error),
        },
        Request::Text { text } => {
            shared
                .injected
                .push_back(Injected::Event(egui::Event::Text(text.clone())));
            Response::ok()
        }
        Request::Paste { text } => {
            shared
                .injected
                .push_back(Injected::Event(egui::Event::Paste(text.clone())));
            Response::ok()
        }
        Request::Scroll {
            unit,
            dx,
            dy,
            x,
            y,
            over,
        } => match scroll_target(*x, *y, *over) {
            Ok(target) => {
                shared.injected.push_back(Injected::Wheel {
                    unit: wheel_unit(*unit),
                    delta: egui::Vec2::new(*dx, *dy),
                    target,
                });
                Response::ok()
            }
            Err(error) => Response::error(error),
        },
        Request::Resize { width, height } => {
            if !width.is_finite() || !height.is_finite() || *width <= 0.0 || *height <= 0.0 {
                return Response::error(format!(
                    "resize needs finite positive points, got {width}x{height}"
                ));
            }
            shared
                .injected
                .push_back(Injected::Resize(egui::Vec2::new(*width, *height)));
            Response::ok()
        }
        Request::Pointer {
            event,
            target,
            x,
            y,
            dx,
            dy,
            button,
            modifiers,
        } => {
            let parsed = match modifiers
                .as_deref()
                .map(parse_modifiers)
                .transpose()
            {
                Ok(mods) => mods,
                Err(error) => return Response::error(error),
            };
            // A bare `modifiers` gesture needs no position.
            let pos = if matches!(event, PointerEventKind::Modifiers) {
                None
            } else {
                match pointer_pos(shared, target.as_deref(), *x, *y, *dx, *dy) {
                    Ok(pos) => Some(pos),
                    Err(error) => return Response::error(error),
                }
            };
            let pushed = ctx.with_plugin(|plugin: &mut DriverInputPlugin| {
                let batch = pointer_batch(
                    *event,
                    pos,
                    button.unwrap_or(PointerButtonKind::Primary),
                    parsed,
                    plugin.held(),
                );
                plugin.push(batch);
            });
            match pushed {
                Some(()) => {
                    shared.injected.push_back(Injected::Repaint);
                    Response::ok()
                }
                None => Response::error(
                    "the pointer plugin is not registered".to_owned(),
                ),
            }
        }
        Request::Click {
            target,
            x,
            y,
            dx,
            dy,
            button,
            modifiers,
        } => {
            let pos = match pointer_pos(shared, target.as_deref(), *x, *y, *dx, *dy) {
                Ok(pos) => pos,
                Err(error) => return Response::error(error),
            };
            let parsed = match modifiers
                .as_deref()
                .map(parse_modifiers)
                .transpose()
            {
                Ok(mods) => mods,
                Err(error) => return Response::error(error),
            };
            let button = egui::PointerButton::from(button.unwrap_or(
                PointerButtonKind::Primary,
            ));
            let pushed = ctx.with_plugin(|plugin: &mut DriverInputPlugin| {
                let mods = parsed.unwrap_or_else(|| plugin.held());
                let mut batch = Vec::with_capacity(4);
                if parsed.is_some() {
                    batch.push(egui::Event::ModifiersChanged(mods));
                }
                batch.push(egui::Event::PointerMoved(pos));
                batch.push(egui::Event::PointerButton {
                    pos,
                    button,
                    pressed: true,
                    modifiers: mods,
                });
                batch.push(egui::Event::PointerButton {
                    pos,
                    button,
                    pressed: false,
                    modifiers: mods,
                });
                let before = plugin.held();
                plugin.push(batch);
                if parsed.is_some() {
                    // The click's set is a gesture, not a hold — the
                    // next frame restores whatever was held before it.
                    plugin.push(vec![egui::Event::ModifiersChanged(before)]);
                }
            });
            match pushed {
                Some(()) => {
                    shared.injected.push_back(Injected::Repaint);
                    Response::ok()
                }
                None => Response::error(
                    "the pointer plugin is not registered".to_owned(),
                ),
            }
        }
        Request::Agent {
            event,
            name,
            summary,
        } => {
            if !shared
                .tabs
                .active()
                .and_then(|tab| tab.agent_panel())
                .is_some_and(|panel| panel.is_open())
            {
                return Response::error(
                    "`agent` needs the panel open — send `key \"Alt+A\"` first".to_owned(),
                );
            }
            let staged = match event {
                AgentEventKind::Approve => ait_agent::AgentEvent::ApprovalRequest {
                    call: ait_agent::ToolCall {
                        id: format!("test-driver.{name}"),
                        name: name.clone(),
                        arguments: "{}".to_owned(),
                    },
                    summary: summary.clone(),
                },
            };
            shared.injected.push_back(Injected::AgentEvent(staged));
            Response::ok()
        }
        Request::Dump => Response::dump(Dump::capture(ctx, shared)),
        Request::Quit => {
            shared.injected.push_back(Injected::Quit);
            Response::ok()
        }
    }
}

/// `pointer`'s `modifiers` field → the held `egui::Modifiers` set.
/// Modifier-only names only (`alt`/`ctrl`/`shift`/`super`) joined by
/// `+`; `""` or `none` is the empty set. Unlike `parse_chord` there
/// is no key to name — a chord is the wrong shape for a held set.
fn parse_modifiers(text: &str) -> Result<egui::Modifiers, String> {
    if text.is_empty() || text.eq_ignore_ascii_case("none") {
        return Ok(egui::Modifiers::NONE);
    }
    let mut mods = egui::Modifiers::NONE;
    for name in text.split('+').filter(|part| !part.is_empty()) {
        match name.to_ascii_lowercase().as_str() {
            "alt" => mods.alt = true,
            "ctrl" | "control" => mods.ctrl = true,
            "shift" => mods.shift = true,
            "super" | "meta" | "cmd" => mods.mac_cmd = true,
            _ => {
                return Err(format!(
                    "modifier set {text:?} names no modifier at {name:?} — \
                     the held set is alt/ctrl/shift/super joined by `+`"
                ))
            }
        }
    }
    mods.command = mods.ctrl || mods.mac_cmd;
    Ok(mods)
}

/// A `pointer`/`click` request's position: `target` resolves a hit
/// registry name to its rect's center, `x`/`y` is the raw escape
/// hatch — plus the `dx`/`dy` offset. The name lookup answers the
/// registry's contents in the error so a bad name names the truth.
fn pointer_pos(
    shared: &Shared,
    target: Option<&str>,
    x: Option<f32>,
    y: Option<f32>,
    dx: f32,
    dy: f32,
) -> Result<egui::Pos2, String> {
    let offset = egui::vec2(dx, dy);
    match (target, x, y) {
        (Some(name), None, None) => shared
            .hits
            .get(name)
            .map(|rect| rect.center() + offset)
            .ok_or_else(|| unknown_target(shared, name)),
        (None, Some(x), Some(y)) => Ok(egui::pos2(x, y) + offset),
        (None, None, None) => {
            Err("pointer needs a `target` name or an `x`/`y` pair".to_owned())
        }
        (Some(_), _, _) => Err(
            "`target` and `x`/`y` are alternatives, not partners".to_owned(),
        ),
        _ => Err("`x` and `y` travel together".to_owned()),
    }
}

/// The `unknown target` error — names the miss and lists what the
/// last painted frame registered, so a typo'd target fails naming
/// the real vocabulary.
fn unknown_target(shared: &Shared, name: &str) -> String {
    let names: Vec<&str> = shared
        .hits
        .entries()
        .map(|(name, _)| name.as_str())
        .collect();
    match names.is_empty() {
        true => format!("unknown hit target {name:?} — nothing registered"),
        false => format!("unknown hit target {name:?} — registered: {}", names.join(", ")),
    }
}

/// One `pointer` request's batch: the `ModifiersChanged` first (real
/// input's order — the modifier lands, then the gesture under it),
/// then the pointer event. `modifiers` names the whole held set;
/// absent, the event carries the stream's `held` state.
fn pointer_batch(
    event: PointerEventKind,
    pos: Option<egui::Pos2>,
    button: PointerButtonKind,
    modifiers: Option<egui::Modifiers>,
    held: egui::Modifiers,
) -> Vec<egui::Event> {
    let mut batch = Vec::new();
    if let Some(mods) = modifiers {
        batch.push(egui::Event::ModifiersChanged(mods));
    }
    match event {
        PointerEventKind::Move => batch.push(egui::Event::PointerMoved(
            pos.expect("move resolves a position"),
        )),
        PointerEventKind::Press | PointerEventKind::Release => {
            batch.push(egui::Event::PointerButton {
                pos: pos.expect("a button event resolves a position"),
                button: egui::PointerButton::from(button),
                pressed: matches!(event, PointerEventKind::Press),
                modifiers: modifiers.unwrap_or(held),
            });
        }
        PointerEventKind::Modifiers => {
            if modifiers.is_none() {
                // `{"event":"modifiers"}` with no set lifts all.
                batch.push(egui::Event::ModifiersChanged(egui::Modifiers::NONE));
            }
        }
    }
    batch
}

/// `PointerButtonKind` → `egui::PointerButton`.
impl From<PointerButtonKind> for egui::PointerButton {
    fn from(kind: PointerButtonKind) -> Self {
        match kind {
            PointerButtonKind::Primary => Self::Primary,
            PointerButtonKind::Secondary => Self::Secondary,
            PointerButtonKind::Middle => Self::Middle,
        }
    }
}

/// `key`'s `twin` → the `Event::Text` payload to emit between press
/// and release: `true` derives the chord's own character through the
/// same `char_key` the production translator uses; a string is the
/// payload verbatim.
fn twin_text(
    twin: Option<&TwinSpec>,
    chord: &str,
    key: egui::Key,
    modifiers: egui::Modifiers,
) -> Result<Option<String>, String> {
    match twin {
        None | Some(TwinSpec::Flag(false)) => Ok(None),
        Some(TwinSpec::Text(text)) => Ok(Some(text.clone())),
        Some(TwinSpec::Flag(true)) => match crate::keys::char_key(key, modifiers.shift) {
            Some(c) => Ok(Some(c.to_string())),
            None => Err(format!(
                "chord {chord:?} has no character twin to derive — \
                 pass an explicit \"twin\":\"<text>\" instead"
            )),
        },
    }
}

/// A `scroll` request's target: explicit `x`/`y` wins and must travel
/// as a pair; else the named region; else the grid.
fn scroll_target(
    x: Option<f32>,
    y: Option<f32>,
    over: Option<ScrollRegion>,
) -> Result<ScrollTarget, String> {
    match (x, y) {
        (Some(x), Some(y)) => Ok(ScrollTarget::At(egui::Pos2::new(x, y))),
        (None, None) => Ok(match over {
            Some(ScrollRegion::Panel) => ScrollTarget::Panel,
            Some(ScrollRegion::Grid) | None => ScrollTarget::Grid,
        }),
        _ => Err("scroll's `x` and `y` travel together or not at all".to_owned()),
    }
}

/// The wire's unit names → egui's.
fn wheel_unit(unit: ScrollUnit) -> egui::MouseWheelUnit {
    match unit {
        ScrollUnit::Line => egui::MouseWheelUnit::Line,
        ScrollUnit::Pixel => egui::MouseWheelUnit::Point,
        ScrollUnit::Page => egui::MouseWheelUnit::Page,
    }
}

/// A `key` chord → the egui key and *source* modifier bits a real
/// press carries (`command` derived the way winit derives it — `ctrl`
/// off macOS, `mac_cmd` on it). The grammar mirrors the testkit's
/// `parse_chord`: `+`-separated modifier names (`Alt`, `Ctrl`,
/// `Shift`, `Super`), then a named key (`Enter`, `Escape`, `Tab`,
/// `Backspace`, `Space`, the arrows, `Home`, `End`, `PageUp`,
/// `PageDown`, `Insert`, `Delete`), `F1`–`F12`, or one character —
/// case-insensitive because `egui::Key` is caseless (shift carries
/// the case downstream in `translate_key`).
fn parse_chord(chord: &str) -> Result<(egui::Key, egui::Modifiers), String> {
    let fail = |reason: &str| format!("invalid chord {chord:?}: {reason}");
    let (mods_part, key_part) = match chord.rsplit_once('+') {
        Some((mods, key)) if !mods.is_empty() => (mods, key),
        _ => ("", chord),
    };
    let (mut ctrl, mut shift, mut alt, mut mac_cmd) = (false, false, false, false);
    for name in mods_part.split('+').filter(|part| !part.is_empty()) {
        match name.to_ascii_lowercase().as_str() {
            "alt" => alt = true,
            "ctrl" | "control" => ctrl = true,
            "shift" => shift = true,
            "super" | "meta" | "cmd" => mac_cmd = true,
            _ => return Err(fail("unknown modifier name")),
        }
    }
    let key = named_key(key_part)
        .or_else(|| function_key(key_part))
        .or_else(|| char_key(key_part))
        .ok_or_else(|| fail("unknown key name"))?;
    Ok((
        key,
        egui::Modifiers {
            alt,
            ctrl,
            shift,
            mac_cmd,
            command: ctrl || mac_cmd,
        },
    ))
}

/// The named (non-character) keys — the testkit's grammar, egui keys.
fn named_key(name: &str) -> Option<egui::Key> {
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => egui::Key::Enter,
        "escape" | "esc" => egui::Key::Escape,
        "tab" => egui::Key::Tab,
        "backspace" => egui::Key::Backspace,
        "space" => egui::Key::Space,
        "up" => egui::Key::ArrowUp,
        "down" => egui::Key::ArrowDown,
        "left" => egui::Key::ArrowLeft,
        "right" => egui::Key::ArrowRight,
        "home" => egui::Key::Home,
        "end" => egui::Key::End,
        "pageup" => egui::Key::PageUp,
        "pagedown" => egui::Key::PageDown,
        "insert" => egui::Key::Insert,
        "delete" | "del" => egui::Key::Delete,
        _ => return None,
    })
}

/// `F1`–`F12` — the testkit's range; other numbers are unknown keys.
fn function_key(name: &str) -> Option<egui::Key> {
    let number: u8 = name
        .strip_prefix('f')
        .or_else(|| name.strip_prefix('F'))?
        .parse()
        .ok()?;
    Some(match number {
        1 => egui::Key::F1,
        2 => egui::Key::F2,
        3 => egui::Key::F3,
        4 => egui::Key::F4,
        5 => egui::Key::F5,
        6 => egui::Key::F6,
        7 => egui::Key::F7,
        8 => egui::Key::F8,
        9 => egui::Key::F9,
        10 => egui::Key::F10,
        11 => egui::Key::F11,
        12 => egui::Key::F12,
        _ => return None,
    })
}

/// One character → its `egui::Key`: letters map caselessly (`e` and
/// `E` are both `Key::E` — shift is a modifier bit, not a different
/// key), digits and the punctuation `keys.rs`'s `char_key` speaks.
/// Anything egui has no key for is an unknown key — composed text
/// like `é` is the `text` verb's job, never a key name.
fn char_key(name: &str) -> Option<egui::Key> {
    let mut chars = name.chars();
    let c = match (chars.next(), chars.next()) {
        (Some(c), None) => c,
        _ => return None,
    };
    if c.is_ascii_alphabetic() {
        return Some(match c.to_ascii_uppercase() {
            'A' => egui::Key::A,
            'B' => egui::Key::B,
            'C' => egui::Key::C,
            'D' => egui::Key::D,
            'E' => egui::Key::E,
            'F' => egui::Key::F,
            'G' => egui::Key::G,
            'H' => egui::Key::H,
            'I' => egui::Key::I,
            'J' => egui::Key::J,
            'K' => egui::Key::K,
            'L' => egui::Key::L,
            'M' => egui::Key::M,
            'N' => egui::Key::N,
            'O' => egui::Key::O,
            'P' => egui::Key::P,
            'Q' => egui::Key::Q,
            'R' => egui::Key::R,
            'S' => egui::Key::S,
            'T' => egui::Key::T,
            'U' => egui::Key::U,
            'V' => egui::Key::V,
            'W' => egui::Key::W,
            'X' => egui::Key::X,
            'Y' => egui::Key::Y,
            'Z' => egui::Key::Z,
            _ => unreachable!("is_ascii_alphabetic bounded it"),
        });
    }
    Some(match c {
        ' ' => egui::Key::Space,
        '0' => egui::Key::Num0,
        '1' => egui::Key::Num1,
        '2' => egui::Key::Num2,
        '3' => egui::Key::Num3,
        '4' => egui::Key::Num4,
        '5' => egui::Key::Num5,
        '6' => egui::Key::Num6,
        '7' => egui::Key::Num7,
        '8' => egui::Key::Num8,
        '9' => egui::Key::Num9,
        ':' => egui::Key::Colon,
        ',' => egui::Key::Comma,
        '\\' => egui::Key::Backslash,
        '/' => egui::Key::Slash,
        '|' => egui::Key::Pipe,
        '?' => egui::Key::Questionmark,
        '!' => egui::Key::Exclamationmark,
        '[' => egui::Key::OpenBracket,
        ']' => egui::Key::CloseBracket,
        '{' => egui::Key::OpenCurlyBracket,
        '}' => egui::Key::CloseCurlyBracket,
        '`' => egui::Key::Backtick,
        '-' => egui::Key::Minus,
        '.' => egui::Key::Period,
        '+' => egui::Key::Plus,
        '=' => egui::Key::Equals,
        ';' => egui::Key::Semicolon,
        '\'' => egui::Key::Quote,
        _ => return None,
    })
}

// ------------------------------------------------------------------
// Server: the socket the pump drains — ctl's shape, cut to the four
// verbs above.
// ------------------------------------------------------------------

/// Whether a pending connection survives this service pass.
enum Service {
    /// Keep it for the next pass (request incomplete, reply stalled).
    Keep,
    /// Drop it — answered, expired, or the client went away.
    Close,
}

/// One accepted connection buffered across passes: request bytes
/// until a full line exists, then the response flushing out.
struct Pending {
    /// The accepted (nonblocking) stream.
    stream: UnixStream,
    /// Buffered request bytes — a complete request ends at `'\n'`.
    request: Vec<u8>,
    /// The response being flushed — `(bytes, written)`.
    reply: Option<(Vec<u8>, usize)>,
    /// When the connection was accepted — the [`PENDING_TTL`] bound.
    at: Instant,
}

impl Pending {
    /// A fresh accepted connection.
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            request: Vec::new(),
            reply: None,
            at: Instant::now(),
        }
    }

    /// Service the connection once: flush a queued reply, else buffer
    /// request bytes until a full line exists and dispatch it.
    /// Returns the verdict plus the injected-input count the pass
    /// needs a frame for (`dump` answers without one — a poll that
    /// repainted every 5 ms would idle-spin the window).
    fn service(&mut self, ctx: &egui::Context, shared: &mut Shared) -> (Service, usize) {
        if self.at.elapsed() > PENDING_TTL {
            return (Service::Close, 0);
        }
        if self.reply.is_some() {
            return (self.flush(), 0);
        }
        let mut chunk = [0u8; 8192];
        match self.stream.read(&mut chunk) {
            Ok(0) => (Service::Close, 0),
            Ok(n) => {
                self.request.extend_from_slice(&chunk[..n]);
                match self.request.iter().position(|b| *b == b'\n') {
                    Some(end) if end > REQUEST_CAP => (self.fail("request too large"), 0),
                    Some(end) => {
                        let line = String::from_utf8_lossy(&self.request[..end]).into_owned();
                        self.answer(ctx, shared, &line)
                    }
                    None if self.request.len() > REQUEST_CAP => (self.fail("request too large"), 0),
                    None => (Service::Keep, 0),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => (Service::Keep, 0),
            Err(_) => (Service::Close, 0),
        }
    }

    /// Decode + dispatch + queue the response — even a malformed line
    /// earns a well-formed `{"ok":false}` before the close. The
    /// returned count is the injected items the dispatch queued.
    fn answer(&mut self, ctx: &egui::Context, shared: &mut Shared, line: &str) -> (Service, usize) {
        let injected_before = shared.injected.len();
        let response = match decode_request(line) {
            Ok(request) => dispatch(ctx, shared, &request),
            Err(error) => Response::error(error),
        };
        (
            self.queue(response),
            shared.injected.len() - injected_before,
        )
    }

    /// Queue `response` as the reply line and start flushing it.
    fn queue(&mut self, response: Response) -> Service {
        let mut line = encode_response(&response);
        line.push('\n');
        self.reply = Some((line.into_bytes(), 0));
        self.flush()
    }

    /// `queue`'s string form — the oversized-request rejection.
    fn fail(&mut self, message: &str) -> Service {
        self.queue(Response::error(message.to_owned()))
    }

    /// Write out the queued reply — a full flush closes the
    /// connection (one request per invocation), a `WouldBlock` keeps
    /// it for the next pass.
    fn flush(&mut self) -> Service {
        use std::net::Shutdown;
        let Some((bytes, mut written)) = self.reply.take() else {
            return Service::Close;
        };
        while written < bytes.len() {
            match self.stream.write(&bytes[written..]) {
                Ok(0) => break,
                Ok(n) => written += n,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.reply = Some((bytes, written));
                    return Service::Keep;
                }
                Err(_) => return Service::Close,
            }
        }
        // Half-close so the client's read sees the line's end even
        // before its own side closes.
        let _ = self.stream.shutdown(Shutdown::Write);
        Service::Close
    }
}

/// Decode one request line — anything that isn't one [`Request`]
/// line is an error naming the failure.
fn decode_request(line: &str) -> Result<Request, String> {
    serde_json::from_str(line.trim()).map_err(|e| format!("malformed request: {e}"))
}

/// Encode one response to its single-line JSON form.
fn encode_response(response: &Response) -> String {
    serde_json::to_string(response).expect("Response serialization is infallible")
}

/// `path` at `mode` — the socket's `0600` (same-uid is the trust
/// boundary), the parent dir's `0700`.
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// The test-driver socket: a nonblocking `UnixListener` at the path
/// [`ENV_DRIVER_SOCKET`] named, drained in the pump loop beside ctl.
pub struct TestDriver {
    /// The nonblocking listener.
    listener: UnixListener,
    /// The bound socket path — unlinked on [`Drop`].
    socket: PathBuf,
    /// Accepted connections mid-request.
    pending: Vec<Pending>,
}

impl TestDriver {
    /// Bind at the path [`ENV_DRIVER_SOCKET`] names — `None` when the
    /// variable is unset or empty (ordinary runs never reach this
    /// code; only harness-spawned instances expose a socket).
    pub fn bind_env() -> Option<io::Result<Self>> {
        let path = std::env::var_os(ENV_DRIVER_SOCKET)?;
        if path.is_empty() {
            return None;
        }
        Some(Self::bind(PathBuf::from(path)))
    }

    /// Bind at an explicit path — [`bind_env`](Self::bind_env)'s
    /// delegate and the test seam. The parent dir is created `0700`,
    /// the socket chmodded `0600`, and an existing file unlinked
    /// first — a crashed run's stale socket is reclaimed, never a
    /// bind failure.
    pub fn bind(socket: PathBuf) -> io::Result<Self> {
        if let Some(dir) = socket.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
                set_mode(dir, 0o700)?;
            }
        }
        if socket.exists() {
            std::fs::remove_file(&socket)?;
        }
        let listener = UnixListener::bind(&socket)?;
        set_mode(&socket, 0o600)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            socket,
            pending: Vec::new(),
        })
    }

    /// The bound socket's path — the tests' seam; production callers
    /// already know the path (they chose it).
    #[allow(dead_code)]
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// One drain pass: accept every queued connection, buffer request
    /// lines, dispatch completed ones and flush responses — all
    /// nonblocking, so a slow client never stalls the pump. Returns
    /// the count of injected items queued this pass — the repaint
    /// decision (a `dump`-only pass needs no frame).
    pub fn drain(&mut self, ctx: &egui::Context, shared: &mut Shared) -> usize {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    // An accept that can't go nonblocking is dropped —
                    // a blocking conn in `pending` would wedge the pass.
                    if stream.set_nonblocking(true).is_ok() {
                        self.pending.push(Pending::new(stream));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        let mut injected = 0;
        for mut conn in std::mem::take(&mut self.pending) {
            let (verdict, count) = conn.service(ctx, shared);
            injected += count;
            if let Service::Keep = verdict {
                self.pending.push(conn);
            }
        }
        injected
    }
}

impl Drop for TestDriver {
    /// Clean exit unlinks the socket — the ctl convention.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

#[cfg(test)]
mod tests {
    //! The driver's seams at the level the pump drains them: the codec
    //! (wire shapes, malformed lines), the chord parser, the socket
    //! round trip against a real `Shared` (injected events land, the
    //! dump answers post-drain state) — all headless: none of it needs
    //! a window, only the `egui::Context::default()` the pump's own
    //! tests also drive.

    use super::*;
    use ait_agent::CommandSuggester;
    use ait_app::config::{self, LiveSettings};
    use ait_app::profile;
    use ait_app::skills::LiveSkills;
    use ait_app::store::StoreHandle;
    use ait_app::suggest::SuggestionPin;
    use ait_app::tabs::{SessionTemplate, TabManager};
    use ait_config::Settings;
    use ait_term::Size;
    use eframe::egui::Plugin;
    use std::collections::VecDeque;
    use std::os::unix::fs::PermissionsExt;

    /// A `Shared` built like `GuiApp::new`'s minus the store, over a
    /// real interactive `/bin/sh` tab — the pump tests' shape.
    fn shared() -> Shared {
        Shared {
            tabs: TabManager::spawn_with(
                Size::new(80, 24),
                SessionTemplate {
                    options: ait_term::LiveSessionOptions {
                        program: Some(std::ffi::OsString::from("/bin/sh")),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .unwrap(),
            config: LiveSettings::adopt(Settings::default()),
            store: StoreHandle::disabled(),
            toast: None,
            skills: LiveSkills::unwatched(),
            profile: profile::DEFAULT_PROFILE.to_string(),
            picker: None,
            conversations: None,
            copied_text: None,
            suggester: CommandSuggester::with_secret_store(config::NoSecrets),
            suggestion: SuggestionPin::new(),
            find: None,
            panes: crate::pane::PaneHost::default(),
            explorer: crate::explorer::ExplorerState::default(),
            browser: crate::browser::BrowserState::default(),
            injected: VecDeque::new(),
            hits: crate::app::HitTargets::default(),
        }
    }

    /// The codec half: every verb decodes from its wire shape and a
    /// malformed line is an error, never a silent default.
    #[test]
    fn requests_decode_from_their_wire_shapes() {
        assert!(matches!(
            decode_request(r#"{"verb":"key","chord":"Ctrl+Shift+E"}"#),
            Ok(Request::Key { twin: None, .. })
        ));
        assert!(matches!(
            decode_request(r#"{"verb":"key","chord":"Alt+A","twin":true}"#),
            Ok(Request::Key {
                twin: Some(TwinSpec::Flag(true)),
                ..
            })
        ));
        assert!(matches!(
            decode_request(r#"{"verb":"key","chord":"Alt+A","twin":"å"}"#),
            Ok(Request::Key {
                twin: Some(TwinSpec::Text(_)),
                ..
            })
        ));
        assert!(matches!(
            decode_request(r#"{"verb":"text","text":"echo hi"}"#),
            Ok(Request::Text { .. })
        ));
        assert!(matches!(
            decode_request(r#"{"verb":"paste","text":"echo hi"}"#),
            Ok(Request::Paste { .. })
        ));
        assert!(matches!(
            decode_request(r#"{"verb":"scroll","unit":"pixel","dy":3.0}"#),
            Ok(Request::Scroll {
                unit: ScrollUnit::Pixel,
                ..
            })
        ));
        // The winit spellings alias — a scenario reads like the event
        // it reproduces.
        assert!(matches!(
            decode_request(r#"{"verb":"scroll","unit":"line_delta","dy":1.0,"over":"panel"}"#),
            Ok(Request::Scroll {
                unit: ScrollUnit::Line,
                over: Some(ScrollRegion::Panel),
                ..
            })
        ));
        assert!(matches!(
            decode_request(r#"{"verb":"resize","width":1100,"height":700}"#),
            Ok(Request::Resize { .. })
        ));
        assert!(matches!(
            decode_request(
                r#"{"verb":"pointer","event":"press","target":"grid","modifiers":"Ctrl"}"#
            ),
            Ok(Request::Pointer {
                event: PointerEventKind::Press,
                ..
            })
        ));
        assert!(matches!(
            decode_request(r#"{"verb":"pointer","event":"move","x":10,"y":20,"dx":1}"#),
            Ok(Request::Pointer {
                event: PointerEventKind::Move,
                x: Some(10.0),
                y: Some(20.0),
                dx: 1.0,
                ..
            })
        ));
        assert!(matches!(
            decode_request(r#"{"verb":"click","target":"gate.approve"}"#),
            Ok(Request::Click { .. })
        ));
        assert!(matches!(
            decode_request(r#"{"verb":"agent","event":"approve","name":"run"}"#),
            Ok(Request::Agent {
                event: AgentEventKind::Approve,
                ..
            })
        ));
        assert!(matches!(
            decode_request(r#"{"verb":"dump"}"#),
            Ok(Request::Dump)
        ));
        assert!(matches!(
            decode_request(r#"{"verb":"quit"}"#),
            Ok(Request::Quit)
        ));
        for line in [
            "",
            "not json",
            r#"{"verb":"bogus"}"#,
            r#"{"verb":"key"}"#,                                // missing chord
            r#"{"verb":"key","chord":1}"#,                      // wrong type
            r#"{"verb":"key","chord":"a","twin":1}"#,           // twin is bool|string
            r#"{"verb":"scroll","dy":3.0}"#,                    // missing unit
            r#"{"verb":"scroll","unit":"inch","dy":1}"#,        // unknown unit
            r#"{"verb":"scroll","unit":"line","over":"moon"}"#, // unknown region
            r#"{"verb":"resize","width":1100}"#,                // missing height
            r#"{"verb":"pointer"}"#,                            // missing event
            r#"{"verb":"pointer","event":"blink"}"#,            // unknown piece
            r#"{"verb":"pointer","event":"press","button":"extra"}"#, // unknown button
            r#"{"verb":"agent","event":"refuse","name":"x"}"#,  // unknown stageable
            r#"{"verb":"agent","event":"approve"}"#,            // missing name
            r#"{"verb":"send","text":"x"}"#,                    // a ctl verb — NOT this socket
        ] {
            assert!(decode_request(line).is_err(), "{line:?}");
        }
    }

    /// The response shapes: bare `ok` for verbs, `ok`+payload for
    /// `dump`, `ok:false`+message for failures.
    #[test]
    fn responses_encode_their_contract() {
        assert_eq!(encode_response(&Response::ok()), r#"{"ok":true}"#);
        assert_eq!(
            encode_response(&Response::error("bad chord".to_owned())),
            r#"{"ok":false,"error":"bad chord"}"#
        );
        let mut shared = shared();
        let response = dispatch(&egui::Context::default(), &mut shared, &Request::Dump);
        let line = encode_response(&response);
        assert!(line.contains(r#""ok":true"#), "{line}");
        assert!(line.contains(r#""v":1"#), "{line}");
    }

    /// Chords resolve to their key and the SOURCE modifier bits —
    /// `Ctrl+Shift+E` is `Key::E` + ctrl|shift with `command` derived
    /// like winit's, `Alt+A` is `Key::A` + alt.
    #[test]
    fn chords_parse_to_keys_and_source_bits() {
        let (key, mods) = parse_chord("Ctrl+Shift+E").unwrap();
        assert_eq!(key, egui::Key::E);
        assert!(mods.ctrl && mods.shift && !mods.alt && !mods.mac_cmd);
        assert!(mods.command, "ctrl derives command off macOS");
        let (key, mods) = parse_chord("Alt+A").unwrap();
        assert_eq!(key, egui::Key::A);
        assert!(mods.alt && !mods.ctrl);
        // Named keys, function keys, caseless letters.
        assert_eq!(parse_chord("Enter").unwrap().0, egui::Key::Enter);
        assert_eq!(parse_chord("F5").unwrap().0, egui::Key::F5);
        assert_eq!(parse_chord("e").unwrap().0, egui::Key::E);
        for chord in ["", "Ctrl+Bogus", "Alt+F13", "Ctrl+", "Alt+é", "x+y"] {
            assert!(parse_chord(chord).is_err(), "{chord:?}");
        }
    }

    /// The full socket round trip through `drain`: `key` queues a
    /// press+release pair (the release second), `text` queues the
    /// composed event, `quit` queues `Injected::Quit`, `dump` answers
    /// in-band and queues nothing.
    #[test]
    fn drain_services_requests_over_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = TestDriver::bind(dir.path().join("driver.sock")).unwrap();
        assert_eq!(
            std::fs::metadata(server.socket_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let ctx = egui::Context::default();
        let mut shared = shared();

        // One request per connection — the ctl convention.
        let mut conn = UnixStream::connect(server.socket_path()).unwrap();
        writeln!(conn, r#"{{"verb":"key","chord":"Ctrl+Shift+E"}}"#).unwrap();
        assert!(server.drain(&ctx, &mut shared) > 0);
        assert_eq!(shared.injected.len(), 2, "press + release");
        let mut response = String::new();
        conn.read_to_string(&mut response).unwrap();
        assert!(response.contains(r#""ok":true"#), "{response}");
        match shared.injected.pop_front().unwrap() {
            Injected::Event(egui::Event::Key {
                key,
                pressed,
                modifiers,
                ..
            }) => {
                assert_eq!(key, egui::Key::E);
                assert!(pressed && modifiers.ctrl && modifiers.shift);
            }
            other => panic!("expected the key press, got {other:?}"),
        }
        match shared.injected.pop_front().unwrap() {
            Injected::Event(egui::Event::Key { pressed, .. }) => assert!(!pressed),
            other => panic!("expected the key release, got {other:?}"),
        }

        let mut conn = UnixStream::connect(server.socket_path()).unwrap();
        writeln!(conn, r#"{{"verb":"text","text":"echo hi"}}"#).unwrap();
        server.drain(&ctx, &mut shared);
        match shared.injected.pop_front().unwrap() {
            Injected::Event(egui::Event::Text(text)) => assert_eq!(text, "echo hi"),
            other => panic!("expected the text event, got {other:?}"),
        }
        drop(conn);

        // `dump` answers in-band and queues no injected input.
        let mut conn = UnixStream::connect(server.socket_path()).unwrap();
        writeln!(conn, r#"{{"verb":"dump"}}"#).unwrap();
        server.drain(&ctx, &mut shared);
        let mut response = String::new();
        conn.read_to_string(&mut response).unwrap();
        assert!(response.contains(r#""ok":true"#), "{response}");
        assert!(response.contains(r#""v":1"#), "{response}");
        assert!(shared.injected.is_empty(), "dump injects nothing");

        // `quit` queues the close path's marker.
        let mut conn = UnixStream::connect(server.socket_path()).unwrap();
        writeln!(conn, r#"{{"verb":"quit"}}"#).unwrap();
        server.drain(&ctx, &mut shared);
        assert!(matches!(
            shared.injected.pop_front().unwrap(),
            Injected::Quit
        ));
        drop(conn);

        // A malformed line still earns a well-formed error.
        let mut conn = UnixStream::connect(server.socket_path()).unwrap();
        writeln!(conn, "not json").unwrap();
        server.drain(&ctx, &mut shared);
        let mut response = String::new();
        conn.read_to_string(&mut response).unwrap();
        assert!(response.contains(r#""ok":false"#), "{response}");
    }

    /// `"twin":true` queues press → `Text` → release — the order
    /// egui-winit emits the pair from one `KeyboardInput` — with the
    /// chord's own character as the payload; an explicit `twin` is
    /// verbatim, and a non-character chord refuses to derive.
    #[test]
    fn the_twin_emits_the_platform_leak_shape() {
        let ctx = egui::Context::default();
        let mut shared = shared();

        // The derived twin: `Alt+A` → `Text("a")` between press and
        // release.
        let response = dispatch(
            &ctx,
            &mut shared,
            &Request::Key {
                chord: "Alt+A".to_owned(),
                twin: Some(TwinSpec::Flag(true)),
            },
        );
        assert!(response.ok);
        assert_eq!(shared.injected.len(), 3);
        match shared.injected.pop_front().unwrap() {
            Injected::Event(egui::Event::Key {
                pressed, modifiers, ..
            }) => assert!(pressed && modifiers.alt),
            other => panic!("expected the key press, got {other:?}"),
        }
        match shared.injected.pop_front().unwrap() {
            Injected::Event(egui::Event::Text(text)) => assert_eq!(text, "a"),
            other => panic!("expected the text twin, got {other:?}"),
        }
        match shared.injected.pop_front().unwrap() {
            Injected::Event(egui::Event::Key { pressed, .. }) => assert!(!pressed),
            other => panic!("expected the key release, got {other:?}"),
        }

        // An explicit payload rides verbatim — `Alt+Shift+A` style
        // chords that derive nothing still take their literal twin.
        let response = dispatch(
            &ctx,
            &mut shared,
            &Request::Key {
                chord: "Alt+Enter".to_owned(),
                twin: Some(TwinSpec::Text("x".to_owned())),
            },
        );
        assert!(response.ok);
        assert_eq!(shared.injected.len(), 3);
        let events: Vec<egui::Event> = shared
            .injected
            .drain(..)
            .filter_map(|item| match item {
                Injected::Event(event) => Some(event),
                _ => None,
            })
            .collect();
        assert!(matches!(events[1], egui::Event::Text(ref t) if t == "x"));

        // `twin:true` on a non-character chord is a dispatch error,
        // never a silent drop.
        let response = dispatch(
            &ctx,
            &mut shared,
            &Request::Key {
                chord: "Alt+F5".to_owned(),
                twin: Some(TwinSpec::Flag(true)),
            },
        );
        assert!(!response.ok, "{response:?}");
        assert!(shared.injected.is_empty());
    }

    /// `paste`, `scroll` and `resize` queue their injected shapes;
    /// the malformed halves (a lone `x`, a non-positive size) answer
    /// `ok:false` and inject nothing.
    #[test]
    fn paste_scroll_resize_dispatch_their_injected_shapes() {
        let ctx = egui::Context::default();
        let mut shared = shared();

        let response = dispatch(
            &ctx,
            &mut shared,
            &Request::Paste {
                text: "echo hi".to_owned(),
            },
        );
        assert!(response.ok);
        match shared.injected.pop_front().unwrap() {
            Injected::Event(egui::Event::Paste(text)) => assert_eq!(text, "echo hi"),
            other => panic!("expected the paste event, got {other:?}"),
        }

        let response = dispatch(
            &ctx,
            &mut shared,
            &Request::Scroll {
                unit: ScrollUnit::Pixel,
                dx: 0.0,
                dy: 3.0,
                x: None,
                y: None,
                over: None,
            },
        );
        assert!(response.ok);
        match shared.injected.pop_front().unwrap() {
            Injected::Wheel {
                unit,
                delta,
                target,
            } => {
                assert_eq!(unit, egui::MouseWheelUnit::Point);
                assert_eq!(delta, egui::Vec2::new(0.0, 3.0));
                assert!(matches!(target, ScrollTarget::Grid));
            }
            other => panic!("expected the wheel item, got {other:?}"),
        }

        // `x` without `y` is a decode-clean dispatch error.
        let response = dispatch(
            &ctx,
            &mut shared,
            &Request::Scroll {
                unit: ScrollUnit::Line,
                dx: 0.0,
                dy: 1.0,
                x: Some(4.0),
                y: None,
                over: None,
            },
        );
        assert!(!response.ok);
        assert!(shared.injected.is_empty());

        let response = dispatch(
            &ctx,
            &mut shared,
            &Request::Resize {
                width: 1100.0,
                height: 700.0,
            },
        );
        assert!(response.ok);
        match shared.injected.pop_front().unwrap() {
            Injected::Resize(size) => assert_eq!(size, egui::Vec2::new(1100.0, 700.0)),
            other => panic!("expected the resize item, got {other:?}"),
        }

        for (width, height) in [(0.0, 100.0), (100.0, -1.0), (f32::NAN, 100.0)] {
            let response = dispatch(&ctx, &mut shared, &Request::Resize { width, height });
            assert!(!response.ok, "{width}x{height}");
            assert!(shared.injected.is_empty());
        }
    }

    /// `pointer`'s `modifiers` strings → the held set: modifier-only
    /// names joined by `+`, `""`/`none` lifts, anything else errors.
    #[test]
    fn modifier_sets_parse_or_fail_loudly() {
        let mods = parse_modifiers("Ctrl").unwrap();
        assert!(mods.ctrl && !mods.alt && !mods.shift);
        assert!(mods.command, "ctrl derives command off macOS");
        let mods = parse_modifiers("ctrl+shift").unwrap();
        assert!(mods.ctrl && mods.shift && !mods.alt);
        let mods = parse_modifiers("Alt+Super").unwrap();
        assert!(mods.alt && mods.mac_cmd);
        for empty in ["", "none"] {
            assert_eq!(parse_modifiers(empty).unwrap(), egui::Modifiers::NONE);
        }
        // Empty join parts skip — `ctrl++alt` is `ctrl+alt`.
        let mods = parse_modifiers("ctrl++alt").unwrap();
        assert!(mods.ctrl && mods.alt);
        for bad in ["Ctrl+E", "bogus", "ctrl "] {
            assert!(parse_modifiers(bad).is_err(), "{bad:?}");
        }
    }

    /// `pointer`/`click` positions: `target` resolves to its rect's
    /// center plus `dx`/`dy`, `x`/`y` is the raw pair — and an
    /// unknown name fails naming the registry's real vocabulary.
    #[test]
    fn pointer_positions_resolve_names_and_raw_pairs() {
        let mut shared = shared();
        shared.hits.register(
            "grid",
            egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(100.0, 50.0)),
        );
        // The target's rect IS the answer — its center, plus offset.
        let pos = pointer_pos(&shared, Some("grid"), None, None, 0.0, 0.0).unwrap();
        assert_eq!(pos, egui::pos2(60.0, 45.0));
        let pos = pointer_pos(&shared, Some("grid"), None, None, 5.0, -5.0).unwrap();
        assert_eq!(pos, egui::pos2(65.0, 40.0));
        // The raw escape hatch.
        let pos = pointer_pos(&shared, None, Some(1.0), Some(2.0), 1.0, 1.0).unwrap();
        assert_eq!(pos, egui::pos2(2.0, 3.0));
        // An unknown name names the miss AND the registry.
        let error = pointer_pos(&shared, Some("bogus"), None, None, 0.0, 0.0).unwrap_err();
        assert!(error.contains("bogus"), "{error}");
        assert!(error.contains("grid"), "{error}");
        // The shape mismatches: neither, half a pair, or both forms.
        for (target, x, y) in [
            (None, None, None),
            (None, Some(1.0), None),
            (None, None, Some(2.0)),
            (Some("grid"), Some(1.0), Some(2.0)),
        ] {
            assert!(pointer_pos(&shared, target, x, y, 0.0, 0.0).is_err());
        }
    }

    /// `pointer_batch` builds the modifier-first batch per piece —
    /// real input's order — and `pointer`/`click` deliveries ride the
    /// plugin's `input_hook` into `RawInput` one batch per frame,
    /// ahead of the frame's native events.
    #[test]
    fn pointer_batches_splice_into_raw_input() {
        // A press under a named set: `ModifiersChanged` leads, the
        // button event carries the set.
        let batch = pointer_batch(
            PointerEventKind::Press,
            Some(egui::pos2(3.0, 4.0)),
            PointerButtonKind::Primary,
            Some(egui::Modifiers::CTRL),
            egui::Modifiers::NONE,
        );
        assert!(matches!(
            batch[0],
            egui::Event::ModifiersChanged(m) if m.ctrl
        ));
        assert!(matches!(
            batch[1],
            egui::Event::PointerButton {
                pos,
                pressed: true,
                modifiers,
                ..
            } if pos == egui::pos2(3.0, 4.0) && modifiers.ctrl
        ));
        // No set named → the event carries the stream's held state.
        let batch = pointer_batch(
            PointerEventKind::Release,
            Some(egui::pos2(3.0, 4.0)),
            PointerButtonKind::Primary,
            None,
            egui::Modifiers::CTRL,
        );
        assert!(matches!(
            batch[0],
            egui::Event::PointerButton {
                pressed: false,
                modifiers,
                ..
            } if modifiers.ctrl
        ));
        // The bare `modifiers` piece with no set lifts all.
        let batch = pointer_batch(
            PointerEventKind::Modifiers,
            None,
            PointerButtonKind::Primary,
            None,
            egui::Modifiers::CTRL,
        );
        assert!(matches!(
            batch[0],
            egui::Event::ModifiersChanged(m) if m == egui::Modifiers::NONE
        ));

        // The plugin: one batch per `input_hook` call, spliced ahead
        // of native events; `held` tracks the queued stream's sets.
        let mut plugin = DriverInputPlugin::default();
        plugin.push(vec![egui::Event::ModifiersChanged(egui::Modifiers::CTRL)]);
        plugin.push(vec![egui::Event::PointerMoved(egui::pos2(1.0, 1.0))]);
        assert_eq!(plugin.held(), egui::Modifiers::CTRL);
        let ctx = egui::Context::default();
        let mut input = egui::RawInput {
            events: vec![egui::Event::Text("native".to_owned())],
            ..Default::default()
        };
        plugin.input_hook(&ctx, &mut input);
        assert!(matches!(
            input.events[0],
            egui::Event::ModifiersChanged(_)
        ));
        assert!(matches!(input.events[1], egui::Event::Text(_)));
        let mut next = egui::RawInput::default();
        plugin.input_hook(&ctx, &mut next);
        assert_eq!(next.events.len(), 1);
        assert!(matches!(next.events[0], egui::Event::PointerMoved(_)));
    }

    /// `click`/`pointer` queue their batches on the plugin and earn a
    /// frame through the `Repaint` marker; an `agent` stage needs the
    /// panel open and lands an `Injected::AgentEvent` for
    /// `dispatch_events`' `apply` fold.
    #[test]
    fn pointer_click_agent_dispatch_their_shapes() {
        let ctx = egui::Context::default();
        let mut shared = shared();
        shared.hits.register(
            "grid",
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(80.0, 40.0)),
        );

        // No plugin registered → a clean `ok:false`, nothing queued.
        let response = dispatch(
            &ctx,
            &mut shared,
            &Request::Click {
                target: Some("grid".to_owned()),
                x: None,
                y: None,
                dx: 0.0,
                dy: 0.0,
                button: None,
                modifiers: None,
            },
        );
        assert!(!response.ok);
        assert!(shared.injected.is_empty());

        ctx.add_plugin(DriverInputPlugin::default());
        // `click` on a name → the move+press+release batch queued on
        // the plugin, the frame earned through `Injected::Repaint`.
        let response = dispatch(
            &ctx,
            &mut shared,
            &Request::Click {
                target: Some("grid".to_owned()),
                x: None,
                y: None,
                dx: 0.0,
                dy: 0.0,
                button: None,
                modifiers: None,
            },
        );
        assert!(response.ok);
        assert!(matches!(
            shared.injected.pop_front(),
            Some(Injected::Repaint)
        ));
        ctx.with_plugin(|plugin: &mut DriverInputPlugin| {
            assert_eq!(plugin.queue.len(), 1);
            let batch = &plugin.queue[0];
            assert!(matches!(batch[0], egui::Event::PointerMoved(_)));
            assert!(matches!(
                batch[1],
                egui::Event::PointerButton {
                    pos,
                    pressed: true,
                    ..
                } if pos == egui::pos2(40.0, 20.0)
            ));
            assert!(matches!(
                batch[2],
                egui::Event::PointerButton { pressed: false, .. }
            ));
        });

        // An unknown name fails loudly and queues nothing.
        let response = dispatch(
            &ctx,
            &mut shared,
            &Request::Click {
                target: Some("bogus".to_owned()),
                x: None,
                y: None,
                dx: 0.0,
                dy: 0.0,
                button: None,
                modifiers: None,
            },
        );
        assert!(!response.ok);
        assert!(shared.injected.is_empty());
        ctx.with_plugin(|plugin: &mut DriverInputPlugin| {
            assert_eq!(plugin.queue.len(), 1, "nothing new queued");
        });

        // A `click` naming a modifier set appends the restore batch —
        // the held state survives the gesture.
        let response = dispatch(
            &ctx,
            &mut shared,
            &Request::Click {
                target: Some("grid".to_owned()),
                x: None,
                y: None,
                dx: 0.0,
                dy: 0.0,
                button: None,
                modifiers: Some("Ctrl".to_owned()),
            },
        );
        assert!(response.ok);
        ctx.with_plugin(|plugin: &mut DriverInputPlugin| {
            assert_eq!(plugin.queue.len(), 3, "click batch + restore");
            assert!(matches!(
                plugin.queue[1][0],
                egui::Event::ModifiersChanged(m) if m.ctrl
            ));
            assert!(matches!(
                plugin.queue[2][0],
                egui::Event::ModifiersChanged(m) if m == egui::Modifiers::NONE
            ));
        });
        shared.injected.clear();

        // `agent` without an open panel refuses; with one it stages
        // the approval through the same `apply` fold real events take.
        let response = dispatch(
            &ctx,
            &mut shared,
            &Request::Agent {
                event: AgentEventKind::Approve,
                name: "run".to_owned(),
                summary: "rm -rf /".to_owned(),
            },
        );
        assert!(!response.ok, "no panel open yet");
        shared
            .tabs
            .active_mut()
            .expect("a tab is active")
            .toggle_agent_panel();
        let response = dispatch(
            &ctx,
            &mut shared,
            &Request::Agent {
                event: AgentEventKind::Approve,
                name: "run".to_owned(),
                summary: "rm -rf /".to_owned(),
            },
        );
        assert!(response.ok);
        match shared.injected.pop_front().unwrap() {
            Injected::AgentEvent(ait_agent::AgentEvent::ApprovalRequest {
                call, summary, ..
            }) => {
                assert_eq!(call.name, "run");
                assert_eq!(summary, "rm -rf /");
            }
            other => panic!("expected the staged approval, got {other:?}"),
        }
    }

    /// A bad `key` chord answers `ok:false` and injects nothing; the
    /// dump reflects the pane host's selection and the active tab's
    /// grid state.
    #[test]
    fn dispatch_errors_and_dumps_real_state() {
        let ctx = egui::Context::default();
        let mut shared = shared();
        let response = dispatch(
            &ctx,
            &mut shared,
            &Request::Key {
                chord: "Bogus".to_owned(),
                twin: None,
            },
        );
        assert!(!response.ok);
        assert!(shared.injected.is_empty());

        shared.panes.select(PaneId::Explorer);
        let dump = Dump::capture(&ctx, &shared);
        assert_eq!(dump.v, DUMP_VERSION);
        assert_eq!(dump.pane.shown, Some("explorer"));
        assert_eq!(dump.pane.selected, Some("explorer"));
        let grid = dump.grid.expect("a tab is active");
        assert_eq!(grid.columns, 80);
        assert_eq!(grid.lines, 24);
        assert_eq!(grid.rows.len(), 24);
    }
}
