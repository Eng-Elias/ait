//! The file tools (Spec R06, §5.6): `read_files`, `grep`, `glob`
//! and `edit_files`, implemented in-crate (`regex` is already a
//! dependency) and all confined to the bound session's current
//! working directory — a path that escapes returns a tool error the
//! model can report, never a panic and never an access outside the
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

/// Unified-diff context lines above and below each change — the
/// conventional three.
const DIFF_CONTEXT: usize = 3;

/// The most cells an edit diff's LCS table may hold — beyond it the
/// diff degrades to one contiguous replace hunk: coarser, but still
/// exactly the bytes that will be written, and never quadratic
/// memory on a whole-file rewrite.
const DIFF_LCS_CELL_CAP: usize = 1_000_000;

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

    /// `edit_files` — plan the whole call: parse the `files` array,
    /// confine every `path`, read each existing file for the diff's
    /// old side and verify a new file's parent exists inside the
    /// tree (us-23). The result is an [`EditPlan`] — the unified
    /// diff the approve card renders AND the write an allow verdict
    /// applies, planned once so card and disk can never diverge
    /// (us-22). Any bad entry fails the whole plan: an edit call is
    /// all-or-nothing, never a partial write.
    pub(crate) fn plan_edits(&self, args: &serde_json::Value) -> Result<EditPlan, String> {
        let Some(files) = args.get("files").and_then(|v| v.as_array()) else {
            return Err(String::from(
                "edit_files: `files` must be an array of {path, content} entries",
            ));
        };
        if files.is_empty() {
            return Err(String::from("edit_files: `files` is empty"));
        }
        let mut edits = Vec::with_capacity(files.len());
        for entry in files {
            edits.push(self.plan_one(entry)?);
        }
        Ok(EditPlan { edits })
    }

    /// `edit_files`' direct execution path — plan then apply. The
    /// gate holds the plan it rendered and calls
    /// [`EditPlan::apply`] on it instead, so this arm only serves
    /// callers that arrive unplanned.
    pub(crate) fn edit_files(&self, args: &serde_json::Value) -> ToolOutput {
        match self.plan_edits(args) {
            Ok(plan) => plan.apply(),
            Err(error) => ToolOutput::error(error),
        }
    }

    /// One `files` entry: `path` + `content` strings, the path
    /// confined, the old side read (or proven a safely-creatable new
    /// file) and the unified diff rendered. Anything a diff cannot
    /// show faithfully — a binary or non-UTF-8 existing file, NUL
    /// bytes in `content` — refuses rather than writing blind.
    fn plan_one(&self, entry: &serde_json::Value) -> Result<PlannedEdit, String> {
        let Some(path) = entry.get("path").and_then(|v| v.as_str()) else {
            return Err(String::from(
                "edit_files: every `files` entry needs a string `path`",
            ));
        };
        let Some(content) = entry.get("content").and_then(|v| v.as_str()) else {
            return Err(format!("edit_files: {path:?} needs a string `content`"));
        };
        if content.contains('\0') {
            return Err(format!(
                "edit_files: {path} — `content` holds NUL bytes; \
                 binary writes are not supported"
            ));
        }
        let confined = self.confine(path)?;
        let display = self.display(&confined);
        let (target, old) = match fs::metadata(&confined) {
            Ok(meta) if meta.is_file() => {
                let bytes =
                    fs::read(&confined).map_err(|e| format!("cannot read {display}: {e}"))?;
                let sniff = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
                if sniff.contains(&0) {
                    return Err(format!("{display} looks binary — refusing to replace it"));
                }
                let text = String::from_utf8(bytes)
                    .map_err(|_| format!("{display} is not valid UTF-8 — refusing to diff it"))?;
                (confined, Some(text))
            }
            Ok(_) => return Err(format!("{display} is not a regular file")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (self.new_file_target(&confined, &display)?, None)
            }
            Err(error) => return Err(format!("cannot stat {display}: {error}")),
        };
        Ok(PlannedEdit {
            diff: unified_diff(&display, old.as_deref(), content),
            path: target,
            display,
            content: content.to_string(),
        })
    }

    /// Where a brand-new `confined` path may be written: its parent
    /// must exist as a directory inside the tree — canonicalized,
    /// so a symlinked directory cannot smuggle the write outside
    /// (the leaf does not exist yet, so [`confine`](Self::confine)
    /// saw only its lexical form). A dangling symlink AT the path
    /// refuses too: `metadata` reported it missing but a write
    /// would land wherever the link points.
    fn new_file_target(&self, confined: &Path, display: &str) -> Result<PathBuf, String> {
        if let Ok(meta) = fs::symlink_metadata(confined) {
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "{display} is a dangling symlink — refusing to write through it"
                ));
            }
        }
        let Some(parent) = confined.parent() else {
            return Err(format!("edit_files: {display} has no parent directory"));
        };
        let Ok(canonical) = fs::canonicalize(parent) else {
            return Err(format!(
                "edit_files: {display}'s parent directory does not exist"
            ));
        };
        if !canonical.is_dir() {
            return Err(format!("edit_files: {display}'s parent is not a directory"));
        }
        if !canonical.starts_with(&self.root) {
            return Err(format!(
                "{display} is outside the working directory {}",
                self.root.display()
            ));
        }
        Ok(canonical.join(
            confined
                .file_name()
                .expect("a path with a parent names a leaf"),
        ))
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

