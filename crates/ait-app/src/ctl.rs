//! The `ait ctl` control surface (REQUIREMENTS.md §5.10.5, Spec R11 /
//! issue #143): a running `ait` listens on a per-instance local
//! socket — the **control socket** — and `ait ctl <verb>` connects to
//! drive it.
//!
//! Three pieces live here so `app` stays presentation-only
//! (CODING_STANDARDS §9):
//!
//! - the **codec** — [`Request`]/[`Response`] as single-line JSON
//!   (`serde`), one request per connection, one response back;
//! - [`dispatch`] — the single seam over `&mut TabManager`: every verb
//!   maps to in-loop, non-blocking tab operations (`open_tab`,
//!   `send`, `last_block`);
//! - [`CtlServer`] — a nonblocking local-socket listener, drained on
//!   the app tick over the [`crate::ipc`] transport (Spec R16 §5.14):
//!   `${XDG_RUNTIME_DIR}/ait/<pid>.sock` on Unix (`$TMPDIR/ait-<uid>`
//!   fallback, the shared `ait_shell::ait_runtime_dir` rule — parent
//!   dir `0700`, socket `0600`, unlinked on clean exit and re-bound
//!   over a stale file on start), `\\.\pipe\ait-<user>-<pid>` on
//!   Windows under the default same-user ACLs. Accepted connections
//!   buffer across app ticks, so a request line can never wedge the
//!   UI;
//!
//! Spawned sessions get [`ENV_CTL_SOCKET`] exported through the same
//! `options.env` seam as `AIT_SESSION_ID` (in `TabManager::spawn_tab`,
//! so plain uninjected shells get it too — the verbs need no
//! integration), so `ait ctl` run inside a tab drives its own
//! instance.

use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::ffi::OsStr;

use serde::{Deserialize, Serialize};

use crate::ipc::{self, LocalSocketListener, Pending, Service};
use crate::tabs::{Tab, TabManager};

/// The env var naming the running instance's control socket —
/// exported into every spawned session beside `AIT_SESSION_ID` and
/// read first by `ait ctl` (CODING_STANDARDS §17: both sides name it
/// here, once).
pub const ENV_CTL_SOCKET: &str = "AIT_CTL_SOCKET";

/// One request on the wire — a single line of JSON, internally tagged
/// on `"verb"` so unknown verbs fail decode with the variant named.
///
/// ```json
/// {"verb":"open_tab"}
/// {"verb":"send","text":"echo hi","enter":true}
/// {"verb":"send","tab":"2","text":"echo hi","enter":false}
/// {"verb":"last_block","tab":"2"}
/// ```
///
/// `tab` is the target tab's `AIT_SESSION_ID` (the stable `Tab::id`
/// as text); absent means the active tab. `enter: false` is `send
/// --no-enter` — the text lands on the input line verbatim, with no
/// `\r` appended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "snake_case")]
pub enum Request {
    /// Allocate a fresh tab in the running instance; the response
    /// carries its new `AIT_SESSION_ID`.
    OpenTab,
    /// Write `text` into the target tab's PTY — plus `\r` unless
    /// `enter` is `false` — and mark the tab's `InputShadow` stale so
    /// scripted input can never arm the `# `/classifier triggers
    /// (us-16/44: ctl bytes are not keystrokes).
    Send {
        /// `--tab <id>`; `None` is the active tab.
        #[serde(default)]
        tab: Option<String>,
        /// The verbatim input bytes.
        text: String,
        /// Append `\r` — `true` when the client leaves `--no-enter`
        /// off.
        #[serde(default = "send_enter")]
        enter: bool,
    },
    /// The newest *sealed* block's `{command, output, exit}` readout —
    /// an in-flight command's partial output is intentionally
    /// unavailable (us-37/39).
    LastBlock {
        /// `--tab <id>`; `None` is the active tab.
        #[serde(default)]
        tab: Option<String>,
    },
}

/// `send`'s `enter` default — off the wire the flag means `--no-enter`.
fn send_enter() -> bool {
    true
}

/// One sealed block's read-back payload — the `last_block` response
/// body and the source of the client's `--command`/`--exit` selectors
/// (no flag prints `output`). `exit` is the `OSC 133;D` code: `null`
/// when `D` carried none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockReadout {
    /// The command text — `preexec`-accurate when integrated.
    pub command: String,
    /// The block's output rows (the `C` mark through `D`), terminal
    /// copy semantics — never a trailing newline.
    pub output: String,
    /// The `OSC 133;D` exit code, `null` when the mark carried none.
    pub exit: Option<u32>,
}

