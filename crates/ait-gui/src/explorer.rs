//! The file-explorer pane (Spec R14 §5.13, ticket R14-T2): a
//! read-only browser rooted at the active tab's `cwd_for` answer —
//! OSC-7-reported cwd, the `/proc/<pid>/cwd` fallback where that
//! function's gate allows, spawn cwd last — re-rooted whenever the
//! answer changes (a `cd` the shell reports, or a tab switch). A live
//! remote session gates the whole pane: the "remote session —
//! browsing unavailable" marker shows and the local disk is never
//! listed under a remote path.
//!
//! [`ExplorerState`] is the model `Shared` carries: listing,
//! selection, preview and navigation. Every filesystem touch goes
//! through the free fns the tests drive over tempfile fixtures —
//! [`list_dir`] and [`load_preview`] — and the editor resolution
//! chain ([`resolve_editor`] + [`editor_command`]) takes its env
//! injected, so precedence and spawn construction are testable
//! without a real editor. The [`Pane`] half is the stateless
//! [`ExplorerPane`]/[`EXPLORER`] handle the registry holds: `key` and
//! `paint` drive the model through `Shared`, and nothing here can
//! mutate the filesystem — no rename, delete or create verb exists.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use ait_app::status::Toast;
use ait_app::suggest;
use ait_term::{Key, KeyEvent};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};

use crate::app::Shared;
use crate::grid::GridTheme;
use crate::pane::{Pane, PaneId};

/// How many bytes a file preview reads — the cap keeps a huge file
/// from stalling the window; anything past it marks the preview
/// truncated.
pub const PREVIEW_BYTES: usize = 64 * 1024;

/// How many leading bytes the binary sniff reads — a NUL in the head
/// settles the placeholder without reading the cap.
const BINARY_SNIFF: usize = 8 * 1024;

/// The remote marker — `tab.remote` non-empty swaps the pane's whole
/// body for this text (us-6). `pub(crate)` so T4's routing and the
/// verification checklist quote the same string.
pub(crate) const REMOTE_MARKER: &str = "remote session — browsing unavailable";

/// One listing row — a directory, a regular file or anything else.
/// Symlinks resolve through `fs::metadata`: one to a directory
/// navigates like the dir; a broken link or a special file stays
/// `Other` — it lists but never navigates or previews.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The entry's file name — the row's label.
    pub name: String,
    /// Its full path — the navigation and open target.
    pub path: PathBuf,
    /// What the row is.
    pub kind: EntryKind,
    /// A dotfile — `visible()` drops it until the toggle shows it.
    pub hidden: bool,
}

/// The row's shape — drives navigation and preview eligibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A directory (or a symlink resolving to one).
    Dir,
    /// A regular file — previewable, openable.
    File,
    /// Anything else — fifo, socket, device, broken symlink.
    Other,
}

/// The selected file's loaded preview (us-9/10): capped text, the
/// binary placeholder, or the read error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preview {
    /// A text file's first [`PREVIEW_BYTES`] bytes — `truncated` when
    /// the file continues past the cap.
    Text { text: String, truncated: bool },
    /// A non-text file — NUL bytes in the sniff head or invalid
    /// UTF-8; the placeholder, never mojibake.
    Binary,
    /// The file couldn't be read — its error text.
    Unreadable(String),
}

/// What `Enter`/`o` does on the selection — [`ExplorerState::activate`]
/// is the pure read; the pane routes the two verbs from it (navigate
/// vs. open-in-editor).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activate {
    /// Descend into the selected directory.
    EnterDir(PathBuf),
    /// Open the selected file (or other entry) in the system editor.
    OpenFile(PathBuf),
    /// Nothing selected — the key is a no-op.
    Nothing,
}

/// The explorer's mutable state on `Shared` — app-global like the
/// pane selection, re-synced to the active tab every routed key and
/// paint. `dir` can drift below the root (parent navigation is
/// unbounded — the pane is read-only, so browsing anywhere is safe);
/// `root` only changes hands when the tab's cwd answer does.
#[derive(Debug, Default)]
pub struct ExplorerState {
    /// A live remote session gates the pane — while set, every
    /// mutating verb no-ops and `visible()` is empty.
    remote: bool,
    /// The last local `cwd_for` answer `sync` rooted on.
    root: Option<PathBuf>,
    /// The directory the listing describes — `root` or anywhere the
    /// user browsed.
    dir: PathBuf,
    /// The dotfile visibility toggle — off by default (us-8),
    /// survives navigation and re-roots.
    show_hidden: bool,
    /// `dir`'s entries — all of them, hidden flagged; `visible()`
    /// applies the filter.
    entries: Vec<Entry>,
    /// The highlighted row — an index into `entries`, so toggling
    /// hidden never moves the highlight off its entry.
    selected: usize,
    /// The selected file's preview — `None` on dirs, `Other`s and
    /// while remote.
    preview: Option<Preview>,
    /// The listing's io error (unreadable dir, vanished path) —
    /// rendered as the error row, toasted once per new error by the
    /// pane's sync.
    error: Option<String>,
    /// The listing's scroll offset, in VISIBLE rows.
    scroll: usize,
    /// The listing window's row count the last paint fit — the
    /// PageUp/PageDown distance (like `GuiApp::panel_rows`).
    page_rows: usize,
}

impl ExplorerState {
    /// Reconcile the model with the active tab. `remote` gates the
    /// listing wholesale — no filesystem read happens while a remote
    /// session is live and the local disk is never listed under a
    /// remote path (us-6). A changed `root` re-roots: `dir` resets to
    /// the new root, selection and scroll rewind, the listing
    /// re-reads (us-3/4). Returning from remote re-reads the browsed
    /// dir when the local root is unchanged — nothing moved.
    pub fn sync(&mut self, remote: bool, root: Option<&Path>) {
        let was_remote = self.remote;
        self.remote = remote;
        if remote {
            self.entries.clear();
            self.preview = None;
            self.error = None;
            self.selected = 0;
            self.scroll = 0;
            return;
        }
        let Some(root) = root else {
            return;
        };
        if self.root.as_deref() != Some(root) {
            self.root = Some(root.to_path_buf());
            self.dir = root.to_path_buf();
            self.selected = 0;
            self.scroll = 0;
            self.relist();
        } else if was_remote {
            // Same local root back after the remote session — restore
            // the browsed listing.
            self.relist();
        }
    }

