//! Session events observed on the PTY output stream (Spec R01 — the OSC
//! interception seam).
//!
//! A second `vte` parser watches the same bytes fed to
//! [`Session::feed_bytes`](crate::Session::feed_bytes). Observation only —
//! nothing is stripped, so the VT emulator's own handling is unaffected and
//! OSC sequences it already understands (title, hyperlinks, …) still render.
//! OSC 7/9/133/777 and the AIT-private 7700-series are dropped by
//! `alacritty_terminal`'s internal performer, so this observer is the only
//! place they surface (REQUIREMENTS.md §5.1, §5.5).

use std::path::PathBuf;

use alacritty_terminal::vte::Perform;

/// An event surfaced by a [`Session`](crate::Session) while observing PTY
/// output. Returned by [`Session::feed_bytes`](crate::Session::feed_bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// OSC 7 — the shell reported its working directory.
    CwdChanged(PathBuf),
    /// OSC 133 — a semantic prompt mark (REQUIREMENTS.md §5.5 glossary:
    /// "OSC 133"). Block assembly lands in Round 5; Round 1 records the
    /// marks.
    PromptMark {
        /// Which boundary the mark delimits.
        kind: PromptMarkKind,
        /// Everything after the mark letter, if present — e.g. the exit code
        /// on `OSC 133;D;0`. Raw bytes joined with `;`, lossy-decoded.
        payload: Option<String>,
    },
    /// OSC 9 / OSC 777 — an application asks for a user notification. The
    /// frontend decides how to toast it; `title` is `Some` only when the
    /// sequence carried one (`OSC 777;notify;<title>;<body>`).
    Notification {
        /// Optional notification title (OSC 777 only).
        title: Option<String>,
        /// Notification text, lossy-decoded from the raw params.
        message: String,
    },
    /// OSC 52 — an application asked to write the clipboard. Emitted only
    /// when the session opted in via [`SessionOptions::osc52`]; the event is
    /// a *request* — the frontend performs the actual clipboard write.
    ClipboardWrite {
        /// Which selection the application targeted.
        selection: ClipboardSelection,
        /// Base64-decoded clipboard contents.
        contents: String,
    },
    /// Any OSC we do not otherwise recognize, including the AIT-private
    /// 7700-series hook channel (REQUIREMENTS.md §5.5): `code` is the raw
    /// first `;`-separated parameter (e.g. `b"7700"`) and `params` the rest.
    /// Malformed sequences for recognized codes also land here, carrying the
    /// raw params untouched.
    Private {
        /// Raw bytes of the OSC code (first parameter).
        code: Vec<u8>,
        /// Raw bytes of every remaining `;`-separated parameter.
        params: Vec<Vec<u8>>,
    },
}

/// Which semantic-prompt boundary an `OSC 133` mark delimits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMarkKind {
    /// `OSC 133;A` — a fresh prompt begins.
    PromptStart,
    /// `OSC 133;B` — the command line begins (prompt ended).
    CommandStart,
    /// `OSC 133;C` — the command was executed; its output begins.
    OutputStart,
    /// `OSC 133;D` — the command finished; the payload usually carries the
    /// exit code.
    CommandEnd,
}

/// Which selection an `OSC 52` clipboard-write request targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardSelection {
    /// The regular clipboard (OSC 52 `c`).
    Clipboard,
    /// The primary/secondary selection (OSC 52 `p`, `s`, …).
    Selection,
}

/// `vte::Perform` implementation for the parallel OSC observer. Implements
/// only `osc_dispatch`; everything else keeps the default no-op.
#[derive(Debug, Default)]
pub(crate) struct OscObserver {
    events: Vec<SessionEvent>,
}

impl OscObserver {
    /// Events accumulated since the last `take` call.
    pub(crate) fn take_events(&mut self) -> Vec<SessionEvent> {
        std::mem::take(&mut self.events)
    }
}

impl Perform for OscObserver {
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let Some((&code, args)) = params.split_first() else {
            return;
        };

