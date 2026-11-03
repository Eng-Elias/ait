//! The read-only file tools (Spec R06, §5.6): `read_files`, `grep`
//! and `glob`, implemented in-crate (`regex` is already a
//! dependency) and all confined to the bound session's current
//! working directory — a path that escapes returns a tool error the
//! model can report, never a panic and never a read outside the
//! tree.
//!
//! Confinement ([`ToolCwd::confine`]) normalizes `..`/`.` segments
//! lexically first, then — when the candidate exists — canonicalizes
//! it so a symlink pointing out of the tree is still rejected. The
//! root itself is canonicalized at construction, so the two sides
//! compare on the same terms; a root that does not resolve falls
//! back to its lexical form (tool errors then come from the
//! filesystem, not the check).
//!
//! [`ToolOutput`] splits "the tool produced output" from "the tool
//! reported an error": errors still become tool results — the model
//! reads them and adjusts — but the card paints them `Failed`.

use std::fs;
use std::path::{Component, Path, PathBuf};

use regex::Regex;

/// The largest single file `read_files` returns (Spec R06's size
/// guard): bigger files report an error rather than flooding the
/// conversation.
const READ_FILE_MAX_BYTES: u64 = 256 * 1024;

/// How much of a file is sniffed for a NUL byte before it is called
/// binary — the same cheap heuristic `grep` uses to skip binaries.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// The largest file `grep` opens — bigger ones are skipped (noted in
/// the output) rather than scanned.
const GREP_FILE_MAX_BYTES: u64 = 1024 * 1024;

/// `grep` reports at most this many matching lines before noting the
/// truncation.
const GREP_MATCH_CAP: usize = 200;

/// `glob` walks at most this many files before stopping.
const GLOB_WALK_CAP: usize = 20_000;

/// `glob` reports at most this many paths before noting the
/// truncation.
const GLOB_RESULT_CAP: usize = 500;

/// What a tool call produced — the text that (after
/// [`redact`](crate::redact)) enters the conversation as the tool
/// result, plus whether it is an error the card should paint
/// `Failed`.
#[derive(Debug)]
pub(crate) struct ToolOutput {
    /// The result text.
    pub(crate) text: String,
    /// `true` when the tool reported an error rather than output.
    pub(crate) is_error: bool,
}

impl ToolOutput {
    /// A successful result.
    pub(crate) fn ok(text: String) -> Self {
        Self {
            text,
            is_error: false,
        }
    }

    /// An error result — still model-facing text, never a throw.
    pub(crate) fn error(text: String) -> Self {
        Self {
            text,
            is_error: true,
        }
    }
}

/// A task's working directory — the tree every file tool is confined
/// to. Holds the root in the form [`confine`](Self::confine)
/// canonicalizes candidates to, so both sides of the prefix check
/// agree.
pub(crate) struct ToolCwd {
    root: PathBuf,
}

impl ToolCwd {
    /// Bind the tools to `cwd` — canonicalized when it resolves, so a
    /// `cwd` reached through a symlink compares equal to canonical
    /// candidates.
    pub(crate) fn new(cwd: &str) -> Self {
        let path = Path::new(cwd);
        let root = fs::canonicalize(path).unwrap_or_else(|_| normalize(path));
        Self { root }
    }

    /// The bound working directory — the directory `write_to_pty`
    /// spawns the agent's interactive session in (R06-T7).
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// `read_files` — read each of the arguments' `paths`, with the
    /// size and binary guards (Spec R06: contents inside the guards;
    /// oversized/binary reads and escapes return tool errors). Each
    /// path reports its own error inline, so one bad path does not
    /// erase the good reads.
    pub(crate) fn read_files(&self, args: &serde_json::Value) -> ToolOutput {
        let Some(paths) = args.get("paths").and_then(|v| v.as_array()) else {
            return ToolOutput::error(String::from(
                "read_files: `paths` must be an array of file paths",
            ));
        };
        if paths.is_empty() {
            return ToolOutput::error(String::from("read_files: `paths` is empty"));
        }
        let mut out = String::new();
        let mut failures = 0usize;
        for (index, path) in paths.iter().enumerate() {
            if index > 0 {
                out.push_str("\n\n");
            }
            let Some(path) = path.as_str() else {
                failures += 1;
                out.push_str("error: every `paths` entry must be a string");
                continue;
            };
            match self.read_one(path) {
                Ok((display, contents)) => {
                    out.push_str(&display);
                    out.push_str(":\n");
                    out.push_str(&contents);
                }
                Err(error) => {
                    failures += 1;
                    out.push_str(path);
                    out.push_str(": error: ");
                    out.push_str(&error);
                }
            }
        }
        if failures == paths.len() {
            ToolOutput::error(out)
        } else {
            ToolOutput::ok(out)
        }
    }

