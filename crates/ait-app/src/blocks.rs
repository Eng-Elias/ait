//! Block assembly (REQUIREMENTS.md §5.5, Spec R05 / issue #73).
//!
//! A **block** is one command, its output and its exit code (GLOSSARY)
//! — delimited by the shell's OSC 133 marks: `B` starts the command
//! echo, `C` starts the output, `D` ends the command carrying the exit
//! code; a block spans `B→D`. [`BlockAssembler`] is the pure seam: it
//! consumes recorded [`PromptMark`]s plus decoded `ait-shell` events
//! (the `preexec` hook's command text) and maintains a bounded history
//! — `Absent`-state tabs simply never feed it marks.
//!
//! Block lines live in **absolute space**: `mark.line + dropped`,
//! where `dropped` counts scrollback lines removed from the top since
//! the session started. Absolute lines stay fixed while recorded
//! indices slide on eviction; [`Tab::blocks`](crate::tabs::Tab::blocks)
//! converts them back into live coordinates on access, clamping
//! best-effort across truncation and reflow and dropping ranges that
//! left the grid entirely.
//!
//! Missing, duplicated, orphaned or out-of-order marks degrade, never
//! panic: a command whose `D` never arrives closes best-effort when the
//! next prompt/command boundary shows the shell moved on; `C`/`D` with
//! no open command are ignored.

use std::collections::VecDeque;
use std::ops::RangeInclusive;
use std::time::{Duration, Instant, SystemTime};

use ait_shell::{HookName, ShellEvent};
use ait_term::{GridView, PromptMarkKind, Row};

use crate::tabs::{PromptMark, PROMPT_MARK_CAP};

/// One block per A/B/C/D mark quadruple — a quarter of the mark cap
/// keeps a thousand commands of blocks, same ~1000-command horizon
/// `PROMPT_MARK_CAP` sets for the raw marks.
pub const BLOCK_CAP: usize = PROMPT_MARK_CAP / 4;

/// The pure block assembler (Spec R05): [`observe_mark`](Self::observe_mark)
/// for the OSC 133 stream, [`observe_shell`](Self::observe_shell) for
/// decoded `7700` hooks. Both degrade silently on anything unexpected —
/// the assembler never panics on malformed or unordered input.
#[derive(Debug, Default)]
pub struct BlockAssembler {
    /// Closed blocks, oldest first, bounded at [`BLOCK_CAP`].
    blocks: VecDeque<BlockEntry>,
    /// A snapshot of every block pushed since the last
    /// [`take_sealed`](Self::take_sealed) — the persistence seam (Spec
    /// R08). Clones, so a `BLOCK_CAP` eviction or a
    /// [`discard_ranges`](Self::discard_ranges) never loses metadata
    /// that was already sealed; the caller drains it every tick.
    sealed: VecDeque<BlockEntry>,
    /// The command still open — between `B` and `D`, or an abandoned
    /// candidate awaiting a boundary that proves the shell moved on.
    open: Option<BlockEntry>,
    /// A `preexec` command that arrived before its `B` mark — held for
    /// the next block, cleared when a new prompt starts instead.
    pending_command: Option<String>,
    /// The remote host stamped on blocks sealed now — the tab's live
    /// remote context (`blocks.host`, Spec R09-T7), `None` for local
    /// blocks. The drain refreshes it per event so a seal mid-batch
    /// gets the context live when the mark landed — stamping at take
    /// time would mislabel blocks that ran locally but drained after
    /// `ssh_begin`.
    remote_host: Option<String>,
}

/// FNV-1a offset basis — shared by [`row_hash`] and the multi-row
/// fingerprints [`hash_combine`] builds.
const FNV_INIT: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a step over one hashed value — folds a row's fingerprint into
/// a range fingerprint preserving order.
pub fn hash_combine(hash: u64, value: u64) -> u64 {
    (hash ^ value).wrapping_mul(0x100_0000_01b3)
}

/// A scrollback row's fingerprint — FNV-1a over cell text plus the wrap
/// flag, cheap enough to refresh per drained event. Used for the
/// rotation anchor window and for block seal verification at resolve.
pub fn row_hash(row: &Row<'_>) -> u64 {
    let mut hash = FNV_INIT;
    for cell in row.cells() {
        hash = hash_combine(hash, cell.character as u64);
    }
    hash_combine(hash, row.wrapped() as u64)
}

/// Fingerprint the grid rows covering an absolute `(start, end)` span:
/// `dropped`/`scrollback` translate to live grid indices; `None` when
/// any row of the span is gone (nothing stable to seal against). The
/// caller feeds the pair to [`BlockAssembler::observe_mark_row`].
pub fn seal_fingerprint(
    grid: &GridView<'_>,
    dropped: usize,
    scrollback: usize,
    range: (usize, usize),
) -> Option<(u64, usize)> {
    let (start, end) = range;
    let first = start as i64 - dropped as i64 - scrollback as i64;
    let last = end as i64 - dropped as i64 - scrollback as i64;
    (first..=last)
        .try_fold(FNV_INIT, |hash, i| {
            i32::try_from(i)
                .ok()
                .and_then(|line| grid.line(line))
                .map(|row| hash_combine(hash, row_hash(&row)))
        })
        .map(|hash| (hash, end - start + 1))
}

/// Row text for debug dumps — trimmed trailing blanks.
#[doc(hidden)] // test-side inspection — not production surface
pub fn row_text(row: &Row<'_>) -> String {
    row.text().trim_end().to_string()
}

/// A recorded mark boundary in **absolute** line space: the record-time
/// scrollback-then-viewport index plus the lines dropped since —
/// absolute lines stay fixed while recorded indices slide on eviction.
#[derive(Debug, Clone, Copy)]
struct Mark {
    line: usize,
    column: usize,
    at: Instant,
    /// The wall clock beside `at` — persistence needs `SystemTime`
    /// bounds (`Instant` never converts), so the drain captures both
    /// at record time (Spec R08).
    wall: SystemTime,
}

