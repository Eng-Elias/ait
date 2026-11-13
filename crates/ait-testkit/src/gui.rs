//! The GUI e2e harness (Spec R15, ticket R15-T1): [`GuiE2e`] spawns the
//! real `ait-gui` binary under a display and drives it over the app's
//! test-driver socket — the same bounded-wait, evidence-dropping
//! conventions [`E2e`] established for the PTY harness, aimed at the
//! window.
//!
//! The socket is the app's, not the harness's: a `test-driver`-feature
//! build binds it only when [`ENV_DRIVER_SOCKET`] names a path, and the
//! wire is the ctl socket's shape — one request per connection,
//! line-delimited JSON. This module is the protocol's only client; it
//! re-declares the env-var name and dump shape (the crate boundary
//! keeps the server dev-free of testkit and vice versa — keep the two
//! in lockstep).
//!
//! Display policy: headed (`AIT_GUI_E2E_HEADED` or
//! [`GuiSpawnConfig::headed`]) requires `DISPLAY` — a headed run on the
//! developer's screen. Headless prefers `xvfb-run` (an owned virtual
//! display — the suite never steals desktop focus) and falls back to
//! `DISPLAY` when xvfb is unavailable. With neither, [`GuiE2e::spawn`]
//! returns `None` — the skip convention the GUI suite codes on, never
//! a failure.
//!
//! The scenario layer mirrors [`crate::Scenario`]: a [`GuiScenario`]
//! is a TOML file under `crates/ait-gui/tests/scenarios/` — `feature`
//! tag, `env` pairs, ordered `[[steps]]` — run through a spawned
//! [`GuiE2e`]. GUI verbs are the driver protocol's verbs plus the
//! bounded dump assertions.

use std::cell::RefCell;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, Read, Write as _};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::evidence::{self, ActionEntry};
use crate::screen::{fnv, FNV_OFFSET};
use crate::tempdir::TempDir;

/// The env var naming the app's test-driver socket — the same string
/// `ait-gui`'s `test_driver` module declares (the crate boundary is
/// why it is written twice).
pub const ENV_DRIVER_SOCKET: &str = "AIT_TEST_DRIVER_SOCKET";

/// The env var that forces headed mode for every GUI spawn — the
/// `AIT_E2E_HEADED` convention, GUI-spelled.
const HEADED_ENV: &str = "AIT_GUI_E2E_HEADED";

/// How long a wait/poll may run — the harness's bounded-wait bound:
/// generous for a cold Xvfb+wgpu boot, never open-ended.
pub const WAIT_TIMEOUT: Duration = Duration::from_secs(15);

/// The dump poll interval — the only sleep the harness takes (bounded
/// waits poll; they never guess a duration).
const WAIT_POLL: Duration = Duration::from_millis(10);

/// How long [`GuiE2e::spawn`] may wait for the app to bind + service
/// its socket — cold-build wgpu boots are slow; still bounded.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(60);

/// One socket round trip's ceiling — the server answers in one drain
/// pass, so a response taking this long means the app is wedged.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The pinned env every spawned GUI gets — the PTY harness's pins
/// (SHELL/PS1/XDG_CONFIG_HOME/skip_global_compinit) plus the GUI's own:
/// `HOME` and `XDG_RUNTIME_DIR` inside the throwaway dir, so a spawned
/// app reads no host config, loads no host dotfiles into its tab
/// shells, and places its ctl/runtime sockets under a dir the harness
/// owns.
fn pinned_env(cwd: &Path, socket: &Path) -> io::Result<Vec<(OsString, OsString)>> {
    use std::os::unix::fs::PermissionsExt;
    let runtime = cwd.join("xdg-runtime");
    std::fs::create_dir(&runtime)?;
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))?;
    Ok(vec![
        (OsString::from("SHELL"), OsString::from("/bin/sh")),
        (OsString::from("PS1"), OsString::from("$ ")),
        (OsString::from("HOME"), cwd.as_os_str().to_os_string()),
        (
            OsString::from("XDG_CONFIG_HOME"),
            cwd.join("xdg-config").into_os_string(),
        ),
        (OsString::from("XDG_RUNTIME_DIR"), runtime.into_os_string()),
        (OsString::from("skip_global_compinit"), OsString::from("1")),
        (
            OsString::from(ENV_DRIVER_SOCKET),
            socket.as_os_str().to_os_string(),
        ),
    ])
}

/// How the spawned binary reaches a display.
enum Display {
    /// Spawn directly — `DISPLAY` (or the user's) is already set.
    Direct,
    /// Wrap the spawn in `xvfb-run -a` — the suite's own virtual
    /// display, no focus stealing.
    Xvfb(PathBuf),
}

/// The headed flag's truthiness — `AIT_GUI_E2E_HEADED` / config.
fn env_flag(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false"),
        Err(_) => false,
    }
}

/// Find `xvfb-run` on `PATH`.
fn xvfb_run() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("xvfb-run"))
        .find(|candidate| candidate.exists())
}

/// The display decision's pure core — `display`'s env reads live at
/// the seam so the policy itself is a testable function (env is
/// process-global; a test that mutated DISPLAY/PATH would race its
/// siblings).
fn choose_display(headed: bool, have_display: bool, xvfb: Option<PathBuf>) -> Option<Display> {
    if headed {
        // Headed means the developer's screen — without one there is
        // nothing to watch.
        return have_display.then_some(Display::Direct);
    }
    // Headless prefers a virtual display it owns.
    if let Some(xvfb) = xvfb {
        return Some(Display::Xvfb(xvfb));
    }
    have_display.then_some(Display::Direct)
}

/// Resolve the display policy — `None` is the suite's skip signal.
fn display(headed: bool) -> Option<Display> {
    let have_display = std::env::var_os("DISPLAY").is_some_and(|display| !display.is_empty());
    choose_display(headed, have_display, xvfb_run())
}

/// What [`GuiE2e::spawn`] launches — the `SpawnConfig` twin: program +
/// argv, extra env applied after the pins, headed override.
pub struct GuiSpawnConfig {
    /// The `ait-gui` binary (from [`ait_gui_bin!`](crate::ait_gui_bin)
    /// or `AIT_GUI_E2E_BIN`).
    pub program: OsString,
    /// `argv[1..]`.
    pub args: Vec<OsString>,
    /// Environment pairs applied after the pinned ones — they may
    /// override any pin, including [`ENV_DRIVER_SOCKET`].
    pub env: Vec<(OsString, OsString)>,
    /// Headed mode: drive the developer's `DISPLAY` instead of xvfb
    /// (debugging a scenario visually).
    pub headed: bool,
}

