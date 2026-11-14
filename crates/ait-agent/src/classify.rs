//! The §5.10.4 input classifier's decision half (Spec R11-T1):
//! [`classify`] takes the verified input line plus a [`ProgramIndex`]
//! and returns [`InputClass`] — pure, no PTY, no network
//! (CODING_STANDARDS.md §11).
//!
//! The frontend arms the index once per session spawn (Spec us-15):
//! [`ProgramIndex::harvest`] merges the detected shell's builtin table
//! (`ait_shell::builtins_for`, keyed on the program basename the status
//! line shows) with the executable basenames on `$PATH`.
//!
//! The rule order is the spec's, and it is deliberately conservative —
//! anything a shell could plausibly run stays a command:
//!
//! 1. an empty or single-token line is `Command` — abbreviations,
//!    shell-private aliases and builtins stay reachable;
//! 2. a first word the index resolves (builtin or `$PATH` basename) is
//!    `Command` — `tar my files` never reaches the evidence rules;
//! 3. any *unquoted* shell syntax — pipes, `&&`/`||`, `;`,
//!    redirections, `$(`/backticks, `=`, globs, `-flag` tokens — is
//!    `Command`;
//! 4. positive sentence evidence — a leading question/auxiliary or
//!    request word, an English stopword, or a trailing unquoted
//!    `?`/`.` — is `NaturalLanguage`;
//! 5. everything else is `Command`.

use std::collections::HashSet;
use std::path::Path;

/// What the verified input line is (§5.10.4). The enum leaves room for
/// a later `Uncertain`/model score — the `local_model` variant lands
/// behind a cargo feature — without a signature break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputClass {
    /// Submit verbatim — a plausible shell command.
    Command,
    /// Rewrite to `# <line>` — the line reads as a natural-language
    /// request and takes the §5.4 suggestion flow.
    NaturalLanguage,
}

/// The set of first words a shell executes directly: the detected
/// shell's builtin table merged with the basenames of executables on
/// `$PATH`. Built once per session spawn — Enter must stay instant
/// (Spec us-15). Constructed only via [`ProgramIndex::harvest`] or
/// [`ProgramIndex::with_path_entries`], so an index can never exist
/// without its shell's builtin table.
#[derive(Debug, Clone)]
pub struct ProgramIndex {
    names: HashSet<String>,
    /// First-word resolution is case-insensitive for the Windows
    /// families — PowerShell and cmd resolve `Get-ChildItem`,
    /// `GET-CHILDITEM` and `get-childitem` identically (§5.14), so
    /// both the stored names and each lookup lowercase. POSIX shells
    /// stay exact — `LS` is not a command.
    insensitive: bool,
}

impl ProgramIndex {
    /// Builtins for `shell_name` plus every executable basename
    /// harvestable from `$PATH` — the per-session-spawn construction.
    /// `shell_name` is the session program's basename — the same
    /// string the status line shows; an unrecognized shell still gets
    /// the POSIX builtin set.
    pub fn harvest(shell_name: &str) -> Self {
        Self::with_path_entries(shell_name, path_executables())
    }

    /// Builtins for `shell_name` plus explicit executable names — the
    /// pure-construction seam tests (and callers that harvest `$PATH`
    /// themselves) drive.
    pub fn with_path_entries<E, N>(shell_name: &str, entries: E) -> Self
    where
        E: IntoIterator<Item = N>,
        N: Into<String>,
    {
        // The Windows families read their first words
        // case-insensitively — keyed on the same normalized basename
        // (`pwsh.exe`, `cmd`) the planner and the builtin tables use.
        let insensitive = matches!(
            ait_shell::shell_family(shell_name),
            ait_shell::ShellFamily::PowerShell | ait_shell::ShellFamily::Cmd
        );
        let names = ait_shell::builtins_for(shell_name)
            .map(String::from)
            .chain(entries.into_iter().map(Into::into))
            .map(|name| {
                if insensitive {
                    name.to_lowercase()
                } else {
                    name
                }
            })
            .collect();
        Self { names, insensitive }
    }

    /// `word` resolves to a shell builtin or a `$PATH` executable —
    /// an exact match on the whole word, case-insensitive when the
    /// index's shell family is (§5.14).
    pub fn contains(&self, word: &str) -> bool {
        if self.insensitive {
            self.names.contains(word.to_lowercase().as_str())
        } else {
            self.names.contains(word)
        }
    }
}

/// Executable basenames found on `$PATH` — best-effort: an unreadable
/// directory, a non-UTF-8 name or a vanished entry is skipped, never
/// fatal (§5.5 degrade).
fn path_executables() -> Vec<String> {
    let mut names = Vec::new();
    let Some(path) = std::env::var_os("PATH") else {
        return names;
    };
    // The `PATHEXT` list is a per-process constant — read it once,
    // not per PATH entry.
    #[cfg(not(unix))]
    let pathext = pathext_value();
    for dir in std::env::split_paths(&path) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            #[cfg(unix)]
            let executable = is_executable_file(&entry.path());
            #[cfg(not(unix))]
            let executable = is_executable_file(&entry.path(), &pathext);
            if executable {
                if let Some(name) = entry.file_name().to_str() {
                    push_index_name(&mut names, name);
                }
            }
        }
    }
    names
}