    /// Whether the remote gate holds — the marker replaces the body
    /// and every verb no-ops while it does.
    pub fn is_remote(&self) -> bool {
        self.remote
    }

    /// The synced session root, once one landed.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// The directory the listing describes — the row the header
    /// paints. Empty before the first `sync`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The listing's io error, if the last read failed.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Whether dotfiles show — the `visible()` filter's flag.
    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    /// The entries the listing renders, paired with their `entries`
    /// index — the pair a mouse click needs to `select` the right
    /// row. Empty while remote.
    pub fn visible(&self) -> impl Iterator<Item = (usize, &Entry)> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| self.show_hidden || !entry.hidden)
    }

    /// The highlighted row's index into `entries`.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The highlighted entry — `None` on an empty listing.
    pub fn selected_entry(&self) -> Option<&Entry> {
        self.entries.get(self.selected)
    }

    /// The selected file's loaded preview.
    pub fn preview(&self) -> Option<&Preview> {
        self.preview.as_ref()
    }

    /// The listing's scroll offset in visible rows.
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// What `Enter` resolves to on the selection — descend, open or
    /// nothing (the pane maps it to `navigate_to_dir`/`open_in_editor`).
    pub fn activate(&self) -> Activate {
        match self.selected_entry() {
            Some(entry) if entry.kind == EntryKind::Dir => Activate::EnterDir(entry.path.clone()),
            Some(entry) => Activate::OpenFile(entry.path.clone()),
            None => Activate::Nothing,
        }
    }

    /// Highlight `index` (an `entries` index, clamped) and load its
    /// preview — selecting a dir or `Other` clears it.
    pub fn select(&mut self, index: usize) {
        if self.remote || self.entries.is_empty() {
            self.selected = 0;
            self.preview = None;
            return;
        }
        self.selected = index.min(self.entries.len() - 1);
        self.load_selected_preview();
    }

    /// Move the highlight `delta` VISIBLE rows — the arrow-key verb;
    /// hidden rows are never landed on.
    pub fn move_by(&mut self, delta: i32) {
        let visible: Vec<usize> = self.visible().map(|(index, _)| index).collect();
        if visible.is_empty() {
            self.selected = 0;
            self.preview = None;
            return;
        }
        let position = visible
            .iter()
            .position(|&index| index == self.selected)
            .unwrap_or(0) as i64;
        let next = (position + i64::from(delta)).clamp(0, visible.len() as i64 - 1) as usize;
        self.select(visible[next]);
    }

    /// PageUp/PageDown — a page is the window the last paint fit.
    pub fn move_page(&mut self, delta: i32) {
        self.move_by(delta * self.page_rows() as i32);
    }

    /// Home/End — the first/last visible row.
    pub fn select_edge(&mut self, last: bool) {
        let index = if last {
            self.visible().map(|(index, _)| index).last()
        } else {
            self.visible().map(|(index, _)| index).next()
        };
        match index {
            Some(index) => self.select(index),
            None => {
                self.selected = 0;
                self.preview = None;
            }
        }
    }

    /// T4's Ctrl+click directory entry point — also `..`/`Enter` on a
    /// dir: `dir` becomes the listing, selection and scroll reset. An
    /// unreadable target still lands there — the error row shows and
    /// `..` walks back.
    pub fn navigate_to_dir(&mut self, dir: &Path) {
        if self.remote {
            return;
        }
        self.dir = dir.to_path_buf();
        self.selected = 0;
        self.scroll = 0;
        self.relist();
    }

    /// T4's Ctrl+click file entry point: browse to the file's parent
    /// (if it isn't already) then select+preview it. A directory
    /// target navigates instead.
    #[allow(dead_code)] // T4's Ctrl+click routing calls it; tested at the seam today
    pub fn select_file(&mut self, path: &Path) {
        if self.remote {
            return;
        }
        if path.is_dir() {
            self.navigate_to_dir(path);
            return;
        }
        if let Some(parent) = path.parent() {
            if parent != self.dir {
                self.dir = parent.to_path_buf();
                self.selected = 0;
                self.scroll = 0;
                self.relist();
            }
        }
        if self.error.is_some() {
            return;
        }
        match self.entries.iter().position(|entry| entry.path == path) {
            Some(index) => self.select(index),
            None => self.error = Some(format!("{}: no such entry", path.display())),
        }
    }

    /// `..`/Left/Backspace — browse to `dir`'s parent and re-select
    /// the dir we left (the "up" affordance's return breadcrumb).
    pub fn go_parent(&mut self) {
        if self.remote {
            return;
        }
        let Some(parent) = self.dir.parent().map(Path::to_path_buf) else {
            return;
        };
        let came_from = self.dir.clone();
        self.dir = parent;
        self.selected = 0;
        self.scroll = 0;
        self.relist();
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.path == came_from)
        {
            self.select(index);
        }
    }

    /// The explicit-refresh verb (`r` — no filesystem watcher, us-14):
    /// re-read `dir`, keeping the highlight where it still fits.
    pub fn refresh(&mut self) {
        if self.remote {
            return;
        }
        self.relist();
    }

    /// The dotfile toggle (`.`) — a filter flip, no re-read.
    pub fn toggle_hidden(&mut self) {
        if self.remote {
            return;
        }
        self.show_hidden = !self.show_hidden;
        self.scroll = self.scroll.min(self.visible().count().saturating_sub(1));
        self.snap_selected_to_visible();
        self.load_selected_preview();
    }

    /// The paint's window-feed: how many listing rows fit last frame —
    /// the PageUp/PageDown distance.
    pub fn set_page_rows(&mut self, rows: usize) {
        self.page_rows = rows;
    }

    /// Scroll the listing `delta` visible rows, clamped to the window
    /// `visible_count - window_rows` leaves.
    pub fn scroll_by(&mut self, delta: i32, window_rows: usize) {
        let max = self.visible().count().saturating_sub(window_rows);
        self.scroll = (self.scroll as i64 + i64::from(delta)).clamp(0, max as i64) as usize;
    }

    /// Pull the selected row into the window: scroll up until it tops
    /// the view or down until it bottoms it.
    pub fn ensure_visible(&mut self, window_rows: usize) {
        let position = self
            .visible()
            .position(|(index, _)| index == self.selected)
            .unwrap_or(0);
        if position < self.scroll {
            self.scroll = position;
        } else if window_rows > 0 && position >= self.scroll + window_rows {
            self.scroll = position + 1 - window_rows;
        }
    }

    /// The PageUp/PageDown distance — the painted window, 8 before
    /// the first paint reports one.
    fn page_rows(&self) -> usize {
        self.page_rows.max(8)
    }

    /// Re-read `dir` — the one place `list_dir` is called. The error
    /// lands on `error` (the row the pane renders); a clean read
    /// clears it.
    fn relist(&mut self) {
        match list_dir(&self.dir) {
            Ok(entries) => {
                self.entries = entries;
                self.error = None;
            }
            Err(error) => {
                self.entries = Vec::new();
                self.error = Some(format!("{}: {error}", self.dir.display()));
            }
        }
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        self.scroll = self.scroll.min(self.visible().count().saturating_sub(1));
        self.snap_selected_to_visible();
        self.load_selected_preview();
    }

    /// Keep the highlight on a VISIBLE row: a relist can reorder the
    /// selection onto a hidden entry (index 0 is `.hdir` while
    /// dotfiles are filtered) and hiding dotfiles can hide the
    /// selection itself — snap to the first visible row either way.
    fn snap_selected_to_visible(&mut self) {
        let current_visible = self
            .selected_entry()
            .is_some_and(|entry| self.show_hidden || !entry.hidden);
        if !current_visible {
            let first = self.visible().map(|(index, _)| index).next().unwrap_or(0);
            self.selected = first;
        }
    }

    /// The selection's preview: files read through [`load_preview`];
    /// dirs and `Other`s clear it.
    fn load_selected_preview(&mut self) {
        self.preview = self
            .selected_entry()
            .filter(|entry| entry.kind == EntryKind::File)
            .map(|entry| load_preview(&entry.path));
    }
}