/// One assembled block in absolute line space (Spec R05): the echo
/// starts at `start` (`B`), `end` is the last line (`D`, or best-effort
/// when `D` never arrived). `output_start` (`C`) splits echo from
/// output; `command` holds the `preexec` hook's report — `None`
/// resolves to the `B→C` echo slice at [`resolve`](Self::resolve) time.
#[derive(Debug, Clone)]
pub struct BlockEntry {
    /// The `B` mark — the command echo's first line and column.
    start: Mark,
    /// The `C` mark — the output's first line; `None` when it never came.
    output_start: Option<Mark>,
    /// The block's last absolute line.
    end: usize,
    /// The column bound on `end`, from the `D` mark's cursor position:
    /// `D` lands on the next prompt's row, so content at/after its
    /// column is foreign (prompt + the next command's echo — Spec R05
    /// us-11's "through the last output line"). `None` = whole row
    /// (abandoned blocks, whose end is a line-only estimate).
    end_column: Option<usize>,
    /// The command text the `preexec` hook reported, if any.
    command: Option<String>,
    /// The `OSC 133;D` exit code, if the mark carried one.
    exit: Option<u32>,
    /// `D.at − C.at` (`B.at` when `C` is missing); `None` without `D`.
    duration: Option<Duration>,
    /// `D` arrived — the block is finished. `false` covers both
    /// still-running commands and ones abandoned without a `D`.
    completed: bool,
    /// `(first absolute line, row count, fingerprint)` over the
    /// block's echo span, taken at the mark that closed it. Checked at
    /// [`resolve`](Self::resolve): absolute-line bookkeeping can lose
    /// track when scrollback rotation goes unmeasured, and content —
    /// not coordinates — is the last word on whether the range still
    /// covers its own block. `None` (no grid at seal) skips the check.
    seal: Option<(usize, usize, u64)>,
    /// The `D` mark's wall-clock time — the block's persisted
    /// `ended_at` (Spec R08). `None` when no `D` ever arrived: an
    /// abandoned block reports only what the hooks did.
    ended_at: Option<SystemTime>,
    /// The remote host live when the block sealed — `blocks.host`
    /// (Spec R09-T7), `None` for a block that ran locally. Stamped at
    /// seal, never at drain time, so the host is the session the
    /// command actually ran under.
    host: Option<String>,
}

/// The `(exit code, duration)` pair the status line renders for the
/// newest finished block (Spec R05 us-15) — kept narrow so that read
/// side needs no grid-resolved [`Block`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockStatus {
    /// The `OSC 133;D` exit code.
    pub exit: u32,
    /// Execution time (`D.at − C.at`, `B.at` fallback).
    pub duration: Duration,
}

/// A block resolved against the live grid — what
/// [`Tab::blocks`](crate::tabs::Tab::blocks) hands consumers (Spec R05).
/// Line coordinates are **live** scrollback-then-viewport indices,
/// clamped to the current grid.
#[derive(Debug)]
pub struct Block {
    /// The block's line range (echo through last output row), clamped
    /// into `0..extent`.
    range: RangeInclusive<usize>,
    /// The output's first line (the `C` mark's, clamped into `range`);
    /// `None` when `C` never arrived — nothing is known to be output.
    output_start: Option<usize>,
    /// The inclusive column bound on `range.end()` — set when the `D`
    /// mark landed mid-row, so the row's tail (prompt + next echo)
    /// stays out of a copy; `None` selects the whole row.
    end_column: Option<usize>,
    /// The command text: the `preexec` hook's report when the shell
    /// sent one, else the `B→C` echo slice — whose first row is cut at
    /// the `B` mark's cursor column so the prompt itself stays out.
    command: String,
    /// The `OSC 133;D` exit code; `None` when `D` had none or never came.
    exit: Option<u32>,
    /// Execution time — `D.at − C.at` (`B.at` without `C`); `None` for
    /// a block `D` never closed.
    duration: Option<Duration>,
    /// `D` arrived — the command finished. `false` on the trailing
    /// still-running block and on blocks abandoned without a `D`.
    completed: bool,
}

impl BlockAssembler {
    /// An empty assembler.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one recorded prompt mark. `dropped` is the caller's count
    /// of scrollback lines removed from the top since the session
    /// started — it converts the mark's record-time `line` into this
    /// assembler's absolute line space.
    ///
    /// `B` opens a block, `C` marks its output's start, `D` closes it
    /// with an exit code. `A` (or a fresh `B`) arriving while one is
    /// open means its `D` never came — the open block closes
    /// best-effort, ending on the row above the new boundary. `C`/`D`
    /// with no open command are orphans and ignored.
    #[doc(hidden)] // the drain feeds `observe_mark_row`; tests use this
    pub fn observe_mark(&mut self, mark: &PromptMark, dropped: usize) {
        self.observe_mark_row(mark, dropped, None);
    }

    /// [`observe_mark`](Self::observe_mark) plus a fingerprint of the
    /// open block's echo span, computed by the caller (it owns the
    /// grid) over [`open_seal_range`](Self::open_seal_range) while this
    /// mark surfaces: every row in the span is final once the command
    /// runs, and a mark that CLOSES the block stores it as the seal
    /// [`resolve`](BlockEntry::resolve) checks — so a range that slid
    /// onto wrong-but-valid rows (a scrollback rotation the caller
    /// could not measure) resolves to nothing instead of selecting
    /// stale content (Spec R05 us-20). The triple is
    /// `(first absolute line, row count, fingerprint)`.
    pub fn observe_mark_row(
        &mut self,
        mark: &PromptMark,
        dropped: usize,
        seal: Option<(usize, usize, u64)>,
    ) {
        let m = BlockEntry::mark(mark, dropped);
        match mark.kind {
            PromptMarkKind::PromptStart => {
                // A new prompt ends any stale `preexec` stash, and a
                // `D`-less open block closes above it.
                self.pending_command = None;
                if let Some(mut open) = self.open.take() {
                    open.abandon(m.line);
                    open.seal(seal);
                    self.push_block(open);
                }
            }
            PromptMarkKind::CommandStart => {
                if let Some(mut open) = self.open.take() {
                    open.abandon(m.line);
                    open.seal(seal);
                    self.push_block(open);
                }
                self.open = Some(BlockEntry::open(m, self.pending_command.take()));
            }
            PromptMarkKind::OutputStart => {
                if let Some(open) = &mut self.open {
                    open.output_start = Some(m);
                    if m.line > open.end {
                        open.end = m.line;
                        open.end_column = None;
                    }
                }
            }
            PromptMarkKind::CommandEnd => {
                if let Some(mut open) = self.open.take() {
                    open.complete(m, mark.payload.as_deref());
                    open.seal(seal);
                    self.push_block(open);
                }
            }
        }
    }