/// A whole `edit_files` call resolved against the tree (Spec R06,
/// us-22/23): every file's confined write target, the exact
/// `content` bytes and the unified diff the approve card shows.
/// The gate plans before the card and an allow verdict applies the
/// same plan, so the rendered diff is byte-for-byte the applied
/// change — never a recomputation that could drift.
#[derive(Debug)]
pub(crate) struct EditPlan {
    edits: Vec<PlannedEdit>,
}

/// One file's planned rewrite.
#[derive(Debug)]
struct PlannedEdit {
    /// The confined, canonicalized write target.
    path: PathBuf,
    /// The root-relative name the diff's `a/`/`b/` labels show.
    display: String,
    /// The exact bytes [`EditPlan::apply`] writes — `fs::write` of
    /// this string, nothing else.
    content: String,
    /// This file's unified diff (`--- a/`/`+++ b/`, or `/dev/null`
    /// for a new file).
    diff: String,
}

impl EditPlan {
    /// The approve card's renderable surface — every file's diff,
    /// one after another.
    pub(crate) fn summary(&self) -> String {
        self.edits
            .iter()
            .map(|edit| edit.diff.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Write each `content` byte-for-byte to its target. A write
    /// failure reports which files already landed — partial writes
    /// can happen on I/O error, so the result says so honestly.
    pub(crate) fn apply(&self) -> ToolOutput {
        let mut wrote = Vec::with_capacity(self.edits.len());
        for edit in &self.edits {
            if let Err(error) = fs::write(&edit.path, edit.content.as_bytes()) {
                let mut text = format!("edit_files: cannot write {}: {error}", edit.display);
                if !wrote.is_empty() {
                    text.push_str(&format!(" — already wrote {}", wrote.join(", ")));
                }
                return ToolOutput::error(text);
            }
            wrote.push(edit.display.clone());
        }
        match wrote.as_slice() {
            [one] => ToolOutput::ok(format!("wrote {one}")),
            many => ToolOutput::ok(format!("wrote {} files: {}", many.len(), many.join(", "))),
        }
    }
}

/// One line of a diffed file — its text plus whether a newline
/// terminated it, so a change that only adds or drops the trailing
/// newline still diffs (and prints `\ No newline at end of file`
/// exactly like a real unified diff).
#[derive(Clone, Copy, PartialEq, Eq)]
struct DiffLine<'a> {
    text: &'a str,
    terminated: bool,
}

/// `text` as [`DiffLine`]s — every line terminated except possibly
/// the last.
fn lines_of(text: &str) -> Vec<DiffLine<'_>> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<DiffLine> = text
        .split('\n')
        .map(|text| DiffLine {
            text,
            terminated: true,
        })
        .collect();
    if text.ends_with('\n') {
        lines.pop();
    } else if let Some(last) = lines.last_mut() {
        last.terminated = false;
    }
    lines
}