        let event = match code {
            b"7" => cwd_from_uri(args.first().copied()).map(SessionEvent::CwdChanged),
            b"133" => prompt_mark(args),
            b"9" => join_params(args).map(|message| SessionEvent::Notification {
                title: None,
                message,
            }),
            b"777" => rxvt_notification(args),
            // OSC 52 is handled inside the emulator (`config.osc52` gate)
            // and arrives via `Event::ClipboardStore` — see `Session`'s
            // listener. The observer never duplicates it, gated or not.
            b"52" => return,
            _ => None,
        };

        // Recognized-but-malformed and unrecognized sequences alike surface as
        // `Private` with the raw params — nothing observed is ever dropped.
        let event = event.unwrap_or_else(|| SessionEvent::Private {
            code: code.to_vec(),
            params: args.iter().map(|param| param.to_vec()).collect(),
        });
        self.events.push(event);
    }
}

/// Parse an `OSC 133` payload (`133;<mark>` or `133;<mark>;<payload>`) into
/// a [`SessionEvent::PromptMark`]. Unknown marks and missing marks return
/// `None` so the sequence surfaces as [`SessionEvent::Private`] instead.
fn prompt_mark(args: &[&[u8]]) -> Option<SessionEvent> {
    let kind = match *args.first()? {
        b"A" => PromptMarkKind::PromptStart,
        b"B" => PromptMarkKind::CommandStart,
        b"C" => PromptMarkKind::OutputStart,
        b"D" => PromptMarkKind::CommandEnd,
        _ => return None,
    };
    // `args.first()?` above guarantees `args[1..]` exists (possibly empty);
    // `join_params` maps an empty rest to `None`.
    let payload = join_params(&args[1..]);
    Some(SessionEvent::PromptMark { kind, payload })
}

/// Join lossy-decoded params on `;`, or `None` when there are none.
fn join_params(args: &[&[u8]]) -> Option<String> {
    if args.is_empty() {
        return None;
    }
    Some(
        args.iter()
            .map(|param| String::from_utf8_lossy(param))
            .collect::<Vec<_>>()
            .join(";"),
    )
}

/// Parse an `OSC 777` payload. The common form is
/// `777;notify;<title>;<body>` (rxvt-unicode convention); a lone payload
/// after the verb is a title-less message. The `notify` verb itself is
/// never part of the payload. Anything else is still a notification, just
/// without a title.
fn rxvt_notification(args: &[&[u8]]) -> Option<SessionEvent> {
    match args {
        // `777;notify;<title>;<body>` — the rxvt-unicode convention.
        [b"notify", title, body @ ..] if !body.is_empty() => Some(SessionEvent::Notification {
            title: Some(String::from_utf8_lossy(title).into_owned()),
            message: join_params(body).unwrap_or_default(),
        }),
        // `777;notify;<message>` — a bare message with no title.
        [b"notify", message] => Some(SessionEvent::Notification {
            title: None,
            message: String::from_utf8_lossy(message).into_owned(),
        }),
        // `777;notify` with no payload: nothing to toast — malformed.
        [b"notify"] => None,
        _ => join_params(args).map(|message| SessionEvent::Notification {
            title: None,
            message,
        }),
    }
}

/// Extract the path from an OSC 7 payload: `file://host/path` (or any
/// `file://` URI) or a bare path. Returns `None` when there is no usable
/// path (empty payload, non-`file` URI, host with no path).
fn cwd_from_uri(uri: Option<&[u8]>) -> Option<PathBuf> {
    let uri = uri?;

    let path = match find_subslice(uri, b"://") {
        Some(scheme_end) => {
            if !uri[..scheme_end].eq_ignore_ascii_case(b"file") {
                return None;
            }
            let after_authority = &uri[scheme_end + 3..];
            let slash = after_authority.iter().position(|&b| b == b'/')?;
            &after_authority[slash..]
        }
        None => uri,
    };

    if path.is_empty() {
        return None;
    }

    let decoded = percent_decode(path);
    Some(PathBuf::from(
        String::from_utf8_lossy(&decoded).into_owned(),
    ))
}

/// First index of `needle` inside `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Decode `%XX` percent escapes. Invalid escapes are passed through
/// untouched; this never fails.
fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%' && index + 2 < input.len() {
            if let (Some(high), Some(low)) =
                (hex_value(input[index + 1]), hex_value(input[index + 2]))
            {
                output.push(high << 4 | low);
                index += 3;
                continue;
            }
        }
        output.push(input[index]);
        index += 1;
    }
    output
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
