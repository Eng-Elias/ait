//! The pure, byte-fed [`Session`] core.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{self, ClipboardType, Osc52, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape as TermCursorShape, Processor};
use alacritty_terminal::vte::Parser;

use crate::events::{ClipboardSelection, OscObserver, SessionEvent};
use crate::grid::{Cursor, CursorShape, Damage, DamageBounds, GridView};
use crate::key::{self, EncodingModes, KeyEvent};

/// Default scrollback capacity, in lines (REQUIREMENTS.md §5.1).
pub const DEFAULT_SCROLLBACK: usize = 10_000;

/// Grid dimensions in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub columns: usize,
    pub lines: usize,
}

impl Size {
    pub fn new(columns: usize, lines: usize) -> Self {
        Self { columns, lines }
    }
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        // A zero-sized terminal is meaningless; clamp so the emulator never
        // sees a dimension of 0.
        self.lines.max(1)
    }

    fn screen_lines(&self) -> usize {
        self.lines.max(1)
    }

    fn columns(&self) -> usize {
        self.columns.max(1)
    }
}

/// Constructor options for [`Session`].
///
/// Configuration via `settings.toml` arrives in Round 3 (REQUIREMENTS.md §4);
/// for now sessions are configured here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionOptions {
    /// Initial grid size (the live driver resizes from the real window).
    pub size: Size,
    /// Maximum scrollback lines kept above the viewport.
    pub scrollback: usize,
    /// Honor OSC 52 clipboard-write requests: emit
    /// [`SessionEvent::ClipboardWrite`]. **Off by default** — remote or
    /// malicious output must never silently overwrite the user's clipboard
    /// (REQUIREMENTS.md §5.1). The flag also gates the emulator's own OSC 52
    /// handling, so the sequence is fully inert when off.
    pub osc52: bool,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            size: Size::new(80, 24),
            scrollback: DEFAULT_SCROLLBACK,
            osc52: false,
        }
    }
}

/// Terminal event sink. Replies the terminal generates for the PTY (device
/// status answers, clipboard loads, …) are queued for a future live driver to
/// write back; OSC 52 clipboard writes become [`SessionEvent`]s; everything
/// else is dropped.
#[derive(Debug, Default)]
struct SessionListener {
    pty_responses: Arc<Mutex<VecDeque<String>>>,
    events: Arc<Mutex<VecDeque<SessionEvent>>>,
}

impl EventListener for SessionListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => {
                if let Ok(mut queue) = self.pty_responses.lock() {
                    queue.push_back(text);
                }
            }
            Event::ClipboardStore(clipboard_type, contents) => {
                if let Ok(mut queue) = self.events.lock() {
                    queue.push_back(SessionEvent::ClipboardWrite {
                        selection: match clipboard_type {
                            ClipboardType::Clipboard => ClipboardSelection::Clipboard,
                            ClipboardType::Selection => ClipboardSelection::Selection,
                        },
                        contents,
                    });
                }
            }
            _ => {}
        }
    }
}

/// One terminal session: PTY output bytes in, styled grid out.
///
/// This is the pure core (Spec R01): no PTY, no TTY, no threads. Feed bytes
/// with [`feed_bytes`](Self::feed_bytes), read the styled
/// [`GridView`] with [`grid`](Self::grid), forward window changes with
/// [`resize`](Self::resize). A live PTY driver wraps this type in a later
/// ticket.
pub struct Session {
    term: Term<SessionListener>,
    processor: Processor,
    osc_parser: Parser,
    osc_observer: OscObserver,
    pty_responses: Arc<Mutex<VecDeque<String>>>,
    pending_events: Arc<Mutex<VecDeque<SessionEvent>>>,
}

impl Session {
    pub fn new(options: SessionOptions) -> Self {
        let config = term::Config {
            scrolling_history: options.scrollback,
            // The emulator decodes OSC 52 for us; `OnlyCopy` keeps paste
            // requests (`clipboard_load`) denied even when opted in.
            osc52: if options.osc52 {
                Osc52::OnlyCopy
            } else {
                Osc52::Disabled
            },
            ..Default::default()
        };
        let listener = SessionListener::default();
        let pty_responses = Arc::clone(&listener.pty_responses);
        let pending_events = Arc::clone(&listener.events);
        let term = Term::new(config, &options.size, listener);
        Self {
            term,
            processor: Processor::new(),
            osc_parser: Parser::new(),
            osc_observer: OscObserver::default(),
            pty_responses,
            pending_events,
        }
    }