/// One file's unified diff: `--- a/<display>`/`+++ b/<display>`
/// labels (`--- /dev/null` for a new file), then `@@` hunks with
/// [`DIFF_CONTEXT`] context lines. Returns the full block, no
/// trailing newline.
fn unified_diff(display: &str, old: Option<&str>, new: &str) -> String {
    let old_lines = old.map(lines_of).unwrap_or_default();
    let new_lines = lines_of(new);
    let ops = line_ops(&old_lines, &new_lines);
    let mut out = String::new();
    if old.is_some() {
        out.push_str("--- a/");
        out.push_str(display);
    } else {
        out.push_str("--- /dev/null");
    }
    out.push_str("\n+++ b/");
    out.push_str(display);
    out.push('\n');
    if ops.iter().all(|(tag, _)| *tag == b' ') {
        // An unchanged write still previews — an empty card would
        // read as broken, not as a no-op.
        out.push_str("(no changes)");
        return out;
    }
    emit_hunks(&mut out, &ops);
    out.trim_end().to_string()
}

/// The line-level ops turning `old` into `new` — `(tag, line)`
/// where `tag` is `' '`/`-`/`+`. Common prefix and suffix are
/// context outright; the middle diffs by LCS when its table fits
/// [`DIFF_LCS_CELL_CAP`], else as one contiguous `-…`/`+…` replace
/// run (still faithful, just coarser).
fn line_ops<'a>(old: &[DiffLine<'a>], new: &[DiffLine<'a>]) -> Vec<(u8, DiffLine<'a>)> {
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old.len() - prefix
        && suffix < new.len() - prefix
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let old_mid = &old[prefix..old.len() - suffix];
    let new_mid = &new[prefix..new.len() - suffix];
    let mut ops = Vec::with_capacity(prefix + suffix + old_mid.len() + new_mid.len());
    ops.extend(old[..prefix].iter().map(|line| (b' ', *line)));
    if old_mid.len().saturating_mul(new_mid.len()) <= DIFF_LCS_CELL_CAP {
        ops.extend(lcs_ops(old_mid, new_mid));
    } else {
        ops.extend(old_mid.iter().map(|line| (b'-', *line)));
        ops.extend(new_mid.iter().map(|line| (b'+', *line)));
    }
    ops.extend(old[old.len() - suffix..].iter().map(|line| (b' ', *line)));
    ops
}

/// LCS backtrack over two line slices — the classic dynamic-
/// programming table, correct and plenty fast inside
/// [`DIFF_LCS_CELL_CAP`].
fn lcs_ops<'a>(old: &[DiffLine<'a>], new: &[DiffLine<'a>]) -> Vec<(u8, DiffLine<'a>)> {
    let (n, m) = (old.len(), new.len());
    let width = m + 1;
    let mut table = vec![0u32; (n + 1) * width];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i * width + j] = if old[i] == new[j] {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
        }
    }
    let mut ops = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old[i] == new[j] {
            ops.push((b' ', old[i]));
            i += 1;
            j += 1;
        } else if table[(i + 1) * width + j] >= table[i * width + j + 1] {
            ops.push((b'-', old[i]));
            i += 1;
        } else {
            ops.push((b'+', new[j]));
            j += 1;
        }
    }
    ops.extend(old[i..].iter().map(|line| (b'-', *line)));
    ops.extend(new[j..].iter().map(|line| (b'+', *line)));
    ops
}

/// Walk `ops` and print each `@@` hunk into `out`: changes closer
/// than a full context pair (`2 * DIFF_CONTEXT` keeps) merge into
/// one hunk; every hunk carries up to [`DIFF_CONTEXT`] leading and
/// trailing context lines.
fn emit_hunks(out: &mut String, ops: &[(u8, DiffLine)]) {
    let mut index = 0;
    while index < ops.len() {
        if ops[index].0 == b' ' {
            index += 1;
            continue;
        }
        // Leading context, at most DIFF_CONTEXT keeps back.
        let mut start = index;
        while start > 0 && ops[start - 1].0 == b' ' && index - start < DIFF_CONTEXT {
            start -= 1;
        }
        // The hunk's last change: keeps longer than a full context
        // pair end it.
        let mut last_change = index;
        let mut keeps = 0;
        let mut j = index;
        while j < ops.len() {
            if ops[j].0 == b' ' {
                keeps += 1;
                if keeps > 2 * DIFF_CONTEXT {
                    break;
                }
            } else {
                keeps = 0;
                last_change = j;
            }
            j += 1;
        }
        // Trailing context.
        let mut stop = last_change + 1;
        while stop < ops.len() && ops[stop].0 == b' ' && stop - last_change - 1 < DIFF_CONTEXT {
            stop += 1;
        }
        emit_hunk(out, ops, start, stop);
        index = stop;
    }
}