/// unix: the basename as-is — what the file is named is what runs.
#[cfg(unix)]
fn push_index_name(names: &mut Vec<String>, name: &str) {
    names.push(name.to_owned());
}

/// Non-unix: the basename plus its extensionless stem — users type
/// `git`, never `git.exe`; the `PATHEXT` extension is the shell's
/// own resolution detail, so the index holds both spellings.
#[cfg(not(unix))]
fn push_index_name(names: &mut Vec<String>, name: &str) {
    names.push(name.to_owned());
    if let Some((stem, _)) = name.rsplit_once('.') {
        if !stem.is_empty() {
            names.push(stem.to_owned());
        }
    }
}

/// An executable regular file — `std::fs::metadata` traverses the
/// symlinks `$PATH` entries usually are.
#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Non-unix fallback: executability isn't a mode bit, it's the
/// `PATHEXT` extension list cmd/PowerShell resolve through (`.COM`,
/// `.EXE`, `.BAT`, `.CMD` when the var is unset) — without it every
/// DLL and doc on `%PATH%` would resolve as a program. `pathext` is
/// the caller's hoisted [`pathext_value`] — one env read per index
/// build, not per entry.
#[cfg(not(unix))]
fn is_executable_file(path: &Path, pathext: &str) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    std::fs::metadata(path)
        .map(|meta| meta.is_file())
        .unwrap_or(false)
        && in_pathext(name, pathext)
}

/// The `PATHEXT` env var's raw value — the documented default when
/// unset.
#[cfg(not(unix))]
fn pathext_value() -> String {
    std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
}

/// Does `name`'s extension appear in a `PATHEXT`-style `;`-separated
/// list — case-insensitive, entries `.`-prefixed or bare?
#[cfg(not(unix))]
fn in_pathext(name: &str, pathext: &str) -> bool {
    let Some((_, extension)) = name.rsplit_once('.') else {
        return false;
    };
    pathext
        .split(';')
        .filter(|entry| !entry.is_empty())
        .any(|entry| extension.eq_ignore_ascii_case(entry.trim_start_matches('.')))
}

/// One whitespace-delimited word with a per-char literal mask: `true`
/// marks characters the shell would treat as literal — inside quotes
/// or backslash-escaped — so the syntax rules fire only on metachars
/// the shell itself would read. Quote characters and escape
/// backslashes are dropped, so `text` is the word as the shell sees
/// it.
struct Token {
    text: String,
    literal: Vec<bool>,
}

/// Split `line` on *unquoted* whitespace with the POSIX quote state
/// machine: `'` quotes everything to the next `'`; `"` quotes
/// everything (classification doesn't expand); `\` escapes the next
/// char outside single quotes. An unterminated quote swallows the rest
/// of the line into the token — matching the shell's own continuation.
/// One exception to POSIX: a `'` BETWEEN word characters is an
/// apostrophe, not a quote open — `what's`/`don't` stay single tokens
/// with the `'` literal instead of swallowing the rest of the line
/// (a real shell would open a continuation, but classifier-side the
/// contraction reading is the user's intent).
fn tokenize(line: &str) -> Vec<Token> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Bare,
        Single,
        Double,
    }
    let mut tokens = Vec::new();
    let mut state = State::Bare;
    let mut escaped = false;
    let mut text = String::new();
    let mut literal: Vec<bool> = Vec::new();
    let mut has_token = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if escaped {
            text.push(ch);
            literal.push(true);
            escaped = false;
            has_token = true;
            continue;
        }
        match state {
            State::Bare if ch.is_whitespace() => {
                if has_token {
                    tokens.push(Token { text, literal });
                    text = String::new();
                    literal = Vec::new();
                    has_token = false;
                }
            }
            State::Bare => match ch {
                '\\' => escaped = true,
                '\'' if text.chars().last().is_some_and(char::is_alphanumeric)
                    && chars.peek().is_some_and(|next| next.is_alphanumeric()) =>
                {
                    // A mid-word apostrophe — literal text, never a
                    // quote open (marked literal so it can never read
                    // as shell syntax).
                    text.push(ch);
                    literal.push(true);
                }
                '\'' => state = State::Single,
                '"' => state = State::Double,
                _ => {
                    text.push(ch);
                    literal.push(false);
                    has_token = true;
                }
            },
            State::Single => {
                if ch == '\'' {
                    state = State::Bare;
                } else {
                    text.push(ch);
                    literal.push(true);
                    has_token = true;
                }
            }
            State::Double => match ch {
                '"' => state = State::Bare,
                '\\' => escaped = true,
                _ => {
                    text.push(ch);
                    literal.push(true);
                    has_token = true;
                }
            },
        }
    }
    if has_token {
        tokens.push(Token { text, literal });
    }
    tokens
}

