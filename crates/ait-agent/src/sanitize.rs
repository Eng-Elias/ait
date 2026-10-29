//! The reply-sanitization pure seam (Spec R04, user stories 5, 6, 28):
//! model text → command bytes. Cooked-mode line discipline makes a
//! newline an execution, so the hard guarantee — no `\n`/`\r` in the
//! output — is enforced at this testable seam rather than by
//! convention, and the frontend can stream the result into the input
//! line knowing it can never self-execute (§5.4).

/// Collapse a model reply to one command line: drop code-fence lines
/// and a leading `$ ` prompt marker, trim, then keep only the text
/// before the first `\n` or `\r`, drop every control character
/// (`char::is_control`), and trim again. Whatever a chatty, fenced or
/// hostile reply carries, the returned bytes hold no line break — the
/// never-execute guarantee behind user story 5 — and no control byte
/// either: a reply can never type `\x03` (SIGINT), `\x15`, `\t`
/// (readline completion), `\x7f` or an `\x1b…` escape into the PTY.
pub fn sanitize_reply(reply: &str) -> String {
    let mut text = reply.trim();
    // An opening fence line is ``` plus an optional language tag.
    if text.starts_with("```") {
        text = match text.find('\n') {
            Some(end) => text[end + 1..].trim_start(),
            None => "",
        };
    }
    // A closing fence is a trailing ``` on a line of its own — a bare
    // ``` suffix glued to text ("ls```") is not a fence.
    if let Some(body) = text.strip_suffix("```") {
        if body.is_empty() || body.ends_with('\n') {
            text = body.trim_end();
        }
    }
    let text = text.trim_start();
    let text = text.strip_prefix("$ ").unwrap_or(text);
    let end = text.find(['\n', '\r']).unwrap_or(text.len());
    // Truncate first (a line break ENDS the command — it is never
    // skipped over to reach a second line), then drop control bytes,
    // then trim the whitespace their removal may have exposed.
    text[..end]
        .chars()
        .filter(|byte| !byte.is_control())
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Incremental [`sanitize_reply`] for the streaming path (§5.4's
/// "streaming preview"): raw reply fragments go in via
/// [`push`](ReplySanitizer::push), sanitized text safe to append to the
/// input line comes out — guaranteed free of control bytes
/// (`char::is_control`, line breaks included) at every chunk boundary,
/// so a hostile or garbage reply can never leak a newline, a `\t`
/// completion trigger, a `\x03` SIGINT or an `\x1b…` escape to the
/// frontend mid-stream.
///
/// Sanitization cannot be applied per-delta: `$ ` and ```` ``` ````
/// stripping and whitespace trimming all rewrite the reply's *start*, so
/// a naive "sanitize each fragment" emits bytes the finished result
/// would have dropped (e.g. the `$` of a `"$ ls"` reply arriving as
/// `"$ "` + `"ls"`). Instead this holds each ambiguous prefix — leading
/// whitespace, a possible opening fence, a possible `"$ "` marker and a
/// possibly-trailing whitespace run — until later bytes resolve it. The
/// contract: the concatenation of every `push` result plus
/// [`finish`](ReplySanitizer::finish) equals `sanitize_reply` on the
/// concatenation of all pushed fragments. The streamed command therefore
/// arrives byte-identical to the batch-cleaned one.
#[derive(Debug, Default)]
pub struct ReplySanitizer {
    state: State,
    /// Whitespace held back in [`State::Body`]: emitted only when a
    /// later non-whitespace, non-newline byte proves it is interior —
    /// a trailing run would be trimmed, and a line break truncates
    /// before it.
    pending_ws: String,
}

/// Where the next raw byte lands in the sanitized output. The states
/// mirror `sanitize_reply`'s transformation order: leading trim → one
/// opening-fence line → `"$ "` marker → truncate at first line break →
/// trailing trim.
#[derive(Debug, Default)]
enum State {
    /// Nothing but whitespace seen — the leading-trim zone. The first
    /// non-whitespace byte may still open a ```` ``` ```` fence or a
    /// `"$ "` marker, so nothing is emitted yet.
    #[default]
    Lead,
    /// The first non-whitespace bytes are 1–2 backticks: still able to
    /// grow into an opening ```` ``` ```` fence.
    Ticks(u8),
    /// The reply opens with a ```` ``` ```` fence line — drop bytes to
    /// the first line break (`sanitize_reply` removes the whole first
    /// line whatever follows the fence marker).
    FenceLine,
    /// The opening fence line is consumed and the body hasn't started —
    /// the post-fence `trim_start` zone. A ```` ``` ```` here is *not*
    /// another fence (`sanitize_reply` strips one opening fence only),
    /// but a `"$ "` marker still applies.
    PostFenceLead,
    /// The first body byte was `$`: one more byte decides whether this
    /// is the strippable `"$ "` marker.
    Dollar,
    /// `"$"` followed by whitespace — inside a possibly-strippable
    /// `"$ "` prefix. Emit only the `$` if the stream ends here;
    /// a non-whitespace byte means the whole prefix is stripped.
    DollarWs,
    /// A `"$ "` marker WAS stripped — the post-strip `trim_start` zone.
    /// Like [`PostFenceLead`](State::PostFenceLead) except a second `$`
    /// is plain body text: `sanitize_reply` strips `"$ "` only once.
    StrippedLead,
    /// Emitting body bytes; whitespace rides in `pending_ws`.
    Body,
    /// A line break was seen: `sanitize_reply` truncates at the first
    /// `\n`/`\r`, so every later byte is discarded.
    Truncated,
}

impl ReplySanitizer {
    /// A fresh sanitizer — one per model reply.
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a raw reply fragment; returns sanitized text to append to
    /// the input line (possibly empty — held-back bytes surface later
    /// or never, per the contract in the type docs).
    pub fn push(&mut self, fragment: &str) -> String {
        let mut out = String::new();
        for byte in fragment.chars() {
            self.step(byte, &mut out);
        }
        out
    }

    /// The stream ended: flush whatever the reply's tail commits. In
    /// most states nothing remains; an interrupted `"$ "` marker
    /// resolves to a bare `"$"` (the reply was trimmed before the
    /// strip check, so `"$ "` at end-of-stream is just `"$"`), and a
    /// pending backtick run emits its ticks.
    pub fn finish(&mut self) -> String {
        // A trailing whitespace run is trimmed — never emitted. The
        // state flips to Truncated so a mistaken later `push` emits
        // nothing.
        self.pending_ws.clear();
        match std::mem::replace(&mut self.state, State::Truncated) {
            State::Ticks(ticks) => "`".repeat(ticks as usize),
            State::Dollar | State::DollarWs => String::from("$"),
            _ => String::new(),
        }
    }

    /// Emit `text` held as interior after all — used when a
    /// non-whitespace byte commits a pending run.
    fn flush_ws(&mut self, out: &mut String) {
        out.push_str(&self.pending_ws);
        self.pending_ws.clear();
    }

    /// Advance one raw byte through the state machine.
    fn step(&mut self, byte: char, out: &mut String) {
        match self.state {
            State::Lead => match byte {
                '`' => self.state = State::Ticks(1),
                '$' => self.state = State::Dollar,
                // A line break in the lead zone is leading whitespace —
                // `sanitize_reply` trims the whole reply first, so only
                // a break *after* content truncates.
                _ if byte.is_whitespace() => {}
                // A leading control byte is dropped by the batch filter
                // — but it still counts as the reply's first byte, so a
                // `$ ` or fence after it is body text, not a marker.
                _ if byte.is_control() => self.state = State::Body,
                _ => {
                    out.push(byte);
                    self.state = State::Body;
                }
            },
            State::Ticks(ticks) => match byte {
                '`' if ticks < 2 => self.state = State::Ticks(ticks + 1),
                '`' => self.state = State::FenceLine,
                '\n' | '\r' => {
                    out.push_str(&"`".repeat(ticks as usize));
                    self.state = State::Truncated;
                }
                _ => {
                    out.push_str(&"`".repeat(ticks as usize));
                    self.state = State::Body;
                    self.step(byte, out);
                }
            },
            State::FenceLine => {
                // `sanitize_reply` drops to the first `\n` — a `\r` is
                // just another byte on the fence line, not its end.
                if byte == '\n' {
                    self.state = State::PostFenceLead;
                }
            }
            State::PostFenceLead => match byte {
                '$' => self.state = State::Dollar,
                _ if byte.is_whitespace() => {}
                // As in `Lead`: dropped, but it ends the marker zone.
                _ if byte.is_control() => self.state = State::Body,
                _ => {
                    out.push(byte);
                    self.state = State::Body;
                }
            },
            State::Dollar => match byte {
                ' ' => self.state = State::DollarWs,
                '\n' | '\r' => {
                    out.push('$');
                    self.state = State::Truncated;
                }
                // A control byte can't be emitted, but the `$` commits
                // as body text (`"$\x03"` filters to `"$"`).
                _ if byte.is_control() => {
                    out.push('$');
                    self.state = State::Body;
                }
                _ if byte.is_whitespace() => {
                    // `"$\t…"` is not the `"$ "` marker — the `$` is
                    // committed and the whitespace becomes pending.
                    out.push('$');
                    self.pending_ws.push(byte);
                    self.state = State::Body;
                }
                _ => {
                    out.push('$');
                    out.push(byte);
                    self.state = State::Body;
                }
            },
            State::DollarWs => match byte {
                '\n' | '\r' => {
                    // `"$ "` was stripped, so this line break is the
                    // reply's first byte — the output stays empty.
                    self.state = State::Truncated;
                }
                // More whitespace — control whitespace included — is
                // still inside the marker's trim zone: it may yet prove
                // trailing and re-resolve the prefix to a bare `"$"`.
                _ if byte.is_whitespace() => {}
                // A NON-whitespace control byte resolves the marker:
                // batch strips `"$ "` then drops the byte — continue in
                // the post-strip zone.
                _ if byte.is_control() => self.state = State::StrippedLead,
                _ => {
                    // A real byte after `"$ "<ws>`: the marker is
                    // stripped and the inner whitespace is trimmed
                    // (`sanitize_reply`'s post-strip `trim`).
                    out.push(byte);
                    self.state = State::Body;
                }
            },
            State::StrippedLead => match byte {
                '\n' | '\r' => self.state = State::Truncated,
                // The post-strip trim zone: leading whitespace is
                // trimmed and control bytes are dropped — neither
                // commits body text.
                _ if byte.is_whitespace() || byte.is_control() => {}
                _ => {
                    out.push(byte);
                    self.state = State::Body;
                }
            },
            State::Body => match byte {
                '\n' | '\r' => {
                    self.pending_ws.clear();
                    self.state = State::Truncated;
                }
                // Control bytes — control whitespace like `\t` included
                // — are dropped outright, never emitted and never
                // parked in `pending_ws`.
                _ if byte.is_control() => {}
                _ if byte.is_whitespace() => self.pending_ws.push(byte),
                _ => {
                    self.flush_ws(out);
                    out.push(byte);
                }
            },
            State::Truncated => {}
        }
    }
}