/// One `@@ -s,c +s,c @@` hunk into `out` — `ops[start..stop]` with
/// the file positions before `start` counted from `ops[..start]`.
/// An unterminated line prints the `\ No newline at end of file`
/// marker right after it.
fn emit_hunk(out: &mut String, ops: &[(u8, DiffLine)], start: usize, stop: usize) {
    let mut old_pos = 0;
    let mut new_pos = 0;
    for (tag, _) in &ops[..start] {
        match tag {
            b' ' => {
                old_pos += 1;
                new_pos += 1;
            }
            b'-' => old_pos += 1,
            _ => new_pos += 1,
        }
    }
    let hunk = &ops[start..stop];
    let old_count = hunk
        .iter()
        .filter(|(tag, _)| *tag == b' ' || *tag == b'-')
        .count();
    let new_count = hunk
        .iter()
        .filter(|(tag, _)| *tag == b' ' || *tag == b'+')
        .count();
    // Unified range syntax: `start` is the 1-based first line, or the
    // line BEFORE the position when the side contributes nothing.
    let old_start = if old_count == 0 { old_pos } else { old_pos + 1 };
    let new_start = if new_count == 0 { new_pos } else { new_pos + 1 };
    out.push_str(&format!(
        "@@ -{} +{} @@\n",
        range(old_start, old_count),
        range(new_start, new_count)
    ));
    for (tag, line) in hunk {
        out.push(*tag as char);
        out.push_str(line.text);
        out.push('\n');
        if !line.terminated {
            out.push_str("\\ No newline at end of file\n");
        }
    }
}

