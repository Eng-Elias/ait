//! `ait ctl <verb>` — the control socket's client arm (Spec R11
//! §5.10.5, us-32/37/38/41): the argv verbs, socket resolution,
//! request round-trip and answer printing. The wire codec, dispatch
//! and server all live in `ait_app::ctl` — the shared layer every
//! frontend's instance registers — so only this binary's CLI surface
//! sits here (the GUI binds the same socket convention but ships no
//! `ctl` client).

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

#[cfg(unix)]
use ait_app::ctl::ctl_dir;
use ait_app::ctl::{
    decode_response, encode_request, encode_response, Request, Response, ENV_CTL_SOCKET,
};
use ait_app::ipc::{self, REQUEST_CAP};

/// Bound on the client's wait for a response — the server drains on
/// the ~8 ms app tick, so this only ever trips on a wedged instance.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);
// ------------------------------------------------------------------
// Client: `ait ctl <verb>` — resolve the socket, exchange one
// request, print the verb's answer (us-41).
// ------------------------------------------------------------------

/// Which `last_block` field the client prints — `Output` is the
/// bare-verb default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastBlockSelect {
    /// The newest sealed block's output text.
    Output,
    /// Its command text (`--command`).
    Command,
    /// Its exit code (`--exit`).
    Exit,
}

/// One `ait ctl` verb — the subcommand shape this ticket owns (Spec
/// R11: later CLI arms add variants additively).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtlVerb {
    /// `ait ctl open-tab` — allocate a tab, print its session id.
    OpenTab,
    /// `ait ctl send [--tab <id>] [--no-enter] <text>` — write input
    /// into the target tab.
    Send {
        /// `--tab <id>`; `None` is the active tab.
        tab: Option<String>,
        /// `false` under `--no-enter` — verbatim bytes, no `\r`.
        enter: bool,
        /// The input text (positionals joined with spaces).
        text: String,
    },
    /// `ait ctl last-block [--tab <id>] [--command|--exit]` — the
    /// newest sealed block's readout.
    LastBlock {
        /// `--tab <id>`; `None` is the active tab.
        tab: Option<String>,
        /// Which field of the readout prints.
        select: LastBlockSelect,
    },
}

impl CtlVerb {
    /// The wire request this verb sends — `last_block`'s selectors
    /// are client-side over the same response.
    fn request(&self) -> Request {
        match self {
            CtlVerb::OpenTab => Request::OpenTab,
            CtlVerb::Send { tab, enter, text } => Request::Send {
                tab: tab.clone(),
                text: text.clone(),
                enter: *enter,
            },
            CtlVerb::LastBlock { tab, .. } => Request::LastBlock { tab: tab.clone() },
        }
    }
}

/// `ait ctl`'s argv tail → its verb. `--tab` and the selector flags
/// are per-verb; `send`'s positionals join with a space so the text
/// needs no quoting.
pub fn parse_ctl_args(args: &[String]) -> Result<CtlVerb, String> {
    match args.first().map(String::as_str) {
        Some("open-tab") => match args.len() {
            1 => Ok(CtlVerb::OpenTab),
            _ => Err(format!("open-tab takes no arguments: {}", args[1])),
        },
        Some("send") => parse_send(&args[1..]),
        Some("last-block") => parse_last_block(&args[1..]),
        Some(other) => Err(format!("unknown ctl verb {other:?}")),
        None => Err("ctl needs a verb: open-tab, send, last-block".to_owned()),
    }
}

/// `send`'s flags and positionals.
fn parse_send(args: &[String]) -> Result<CtlVerb, String> {
    let mut tab = None;
    let mut enter = true;
    let mut words = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--tab" => {
                let Some(value) = args.next() else {
                    return Err("send --tab needs a session id".to_owned());
                };
                tab = Some(value.clone());
            }
            "--no-enter" => enter = false,
            flag if flag.starts_with("--") => {
                return Err(format!("unknown send flag {flag:?}"));
            }
            _ => words.push(arg.clone()),
        }
    }
    if words.is_empty() {
        return Err("ctl send needs text to write".to_owned());
    }
    Ok(CtlVerb::Send {
        tab,
        enter,
        text: words.join(" "),
    })
}

