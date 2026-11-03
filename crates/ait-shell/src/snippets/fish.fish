# ait shell integration for fish — REQUIREMENTS.md §5.5 / §5.5.1.
# Sourced via `fish --init-command 'source <snippet>'` after the user's
# own config loads. Emits OSC 133 A/B/C/D prompt marks, OSC 7 cwd
# reports, and the five OSC 7700 hooks as hex-JSON; defines __ait_run
# for in-band exec.
#
# Re-sourcing in the same shell is a no-op: the guard reports
# `bootstrapped` (still integrated) and bails. A nested fish reads its
# normal config, never sees this file, and degrades cleanly instead of
# double-hooking (Spec R05 us-27).
if set -q __AIT_INTEGRATED_FISH; and functions -q __ait_emit
    __ait_emit "{\"hook\":\"bootstrapped\",\"value\":{\"session_id\":(__ait_sid)}}"
    return 0
end

# --- emit helpers -------------------------------------------------------

# Hex-encode stdin, lowercase — the only alphabet the 7700 codec reads.
function __ait_hex
    od -An -v -tx1 | tr -d ' \n'
end

# OSC 7700 hook event; $argv[1] is a {"hook":…,"value":…} JSON object.
function __ait_emit
    set -l hex (printf '%s' "$argv[1]" | __ait_hex)
    printf '\033]7700;%s\007' "$hex"
end

# The session id as a JSON string literal — always quoted, so a
# non-numeric or unset value can never produce invalid JSON (an
# undecodable payload moves nothing — the tab would silently never
# integrate). Unset reads as `""`, which matches no allocated id.
function __ait_sid
    if set -q AIT_SESSION_ID
        printf '"%s"' (__ait_json_escape "$AIT_SESSION_ID")
    else
        printf '""'
    end
end

# A named hook whose value is just the session id.
function __ait_hook -a name
    __ait_emit "{\"hook\":\"$name\",\"value\":{\"session_id\":(__ait_sid)}}"
end

# Escape a command string for JSON: quote, backslash and the named
# controls; strip remaining C0/DEL bytes — a sanitized command beats an
# invalid payload (the grid B→C slice stays the fallback source).
function __ait_json_escape -a s
    set s (string replace -a '\\' '\\\\' -- "$s")
    set s (string replace -a '"' '\"' -- "$s")
    set s (string replace -a \b '\b' -- "$s")
    set s (string replace -a \f '\f' -- "$s")
    set s (string replace -a \n '\n' -- "$s")
    set s (string replace -a \r '\r' -- "$s")
    set s (string replace -a \t '\t' -- "$s")
    set s (string replace -ra '[\x00-\x08\x0b\x0e-\x1f\x7f]' '' -- "$s")
    printf '%s' "$s"
end

# preexec carries the typed command text alongside the session id.
function __ait_hook_preexec -a command
    set -l escaped (__ait_json_escape "$command")
    __ait_emit "{\"hook\":\"preexec\",\"value\":{\"session_id\":(__ait_sid),\"command\":\"$escaped\"}}"
end

# --- §5.5.1 in-band exec ------------------------------------------------

# `__ait_run <id> <hex(command)>` — ait types this line on the prompt so
# in-band execution always leaves a trace (us-23). Runs the decoded
# command in a command-substitution subshell: the user's env, functions
# and cwd apply, and nothing renders in the grid. Replies on OSC 7701:
# A, then R frames of <=500 decoded bytes each (a 1000-char hex slice
# keeps every raw OSC under the 1024-byte vte budget), then B. The
# concatenated R payload is `<id>;<merged stdout+stderr>;<exit_code>`.
function __ait_run -a id hexcmd
    set -l cmd (printf '%b' (printf '%s' "$hexcmd" | sed 's/\(..\)/\\x\1/g'))
    # Merged output lines plus one `:<exit_code>` sentinel line.
    set -l capture (eval "$cmd" 2>&1; printf ':%d\n' $status)
    set -l last $capture[-1]
    set -l exit_code (string replace -r '^.*:' '' -- $last)
    set -l outlines $capture
    set -e outlines[-1]
    set -l lastline (string replace -r ':[0-9]+$' '' -- $last)
    set outlines $outlines $lastline
    set -l out (string join \n -- $outlines)
    set -l hex (printf '%s;%s;%s' "$id" "$out" "$exit_code" | __ait_hex)
    printf '\033]7701;A\007'
    set -l hlen (string length -- $hex)
    set -l pos 1
    while test $pos -le $hlen
        set -l frag (string sub -s $pos -l 1000 -- $hex)
        set -l dlen (math (string length -- $frag) / 2)
        printf '\033]7701;R;%d;%s\007' $dlen $frag
        set pos (math $pos + 1000)
    end
    printf '\033]7701;B\007'
end

# --- prompt/command hooks -----------------------------------------------

# Right after a command finishes: close the block with its exit code.
function __ait_fish_postexec --on-event fish_postexec
    printf '\033]133;D;%d\007' $status
end

# Before each prompt renders: cwd report, prompt marks, hook event.
function __ait_fish_prompt --on-event fish_prompt
    set -l cwd (string replace -a '%' '%25' -- $PWD)
    printf '\033]7;file://%s%s\007' (hostname) "$cwd"
    printf '\033]133;A\007'
    printf '\033]133;B\007'
    __ait_hook precmd
end

# $argv is the command line exactly as typed.
function __ait_fish_preexec --on-event fish_preexec
    printf '\033]133;C\007'
    __ait_hook_preexec (string join ' ' -- $argv)
end

function __ait_fish_exit --on-event fish_exit
    __ait_hook exit_shell
end

# --- install ------------------------------------------------------------

__ait_emit "{\"hook\":\"init_shell\",\"value\":{\"session_id\":(__ait_sid),\"shell\":\"fish\"}}"
set -g __AIT_INTEGRATED_FISH 1
