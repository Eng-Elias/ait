//! [`ScreenState`]: the serializable screen dump a [`Driver`](crate::Driver)
//! reads (Spec E01, ticket E01-T2) — viewport text rows plus the cursor,
//! the same observables a user has.
//!
//! Two uses share the type: it is the `screen` argument of
//! `Driver::act`/`Driver::assert` (T3's `Command` backing ships it to a
//! subprocess as JSON), and it is the drift guard inside every recording —
//! its [`hash`](ScreenState::hash) is what replay re-asserts.
//!
//! Captured text is normalized the way [`E2e::screen_text`] is — trailing
//! blanks trimmed — plus the spawn's fresh temp cwd is redacted to
//! `[cwd]` so recordings stay stable across runs and machines (a spawned
//! `ait`'s status line prints its cwd verbatim).

use serde::{Deserialize, Serialize};

use crate::harness::E2e;

/// The placeholder every occurrence of the spawned program's cwd is
/// redacted to inside a [`ScreenState`].
const CWD_PLACEHOLDER: &str = "[cwd]";

/// One screen, frozen: visible rows as trimmed text plus the renderable
/// cursor and the grid size. Serializable — this is the `screen` a
/// [`Driver`](crate::Driver) judges, and the guard a recording replays
/// against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenState {
    /// Grid width in cells.
    pub columns: usize,
    /// Grid height (visible rows).
    pub lines: usize,
    /// The viewport rows, top→bottom, each trimmed of trailing blanks —
    /// the same rows [`E2e::screen_text`] joins, with the spawn's cwd
    /// redacted to `[cwd]`.
    pub rows: Vec<String>,
    /// The renderable cursor.
    pub cursor: CursorState,
}

/// The cursor as plain serializable data — [`ait_term::Cursor`] by value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorState {
    /// Viewport row (0-based).
    pub line: usize,
    /// Column (0-based).
    pub column: usize,
    /// The shape's name (`"Block"`, `"Beam"`, `"Hidden"`, …).
    pub shape: String,
}

impl ScreenState {
    /// The screen-state hash the recording guard asserts: an FNV-1a over
    /// size, rows and cursor — stable across runs, insensitive to
    /// everything except the observable screen itself. No binary hash is
    /// mixed in anywhere (Spec E01: a rebuild without behavior change
    /// must not invalidate a recording).
    pub fn hash(&self) -> u64 {
        let mut h = FNV_OFFSET;
        fnv(&mut h, &self.columns.to_le_bytes());
        fnv(&mut h, &self.lines.to_le_bytes());
        for row in &self.rows {
            fnv(&mut h, row.as_bytes());
            fnv(&mut h, b"\n");
        }
        fnv(&mut h, &self.cursor.line.to_le_bytes());
        fnv(&mut h, &self.cursor.column.to_le_bytes());
        fnv(&mut h, self.cursor.shape.as_bytes());
        h
    }

    /// The 16-hex-digit form recordings store.
    pub fn hash_hex(&self) -> String {
        format!("{:016x}", self.hash())
    }

    /// Capture `e2e`'s current screen (crate-internal; the public read is
    /// [`E2e::screen_state`]).
    pub(crate) fn capture(e2e: &E2e) -> Self {
        let cwd = e2e.cwd().to_string_lossy().into_owned();
        let size = e2e.size();
        let cursor = e2e.cursor();
        Self {
            columns: size.columns,
            lines: size.lines,
            rows: e2e
                .grid()
                .visible_rows()
                .map(|row| row.text().trim_end().replace(&cwd, CWD_PLACEHOLDER))
                .collect(),
            cursor: CursorState {
                line: cursor.line,
                column: cursor.column,
                shape: format!("{:?}", cursor.shape),
            },
        }
    }
}

/// FNV-1a 64 — tiny, stable, dependency-free; a drift guard wants a
/// fingerprint, not a cryptographic hash. The offset basis and the
/// folding step live here so every hash the crate computes
/// ([`ScreenState::hash`], `recording::text_hash`) shares one
/// implementation.
pub(crate) const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Fold `bytes` into FNV-1a state `h` (start `h` at [`FNV_OFFSET`]).
pub(crate) fn fnv(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= u64::from(b);
        *h = h.wrapping_mul(FNV_PRIME);
    }
}

/// A `- recorded` / `+ actual` line diff of two screens for the drift
/// failure message: each row is context (` `), removed (`-`) or added
/// (`+`), with size and cursor differences called out first.
pub(crate) fn diff(recorded: &ScreenState, actual: &ScreenState) -> String {
    let mut out = String::new();
    if (recorded.columns, recorded.lines) != (actual.columns, actual.lines) {
        out.push_str(&format!(
            "  size: recorded {}x{}, actual {}x{}\n",
            recorded.columns, recorded.lines, actual.columns, actual.lines
        ));
    }
    for i in 0..recorded.rows.len().max(actual.rows.len()) {
        match (recorded.rows.get(i), actual.rows.get(i)) {
            (Some(r), Some(a)) if r == a => out.push_str(&format!("  {r}\n")),
            (Some(r), Some(a)) => out.push_str(&format!("- {r}\n+ {a}\n")),
            (Some(r), None) => out.push_str(&format!("- {r}\n")),
            (None, Some(a)) => out.push_str(&format!("+ {a}\n")),
            (None, None) => {}
        }
    }
    if recorded.cursor != actual.cursor {
        out.push_str(&format!(
            "  cursor: recorded {:?}, actual {:?}\n",
            recorded.cursor, actual.cursor
        ));
    }
    out
}
