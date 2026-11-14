//! The local-socket IPC transport (Spec R16 §5.14, R16-T6) — the one
//! seam the control socket's server, the `ait ctl` client and
//! `ait-gui`'s test-driver socket all share. Both line sockets run
//! the same conventions on every platform: a nonblocking listener,
//! one request per connection, single-line JSON, accepted
//! connections buffered across drain passes so a slow client never
//! wedges the UI.
//!
//! An **endpoint** is the name a socket binds and clients connect
//! to: a filesystem path on Unix (`<dir>/<pid>.sock`, socket `0600`
//! under a `0700` dir, a stale file reclaimed on bind and unlinked
//! on clean exit) or a `\\.\pipe\…` named-pipe path on Windows —
//! `\\.\pipe\ait-<user>-<pid>` for the control socket — whose
//! default ACLs already scope it to the same user and which vanishes
//! with its last handle, so nothing needs unlinking there.
//! `ENV_CTL_SOCKET`/`AIT_TEST_DRIVER_SOCKET` carry whichever form
//! the platform uses; [`local_name`] reads both the verbatim pipe
//! path and a bare pipe name on Windows.
//!
//! The transport is `interprocess`'s local socket — a plain byte
//! stream on both platforms, so the wire protocol is byte-identical
//! to the pre-port Unix sockets. Only naming and teardown differ
//! per-platform; everything else — [`bind_socket`], [`connect`],
//! [`Pending`]'s buffered lifecycle — is one code path.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{GenericFilePath, ListenerNonblockingMode, ListenerOptions, Name};

#[cfg(unix)]
use std::path::Path;
#[cfg(windows)]
use interprocess::local_socket::GenericNamespaced;

/// The listener type [`bind_socket`] returns — `interprocess`'s
/// local-socket listener, a Unix socket on Unix and a named-pipe
/// listener on Windows. Re-exported so consumers name the transport
/// through this seam, not the crate.
pub use interprocess::local_socket::Listener as LocalSocketListener;
/// The stream type [`connect`] and accepted [`Pending`]s carry —
/// the listener's client side.
pub use interprocess::local_socket::Stream as LocalSocketStream;

/// `endpoint` → its `interprocess` name. On Unix every endpoint is
/// a filesystem path; on Windows a `\\HOST\pipe\…` path maps
/// verbatim (the form `socket_path`/the env vars carry) and anything
/// else is a bare name under `\\.\pipe\`. The one interpretation
/// [`bind_socket`] and [`connect`] share — the testkit harness
/// mirrors it across the crate boundary.
fn local_name(endpoint: &OsStr) -> io::Result<Name<'_>> {
    #[cfg(unix)]
    {
        endpoint.to_fs_name::<GenericFilePath>()
    }
    #[cfg(windows)]
    {
        // `\\.\pipe\…` (or `\\HOST\pipe\…`) is a pipe path already —
        // anything else is a bare pipe name.
        match endpoint.as_encoded_bytes().starts_with(b"\\\\") {
            true => endpoint.to_fs_name::<GenericFilePath>(),
            false => endpoint.to_ns_name::<GenericNamespaced>(),
        }
    }
}

/// `\\.\pipe\<prefix>-<user>-<pid>` — the named-pipe path an
/// endpoint takes on Windows: the `<user>` token scopes it to the
/// account the way the Unix `ait-<uid>` runtime dirs do, `<pid>` to
/// the instance the way `<pid>.sock` does. Pure so its shape is
/// pinned by tests on every platform — Windows is the only
/// production reach.
#[cfg(any(windows, test))]
pub fn pipe_name(prefix: &str, user: &str, pid: u32) -> String {
    format!(r"\\.\pipe\{prefix}-{user}-{pid}")
}

/// The per-user token [`pipe_name`] scopes on — `$USER`/`$USERNAME`
/// like [`ait_shell::ait_runtime_dir`]'s fallback uid. A pipe name
/// can't contain a path separator, so any fold to `-`.
#[cfg(windows)]
pub fn user_token() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".to_owned())
        .replace(['\\', '/'], "-")
}