    /// Feed one decoded `ait-shell` event — only `preexec` matters: its
    /// `value.command` patches the waiting open block (the hook lands
    /// between `B` and `C`), or waits for the next `B` when it arrives
    /// first. Everything else is ignored.
    pub fn observe_shell(&mut self, event: &ShellEvent) {
        if let ShellEvent::Hook {
            name: HookName::Preexec,
            value,
        } = event
        {
            if let Some(command) = value.get("command").and_then(|v| v.as_str()) {
                match &mut self.open {
                    Some(open) => open.command = Some(command.to_owned()),
                    None => self.pending_command = Some(command.to_owned()),
                }
            }
        }
    }

    /// The open block's echo span `(start, end)` in this assembler's
    /// absolute space — the rows through the one above `C` (or the
    /// whole block when no `C` arrived). Every row in it is final the
    /// moment the command starts running — echo text cannot grow — so
    /// the caller hashes exactly these for
    /// [`observe_mark_row`](Self::observe_mark_row) (see its doc).
    pub fn open_seal_range(&self) -> Option<(usize, usize)> {
        self.open.as_ref().map(|open| {
            let end = open
                .output_start
                .map(|mark| mark.line.saturating_sub(1))
                .unwrap_or(open.end);
            (open.start.line, end.max(open.start.line))
        })
    }

    /// Closed blocks oldest-first, then the still-open one if any —
    /// the trailing `completed: false` entry is the running command.
    pub fn blocks(&self) -> impl Iterator<Item = &BlockEntry> {
        self.blocks.iter().chain(self.open.iter())
    }

    /// Whether any block — closed or still open — is tracked. The
    /// block chords consult it: a tab with none treats `Alt+B`/`Alt+↑`/
    /// `Alt+↓` as ordinary PTY input again (Spec R05 Out-of-Scope).
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.open.is_none()
    }

    /// Forget every tracked range: the caller detected scrollback
    /// rotation it could no longer measure, so recorded absolute lines
    /// can point at wrong-but-valid rows — the spec's bound is that a
    /// mis-select must be impossible, so every block this stream
    /// assembled drops rather than resolves stale (us-20). Marks
    /// arriving after the loss anchor correctly again.
    pub fn discard_ranges(&mut self) {
        self.blocks.clear();
        self.open = None;
        self.pending_command = None;
    }

    /// The newest block's [`BlockStatus`] — the status line's
    /// `✓/✗ <code> · <dur>` segment (Spec R05 us-15). Only blocks that
    /// reported an `OSC 133;D` exit code count: a still-running or
    /// abandoned command has nothing to report. Cheaper than
    /// [`blocks`](Self::blocks) + [`resolve`](BlockEntry::resolve) — it
    /// reads the raw entries, no grid pass.
    pub fn last_status(&self) -> Option<BlockStatus> {
        self.blocks.iter().rev().find_map(|entry| {
            entry
                .exit()
                .zip(entry.duration())
                .map(|(exit, duration)| BlockStatus { exit, duration })
        })
    }

    /// The blocks sealed since the last call, oldest first — the
    /// drain's persist batch (Spec R08). Each entry is a snapshot of
    /// the block as it closed: the wall-clock bounds, the `preexec`
    /// command (if any) and the grid coordinates `persisted` resolves
    /// the echo-slice fallback through.
    pub fn take_sealed(&mut self) -> Vec<BlockEntry> {
        self.sealed.drain(..).collect()
    }

    /// The remote host newly sealed blocks carry — `Some` while the
    /// tab sits in an `ssh()` session, `None` local (Spec R09-T7).
    pub fn set_remote_host(&mut self, host: Option<String>) {
        self.remote_host = host;
    }

    fn push_block(&mut self, mut entry: BlockEntry) {
        entry.host.clone_from(&self.remote_host);
        self.sealed.push_back(entry.clone());
        if self.blocks.len() == BLOCK_CAP {
            self.blocks.pop_front();
        }
        self.blocks.push_back(entry);
    }
}

impl BlockEntry {
    /// Open a block on the `B` mark: echo starts at its line/column,
    /// `end` floats at that line until later marks push it down.
    fn open(start: Mark, command: Option<String>) -> Self {
        Self {
            start,
            output_start: None,
            end: start.line,
            end_column: None,
            command,
            exit: None,
            duration: None,
            completed: false,
            seal: None,
            ended_at: None,
            host: None,
        }
    }

    /// `D` arrived: the exit code rides its payload, the duration is
    /// execution time `D.at − C.at` — `B.at` when `C` never came, so
    /// typing time never counts as runtime (Spec R05 us-14). Its cursor
    /// column bounds the end row — see [`BlockEntry::end_column`].
    fn complete(&mut self, mark: Mark, payload: Option<&str>) {
        if mark.line >= self.end {
            self.end = mark.line;
            self.end_column = Some(mark.column);
        }
        self.exit = payload.and_then(exit_code);
        let began = self.output_start.map_or(self.start.at, |m| m.at);
        self.duration = Some(mark.at.saturating_duration_since(began));
        self.completed = true;
        self.ended_at = Some(mark.wall);
    }

    /// Store the seal fingerprint — the caller hashed the block's
    /// [`open_seal_range`](BlockAssembler::open_seal_range) rows as
    /// this mark surfaced. Stored on whatever block the mark closes.
    fn seal(&mut self, seal: Option<(usize, usize, u64)>) {
        if seal.is_some() {
            self.seal = seal;
        }
    }

    /// The stream moved on without a `D`: the block ends best-effort on
    /// the row above the boundary that displaced it — a whole-row
    /// estimate, so the column bound is cleared — no exit code, no
    /// duration, `completed` stays `false`.
    fn abandon(&mut self, anchor: usize) {
        let line = anchor.saturating_sub(1);
        if line > self.end {
            self.end = line;
            self.end_column = None;
        }
    }