impl GuiSpawnConfig {
    /// A spawn of `program` with no args and the default headless
    /// policy.
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            headed: false,
        }
    }
}

/// One request on the driver wire — the server's enum, serialized
/// identically (`{"verb":"key","chord":"…"}` etc.).
#[derive(Serialize)]
#[serde(tag = "verb", rename_all = "snake_case")]
enum Request<'a> {
    /// `key` — the chord string (`"Ctrl+Shift+E"`), optionally with
    /// the platform text twin (`"twin":true|"å"`).
    Key {
        /// The chord to press and release.
        chord: &'a str,
        /// The `Event::Text` twin emitted between press and release.
        #[serde(skip_serializing_if = "Option::is_none")]
        twin: Option<TwinSpec<'a>>,
    },
    /// `text` — composed text.
    Text {
        /// The text to compose.
        text: &'a str,
    },
    /// `paste` — the OS-clipboard shape.
    Paste {
        /// The clipboard payload.
        text: &'a str,
    },
    /// `scroll` — one wheel gesture.
    Scroll {
        /// The delta's unit.
        unit: ScrollUnit,
        /// The horizontal delta in `unit`s.
        dx: f32,
        /// The vertical delta in `unit`s.
        dy: f32,
        /// An explicit hover position — `x`/`y` travel together.
        #[serde(skip_serializing_if = "Option::is_none")]
        x: Option<f32>,
        /// See `x`.
        #[serde(skip_serializing_if = "Option::is_none")]
        y: Option<f32>,
        /// The region a bare scroll hovers.
        #[serde(skip_serializing_if = "Option::is_none")]
        over: Option<ScrollRegion>,
    },
    /// `resize` — a viewport inner-size request in points.
    Resize {
        /// The requested width.
        width: f32,
        /// The requested height.
        height: f32,
    },
    /// `dump` — the versioned snapshot.
    Dump,
    /// `quit` — the app's normal close.
    Quit,
}

/// `key`'s `twin` on the wire — `true` (derive the chord's character)
/// or the literal payload.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(untagged)]
pub enum TwinSpec<'a> {
    /// `true`/`false`.
    Flag(bool),
    /// The literal `Text` payload.
    Text(&'a str),
}

/// `scroll`'s `unit` — egui's `MouseWheelUnit` spelled the winit way.
/// The client and server spell it identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollUnit {
    /// Whole lines — winit's `LineDelta`.
    #[serde(alias = "line_delta")]
    Line,
    /// Physical pixels — winit's `PixelDelta` (sub-line touchpad
    /// deltas).
    #[serde(alias = "pixel_delta")]
    Pixel,
    /// Whole pages.
    Page,
}

/// `scroll`'s `over` — the named region an explicit `x`/`y` replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollRegion {
    /// The terminal grid's pane.
    Grid,
    /// The agent panel's conversation region.
    Panel,
}

/// One response on the wire — `ok` + optional `error`/`dump`.
#[derive(Debug, Deserialize)]
struct Response {
    /// `true` on success.
    ok: bool,
    /// The failure text — present iff `!ok`.
    #[serde(default)]
    error: Option<String>,
    /// `dump`'s payload.
    #[serde(default)]
    dump: Option<Dump>,
}

/// The versioned snapshot a `dump` answers — the client's mirror of
/// the server's `Dump` (keep in lockstep; additive fields stay v1).
/// `Serialize` too so the evidence dump re-encodes the raw payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dump {
    /// Protocol version (`1`).
    pub v: u32,
    /// The active tab's grid — `null` only with no tabs.
    pub grid: Option<GridDump>,
    /// The side region's state.
    pub pane: PaneDump,
    /// The find bar while it's open — `null` IS its closed state.
    #[serde(default)]
    pub find: Option<FindDump>,
    /// Whether a suggestion is armed — the `suggesting` status
    /// segment's condition.
    #[serde(default)]
    pub suggesting: bool,
    /// The window's content size in points.
    pub window: WindowDump,
}

/// The active tab's visible grid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridDump {
    /// Cell width.
    pub columns: usize,
    /// Cell height.
    pub lines: usize,
    /// Rows scrolled back into scrollback — the `⤓` pill's paint
    /// condition is exactly `scroll_offset > 0`.
    #[serde(default)]
    pub scroll_offset: usize,
    /// Rows held in scrollback above the viewport.
    #[serde(default)]
    pub scrollback: usize,
    /// Visible rows' text, top→bottom, trailing blanks trimmed.
    pub rows: Vec<String>,
    /// The renderable cursor.
    pub cursor: CursorDump,
}

/// The cursor's viewport position and shape name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorDump {
    /// Viewport row.
    pub line: usize,
    /// Viewport column.
    pub column: usize,
    /// `block`/`hollow_block`/`underline`/`beam`/`hidden`.
    pub shape: String,
}

/// The side region's state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneDump {
    /// What shows: `agent`, `explorer`, `browser`, or `null`.
    pub shown: Option<String>,
    /// The app-global aux selection: `explorer`/`browser`/`null`.
    pub selected: Option<String>,
}

/// The find bar's state while it's open.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindDump {
    /// The query typed so far.
    pub query: String,
    /// The number of hits.
    pub hits: usize,
    /// The 1-based `(current, total)` position — `null` with no hits.
    pub position: Option<(usize, usize)>,
}

/// The window's content area in points.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowDump {
    /// Content width.
    pub width: f32,
    /// Content height.
    pub height: f32,
}

impl Dump {
    /// The visible grid text, rows joined with `\n` — the GUI analog
    /// of [`E2e::screen_text`](crate::E2e::screen_text), empty with no
    /// active tab.
    pub fn grid_text(&self) -> String {
        match &self.grid {
            Some(grid) => grid.rows.join("\n"),
            None => String::new(),
        }
    }

    /// The dump's FNV-1a fingerprint — the action log's `screen=` twin:
    /// same hash family, over the dump's observables (rows, cursor,
    /// pane, size) so a human can see *when* the visible state turned.
    fn hash(&self) -> u64 {
        let mut h = FNV_OFFSET;
        fnv(&mut h, &self.v.to_le_bytes());
        if let Some(grid) = &self.grid {
            fnv(&mut h, &grid.columns.to_le_bytes());
            fnv(&mut h, &grid.lines.to_le_bytes());
            fnv(&mut h, &grid.scroll_offset.to_le_bytes());
            fnv(&mut h, &grid.scrollback.to_le_bytes());
            for row in &grid.rows {
                fnv(&mut h, row.as_bytes());
                fnv(&mut h, b"\n");
            }
            fnv(&mut h, &grid.cursor.line.to_le_bytes());
            fnv(&mut h, &grid.cursor.column.to_le_bytes());
            fnv(&mut h, grid.cursor.shape.as_bytes());
        }
        if let Some(shown) = &self.pane.shown {
            fnv(&mut h, shown.as_bytes());
        }
        if let Some(selected) = &self.pane.selected {
            fnv(&mut h, selected.as_bytes());
        }
        if let Some(find) = &self.find {
            fnv(&mut h, find.query.as_bytes());
            fnv(&mut h, &find.hits.to_le_bytes());
        }
        fnv(&mut h, &[self.suggesting as u8]);
        fnv(&mut h, &self.window.width.to_le_bytes());
        fnv(&mut h, &self.window.height.to_le_bytes());
        h
    }
}