/// A hunk range — `,count` is omitted when the count is 1, matching
/// `git diff`'s rendering.
fn range(start: usize, count: usize) -> String {
    if count == 1 {
        start.to_string()
    } else {
        format!("{start},{count}")
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

    /// A rewrite diffs old against new — `a/`/`b/` labels, a `@@`
    /// hunk with `-`/`+` lines and context — and `apply` writes
    /// `content` byte-for-byte (us-22: the diff IS the change).
    #[test]
    fn edit_files_diffs_a_rewrite_and_applies_it_exactly() {
        let (dir, cwd) = tree(&[("note.txt", "one\ntwo\nthree\n")]);
        let plan = cwd
            .plan_edits(&args(
                r#"{"files": [{"path": "note.txt", "content": "one\nTWO\nthree\n"}]}"#,
            ))
            .expect("plan");
        assert_eq!(
            plan.summary(),
            "--- a/note.txt\n+++ b/note.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three"
        );
        let out = plan.apply();
        assert!(!out.is_error, "{:?}", out.text);
        assert_eq!(out.text, "wrote note.txt");
        assert_eq!(
            std::fs::read(dir.path().join("note.txt")).expect("read"),
            b"one\nTWO\nthree\n"
        );
    }

    /// A scattered rewrite produces separate hunks with three
    /// context lines each — the diff is line-granular, not one
    /// whole-file replace.
    #[test]
    fn edit_files_splits_scattered_changes_into_hunks() {
        let mut old = String::new();
        let mut new = String::new();
        for i in 1..=20 {
            old.push_str(&format!("line{i}\n"));
            new.push_str(&if i == 3 || i == 18 {
                format!("LINE{i}\n")
            } else {
                format!("line{i}\n")
            });
        }
        let (dir, cwd) = tree(&[("big.txt", &old)]);
        let plan = cwd
            .plan_edits(&args(
                &serde_json::json!({
                    "files": [{"path": "big.txt", "content": new}]
                })
                .to_string(),
            ))
            .expect("plan");
        let summary = plan.summary();
        assert!(
            summary.contains("@@ -1,6 +1,6 @@"),
            "first hunk keeps 3 context lines: {summary}"
        );
        assert!(
            summary.contains("@@ -15,6 +15,6 @@"),
            "second hunk: {summary}"
        );
        assert_eq!(summary.matches("@@").count(), 4, "two hunk headers");
        plan.apply();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("big.txt")).expect("read"),
            new
        );
    }

    /// A new file previews `--- /dev/null` → `+++ b/<path>` with all
    /// `+` lines, and only inside an existing parent (us-23).
    #[test]
    fn edit_files_previews_new_files_as_dev_null_diffs() {
        let (dir, cwd) = tree(&[("sub/keep.txt", "k")]);
        let plan = cwd
            .plan_edits(&args(
                r#"{"files": [{"path": "sub/new.txt", "content": "hello\nworld\n"}]}"#,
            ))
            .expect("plan");
        assert_eq!(
            plan.summary(),
            "--- /dev/null\n+++ b/sub/new.txt\n@@ -0,0 +1,2 @@\n+hello\n+world"
        );
        let out = plan.apply();
        assert!(!out.is_error, "{:?}", out.text);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sub/new.txt")).expect("read"),
            "hello\nworld\n"
        );
    }

    /// A new file under a missing parent directory refuses at
    /// planning — before the card, never as a silent mkdir (us-23).
    #[test]
    fn edit_files_refuses_a_missing_parent_directory() {
        let (_dir, cwd) = tree(&[]);
        let error = cwd
            .plan_edits(&args(
                r#"{"files": [{"path": "nope/new.txt", "content": "x"}]}"#,
            ))
            .expect_err("missing parent");
        assert!(error.contains("parent"), "{error}");
        assert!(error.contains("nope/new.txt"), "{error}");
    }

    /// The confinement rule binds writes the same as reads: `..`
    /// and absolute escapes, plus a symlinked directory pointing
    /// out of the tree — the leaf's lexical normalize cannot see
    /// the last one, the parent's canonicalize can.
    #[test]
    fn edit_files_confines_writes_to_the_tree() {
        let (dir, cwd) = tree(&[("note.txt", "x")]);
        for path in ["../out.txt", "/tmp/ait-edit-escape.txt"] {
            let error = cwd
                .plan_edits(&args(&format!(
                    r#"{{"files": [{{"path": {path:?}, "content": "y"}}]}}"#
                )))
                .expect_err(path);
            assert!(error.contains("outside the working directory"), "{error}");
        }
        // A new file under a symlinked directory escapes through
        // the parent even though the leaf never exists.
        let outside = tempfile::tempdir().expect("outside dir");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("linkdir")).expect("symlink");
        let error = cwd
            .plan_edits(&args(
                r#"{"files": [{"path": "linkdir/evil.txt", "content": "y"}]}"#,
            ))
            .expect_err("symlinked parent");
        assert!(error.contains("outside the working directory"), "{error}");
        assert!(!outside.path().join("evil.txt").exists());
    }

    /// A binary or non-UTF-8 existing file, or `content` holding a
    /// NUL, refuses: the diff could not show the change faithfully,
    /// so the write must not happen either (us-22).
    #[test]
    fn edit_files_refuses_what_a_diff_cannot_show() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("bin.dat"), b"\x89PNG\x00\x01").expect("bin");
        std::fs::write(dir.path().join("bad.txt"), b"\xff\xfe invalid utf8").expect("bad");
        let cwd = ToolCwd::new(dir.path().to_str().expect("utf-8"));
        let error = cwd
            .plan_edits(&args(r#"{"files": [{"path": "bin.dat", "content": "x"}]}"#))
            .expect_err("binary");
        assert!(error.contains("binary"), "{error}");
        let error = cwd
            .plan_edits(&args(r#"{"files": [{"path": "bad.txt", "content": "x"}]}"#))
            .expect_err("non-utf8");
        assert!(error.contains("UTF-8"), "{error}");
        let error = cwd
            .plan_edits(&args(
                "{\"files\": [{\"path\": \"note.txt\", \"content\": \"a\\u0000b\"}]}",
            ))
            .expect_err("nul content");
        assert!(error.contains("NUL"), "{error}");
    }

    /// Trailing-newline changes still diff: a line's terminator is
    /// part of its identity, and the marker prints exactly like a
    /// real unified diff.
    #[test]
    fn edit_files_diffs_trailing_newline_changes() {
        let (dir, cwd) = tree(&[("f.txt", "a\nb")]); // no trailing newline
        let plan = cwd
            .plan_edits(&args(
                r#"{"files": [{"path": "f.txt", "content": "a\nb\n"}]}"#,
            ))
            .expect("plan");
        assert_eq!(
            plan.summary(),
            "--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+b"
        );
        plan.apply();
        assert_eq!(
            std::fs::read(dir.path().join("f.txt")).expect("read"),
            b"a\nb\n"
        );

        // The reverse — dropping the newline — mirrors the marker.
        let plan = cwd
            .plan_edits(&args(
                r#"{"files": [{"path": "f.txt", "content": "a\nb"}]}"#,
            ))
            .expect("plan");
        assert_eq!(
            plan.summary(),
            "--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,2 @@\n a\n-b\n+b\n\\ No newline at end of file"
        );
    }

    /// Identical content produces a header-only diff marked as no
    /// change — an empty summary on the approve card would read as
    /// a broken card, not a no-op.
    #[test]
    fn edit_files_marks_unchanged_content() {
        let (_dir, cwd) = tree(&[("note.txt", "same\n")]);
        let plan = cwd
            .plan_edits(&args(
                r#"{"files": [{"path": "note.txt", "content": "same\n"}]}"#,
            ))
            .expect("plan");
        assert_eq!(
            plan.summary(),
            "--- a/note.txt\n+++ b/note.txt\n(no changes)"
        );
    }

    /// A rewrite bigger than the LCS table still yields a faithful
    /// unified diff — one contiguous replace hunk instead of a
    /// quadratic-memory table (coarser, never wrong).
    #[test]
    fn edit_files_falls_back_to_one_hunk_when_the_change_is_huge() {
        let old: String = (0..1200).map(|i| format!("old{i}\n")).collect();
        let new: String = (0..1200).map(|i| format!("new{i}\n")).collect();
        let (dir, cwd) = tree(&[("huge.txt", &old)]);
        let plan = cwd
            .plan_edits(&args(
                &serde_json::json!({
                    "files": [{"path": "huge.txt", "content": new}]
                })
                .to_string(),
            ))
            .expect("plan");
        let summary = plan.summary();
        assert!(
            summary.contains("@@ -1,1200 +1,1200 @@"),
            "single replace hunk: {}",
            &summary[..summary.len().min(300)]
        );
        assert!(summary.contains("-old0") && summary.contains("+new1199"));
        plan.apply();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("huge.txt")).expect("read"),
            new
        );
    }

    /// One call can carry several files — every entry diffs into
    /// the same summary and all of them apply together.
    #[test]
    fn edit_files_plans_and_applies_several_files() {
        let (dir, cwd) = tree(&[("a.txt", "A\n")]);
        let plan = cwd
            .plan_edits(&args(
                r#"{"files": [
                    {"path": "a.txt", "content": "A2\n"},
                    {"path": "b.txt", "content": "B\n"}
                ]}"#,
            ))
            .expect("plan");
        let summary = plan.summary();
        assert!(summary.contains("+++ b/a.txt"), "{summary}");
        assert!(summary.contains("+++ b/b.txt"), "{summary}");
        assert!(summary.contains("--- /dev/null"), "{summary}");
        let out = plan.apply();
        assert!(!out.is_error, "{:?}", out.text);
        assert_eq!(out.text, "wrote 2 files: a.txt, b.txt");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).expect("a"),
            "A2\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).expect("b"),
            "B\n"
        );
    }

    /// Every bad-arguments shape is a planning error — the model
    /// reads it and retries; nothing panics and nothing writes.
    #[test]
    fn edit_files_rejects_bad_arguments() {
        let (_dir, cwd) = tree(&[]);
        for bad in [
            r#"{}"#,
            r#"{"files": "x"}"#,
            r#"{"files": []}"#,
            r#"{"files": [{"path": 1, "content": "x"}]}"#,
            r#"{"files": [{"path": "a.txt"}]}"#,
        ] {
            assert!(cwd.plan_edits(&args(bad)).is_err(), "{bad} must error");
        }
    }

    /// The `edit_files` entry point: plans then applies — the path
    /// the dispatch arm takes when no plan was precomputed.
    #[test]
    fn edit_files_executes_through_plan_and_apply() {
        let (dir, cwd) = tree(&[("note.txt", "old\n")]);
        let out = cwd.edit_files(&args(
            r#"{"files": [{"path": "note.txt", "content": "new\n"}]}"#,
        ));
        assert!(!out.is_error, "{:?}", out.text);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.txt")).expect("read"),
            "new\n"
        );
        let out = cwd.edit_files(&args(
            r#"{"files": [{"path": "../x.txt", "content": "y"}]}"#,
        ));
        assert!(out.is_error, "{:?}", out.text);
    }
}