    /// The block's last content line and the inclusive column bound on
    /// it. `D` lands on the next prompt's row: at column 0 that row is
    /// wholly foreign and the block ends one line up; mid-row means
    /// unterminated output filled `0..column` and the row ends at
    /// `column - 1` (Spec R05 us-11).
    fn effective_end(&self) -> (usize, Option<usize>) {
        match self.end_column {
            Some(0) => (self.end.saturating_sub(1), None),
            Some(c) => (self.end, Some(c - 1)),
            None => (self.end, None),
        }
    }

    /// The `D` mark's exit code, once it closed the block.
    fn exit(&self) -> Option<u32> {
        self.exit
    }

    /// Execution time once `D` closed the block.
    fn duration(&self) -> Option<Duration> {
        self.duration
    }

    /// The `preexec`-reported command, if one arrived.
    #[allow(dead_code)] // test-side inspection today; `resolve` reads the field
    fn command(&self) -> Option<&str> {
        self.command.as_deref()
    }

    /// Whether `D` closed this block.
    #[allow(dead_code)] // test-side inspection today
    fn completed(&self) -> bool {
        self.completed
    }

    /// The block's line range in LIVE scrollback-then-viewport
    /// coordinates: absolute lines minus `dropped` (lines evicted since
    /// the session started), clamped to `0..extent`. The end line is
    /// [`effective_end`](Self::effective_end)'s — a `D` at column 0
    /// already excludes its wholly-foreign prompt row. `None` when
    /// every line fell out of the grid — fully-evicted ranges drop
    /// cleanly — or the range sits past the current bottom.
    pub fn range(&self, dropped: usize, extent: usize) -> Option<RangeInclusive<usize>> {
        let (end_line, _) = self.effective_end();
        if extent == 0 || end_line < dropped {
            return None;
        }
        let bottom = dropped + extent - 1;
        if self.start.line > bottom {
            return None;
        }
        let start = self.start.line.max(dropped) - dropped;
        let end = end_line.min(bottom).max(start + dropped) - dropped;
        Some(start..=end)
    }

    /// Resolve to the consumer-facing [`Block`]: the clamped live range,
    /// the command text (hook report, else the `B→C` echo slice read off
    /// `grid`), exit code and duration. `None` when the block's lines
    /// all left the grid.
    pub fn resolve(&self, grid: &GridView<'_>, dropped: usize, extent: usize) -> Option<Block> {
        // Content verification: the rows fingerprinted at seal time
        // must still sit at their recorded absolute positions. A range
        // that slid onto wrong-but-valid rows — rotation the caller
        // could not measure — fails here instead of mis-selecting
        // (Spec R05 us-20); rows that left the grid fail the same way.
        if let Some((first, rows, expected)) = self.seal {
            let recomputed =
                seal_fingerprint(grid, dropped, grid.scrollback(), (first, first + rows - 1));
            if recomputed.map(|(hash, _)| hash) != Some(expected) {
                return None;
            }
        }
        let range = self.range(dropped, extent)?;
        let output_start = self
            .output_start
            .map(|m| (m.line.max(dropped) - dropped).clamp(*range.start(), *range.end()));
        // The `D` column bound applies only while the live end still
        // sits on the `D` row — a clamped end is a different row.
        let end_column = match self.end_column {
            Some(c) if c > 0 && *range.end() + dropped == self.end => Some(c - 1),
            _ => None,
        };
        let command = self
            .command
            .clone()
            .unwrap_or_else(|| self.echo_text(grid, dropped, extent));
        Some(Block {
            range,
            output_start,
            end_column,
            command,
            exit: self.exit,
            duration: self.duration,
            completed: self.completed,
        })
    }

    /// The `B→C` echo slice: the `B` row from its cursor column through
    /// the row above `C` — the command as it was typed, without the
    /// prompt. Rows join with `'\n'` across hard breaks and flow through
    /// soft wraps, the same rule `selection::extract` applies.
    fn echo_text(&self, grid: &GridView<'_>, dropped: usize, extent: usize) -> String {
        if extent == 0 {
            return String::new();
        }
        let scrollback = grid.scrollback() as i64;
        let row_at = |live: usize| {
            i32::try_from(live as i64 - scrollback)
                .ok()
                .and_then(|line| grid.line(line))
        };
        let first = self.start.line.max(dropped) - dropped;
        // The echo's last row is the one above `C`; without `C` it is
        // just `B`'s row.
        let last = self
            .output_start
            .map_or(self.start.line, |m| m.line.saturating_sub(1))
            .max(self.start.line)
            .min(dropped + extent - 1)
            .max(dropped)
            - dropped;
        let mut text = String::new();
        for live in first..=last {
            let Some(row) = row_at(live) else {
                continue;
            };
            let line = if live == first {
                row_text_from(&row, self.start.column)
            } else {
                row.text().trim_end().to_string()
            };
            text.push_str(&line);
            if live != last && !row.wrapped() {
                text.push('\n');
            }
        }
        text
    }

    /// A mark the drain just recorded — `line` plus `dropped` puts it
    /// in this assembler's absolute space.
    fn mark(mark: &PromptMark, dropped: usize) -> Mark {
        Mark {
            line: mark.line.saturating_add(dropped),
            column: mark.column,
            at: mark.at,
            wall: mark.wall,
        }
    }

    /// The `blocks`-row-shaped metadata of this sealed entry (Spec
    /// R08): the command is the resolved block's — `preexec` text or
    /// the `B→C` echo slice — with the hook report kept when the grid
    /// can no longer resolve (seal mismatch, evicted rows); `exit`,
    /// `duration` and the wall-clock bounds come straight from the
    /// marks. `ended_at` stays `None` when no `D` reported. Metadata
    /// only — no output text, no cells.
    pub fn persisted(
        &self,
        grid: &GridView<'_>,
        dropped: usize,
        extent: usize,
        cwd: String,
    ) -> PersistedBlock {
        PersistedBlock {
            command: self
                .resolve(grid, dropped, extent)
                .map(|block| block.command)
                .or_else(|| self.command.clone()),
            cwd,
            host: self.host.clone(),
            exit: self.exit.map(i64::from),
            duration: self.duration,
            started_at: Some(self.output_start.map_or(self.start.wall, |m| m.wall)),
            ended_at: self.ended_at,
        }
    }
}

