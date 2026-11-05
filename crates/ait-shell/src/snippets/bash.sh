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

# The `,"hop":"<id>"` tail a remote shell's hooks carry — the
# wrapping ssh() exported AIT_SSH_HOP into this session so every
# nested level's emissions attribute to their level (§5.9). Empty on
# a local shell.
__ait_hop() {
    if [[ -n ${AIT_SSH_HOP:-} ]]; then
        printf ',"hop":"%s"' "$(__ait_json_escape "$AIT_SSH_HOP")"
    fi
}

# A named hook whose value is just the session id (plus the hop
# attribution a remote shell adds).
__ait_hook() {
    __ait_emit "{\"hook\":\"$1\",\"value\":{\"session_id\":$(__ait_sid)$(__ait_hop)}}"
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
    __ait_emit "{\"hook\":\"preexec\",\"value\":{\"session_id\":$(__ait_sid)$(__ait_hop),\"command\":\"$command\"}}"
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

# --- §5.9 ssh integration -------------------------------------------------
#
# When the [ssh] policy env is armed at spawn (`AIT_SSH_*`), `ssh`
# becomes a function: an interactive ssh to an allowed host is rewritten
# to `ssh -t <args> '<remote bootstrap>'`, bracketed by the ssh_begin /
# ssh_end hooks, with ControlMaster options attached when no user socket
# already multiplexes the host. Every other shape — `ssh host cmd`,
# denied hosts, `AIT_SSH_OFF=1`, control/config operations — is a plain
# `command ssh` passthrough, and `command ssh` typed directly always
# bypasses the wrapper.

# Match the destination token $1 against the newline-separated glob list
# $2 — OpenSSH Host-pattern semantics: `*`/`?` globs, a leading `!`
# negates. Exit 0 when a non-negated entry matched (last match wins).
__ait_ssh_match() {
    local dest=$1 pattern
    local matched=1
    while IFS= read -r pattern; do
        [[ -z $pattern ]] && continue
        if [[ $pattern == \!* ]]; then
            [[ $dest == ${pattern#!} ]] && return 1
        elif [[ $dest == $pattern ]]; then
            matched=0
        fi
    done <<< "$2"
    return "$matched"
}

# The armed allow/deny decision for one destination token — deny beats
# allow (§5.7's rule); an empty allowlist is open.
__ait_ssh_allowed() {
    if __ait_ssh_match "$1" "${AIT_SSH_DENY:-}"; then
        return 1
    fi
    if [[ -n ${AIT_SSH_ALLOW:-} ]] && ! __ait_ssh_match "$1" "$AIT_SSH_ALLOW"; then
        return 1
    fi
    return 0
}

# Print the destination token of a plain interactive `ssh` argv —
# `ssh [opts] dest` — and exit 0. Anything that is not a bare
# interactive login exits 1, which the wrapper reads as "passthrough":
# a remote command (tokens after the destination), a missing
# destination, or an option the rewrite must not touch
# (-G -N -T -n -f -M -O -V).
__ait_ssh_dest() {
    local -a argv=("$@")
    local i arg expects=0
    for ((i = 0; i < ${#argv[@]}; i++)); do
        arg=${argv[i]}
        if ((expects)); then
            expects=0
            continue
        fi
        case $arg in
            # Control/config/proxy shapes never get a remote shell.
            -G|-N|-T|-n|-f|-M|-V|-O|-O*)
                return 1 ;;
            # Options whose argument is the NEXT token.
            -[bBcDEeFIiJLlmopQRSWw])
                expects=1 ;;
            # Bundled-argument options (-p22, -oKey=val, …).
            -[bBcDEeFIiJLlmopQRSWw]*)
                ;;
            --)
                ;; # end-of-options: the destination follows
            -*)
                ;; # a flag option (-t -v -4 …)
            *)
                # First non-option token: the destination. Anything
                # after it is the remote command → not interactive.
                if ((i + 1 < ${#argv[@]})); then
                    return 1
                fi
                printf '%s' "$arg"
                return 0 ;;
        esac
    done
    return 1
}

# Does a `ssh -G` result already multiplex the host? controlmaster
# says a multiplexing mode AND controlpath resolves to a real socket
# — reuse_existing_control_master=1 then adds nothing (the user's
# socket is reused, so no second authentication ever happens).
__ait_ssh_mastered() { # $1 = controlmaster value, $2 = controlpath value
    case $1 in
        auto|autoask|yes|ask|true|1)
            [[ -n $2 && $2 != none ]] ;;
        *)
            return 1 ;;
    esac
}

# ssh_begin: session id, the typed destination, this hop's id,
# whether a bootstrap is sent and the negotiated ControlPath pattern.
__ait_ssh_begin() {
    local bootstrapped=false cp=""
    [[ -n ${AIT_SSH_BOOTSTRAP:-} ]] && bootstrapped=true
    [[ -n $2 && $2 != none ]] && cp=",\"control_path\":\"$(__ait_json_escape "$2")\""
    __ait_emit "{\"hook\":\"ssh_begin\",\"value\":{\"session_id\":$(__ait_sid),\"target\":\"$(__ait_json_escape "$1")\",\"hop\":\"$(__ait_json_escape "$3")\",\"bootstrapped\":$bootstrapped$cp}}"
}

# ssh_end: the local `ssh` returned — the remote session is over
# (cleanly or by a dropped connection).
__ait_ssh_end() {
    __ait_emit "{\"hook\":\"ssh_end\",\"value\":{\"session_id\":$(__ait_sid),\"target\":\"$(__ait_json_escape "$1")\",\"hop\":\"$(__ait_json_escape "$2")\"}}"
}

# The remote command: export this hop's id and re-export
# AIT_SSH_BOOTSTRAP into the remote session (single-quoted, the only
# self-reference the protocol needs — a nested ssh() sends the same
# script one hop further), then run it.
__ait_ssh_remote_cmd() { # $1 = this hop's id
    local quoted=${AIT_SSH_BOOTSTRAP//"'"/"'\\''"}
    local hop=${1//"'"/"'\\''"}
    printf '%s' "export AIT_SSH_HOP='$hop'
export AIT_SSH_BOOTSTRAP='$quoted'
$AIT_SSH_BOOTSTRAP"
}

ssh() {
    # Opt-outs first: `AIT_SSH_OFF=1` per session, the armed-off policy —
    # the wrapper must never change what the user asked for.
    if [[ -n ${AIT_SSH_OFF:-} || ${AIT_SSH_ENABLED:-1} != 1 ]]; then
        command ssh "$@"
        return
    fi
    local dest
    if ! dest=$(__ait_ssh_dest "$@"); then
        command ssh "$@"
        return
    fi
    if ! __ait_ssh_allowed "$dest"; then
        command ssh "$@"
        return
    fi
    # One `ssh -G` answers both ControlMaster questions: whether the
    # user's own config already multiplexes `dest` (reuse attaches
    # nothing — their socket carries the commands) and, when it does
    # not, the ControlPath the session still reports so the executor
    # can try it.
    local conf master control_path
    conf=$(command ssh -G "$dest" 2>/dev/null)
    master=$(printf '%s\n' "$conf" | sed -n 's/^controlmaster //p' | head -n1)
    control_path=$(printf '%s\n' "$conf" | sed -n 's/^controlpath //p' | head -n1)
    local -a extra=()
    if [[ ${AIT_SSH_REUSE_CM:-1} == 1 ]] && __ait_ssh_mastered "$master" "$control_path"; then
        :
    elif [[ -n ${AIT_SSH_CONTROL_DIR:-} ]]; then
        extra=(-o "ControlMaster=auto"
               -o "ControlPath=${AIT_SSH_CONTROL_DIR}/%C"
               -o "ControlPersist=${AIT_SSH_CONTROL_PERSIST:-10m}")
        # The effective path with our -o adds and any of the user's
        # own -o overrides applied — a second -G only when the attach
        # actually changes argv.
        control_path=$(command ssh -G "${extra[@]}" "$@" 2>/dev/null \
            | sed -n 's/^controlpath //p' | head -n1)
    fi
    # This hop's identity — attributes this session's ssh_begin /
    # ssh_end and (via the remote env) the remote shell's own hooks
    # to this level of a possibly-nested chain.
    local hop="${AIT_SESSION_ID:-0}.$$.$((__AIT_SSH_SEQ += 1))"
    __ait_ssh_begin "$dest" "$control_path" "$hop"
    if [[ -n ${AIT_SSH_BOOTSTRAP:-} ]]; then
        command ssh -t "${extra[@]}" "$@" "$(__ait_ssh_remote_cmd "$hop")"
    else
        command ssh -t "${extra[@]}" "$@"
    fi
    local status=$?
    __ait_ssh_end "$dest" "$hop"
    # The remote session demoted this tab when its exit_shell landed
    # (or a drop killed it); the local shell is back at the prompt —
    # re-integrate it.
    __ait_hook bootstrapped
    return "$status"
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
# Local snippets trap EXIT only — remote-session HUP coverage is the
# remote bootstrap script's own trap (§5.9).
__AIT_SSH_SEQ=0

__ait_emit "{\"hook\":\"init_shell\",\"value\":{\"session_id\":$(__ait_sid),\"shell\":\"bash\"$(__ait_hop)}}"
__AIT_INTEGRATED_BASH=1