/// One spawned `ait-gui` under a display: keys, text, dumps, bounded
/// waits, and the same failure evidence the PTY harness drops — the
/// last `dump.json` plus `actions.log` under
/// `target/e2e-failures/<test>/`.
pub struct GuiE2e {
    /// The app process — `RefCell` because `try_wait`/`kill` need
    /// `&mut` while every wait on the harness is `&self`.
    child: RefCell<Child>,
    /// The driver socket path ([`ENV_DRIVER_SOCKET`] at spawn).
    socket: PathBuf,
    /// The throwaway cwd/`HOME` — removed on drop.
    cwd: TempDir,
    /// Spawn instant — action-log timestamps.
    started: Instant,
    /// The ordered action log (what was sent, what was waited on).
    log: RefCell<Vec<ActionEntry>>,
    /// The last dump's raw JSON — the `dump.json` evidence file.
    last_dump: RefCell<Option<String>>,
    /// The app's stderr tail — startup diagnostics for the evidence
    /// dump, capped at 64 KiB.
    stderr: Arc<Mutex<Vec<u8>>>,
    /// The exit status once observed — a dead app fails waits fast
    /// instead of polling to timeout.
    exited: RefCell<Option<ExitStatus>>,
}

impl GuiE2e {
    /// Spawn the app under the resolved display and wait for the
    /// driver socket to service a `dump`. `Ok(None)` is the skip
    /// convention — no display was reachable (unset `DISPLAY`, no
    /// `xvfb-run`); the caller returns without failing.
    ///
    /// # Errors
    /// Spawn I/O, a socket that never services (with the app's stderr
    /// tail in the message), or an app that exits during boot.
    pub fn spawn(config: GuiSpawnConfig) -> io::Result<Option<Self>> {
        let headed = config.headed || env_flag(HEADED_ENV);
        let Some(display) = display(headed) else {
            return Ok(None);
        };
        let cwd = TempDir::fresh()?;
        let socket = cwd.path().join("driver.sock");
        let mut env = pinned_env(cwd.path(), &socket)?;
        env.extend(config.env);

        let mut command = match display {
            Display::Direct => {
                let mut command = std::process::Command::new(&config.program);
                command.args(&config.args);
                command
            }
            Display::Xvfb(xvfb) => {
                // `-a` picks a free display number — parallel tests
                // never race for :99. The wayland variables must not
                // reach the child: on a wayland host winit prefers the
                // compositor and dies ("could not find wayland
                // compositor") instead of taking the X11 display the
                // wrapper provides.
                let mut command = std::process::Command::new(xvfb);
                command.arg("-a").arg(&config.program).args(&config.args);
                command
                    .env_remove("WAYLAND_DISPLAY")
                    .env_remove("WAYLAND_SOCKET");
                command
            }
        };
        command
            .envs(env)
            .current_dir(cwd.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;

        // Drain both output pipes into one tail buffer: an un-drained
        // pipe fills and the app wedges mid-boot, and the tail doubles
        // as boot-failure evidence (winit reports on stdout).
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let pipes: Vec<Box<dyn Read + Send>> = [
            child
                .stderr
                .take()
                .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
            child
                .stdout
                .take()
                .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
        ]
        .into_iter()
        .flatten()
        .collect();
        for pipe in pipes {
            let sink = Arc::clone(&stderr);
            std::thread::spawn(move || {
                let mut pipe = pipe;
                let mut chunk = [0u8; 8192];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            let mut buf = sink.lock().unwrap_or_else(|e| e.into_inner());
                            buf.extend_from_slice(&chunk[..n]);
                            const CAP: usize = 64 * 1024;
                            let len = buf.len();
                            if len > CAP * 2 {
                                buf.drain(..len - CAP);
                            }
                        }
                    }
                }
            });
        }

        let e2e = Self {
            child: RefCell::new(child),
            socket,
            cwd,
            started: Instant::now(),
            log: RefCell::new(Vec::new()),
            last_dump: RefCell::new(None),
            stderr,
            exited: RefCell::new(None),
        };

        // Boot window: the socket file's existence precedes the pump
        // thread servicing it — poll `dump` until it answers, the app
        // dies, or the bound expires.
        let deadline = Instant::now() + SPAWN_TIMEOUT;
        loop {
            if e2e.try_dump().is_ok() {
                return Ok(Some(e2e));
            }
            if let Some(status) = e2e.exited() {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!(
                        "ait-gui exited during boot ({status:?})\n--- stderr tail ---\n{}",
                        e2e.stderr_tail()
                    ),
                ));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "ait-gui never serviced its driver socket within {SPAWN_TIMEOUT:?}\n\
                         --- stderr tail ---\n{}",
                        e2e.stderr_tail()
                    ),
                ));
            }
            std::thread::sleep(WAIT_POLL);
        }
    }

    /// One socket round trip — connect, send the line, read the
    /// response line (the server half-closes after its reply, which
    /// terminates the read).
    fn request(&self, request: &Request<'_>) -> Result<Response, String> {
        let fail = |message: String| message;
        let mut conn = UnixStream::connect(&self.socket)
            .map_err(|e| fail(format!("connect {}: {e}", self.socket.display())))?;
        conn.set_read_timeout(Some(REQUEST_TIMEOUT))
            .map_err(|e| fail(format!("read timeout: {e}")))?;
        let mut line =
            serde_json::to_string(request).map_err(|e| fail(format!("encode request: {e}")))?;
        line.push('\n');
        conn.write_all(line.as_bytes())
            .map_err(|e| fail(format!("write request: {e}")))?;
        // Signal request-complete — the server answers after the line.
        let _ = conn.shutdown(Shutdown::Write);
        let mut reply = String::new();
        conn.read_to_string(&mut reply)
            .map_err(|e| fail(format!("read response: {e}")))?;
        serde_json::from_str(reply.trim())
            .map_err(|e| fail(format!("malformed response {e}: {reply:?}")))
    }

    /// `request` + the `ok` contract — a driver-side failure (bad
    /// chord, wedged app) is a test failure with the server's message.
    fn act(&self, request: &Request<'_>) {
        match self.request(request) {
            Ok(response) if response.ok => {}
            Ok(response) => panic!(
                "the driver refused the request: {}",
                response.error.unwrap_or_else(|| "(no message)".into())
            ),
            Err(error) => panic!("the driver request failed: {error}"),
        }
    }

    /// `dump` — retrying transport errors the way a live display
    /// deserves (a frame may sit between drain passes); a refused or
    /// malformed answer still fails.
    fn try_dump(&self) -> Result<Dump, String> {
        match self.request(&Request::Dump) {
            Ok(Response {
                ok: true,
                dump: Some(dump),
                ..
            }) => Ok(dump),
            Ok(response) => Err(response
                .error
                .unwrap_or_else(|| "ok response carried no dump".into())),
            Err(error) => Err(error),
        }
    }

    /// The current state snapshot — panics if the app can't answer.
    pub fn dump(&self) -> Dump {
        match self.try_dump() {
            Ok(dump) => dump,
            Err(error) => panic!("dump failed: {error}"),
        }
    }

    /// `key "Ctrl+Shift+E"` — one press+release through the app's own
    /// input dispatch.
    pub fn key(&self, chord: &str) {
        self.log_action(format!("key({chord:?})"));
        self.act(&Request::Key { chord, twin: None });
    }

    /// `key "Alt+A" twin` — the press+release plus the platform text
    /// twin twin-emitting platforms deliver for `Alt+<letter>` (the
    /// R13 leak shape): press, `Text`, release — the order
    /// egui-winit produces them. The payload derives from the chord's
    /// own character; [`key_twin_text`](Self::key_twin_text) gives it
    /// explicitly.
    pub fn key_twin(&self, chord: &str) {
        self.log_action(format!("key({chord:?}, twin=derive)"));
        self.act(&Request::Key {
            chord,
            twin: Some(TwinSpec::Flag(true)),
        });
    }

    /// [`key_twin`](Self::key_twin) with a literal payload — a layout
    /// whose `Alt+A` composes `å` passes `"å"`.
    pub fn key_twin_text(&self, chord: &str, twin: &str) {
        self.log_action(format!("key({chord:?}, twin={twin:?})"));
        self.act(&Request::Key {
            chord,
            twin: Some(TwinSpec::Text(twin)),
        });
    }

    /// `text "…"` — composed text through the same dispatch.
    pub fn text(&self, text: &str) {
        self.log_action(format!("text({:?})", truncate(text, 96)));
        self.act(&Request::Text { text });
    }

    /// `paste "…"` — the OS-clipboard event a Ctrl+V delivers.
    pub fn paste(&self, text: &str) {
        self.log_action(format!("paste({:?})", truncate(text, 96)));
        self.act(&Request::Paste { text });
    }

    /// `scroll unit dx dy` — one wheel gesture over the grid. `unit`
    /// is [`ScrollUnit::Line`], [`ScrollUnit::Pixel`] (sub-line
    /// touchpad deltas) or [`ScrollUnit::Page`]; positive `dy`
    /// scrolls back.
    pub fn scroll(&self, unit: ScrollUnit, dx: f32, dy: f32) {
        self.log_action(format!("scroll({unit:?}, dx={dx}, dy={dy})"));
        self.act(&Request::Scroll {
            unit,
            dx,
            dy,
            x: None,
            y: None,
            over: None,
        });
    }

    /// [`scroll`](Self::scroll) over a named region — the agent
    /// panel's conversation scrolls under `over: "panel"`.
    pub fn scroll_over(&self, unit: ScrollUnit, dx: f32, dy: f32, over: ScrollRegion) {
        self.log_action(format!("scroll({unit:?}, dx={dx}, dy={dy}, over={over:?})"));
        self.act(&Request::Scroll {
            unit,
            dx,
            dy,
            x: None,
            y: None,
            over: Some(over),
        });
    }

    /// [`scroll`](Self::scroll) at an explicit position in points.
    pub fn scroll_at(&self, unit: ScrollUnit, dx: f32, dy: f32, x: f32, y: f32) {
        self.log_action(format!("scroll({unit:?}, dx={dx}, dy={dy}, at={x},{y})"));
        self.act(&Request::Scroll {
            unit,
            dx,
            dy,
            x: Some(x),
            y: Some(y),
            over: None,
        });
    }

    /// `resize w h` — a viewport inner-size request in points, driven
    /// through the real `ViewportCommand::InnerSize` path so the
    /// winsize settle is the production one.
    pub fn resize(&self, width: f32, height: f32) {
        self.log_action(format!("resize({width}x{height})"));
        self.act(&Request::Resize { width, height });
    }

    /// `quit` — the app takes its normal close path; pair with
    /// [`wait_exit`](Self::wait_exit) to reap it.
    pub fn quit(&self) {
        self.log_action("quit()".to_string());
        self.act(&Request::Quit);
    }

    /// Bounded wait until `predicate` accepts a dump — the harness's
    /// only wait shape. The failure message names the description and
    /// embeds the last dump's grid text; a dead app fails immediately.
    ///
    /// # Panics
    /// On timeout or app exit, with the last dump in the message.
    pub fn wait_for_dump(&self, description: &str, predicate: impl Fn(&Dump) -> bool) {
        self.log_action(format!("wait_for_dump({description:?})"));
        let deadline = Instant::now() + WAIT_TIMEOUT;
        loop {
            match self.try_dump() {
                Ok(dump) if predicate(&dump) => {
                    self.log_action(format!("wait_for_dump({description:?}) → satisfied"));
                    return;
                }
                Ok(dump) => self.record_dump(&dump),
                Err(error) => self.log_action(format!("dump poll failed: {error}")),
            }
            if let Some(status) = self.exited() {
                panic!(
                    "waiting for {description}: ait-gui exited ({status:?})\n\
                     --- last dump ---\n{}",
                    self.last_dump_text()
                );
            }
            assert!(
                Instant::now() < deadline,
                "timed out after {WAIT_TIMEOUT:?} waiting for {description}\n\
                 --- last dump ---\n{}",
                self.last_dump_text()
            );
            std::thread::sleep(WAIT_POLL);
        }
    }

    /// Wait for the app to exit (bounded) — after [`quit`](Self::quit).
    ///
    /// # Panics
    /// If the app is still running at the bound.
    pub fn wait_exit(&self) -> ExitStatus {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        loop {
            if let Some(status) = self.exited() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "timed out after {WAIT_TIMEOUT:?} waiting for ait-gui to exit"
            );
            std::thread::sleep(WAIT_POLL);
        }
    }

    /// The app's exit status if it already exited — cached: `try_wait`
    /// is only consulted until it first yields `Some`.
    fn exited(&self) -> Option<ExitStatus> {
        if let Some(status) = *self.exited.borrow() {
            return Some(status);
        }
        match self.child.borrow_mut().try_wait() {
            Ok(Some(status)) => {
                *self.exited.borrow_mut() = Some(status);
                Some(status)
            }
            _ => None,
        }
    }

    /// The throwaway cwd the app was spawned in.
    pub fn cwd(&self) -> &Path {
        self.cwd.path()
    }

    /// One action-log entry with the current dump hash — the same
    /// `#<seq> [t+<ms>] <action> screen=<hash>` line shape `E2e` writes.
    fn log_action(&self, action: String) {
        let screen_hash = self.try_dump().map(|dump| dump.hash()).unwrap_or(0);
        self.log.borrow_mut().push(ActionEntry {
            at: self.started.elapsed(),
            action,
            screen_hash,
        });
    }

    /// Keep the last dump's raw JSON for the evidence file.
    fn record_dump(&self, dump: &Dump) {
        let raw = serde_json::to_string_pretty(dump).unwrap_or_default();
        *self.last_dump.borrow_mut() = Some(raw);
    }

    /// The last dump's text for a failure message — pretty JSON, or a
    /// marker when no dump was ever answered.
    fn last_dump_text(&self) -> String {
        match self.last_dump.borrow().as_ref() {
            Some(raw) => raw.clone(),
            None => "(no dump was ever answered)".to_string(),
        }
    }

    /// The app's stderr tail — utf8-lossy.
    fn stderr_tail(&self) -> String {
        let buf = self.stderr.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Write `dump.json` + `actions.log` + `stderr.txt` under
    /// `target/e2e-failures/<test_name>/` — the GUI evidence set.
    pub fn dump_evidence(&self, test_name: &str) -> io::Result<PathBuf> {
        evidence::write_gui_dump(
            test_name,
            &self.last_dump_text(),
            &self.stderr_tail(),
            &self.log.borrow(),
        )
    }
}