/// One response on the wire — a single line of JSON. Success is
/// `{"ok":true,…}` with the verb's fields; failure is
/// `{"ok":false,"error":"…"}` with a human-readable message the
/// client relays to stderr.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    /// `true` on success.
    pub ok: bool,
    /// The failure message — present iff `ok` is `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `open_tab`'s answer: the new tab's `AIT_SESSION_ID`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// `last_block`'s answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<BlockReadout>,
}

impl Response {
    /// `{"ok":true}` — a bare success.
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            session_id: None,
            block: None,
        }
    }

    /// `{"ok":true,"session_id":"…"}` — `open_tab`'s answer.
    pub fn opened(session_id: String) -> Self {
        Self {
            session_id: Some(session_id),
            ..Self::ok()
        }
    }

    /// `{"ok":true,"block":{…}}` — `last_block`'s answer.
    pub fn block(block: BlockReadout) -> Self {
        Self {
            block: Some(block),
            ..Self::ok()
        }
    }

    /// `{"ok":false,"error":"…"}` — a failure the client exits
    /// non-zero on, relaying `error` to stderr.
    pub fn error(message: String) -> Self {
        Self {
            ok: false,
            error: Some(message),
            session_id: None,
            block: None,
        }
    }
}

/// Decode one request line. Trailing whitespace is trimmed (the line
/// is already split off the byte stream); anything that isn't one
/// [`Request`] line is an error naming the failure — the server
/// relays it as an `{"ok":false}` response rather than dropping the
/// connection silently.
pub fn decode_request(line: &str) -> Result<Request, String> {
    serde_json::from_str(line.trim()).map_err(|e| format!("malformed request: {e}"))
}

/// Encode one request to its single-line JSON form.
pub fn encode_request(request: &Request) -> String {
    serde_json::to_string(request).expect("Request serialization is infallible")
}

/// Decode one response line — the client's half of the codec.
pub fn decode_response(line: &str) -> Result<Response, String> {
    serde_json::from_str(line.trim()).map_err(|e| format!("malformed response: {e}"))
}

/// Encode one response to its single-line JSON form.
pub fn encode_response(response: &Response) -> String {
    serde_json::to_string(response).expect("Response serialization is infallible")
}

/// The dispatch seam (Spec R11 §5.10.5): every verb maps to in-loop,
/// non-blocking operations on `tabs` — `open_tab` spawns (and the
/// spawned tab becomes active), `send` writes bytes + `\r` — verbatim
/// under `enter: false` — into the target's PTY and marks its
/// `InputShadow` stale so scripted input can never arm the
/// `# `/classifier triggers, `last_block` reads the newest *sealed*
/// block off the target. Errors are `{"ok":false}` responses naming
/// the problem — this function never fails open and never panics on
/// an empty tab set.
pub fn dispatch(tabs: &mut TabManager, request: &Request) -> Response {
    match request {
        Request::OpenTab => match tabs.new_tab() {
            Ok(()) => {
                // `spawn_tab` activates what it appended — a
                // successful `new_tab` can never leave the set empty.
                let id = tabs
                    .active()
                    .map(|tab| tab.id.to_string())
                    .unwrap_or_default();
                Response::opened(id)
            }
            Err(error) => Response::error(format!("open-tab failed: {error}")),
        },
        Request::Send { tab, text, enter } => {
            let named = tab.is_some();
            let Some(tab) = target_tab_mut(tabs, tab.as_deref()) else {
                return tab_error(tab.as_deref(), "unknown tab");
            };
            if tab.exit_status().is_some() {
                return Response::error(which_tab(named, tab, "has exited"));
            }
            let mut bytes = text.clone().into_bytes();
            if *enter {
                bytes.push(b'\r');
            }
            if let Err(error) = tab.session.send_bytes(&bytes) {
                return Response::error(format!("write to tab failed: {error}"));
            }
            // ctl bytes are not keystrokes — a half-shadowed line must
            // degrade to an ordinary submission, never a wrong
            // `# `/classifier trigger (us-16/44).
            tab.shadow.mark_stale();
            Response::ok()
        }
        Request::LastBlock { tab } => {
            let named = tab.is_some();
            let Some(tab) = target_tab(tabs, tab.as_deref()) else {
                return tab_error(tab.as_deref(), "unknown tab");
            };
            let Some(block) = tab.blocks().into_iter().rev().find(|b| b.completed()) else {
                return Response::error(which_tab(named, tab, "has no sealed blocks"));
            };
            Response::block(BlockReadout {
                command: block.command().to_owned(),
                output: tab.block_output(&block),
                exit: block.exit(),
            })
        }
    }
}