    /// One `read_files` path: confine, then read with the size and
    /// binary guards. Returns the display path (input-relative when
    /// it stayed inside the root) and the contents as lossy UTF-8.
    fn read_one(&self, path: &str) -> Result<(String, String), String> {
        let confined = self.confine(path)?;
        let metadata = fs::metadata(&confined)
            .map_err(|e| format!("cannot stat {}: {e}", self.display(&confined)))?;
        if !metadata.is_file() {
            return Err(format!("{} is not a regular file", self.display(&confined)));
        }
        if metadata.len() > READ_FILE_MAX_BYTES {
            return Err(format!(
                "{} exceeds the {} KiB read limit ({} bytes)",
                self.display(&confined),
                READ_FILE_MAX_BYTES / 1024,
                metadata.len()
            ));
        }
        let bytes = fs::read(&confined)
            .map_err(|e| format!("cannot read {}: {e}", self.display(&confined)))?;
        let sniff = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
        if sniff.contains(&0) {
            return Err(format!("{} looks binary", self.display(&confined)));
        }
        Ok((
            self.display(&confined),
            String::from_utf8_lossy(&bytes).into_owned(),
        ))
    }

    /// `grep` — the in-crate regex search (Spec R06): `pattern`
    /// (required, a `regex` pattern) applied line-wise over the text
    /// files under `path` (optional, default the root). Binary and
    /// oversized files are skipped, hidden entries (`.git`, dotfiles)
    /// are never walked, and matches cap at [`GREP_MATCH_CAP`].
    pub(crate) fn grep(&self, args: &serde_json::Value) -> ToolOutput {
        let Some(pattern) = args.get("pattern").and_then(|v| v.as_str()) else {
            return ToolOutput::error(String::from("grep: `pattern` must be a string"));
        };
        let regex = match Regex::new(pattern) {
            Ok(regex) => regex,
            Err(error) => {
                return ToolOutput::error(format!("grep: invalid pattern {pattern:?}: {error}"))
            }
        };
        let base = match self.base_dir(args.get("path")) {
            Ok(base) => base,
            Err(error) => return ToolOutput::error(error),
        };
        if !base.is_dir() {
            return ToolOutput::error(format!("grep: {} is not a directory", self.display(&base)));
        }
        let mut files = Vec::new();
        walk(&base, &mut files, GLOB_WALK_CAP);
        files.sort();
        let mut out = String::new();
        let mut matched = 0usize;
        let mut truncated = false;
        'files: for file in files {
            let Ok(metadata) = fs::metadata(&file) else {
                continue;
            };
            if !metadata.is_file() || metadata.len() > GREP_FILE_MAX_BYTES {
                continue;
            }
            let Ok(bytes) = fs::read(&file) else { continue };
            let sniff = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
            if sniff.contains(&0) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            for (number, line) in text.lines().enumerate() {
                if regex.is_match(line) {
                    if matched == GREP_MATCH_CAP {
                        truncated = true;
                        break 'files;
                    }
                    matched += 1;
                    out.push_str(&format!(
                        "{}:{}: {}\n",
                        self.display(&file),
                        number + 1,
                        line
                    ));
                }
            }
        }
        if truncated {
            out.push_str(&format!("… stopped after {GREP_MATCH_CAP} matches\n"));
        }
        if out.is_empty() {
            ToolOutput::ok(format!("grep: no lines match {pattern:?}"))
        } else {
            ToolOutput::ok(out.trim_end().to_string())
        }
    }

    /// `glob` — the in-crate filename match (Spec R06): `pattern`
    /// (required — `*`/`?` within a path segment, `**` across
    /// segments) matched against paths under `path` (optional,
    /// default the root). Hidden entries are never walked; results
    /// cap at [`GLOB_RESULT_CAP`].
    pub(crate) fn glob(&self, args: &serde_json::Value) -> ToolOutput {
        let Some(pattern) = args.get("pattern").and_then(|v| v.as_str()) else {
            return ToolOutput::error(String::from("glob: `pattern` must be a string"));
        };
        let segments: Vec<&str> = pattern.split('/').collect();
        if pattern.is_empty()
            || segments
                .iter()
                .any(|s| s.is_empty() || *s == "." || *s == "..")
            || pattern.starts_with('/')
        {
            return ToolOutput::error(format!(
                "glob: `pattern` {pattern:?} must be a relative path pattern \
                 (`*`, `?`, `**`; no `.`/`..` segments)"
            ));
        }
        let base = match self.base_dir(args.get("path")) {
            Ok(base) => base,
            Err(error) => return ToolOutput::error(error),
        };
        if !base.is_dir() {
            return ToolOutput::error(format!("glob: {} is not a directory", self.display(&base)));
        }
        let mut files = Vec::new();
        walk(&base, &mut files, GLOB_WALK_CAP);
        files.sort();
        let mut out = Vec::new();
        for file in &files {
            if out.len() == GLOB_RESULT_CAP {
                break;
            }
            let Ok(relative) = file.strip_prefix(&base) else {
                continue;
            };
            let names: Vec<&str> = relative
                .components()
                .filter_map(|c| c.as_os_str().to_str())
                .collect();
            if glob_match(&segments, &names) {
                out.push(self.display(file));
            }
        }
        if out.is_empty() {
            ToolOutput::ok(format!("glob: no paths match {pattern:?}"))
        } else {
            let mut text = out.join("\n");
            if files.len() == GLOB_WALK_CAP || out.len() == GLOB_RESULT_CAP {
                text.push_str("\n… truncated");
            }
            ToolOutput::ok(text)
        }
    }

    /// Resolve the optional `path` argument (a directory) against the
    /// root — confined like every other path.
    fn base_dir(&self, arg: Option<&serde_json::Value>) -> Result<PathBuf, String> {
        match arg {
            None | Some(serde_json::Value::Null) => Ok(self.root.clone()),
            Some(value) => match value.as_str() {
                Some(path) => self.confine(path),
                None => Err(String::from("`path` must be a string")),
            },
        }
    }

    /// Confine one model-supplied path to the root: join relative
    /// paths, normalize `.`/`..` lexically, then — when it exists —
    /// canonicalize so a symlink cannot smuggle the path out of the
    /// tree. Anything still outside returns a tool error.
    fn confine(&self, input: &str) -> Result<PathBuf, String> {
        let candidate = Path::new(input);
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.root.join(candidate)
        };
        let normalized = normalize(&joined);
        // The canonical check resolves symlink escapes; a path that
        // does not exist yet keeps its lexical form (reads of it
        // simply error below).
        let resolved = fs::canonicalize(&normalized).unwrap_or(normalized);
        if !resolved.starts_with(&self.root) {
            return Err(format!(
                "{input:?} is outside the working directory {}",
                self.root.display()
            ));
        }
        Ok(resolved)
    }

    /// `path` shown root-relative when it sits inside the tree —
    /// tool results read like the paths the model passed.
    fn display(&self, path: &Path) -> String {
        match path.strip_prefix(&self.root) {
            Ok(relative) if relative.as_os_str().is_empty() => String::from("."),
            Ok(relative) => relative.display().to_string(),
            Err(_) => path.display().to_string(),
        }
    }
}

