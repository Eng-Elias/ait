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
use std::path::{Path, PathBuf};

use ait_term::{
    Cursor, ExitStatus, GridView, LiveEvent, LiveSession, LiveSessionOptions, PromptMarkKind,
    SessionEvent, Size, SpawnError,
};

use crate::scroll::ScrollState;
use crate::selection::{GridPoint, Selection, SelectionRange};
use crate::suggest::InputShadow;

/// Title shown when the resolved program has no usable basename — the
/// resolved program is `$SHELL` or `ait-term`'s `/bin/sh` fallback, so
/// this only guards pathological paths (e.g. `/`).
const FALLBACK_TITLE: &str = "sh";

/// How many prompt marks a tab retains — four arrive per shell
/// command (A/B/C/D), so this covers ~1000 commands of history; the
/// oldest drop off the front, same rule as scrollback.
const PROMPT_MARK_CAP: usize = 4096;

/// One OSC 133 mark recorded against the line it landed on (Spec R01
/// user story 14). `line` indexes the "scrollback then viewport" row
/// sequence — the same coordinates [`Tab::grid`]'s `scrollback_rows` +
/// `visible_rows` iterate — and stays valid as scrollback grows because
/// new history only ever appends below it. Round 5's block assembly
/// reads these instead of re-parsing output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptMark {
    /// Which boundary the mark delimits.
    pub kind: PromptMarkKind,
    /// The mark's payload, if present — e.g. the exit code on
    /// `OSC 133;D;0`.
    pub payload: Option<String>,
    /// The mark's line in scrollback-then-viewport coordinates.
    pub line: usize,
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
}

impl Tab {
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
        let grown = self.scroll.on_output(self.session.grid().scrollback());
        self.selection.on_output(grown);
    }
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
    /// The spawn-options template [`new_tab`](Self::new_tab) clones —
    /// where `[terminal]` `shell`/`scrollback_lines` land (Spec R03).
    /// Applying a template reaches only sessions spawned from then on;
    /// live tabs keep their spawn-time values (story 14).
    session_options: LiveSessionOptions,
    /// The id the next spawned [`Tab`] gets — a plain counter rather
    /// than an index or pid so a tab's identity is immune to list
    /// shifts and pid-less platforms (Spec R04 user story 14).
    next_id: u64,
}

impl TabManager {
    /// Spawn the manager with its first tab in the directory `ait` was
    /// launched from: `options` — the app's `[terminal]`-derived
    /// template (Spec R03), `LiveSessionOptions::default()` for
    /// `$SHELL`-or-`/bin/sh` — shapes the first tab AND every
    /// `new_tab` after it.
    pub fn spawn_with(size: Size, options: LiveSessionOptions) -> Result<Self, SpawnError> {
        let launch_cwd = std::env::current_dir().unwrap_or_default();
        Self::spawn_at(size, launch_cwd, options)
    }

    /// `spawn_with` with an explicit launch cwd — the test seam.
    /// `options` spawns the first tab and becomes the template for
    /// later ones.
    fn spawn_at(
        size: Size,
        launch_cwd: PathBuf,
        options: LiveSessionOptions,
    ) -> Result<Self, SpawnError> {
        let mut manager = Self {
            tabs: Vec::new(),
            active: 0,
            launch_cwd,
            size,
            session_options: options.clone(),
            next_id: 0,
        };
        manager.spawn_tab(options)?;
        Ok(manager)
    }

    /// Replace the spawn-options template: sessions spawned from now on
    /// get `options` — the `[terminal]` half of a settings reload —
    /// while live tabs keep their spawn-time program and scrollback
    /// (Spec R03 user story 14).
    pub fn set_session_options(&mut self, options: LiveSessionOptions) {
        self.session_options = options;
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
        options.cwd = Some(cwd.clone());
        let title = title_for(&options);
        let session = LiveSession::spawn(options)?;
        let id = self.next_id;
        self.next_id += 1;
        self.tabs.push(Tab {
            id,
            session,
            scroll: ScrollState::new(),
            selection: Selection::default(),
            shadow: InputShadow::new(),
            cwd,
            cwd_reported: false,
            exit: None,
            title,
            prompt_marks: VecDeque::new(),
        });
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
        self.spawn_tab(self.session_options.clone())
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
    /// visible one (Spec R02 user story 17), and remember the size for
    /// tabs spawned later. Reflow invalidates the grid coordinates a
    /// selection range points at, so every tab's highlight is cleared.
    /// PTY-write failures on a just-dead child are tolerated — the
    /// session's `Exited` event carries that state.
    pub fn resize_all(&mut self, size: Size) {
        self.size = size;
        for tab in &mut self.tabs {
            tab.clear_selection();
            let _ = tab.session.resize(size);
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
                match &event {
                    LiveEvent::Osc(SessionEvent::CwdChanged(cwd)) => {
                        tab.cwd = cwd.clone();
                        tab.cwd_reported = true;
                    }
                    LiveEvent::Osc(SessionEvent::PromptMark { kind, payload }) => {
                        if tab.prompt_marks.len() == PROMPT_MARK_CAP {
                            tab.prompt_marks.pop_front();
                        }
                        tab.prompt_marks.push_back(PromptMark {
                            kind: *kind,
                            payload: payload.clone(),
                            // The mark delimits the line the emulator's
                            // cursor sits on when the OSC lands, indexed
                            // in scrollback-then-viewport coordinates.
                            line: tab.session.grid().scrollback() + tab.session.cursor().line,
                        });
                    }
                    LiveEvent::Exited(status) => tab.exit = Some(status.clone()),
                    _ => {}
                }
                events.push(TabEvent { tab: index, event });
            }
            tab.on_output();
        }
        events
    }
}

/// The tab-bar title for a session spawned from `options`: the resolved
/// program's basename, with `sh` as the fallback when the path has no
/// usable name. The program itself comes from
/// [`LiveSessionOptions::resolved_program`] — the same resolution
/// [`LiveSession::spawn`] applies — so the title can never drift from
/// what actually spawned.
fn title_for(options: &LiveSessionOptions) -> String {
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
    use std::time::{Duration, Instant};

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
        let mut manager =
            TabManager::spawn_at(Size::new(40, 10), PathBuf::from(launch_cwd), sh()).unwrap();
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
            sh_script("exit 3"),
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
        let grid = manager.tabs()[1].grid();
        let total = grid.scrollback() + grid.lines();
        for mark in marks {
            assert!(mark.line < total, "mark line out of grid: {mark:?}");
        }
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

    /// Spec R03 stories 12–14: the session-options template shapes NEW
    /// tabs — the spawned program and the scrollback capacity — while a
    /// tab spawned before the template changed keeps its spawn-time
    /// values. `new_tab` used to hardcode `LiveSessionOptions::default()`;
    /// the template is what `[terminal]` writes land on at reload.
    #[test]
    fn session_options_template_shapes_new_tabs_only() {
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
        manager.set_session_options(LiveSessionOptions {
            program: Some(OsString::from("/bin/cat")),
            session: SessionOptions {
                scrollback: 3,
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
}