impl Drop for GuiE2e {
    /// A panic with a live `GuiE2e` drops evidence the harness way —
    /// `dump.json` + the action log under the test's name — then reaps
    /// the app so no window leaks past the suite.
    fn drop(&mut self) {
        if std::thread::panicking() {
            let name = evidence::current_test_name();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = self.dump_evidence(&name);
            }));
        }
        let _ = self.child.borrow_mut().kill();
        let _ = self.child.borrow_mut().wait();
    }
}

/// A long wait string or text payload, truncated for the action log.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max).collect();
    let _ = write!(cut, "…(+{} chars)", text.chars().count() - max);
    cut
}

// ------------------------------------------------------------------
// Scenarios — TOML steps driving a `GuiE2e` the way `Scenario` drives
// an `E2e`.
// ------------------------------------------------------------------

/// The GUI scenario verbs — the driver protocol's plus the bounded
/// assertions. Exactly one per `[[steps]]` table.
const GUI_VERBS: &[&str] = &[
    "key",
    "text",
    "paste",
    "scroll",
    "resize",
    "wait_text",
    "expect_text",
    "expect_pane",
    "expect_find",
    "expect_scroll",
    "expect_suggesting",
    "quit",
];

/// A declarative GUI scenario (Spec R15): `feature`, `env`, ordered
/// steps — parsed by [`GuiScenario::load`], run by
/// [`GuiScenario::run`].
///
/// ```toml
/// feature = "panes"
///
/// [[steps]]
/// key = "Ctrl+Shift+E"               # one chord through the app's dispatch
///
/// [[steps]]
/// key = { chord = "Alt+A", twin = true }   # the platform text twin
///
/// [[steps]]
/// scroll = { unit = "pixel", dy = 3.0 }    # a sub-line wheel flick
///
/// [[steps]]
/// expect_pane = "explorer"           # bounded wait on pane.shown
///
/// [[steps]]
/// expect_text = "files"              # bounded wait on grid text
///
/// [[steps]]
/// quit = true                        # the normal close path
/// ```
#[derive(Debug)]
pub struct GuiScenario {
    /// The `feature` tag.
    pub feature: String,
    /// Extra env pairs applied after the pins.
    pub env: Vec<(OsString, OsString)>,
    /// The ordered steps.
    pub steps: Vec<GuiStep>,
    /// `headed = true` — drive the developer's `DISPLAY`.
    headed: bool,
    /// The file stem — failure messages and dump dirs.
    name: String,
    /// The path it was loaded from.
    source: String,
}