/// `path` with `.`/`..` resolved lexically — no filesystem access, so
/// it applies to paths that do not exist yet. `..` at the root stays
/// at the root (lexical pops cannot leave `/`); the root-confinement
/// check is the escape guard, this is only normalization.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Depth-first file listing under `base`: hidden entries (any name
/// starting `.`) are skipped wholesale — `.git` is never walked —
/// and symlinks are neither followed nor listed (`file_type` does
/// not follow, so a link matches neither `is_dir` nor `is_file`),
/// which keeps the walk inside the tree by construction. Stops
/// growing `out` at `cap` entries.
fn walk(base: &Path, out: &mut Vec<PathBuf>, cap: usize) {
    if out.len() >= cap {
        return;
    }
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= cap {
            return;
        }
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            walk(&entry.path(), out, cap);
        } else if kind.is_file() {
            out.push(entry.path());
        }
    }
}

/// Match a `glob` pattern's segments against a relative path's
/// components: `**` spans zero or more components, every other
/// segment must match its component in place.
fn glob_match(pattern: &[&str], names: &[&str]) -> bool {
    match pattern.split_first() {
        None => names.is_empty(),
        Some((&"**", rest)) => {
            glob_match(rest, names) || (!names.is_empty() && glob_match(pattern, &names[1..]))
        }
        Some((segment, rest)) => {
            !names.is_empty() && component_match(segment, names[0]) && glob_match(rest, &names[1..])
        }
    }
}

