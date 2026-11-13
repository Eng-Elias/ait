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
//!   code.
//! - `{"verb":"text","text":"…"}` — one `egui::Event::Text`, the
//!   composed-text twin of real typing.
//! - `{"verb":"dump"}` — a versioned JSON snapshot of what the user
//!   sees, sourced from the same [`Shared`] state `ui`'s paint pass
//!   reads: the active tab's visible grid rows + cursor, the pane
//!   host's selection, and the window's content size.
//! - `{"verb":"quit"}` — the app takes its normal close path
//!   (`GuiApp::quit` → `ViewportCommand::Close`).
//!
//! Injected input joins the `dispatch_events` stream at frame start
//! via [`Shared::injected`]; the socket is drained in the pump loop
//! beside ctl (`drain_pass`), where a `dump` answers from post-drain
//! state. The control socket's request enum and `ait ctl` are
//! untouched — this socket is the only path to input injection.

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
    },
    /// `text` — one `egui::Event::Text` payload, composed text the way
    /// real typing delivers it.
    Text {
        /// The text to compose.
        text: String,
    },
    /// `dump` — answer with the versioned state snapshot.
    Dump,
    /// `quit` — close the window the normal way.
    Quit,
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
    /// The window's content area in points.
    pub window: WindowDump,
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
    /// Every visible row's text, top to bottom, trailing whitespace
    /// trimmed (the `screen_text` convention).
    pub rows: Vec<String>,
    /// The renderable cursor.
    pub cursor: CursorDump,
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

/// The window's content rect in points — the same `content_rect` the
/// `ui` pass layouts against.
#[derive(Debug, Serialize)]
pub struct WindowDump {
    /// Content width.
    pub width: f32,
    /// Content height.
    pub height: f32,
}

impl Dump {
    /// Snapshot `shared` — the state `ui`'s paint pass reads — plus
    /// the window's current content rect.
    fn capture(ctx: &egui::Context, shared: &Shared) -> Self {
        let grid = shared.tabs.active().map(|tab| {
            let cursor = tab.cursor();
            let size = tab.session.size();
            GridDump {
                columns: size.columns,
                lines: size.lines,
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
            window: WindowDump {
                width: area.width(),
                height: area.height(),
            },
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
    /// text. Everything downstream is production code.
    Event(egui::Event),
    /// Close the window — `GuiApp::quit`'s flag path.
    Quit,
}

/// The boot-degradation text a bind failure produces — mirrors
/// [`ait_app::ctl::socket_error_text`]: a bind-failed run simply
/// never gets its driver socket; the toast names why.
pub fn socket_error_text(error: impl std::fmt::Display) -> String {
    format!("test-driver socket unavailable: {error}")
}

/// The per-request dispatch: `key`/`text`/`quit` push onto
/// `shared.injected` for the next frame's `dispatch_events`, `dump`
/// answers from `shared` — the same state the paint pass reads.
fn dispatch(ctx: &egui::Context, shared: &mut Shared, request: &Request) -> Response {
    match request {
        Request::Key { chord } => match parse_chord(chord) {
            Ok((key, modifiers)) => {
                // Press then release — a real chord is both events;
                // the release is a no-op downstream, the press does
                // the work.
                for pressed in [true, false] {
                    shared.injected.push_back(Injected::Event(egui::Event::Key {
                        key,
                        physical_key: Some(key),
                        pressed,
                        repeat: false,
                        modifiers,
                    }));
                }
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
        Request::Dump => Response::dump(Dump::capture(ctx, shared)),
        Request::Quit => {
            shared.injected.push_back(Injected::Quit);
            Response::ok()
        }
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
        }
    }

    /// The codec half: every verb decodes from its wire shape and a
    /// malformed line is an error, never a silent default.
    #[test]
    fn requests_decode_from_their_wire_shapes() {
        assert!(matches!(
            decode_request(r#"{"verb":"key","chord":"Ctrl+Shift+E"}"#),
            Ok(Request::Key { .. })
        ));
        assert!(matches!(
            decode_request(r#"{"verb":"text","text":"echo hi"}"#),
            Ok(Request::Text { .. })
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
            r#"{"verb":"key"}"#,             // missing chord
            r#"{"verb":"key","chord":1}"#,   // wrong type
            r#"{"verb":"send","text":"x"}"#, // a ctl verb — NOT this socket
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
