//! The Ctrl+click affordance's detection half (Spec R14 §5.13, ticket
//! R14-T4): a pure map from one grid row's text, the tab's cwd and the
//! filesystem to the token spans that afford a route — `https?://`/
//! `file://` URLs and path-shaped tokens (absolute, `~/`, and
//! relative names that resolve to an EXISTING entry under the tab's
//! cwd — a nonexistent relative never affords).
//!
//! [`tokens`] is the line seam the spec assigns: text + cwd + home in,
//! [`Token`] spans out (char indices into the scanned text). The
//! position half — "the token under this (line, column)" — is
//! [`detect_at`]: it rebuilds the row's text→column map (`Row::text`
//! drops wide-char spacer cells and appends each cell's zero-width
//! riders, so char index ≠ column), runs the line seam and returns
//! the hit's CELL span for the hover underline plus its [`Target`]
//! for the click route. The remote gate is the caller's one flag: a
//! remote tab's cwd names a REMOTE dir the local fs cannot resolve,
//! so `remote` scans URLs only.
//!
//! The known spec gap stays known: OSC-8 hyperlinks whose display
//! text isn't the URI are undetectable — `ait_term::Cell` drops the
//! URI and stays `Copy`, so only what the row actually shows can
//! ever afford.

use std::path::{Path, PathBuf};

#[cfg(test)]
use ait_term::GridView;
use ait_term::{CellKind, Row};

/// What a detected token routes to — the Ctrl+click targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// An `https?://`/`file://` URL — the browser pane's navigation
    /// (embedded, or the external-open surface on the fallback).
    Url(String),
    /// A filesystem path that exists, absolute after `~`-expansion —
    /// the explorer's select+preview (files) or navigate (dirs).
    Path(PathBuf),
}

/// One detected token: its char-index span inside the scanned line
/// plus its route target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// First char index (into the scanned text) of the token.
    pub start: usize,
    /// One past the last char index — `text`'s chars `start..end`.
    pub end: usize,
    /// Where Ctrl+click sends it.
    pub target: Target,
}

/// A Ctrl+hover/click hit under one column: the token's cell span
/// (the underline overlay's shape) plus its route target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// The first covered column — where the underline starts.
    pub column: usize,
    /// How many cells the token covers — the underline's width.
    pub cells: usize,
    /// The route target.
    pub target: Target,
}

/// One Ctrl+hover position's detection inputs — the CHEAP half, the
/// cache key. The expensive half (the `cwd_for`/`home_dir` answers)
/// isn't here: [`DetectCache::resolve`] takes it as a closure it
/// calls only on a miss, so a held Ctrl at an unchanged position
/// never reads `/proc` or the env at all.
pub struct HoverAt<'a> {
    /// The hovered tab's id — the same (line, column, text) on
    /// another tab is another position.
    pub tab: u64,
    /// The pointer's grid line (negative in scrollback).
    pub line: i32,
    /// The pointer's cell column.
    pub column: usize,
    /// The row's text — `line_cells`'s first half.
    pub text: &'a str,
    /// `text`'s char→column map — `line_cells`'s second half.
    pub cols: &'a [usize],
    /// The remote gate `detect_in` reads.
    pub remote: bool,
}

/// `detect_in` memoized on its cheap inputs — the Ctrl+hover
/// affordance's §6 guard: the filesystem probes (`exists()` per
/// path-shaped candidate) run once per hover position+content, not
/// once per frame while Ctrl is held, and even the `cwd_for`/`home`
/// environment answers evaluate only on a miss (the caller's `env`
/// closure). A mouse move, a scroll or new output on the row lands a
/// new key and re-probes; a `cd`, or a file created/deleted under an
/// UNCHANGED hover, keeps the stale verdict until one of those
/// moves — the accepted trade for never probing per frame (a `cd`'s
/// first cue is usually new prompt text on the row anyway, which is
/// itself a new key).
#[derive(Debug, Default)]
pub struct DetectCache {
    key: Option<OwnedKey>,
    hit: Option<Detection>,
}

/// The remembered inputs — owned copies of `HoverAt`'s borrows, so
/// the cache outlives the frame's row text.
#[derive(Debug)]
struct OwnedKey {
    /// `HoverAt::tab`.
    tab: u64,
    /// `HoverAt::line`.
    line: i32,
    /// `HoverAt::column`.
    column: usize,
    /// `HoverAt::remote`.
    remote: bool,
    /// `HoverAt::text`.
    text: String,
}