/// One GUI `[[steps]]` entry.
#[derive(Debug)]
pub enum GuiStep {
    /// `key` — one chord (`"Ctrl+Shift+E"`), or a table carrying the
    /// chord plus its platform text twin
    /// (`key = { chord = "Alt+A", twin = true }`).
    Key {
        /// The chord to press and release.
        chord: String,
        /// The text twin — `Derived` asks the app for the chord's own
        /// character, `Text` is the literal payload.
        twin: Option<KeyTwin>,
    },
    /// `text` — composed text.
    Text(String),
    /// `paste` — the OS-clipboard event.
    Paste(String),
    /// `scroll` — one wheel gesture; the table's `unit` is required,
    /// `dx`/`dy` default to `0`, `x`/`y` give an explicit position,
    /// `over` (`grid`/`panel`) the named region.
    Scroll {
        /// The delta's unit (`line`/`pixel`/`page`).
        unit: ScrollUnit,
        /// The horizontal delta.
        dx: f32,
        /// The vertical delta — positive scrolls back.
        dy: f32,
        /// An explicit hover position in points.
        x: Option<f32>,
        /// See `x`.
        y: Option<f32>,
        /// The named hover region.
        over: Option<ScrollRegion>,
    },
    /// `resize` — a viewport inner-size request in points.
    Resize {
        /// The requested width.
        width: f32,
        /// The requested height.
        height: f32,
    },
    /// `wait_text` — gate until the grid text contains it.
    WaitText(String),
    /// `expect_text` — assert the grid text contains it.
    ExpectText(String),
    /// `expect_pane` — assert the shown pane's name (`explorer`,
    /// `browser`, `agent`) or `none` for a closed region — the
    /// user-visible pane selection.
    ExpectPane(String),
    /// `expect_find` — `true`/`false` asserts the find bar open or
    /// closed; a string asserts the open bar's query contains it.
    ExpectFind(FindExpect),
    /// `expect_scroll` — assert the view scrolled back at least this
    /// many rows (`grid.scroll_offset >= n` — the `⤓` pill's paint
    /// condition is `> 0`).
    ExpectScroll(usize),
    /// `expect_suggesting` — assert the suggestion arm's state.
    ExpectSuggesting(bool),
    /// `quit` — the app's normal close.
    Quit,
}