/// One sealed block's `blocks` row (Spec R08): what
/// [`Tab::take_persisted_blocks`](crate::tabs::Tab::take_persisted_blocks)
/// hands the store — metadata only, never output text or scrollback
/// cells. Fields the hooks never reported stay `None`.
#[derive(Debug)]
pub struct PersistedBlock {
    /// The command text — `preexec` when the shell reported one, else
    /// the echo slice; `None` when nothing could be resolved (the
    /// `blocks.command` column is nullable: NULL is honest, `""`
    /// claims a command that never was).
    pub command: Option<String>,
    /// The tab's tracked cwd at seal — the `blocks.cwd` column (Spec
    /// R08: the tab's cwd at `preexec`). Filled by the tab, the only
    /// side that knows it.
    pub cwd: String,
    /// The remote host the block ran under — the `blocks.host` column
    /// (Spec R09-T7): the destination token of the tab's live remote
    /// context at seal, `None` for a local block. Filled by the tab —
    /// the only side that tracks remote context.
    pub host: Option<String>,
    /// The `OSC 133;D` exit code.
    pub exit: Option<i64>,
    /// Execution time (`D.at − C.at`, `B.at` fallback).
    pub duration: Option<Duration>,
    /// Wall-clock start — the `C` mark's time (`B`'s without `C`),
    /// matching `duration`'s basis.
    pub started_at: Option<SystemTime>,
    /// Wall-clock end — the `D` mark's time; `None` when `D` never
    /// arrived (the row records what the hooks reported).
    pub ended_at: Option<SystemTime>,
}

// The full read-side surface stays: `range`/`end_column` are consumed
// by the block nav/select/status paths; the rest serve the seams'
// tests and the suggester's context assembly (Spec R05).
impl Block {
    /// The block's clamped live-coordinate line range (`B→D`), inclusive.
    pub fn range(&self) -> RangeInclusive<usize> {
        self.range.clone()
    }

    /// The output's first live line — the `C` mark's row clamped into
    /// `range`, so `output_start..=range.end` is the block's output.
    /// `None` when `C` never arrived.
    pub fn output_start(&self) -> Option<usize> {
        self.output_start
    }

    /// The inclusive column bound on the range's last line — a
    /// mid-row `D` keeps the prompt and next echo out of a copy
    /// (Spec R05 us-18). `None` = whole row.
    pub fn end_column(&self) -> Option<usize> {
        self.end_column
    }

    /// The command text — `preexec`-accurate when integrated, the
    /// echo slice otherwise. Empty when nothing could be read.
    pub fn command(&self) -> &str {
        &self.command
    }

    /// The `OSC 133;D` exit code.
    pub fn exit(&self) -> Option<u32> {
        self.exit
    }

    /// Execution time — typing time is never counted (Spec R05 us-14).
    pub fn duration(&self) -> Option<Duration> {
        self.duration
    }

    /// `D` arrived — the command finished.
    pub fn completed(&self) -> bool {
        self.completed
    }
}

/// `OSC 133;D`'s payload → an exit code: a bare integer, tolerating an
/// `exit=` prefix some emitters use; anything else is no code.
fn exit_code(payload: &str) -> Option<u32> {
    let digits = payload.strip_prefix("exit=").unwrap_or(payload);
    digits.parse().ok()
}