/// `last-block`'s flags — the selectors are mutually exclusive.
fn parse_last_block(args: &[String]) -> Result<CtlVerb, String> {
    let mut tab = None;
    let mut select = LastBlockSelect::Output;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--tab" => {
                let Some(value) = args.next() else {
                    return Err("last-block --tab needs a session id".to_owned());
                };
                tab = Some(value.clone());
            }
            "--command" => select = choose(select, LastBlockSelect::Command, "--command")?,
            "--exit" => select = choose(select, LastBlockSelect::Exit, "--exit")?,
            flag => return Err(format!("unknown last-block argument {flag:?}")),
        }
    }
    Ok(CtlVerb::LastBlock { tab, select })
}

/// `last-block`'s selector exclusivity — a second selector flag is a
/// usage error, not a silent override.
fn choose(
    current: LastBlockSelect,
    new: LastBlockSelect,
    flag: &str,
) -> Result<LastBlockSelect, String> {
    if current == LastBlockSelect::Output {
        Ok(new)
    } else {
        Err(format!("{flag} can't combine with another selector"))
    }
}

/// Where `ait ctl` connects: `AIT_CTL_SOCKET` when set (a spawned
/// session's own instance — on Windows it carries the
/// `\\.\pipe\ait-<user>-<pid>` name), else the platform's discovery
/// rule for the live instance's endpoint — zero is "no running ait
/// instance", several an error listing each candidate (us-32).
///
/// On Unix that's the single LIVE `*.sock` under the resolved ctl
/// dir: a `kill -9`'d ait leaves its socket file behind, so each
/// candidate is probed with a connect (a dead socket refuses
/// instantly); stale files are unlinked and never counted.
#[cfg(unix)]
fn resolve_socket(
    socket_env: Option<&OsStr>,
    runtime_dir: Option<&OsStr>,
) -> Result<PathBuf, String> {
    if let Some(path) = socket_env.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let dir = ctl_dir(runtime_dir);
    let mut sockets: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "sock"))
                .collect()
        })
        .unwrap_or_default();
    sockets.sort();
    // Liveness probe: a bound-but-dead socket fails `connect` with
    // ECONNREFUSED immediately — the probe costs a local syscall,
    // the reclaimed file saves every later scan the re-probe, and a
    // successful probe's connection closes unanswered (the server's
    // pending-connection TTL reaps it).
    sockets.retain(|path| {
        if ipc::connect(path.as_os_str(), CLIENT_TIMEOUT).is_ok() {
            true
        } else {
            let _ = std::fs::remove_file(path);
            false
        }
    });
    pick_socket(sockets)
}

/// The Windows arm of [`resolve_socket`]: named pipes live under
/// `\\.\pipe\` and die with their last server handle, so every
/// `ait-<user>-<pid>` name the pipe root lists is live by
/// construction — no probe, nothing to unlink. `runtime_dir` is a
/// Unix concept the pipe namespace doesn't need.
#[cfg(windows)]
fn resolve_socket(
    socket_env: Option<&OsStr>,
    runtime_dir: Option<&OsStr>,
) -> Result<PathBuf, String> {
    let _ = runtime_dir;
    if let Some(path) = socket_env.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let prefix = format!("ait-{}-", ipc::user_token());
    let mut sockets: Vec<PathBuf> = std::fs::read_dir(r"\\.\pipe\")
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
                })
                .collect()
        })
        .unwrap_or_default();
    sockets.sort();
    pick_socket(sockets)
}

