# ait shell integration for bash — REQUIREMENTS.md §5.5 / §5.5.1.
# Sourced from the `--rcfile` wrapper after the user's real ~/.bashrc.
# Emits OSC 133 A/B/C/D prompt marks, OSC 7 cwd reports, and the five
# OSC 7700 hooks as hex-JSON; defines __ait_run for in-band exec.
#
# Re-sourcing in the same shell is a no-op: the guard fires once, so a
# second source reports `bootstrapped` (still integrated) and bails. A
# nested bash never sees this file — --rcfile is not inherited — so it
# degrades to a plain shell instead of double-hooking (Spec R05 us-27).
if [[ -n ${__AIT_INTEGRATED_BASH:-} ]] && declare -F __ait_emit >/dev/null 2>&1; then
    __ait_emit "{\"hook\":\"bootstrapped\",\"value\":{\"session_id\":${AIT_SESSION_ID:-null}}}"
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

# A named hook whose value is just the session id (null when unset).
__ait_hook() {
    __ait_emit "{\"hook\":\"$1\",\"value\":{\"session_id\":${AIT_SESSION_ID:-null}}}"
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

# preexec carries the typed command text alongside the session id.
__ait_hook_preexec() {
    local command
    command=$(__ait_json_escape "$1")
    __ait_emit "{\"hook\":\"preexec\",\"value\":{\"session_id\":${AIT_SESSION_ID:-null},\"command\":\"$command\"}}"
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
    local cmd capture out status hex hlen pos frag
    cmd=$(printf '%b' "$(printf '%s' "$hexcmd" | sed 's/\(..\)/\\x\1/g')")
    capture=$(eval "$cmd" 2>&1; printf ':%d\n' "$?")
    status=${capture##*:}
    out=${capture%:*}
    hex=$(printf '%s' "$id;$out;$status" | __ait_hex)
    printf '\033]7701;A\007'
    hlen=${#hex}
    for ((pos = 0; pos < hlen; pos += 1000)); do
        frag=${hex:pos:1000}
        printf '\033]7701;R;%d;%s\007' "$((${#frag} / 2))" "$frag"
    done
    printf '\033]7701;B\007'
}

# --- prompt/command hooks -----------------------------------------------

# precmd: close the previous block (133;D;<exit>), report cwd (OSC 7),
# open the next prompt (133;A / 133;B) and fire the hook. Runs the user's
# original PROMPT_COMMAND first so `$?`-reading prompt code stays true.
__ait_precmd() {
    local exit_status=$?
    __AIT_INSIDE_PRECMD=1
    local element
    for element in "${__AIT_ORIG_PROMPT_COMMAND[@]}"; do
        eval "$element"
    done
    printf '\033]133;D;%d\007' "$exit_status"
    printf '\033]7;file://%s%s\007' "${HOSTNAME:-localhost}" "${PWD//%/%25}"
    printf '\033]133;A\007'
    printf '\033]133;B\007'
    __ait_hook precmd
    __AIT_INSIDE_PRECMD=
    __AIT_AT_PROMPT=1
    return "$exit_status"
}

# bash has no preexec: the DEBUG trap fires before *every* simple
# command, so only the first one after a rendered prompt is the typed
# command. The inside-precmd flag keeps PROMPT_COMMAND internals from
# tripping it (and consuming the at-prompt flag early).
__ait_preexec() {
    [[ -z ${__AIT_AT_PROMPT:-} || -n ${__AIT_INSIDE_PRECMD:-} ]] && return 0
    __AIT_AT_PROMPT=
    printf '\033]133;C\007'
    __ait_hook_preexec "$BASH_COMMAND"
}

__ait_exit_shell() {
    __ait_hook exit_shell
}

# --- install ------------------------------------------------------------

# Preserve the user's PROMPT_COMMAND (string or bash>=5.1 array) — it
# runs inside __ait_precmd instead of being replaced.
if declare -p PROMPT_COMMAND 2>/dev/null | grep -q '^declare \-a'; then
    __AIT_ORIG_PROMPT_COMMAND=("${PROMPT_COMMAND[@]}")
elif [[ -n ${PROMPT_COMMAND:-} ]]; then
    __AIT_ORIG_PROMPT_COMMAND=("$PROMPT_COMMAND")
else
    __AIT_ORIG_PROMPT_COMMAND=()
fi
PROMPT_COMMAND=__ait_precmd
trap '__ait_preexec' DEBUG
trap '__ait_exit_shell' EXIT

__ait_emit "{\"hook\":\"init_shell\",\"value\":{\"session_id\":${AIT_SESSION_ID:-null},\"shell\":\"bash\"}}"
__AIT_INTEGRATED_BASH=1