impl DetectCache {
    /// `detect_in` on `at`, re-probing only when a keyed input moved —
    /// identical inputs return the remembered verdict without
    /// touching the filesystem OR `env` (and, per the struct doc,
    /// possibly a stale one). `env` supplies the expensive half —
    /// the tab's `cwd` and the `~` root — and runs only on a miss.
    pub fn resolve(
        &mut self,
        at: &HoverAt<'_>,
        env: impl FnOnce() -> (PathBuf, PathBuf),
    ) -> Option<Detection> {
        let remembered = self.key.as_ref().is_some_and(|key| {
            key.tab == at.tab
                && key.line == at.line
                && key.column == at.column
                && key.remote == at.remote
                && key.text == at.text
        });
        if remembered {
            return self.hit.clone();
        }
        let (cwd, home) = env();
        let hit = detect_in(at.text, at.cols, at.column, at.remote, &cwd, &home);
        self.key = Some(OwnedKey {
            tab: at.tab,
            line: at.line,
            column: at.column,
            remote: at.remote,
            text: at.text.to_owned(),
        });
        self.hit = hit.clone();
        hit
    }
}

/// `text` → the tokens that afford a route: URLs first (their spans
/// are claimed before the path pass runs, so a `//x` inside a URL
/// never starts a path candidate), then path-shaped tokens resolved
/// against `cwd` (relatives — `./`/`../` included), absolute forms
/// standing alone (`/` — plus `X:\`, `\\server\share` and `\rooted`
/// under the Windows grammar, §5.14), and `~` under `home`. Remote
/// tabs call [`url_tokens`] instead — `detect_at`'s `remote` flag
/// already picks between the two.
pub fn tokens(text: &str, cwd: &Path, home: &Path) -> Vec<Token> {
    tokens_on(text, cwd, home, cfg!(windows), Path::exists)
}

/// [`tokens`] with the platform's path grammar and the filesystem
/// probe named — the testable seam (§5.14 / R16-T7): `windows` adds
/// `\` to the run alphabet plus the `X:\`, `\\server\share` and `~\`
/// forms Windows terminal text carries, and `exists` stands in for
/// the fs so the Windows grammar resolves off-Windows (no fixture
/// needs a literal `C:` file name). Production callers bake both:
/// `cfg!(windows)` and [`Path::exists`].
fn tokens_on(
    text: &str,
    cwd: &Path,
    home: &Path,
    windows: bool,
    exists: impl Fn(&Path) -> bool,
) -> Vec<Token> {
    let chars: Vec<char> = text.chars().collect();
    let mut found = url_scan(&chars);
    // A claimed URL span can never host a path candidate — a `//`
    // inside one is part of the URL, not an absolute path start.
    let mut claimed = vec![false; chars.len()];
    for url in &found {
        claimed[url.start..url.end].fill(true);
    }
    let mut i = 0;
    while i < chars.len() {
        if !is_path_char(chars[i], windows) || claimed[i] {
            i += 1;
            continue;
        }
        let start = i;
        // A drive-letter absolute opens `X:` + a separator — but `:`
        // is no path char (`file:line` must keep cutting there), so
        // the Windows grammar consumes the `X:` head explicitly and
        // lets the run resume on the separator.
        if windows
            && chars[i].is_ascii_alphabetic()
            && chars.get(i + 1) == Some(&':')
            && matches!(chars.get(i + 2), Some('\\') | Some('/'))
        {
            i += 2;
        }
        while i < chars.len() && is_path_char(chars[i], windows) && !claimed[i] {
            i += 1;
        }
        if let Some((len, path)) = resolve_candidate(&chars[start..i], cwd, home, windows, &exists)
        {
            found.push(Token {
                start,
                end: start + len,
                target: Target::Path(path),
            });
        }
    }
    found.sort_by_key(|token| token.start);
    found
}

/// `text` → just the URL tokens — the remote-tab scan (a remote cwd
/// names a dir the local fs can't resolve, so paths never afford
/// there) and `tokens`'s first pass.
pub fn url_tokens(text: &str) -> Vec<Token> {
    let chars: Vec<char> = text.chars().collect();
    url_scan(&chars)
}