/// One pattern segment against one path component: `*` matches any
/// run (including empty), `?` one character, everything else is
/// literal.
fn component_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    fn matches(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => matches(&p[1..], n) || (!n.is_empty() && matches(p, &n[1..])),
            Some('?') => !n.is_empty() && matches(&p[1..], &n[1..]),
            Some(&literal) => !n.is_empty() && n[0] == literal && matches(&p[1..], &n[1..]),
        }
    }
    matches(&pattern, &name)
}

#[cfg(test)]
mod tests {
    //! The file tools as a pure seam (Spec R06 testing decisions):
    //! temp-dir trees in, [`ToolOutput`] text out — no PTY, no model.

    use super::*;

    /// A tempdir bound as the tool root.
    fn tree(files: &[(&str, &str)]) -> (tempfile::TempDir, ToolCwd) {
        let dir = tempfile::tempdir().expect("tempdir");
        for (path, contents) in files {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(&full, contents).expect("write");
        }
        let cwd = ToolCwd::new(dir.path().to_str().expect("utf-8 tempdir"));
        (dir, cwd)
    }

    /// JSON args helper.
    fn args(json: &str) -> serde_json::Value {
        serde_json::from_str(json).expect("test args parse")
    }

    /// A read inside the tree returns the file, root-relative.
    #[test]
    fn read_files_returns_contents_inside_the_tree() {
        let (_dir, cwd) = tree(&[("note.txt", "hello"), ("src/a.rs", "fn a() {}")]);
        let out = cwd.read_files(&args(r#"{"paths": ["note.txt", "src/a.rs"]}"#));
        assert!(!out.is_error, "{:?}", out.text);
        assert_eq!(out.text, "note.txt:\nhello\n\nsrc/a.rs:\nfn a() {}");
    }

    /// `..` and absolute escapes are tool errors, and a good path in
    /// the same call still reads — the error is inline, not fatal.
    #[test]
    fn read_files_confines_escapes_to_tool_errors() {
        let (dir, cwd) = tree(&[("inside.txt", "safe")]);
        std::fs::write(dir.path().join("../outside.txt"), "SECRET-OUTSIDE").expect("outside");
        let out = cwd.read_files(&args(r#"{"paths": ["../outside.txt", "inside.txt"]}"#));
        assert!(
            !out.is_error,
            "one good read keeps the call ok: {:?}",
            out.text
        );
        assert!(
            out.text.contains("outside the working directory"),
            "{:?}",
            out.text
        );
        assert!(!out.text.contains("SECRET-OUTSIDE"));
        assert!(out.text.contains("inside.txt:\nsafe"));

        // An absolute escape behaves identically.
        let out = cwd.read_files(&args(&format!(
            r#"{{"paths": [{}]}}"#,
            serde_json::to_string(dir.path().join("../outside.txt").to_str().expect("utf-8"))
                .expect("json")
        )));
        assert!(out.is_error, "every path failed: {:?}", out.text);
        assert!(out.text.contains("outside the working directory"));
    }

    /// A symlink inside the tree pointing outside it still escapes —
    /// canonicalization catches what lexical normalization cannot.
    #[test]
    fn read_files_rejects_symlink_escapes() {
        let (dir, cwd) = tree(&[("inside.txt", "safe")]);
        let outside = dir.path().join("../outside.txt");
        std::fs::write(&outside, "SECRET-OUTSIDE").expect("outside");
        std::os::unix::fs::symlink(&outside, dir.path().join("link.txt")).expect("symlink");
        let out = cwd.read_files(&args(r#"{"paths": ["link.txt"]}"#));
        assert!(out.is_error);
        assert!(
            out.text.contains("outside the working directory"),
            "{:?}",
            out.text
        );
        assert!(!out.text.contains("SECRET-OUTSIDE"));
    }

    /// The size guard reports instead of reading.
    #[test]
    fn read_files_refuses_oversized_files() {
        let (_dir, cwd) = tree(&[("big.txt", &"x".repeat(300 * 1024))]);
        let out = cwd.read_files(&args(r#"{"paths": ["big.txt"]}"#));
        assert!(out.is_error);
        assert!(out.text.contains("exceeds"), "{:?}", out.text);
    }

    /// A NUL byte in the sniff window marks a file binary.
    #[test]
    fn read_files_refuses_binary_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("bin.dat"), b"\x89PNG\x00\x01").expect("write");
        let cwd = ToolCwd::new(dir.path().to_str().expect("utf-8 tempdir"));
        let out = cwd.read_files(&args(r#"{"paths": ["bin.dat"]}"#));
        assert!(out.is_error);
        assert!(out.text.contains("binary"), "{:?}", out.text);
    }

    /// Malformed arguments are tool errors too — never a panic.
    #[test]
    fn read_files_rejects_bad_arguments() {
        let (_dir, cwd) = tree(&[]);
        for bad in [r#"{}"#, r#"{"paths": "x"}"#, r#"{"paths": [1]}"#] {
            let out = cwd.read_files(&args(bad));
            assert!(out.is_error, "{bad}: {:?}", out.text);
        }
    }

    /// `grep` finds matching lines `path:line: text` inside the tree
    /// and skips hidden entries.
    #[test]
    fn grep_reports_matching_lines_with_paths() {
        let (_dir, cwd) = tree(&[
            ("src/a.rs", "fn main() {}\n// TODO: x"),
            ("src/deep/b.rs", "// TODO: deeper"),
            (".hidden/secret.rs", "TODO hidden"),
        ]);
        let out = cwd.grep(&args(r#"{"pattern": "TODO"}"#));
        assert!(!out.is_error, "{:?}", out.text);
        assert_eq!(
            out.text,
            "src/a.rs:2: // TODO: x\nsrc/deep/b.rs:1: // TODO: deeper"
        );
    }

    /// `grep`'s `path` argument scopes the walk — and confines.
    #[test]
    fn grep_scopes_to_a_confined_subdirectory() {
        let (_dir, cwd) = tree(&[("src/a.rs", "MARK"), ("other/b.rs", "MARK MARK")]);
        let out = cwd.grep(&args(r#"{"pattern": "MARK", "path": "src"}"#));
        assert_eq!(out.text, "src/a.rs:1: MARK");
        let out = cwd.grep(&args(r#"{"pattern": "MARK", "path": ".."}"#));
        assert!(out.is_error, "{:?}", out.text);
        let out = cwd.grep(&args(r#"{"pattern": "["}"#));
        assert!(
            out.is_error,
            "invalid regex is a tool error: {:?}",
            out.text
        );
    }

    /// `glob` matches `**` across directories and `*`/`?` inside
    /// components — root-relative paths, no hidden entries.
    #[test]
    fn glob_matches_across_the_tree() {
        let (_dir, cwd) = tree(&[
            ("src/a.rs", ""),
            ("src/deep/b.rs", ""),
            ("note.txt", ""),
            ("src/a.test", ""),
            (".git/config", ""),
        ]);
        let out = cwd.glob(&args(r#"{"pattern": "**/*.rs"}"#));
        assert!(!out.is_error);
        assert_eq!(out.text, "src/a.rs\nsrc/deep/b.rs");

        let out = cwd.glob(&args(r#"{"pattern": "src/*.?s"}"#));
        assert_eq!(out.text, "src/a.rs");

        let out = cwd.glob(&args(r#"{"pattern": "*.txt"}"#));
        assert_eq!(out.text, "note.txt");
    }

    /// `glob` confines `path` and rejects `..`/absolute patterns.
    #[test]
    fn glob_confines_and_rejects_escape_patterns() {
        let (_dir, cwd) = tree(&[("a.rs", "")]);
        for bad in ["../x.rs", "/etc/*.conf", "src/../a.rs", ""] {
            let out = cwd.glob(&args(&format!("{{\"pattern\": {bad:?}}}")));
            assert!(out.is_error, "{bad:?}: {:?}", out.text);
        }
        let out = cwd.glob(&args(r#"{"pattern": "*.rs", "path": ".."}"#));
        assert!(out.is_error, "{:?}", out.text);
    }

    /// The lexical normalizer resolves `.`/`..` without filesystem
    /// access and cannot pop above `/`.
    #[test]
    fn normalize_resolves_segments_lexically() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(normalize(Path::new("/../../x")), PathBuf::from("/x"));
    }
}
