//! The reply-sanitization pure seam (Spec R04, user stories 5, 6, 28):
//! model text → command bytes. Cooked-mode line discipline makes a
//! newline an execution, so the hard guarantee — no `\n`/`\r` in the
//! output — is enforced at this testable seam rather than by
//! convention, and the frontend can stream the result into the input
//! line knowing it can never self-execute (§5.4).

/// Collapse a model reply to one command line: drop code-fence lines
/// and a leading `$ ` prompt marker, trim, then keep only the text
/// before the first `\n` or `\r`. Whatever a chatty or fenced reply
/// carries, the returned bytes hold no line break — the never-execute
/// guarantee behind user story 5.
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
    text[..end].trim().to_owned()
}

/// Incremental [`sanitize_reply`] for the streaming path (§5.4's
/// "streaming preview"): raw reply fragments go in via
/// [`push`](ReplySanitizer::push), sanitized text safe to append to the
/// input line comes out — guaranteed free of `\n`/`\r` at every chunk
/// boundary, so a multi-line reply can never leak a newline to the
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
                if matches!(byte, '\n' | '\r') {
                    self.state = State::PostFenceLead;
                }
            }
            State::PostFenceLead => match byte {
                '$' => self.state = State::Dollar,
                _ if byte.is_whitespace() => {}
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
                _ if byte.is_whitespace() => {}
                _ => {
                    // A real byte after `"$ "<ws>`: the marker is
                    // stripped and the inner whitespace is trimmed
                    // (`sanitize_reply`'s post-strip `trim`).
                    out.push(byte);
                    self.state = State::Body;
                }
            },
            State::Body => match byte {
                '\n' | '\r' => {
                    self.pending_ws.clear();
                    self.state = State::Truncated;
                }
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