/// `key`'s `twin` — how the platform text twin's payload resolves.
#[derive(Debug, Clone)]
pub enum KeyTwin {
    /// `twin = true` — the chord's own character (`Alt+A` → `"a"`).
    Derived,
    /// `twin = "å"` — the literal payload.
    Text(String),
}

/// What `expect_find` asserts about the find bar.
#[derive(Debug)]
pub enum FindExpect {
    /// `expect_find = true` — the bar is open.
    Open,
    /// `expect_find = false` — the bar is closed.
    Closed,
    /// `expect_find = "…"` — the bar is open with this in its query.
    Query(String),
}

impl GuiStep {
    /// The verb's TOML key, for failure messages.
    fn verb(&self) -> &'static str {
        match self {
            Self::Key { .. } => "key",
            Self::Text(_) => "text",
            Self::Paste(_) => "paste",
            Self::Scroll { .. } => "scroll",
            Self::Resize { .. } => "resize",
            Self::WaitText(_) => "wait_text",
            Self::ExpectText(_) => "expect_text",
            Self::ExpectPane(_) => "expect_pane",
            Self::ExpectFind(_) => "expect_find",
            Self::ExpectScroll(_) => "expect_scroll",
            Self::ExpectSuggesting(_) => "expect_suggesting",
            Self::Quit => "quit",
        }
    }

    /// Execute against `e2e`.
    fn run(&self, e2e: &GuiE2e) {
        match self {
            Self::Key { chord, twin } => match twin {
                None => e2e.key(chord),
                Some(KeyTwin::Derived) => e2e.key_twin(chord),
                Some(KeyTwin::Text(twin)) => e2e.key_twin_text(chord, twin),
            },
            Self::Text(text) => e2e.text(text),
            Self::Paste(text) => e2e.paste(text),
            Self::Scroll {
                unit,
                dx,
                dy,
                x,
                y,
                over,
            } => match (x, y, over) {
                (Some(x), Some(y), _) => e2e.scroll_at(*unit, *dx, *dy, *x, *y),
                (_, _, Some(over)) => e2e.scroll_over(*unit, *dx, *dy, *over),
                _ => e2e.scroll(*unit, *dx, *dy),
            },
            Self::Resize { width, height } => e2e.resize(*width, *height),
            Self::WaitText(text) => {
                e2e.wait_for_dump(&format!("wait_text {text:?}"), |dump| {
                    dump.grid_text().contains(text.as_str())
                });
            }
            Self::ExpectText(text) => {
                e2e.wait_for_dump(&format!("expect_text {text:?}"), |dump| {
                    dump.grid_text().contains(text.as_str())
                });
            }
            Self::ExpectPane(pane) => {
                e2e.wait_for_dump(&format!("expect_pane {pane:?}"), |dump| {
                    match pane.as_str() {
                        "none" => dump.pane.shown.is_none(),
                        name => dump.pane.shown.as_deref() == Some(name),
                    }
                });
            }
            Self::ExpectFind(expect) => {
                e2e.wait_for_dump(&format!("expect_find {expect:?}"), |dump| {
                    match expect {
                        FindExpect::Open => dump.find.is_some(),
                        FindExpect::Closed => dump.find.is_none(),
                        FindExpect::Query(query) => dump
                            .find
                            .as_ref()
                            .is_some_and(|find| find.query.contains(query.as_str())),
                    }
                });
            }
            Self::ExpectScroll(lines) => {
                e2e.wait_for_dump(&format!("expect_scroll {lines}"), |dump| {
                    dump.grid
                        .as_ref()
                        .is_some_and(|grid| grid.scroll_offset >= *lines)
                });
            }
            Self::ExpectSuggesting(want) => {
                e2e.wait_for_dump(&format!("expect_suggesting {want}"), |dump| {
                    dump.suggesting == *want
                });
            }
            Self::Quit => e2e.quit(),
        }
    }
}

/// The file's top level, parsed loosely like `RawScenario` so errors
/// name the step.
#[derive(Deserialize)]
struct RawGuiScenario {
    /// Required — the tag.
    feature: String,
    /// `env = { KEY = "value" }` applied after the pins.
    #[serde(default)]
    env: toml::Table,
    /// `headed = true` — drive the developer's `DISPLAY`.
    #[serde(default)]
    headed: bool,
    /// The `[[steps]]` tables.
    #[serde(default)]
    steps: Vec<toml::Table>,
}

impl GuiScenario {
    /// Read and validate the scenario at `path`. Every error names the
    /// file — and the step index when the problem sits inside a step.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ScenarioError> {
        let path = path.as_ref();
        let source = path.display().to_string();
        let text = std::fs::read_to_string(path)
            .map_err(|e| ScenarioError::file(&source, format!("could not read: {e}")))?;
        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "scenario".to_string());
        Self::parse(name, &source, &text)
    }

    /// Parse `text` as the scenario named `name` from `source`.
    fn parse(name: String, source: &str, text: &str) -> Result<Self, ScenarioError> {
        let raw: RawGuiScenario = toml::from_str(text)
            .map_err(|e| ScenarioError::file(source, format!("invalid TOML: {e}")))?;
        let env = raw
            .env
            .into_iter()
            .map(|(key, value)| match value {
                toml::Value::String(value) => Ok((OsString::from(key), OsString::from(value))),
                other => Err(ScenarioError::file(
                    source,
                    format!("env.{key} must be a string, got {other}"),
                )),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let steps = raw
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| parse_gui_step(source, index, step))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            feature: raw.feature,
            env,
            steps,
            headed: raw.headed,
            name,
            source: source.to_string(),
        })
    }

    /// A [`GuiSpawnConfig`] honoring the scenario's `env` and `headed`.
    pub fn spawn_config(&self, program: impl Into<OsString>) -> GuiSpawnConfig {
        let mut config = GuiSpawnConfig::new(program);
        config.env = self.env.clone();
        config.headed = self.headed;
        config
    }

    /// Execute every step in order — a step's own panic is re-raised
    /// naming the scenario and step, the [`Scenario::run`] convention.
    ///
    /// # Panics
    /// On the first failing step, with context.
    pub fn run(&self, e2e: &GuiE2e) {
        for (index, step) in self.steps.iter().enumerate() {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| step.run(e2e)));
            if let Err(payload) = outcome {
                panic!(
                    "{} step {index} ({}) failed: {}",
                    self.name,
                    step.verb(),
                    evidence::panic_text(payload.as_ref()),
                );
            }
        }
    }

    /// The scenario's name (its file stem).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The path it was loaded from.
    pub fn source(&self) -> &str {
        &self.source
    }
}