/// The `--tab`-or-active resolution, immutable — `last_block`'s half.
fn target_tab<'m>(tabs: &'m TabManager, id: Option<&str>) -> Option<&'m Tab> {
    match id {
        None => tabs.active(),
        Some(id) => tabs.session_tab(id),
    }
}

/// The `--tab`-or-active resolution, mutable — `send`'s half.
fn target_tab_mut<'m>(tabs: &'m mut TabManager, id: Option<&str>) -> Option<&'m mut Tab> {
    match id {
        None => tabs.active_mut(),
        Some(id) => tabs.session_tab_mut(id),
    }
}

/// The shared "could not resolve the target" response — `unknown tab
/// "9"` names the id the caller passed; a missing active tab is the
/// empty-set edge.
fn tab_error(id: Option<&str>, what: &str) -> Response {
    match id {
        Some(id) => Response::error(format!("{what} {id:?}")),
        None => Response::error("no tabs".to_owned()),
    }
}

/// A per-target error naming the resolved tab — `tab "3" has
/// exited`/`the active tab has no sealed blocks`.
fn which_tab(named: bool, tab: &Tab, what: &str) -> String {
    if named {
        format!("tab {:?} {what}", tab.id.to_string())
    } else {
        format!("the active tab {what}")
    }
}

// ------------------------------------------------------------------
// Server: the per-instance control socket drained on the app tick.
// The transport half — [`Pending`]'s buffered request/flush
// lifecycle, the accept loop, the listener bind — lives in
// [`crate::ipc`] (Spec R16 §5.14), shared verbatim with `ait-gui`'s
// test-driver socket; what lives here is the socket-name choice and the
// verb set it serves.
// ------------------------------------------------------------------

/// The dir this instance's socket lives under — the shared runtime-dir
/// rule [`ait_shell::ait_runtime_dir`] owns (`$XDG_RUNTIME_DIR/ait`,
/// else the per-user `$TMPDIR/ait-<uid>`): the same `ait` dir
/// `ait/ssh` ControlMaster sockets live beside. Unix-only: Windows
/// binds a named pipe, which has no dir.
#[cfg(unix)]
pub fn ctl_dir(runtime_dir: Option<&OsStr>) -> PathBuf {
    ait_shell::ait_runtime_dir(runtime_dir, None)
}

/// What a serviced request leaves for the app to finish — the pieces
/// of app state [`dispatch`] can't reach (the store is `app`'s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowUp {
    /// `open_tab` spawned a tab — it is the active tab; register its
    /// `terminal_sessions` row like `Alt+T` does.
    OpenedTab,
}

/// The boot-degradation toast text a [`CtlServer::bind`] failure
/// produces — "ctl socket unavailable: <error>". Both frontends'
/// launches degrade under this message (Spec R13 §5.12,
/// CODING_STANDARDS §17). A bind-failed run simply never gets its
/// ctl socket; the frontend keeps going.
pub fn socket_error_text(error: impl std::fmt::Display) -> String {
    format!("ctl socket unavailable: {error}")
}

/// The per-instance control socket: a nonblocking local-socket
/// listener — `<dir>/<pid>.sock` on Unix, `\\.\pipe\ait-<user>-<pid>`
/// on Windows — drained on the app tick.
pub struct CtlServer {
    /// The nonblocking listener.
    listener: LocalSocketListener,
    /// The bound socket name — the socket path on Unix, the pipe path
    /// on Windows; what `AIT_CTL_SOCKET` exports. Unlinked on [`Drop`]
    /// on Unix (a pipe name dies with the listener — nothing to do).
    socket: PathBuf,
    /// The socket's parent dir — removed on [`Drop`] when it's empty.
    /// `None` on Windows, where no dir exists.
    dir: Option<PathBuf>,
    /// Accepted connections mid-request.
    pending: Vec<Pending>,
}