/// The token under `column` on grid `line`, if any — the uncached
/// `line_cells` + [`detect_in`] composition the tests drive (the live
/// affordance goes through [`DetectCache`], which memoizes the same
/// inputs so the fs probes don't run per frame). `remote` restricts
/// the scan to URLs: a remote session's paths name remote entries the
/// local fs can't resolve, so they never afford (the explorer's own
/// marker rule, from the other side). `cwd` is the tab's
/// `suggest::cwd_for` answer, `home` the `~` expansion root.
#[cfg(test)]
pub fn detect_at(
    grid: &GridView<'_>,
    line: i32,
    column: usize,
    remote: bool,
    cwd: &Path,
    home: &Path,
) -> Option<Detection> {
    let row = grid.line(line)?;
    let (text, cols) = line_cells(&row);
    detect_in(&text, &cols, column, remote, cwd, home)
}

/// `detect_at` with the row already split — the pure half the unit
/// tests drive without a live grid: `cols[i]` is the column char
/// `i` of `text` paints on (identity for ASCII rows).
pub fn detect_in(
    text: &str,
    cols: &[usize],
    column: usize,
    remote: bool,
    cwd: &Path,
    home: &Path,
) -> Option<Detection> {
    let index = cols.iter().position(|&col| col == column)?;
    let all = if remote {
        url_tokens(text)
    } else {
        tokens(text, cwd, home)
    };
    let token = all
        .iter()
        .find(|token| token.start <= index && index < token.end)?;
    let first = *cols.get(token.start)?;
    let last = *cols.get(token.end - 1)?;
    Some(Detection {
        column: first,
        cells: last - first + 1,
        target: token.target.clone(),
    })
}

/// `row` → its `Row::text` plus, per text char, the grid column it
/// paints on. `Row::text` skips wide-char spacer cells (a wide glyph
/// costs one char but two columns) and appends each cell's zero-width
/// riders (combining marks cost a char but no column), so char index
/// ≠ column the moment either appears — this rebuilds both halves of
/// that contract, and every text char lands on SOME column.
pub fn line_cells(row: &Row<'_>) -> (String, Vec<usize>) {
    let text = row.text();
    let cells: Vec<(usize, char)> = row
        .cells()
        .enumerate()
        .filter(|(_, cell)| cell.kind != CellKind::WideSpacer)
        .map(|(column, cell)| (column, cell.character))
        .collect();
    let mut cols = Vec::with_capacity(text.chars().count());
    let mut chars = text.chars().peekable();
    for (index, &(column, base)) in cells.iter().enumerate() {
        if chars.next() != Some(base) {
            // The cell walk and `Row::text` disagree — hand back the
            // partial map; lookups past it simply don't afford.
            return (text, cols);
        }
        cols.push(column);
        // A cell's zero-width riders share its column — consume until
        // the next cell's base char. A rider that coincides with it
        // (a real ambiguity `Row::text` itself can't settle) lands on
        // the next column instead — a one-cell underline shift.
        let next = cells.get(index + 1).map(|&(_, base)| base);
        while let Some(&rider) = chars.peek() {
            if Some(rider) == next {
                break;
            }
            cols.push(column);
            chars.next();
        }
    }
    (text, cols)
}

/// The schemes that afford as URLs — the spec's `https?://`/`file://`
/// set. `https` can't false-match `http`'s prefix (the `s` is where
/// `http`'s `:` would sit), so order never matters.
const SCHEMES: [&str; 3] = ["https://", "http://", "file://"];

/// `chars` → every URL token, in order — each a `SCHEMES` match at a
/// token boundary (nothing alphanumeric immediately before, so
/// `xhttps://a` is nobody's URL), the body consumed as URL chars and
/// the prose tail trimmed back off.
fn url_scan(chars: &[char]) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let Some(scheme_len) = scheme_len(&chars[i..]) else {
            i += 1;
            continue;
        };
        if i > 0 && chars[i - 1].is_alphanumeric() {
            i += 1;
            continue;
        }
        let body = i + scheme_len;
        let mut end = body;
        while end < chars.len() && is_url_char(chars[end]) {
            end += 1;
        }
        let end = trim_url_end(chars, body, end);
        // A bare scheme (`https://` and nothing after) affords nothing.
        if end > body {
            let url: String = chars[i..end].iter().collect();
            tokens.push(Token {
                start: i,
                end,
                target: Target::Url(url),
            });
        }
        i = end.max(i + 1);
    }
    tokens
}