/// Reuse [`crate::ScenarioError`] — the diagnostic shape is identical.
pub use crate::ScenarioError;

/// Turn one raw `[[steps]]` table into a typed [`GuiStep`] — the
/// `parse_step` twin: unknown keys, exactly-one-verb, typed values.
fn parse_gui_step(
    source: &str,
    index: usize,
    table: &toml::Table,
) -> Result<GuiStep, ScenarioError> {
    let fail = |message: String| ScenarioError::step(source, index, message);

    let unknown: Vec<&String> = table
        .keys()
        .filter(|key| !GUI_VERBS.contains(&key.as_str()))
        .collect();
    if !unknown.is_empty() {
        let names: Vec<String> = unknown.iter().map(|key| format!("`{key}`")).collect();
        return Err(fail(format!(
            "unknown verb(s) {}; known verbs: {}",
            names.join(", "),
            GUI_VERBS.join(", ")
        )));
    }
    let verbs: Vec<&String> = table
        .keys()
        .filter(|key| GUI_VERBS.contains(&key.as_str()))
        .collect();
    if verbs.len() != 1 {
        let found = if verbs.is_empty() {
            "none".to_string()
        } else {
            verbs
                .iter()
                .map(|verb| format!("`{verb}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(fail(format!(
            "a step carries exactly one verb, found {found}"
        )));
    }
    let verb = verbs[0].as_str();
    let value = &table[verb];

    /// Deserialize one value, re-wording toml's error to name the verb.
    fn want<T: for<'de> Deserialize<'de>>(
        value: &toml::Value,
        verb: &str,
        what: &str,
    ) -> Result<T, String> {
        T::deserialize(value.clone()).map_err(|e| format!("`{verb}` must be {what}: {e}"))
    }

    match verb {
        "key" => match value {
            // `key = "Ctrl+Shift+E"` — a bare chord.
            toml::Value::String(chord) => Ok(GuiStep::Key {
                chord: chord.clone(),
                twin: None,
            }),
            // `key = { chord = "Alt+A", twin = true }` — the platform
            // text twin: `true` derives the chord's character, a
            // string is the payload.
            toml::Value::Table(_) => {
                let table: KeyTable =
                    want(value, verb, "a { chord, twin } table").map_err(fail)?;
                let twin = table.twin.and_then(|twin| match twin {
                    TwinToml::Flag(true) => Some(KeyTwin::Derived),
                    // `twin = false` spelled out means no twin.
                    TwinToml::Flag(false) => None,
                    TwinToml::Text(text) => Some(KeyTwin::Text(text)),
                });
                Ok(GuiStep::Key {
                    chord: table.chord,
                    twin,
                })
            }
            _ => Err(fail(format!(
                "`key` must be a chord string or a {{ chord, twin }} table, got {value}"
            ))),
        },
        "text" => Ok(GuiStep::Text(want(value, verb, "a string").map_err(fail)?)),
        "paste" => Ok(GuiStep::Paste(
            want(value, verb, "a string").map_err(fail)?,
        )),
        "scroll" => {
            let table: ScrollTable = want(value, verb, "a { unit, dx?, dy?, x?, y?, over? } table")
                .map_err(fail)?;
            if table.x.is_some() != table.y.is_some() {
                return Err(fail(
                    "`scroll`'s `x` and `y` travel together or not at all".to_string(),
                ));
            }
            Ok(GuiStep::Scroll {
                unit: table.unit,
                dx: table.dx,
                dy: table.dy,
                x: table.x,
                y: table.y,
                over: table.over,
            })
        }
        "resize" => {
            let table: ResizeTable = want(value, verb, "a { width, height } table")
                .map_err(fail)?;
            Ok(GuiStep::Resize {
                width: table.width,
                height: table.height,
            })
        }
        "wait_text" => Ok(GuiStep::WaitText(
            want(value, verb, "a string").map_err(fail)?,
        )),
        "expect_text" => Ok(GuiStep::ExpectText(
            want(value, verb, "a string").map_err(fail)?,
        )),
        "expect_pane" => Ok(GuiStep::ExpectPane(
            want(value, verb, "a pane name").map_err(fail)?,
        )),
        "expect_find" => match value {
            toml::Value::Boolean(want) => Ok(GuiStep::ExpectFind(if *want {
                FindExpect::Open
            } else {
                FindExpect::Closed
            })),
            toml::Value::String(query) => {
                Ok(GuiStep::ExpectFind(FindExpect::Query(query.clone())))
            }
            _ => Err(fail(format!(
                "`expect_find` must be `true`, `false` or a query string, got {value}"
            ))),
        },
        "expect_scroll" => Ok(GuiStep::ExpectScroll(
            want(value, verb, "a row count").map_err(fail)?,
        )),
        "expect_suggesting" => Ok(GuiStep::ExpectSuggesting(
            want(value, verb, "`true` or `false`").map_err(fail)?,
        )),
        "quit" => {
            let flag: bool = want(value, verb, "`true`").map_err(fail)?;
            if !flag {
                return Err(fail("`quit` must be `true`".to_string()));
            }
            Ok(GuiStep::Quit)
        }
        other => unreachable!("{other} passed the verb check"),
    }
}

/// `key`'s table form — `{ chord = "…", twin = true | "…" }`.
#[derive(Deserialize)]
struct KeyTable {
    /// The chord to press and release.
    chord: String,
    /// The platform text twin.
    #[serde(default)]
    twin: Option<TwinToml>,
}

/// `twin`'s shapes in the TOML table.
#[derive(Deserialize)]
#[serde(untagged)]
enum TwinToml {
    /// `true`/`false`.
    Flag(bool),
    /// The literal payload.
    Text(String),
}

/// `scroll`'s table form — `unit` required, the rest defaulted.
#[derive(Deserialize)]
struct ScrollTable {
    /// The delta's unit.
    unit: ScrollUnit,
    /// The horizontal delta.
    #[serde(default)]
    dx: f32,
    /// The vertical delta.
    #[serde(default)]
    dy: f32,
    /// An explicit hover position.
    #[serde(default)]
    x: Option<f32>,
    /// See `x`.
    #[serde(default)]
    y: Option<f32>,
    /// The named hover region.
    #[serde(default)]
    over: Option<ScrollRegion>,
}