/// The shared end of socket resolution: zero candidates is "no
/// running ait instance", one is it, several an error naming each —
/// set [`ENV_CTL_SOCKET`] to pick (us-32).
fn pick_socket(mut sockets: Vec<PathBuf>) -> Result<PathBuf, String> {
    match sockets.len() {
        0 => Err("no running ait instance".to_owned()),
        1 => Ok(sockets.remove(0)),
        _ => Err(format!(
            "several ait instances running — set {ENV_CTL_SOCKET} to pick one: {}",
            sockets
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// One request → one response line on `endpoint` (us-41): connect,
/// write the JSON line, read to the response's newline.
/// [`CLIENT_TIMEOUT`] — armed by [`ipc::connect`] — bounds the wait
/// so a wedged instance fails rather than hangs.
fn round_trip(endpoint: &Path, request: &Request) -> Result<Response, String> {
    let mut stream = ipc::connect(endpoint.as_os_str(), CLIENT_TIMEOUT)
        .map_err(|e| format!("can't reach {}: {e}", endpoint.display()))?;
    let mut line = encode_request(request);
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|e| format!("request write failed: {e}"))?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return Err("the instance closed without answering".to_owned()),
            Ok(n) => {
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(end) = bytes.iter().position(|b| *b == b'\n') {
                    return decode_response(&String::from_utf8_lossy(&bytes[..end]));
                }
                if bytes.len() > REQUEST_CAP {
                    return Err("response too large".to_owned());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("response read failed: {error}")),
        }
    }
}

/// The verb's stdout text — the response's JSON line for `open_tab`
/// and `send` (us-41 composes with `jq`), the selected field for
/// `last_block` (us-37/38).
fn render(verb: &CtlVerb, response: &Response) -> Result<String, String> {
    if !response.ok {
        return Err(response
            .error
            .clone()
            .unwrap_or_else(|| "the instance refused the request".to_owned()));
    }
    match verb {
        CtlVerb::OpenTab | CtlVerb::Send { .. } => {
            let mut line = encode_response(response);
            line.push('\n');
            Ok(line)
        }
        CtlVerb::LastBlock { select, .. } => {
            let Some(block) = &response.block else {
                return Err("the instance answered without a block".to_owned());
            };
            match select {
                LastBlockSelect::Output => Ok(format!("{}\n", block.output)),
                LastBlockSelect::Command => Ok(format!("{}\n", block.command)),
                LastBlockSelect::Exit => match block.exit {
                    Some(code) => Ok(format!("{code}\n")),
                    None => Err("the newest sealed block has no exit code".to_owned()),
                },
            }
        }
    }
}

/// `ait ctl <verb>` — the client arm `main` dispatches to. One
/// request per invocation; the verb's answer goes to stdout and every
/// failure exits non-zero with its message on stderr (us-41).
pub fn run_client(verb: &CtlVerb) -> ExitCode {
    let request = verb.request();
    let outcome = resolve_socket(
        std::env::var_os(ENV_CTL_SOCKET).as_deref(),
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
    )
    .and_then(|path| round_trip(&path, &request))
    .and_then(|response| render(verb, &response));
    match outcome {
        Ok(text) => {
            let _ = io::stdout().lock().write_all(text.as_bytes());
            ExitCode::SUCCESS
        }
        Err(message) => {
            let _ = writeln!(io::stderr().lock(), "ait ctl: {message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    //! The client-side seams — socket resolution, the answer render
    //! and the argv parse — over `ait_app::ctl`'s wire types.
    use super::*;
    use ait_app::ctl::BlockReadout;
    #[cfg(unix)]
    use std::os::unix::net::UnixListener;

    /// A per-test tempdir.
    #[cfg(unix)]
    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// Socket resolution (us-32): `AIT_CTL_SOCKET` wins; under the
    /// ctl dir zero LIVE sockets is "no running ait instance", one
    /// live socket is it, several is a candidate listing — and a
    /// stale file a killed ait left behind is probed out and
    /// unlinked, never counted.
    #[cfg(unix)]
    #[test]
    fn resolve_socket_env_then_dir_scan() {
        let dir = tempdir();
        let explicit = std::ffi::OsStr::new("/explicit/path.sock");
        assert_eq!(
            resolve_socket(Some(explicit), Some(dir.path().as_os_str())).unwrap(),
            PathBuf::from("/explicit/path.sock")
        );

        // No sockets under the runtime dir → the zero-instance error.
        let none = resolve_socket(None, Some(dir.path().as_os_str())).unwrap_err();
        assert_eq!(none, "no running ait instance");

        let ait_dir = dir.path().join("ait");
        std::fs::create_dir_all(&ait_dir).unwrap();

        // A lone stale file probes dead, gets unlinked and reports
        // exactly like an empty dir — not "can't reach".
        std::fs::write(ait_dir.join("999.sock"), b"").unwrap();
        let stale = resolve_socket(None, Some(dir.path().as_os_str())).unwrap_err();
        assert_eq!(stale, "no running ait instance");
        assert!(
            !ait_dir.join("999.sock").exists(),
            "the probe reclaims the stale file"
        );

        // One live listener → it. The binding stays alive in `_one`
        // for the rest of the test (`let _ =` would drop it now).
        let _one = UnixListener::bind(ait_dir.join("111.sock")).unwrap();
        assert_eq!(
            resolve_socket(None, Some(dir.path().as_os_str())).unwrap(),
            ait_dir.join("111.sock")
        );

        // A stale file beside the live socket is skipped — live +
        // stale still connects to the live one, not "several".
        std::fs::write(ait_dir.join("000.sock"), b"").unwrap();
        assert_eq!(
            resolve_socket(None, Some(dir.path().as_os_str())).unwrap(),
            ait_dir.join("111.sock"),
            "a dead sibling never reaches the several-error"
        );
        assert!(!ait_dir.join("000.sock").exists());

        // Two live sockets → the error lists each candidate.
        let _two = UnixListener::bind(ait_dir.join("222.sock")).unwrap();
        let several = resolve_socket(None, Some(dir.path().as_os_str())).unwrap_err();
        assert!(several.contains("several ait instances"), "{several}");
        assert!(several.contains("111.sock"), "{several}");
        assert!(several.contains("222.sock"), "{several}");
        assert!(several.contains(ENV_CTL_SOCKET), "{several}");
    }

    /// The render contract (us-37/38/41): `open_tab`/`send` print the
    /// response's JSON line; `last_block` prints the output by
    /// default, the command or exit code under its selectors.
    #[test]
    fn render_prints_each_verbs_answer() {
        assert_eq!(
            render(
                &CtlVerb::Send {
                    tab: None,
                    enter: true,
                    text: "x".into()
                },
                &Response::ok()
            )
            .unwrap(),
            "{\"ok\":true}\n"
        );
        assert_eq!(
            render(&CtlVerb::OpenTab, &Response::opened("4".into())).unwrap(),
            "{\"ok\":true,\"session_id\":\"4\"}\n"
        );
        let block = Response::block(BlockReadout {
            command: "echo hi".into(),
            output: "hi\nthere".into(),
            exit: Some(3),
        });
        let verb = |select| CtlVerb::LastBlock { tab: None, select };
        assert_eq!(
            render(&verb(LastBlockSelect::Output), &block).unwrap(),
            "hi\nthere\n"
        );
        assert_eq!(
            render(&verb(LastBlockSelect::Command), &block).unwrap(),
            "echo hi\n"
        );
        assert_eq!(render(&verb(LastBlockSelect::Exit), &block).unwrap(), "3\n");

        // A block with no `D` payload: `--exit` fails clearly rather
        // than printing `null`.
        let no_exit = Response::block(BlockReadout {
            command: "x".into(),
            output: String::new(),
            exit: None,
        });
        assert_eq!(
            render(&verb(LastBlockSelect::Exit), &no_exit).unwrap_err(),
            "the newest sealed block has no exit code"
        );
        // Server errors surface verbatim for stderr.
        assert_eq!(
            render(&CtlVerb::OpenTab, &Response::error("nope".into())).unwrap_err(),
            "nope"
        );
    }

    /// The argv seam for `ctl` itself: verbs, flags, the selectors'
    /// exclusivity and every malformed shape.
    #[test]
    fn ctl_args_parse_their_verbs() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_ctl_args(&args(&["open-tab"])), Ok(CtlVerb::OpenTab));
        assert_eq!(
            parse_ctl_args(&args(&["send", "echo hi"])),
            Ok(CtlVerb::Send {
                tab: None,
                enter: true,
                text: "echo hi".to_owned(),
            })
        );
        // Unquoted words join with spaces.
        assert_eq!(
            parse_ctl_args(&args(&["send", "--tab", "2", "--no-enter", "echo", "hi"])),
            Ok(CtlVerb::Send {
                tab: Some("2".to_owned()),
                enter: false,
                text: "echo hi".to_owned(),
            })
        );
        assert_eq!(
            parse_ctl_args(&args(&["last-block"])),
            Ok(CtlVerb::LastBlock {
                tab: None,
                select: LastBlockSelect::Output,
            })
        );
        assert_eq!(
            parse_ctl_args(&args(&["last-block", "--tab", "2", "--exit"])),
            Ok(CtlVerb::LastBlock {
                tab: Some("2".to_owned()),
                select: LastBlockSelect::Exit,
            })
        );
        assert_eq!(
            parse_ctl_args(&args(&["last-block", "--command"])),
            Ok(CtlVerb::LastBlock {
                tab: None,
                select: LastBlockSelect::Command,
            })
        );
        for list in [
            &[][..],
            &["bogus"][..],
            &["send"][..],                              // no text
            &["send", "--tab"][..],                     // missing value
            &["send", "--exit", "x"][..],               // wrong verb's flag
            &["last-block", "--command", "--exit"][..], // exclusive
            &["last-block", "extra"][..],
            &["open-tab", "extra"][..],
        ] {
            assert!(parse_ctl_args(&args(list)).is_err(), "{list:?}");
        }
    }
}