/// `chars`'s leading scheme length, if it starts with one of
/// [`SCHEMES`]. The schemes are ASCII, so the compare runs byte-wise
/// against each scalar's low value — no per-position `Vec<char>`
/// collection (the scan calls this once per char).
fn scheme_len(chars: &[char]) -> Option<usize> {
    SCHEMES.iter().find_map(|scheme| {
        let matches = chars.len() >= scheme.len()
            && scheme
                .bytes()
                .zip(chars.iter())
                .all(|(byte, c)| u32::from(byte) == *c as u32);
        matches.then_some(scheme.len())
    })
}

/// A URL body's char set — maximal-munch over everything that can't
/// delimit it: whitespace and control chars end the token, and the
/// quote/angle/backtick/pipe/backslash set delimits prose (parens,
/// commas and `?`/`&`/`#` stay IN — `trim_url_end` owns the tail).
fn is_url_char(c: char) -> bool {
    !c.is_whitespace() && !c.is_control() && !matches!(c, '"' | '\'' | '<' | '>' | '`' | '|' | '\\')
}

/// `chars[body..end]` is a URL's would-be body — strip the prose
/// tail: sentence punctuation a URL never ends on, and `)]}` that
/// close nothing the URL itself opened (a balanced `Foo_(bar)` keeps
/// its parens).
fn trim_url_end(chars: &[char], body: usize, mut end: usize) -> usize {
    while end > body {
        let last = chars[end - 1];
        let strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' => true,
            close @ (')' | ']' | '}') => {
                let open = match close {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                let count = |needle| chars[body..end].iter().filter(|&&c| c == needle).count();
                count(close) > count(open)
            }
            _ => false,
        };
        if !strip {
            break;
        }
        end -= 1;
    }
    end
}

/// The characters a path token may hold — conservative on purpose:
/// `:` ends a token so `file.rs:12` affords `file.rs`, `=`, quotes
/// and brackets delimit, and whitespace splits (escaped-space names
/// don't afford — a documented conservatism, not a bug). `windows`
/// adds `\` — the separator `C:\…`, `\\server\share` and `dir\file`
/// all need (the drive colon itself is no path char; the scan
/// consumes `X:` explicitly). Unix keeps `\` a delimiter: a `\` in a
/// unix file name is too rare to pay the `a\b` false-join.
fn is_path_char(c: char, windows: bool) -> bool {
    c.is_alphanumeric()
        || matches!(c, '.' | '_' | '~' | '/' | '-' | '+' | '%' | '@')
        || (windows && c == '\\')
}