/// `path` at `mode` — the spec's permissions (dir `0700`, socket
/// `0600`); same-uid is the trust boundary. Public so the
/// test-driver socket (`ait-gui`'s `test_driver` module, Spec R15)
/// holds the same line. Unix-only: a named pipe carries no mode
/// bits — Windows's default ACLs scope it to the same user already.
#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// The listener bind every line socket shares. On Unix a stale file
/// at the path is reclaimed (a crashed predecessor's), the socket
/// gets its `0600`, and the listener plus its accepted streams are
/// nonblocking so accept/read/write never stall a drain pass. On
/// Windows `endpoint` binds a `\\.\pipe\…` named pipe under the
/// default (same-user) ACLs — there is no file to unlink or chmod.
pub fn bind_socket(endpoint: &OsStr) -> io::Result<LocalSocketListener> {
    #[cfg(unix)]
    {
        let path = Path::new(endpoint);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    let listener = ListenerOptions::new()
        .name(local_name(endpoint)?)
        .nonblocking(ListenerNonblockingMode::Both)
        .create_sync()?;
    #[cfg(unix)]
    set_mode(Path::new(endpoint), 0o600)?;
    Ok(listener)
}

/// The client half [`bind_socket`] answers: connect to `endpoint`
/// and arm `timeout` on reads and writes — one request's whole
/// round trip lives inside it.
pub fn connect(endpoint: &OsStr, timeout: Duration) -> io::Result<LocalSocketStream> {
    let stream = LocalSocketStream::connect(local_name(endpoint)?)?;
    stream.set_recv_timeout(Some(timeout))?;
    stream.set_send_timeout(Some(timeout))?;
    Ok(stream)
}

/// Bound on one buffered request/response line — a peer that never
/// finishes its line can't grow memory unboundedly; past this the
/// connection gets a `{"ok":false}` and closes.
pub const REQUEST_CAP: usize = 64 * 1024;

/// How long a half-open or mid-reply connection lives before the
/// drain drops it — a client that connects but never completes a
/// line is never serviced forever.
const PENDING_TTL: Duration = Duration::from_secs(30);

/// Whether a pending connection survives this service pass.
pub enum Service {
    /// Keep it for the next tick (request incomplete, reply stalled).
    Keep,
    /// Drop it — answered, expired, or the client went away.
    Close,
}

/// One accepted connection buffered across ticks: request bytes until
/// a full line exists, then the response flushing out. This is the
/// transport half of the line-socket protocol — shared verbatim with
/// `ait-gui`'s test-driver socket (Spec R15), which runs the same
/// conventions against a different verb set: the caller supplies
/// `answer`/`reject` so this never knows the server's response shape.
pub struct Pending {
    /// The accepted (nonblocking) stream.
    stream: LocalSocketStream,
    /// Buffered request bytes — a complete request ends at `'\n'`.
    request: Vec<u8>,
    /// The response being flushed — `(bytes, written)`; a nonblocking
    /// write that stalls mid-response resumes next tick.
    reply: Option<(Vec<u8>, usize)>,
    /// When the connection was accepted — the [`PENDING_TTL`] bound.
    at: Instant,
}

impl Pending {
    /// A fresh accepted connection.
    fn new(stream: LocalSocketStream) -> Self {
        Self {
            stream,
            request: Vec::new(),
            reply: None,
            at: Instant::now(),
        }
    }

    /// Accept every queued connection on `listener` into `pending` —
    /// the drain's accept loop, shared by ctl and the test-driver
    /// socket. An accept that can't go nonblocking is dropped: a
    /// blocking conn in `pending` would wedge the tick.
    pub fn accept(listener: &LocalSocketListener, pending: &mut Vec<Pending>) {
        loop {
            match listener.accept() {
                Ok(stream) => {
                    if stream.set_nonblocking(true).is_ok() {
                        pending.push(Pending::new(stream));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }

    /// Service the connection once: flush a queued reply, else buffer
    /// request bytes until a full line exists and hand it to
    /// `answer` — the server's decode+dispatch+encode fold, whose
    /// return is queued verbatim as the reply line (the `\n` is
    /// appended here). `reject` produces the oversized-request
    /// refusal's line. Each closure runs at most once, and `answer`
    /// always runs synchronously — the caller's side effects (state
    /// reads, queued side work) happen inside it.
    pub fn service(
        &mut self,
        answer: impl FnOnce(&str) -> String,
        reject: impl FnOnce() -> String,
    ) -> Service {
        if self.at.elapsed() > PENDING_TTL {
            return Service::Close;
        }
        if self.reply.is_some() {
            return self.flush();
        }
        let mut chunk = [0u8; 8192];
        match self.stream.read(&mut chunk) {
            // EOF — the client went away mid-request.
            Ok(0) => Service::Close,
            Ok(n) => {
                self.request.extend_from_slice(&chunk[..n]);
                match self.request.iter().position(|b| *b == b'\n') {
                    // A line that only terminates past the cap is the
                    // same oversize request, not a valid line.
                    Some(end) if end > REQUEST_CAP => self.queue(reject()),
                    Some(end) => {
                        let line = String::from_utf8_lossy(&self.request[..end]).into_owned();
                        self.queue(answer(&line))
                    }
                    None if self.request.len() > REQUEST_CAP => self.queue(reject()),
                    None => Service::Keep,
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Service::Keep,
            Err(_) => Service::Close,
        }
    }

    /// Queue `line` as the reply and start flushing it.
    fn queue(&mut self, mut line: String) -> Service {
        line.push('\n');
        self.reply = Some((line.into_bytes(), 0));
        self.flush()
    }

    /// Write out the queued reply — a full flush closes the
    /// connection (one request per invocation), a `WouldBlock` keeps
    /// it for the next tick.
    fn flush(&mut self) -> Service {
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
        // One request per connection: the answer is flushed and the
        // stream drops. A local socket can't half-close portably, but
        // the drop ends the client's read exactly like the Unix
        // `shutdown(Write)` did — on Windows the peer's outstanding
        // write drains through interprocess's linger pool first.
        Service::Close
    }
}

#[cfg(test)]
mod tests {
    //! The pure seams: the pipe-name shape Windows binds, and the
    //! Unix half of [`local_name`] (a filesystem path verbatim).
    use super::*;

    /// `\\.\pipe\<prefix>-<user>-<pid>` — the literal the spec pins
    /// (`ait-<user>-<pid>` under `\\.\pipe\`), asserted on every
    /// platform so the shape can't drift.
    #[test]
    fn pipe_name_builds_the_spec_shape() {
        assert_eq!(
            pipe_name("ait", "alice", 4242),
            r"\\.\pipe\ait-alice-4242"
        );
        assert_eq!(
            pipe_name("ait-driver", "bob", 7),
            r"\\.\pipe\ait-driver-bob-7"
        );
    }

    /// On Unix an endpoint is a filesystem path verbatim: the bind
    /// lands the socket file at `0600` and [`connect`] reaches it —
    /// the same behavior `UnixListener` used to give.
    #[cfg(unix)]
    #[test]
    fn unix_endpoint_binds_and_connects_at_its_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("x.sock");
        let _listener = bind_socket(socket.as_os_str()).unwrap();
        assert_eq!(
            std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _conn = connect(socket.as_os_str(), Duration::from_secs(5)).unwrap();
    }
}