/// `dir`'s listing — directories first, each half sorted by name
/// (case-insensitive, name tiebreak so the order never wobbles).
/// Hidden entries come back flagged, never filtered: the toggle is a
/// view change, not a re-read.
pub fn list_dir(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        // `metadata` follows symlinks — a link to a dir navigates.
        let kind = match fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => EntryKind::Dir,
            Ok(meta) if meta.is_file() => EntryKind::File,
            _ => EntryKind::Other,
        };
        let hidden = name.starts_with('.');
        entries.push(Entry {
            name,
            path,
            kind,
            hidden,
        });
    }
    entries.sort_by(|a, b| {
        let rank = |entry: &Entry| usize::from(entry.kind != EntryKind::Dir);
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(entries)
}

/// Read `path`'s head for the preview: at most [`PREVIEW_BYTES`], a
/// NUL in the first [`BINARY_SNIFF`] or invalid UTF-8 means binary —
/// the placeholder, never mojibake. A UTF-8 boundary split at the cap
/// trims to whole chars rather than reading binary.
pub fn load_preview(path: &Path) -> Preview {
    let bytes = match read_head(path, PREVIEW_BYTES + 1) {
        Ok(bytes) => bytes,
        Err(error) => return Preview::Unreadable(error.to_string()),
    };
    let truncated = bytes.len() > PREVIEW_BYTES;
    let head = &bytes[..bytes.len().min(PREVIEW_BYTES)];
    if head[..head.len().min(BINARY_SNIFF)].contains(&0) {
        return Preview::Binary;
    }
    match String::from_utf8(head.to_vec()) {
        Ok(text) => Preview::Text { text, truncated },
        Err(error) => {
            let utf8 = error.utf8_error();
            if truncated && utf8.error_len().is_none() {
                // An incomplete sequence straddling the cap — trim to
                // the last whole char instead of crying binary.
                let text = String::from_utf8_lossy(&head[..utf8.valid_up_to()]).into_owned();
                Preview::Text { text, truncated }
            } else {
                Preview::Binary
            }
        }
    }
}

/// The first `limit` bytes of `path`.
fn read_head(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.by_ref().take(limit as u64).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// What the editor resolution chain settled on (us-12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorResolution {
    /// `[gui].editor`, `$VISUAL` or `$EDITOR` — a shell-evaluated
    /// command line, so `"code --goto"` carries its own arguments.
    Program(String),
    /// The platform opener — `open` on macOS, `xdg-open` elsewhere.
    Opener,
}

/// The documented chain: `[gui].editor` → `$VISUAL` → `$EDITOR` → the
/// platform opener. Empty values never count — a blank `editor` or
/// unset env falls through to the next leg. `env` is injected so the
/// precedence is testable without touching the process env.
pub fn resolve_editor(
    configured: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
) -> EditorResolution {
    for candidate in [configured.map(str::to_owned), env("VISUAL"), env("EDITOR")]
        .into_iter()
        .flatten()
    {
        if !candidate.trim().is_empty() {
            return EditorResolution::Program(candidate);
        }
    }
    EditorResolution::Opener
}

/// The `Command` a resolution spawns — `Program` goes through `sh -c`
/// so `editor = "code --goto"` splits its own arguments while the
/// target path stays literal argv (never spliced into the evaluated
/// string — no quoting bug can smuggle it); `Opener` execs the
/// platform tool directly.
pub fn editor_command(resolution: &EditorResolution, path: &Path) -> Command {
    match resolution {
        EditorResolution::Program(program) => {
            let mut command = Command::new("sh");
            command
                .arg("-c")
                .arg(format!("{program} \"$@\""))
                .arg("ait-explorer")
                .arg(path);
            command
        }
        EditorResolution::Opener => {
            #[cfg(target_os = "macos")]
            const PLATFORM_OPENER: &str = "open";
            #[cfg(not(target_os = "macos"))]
            const PLATFORM_OPENER: &str = "xdg-open";
            let mut command = Command::new(PLATFORM_OPENER);
            command.arg(path);
            command
        }
    }
}

/// Spawn the editor detached: null stdio, its own process group on
/// unix so it survives the terminal's lifecycle, and the `Child`
/// dropped un-`wait`ed — the pane never blocks on the editor.
/// Failure is the io error the caller toasts.
pub fn spawn_detached(resolution: &EditorResolution, path: &Path) -> io::Result<()> {
    let mut command = editor_command(resolution, path);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn().map(|_| ())
}

/// The pane's tab→model sync: compute the remote gate and the
/// `cwd_for` root from the active tab, fold them into the model, and
/// toast a NEWLY-failed listing once (us-13 — a persistently
/// unreadable cwd doesn't re-toast every frame). `pub(crate)` so the
/// app tests drive the same seam the pane's `key`/`paint` do.
pub(crate) fn sync_explorer(shared: &mut Shared) {
    let (remote, root) = shared
        .tabs
        .active()
        .map(|tab| {
            (
                !tab.remote.is_empty(),
                Some(PathBuf::from(suggest::cwd_for(tab))),
            )
        })
        .unwrap_or((false, None));
    let before = shared.explorer.error().map(str::to_owned);
    shared.explorer.sync(remote, root.as_deref());
    let after = shared.explorer.error().map(str::to_owned);
    if let Some(error) = after {
        if Some(error.as_str()) != before.as_deref() {
            shared.toast = Some(Toast::new(error, Instant::now()));
        }
    }
}

/// "Open in system editor" (us-11/12): resolve through
/// `[gui].editor` → `$VISUAL` → `$EDITOR` → the platform opener and
/// spawn detached; a spawn failure toasts (us-13).
fn open_in_editor(shared: &mut Shared, path: &Path) {
    let configured = shared.config.settings().gui.editor.as_deref();
    let resolution = resolve_editor(configured, |name| std::env::var(name).ok());
    if let Err(error) = spawn_detached(&resolution, path) {
        shared.toast = Some(Toast::new(
            format!("cannot open {}: {error}", path.display()),
            Instant::now(),
        ));
    }
}

/// A click resolved after the paint pass — the same queued-action
/// pattern the switcher chips and gate card use (the responses
/// borrow `ui`, the mutation borrows `shared`).
enum ExplorerAction {
    /// The `..` row — browse to the parent.
    Parent,
    /// A row clicked — select it.
    Select(usize),
    /// A dir row double-clicked — descend.
    Enter(PathBuf),
    /// A file row double-clicked — open in the system editor.
    Open(PathBuf),
}

/// The explorer's registry handle — stateless like every `Pane`;
/// all mutable state lives in `Shared::explorer`.
pub struct ExplorerPane;

/// The `PaneId::Explorer` registry entry — `PANES` holds this where
/// T1's placeholder stood.
pub static EXPLORER: ExplorerPane = ExplorerPane;

impl Pane for ExplorerPane {
    fn id(&self) -> PaneId {
        PaneId::Explorer
    }

    fn title(&self) -> &'static str {
        "explorer"
    }

    fn chord(&self) -> egui::Key {
        egui::Key::E
    }

    /// The explorer's keys while it owns unbound input (the find
    /// bar's focus rule): arrows/Home/End/Page move the highlight,
    /// `Enter`/`→` descend, `Enter` on a file opens it in the editor,
    /// `←`/`⌫` go up, `.` toggles dotfiles, `r` refreshes, `o` opens
    /// the selection in the system editor. Modified keys fall
    /// through untouched — nothing reaches the PTY either way.
    fn key(&self, shared: &mut Shared, key: KeyEvent) {
        sync_explorer(shared);
        if shared.explorer.is_remote() || !key.modifiers.is_empty() {
            return;
        }
        match key.key {
            Key::Up => shared.explorer.move_by(-1),
            Key::Down => shared.explorer.move_by(1),
            Key::PageUp => shared.explorer.move_page(-1),
            Key::PageDown => shared.explorer.move_page(1),
            Key::Home => shared.explorer.select_edge(false),
            Key::End => shared.explorer.select_edge(true),
            Key::Enter => match shared.explorer.activate() {
                Activate::EnterDir(dir) => shared.explorer.navigate_to_dir(&dir),
                Activate::OpenFile(path) => open_in_editor(shared, &path),
                Activate::Nothing => {}
            },
            Key::Right => {
                if let Activate::EnterDir(dir) = shared.explorer.activate() {
                    shared.explorer.navigate_to_dir(&dir);
                }
            }
            Key::Left | Key::Backspace => shared.explorer.go_parent(),
            Key::Char('.') => shared.explorer.toggle_hidden(),
            Key::Char('r') => shared.explorer.refresh(),
            Key::Char('o') => {
                let path = match shared.explorer.activate() {
                    Activate::EnterDir(path) | Activate::OpenFile(path) => Some(path),
                    Activate::Nothing => None,
                };
                if let Some(path) = path {
                    open_in_editor(shared, &path);
                }
            }
            _ => {}
        }
    }

    /// No text input — composed text and pastes are swallowed while
    /// the pane shows (the focus rule); nothing reaches the PTY.
    fn text(&self, _shared: &mut Shared, _text: &str) {}

    /// The pane body under the switcher strip: a header naming the
    /// browsed dir, a pinned `..` row, the windowed listing
    /// (selected row reversed, dirs suffixed `/`), the preview split
    /// while a file is selected, and the footer hint. Clicks queue
    /// [`ExplorerAction`]s applied after the pass.
    fn paint(&self, ui: &mut egui::Ui, shared: &mut Shared, rect: Rect, theme: GridTheme) {
        sync_explorer(shared);
        let painter = ui.painter().with_clip_rect(rect);
        let font = FontId::monospace(13.0);
        let small = FontId::monospace(11.0);
        let row_h = ui.ctx().fonts_mut(|fonts| fonts.row_height(&font)).max(1.0);
        let dim = Color32::from_rgb(0x77, 0x77, 0x77);
        let sep = Color32::from_rgb(0x33, 0x33, 0x33);
        let hover = Color32::from_rgb(0x2A, 0x2A, 0x2A);

        if shared.explorer.is_remote() {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                REMOTE_MARKER,
                font,
                theme.fg,
            );
            return;
        }
        if shared.explorer.root().is_none() {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "no session",
                font,
                theme.fg,
            );
            return;
        }

        // The chrome rows: header (the browsed dir) and the footer
        // hint — fixed like the switcher strip, never cell-tied.
        let header = Rect::from_min_size(rect.left_top(), Vec2::new(rect.width(), row_h + 6.0));
        let footer = Rect::from_min_max(
            Pos2::new(rect.left(), rect.bottom() - row_h - 4.0),
            rect.right_bottom(),
        );
        let middle = Rect::from_min_max(
            Pos2::new(rect.left(), header.bottom()),
            Pos2::new(rect.right(), footer.top()),
        );
        // The preview takes the middle's bottom share while a file
        // preview is loaded.
        let (list_rect, preview_rect) = if shared.explorer.preview().is_some() {
            let split = middle.top() + middle.height() * 0.55;
            (
                Rect::from_min_max(middle.left_top(), Pos2::new(middle.right(), split)),
                Some(Rect::from_min_max(
                    Pos2::new(middle.left(), split),
                    middle.right_bottom(),
                )),
            )
        } else {
            (middle, None)
        };

        // The header names the browsed dir; a `· .*` tail marks the
        // dotfile toggle while it's on.
        let header_text = if shared.explorer.show_hidden() {
            format!("{} · .*", shared.explorer.dir().display())
        } else {
            shared.explorer.dir().display().to_string()
        };
        painter.text(
            header.left_center() + Vec2::new(8.0, 0.0),
            Align2::LEFT_CENTER,
            header_text,
            font.clone(),
            theme.fg,
        );
        painter.line_segment(
            [header.left_bottom(), header.right_bottom()],
            Stroke::new(1.0, sep),
        );

        // The listing: `..` pinned first, then the scrolled window of
        // visible rows. `window` counts entry rows (the `..` row eats
        // one) — it doubles as the PageUp/PageDown distance.
        let window = ((list_rect.height() / row_h).floor() as usize).saturating_sub(1);
        shared.explorer.set_page_rows(window.max(1));
        shared.explorer.ensure_visible(window.max(1));
        let scroll = shared.explorer.scroll();

        // The pane's own wheel — the app-level wheel router only
        // knows the grid and the agent conversation, so a flick over
        // the listing lands here (positive delta = scroll up).
        if ui.rect_contains_pointer(list_rect) {
            let delta = ui.input(|input| input.smooth_scroll_delta.y / row_h) as i32;
            if delta != 0 {
                shared.explorer.scroll_by(-delta, window.max(1));
            }
        }

        let mut action: Option<ExplorerAction> = None;
        let mut y = list_rect.top();
        let up_row = Rect::from_min_size(
            Pos2::new(list_rect.left(), y),
            Vec2::new(list_rect.width(), row_h),
        );
        if up_row.bottom() <= list_rect.bottom() {
            let hit = ui.interact(up_row, egui::Id::unique(("explorer", "up")), Sense::click());
            if hit.hovered() {
                painter.rect_filled(up_row, 0.0, hover);
            }
            painter.text(
                up_row.left_center() + Vec2::new(8.0, 0.0),
                Align2::LEFT_CENTER,
                "../",
                font.clone(),
                theme.fg,
            );
            if hit.clicked() {
                action = Some(ExplorerAction::Parent);
            }
        }
        y += row_h;

        if let Some(error) = shared.explorer.error() {
            let row = Rect::from_min_size(
                Pos2::new(list_rect.left(), y),
                Vec2::new(list_rect.width(), row_h),
            );
            painter.text(
                row.left_center() + Vec2::new(8.0, 0.0),
                Align2::LEFT_CENTER,
                error.to_owned(),
                small.clone(),
                Color32::from_rgb(0xE0, 0x50, 0x50),
            );
            y += row_h;
        }

        // A snapshot of the visible rows — the paint's `ui.interact`
        // borrows `ui` while the model stays borrowed immutably, so
        // the row data rides a clone (small names, one window).
        let rows: Vec<(usize, Entry)> = shared
            .explorer
            .visible()
            .map(|(index, entry)| (index, entry.clone()))
            .collect();
        let selected = shared.explorer.selected();
        for &(index, ref entry) in rows.iter().skip(scroll).take(window) {
            let row = Rect::from_min_size(
                Pos2::new(list_rect.left(), y),
                Vec2::new(list_rect.width(), row_h),
            );
            if row.bottom() > list_rect.bottom() {
                break;
            }
            let hit = ui.interact(row, egui::Id::unique(("explorer", index)), Sense::click());
            let label = if entry.kind == EntryKind::Dir {
                format!("{}/", entry.name)
            } else {
                entry.name.clone()
            };
            if index == selected {
                painter.rect_filled(row, 0.0, theme.fg);
                painter.text(
                    row.left_center() + Vec2::new(8.0, 0.0),
                    Align2::LEFT_CENTER,
                    label,
                    font.clone(),
                    theme.bg,
                );
            } else {
                if hit.hovered() {
                    painter.rect_filled(row, 0.0, hover);
                }
                let color = match entry.kind {
                    EntryKind::Dir => theme.fg,
                    EntryKind::File => theme.fg,
                    EntryKind::Other => dim,
                };
                painter.text(
                    row.left_center() + Vec2::new(8.0, 0.0),
                    Align2::LEFT_CENTER,
                    label,
                    font.clone(),
                    color,
                );
            }
            if hit.double_clicked() {
                action = Some(match entry.kind {
                    EntryKind::Dir => ExplorerAction::Enter(entry.path.clone()),
                    _ => ExplorerAction::Open(entry.path.clone()),
                });
            } else if hit.clicked() {
                action = Some(ExplorerAction::Select(index));
            }
            y += row_h;
        }

        // The preview split: separator, then the capped text or the
        // binary/error placeholder.
        if let (Some(preview), Some(preview_rect)) = (shared.explorer.preview(), preview_rect) {
            painter.line_segment(
                [preview_rect.left_top(), preview_rect.right_top()],
                Stroke::new(1.0, sep),
            );
            let rows = (preview_rect.height() / row_h).floor() as usize;
            match preview {
                Preview::Text { text, truncated } => {
                    for (line_index, line) in text.lines().take(rows).enumerate() {
                        painter.text(
                            Pos2::new(
                                preview_rect.left() + 8.0,
                                preview_rect.top() + 3.0 + line_index as f32 * row_h,
                            ),
                            Align2::LEFT_TOP,
                            line.to_owned(),
                            small.clone(),
                            theme.fg,
                        );
                    }
                    if *truncated {
                        painter.text(
                            Pos2::new(preview_rect.right() - 8.0, preview_rect.top() + 3.0),
                            Align2::RIGHT_TOP,
                            "…",
                            small.clone(),
                            dim,
                        );
                    }
                }
                Preview::Binary => {
                    painter.text(
                        preview_rect.center(),
                        Align2::CENTER_CENTER,
                        "binary file — no preview",
                        small.clone(),
                        dim,
                    );
                }
                Preview::Unreadable(error) => {
                    painter.text(
                        preview_rect.center(),
                        Align2::CENTER_CENTER,
                        format!("cannot preview: {error}"),
                        small.clone(),
                        Color32::from_rgb(0xE0, 0x50, 0x50),
                    );
                }
            }
        }

        painter.text(
            footer.left_center() + Vec2::new(8.0, 0.0),
            Align2::LEFT_CENTER,
            "↑↓ select · ⏎/→ open · ←/⌫ up · . hidden · r refresh · o editor · esc close",
            small,
            dim,
        );

        match action {
            Some(ExplorerAction::Parent) => shared.explorer.go_parent(),
            Some(ExplorerAction::Select(index)) => shared.explorer.select(index),
            Some(ExplorerAction::Enter(dir)) => shared.explorer.navigate_to_dir(&dir),
            Some(ExplorerAction::Open(path)) => open_in_editor(shared, &path),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    //! The explorer's pure seams over tempfile fixtures (Spec R14
    //! testing decisions): `list_dir`/`ExplorerState` navigation on a
    //! real tempdir, `load_preview`'s cap and binary gate on fixture
    //! bytes, `resolve_editor`/`editor_command` on injected env — no
    //! PTY, no window, no real editor spawn.

    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// The fixture dir every model test builds on:
    ///   .hdir/ adir/ zdir/   — directories, one hidden
    ///   .hfile afile.txt bfile.txt — files, one hidden
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for name in ["zdir", "adir", ".hdir"] {
            fs::create_dir(dir.path().join(name)).unwrap();
        }
        for (name, body) in [
            ("bfile.txt", "beta\n"),
            ("afile.txt", "alpha\n"),
            (".hfile", "secret\n"),
        ] {
            fs::write(dir.path().join(name), body).unwrap();
        }
        dir
    }

    fn names(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.name.as_str()).collect()
    }

    fn visible_names(state: &ExplorerState) -> Vec<&str> {
        state
            .visible()
            .map(|(_, entry)| entry.name.as_str())
            .collect()
    }

    // ---- listing (us-8) ----

    /// Directories sort before files, each half by name; hidden
    /// entries come back flagged, never filtered — the toggle is a
    /// view change.
    #[test]
    fn listing_sorts_dirs_first_and_flags_hidden() {
        let dir = fixture();
        let entries = list_dir(dir.path()).unwrap();
        assert_eq!(
            names(&entries),
            [".hdir", "adir", "zdir", ".hfile", "afile.txt", "bfile.txt"]
        );
        for (index, (kind, hidden)) in [
            (EntryKind::Dir, true),
            (EntryKind::Dir, false),
            (EntryKind::Dir, false),
            (EntryKind::File, true),
            (EntryKind::File, false),
            (EntryKind::File, false),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(entries[index].kind, kind, "entry {index}");
            assert_eq!(entries[index].hidden, hidden, "entry {index}");
        }
    }

    /// A symlink resolves through `metadata`: one to a dir navigates
    /// like the dir, a broken one stays `Other` — it lists but never
    /// navigates or previews.
    #[test]
    fn symlinks_resolve_and_broken_ones_stay_other() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("real_dir")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("real_dir"), dir.path().join("dir_link"))
            .unwrap();
        std::os::unix::fs::symlink(dir.path().join("gone"), dir.path().join("dead_link")).unwrap();
        let entries = list_dir(dir.path()).unwrap();
        let kind_of = |name: &str| entries.iter().find(|e| e.name == name).unwrap().kind;
        assert_eq!(kind_of("dir_link"), EntryKind::Dir);
        assert_eq!(kind_of("dead_link"), EntryKind::Other);
    }

    /// An unreadable directory is an io error — the model turns it
    /// into the error row, never a panic or an empty listing that
    /// looks like a real empty dir.
    #[test]
    fn an_unreadable_dir_reports_its_error() {
        let dir = fixture();
        let mut state = ExplorerState::default();
        // A nonexistent target errors the same way — the portable
        // half of the assertion.
        state.navigate_to_dir(&dir.path().join("missing"));
        let error = state.error().unwrap();
        assert!(
            error.contains("missing"),
            "the error names the dir: {error}"
        );
        assert_eq!(state.visible().count(), 0);

        // Where the fs honors permissions a `0o000` dir errors
        // identically (root reads through — the assert falls back to
        // the nonexistent path, which exercises the same code path).
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&locked).is_err() {
            state.navigate_to_dir(&locked);
            assert!(
                state.error().is_some_and(|e| e.contains("locked")),
                "locked dir must error: {:?}",
                state.error()
            );
        }
    }

    // ---- the model: sync, filter, selection ----

    /// The first `sync` roots on the tab's cwd and lists; `visible()`
    /// drops dotfiles until `toggle_hidden` — a view flip, no re-read.
    #[test]
    fn sync_roots_and_hidden_toggles_the_filter() {
        let dir = fixture();
        let mut state = ExplorerState::default();
        state.sync(false, Some(dir.path()));
        assert_eq!(state.root(), Some(dir.path()));
        assert_eq!(state.dir(), dir.path());
        assert_eq!(
            visible_names(&state),
            ["adir", "zdir", "afile.txt", "bfile.txt"]
        );
        state.toggle_hidden();
        assert!(state.show_hidden());
        assert_eq!(
            visible_names(&state),
            [".hdir", "adir", "zdir", ".hfile", "afile.txt", "bfile.txt"]
        );
    }

    /// us-3: a changed root re-roots — `dir` resets to the new root
    /// (a browsed position doesn't follow across re-roots) and the
    /// listing re-reads. The same root re-synced is a no-op.
    #[test]
    fn a_root_change_re_roots_and_relists() {
        let first = fixture();
        let second = tempfile::tempdir().unwrap();
        fs::create_dir(second.path().join("elsewhere")).unwrap();
        let mut state = ExplorerState::default();
        state.sync(false, Some(first.path()));
        state.navigate_to_dir(&first.path().join("adir"));
        assert_ne!(state.dir(), first.path(), "browsed away from root");

        state.sync(false, Some(second.path()));
        assert_eq!(state.dir(), second.path());
        assert_eq!(visible_names(&state), ["elsewhere"]);
        assert_eq!(state.selected(), 0);
    }

    /// us-7: `navigate_to_dir` descends; `go_parent` walks back and
    /// re-selects the dir we left — the up affordance's breadcrumb.
    #[test]
    fn navigation_descends_and_parent_selects_where_we_left() {
        let dir = fixture();
        fs::write(dir.path().join("adir").join("inner.txt"), "inner\n").unwrap();
        let mut state = ExplorerState::default();
        state.sync(false, Some(dir.path()));

        state.navigate_to_dir(&dir.path().join("adir"));
        assert_eq!(state.dir(), dir.path().join("adir"));
        assert_eq!(visible_names(&state), ["inner.txt"]);

        state.go_parent();
        assert_eq!(state.dir(), dir.path());
        assert_eq!(
            state.selected_entry().unwrap().name,
            "adir",
            "the dir we left is re-selected"
        );
    }

    /// `activate` is the Enter/`o` read: a dir enters, a file opens,
    /// `Other` opens, an empty listing is nothing.
    #[test]
    fn activate_reads_the_selections_verb() {
        let dir = fixture();
        let mut state = ExplorerState::default();
        state.sync(false, Some(dir.path()));
        // First row is `adir` — Enter descends.
        assert_eq!(
            state.activate(),
            Activate::EnterDir(dir.path().join("adir"))
        );
        let file_index = state
            .visible()
            .find(|(_, e)| e.name == "afile.txt")
            .map(|(i, _)| i)
            .unwrap();
        state.select(file_index);
        assert_eq!(
            state.activate(),
            Activate::OpenFile(dir.path().join("afile.txt"))
        );
    }

    /// Arrow keys move through the VISIBLE rows — a hidden entry is
    /// never landed on — and the edges clamp.
    #[test]
    fn move_by_walks_only_visible_rows() {
        let dir = fixture();
        let mut state = ExplorerState::default();
        state.sync(false, Some(dir.path()));
        assert_eq!(state.selected_entry().unwrap().name, "adir");
        state.move_by(1);
        assert_eq!(state.selected_entry().unwrap().name, "zdir");
        state.move_by(1);
        assert_eq!(state.selected_entry().unwrap().name, "afile.txt");
        // Bottom clamp.
        state.move_by(10);
        assert_eq!(state.selected_entry().unwrap().name, "bfile.txt");
        // Top clamp.
        state.move_by(-10);
        assert_eq!(state.selected_entry().unwrap().name, "adir");
        // The hidden file between zdir and afile.txt is skipped only
        // because it's filtered — with the toggle on it lands.
        state.toggle_hidden();
        state.select_edge(false);
        assert_eq!(state.selected_entry().unwrap().name, ".hdir");
    }

    /// T4's entry points: `select_file` browses to the file's parent
    /// and previews it; `navigate_to_dir` is the directory form.
    #[test]
    fn select_file_reveals_across_directories() {
        let dir = fixture();
        let nested = dir.path().join("adir");
        fs::write(nested.join("deep.txt"), "deep\n").unwrap();
        let mut state = ExplorerState::default();
        state.sync(false, Some(dir.path()));

        state.select_file(&nested.join("deep.txt"));
        assert_eq!(state.dir(), nested);
        assert_eq!(state.selected_entry().unwrap().name, "deep.txt");
        assert_eq!(
            state.preview(),
            Some(&Preview::Text {
                text: "deep\n".to_owned(),
                truncated: false
            })
        );
    }

    // ---- preview (us-9/10) ----

    /// Selecting a file loads its preview; selecting a dir clears it.
    #[test]
    fn selecting_a_file_loads_its_preview() {
        let dir = fixture();
        let mut state = ExplorerState::default();
        state.sync(false, Some(dir.path()));
        let file_index = state
            .visible()
            .find(|(_, e)| e.name == "afile.txt")
            .map(|(i, _)| i)
            .unwrap();
        state.select(file_index);
        assert_eq!(
            state.preview(),
            Some(&Preview::Text {
                text: "alpha\n".to_owned(),
                truncated: false
            })
        );
        // A dir selection clears the preview again.
        let dir_index = state
            .visible()
            .find(|(_, e)| e.name == "adir")
            .map(|(i, _)| i)
            .unwrap();
        state.select(dir_index);
        assert_eq!(state.preview(), None);
    }

    /// A file past the cap previews only `PREVIEW_BYTES`, flagged
    /// truncated.
    #[test]
    fn preview_caps_at_the_byte_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        fs::write(&path, "a".repeat(PREVIEW_BYTES + 100)).unwrap();
        match load_preview(&path) {
            Preview::Text { text, truncated } => {
                assert!(truncated);
                assert_eq!(text.len(), PREVIEW_BYTES);
            }
            other => panic!("expected a capped text preview, got {other:?}"),
        }
    }

    /// A UTF-8 char straddling the cap trims to the boundary — the
    /// split must not read as binary.
    #[test]
    fn preview_trims_a_split_multibyte_char() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("split.txt");
        // `PREVIEW_BYTES - 1` ascii bytes + the 2-byte 'é': byte
        // PREVIEW_BYTES holds é's second half — a naive decode would
        // fail right at the cap.
        let mut body = "a".repeat(PREVIEW_BYTES - 1).into_bytes();
        body.extend_from_slice("é".as_bytes());
        fs::write(&path, &body).unwrap();
        match load_preview(&path) {
            Preview::Text { text, truncated } => {
                assert!(truncated);
                assert!(text.ends_with('a'), "trimmed at the split char");
                assert_eq!(text.len(), PREVIEW_BYTES - 1);
            }
            other => panic!("expected a trimmed text preview, got {other:?}"),
        }
    }

    /// NUL bytes or invalid UTF-8 anywhere in the read head read as
    /// binary — the placeholder, never mojibake.
    #[test]
    fn binary_files_show_the_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let with_nul = dir.path().join("bin.dat");
        fs::write(&with_nul, [b"header".as_slice(), &[0], b"tail"].concat()).unwrap();
        assert_eq!(load_preview(&with_nul), Preview::Binary);

        let bad_utf8 = dir.path().join("bad.dat");
        fs::write(&bad_utf8, [0xFF, 0xFE, 0xFD, 0x41]).unwrap();
        assert_eq!(load_preview(&bad_utf8), Preview::Binary);
    }

    /// An unreadable file previews its error text.
    #[test]
    fn an_unreadable_file_reports_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        match load_preview(&dir.path().join("missing")) {
            Preview::Unreadable(_) => {}
            other => panic!("expected the read error, got {other:?}"),
        }
    }

    // ---- the remote gate (us-6) ----

    /// While `remote` holds the model never lists — `visible()` is
    /// empty, every verb no-ops — and coming back to an unchanged
    /// local root restores the browsed position rather than
    /// re-rooting.
    #[test]
    fn remote_gates_the_listing_and_the_verbs() {
        let dir = fixture();
        fs::write(dir.path().join("adir").join("inner.txt"), "inner\n").unwrap();
        let mut state = ExplorerState::default();
        state.sync(false, Some(dir.path()));
        state.navigate_to_dir(&dir.path().join("adir"));
        let browsed = state.dir().to_path_buf();

        state.sync(true, Some(dir.path()));
        assert!(state.is_remote());
        assert_eq!(state.visible().count(), 0);
        assert_eq!(state.selected_entry(), None);
        assert_eq!(state.preview(), None);
        // Verbs are inert under the gate — a remote path must never
        // reach the local fs.
        let before = state.dir().to_path_buf();
        state.navigate_to_dir(Path::new("/"));
        state.refresh();
        state.toggle_hidden();
        assert_eq!(state.dir(), before);
        assert_eq!(state.visible().count(), 0);

        state.sync(false, Some(dir.path()));
        assert!(!state.is_remote());
        assert_eq!(state.dir(), browsed, "the browsed position restored");
        assert_eq!(visible_names(&state), ["inner.txt"]);
    }

    // ---- scroll ----

    /// `ensure_visible` pulls the selected row into the window;
    /// `scroll_by` clamps to the listing's tail.
    #[test]
    fn scroll_tracks_the_selection_into_the_window() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..30 {
            fs::write(dir.path().join(format!("f{index:02}.txt")), "x").unwrap();
        }
        let mut state = ExplorerState::default();
        state.sync(false, Some(dir.path()));
        let window = 10;
        state.select_edge(true); // last row
        state.ensure_visible(window);
        assert!(state.scroll() > 0, "selection is below the window");
        state.select_edge(false);
        state.ensure_visible(window);
        assert_eq!(state.scroll(), 0);
        state.scroll_by(100, window);
        assert_eq!(state.scroll(), 30 - window);
        state.scroll_by(-100, window);
        assert_eq!(state.scroll(), 0);
    }

    // ---- the editor chain (us-12) ----

    /// `[gui].editor` beats `$VISUAL`, which beats `$EDITOR`, which
    /// beats the platform opener — empty values never count.
    #[test]
    fn editor_resolution_precedence() {
        let env = |pairs: &[(&str, &str)]| {
            let pairs: Vec<(String, String)> = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.clone())
            }
        };
        assert_eq!(
            resolve_editor(Some("code --goto"), env(&[("VISUAL", "vi")])),
            EditorResolution::Program("code --goto".into())
        );
        assert_eq!(
            resolve_editor(None, env(&[("VISUAL", "vi"), ("EDITOR", "nano")])),
            EditorResolution::Program("vi".into())
        );
        // A blank override and a blank $VISUAL fall through.
        assert_eq!(
            resolve_editor(Some("  "), env(&[("VISUAL", "vi")])),
            EditorResolution::Program("vi".into())
        );
        assert_eq!(
            resolve_editor(None, env(&[("VISUAL", ""), ("EDITOR", "nano")])),
            EditorResolution::Program("nano".into())
        );
        assert_eq!(resolve_editor(None, env(&[])), EditorResolution::Opener);
    }

    /// The spawn construction: a named editor goes through `sh -c` —
    /// so `editor = "code --goto"` splits its own args — with the
    /// target path passed as literal argv, never spliced into the
    /// evaluated string (a `;` in a path can't smuggle a command).
    /// The opener execs the platform tool directly.
    #[test]
    fn editor_command_constructs_the_spawn() {
        let hostile = Path::new("/tmp/f; touch /tmp/pwned");
        let command = editor_command(&EditorResolution::Program("code --goto".into()), hostile);
        assert_eq!(command.get_program(), "sh");
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap().to_owned())
            .collect();
        assert_eq!(args[0], "-c");
        assert_eq!(args[1], "code --goto \"$@\"");
        assert!(
            !args[1].contains("touch"),
            "the path never enters the shell string: {args:?}"
        );
        assert_eq!(
            args.last().unwrap(),
            &hostile.to_string_lossy().into_owned()
        );

        let command = editor_command(&EditorResolution::Opener, hostile);
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        assert_eq!(command.get_program(), opener);
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, [hostile.as_os_str()]);
    }
}