    /// Feed raw PTY output bytes into the session.
    ///
    /// Returns the [`SessionEvent`]s observed on the stream (Spec R01 — the
    /// OSC interception seam): a parallel `vte` parser watches the same
    /// bytes, so nothing is stripped from what the emulator renders.
    ///
    /// Ordering guarantee: within one feed the events are grouped by
    /// source — emulator-side events (currently `OSC 52` clipboard
    /// writes) first, then the observer's events in stream order —
    /// because the two pipelines don't share a position index. Across
    /// feeds, events are chronological.
    pub fn feed_bytes(&mut self, bytes: &[u8]) -> Vec<SessionEvent> {
        self.processor.advance(&mut self.term, bytes);
        self.osc_parser.advance(&mut self.osc_observer, bytes);

        let mut events: Vec<SessionEvent> = match self.pending_events.lock() {
            Ok(mut queue) => queue.drain(..).collect(),
            Err(_) => Vec::new(),
        };
        events.append(&mut self.osc_observer.take_events());
        events
    }

    /// Resize the grid, reflowing wrapped lines.
    pub fn resize(&mut self, size: Size) {
        self.term.resize(size);
    }

    /// Current grid size.
    pub fn size(&self) -> Size {
        Size::new(self.term.columns(), self.term.screen_lines())
    }

    /// The renderable cursor: viewport position and shape.
    pub fn cursor(&self) -> Cursor {
        let cursor = self.term.renderable_content().cursor;
        Cursor {
            line: cursor.point.line.0.max(0) as usize,
            column: cursor.point.column.0,
            shape: match cursor.shape {
                TermCursorShape::Block => CursorShape::Block,
                TermCursorShape::HollowBlock => CursorShape::HollowBlock,
                TermCursorShape::Underline => CursorShape::Underline,
                TermCursorShape::Beam => CursorShape::Beam,
                TermCursorShape::Hidden => CursorShape::Hidden,
            },
        }
    }

    /// Read-only view of the grid (viewport + scrollback).
    pub fn grid(&self) -> GridView<'_> {
        GridView::new(self.term.grid())
    }

    /// Whether the application enabled bracketed-paste mode (`DECSET`
    /// 2004). Frontends consult this when sending paste text: wrap in
    /// `\x1b[200~` … `\x1b[201~` iff `true` (REQUIREMENTS.md §5.2).
    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Damage accumulated since the last `damage` call; clears the tracker.
    /// Starts as [`Damage::Full`] on a fresh session.
    ///
    /// The cursor's cell is always included in partial damage, so a renderer
    /// can repaint the cursor without special-casing it.
    pub fn damage(&mut self) -> Damage {
        let damage = match self.term.damage() {
            TermDamage::Full => Damage::Full,
            TermDamage::Partial(lines) => Damage::Partial(
                lines
                    .map(|line| DamageBounds {
                        line: line.line,
                        left: line.left,
                        right: line.right,
                    })
                    .collect(),
            ),
        };
        self.term.reset_damage();
        damage
    }

    /// Encode a [`KeyEvent`] into the bytes to write to the PTY.
    ///
    /// The encoding honors the modes the application set through its own
    /// output: application cursor keys (DECCKM) switch unmodified
    /// arrows/Home/End to their SS3 forms, and application keypad (DECKPAM)
    /// switches numpad keys. Frontends hand over `KeyEvent`s only — escape
    /// sequences are owned by the core.
    pub fn encode_key(&self, event: KeyEvent) -> Vec<u8> {
        let mode = self.term.mode();
        let modes = EncodingModes {
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            app_keypad: mode.contains(TermMode::APP_KEYPAD),
        };
        key::encode(&event, modes)
    }

    /// Responses the terminal generated for the PTY (e.g. device-status
    /// replies), drained from the queue. A live driver writes these back to
    /// the PTY master; byte-fed tests can assert on them.
    pub fn take_pty_responses(&mut self) -> Vec<String> {
        match self.pty_responses.lock() {
            Ok(mut queue) => queue.drain(..).collect(),
            Err(_) => Vec::new(),
        }
    }
}