/// One path-shaped run → its resolved target and the trimmed length
/// the token covers, if `exists` says it's there: [`resolve_form`]
/// maps the candidate (absolute forms stand alone, `~` joins `home`,
/// the rest join `cwd`). The probe trims trailing non-name characters
/// one at a time (`main.rs.` affords `main.rs`) but never mid-run —
/// `foo.txt` doesn't fall back to `foo`, and a run with no name
/// character (`//`, `…`, `~/` bare) is punctuation, not a path.
fn resolve_candidate(
    run: &[char],
    cwd: &Path,
    home: &Path,
    windows: bool,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<(usize, PathBuf)> {
    if !run.iter().any(|c| c.is_alphanumeric()) {
        return None;
    }
    let mut len = run.len();
    loop {
        let candidate: String = run[..len].iter().collect();
        let resolved = resolve_form(&candidate, cwd, home, windows);
        if exists(&resolved) {
            return Some((len, resolved));
        }
        if len == 0 || run[len - 1].is_alphanumeric() {
            return None;
        }
        len -= 1;
    }
}

/// `candidate` → the path `exists` is asked about: `~` forms join
/// `home`, [`is_absolute`] forms stand alone, everything else joins
/// the tab's `cwd`.
fn resolve_form(candidate: &str, cwd: &Path, home: &Path, windows: bool) -> PathBuf {
    if let Some(rest) = tilde_rest(candidate, windows) {
        home.join(rest)
    } else if is_absolute(candidate, windows) {
        PathBuf::from(candidate)
    } else {
        cwd.join(candidate)
    }
}

/// The `~`-expansion's rest — `~/x` everywhere, `~\x` too under the
/// Windows grammar (the separator a PowerShell user types).
fn tilde_rest(candidate: &str, windows: bool) -> Option<&str> {
    candidate.strip_prefix("~/").or_else(|| {
        if windows {
            candidate.strip_prefix("~\\")
        } else {
            None
        }
    })
}

/// Whether `candidate` is already absolute — `/…` everywhere; the
/// Windows grammar adds `X:\`/`X:/` drive paths, `\\…` (UNC and
/// `\\?\`-style device paths) and a lone leading `\` (rooted on the
/// current drive). `X:foo` with no separator is the legacy
/// drive-relative form — deliberately NOT absolute.
fn is_absolute(candidate: &str, windows: bool) -> bool {
    if candidate.starts_with('/') {
        return true;
    }
    if !windows {
        return false;
    }
    let drive = matches!(
        candidate.as_bytes(),
        [drive, b':', b'\\' | b'/', ..] if drive.is_ascii_alphabetic()
    );
    drive || candidate.starts_with('\\')
}

#[cfg(test)]
mod tests {
    //! The detector at its own seam (Spec R14 §5.13's testing
    //! decisions — "text + cwd + fs probe → spans"): fixture lines
    //! over `tempfile` directories for the path cases, plain strings
    //! for URLs, and the negative specimens CODING_STANDARDS §18
    //! asks an over-match-prone scanner to ship.

    use super::*;

    /// The tempfile fixture every path test roots at:
    /// `sub/` a dir, `inner.txt` inside it, `main.rs` and `file.txt`
    /// files, `.hidden` a dotfile.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("inner.txt"), "x\n").unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("file.txt"), "x\n").unwrap();
        std::fs::write(dir.path().join(".hidden"), "x\n").unwrap();
        dir
    }

    /// The text one token covers — spans are char indices into the
    /// scanned line.
    fn covered(text: &str, token: &Token) -> String {
        text.chars()
            .skip(token.start)
            .take(token.end - token.start)
            .collect()
    }

    /// The URL targets found in `text` — the common shape of "exactly
    /// this list of URLs affords".
    fn urls(text: &str) -> Vec<String> {
        url_tokens(text)
            .iter()
            .map(|token| match &token.target {
                Target::Url(url) => url.clone(),
                Target::Path(_) => panic!("a URL scan produced a path"),
            })
            .collect()
    }

    /// The path targets found in `text` under `cwd`/`home`.
    fn paths(text: &str, cwd: &Path, home: &Path) -> Vec<PathBuf> {
        tokens(text, cwd, home)
            .iter()
            .filter_map(|token| match &token.target {
                Target::Path(path) => Some(path.clone()),
                Target::Url(_) => None,
            })
            .collect()
    }

    // ---- URLs ----

    /// `https?://` and `file://` URLs afford — the token covers
    /// exactly the URL, wherever it sits in the line.
    #[test]
    fn schemed_urls_afford() {
        let found = url_tokens("visit https://example.com/x now");
        assert_eq!(found.len(), 1);
        assert_eq!(
            covered("visit https://example.com/x now", &found[0]),
            "https://example.com/x"
        );
        assert_eq!(
            urls("a http://b.c/d e file:///etc/passwd f"),
            ["http://b.c/d", "file:///etc/passwd",]
        );
    }

    /// URLs inside quotes, parens, angle brackets and trailing prose
    /// punctuation strip their wrappers — the token is the URL, not
    /// the sentence around it.
    #[test]
    fn quoted_and_punctuated_urls_strip_the_wrapper() {
        assert_eq!(urls("(https://a/b)."), ["https://a/b"]);
        assert_eq!(urls("\"https://a/x,\""), ["https://a/x"]);
        assert_eq!(urls("'https://a'"), ["https://a"]);
        assert_eq!(urls("<https://a/b>"), ["https://a/b"]);
        assert_eq!(urls("see https://a/b?x=1, then"), ["https://a/b?x=1"]);
        assert_eq!(urls("https://a/seg);"), ["https://a/seg"]);
    }

    /// A URL's own balanced parens survive trimming — Wikipedia-style
    /// `Foo_(bar)` links stay whole while an unbalanced closer still
    /// drops.
    #[test]
    fn balanced_parens_inside_a_url_survive() {
        assert_eq!(
            urls("https://en.wikipedia.org/wiki/Foo_(bar) end"),
            ["https://en.wikipedia.org/wiki/Foo_(bar)"]
        );
        assert_eq!(urls("https://a/x))"), ["https://a/x"]);
    }

    /// The negative specimens: a bare scheme is no URL, a scheme
    /// embedded in a longer token is no URL start, and schemeless
    /// host text never affords as a URL.
    #[test]
    fn url_near_misses_never_afford() {
        assert!(urls("https://").is_empty(), "a bare scheme is nothing");
        assert!(
            urls("xhttps://a").is_empty(),
            "mid-token scheme is no boundary"
        );
        assert!(
            urls("example.com www.a.b").is_empty(),
            "schemeless hosts aren't URLs"
        );
        assert!(urls("http//a missing colon").is_empty());
    }

    // ---- Paths ----

    /// Absolute paths afford and resolve verbatim — a dir and a file
    /// both produce their own path target.
    #[test]
    fn absolute_paths_resolve_verbatim() {
        let fixture = fixture();
        let file = fixture.path().join("main.rs");
        let dir = fixture.path().join("sub");
        let text = format!("open {} then {}", file.display(), dir.display());
        assert_eq!(paths(&text, fixture.path(), fixture.path()), [file, dir]);
    }

    /// `~/` expands under the injected home — `~/main.rs` and
    /// `~/sub/inner.txt` resolve while `~/ghost` never does.
    #[test]
    fn tilde_paths_expand_under_home() {
        let fixture = fixture();
        let found = paths(
            "cat ~/main.rs ~/sub/inner.txt",
            fixture.path(),
            fixture.path(),
        );
        assert_eq!(
            found,
            [
                fixture.path().join("main.rs"),
                fixture.path().join("sub").join("inner.txt"),
            ]
        );
        assert!(paths("ls ~/ghost", fixture.path(), fixture.path()).is_empty());
    }

    /// Relatives resolve under the tab's cwd: bare filenames, `./`
    /// forms and nested names — and `file:line` compiler output
    /// affords just the file (the `:` ends the token).
    #[test]
    fn relative_paths_resolve_under_the_tab_cwd() {
        let fixture = fixture();
        let found = paths(
            "edit main.rs ./file.txt sub/inner.txt missing.rs",
            fixture.path(),
            fixture.path(),
        );
        assert_eq!(
            found,
            [
                fixture.path().join("main.rs"),
                fixture.path().join("file.txt"),
                fixture.path().join("sub").join("inner.txt"),
            ]
        );
        // `main.rs:12` affords the file — the span stops before `:`.
        let abs = fixture.path().join("main.rs").display().to_string();
        let line = format!("{abs}:12 broke");
        let found = tokens(&line, fixture.path(), fixture.path());
        assert_eq!(found.len(), 1);
        assert_eq!(covered(&line, &found[0]), abs);
    }

    /// The over-match specimens: nonexistent relatives, absolute and
    /// `~/` misses, plain words and number/dotted runs — none afford.
    #[test]
    fn nonexistent_paths_and_non_paths_never_afford() {
        let fixture = fixture();
        for text in [
            "missing.rs no/such/dir",
            "/nonexistent/abs/path",
            "hello world 1.2.3 v0.14",
            "example.com not-a-scheme",
            "fn main() { let x = 1; }",
            "column:value key=value",
        ] {
            assert!(
                tokens(text, fixture.path(), fixture.path()).is_empty(),
                "{text:?} produced a token"
            );
        }
    }

    /// Prose punctuation after a real file still affords the file —
    /// `main.rs.` resolves `main.rs` and the span stops before `.`.
    #[test]
    fn trailing_punctuation_trims_to_the_existing_path() {
        let fixture = fixture();
        let text = format!("edit {}. done", fixture.path().join("main.rs").display());
        let found = tokens(&text, fixture.path(), fixture.path());
        assert_eq!(found.len(), 1);
        assert_eq!(
            covered(&text, &found[0]),
            fixture.path().join("main.rs").display().to_string()
        );
    }

    /// A URL's span is claimed before the path pass — `//` inside a
    /// URL never starts a bogus absolute path and the URL's tail
    /// never lists as a relative one.
    #[test]
    fn url_spans_claim_their_text_against_the_path_pass() {
        let fixture = fixture();
        let found = tokens(
            &format!("see https://a{}/b", fixture.path().display()),
            fixture.path(),
            fixture.path(),
        );
        assert_eq!(found.len(), 1);
        assert!(matches!(found[0].target, Target::Url(_)));
    }

    // ---- Windows paths (§5.14 / R16-T7) ----
    //
    // The platform grammar + fs probe are `tokens_on`'s parameters —
    // a Windows session's `C:\`, `\\share` and `dir\file` tokens all
    // resolve off-Windows here, the `existing` list standing in for
    // the filesystem (PathBuf equality is literal on Linux, which is
    // exactly what the oracle needs).

    /// The path targets found under the WINDOWS grammar — `exists`
    /// answers for the paths `existing` lists, nothing else.
    fn win_paths(text: &str, existing: &[&str], cwd: &Path, home: &Path) -> Vec<PathBuf> {
        tokens_on(text, cwd, home, true, |path| {
            existing.iter().any(|known| Path::new(known) == path)
        })
        .iter()
        .filter_map(|token| match &token.target {
            Target::Path(path) => Some(path.clone()),
            Target::Url(_) => None,
        })
        .collect()
    }

    /// `X:\…` and `X:/…` afford as absolute paths — the token covers
    /// the whole run, drive colon included.
    #[test]
    fn windows_drive_paths_afford_absolutely() {
        let cwd = Path::new("/cwd");
        for text in ["open C:\\work\\main.rs now", "open D:/work/main.rs now"] {
            let target = text.split_whitespace().nth(1).unwrap();
            let found = tokens_on(text, cwd, cwd, true, |path| Path::new(target) == path);
            assert_eq!(found.len(), 1, "{text:?}");
            assert_eq!(covered(text, &found[0]), target);
            assert_eq!(found[0].target, Target::Path(PathBuf::from(target)));
        }
    }

    /// A `\\server\share` UNC path is absolute — never joined under
    /// the tab's cwd.
    #[test]
    fn windows_unc_paths_afford_absolutely() {
        let cwd = Path::new("/cwd");
        let text = "copy \\\\server\\share\\dir\\f.txt done";
        let found = tokens_on(text, cwd, cwd, true, |path| {
            Path::new("\\\\server\\share\\dir\\f.txt") == path
        });
        assert_eq!(found.len(), 1, "{text:?}");
        assert_eq!(covered(text, &found[0]), "\\\\server\\share\\dir\\f.txt");
        assert_eq!(
            found[0].target,
            Target::Path(PathBuf::from("\\\\server\\share\\dir\\f.txt"))
        );
    }

    /// `~\x` joins the injected home like `~/x` does; a relative
    /// `dir\file` joins the tab's cwd.
    #[test]
    fn windows_tilde_and_relative_backslash_forms_resolve() {
        let home = Path::new("/home/me");
        let cwd = Path::new("/work/proj");
        let in_home = home.join("Docs\\a.txt").display().to_string();
        let in_cwd = cwd.join("src\\b.rs").display().to_string();
        let found = win_paths(
            "open ~\\Docs\\a.txt and src\\b.rs",
            &[&in_home, &in_cwd],
            cwd,
            home,
        );
        assert_eq!(found, [home.join("Docs\\a.txt"), cwd.join("src\\b.rs")]);
    }

    /// `file:line` keeps its meaning under the Windows grammar — the
    /// token stops at the SECOND colon (the drive's is the path's
    /// own), and `C:foo` without a separator is no drive path.
    #[test]
    fn windows_file_line_and_bare_drive_colon_stay_conservative() {
        let cwd = Path::new("/cwd");
        let found = tokens_on("err C:\\work\\main.rs:12 bad", cwd, cwd, true, |path| {
            Path::new("C:\\work\\main.rs") == path
        });
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].target,
            Target::Path(PathBuf::from("C:\\work\\main.rs"))
        );
        // `C:foo` is the legacy drive-relative form — unhandled by
        // design; `foo` joins cwd like any relative name.
        let in_cwd = cwd.join("foo").display().to_string();
        let found = win_paths("cat C:foo", &[&in_cwd], cwd, cwd);
        assert_eq!(found, [cwd.join("foo")]);
    }

    /// The unix grammar is untouched: `\` still splits tokens, so a
    /// `C:\…` read on Linux never gloms into one candidate — every
    /// probe lands on a `\`-free fragment (`C`, `work`, `main.rs`).
    #[test]
    fn backslash_splits_tokens_under_the_unix_grammar() {
        let cwd = Path::new("/cwd");
        let text = "open C:\\work\\main.rs now";
        // An always-true oracle still can't yield a `:`/`\`-carrying
        // token: the scanner never merges them under unix rules.
        let found = tokens_on(text, cwd, cwd, false, |_| true);
        assert!(!found.is_empty(), "candidates did probe");
        for token in &found {
            let covered = covered(text, token);
            assert!(
                !covered.contains([':', '\\']),
                "unix grammar merged a Windows run: {covered:?}"
            );
        }
    }

    // ---- The position seam ----

    /// `detect_at`'s text half — `detect_in` — picks the token under
    /// the column: the covered span lands on the token's cells and
    /// whitespace/non-token columns hit nothing.
    #[test]
    fn the_column_under_the_pointer_selects_the_token() {
        let fixture = fixture();
        let text = format!(
            "go https://a/b or {}",
            fixture.path().join("main.rs").display()
        );
        // Identity map: ASCII rows paint one char per column.
        let cols: Vec<usize> = (0..text.chars().count()).collect();
        let url_col = text.find("https").unwrap();
        let hit = detect_in(&text, &cols, url_col, false, fixture.path(), fixture.path())
            .expect("the URL under the pointer affords");
        assert_eq!(hit.target, Target::Url("https://a/b".to_string()));
        assert_eq!(hit.column, url_col);
        assert_eq!(hit.cells, "https://a/b".len());

        let file_col = text.find("main.rs").unwrap();
        let hit = detect_in(
            &text,
            &cols,
            file_col,
            false,
            fixture.path(),
            fixture.path(),
        )
        .expect("the existing file affords");
        assert_eq!(hit.target, Target::Path(fixture.path().join("main.rs")));
        // The token's last column hits the same token; the leading
        // column (a space) and a blank line afford nothing.
        let hit = detect_in(
            &text,
            &cols,
            text.len() - 1,
            false,
            fixture.path(),
            fixture.path(),
        );
        assert!(hit.is_some());
        assert!(detect_in(&text, &cols, 0, false, fixture.path(), fixture.path()).is_none());
        assert!(detect_in("   ", &[0, 1, 2], 1, false, fixture.path(), fixture.path()).is_none());
    }

    /// The remote gate: URLs still afford on a remote tab, paths
    /// never do — the local disk is never resolved under a remote
    /// cwd, mirroring the explorer's marker rule.
    #[test]
    fn remote_tabs_scan_urls_only() {
        let fixture = fixture();
        let text = format!(
            "go https://a/b or {}",
            fixture.path().join("main.rs").display()
        );
        let cols: Vec<usize> = (0..text.chars().count()).collect();
        let url_col = text.find("https").unwrap();
        let file_col = text.find("main.rs").unwrap();
        assert!(detect_in(&text, &cols, url_col, true, fixture.path(), fixture.path()).is_some());
        assert!(detect_in(&text, &cols, file_col, true, fixture.path(), fixture.path()).is_none());
    }

    // ---- The §6 cache ----

    /// `DetectCache` re-probes only when a keyed input moves — proven
    /// against the filesystem itself: a file created after a miss is
    /// invisible to the SAME key (no second `exists()` ran, and `env`
    /// isn't even called) but found the moment the column or the row
    /// text changes.
    #[test]
    fn the_cache_remembers_a_position_until_an_input_moves() {
        let fixture = fixture();
        let cwd = fixture.path().join("sub2");
        std::fs::create_dir(&cwd).unwrap();
        let text = "open newfile.txt now";
        let cols: Vec<usize> = (0..text.chars().count()).collect();
        let column = text.find("newfile.txt").unwrap();
        let mut cache = DetectCache::default();
        let at = |column| HoverAt {
            tab: 0,
            line: -1,
            column,
            text,
            cols: &cols,
            remote: false,
        };
        // The expensive half as a probe — `env` must never run on a
        // cache hit.
        let env_calls = std::cell::Cell::new(0);
        let env = || {
            env_calls.set(env_calls.get() + 1);
            (cwd.clone(), cwd.clone())
        };

        assert!(
            cache.resolve(&at(column), env).is_none(),
            "no file → no afford"
        );
        assert_eq!(env_calls.get(), 1, "the miss evaluated the env");
        std::fs::write(cwd.join("newfile.txt"), "x\n").unwrap();
        assert!(
            cache.resolve(&at(column), env).is_none(),
            "the same key never re-probed — the new file is invisible"
        );
        assert_eq!(env_calls.get(), 1, "a hit never even asks the env");
        // A moved pointer is a new key — the probe runs and finds it.
        assert!(
            cache.resolve(&at(column + 1), env).is_some(),
            "a new position re-probes"
        );
        assert_eq!(env_calls.get(), 2);
        // A changed row re-probes too.
        let mut moved = at(column);
        moved.text = "open newfile.txt again";
        assert!(cache.resolve(&moved, env).is_some());
        assert_eq!(env_calls.get(), 3);
        // …and a `cd` between misses: the env re-reads, the same text
        // misses under the other root (the stale-verdict trade is the
        // key staying cheap — a cd's own prompt redraw usually moves
        // the text anyway).
        moved.text = text;
        assert!(cache
            .resolve(&moved, || (fixture.path().to_path_buf(), cwd.clone()))
            .is_none());
    }
}