impl CtlServer {
    /// Bind this instance's socket: `<pid>.sock` under the resolved
    /// ctl dir on Unix — the dir created `0700`, the socket chmodded
    /// `0600`, an existing file unlinked first (a crashed ait's stale
    /// socket is reclaimed, never a bind failure) — or
    /// `\\.\pipe\ait-<user>-<pid>` on Windows, where the default
    /// same-user ACLs take the `0600`'s place.
    pub fn bind() -> io::Result<Self> {
        #[cfg(unix)]
        {
            let dir = ctl_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref());
            Self::bind_under(dir)
        }
        #[cfg(windows)]
        {
            Self::bind_named(PathBuf::from(ipc::pipe_name(
                "ait",
                &ipc::user_token(),
                std::process::id(),
            )))
        }
    }

    /// [`bind`](Self::bind) at an explicit dir — the test seam.
    #[cfg(unix)]
    fn bind_under(dir: PathBuf) -> io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        ipc::set_mode(&dir, 0o700)?;
        let socket = dir.join(format!("{}.sock", std::process::id()));
        // Re-bind over a stale file: the pid is this process's, so the
        // file can only be ours or a dead predecessor's.
        let listener = ipc::bind_socket(socket.as_os_str())?;
        Ok(Self {
            listener,
            socket,
            dir: Some(dir),
            pending: Vec::new(),
        })
    }

    /// [`bind`](Self::bind) at an explicit pipe path — the Windows
    /// test seam (parallel binds pick their own names; the pid is
    /// this process's, so a caller-chosen name can only collide with
    /// a dead predecessor's).
    #[cfg(windows)]
    fn bind_named(socket: PathBuf) -> io::Result<Self> {
        let listener = ipc::bind_socket(socket.as_os_str())?;
        Ok(Self {
            listener,
            socket,
            dir: None,
            pending: Vec::new(),
        })
    }

    /// The bound socket name — the socket path on Unix, the
    /// `\\.\pipe\…` path on Windows; what `AIT_CTL_SOCKET` exports.
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// One app tick: accept every queued connection, buffer request
    /// lines, dispatch completed ones and flush responses — all
    /// nonblocking, so a slow or malicious client never stalls the UI
    /// (us-42). The [`FollowUp`]s dispatches leave (a spawned tab
    /// needs its `terminal_sessions` row) are returned for the caller.
    pub fn drain(&mut self, tabs: &mut TabManager) -> Vec<FollowUp> {
        Pending::accept(&self.listener, &mut self.pending);
        let mut follows = Vec::new();
        for mut conn in std::mem::take(&mut self.pending) {
            // `answer` is the decode+dispatch+encode fold — even a
            // malformed line earns a well-formed `{"ok":false}`.
            let mut follow = None;
            let verdict = conn.service(
                |line| {
                    let response = match decode_request(line) {
                        Ok(request) => {
                            let response = dispatch(tabs, &request);
                            follow = (matches!(request, Request::OpenTab) && response.ok)
                                .then_some(FollowUp::OpenedTab);
                            response
                        }
                        Err(error) => Response::error(error),
                    };
                    encode_response(&response)
                },
                || encode_response(&Response::error("request too large".to_owned())),
            );
            follows.extend(follow);
            if let Service::Keep = verdict {
                self.pending.push(conn);
            }
        }
        follows
    }
}

