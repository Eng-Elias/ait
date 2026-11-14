//! Screen probes and payload helpers shared by `ait`'s code-first
//! e2e tests (`crates/ait/tests/*.rs`) — the prompt, panel and
//! sentinel predicates those files used to carry as private copies.
//! One home means a fix like the root-prompt or echo-satisfiable-wait
//! trap lands once instead of per copy (R07 retro).

use crate::harness::E2e;

/// Wait for a fresh, empty prompt before typing: an interactive
/// bash/zsh/fish prompt ends `$`/`%`/`>` — or `#` under uid 0, where
/// PS1's `\$` renders `#` — and a line *ending* in such a char is a
/// prompt with nothing typed yet: screen rows are trim_end'ed, and a
/// prompt carrying text ends in the command instead. Without this
/// gate a `type_text` can beat the prompt's render and interleave
/// echo, prompt and output on one row (the `-sh` scenario race).
///
/// `#` widens the heuristic on the same terms the other prompt chars
/// already accept: output that itself ends in `#` satisfies the wait
/// early, so a test printing such a line needs its own gate.
pub fn wait_fresh_prompt(app: &mut E2e, description: &str) {
    app.wait_for_screen(description, |app| {
        app.screen_text().lines().any(|line| {
            line.ends_with('$') || line.ends_with('%') || line.ends_with('>') || line.ends_with('#')
        })
    });
}

/// The terminal half of one screen row — everything left of the agent
/// panel's `│` separator. The panel never reaches these columns, so
/// probes here can only be the session's own grid, never a card's
/// summary.
pub fn terminal_half(row: &str) -> &str {
    row.split('│').next().unwrap_or(row)
}

/// Whether any screen row's *panel* half trims to exactly `text` —
/// how a conversation status line (`done`, `cancelled`) is found
/// without colliding with terminal-side text.
pub fn panel_has_line(app: &E2e, text: &str) -> bool {
    app.screen_text().lines().any(|row| {
        row.split('│')
            .nth(1)
            .is_some_and(|half| half.trim() == text)
    })
}

/// `true` when `row`'s terminal half carries a `run_command` sentinel
/// *result* line — `__AIT_END_<id>=<exit>` — which only the run's
/// sentinel print produces. The echoed type-in line carries the same
/// `__AIT_END_` prefix verbatim but `=%d`/`=$LASTEXITCODE`-style
/// text, never `=<digit>`, so a bare `contains("__AIT_END_")` wait
/// passes on echo alone — the trap CODING_STANDARDS §15 forbids. A
/// leading `-` counts: signed codes (`exit -1`, killed-process
/// statuses) are results too (§5.14).
pub fn sentinel_result(row: &str) -> bool {
    let half = terminal_half(row);
    let Some(start) = half.find("__AIT_END_") else {
        return false;
    };
    half[start..]
        .split('=')
        .nth(1)
        .is_some_and(|tail| tail.starts_with(|ch: char| ch.is_ascii_digit() || ch == '-'))
}

/// A JSON string literal (`"…"`) for hand-built wire bodies — the
/// escaping the agent-panel/profile SSE transcripts and the R04
/// suggester tests share.
pub fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sentinel predicate's whole job: the echoed type-in carries
    /// `=%d`-style text and never satisfies it — whatever the shell
    /// family spells (§5.14's `Write-Output`/`call echo` forms echo
    /// `=$(…`/`=%^ERRORLEVEL%` literally); the result's own output —
    /// signed codes included — does; a panel-side `=<digit>` never
    /// leaks through the `│` split.
    #[test]
    fn sentinel_result_needs_the_result_row_not_the_echo() {
        assert!(!sentinel_result("$ echo hi; printf '__AIT_END_7=%d\\n' $?"));
        assert!(!sentinel_result(
            "PS> Write-Output x; Write-Output \"__AIT_END_7=$(if ($null -ne $LASTEXITCODE) { 1 })\""
        ));
        assert!(!sentinel_result(
            "C:\\> echo x & call echo __AIT_END_7=%^ERRORLEVEL%"
        ));
        assert!(sentinel_result("__AIT_END_7=0"));
        assert!(sentinel_result("__AIT_END_7=-1"));
        assert!(!sentinel_result("some output │ __AIT_END_7=0"));
        assert!(!sentinel_result("no sentinel here"));
    }

    #[test]
    fn terminal_half_splits_at_the_panel_separator() {
        assert_eq!(terminal_half("left │ right"), "left ");
        assert_eq!(terminal_half("no separator"), "no separator");
    }

    #[test]
    fn json_string_escapes_the_wire_chars() {
        assert_eq!(json_string("a\"b\\c\nd\te"), "\"a\\\"b\\\\c\\nd\\te\"");
        assert_eq!(json_string("plain"), "\"plain\"");
    }
}