/// Leading words marking a request: wh-questions, auxiliaries and the
/// imperative verbs a suggestion request opens with. Compared
/// case-insensitively against the first token.
const SENTENCE_OPENERS: &[&str] = &[
    "who",
    "what",
    "whats",
    "when",
    "where",
    "which",
    "why",
    "how",
    "hows",
    "can",
    "could",
    "should",
    "would",
    "will",
    "shall",
    "do",
    "does",
    "did",
    "is",
    "are",
    "am",
    "was",
    "were",
    "may",
    "might",
    "must",
    "show",
    "list",
    "tell",
    "explain",
    "describe",
    "find",
    "search",
    "give",
    "help",
    "print",
    "display",
    "create",
    "make",
    "delete",
    "remove",
    "undo",
    "redo",
    "check",
    "count",
    "please",
    "write",
    "summarize",
];

/// English stopwords: a token from this list anywhere in the line is
/// positive sentence evidence (§5.10.4). Shell keywords (`in`, `do`,
/// `if`, `and`, `or`, `not`, …) are deliberately absent — those lines
/// resolve through the index or read as commands.
const STOPWORDS: &[&str] = &[
    "a", "all", "an", "any", "are", "every", "i", "is", "me", "my", "our", "please", "some",
    "that", "the", "these", "this", "those", "us", "was", "we", "were", "you", "your",
];

/// Classify the verified input line (§5.10.4, Spec R11): `Command`
/// submits verbatim; `NaturalLanguage` rewrites to `# <line>` for the
/// §5.4 suggestion flow. See the module docs for the rule order — the
/// heuristic resolves every doubt toward `Command`.
pub fn classify(line: &str, index: &ProgramIndex) -> InputClass {
    let tokens = tokenize(line);
    // (1) Empty and single-token lines are always commands.
    if tokens.len() <= 1 {
        return InputClass::Command;
    }
    // (2) A first word the shell resolves directly is a command
    // however the rest reads.
    if index.contains(&tokens[0].text) {
        return InputClass::Command;
    }
    // (3) Any unquoted shell syntax keeps the line a command.
    if tokens.iter().any(has_shell_syntax) {
        return InputClass::Command;
    }
    // (4) Positive sentence evidence — and only positive evidence —
    // flips the verdict.
    if has_sentence_evidence(&tokens) {
        return InputClass::NaturalLanguage;
    }
    // (5) Everything else stays a command.
    InputClass::Command
}

/// The shell metachars rule 3 scans for: pipes and `&&`/`||` (any
/// `&`), `;`, redirections, command substitution (`$(` and backticks —
/// `(` itself is a subshell), `=` assignments, globs, and brace
/// expansions. `?` is deliberately absent — it is the classifier's
/// strongest sentence marker, and a glob-shaped first word resolves
/// at rule 2 anyway.
const SHELL_METACHARS: &[char] = &[
    '|', '&', ';', '>', '<', '`', '=', '*', '[', ']', '(', ')', '{', '}',
];

/// Unquoted shell syntax in one token (rule 3).
fn has_shell_syntax(token: &Token) -> bool {
    // `-x`, `--verbose`: a flag-shaped token is a command argument,
    // never a sentence word.
    if token.text.len() > 1 && token.text.starts_with('-') && !token.literal[0] {
        return true;
    }
    token
        .text
        .chars()
        .zip(&token.literal)
        .any(|(ch, literal)| !*literal && SHELL_METACHARS.contains(&ch))
}

/// Positive sentence evidence (rule 4): a leading question/auxiliary
/// or request word, an English [`STOPWORDS`] entry anywhere, or a
/// trailing unquoted `?`/`.`. A trailing `..` is a path, not a full
/// stop.
fn has_sentence_evidence(tokens: &[Token]) -> bool {
    // Apostrophes normalize out before the opener lookup — `what's`
    // and `how's` hit `whats`/`hows`, the forms the table already
    // carries for their uncontracted spellings.
    let opener: String = tokens[0]
        .text
        .to_lowercase()
        .chars()
        .filter(|ch| *ch != '\'')
        .collect();
    if SENTENCE_OPENERS.contains(&opener.as_str()) {
        return true;
    }
    if tokens
        .iter()
        .any(|token| STOPWORDS.contains(&token.text.to_lowercase().as_str()))
    {
        return true;
    }
    // The last unquoted char of the last token — a `?` or `.` the
    // shell would actually see closes a sentence.
    let last = &tokens[tokens.len() - 1];
    let trailing = last
        .text
        .chars()
        .zip(&last.literal)
        .last()
        .and_then(|(ch, literal)| (!*literal).then_some(ch));
    match trailing {
        Some('?') => true,
        Some('.') => {
            // `..` is a path, not a full stop.
            let bare: String = last
                .text
                .chars()
                .zip(&last.literal)
                .filter(|(_, literal)| !*literal)
                .map(|(ch, _)| ch)
                .collect();
            !bare.ends_with("..")
        }
        _ => false,
    }
}