/// A row's text from `column` right — cell-wise (not byte-wise) so wide
/// characters and the `B` mark's column agree.
fn row_text_from(row: &ait_term::Row<'_>, column: usize) -> String {
    let mut text = String::new();
    for cell in row.cells().skip(column) {
        text.push(cell.character);
    }
    text.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ait_term::{Session, SessionOptions, Size};

    const MS: Duration = Duration::from_millis(1);

    /// A synthetic mark in record-time coordinates — the shape
    /// `TabManager::drain_events` hands the assembler.
    fn mark(kind: PromptMarkKind, line: usize, column: usize, at: Instant) -> PromptMark {
        PromptMark {
            kind,
            payload: None,
            line,
            column,
            at,
            wall: SystemTime::UNIX_EPOCH,
        }
    }

    /// A `D` mark carrying an exit-code payload.
    fn end_mark(line: usize, at: Instant, payload: Option<&str>) -> PromptMark {
        PromptMark {
            kind: PromptMarkKind::CommandEnd,
            payload: payload.map(str::to_owned),
            line,
            column: 0,
            at,
            wall: SystemTime::UNIX_EPOCH,
        }
    }

    /// A real `OSC 7700` wire payload through `ait_shell::decode` — the
    /// exact shape `drain_events` hands `observe_shell`.
    fn hook(json: &str) -> ShellEvent {
        let hex: String = json.bytes().map(|b| format!("{b:02x}")).collect();
        ait_shell::decode(b"7700", &[hex.into_bytes()]).expect("a hook decodes")
    }

    fn preexec(command: &str) -> ShellEvent {
        hook(&format!(
            r#"{{"hook":"preexec","value":{{"session_id":0,"command":{command:?}}}}}"#
        ))
    }

    /// Spec R05: a full `A B C D` quadruple plus a `preexec` hook is
    /// one block — range `B→D`, exit from `D`'s payload, duration
    /// `D.at − C.at`, command text from the hook.
    #[test]
    fn a_full_quadruple_becomes_one_block() {
        let t0 = Instant::now();
        let mut asm = BlockAssembler::new();
        asm.observe_mark(&mark(PromptMarkKind::PromptStart, 0, 0, t0), 0);
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 0, 2, t0 + 10 * MS), 0);
        asm.observe_shell(&preexec("ls -la"));
        asm.observe_mark(&mark(PromptMarkKind::OutputStart, 1, 0, t0 + 20 * MS), 0);
        asm.observe_mark(&end_mark(2, t0 + 50 * MS, Some("3")), 0);

        let blocks: Vec<_> = asm.blocks().collect();
        assert_eq!(blocks.len(), 1);
        let block = blocks[0];
        assert!(block.completed());
        assert_eq!(block.exit(), Some(3));
        assert_eq!(block.duration(), Some(30 * MS));
        assert_eq!(block.command(), Some("ls -la"));
        assert_eq!(
            block.range(0, 24),
            Some(0..=1),
            "D at column 0: the prompt row is foreign — end is the last output line"
        );
    }

    /// Spec R05: with no `C` mark the duration falls back to `B.at` the duration falls back to `B.at` —
    /// the spec's `D.at − C.at` (fallback `B.at`).
    #[test]
    fn duration_falls_back_to_b_when_c_is_missing() {
        let t0 = Instant::now();
        let mut asm = BlockAssembler::new();
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 4, 0, t0 + 10 * MS), 0);
        asm.observe_mark(&end_mark(6, t0 + 40 * MS, Some("0")), 0);

        let block = asm.blocks().next().unwrap();
        assert_eq!(block.duration(), Some(30 * MS), "D.at − B.at");
        assert!(block.completed());
    }

    /// Marks-only mode (Spec R05 us-19): `OSC 133` without hook events
    /// still yields blocks — `command` stays `None` for the echo-slice
    /// fallback while exit and duration arrive normally.
    #[test]
    fn marks_without_hooks_yield_unadorned_blocks() {
        let t0 = Instant::now();
        let mut asm = BlockAssembler::new();
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 0, 2, t0), 0);
        asm.observe_mark(&mark(PromptMarkKind::OutputStart, 1, 0, t0 + 5 * MS), 0);
        asm.observe_mark(&end_mark(3, t0 + 15 * MS, Some("1")), 0);

        let block = asm.blocks().next().unwrap();
        assert!(block.completed());
        assert_eq!(block.exit(), Some(1));
        assert_eq!(block.duration(), Some(10 * MS));
        assert_eq!(block.command(), None, "no preexec → echo-slice fallback");
    }

    /// A missing `D` never panics: the next `A` proves the shell moved
    /// on, so the open block closes best-effort — `completed` stays
    /// `false`, no exit, no duration — and the command after it still
    /// assembles cleanly.
    #[test]
    fn a_missing_d_closes_on_the_next_prompt() {
        let t0 = Instant::now();
        let mut asm = BlockAssembler::new();
        asm.observe_mark(&mark(PromptMarkKind::PromptStart, 0, 0, t0), 0);
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 0, 2, t0 + MS), 0);
        asm.observe_mark(&mark(PromptMarkKind::OutputStart, 1, 0, t0 + 2 * MS), 0);
        // …command output lands on rows 1..4; D never arrives…
        asm.observe_mark(&mark(PromptMarkKind::PromptStart, 5, 0, t0 + 9 * MS), 0);
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 5, 2, t0 + 10 * MS), 0);
        asm.observe_mark(&end_mark(6, t0 + 20 * MS, Some("0")), 0);

        let blocks: Vec<_> = asm.blocks().collect();
        assert_eq!(blocks.len(), 2);
        assert!(!blocks[0].completed(), "no D — never finished");
        assert_eq!(blocks[0].exit(), None);
        assert_eq!(blocks[0].duration(), None);
        assert_eq!(
            blocks[0].range(0, 24),
            Some(0..=4),
            "ends above the new prompt"
        );
        assert!(blocks[1].completed());
        assert_eq!(blocks[1].range(0, 24), Some(5..=5));
    }

    /// Orphaned and duplicated marks are inert — never a panic, never a
    /// phantom block: `C`/`D` without a `B` open are ignored, a second
    /// `B` closes the first best-effort, and a stray second `D` is dropped.
    #[test]
    fn orphaned_and_duplicate_marks_degrade() {
        let t0 = Instant::now();
        let mut asm = BlockAssembler::new();
        // Orphans: no open block exists.
        asm.observe_mark(&mark(PromptMarkKind::OutputStart, 0, 0, t0), 0);
        asm.observe_mark(&end_mark(1, t0 + MS, Some("0")), 0);
        assert_eq!(asm.blocks().count(), 0);

        // B B D D: the second B abandons the first; the second D is an orphan.
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 2, 0, t0 + 2 * MS), 0);
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 3, 0, t0 + 3 * MS), 0);
        asm.observe_mark(&end_mark(4, t0 + 4 * MS, Some("0")), 0);
        asm.observe_mark(&end_mark(4, t0 + 5 * MS, Some("9")), 0);

        let blocks: Vec<_> = asm.blocks().collect();
        assert_eq!(blocks.len(), 2);
        assert!(!blocks[0].completed(), "abandoned by the second B");
        assert_eq!(blocks[0].range(0, 24), Some(2..=2));
        assert!(blocks[1].completed());
        assert_eq!(blocks[1].exit(), Some(0));
    }

    /// Out-of-order lines never invert a range — a `D` recorded above
    /// its `B` (a mid-burst mark landing early) still yields
    /// `start <= end`.
    #[test]
    fn out_of_order_lines_still_clamp() {
        let t0 = Instant::now();
        let mut asm = BlockAssembler::new();
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 8, 0, t0), 0);
        asm.observe_mark(&end_mark(7, t0 + MS, Some("0")), 0);

        let block = asm.blocks().next().unwrap();
        assert_eq!(block.range(0, 24), Some(8..=8), "end never precedes start");
    }

    /// A `preexec` arriving before its `B` (reordered stream) still
    /// attaches to the block — and a stale stash is cleared by `A`
    /// rather than leaking into an unrelated command.
    #[test]
    fn preexec_stash_pairs_with_the_next_b_and_dies_at_a() {
        let t0 = Instant::now();
        let mut asm = BlockAssembler::new();
        asm.observe_shell(&preexec("early cmd"));
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 0, 0, t0), 0);
        asm.observe_mark(&end_mark(1, t0 + MS, Some("0")), 0);
        assert_eq!(asm.blocks().next().unwrap().command(), Some("early cmd"));

        // A stale stash is dropped when a prompt arrives instead of a B.
        asm.observe_shell(&preexec("stale"));
        asm.observe_mark(&mark(PromptMarkKind::PromptStart, 2, 0, t0 + 2 * MS), 0);
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 2, 0, t0 + 3 * MS), 0);
        asm.observe_mark(&end_mark(3, t0 + 4 * MS, Some("0")), 0);
        assert_eq!(asm.blocks().nth(1).unwrap().command(), None);
    }

    /// Eviction slides absolute ranges up: a block whose lines all left
    /// the grid reports no range, a partially evicted one clamps its
    /// start to the oldest live row, and a shrinking extent (reflow)
    /// clamps the end — `resolve` filters the dropped ones out.
    #[test]
    fn evicted_ranges_drop_and_survivors_clamp() {
        let t0 = Instant::now();
        let mut asm = BlockAssembler::new();
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 0, 0, t0), 0);
        asm.observe_mark(&mark(PromptMarkKind::OutputStart, 1, 0, t0 + MS), 0);
        asm.observe_mark(&end_mark(3, t0 + 2 * MS, Some("0")), 0);
        let block = asm.blocks().next().unwrap();

        // Fully evicted: the top 5 lines are gone, the block sat at 0..3.
        assert_eq!(block.range(5, 10), None);
        // Partially evicted: start clamps to the oldest live row.
        assert_eq!(block.range(2, 10), Some(0..=0));
        // Shrunk extent (post-reflow): end clamps to the bottom.
        assert_eq!(block.range(0, 2), Some(0..=1));
        // Degenerate grid: no rows at all.
        assert_eq!(block.range(0, 0), None);
        // Resolve follows the same rule — dropped ranges vanish.
        let session = Session::new(SessionOptions {
            size: Size::new(40, 10),
            ..Default::default()
        });
        let grid = session.grid();
        assert!(block.resolve(&grid, 5, 10).is_none());
    }

    /// `dropped` at feed time maps recorded lines into absolute space:
    /// two identical recorded lines observed across a truncation are
    /// different absolute lines, and resolve against the post-truncation
    /// grid.
    #[test]
    fn dropped_at_feed_time_keeps_new_marks_live() {
        let t0 = Instant::now();
        let mut asm = BlockAssembler::new();
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 2, 0, t0), 0);
        asm.observe_mark(&end_mark(3, t0 + MS, Some("0")), 0);
        // Truncation: 4 lines dropped from the top. The next block's
        // marks arrive in fresh recorded coordinates.
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 1, 0, t0 + 2 * MS), 4);
        asm.observe_mark(&end_mark(2, t0 + 3 * MS, Some("0")), 4);

        let blocks: Vec<_> = asm.blocks().collect();
        assert_eq!(blocks[0].range(4, 10), None, "abs 2..=3 all below drop 4");
        assert_eq!(
            blocks[1].range(4, 10),
            Some(1..=1),
            "abs 5 (D col 0 → end 5) → live 1"
        );
    }

    /// The `B→C` echo slice: a byte-fed `Session` grid, a `B` mark on
    /// the prompt's row at the command's first column, `C` on the first
    /// output row — the slice is the typed command without the prompt.
    #[test]
    fn command_falls_back_to_the_echo_slice() {
        let mut session = Session::new(SessionOptions {
            size: Size::new(40, 10),
            ..Default::default()
        });
        session.feed_bytes(b"$ echo hi\nhello\n$ ");
        let grid = session.grid();
        let extent = grid.scrollback() + grid.lines();

        let t0 = Instant::now();
        let mut asm = BlockAssembler::new();
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 0, 2, t0), 0);
        asm.observe_mark(&mark(PromptMarkKind::OutputStart, 1, 0, t0 + MS), 0);
        asm.observe_mark(&end_mark(2, t0 + 2 * MS, Some("0")), 0);

        let block = asm
            .blocks()
            .next()
            .unwrap()
            .resolve(&grid, 0, extent)
            .unwrap();
        assert_eq!(block.command(), "echo hi");
        assert_eq!(block.range(), 0..=1, "D col 0 ends on the last output row");
        assert_eq!(block.output_start(), Some(1));
        assert_eq!(block.exit(), Some(0));
        assert!(block.completed());
    }

    /// `BLOCK_CAP` bounds the history: a long session keeps only the
    /// newest blocks, oldest dropping off the front.
    #[test]
    fn blocks_are_bounded() {
        let t0 = Instant::now();
        let mut asm = BlockAssembler::new();
        for i in 0..BLOCK_CAP + 5 {
            let line = i * 2;
            asm.observe_mark(&mark(PromptMarkKind::CommandStart, line, 0, t0), 0);
            asm.observe_mark(&end_mark(line + 1, t0, Some("0")), 0);
        }
        let blocks: Vec<_> = asm.blocks().collect();
        assert_eq!(blocks.len(), BLOCK_CAP);
        // The five oldest are gone — the survivor's start line moved up.
        assert_eq!(blocks[0].range(0, usize::MAX / 2), Some(10..=10));
    }

    /// A `D` landing mid-row — unterminated output — ends the block on
    /// that row at `column - 1`, so a copy stops before the prompt that
    /// follows (Spec R05 us-11/18); a `D` at column 0 ends one line up
    /// because its whole row is the next prompt.
    #[test]
    fn a_mid_row_d_bounds_the_end_column() {
        let mut session = Session::new(SessionOptions {
            size: Size::new(40, 10),
            ..Default::default()
        });
        // `no-newline` leaves the cursor mid-row; the prompt continues
        // on the same row — exactly what a mid-row `D` models.
        session.feed_bytes(b"$ echo x\nno-newline");
        let grid = session.grid();
        let extent = grid.scrollback() + grid.lines();

        let t0 = Instant::now();
        let mut asm = BlockAssembler::new();
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 0, 2, t0), 0);
        asm.observe_mark(&mark(PromptMarkKind::OutputStart, 1, 0, t0 + MS), 0);
        // `D` at column 10 on the output row: cells 0..9 are the
        // output tail, 10+ is whatever drew after.
        asm.observe_mark(
            &PromptMark {
                kind: PromptMarkKind::CommandEnd,
                payload: Some("0".to_owned()),
                line: 1,
                column: 10,
                at: t0 + 2 * MS,
                wall: SystemTime::UNIX_EPOCH,
            },
            0,
        );

        let block = asm
            .blocks()
            .next()
            .unwrap()
            .resolve(&grid, 0, extent)
            .unwrap();
        assert_eq!(block.range(), 0..=1);
        assert_eq!(
            block.end_column(),
            Some(9),
            "the copy stops before the prompt"
        );
    }

    /// Rotation insurance: ranges the assembler can no longer anchor
    /// drop wholesale rather than resolve stale (Spec R05 us-20 —
    /// mis-select must be impossible, a dropped range is the
    /// spec-sanctioned worst case).
    #[test]
    fn unanchorable_ranges_discard_cleanly() {
        let t0 = Instant::now();
        let mut asm = BlockAssembler::new();
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 0, 0, t0), 0);
        asm.observe_mark(&end_mark(1, t0 + MS, Some("0")), 0);
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 2, 0, t0 + 2 * MS), 0);
        assert!(!asm.is_empty());

        asm.discard_ranges();

        assert!(asm.is_empty());
        assert_eq!(asm.blocks().count(), 0);
    }

    /// Spec R08: sealing a block queues a snapshot for persistence —
    /// `take_sealed` drains it once, and `persisted` renders the
    /// `blocks` row's fields: the `preexec` command, the exit code and
    /// duration, and WALL-CLOCK bounds from the marks' paired
    /// `SystemTime`s (`started_at` is `C`'s wall — `duration`'s basis —
    /// `ended_at` is `D`'s).
    #[test]
    fn sealed_blocks_snapshot_persistable_metadata() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mark = |kind, line, column, at, wall| PromptMark {
            kind,
            payload: None,
            line,
            column,
            at,
            wall,
        };
        let mut asm = BlockAssembler::new();
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 0, 2, t0, w0), 0);
        asm.observe_shell(&preexec("sleep 1"));
        asm.observe_mark(
            &mark(PromptMarkKind::OutputStart, 1, 0, t0 + MS, w0 + MS),
            0,
        );
        asm.observe_mark(
            &PromptMark {
                kind: PromptMarkKind::CommandEnd,
                payload: Some("3".to_owned()),
                line: 2,
                column: 0,
                at: t0 + 3 * MS,
                wall: w0 + 3 * MS,
            },
            0,
        );

        let sealed = asm.take_sealed();
        assert!(asm.take_sealed().is_empty(), "the queue drains once");
        assert_eq!(sealed.len(), 1);
        let session = Session::new(SessionOptions {
            size: Size::new(40, 10),
            ..Default::default()
        });
        let persisted = sealed[0].persisted(&session.grid(), 0, 10, "/w".to_owned());
        assert_eq!(persisted.command.as_deref(), Some("sleep 1"));
        assert_eq!(persisted.cwd, "/w");
        assert_eq!(persisted.exit, Some(3));
        assert_eq!(persisted.duration, Some(2 * MS));
        assert_eq!(persisted.started_at, Some(w0 + MS), "C's wall time");
        assert_eq!(persisted.ended_at, Some(w0 + 3 * MS), "D's wall time");
    }

    /// Spec R08's nullable contract: a block abandoned without a `D`
    /// still persists — the row keeps what the hooks reported (`B`'s
    /// wall-clock start, the `preexec` command) and leaves `exit`,
    /// `duration` and `ended_at` NULL.
    #[test]
    fn abandoned_blocks_persist_with_null_outcome_fields() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut asm = BlockAssembler::new();
        asm.observe_mark(
            &PromptMark {
                kind: PromptMarkKind::CommandStart,
                payload: None,
                line: 0,
                column: 2,
                at: t0,
                wall: w0,
            },
            0,
        );
        asm.observe_shell(&preexec("ctrl-c'd thing"));
        // The next prompt proves the shell moved on — no `D` ever came.
        asm.observe_mark(&mark(PromptMarkKind::PromptStart, 5, 0, t0 + 9 * MS), 0);

        let sealed = asm.take_sealed();
        assert_eq!(sealed.len(), 1);
        let session = Session::new(SessionOptions {
            size: Size::new(40, 10),
            ..Default::default()
        });
        let persisted = sealed[0].persisted(&session.grid(), 0, 24, "/w".to_owned());
        assert_eq!(persisted.command.as_deref(), Some("ctrl-c'd thing"));
        assert_eq!(persisted.exit, None);
        assert_eq!(persisted.duration, None);
        assert_eq!(persisted.started_at, Some(w0), "B's wall — no C ever came");
        assert_eq!(persisted.ended_at, None, "the hooks never reported an end");
    }

    /// Marks-only tabs (`Absent`/`MarksOnly` integration) report no
    /// `preexec`: the persisted command falls back to the `B→C` echo
    /// slice resolved against the live grid — the same answer
    /// `Block::command` gives the user.
    #[test]
    fn persisted_command_falls_back_to_the_echo_slice() {
        let mut session = Session::new(SessionOptions {
            size: Size::new(40, 10),
            ..Default::default()
        });
        session.feed_bytes(b"$ echo hi\nhello\n$ ");
        let grid = session.grid();
        let extent = grid.scrollback() + grid.lines();

        let t0 = Instant::now();
        let mut asm = BlockAssembler::new();
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 0, 2, t0), 0);
        asm.observe_mark(&mark(PromptMarkKind::OutputStart, 1, 0, t0 + MS), 0);
        asm.observe_mark(&end_mark(2, t0 + 2 * MS, Some("0")), 0);

        let sealed = asm.take_sealed();
        assert_eq!(sealed.len(), 1);
        let persisted = sealed[0].persisted(&grid, 0, extent, "/w".to_owned());
        assert_eq!(persisted.command.as_deref(), Some("echo hi"));
    }

    /// A `preexec` report survives an unresolvable grid: when the
    /// recorded rows are gone (rotation past the cap) the persisted
    /// command is still the hook's text — metadata is never dropped
    /// just because the echo slice can't be re-read.
    #[test]
    fn persisted_keeps_the_hook_command_when_the_grid_is_gone() {
        let t0 = Instant::now();
        let mut asm = BlockAssembler::new();
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 0, 2, t0), 0);
        asm.observe_shell(&preexec("kept"));
        asm.observe_mark(&end_mark(1, t0 + MS, Some("0")), 0);

        let session = Session::new(SessionOptions {
            size: Size::new(40, 10),
            ..Default::default()
        });
        let sealed = asm.take_sealed();
        // `dropped` past every recorded line: resolve finds no range.
        let persisted = sealed[0].persisted(&session.grid(), 99, 10, "/w".to_owned());
        assert_eq!(persisted.command.as_deref(), Some("kept"));
        assert_eq!(persisted.exit, Some(0));
    }
}
