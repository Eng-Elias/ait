//! The unified-diff engine behind `edit_files` (Spec R06 us-22):
//! the same unified format `git diff` prints — `--- a/`/`+++ b/`
//! labels, `@@` hunks with context — computed in-crate so the
//! approve card's rendered diff is byte-for-byte the write an allow
//! verdict applies. [`unified_diff`] is the whole public surface;
//! everything else is its machinery: line splitting that tracks the
//! trailing-newline marker, an LCS table bounded by
//! [`DIFF_LCS_CELL_CAP`] (a huge rewrite degrades to one contiguous
//! replace hunk — coarser, never wrong), and the hunk emitter.

/// Unified-diff context lines above and below each change — the
/// conventional three.
const DIFF_CONTEXT: usize = 3;

/// The most cells an edit diff's LCS table may hold — beyond it the
/// diff degrades to one contiguous replace hunk: coarser, but still
/// exactly the bytes that will be written, and never quadratic
/// memory on a whole-file rewrite.
const DIFF_LCS_CELL_CAP: usize = 1_000_000;

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
pub(crate) fn unified_diff(display: &str, old: Option<&str>, new: &str) -> String {
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