/// `resize`'s table form.
#[derive(Deserialize)]
struct ResizeTable {
    /// The requested width in points.
    width: f32,
    /// The requested height in points.
    height: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The codec twin of the server's request type — the wire shapes
    /// must stay byte-identical (they're the protocol).
    #[test]
    fn requests_encode_the_wire_shapes() {
        assert_eq!(
            serde_json::to_string(&Request::Key {
                chord: "Ctrl+Shift+E",
                twin: None,
            })
            .unwrap(),
            r#"{"verb":"key","chord":"Ctrl+Shift+E"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Key {
                chord: "Alt+A",
                twin: Some(TwinSpec::Flag(true)),
            })
            .unwrap(),
            r#"{"verb":"key","chord":"Alt+A","twin":true}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Key {
                chord: "Alt+A",
                twin: Some(TwinSpec::Text("å")),
            })
            .unwrap(),
            r#"{"verb":"key","chord":"Alt+A","twin":"å"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Text { text: "hi" }).unwrap(),
            r#"{"verb":"text","text":"hi"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Paste { text: "hi" }).unwrap(),
            r#"{"verb":"paste","text":"hi"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Scroll {
                unit: ScrollUnit::Pixel,
                dx: 0.0,
                dy: 3.0,
                x: None,
                y: None,
                over: None,
            })
            .unwrap(),
            r#"{"verb":"scroll","unit":"pixel","dx":0.0,"dy":3.0}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Scroll {
                unit: ScrollUnit::Line,
                dx: 0.0,
                dy: 1.0,
                x: None,
                y: None,
                over: Some(ScrollRegion::Panel),
            })
            .unwrap(),
            r#"{"verb":"scroll","unit":"line","dx":0.0,"dy":1.0,"over":"panel"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Resize {
                width: 1100.0,
                height: 700.0,
            })
            .unwrap(),
            r#"{"verb":"resize","width":1100.0,"height":700.0}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Dump).unwrap(),
            r#"{"verb":"dump"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Quit).unwrap(),
            r#"{"verb":"quit"}"#
        );
    }

    /// The dump client decodes the server's payload.
    #[test]
    fn dump_decodes_the_server_shape() {
        let line = r#"{"ok":true,"dump":{"v":1,"grid":{"columns":80,"lines":24,"scroll_offset":2,"scrollback":100,"rows":["a","","c"],"cursor":{"line":1,"column":2,"shape":"block"}},"pane":{"shown":"explorer","selected":"explorer"},"find":{"query":"nd","hits":3,"position":[1,3]},"suggesting":false,"window":{"width":640.0,"height":480.0}}}"#;
        let response: Response = serde_json::from_str(line).unwrap();
        assert!(response.ok);
        let dump = response.dump.unwrap();
        assert_eq!(dump.v, 1);
        assert_eq!(dump.pane.shown.as_deref(), Some("explorer"));
        let grid = dump.grid.as_ref().unwrap();
        assert_eq!(grid.cursor.shape, "block");
        assert_eq!(grid.scroll_offset, 2);
        assert_eq!(grid.scrollback, 100);
        let find = dump.find.as_ref().unwrap();
        assert_eq!(find.query, "nd");
        assert_eq!(find.position, Some((1, 3)));
        assert!(!dump.suggesting);
        assert_eq!(dump.grid_text(), "a\n\nc");
        let dump = Dump {
            v: 1,
            grid: Some(GridDump {
                columns: 80,
                lines: 24,
                scroll_offset: 0,
                scrollback: 0,
                rows: vec!["a".into(), "".into(), "c".into()],
                cursor: CursorDump {
                    line: 0,
                    column: 0,
                    shape: "block".into(),
                },
            }),
            pane: PaneDump {
                shown: None,
                selected: None,
            },
            find: None,
            suggesting: false,
            window: WindowDump {
                width: 0.0,
                height: 0.0,
            },
        };
        assert_eq!(dump.grid_text(), "a\n\nc");
    }

    /// Scenario parsing: verbs type-check, unknown keys and
    /// double-verbs fail at load.
    #[test]
    fn gui_scenarios_parse_and_validate() {
        let scenario = GuiScenario::parse(
            "demo".into(),
            "demo.toml",
            r#"
feature = "panes"
env = { FOO = "bar" }
[[steps]]
key = "Ctrl+Shift+E"
[[steps]]
key = { chord = "Alt+A", twin = true }
[[steps]]
paste = "echo hi"
[[steps]]
scroll = { unit = "pixel", dy = 3.0 }
[[steps]]
resize = { width = 1100, height = 700 }
[[steps]]
expect_pane = "explorer"
[[steps]]
expect_find = "needle"
[[steps]]
expect_scroll = 1
[[steps]]
expect_suggesting = false
[[steps]]
quit = true
"#,
        )
        .unwrap();
        assert_eq!(scenario.feature, "panes");
        assert_eq!(scenario.steps.len(), 10);

        for (text, want) in [
            (r#"feature = "x""#, "missing steps is fine"),
            ("[[steps]]\nkey = 1", "typed values"),
            ("[[steps]]\nkey = { chord = \"a\", twin = 1 }", "typed twin"),
            ("[[steps]]\nscroll = { dy = 1 }", "scroll wants a unit"),
            ("[[steps]]\nscroll = { unit = \"line\", x = 1 }", "x wants y"),
            ("[[steps]]\nexpect_find = 1", "typed find"),
            ("[[steps]]\nbogus = 1", "unknown verb"),
            ("[[steps]]\nkey = \"a\"\ntext = \"b\"", "two verbs"),
        ] {
            let outcome = GuiScenario::parse("t".into(), "t.toml", text);
            if want == "missing steps is fine" {
                assert!(outcome.is_ok(), "{want}: {outcome:?}");
            } else {
                assert!(outcome.is_err(), "{want}: {text}");
            }
        }
    }

    /// The skip convention lives in `choose_display`: no DISPLAY and
    /// no xvfb-run yields `None`; headed requires DISPLAY; headless
    /// prefers the owned xvfb display.
    #[test]
    fn display_policy_skips_without_a_display() {
        let xvfb = Some(PathBuf::from("/usr/bin/xvfb-run"));
        assert!(choose_display(false, false, None).is_none());
        assert!(choose_display(true, false, xvfb.clone()).is_none());
        assert!(matches!(
            choose_display(true, true, None),
            Some(Display::Direct)
        ));
        assert!(matches!(
            choose_display(false, true, xvfb.clone()),
            Some(Display::Xvfb(_))
        ));
        assert!(matches!(
            choose_display(false, true, None),
            Some(Display::Direct)
        ));
        assert!(matches!(
            choose_display(false, false, xvfb),
            Some(Display::Xvfb(_))
        ));
    }
}