impl Drop for CtlServer {
    /// Clean exit unlinks the socket (us-43) and removes the `ait`
    /// dir best-effort — `remove_dir` only succeeds when it's empty,
    /// so sibling sockets (`ait/ssh`, other instances') keep it. On
    /// Windows the pipe name dies with the listener — nothing to do.
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.socket);
        if let Some(dir) = &self.dir {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

#[cfg(test)]
mod tests {
    //! The codec seam (Spec R11): the wire is single-line JSON —
    //! requests internally tagged on `"verb"`, responses `{"ok":…}` —
    //! and every malformed line is an error naming the failure, never
    //! a panic or a silent default.
    use super::*;
    /// The unix-gated tests' imports (live `/bin/sh` waits and socket
    /// reads/writes).
    #[cfg(unix)]
    use std::io::{Read, Write};
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    /// Every verb decodes from its documented wire shape — field
    /// names, optionality and defaults included.
    #[test]
    fn requests_decode_from_their_wire_shapes() {
        assert_eq!(
            decode_request(r#"{"verb":"open_tab"}"#),
            Ok(Request::OpenTab)
        );
        assert_eq!(
            decode_request(r#"{"verb":"send","text":"echo hi"}"#),
            Ok(Request::Send {
                tab: None,
                text: "echo hi".to_owned(),
                enter: true,
            }),
            "`enter` defaults true — plain `send` submits"
        );
        assert_eq!(
            decode_request(r#"{"verb":"send","tab":"2","text":"ls","enter":false}"#),
            Ok(Request::Send {
                tab: Some("2".to_owned()),
                text: "ls".to_owned(),
                enter: false,
            })
        );
        assert_eq!(
            decode_request(r#"{"verb":"last_block"}"#),
            Ok(Request::LastBlock { tab: None })
        );
        assert_eq!(
            decode_request(r#"{"verb":"last_block","tab":"7"}"#),
            Ok(Request::LastBlock {
                tab: Some("7".to_owned())
            })
        );
    }

    /// The encoder emits the same single line — embedded newlines in
    /// `text` stay JSON-escaped, so a request is always one physical
    /// line on the socket.
    #[test]
    fn requests_encode_to_one_json_line() {
        let line = encode_request(&Request::Send {
            tab: Some("2".to_owned()),
            text: "a\nb".to_owned(),
            enter: false,
        });
        assert_eq!(
            line,
            r#"{"verb":"send","tab":"2","text":"a\nb","enter":false}"#
        );
        assert!(!line.contains('\n'), "still one line");
        // Codec symmetry: encode → decode is the identity.
        assert_eq!(
            decode_request(&line).as_ref(),
            Ok(&Request::Send {
                tab: Some("2".to_owned()),
                text: "a\nb".to_owned(),
                enter: false,
            })
        );
        assert_eq!(encode_request(&Request::OpenTab), r#"{"verb":"open_tab"}"#);
    }

    /// Garbage lines, unknown verbs and missing required fields are
    /// all errors — no request is ever silently defaulted.
    #[test]
    fn malformed_requests_are_errors() {
        for line in [
            "",
            "not json",
            r#"{"verb":"bogus"}"#,
            r#"{"verb":"send"}"#,          // missing text
            r#"{"verb":"send","text":1}"#, // wrong type
            "[1,2]",
        ] {
            assert!(decode_request(line).is_err(), "{line:?}");
        }
        assert!(
            !decode_request("").unwrap_err().is_empty(),
            "the error says why"
        );
    }

    /// The response shapes the client and the spec's jq-composability
    /// depend on (us-41): successes carry their verb's fields,
    /// failures carry only `ok` + `error`, and a block with no `D`
    /// payload still reports `"exit":null` — absent and
    /// never-reported stay distinguishable.
    #[test]
    fn responses_encode_their_contract() {
        assert_eq!(encode_response(&Response::ok()), r#"{"ok":true}"#);
        assert_eq!(
            encode_response(&Response::opened("3".to_owned())),
            r#"{"ok":true,"session_id":"3"}"#
        );
        assert_eq!(
            encode_response(&Response::block(BlockReadout {
                command: "echo hi".to_owned(),
                output: "hi".to_owned(),
                exit: Some(3),
            })),
            r#"{"ok":true,"block":{"command":"echo hi","output":"hi","exit":3}}"#
        );
        assert_eq!(
            encode_response(&Response::block(BlockReadout {
                command: String::new(),
                output: String::new(),
                exit: None,
            })),
            r#"{"ok":true,"block":{"command":"","output":"","exit":null}}"#
        );
        assert_eq!(
            encode_response(&Response::error("no running ait instance".to_owned())),
            r#"{"ok":false,"error":"no running ait instance"}"#
        );
    }

    /// The client's decode half: every server shape lands, and a
    /// non-Response line is an error, not a zero-value.
    #[test]
    fn responses_decode_for_the_client() {
        assert_eq!(
            decode_response(r#"{"ok":true,"session_id":"3"}"#),
            Ok(Response::opened("3".to_owned()))
        );
        assert_eq!(
            decode_response(r#"{"ok":false,"error":"nope"}"#),
            Ok(Response::error("nope".to_owned()))
        );
        assert!(decode_response("wat").is_err());
    }

    // --------------------------------------------------------------
    // The dispatch seam: `dispatch(&mut TabManager, &Request)` over
    // real `/bin/sh` tabs — the same byte-fed session shapes the
    // tabs.rs tests drive. Unix-gated while the spawned shell is.
    // --------------------------------------------------------------

    /// Options spawning an interactive `/bin/sh` — the cheap real
    /// session the ticket allows.
    #[cfg(unix)]
    fn sh() -> ait_term::LiveSessionOptions {
        ait_term::LiveSessionOptions {
            program: Some(std::ffi::OsString::from("/bin/sh")),
            ..Default::default()
        }
    }

    /// Options spawning `/bin/sh -c <script>` — a scripted tab.
    #[cfg(unix)]
    fn sh_script(script: &str) -> ait_term::LiveSessionOptions {
        ait_term::LiveSessionOptions {
            program: Some(std::ffi::OsString::from("/bin/sh")),
            args: vec![
                std::ffi::OsString::from("-c"),
                std::ffi::OsString::from(script),
            ],
            ..Default::default()
        }
    }

    /// A manager with `tabs` interactive `/bin/sh` tabs.
    #[cfg(unix)]
    fn manager(tabs: usize) -> TabManager {
        let mut manager = TabManager::spawn_with(
            ait_term::Size::new(40, 10),
            crate::tabs::SessionTemplate {
                options: sh(),
                ..Default::default()
            },
        )
        .unwrap();
        for _ in 1..tabs {
            manager.new_tab().unwrap();
        }
        manager
    }

    /// Generous bound for every wait below (mirrors tabs.rs' live
    /// tests): tiny `/bin/sh` runs finish far under it.
    #[cfg(unix)]
    const TIMEOUT: Duration = Duration::from_secs(15);

    /// Drain until `pred` holds, or the timeout kills the test.
    #[cfg(unix)]
    fn drain_until(manager: &mut TabManager, pred: impl Fn(&TabManager) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !pred(manager) {
            manager.drain_events();
            assert!(Instant::now() < deadline, "timed out waiting");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// All text in a session's grid (scrollback then viewport).
    #[cfg(unix)]
    fn grid_text(session: &ait_term::LiveSession) -> String {
        let grid = session.grid();
        grid.scrollback_rows()
            .chain(grid.visible_rows())
            .map(|row| row.text().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A `sh -c` script emitting one `A B C D` block (`echo hi`,
    /// output `hi`, exit `3`) — the shape tabs.rs' block tests use.
    #[cfg(unix)]
    fn marks_script() -> ait_term::LiveSessionOptions {
        sh_script(
            "printf '\\033]133;A\\007$ \\033]133;B\\007'; sleep 0.2; \
             printf 'echo hi'; sleep 0.2; \
             printf '\\n\\033]133;C\\007'; sleep 0.2; \
             printf 'hi\\n'; sleep 0.2; \
             printf '\\033]133;D;3\\007'; sleep 0.5",
        )
    }

    /// The error half of a response — `None` on success.
    #[cfg(unix)]
    fn error_of(response: &Response) -> Option<&str> {
        response.error.as_deref()
    }

    /// `open_tab` allocates a tab in the running instance and answers
    /// its new `AIT_SESSION_ID` — the stable `Tab::id` as text that
    /// `--tab` resolution later matches.
    #[cfg(unix)]
    #[test]
    fn open_tab_allocates_and_reports_the_new_session_id() {
        let mut tabs = manager(1);
        let response = dispatch(&mut tabs, &Request::OpenTab);
        assert!(response.ok, "{}", error_of(&response).unwrap_or(""));
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs.active_index(), 1, "the spawned tab is active");
        assert_eq!(response.session_id.as_deref(), Some("1"));
        // ...and `--tab` resolves the reported id back to it.
        assert!(tabs.session_tab("1").is_some());
    }

    /// `send` writes the text + `\r` into the target's PTY — the
    /// command actually runs — and marks the tab's `InputShadow`
    /// stale so scripted input can never arm the triggers (us-16/44).
    #[cfg(unix)]
    #[test]
    fn send_types_and_submits_then_stales_the_shadow() {
        let mut tabs = manager(1);
        let response = dispatch(
            &mut tabs,
            &Request::Send {
                tab: None,
                text: "echo CTL$((40+2))".to_owned(),
                enter: true,
            },
        );
        assert!(response.ok, "{}", error_of(&response).unwrap_or(""));
        assert!(tabs.tabs()[0].shadow.is_stale(), "send staled the shadow");
        drain_until(&mut tabs, |m| {
            grid_text(&m.tabs()[0].session).contains("CTL42")
        });
    }

    /// `send --no-enter` leaves the text on the input line unexecuted
    /// — a following Enter (a later `send` without the flag) is what
    /// runs it.
    #[cfg(unix)]
    #[test]
    fn send_no_enter_stages_text_without_submitting() {
        let mut tabs = manager(1);
        assert!(
            dispatch(
                &mut tabs,
                &Request::Send {
                    tab: None,
                    text: "echo ST$((3+4))".to_owned(),
                    enter: false,
                },
            )
            .ok
        );
        // The typed line echoes, but nothing runs: `ST7` is what only
        // execution could produce (standard 15).
        drain_until(&mut tabs, |m| {
            grid_text(&m.tabs()[0].session).contains("echo ST$((3+4))")
        });
        assert!(
            !grid_text(&tabs.tabs()[0].session).contains("ST7"),
            "no Enter — nothing ran"
        );
        // A plain `send ""` is just the Enter: the staged line runs.
        assert!(
            dispatch(
                &mut tabs,
                &Request::Send {
                    tab: None,
                    text: String::new(),
                    enter: true,
                },
            )
            .ok
        );
        drain_until(&mut tabs, |m| {
            grid_text(&m.tabs()[0].session).contains("ST7")
        });
    }

    /// `--tab` routes `send` to the named tab — here the second one,
    /// while a third stays active and untouched.
    #[cfg(unix)]
    #[test]
    fn send_targets_a_tab_by_session_id() {
        let mut tabs = manager(3);
        let response = dispatch(
            &mut tabs,
            &Request::Send {
                tab: Some("1".to_owned()),
                text: "echo TAB$((1+1))".to_owned(),
                enter: true,
            },
        );
        assert!(response.ok);
        drain_until(&mut tabs, |m| {
            grid_text(&m.tabs()[1].session).contains("TAB2")
        });
        assert!(
            !grid_text(&tabs.tabs()[2].session).contains("TAB2"),
            "the active tab was not the target"
        );
    }

    /// An unknown `--tab` fails naming the id; a `send` to an exited
    /// tab fails instead of writing to a dead PTY.
    #[cfg(unix)]
    #[test]
    fn send_errors_on_unknown_and_exited_tabs() {
        let mut tabs = manager(1);
        let unknown = dispatch(
            &mut tabs,
            &Request::Send {
                tab: Some("9".to_owned()),
                text: "x".to_owned(),
                enter: true,
            },
        );
        assert!(!unknown.ok);
        assert_eq!(error_of(&unknown), Some("unknown tab \"9\""));

        // A tab whose shell already exited refuses input.
        let mut tabs = TabManager::spawn_with(
            ait_term::Size::new(40, 10),
            crate::tabs::SessionTemplate {
                options: sh_script("exit 7"),
                ..Default::default()
            },
        )
        .unwrap();
        drain_until(&mut tabs, |m| m.tabs()[0].exit_status().is_some());
        let exited = dispatch(
            &mut tabs,
            &Request::Send {
                tab: None,
                text: "x".to_owned(),
                enter: true,
            },
        );
        assert!(!exited.ok);
        assert_eq!(error_of(&exited), Some("the active tab has exited"));
    }

    /// `last_block` reads the newest *sealed* block's readout off the
    /// target — command, output rows and the `D` exit code — and a
    /// tab with no sealed blocks fails with a clear message (us-39).
    #[cfg(unix)]
    #[test]
    fn last_block_reads_the_newest_sealed_block() {
        let mut tabs = TabManager::spawn_with(
            ait_term::Size::new(40, 10),
            crate::tabs::SessionTemplate {
                options: marks_script(),
                ..Default::default()
            },
        )
        .unwrap();

        drain_until(&mut tabs, |m| {
            m.tabs()[0].blocks().iter().any(|b| b.completed())
        });
        let response = dispatch(&mut tabs, &Request::LastBlock { tab: None });
        assert!(response.ok, "{}", error_of(&response).unwrap_or(""));
        let block = response.block.expect("a block readout");
        assert_eq!(block.command, "echo hi");
        assert_eq!(block.output, "hi");
        assert_eq!(block.exit, Some(3));

        // A blockless tab (an interactive sh emits no marks) and an
        // unknown id both fail clearly.
        let mut plain = manager(1);
        let empty = dispatch(&mut plain, &Request::LastBlock { tab: None });
        assert!(!empty.ok);
        assert_eq!(
            error_of(&empty),
            Some("the active tab has no sealed blocks")
        );
        let unknown = dispatch(
            &mut plain,
            &Request::LastBlock {
                tab: Some("9".to_owned()),
            },
        );
        assert_eq!(error_of(&unknown), Some("unknown tab \"9\""));
    }

    // --------------------------------------------------------------
    // The socket surface: bind permissions, stale reclaim, cleanup on
    // drop, and the full request→dispatch→response drain — plus the
    // client-side resolution and render rules.
    // --------------------------------------------------------------

    /// A per-test tempdir.
    #[cfg(unix)]
    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// `ctl_dir` honors `$XDG_RUNTIME_DIR` and falls back to the
    /// per-user `$TMPDIR` name the ssh control dir uses.
    #[cfg(unix)]
    #[test]
    fn ctl_dir_prefers_xdg_then_tmpdir() {
        assert_eq!(
            ctl_dir(Some(std::ffi::OsStr::new("/run/user/1000"))),
            PathBuf::from("/run/user/1000/ait")
        );
        let fallback = ctl_dir(None);
        assert!(
            fallback.starts_with(std::env::temp_dir()),
            "fallback under $TMPDIR: {}",
            fallback.display()
        );
        // An empty runtime dir is "unset", not the current dir.
        assert_eq!(ctl_dir(Some(std::ffi::OsStr::new(""))), fallback);
    }

    /// The bind contract (us-43): parent dir `0700`, socket `0600`, a
    /// stale file re-bound over, and the socket unlinked when the
    /// server drops.
    #[cfg(unix)]
    #[test]
    fn bind_reclaims_stale_and_cleans_up() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        let socket_dir = dir.path().join("ait");

        let server = CtlServer::bind_under(socket_dir.clone()).unwrap();
        let socket = server.socket_path().to_path_buf();
        assert!(socket.exists());
        assert_eq!(
            std::fs::metadata(&socket_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(server);
        assert!(!socket.exists(), "clean exit unlinked the socket");

        // A stale file (a crashed predecessor) is reclaimed, not a
        // bind failure — the dir may have gone with the drop, so
        // recreate it first.
        std::fs::create_dir_all(&socket_dir).unwrap();
        std::fs::write(&socket, b"stale").unwrap();
        let server = CtlServer::bind_under(socket_dir).unwrap();
        assert!(server.socket_path().exists());
    }

    /// The full loop (us-41/42): a client writes one line, the drain
    /// dispatches it and flushes the response — several connections
    /// in flight at once get serviced by the same tick.
    #[cfg(unix)]
    #[test]
    fn drain_services_connections_on_the_tick() {
        let dir = tempdir();
        let mut server = CtlServer::bind_under(dir.path().to_path_buf()).unwrap();
        let mut tabs = manager(1);

        let mut first = ipc::connect(server.socket_path().as_os_str(), TIMEOUT).unwrap();
        let mut second = ipc::connect(server.socket_path().as_os_str(), TIMEOUT).unwrap();
        writeln!(first, r#"{{"verb":"last_block"}}"#).unwrap();
        writeln!(
            second,
            "{}",
            encode_request(&Request::Send {
                tab: None,
                text: "echo DR$((5+5))".to_owned(),
                enter: true,
            })
        )
        .unwrap();

        // One drain accepts both, answers both.
        let follows = server.drain(&mut tabs);
        assert!(follows.is_empty(), "no open_tab → no follow-up");

        let mut buf = String::new();
        first.read_to_string(&mut buf).unwrap();
        let response = decode_response(&buf).unwrap();
        assert!(
            !response.ok
                && response
                    .error
                    .is_some_and(|e| e.contains("no sealed blocks")),
            "{buf}"
        );
        let mut buf = String::new();
        second.read_to_string(&mut buf).unwrap();
        let response = decode_response(&buf).unwrap();
        assert!(response.ok, "{}", buf);
        assert!(tabs.tabs()[0].shadow.is_stale());
        drain_until(&mut tabs, |m| {
            grid_text(&m.tabs()[0].session).contains("DR10")
        });
    }

    /// A malformed line still earns a well-formed `{"ok":false}`
    /// before the close, and `open_tab` surfaces its `OpenedTab`
    /// follow-up for the app's store row.
    #[cfg(unix)]
    #[test]
    fn drain_answers_malformed_lines_and_reports_opened_tabs() {
        let dir = tempdir();
        let mut server = CtlServer::bind_under(dir.path().to_path_buf()).unwrap();
        let mut tabs = manager(1);

        let mut bogus = ipc::connect(server.socket_path().as_os_str(), TIMEOUT).unwrap();
        writeln!(bogus, "not json").unwrap();
        let mut open = ipc::connect(server.socket_path().as_os_str(), TIMEOUT).unwrap();
        writeln!(open, "{}", encode_request(&Request::OpenTab)).unwrap();

        let follows = server.drain(&mut tabs);
        assert_eq!(follows, vec![FollowUp::OpenedTab]);
        assert_eq!(tabs.len(), 2);

        let mut buf = String::new();
        bogus.read_to_string(&mut buf).unwrap();
        let response = decode_response(&buf).unwrap();
        assert!(!response.ok);
        assert!(
            response.error.unwrap().starts_with("malformed request"),
            "{buf}"
        );

        let mut buf = String::new();
        open.read_to_string(&mut buf).unwrap();
        assert_eq!(
            decode_response(&buf).unwrap(),
            Response::opened("1".to_owned())
        );
    }
}
