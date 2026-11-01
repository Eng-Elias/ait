# ait shell integration for zsh — REQUIREMENTS.md §5.5 / §5.5.1.
# Sourced from the temp-ZDOTDIR .zshrc forwarder after the user's real
# .zshrc. Emits OSC 133 A/B/C/D prompt marks, OSC 7 cwd reports, and the
# five OSC 7700 hooks as hex-JSON; defines __ait_run for in-band exec.
#
# Re-sourcing in the same shell is a no-op: the guard reports
# `bootstrapped` (still integrated) and bails. A nested zsh reads the
# restored real ZDOTDIR, never sees this file, and degrades cleanly
# instead of double-hooking (Spec R05 us-27).
if [[ -n ${__AIT_INTEGRATED_ZSH:-} ]] && (( $+functions[__ait_emit] )); then
    __ait_emit "{\"hook\":\"bootstrapped\",\"value\":{\"session_id\":$(__ait_sid)}}"
    return 0
fi

# --- emit helpers -------------------------------------------------------

# Hex-encode stdin, lowercase — the only alphabet the 7700 codec reads.
__ait_hex() {
    od -An -v -tx1 | tr -d ' \n'
}

# OSC 7700 hook event; $1 is a complete {"hook":…,"value":…} JSON object.
__ait_emit() {
    local hex
    hex=$(printf '%s' "$1" | __ait_hex)
    printf '\033]7700;%s\007' "$hex"
}

# The session id as a JSON string literal — always quoted, so a
# non-numeric or unset value can never produce invalid JSON (an
# undecodable payload moves nothing — the tab would silently never
# integrate). Unset reads as `""`, which matches no allocated id.
__ait_sid() {
    printf '"%s"' "$(__ait_json_escape "${AIT_SESSION_ID:-}")"
}

# A named hook whose value is just the session id.
__ait_hook() {
    __ait_emit "{\"hook\":\"$1\",\"value\":{\"session_id\":$(__ait_sid)}}"
}

# Escape a command string for JSON: quote, backslash and the named
# controls; strip remaining C0/DEL bytes — a sanitized command beats an
# invalid payload (the grid B→C slice stays the fallback source).
__ait_json_escape() {
    local s=$1
    s=${s//\\/\\\\}
    s=${s//\"/\\\"}
    s=${s//$'\b'/\\b}
    s=${s//$'\f'/\\f}
    s=${s//$'\n'/\\n}
    s=${s//$'\r'/\\r}
    s=${s//$'\t'/\\t}
    s=$(printf '%s' "$s" | tr -d '\000-\010\013\016-\037\177')
    printf '%s' "$s"
}

# --- §5.5.1 in-band exec ------------------------------------------------

# `__ait_run <id> <hex(command)>` — ait types this line on the prompt so
# in-band execution always leaves a trace (us-23). Runs the decoded
# command in a command-substitution subshell: the user's env, functions
# and cwd apply, and nothing renders in the grid. Replies on OSC 7701:
# A, then R frames of <=500 decoded bytes each (a 1000-char hex slice
# keeps every raw OSC under the 1024-byte vte budget), then B. The
# concatenated R payload is `<id>;<merged stdout+stderr>;<exit_code>`.
__ait_run() {
    local id=$1 hexcmd=$2
    local cmd capture out status hex hlen frag
    local pos=1
    cmd=$(printf '%b' "$(printf '%s' "$hexcmd" | sed 's/\(..\)/\\x\1/g')")
    capture=$(eval "$cmd" 2>&1; printf ':%d\n' "$?")
    status=${capture##*:}
    out=${capture%:*}
    hex=$(printf '%s' "$id;$out;$status" | __ait_hex)
    printf '\033]7701;A\007'
    hlen=${#hex}
    while (( pos <= hlen )); do
        frag=${hex[pos,pos+999]}
        printf '\033]7701;R;%d;%s\007' "$(( ${#frag} / 2 ))" "$frag"
        (( pos += 1000 ))
    done
    printf '\033]7701;B\007'
}

# --- prompt/command hooks -----------------------------------------------

# Runs FIRST in precmd_functions so `exit_status` is the real $?; the
# trailing `return` restores it for every later precmd entry.
__ait_zsh_precmd() {
    local exit_status=$?
    printf '\033]133;D;%d\007' "$exit_status"
    printf '\033]7;file://%s%s\007' "${HOST:-localhost}" "${PWD//\%/%25}"
    printf '\033]133;A\007'
    __ait_hook precmd
    return "$exit_status"
}

# 133;B right after the prompt is drawn — line-init fires as ZLE starts
# reading a fresh command line.
__ait_zsh_line_init() {
    printf '\033]133;B\007'
}

# Fallback for zsh < 5.3 (no add-zle-hook-widget): B rides precmd and
# lands on the prompt's first row.
__ait_zsh_precmd_b() {
    printf '\033]133;B\007'
}

# $1 is the command line exactly as typed.
__ait_zsh_preexec() {
    local command
    printf '\033]133;C\007'
    command=$(__ait_json_escape "$1")
    __ait_emit "{\"hook\":\"preexec\",\"value\":{\"session_id\":$(__ait_sid),\"command\":\"$command\"}}"
}

__ait_zsh_exit() {
    __ait_hook exit_shell
}

# --- install ------------------------------------------------------------

autoload -Uz add-zsh-hook
add-zsh-hook precmd __ait_zsh_precmd
add-zsh-hook preexec __ait_zsh_preexec
add-zsh-hook zshexit __ait_zsh_exit
# Front of the hook array sees the true $? (see __ait_zsh_precmd).
precmd_functions=(__ait_zsh_precmd ${precmd_functions:#__ait_zsh_precmd})

# zsh >= 5.3: add-zle-hook-widget puts B right after the drawn prompt;
# older zsh falls back to emitting it from precmd (prompt's first row).
autoload -Uz add-zle-hook-widget
if ! add-zle-hook-widget line-init __ait_zsh_line_init 2>/dev/null; then
    add-zsh-hook precmd __ait_zsh_precmd_b
fi

__ait_emit "{\"hook\":\"init_shell\",\"value\":{\"session_id\":$(__ait_sid),\"shell\":\"zsh\"}}"
__AIT_INTEGRATED_ZSH=1
