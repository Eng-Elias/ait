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
    # `status` is zsh's read-only $? alias — `local` on it aborts the
    # function; `code` carries the exit instead.
    local cmd capture out code hex hlen frag
    local pos=1
    cmd=$(printf '%b' "$(printf '%s' "$hexcmd" | sed 's/\(..\)/\\x\1/g')")
    capture=$(eval "$cmd" 2>&1; printf ':%d\n' "$?")
    code=${capture##*:}
    out=${capture%:*}
    hex=$(printf '%s' "$id;$out;$code" | __ait_hex)
    printf '\033]7701;A\007'
    hlen=${#hex}
    while (( pos <= hlen )); do
        frag=${hex[pos,pos+999]}
        printf '\033]7701;R;%d;%s\007' "$(( ${#frag} / 2 ))" "$frag"
        (( pos += 1000 ))
    done
    printf '\033]7701;B\007'
}

# --- §5.9 ssh integration -------------------------------------------------
#
# The same wrapper bash carries (Spec R09-T3): an interactive ssh to an
# allowed host is rewritten to `ssh -t <args> '<remote bootstrap>'`,
# bracketed by ssh_begin/ssh_end, with ControlMaster options attached
# when no user socket already multiplexes the host. Every other shape —
# `ssh host cmd`, denied hosts, `AIT_SSH_OFF`, control/config
# operations — is a plain `command ssh` passthrough.

# $1 = destination token, $2 = newline-separated glob list — OpenSSH
# Host-pattern semantics: `*`/`?` globs (via ${~pattern}), a leading
# `!` negates. Exit 0 when a non-negated entry matched.
__ait_ssh_match() {
    local dest=$1 pattern negated
    local matched=1
    while IFS= read -r pattern; do
        [[ -z $pattern ]] && continue
        if [[ $pattern == \!* ]]; then
            negated=${pattern#!}
            [[ $dest == ${~negated} ]] && return 1
        elif [[ $dest == ${~pattern} ]]; then
            matched=0
        fi
    done <<< "$2"
    return "$matched"
}

# Deny beats allow (§5.7's rule); an empty allowlist is open.
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
# interactive login exits 1: a remote command (a second positional),
# a missing destination, or an option the rewrite must not touch.
__ait_ssh_dest() {
    local dest="" arg expects=0
    for arg in "$@"; do
        if (( expects )); then
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
            --|-*)
                ;; # a flag option or end-of-options
            *)
                # A second positional is the remote command → not
                # interactive.
                [[ -n $dest ]] && return 1
                dest=$arg ;;
        esac
    done
    [[ -z $dest ]] && return 1
    printf '%s' "$dest"
}

# Does the user's own ssh config already multiplex `dest`? `ssh -G`
# answers with a real ControlPath and a multiplexing ControlMaster —
# reuse_existing_control_master=1 then adds nothing.
__ait_ssh_user_master() {
    local conf master path
    conf=$(command ssh -G "$1" 2>/dev/null) || return 1
    master=$(printf '%s\n' "$conf" | sed -n 's/^controlmaster //p' | head -n1)
    path=$(printf '%s\n' "$conf" | sed -n 's/^controlpath //p' | head -n1)
    case $master in
        auto|autoask|yes|ask|true|1)
            [[ -n $path && $path != none ]] ;;
        *)
            return 1 ;;
    esac
}

__ait_ssh_begin() {
    local bootstrapped=false cp=""
    [[ -n ${AIT_SSH_BOOTSTRAP:-} ]] && bootstrapped=true
    [[ -n $2 ]] && cp=",\"control_path\":\"$(__ait_json_escape "$2")\""
    __ait_emit "{\"hook\":\"ssh_begin\",\"value\":{\"session_id\":$(__ait_sid),\"target\":\"$(__ait_json_escape "$1")\",\"bootstrapped\":$bootstrapped$cp}}"
}

__ait_ssh_end() {
    __ait_emit "{\"hook\":\"ssh_end\",\"value\":{\"session_id\":$(__ait_sid),\"target\":\"$(__ait_json_escape "$1")\"}}"
}

# The remote command: re-export AIT_SSH_BOOTSTRAP into the remote
# session (a nested ssh() sends the same script one hop further),
# then run it.
__ait_ssh_remote_cmd() {
    local quoted=${AIT_SSH_BOOTSTRAP//"'"/"'\\''"}
    printf '%s' "export AIT_SSH_BOOTSTRAP='$quoted'
$AIT_SSH_BOOTSTRAP"
}

ssh() {
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
    local -a extra=()
    if [[ ${AIT_SSH_REUSE_CM:-1} == 1 ]] && __ait_ssh_user_master "$dest"; then
        :
    elif [[ -n ${AIT_SSH_CONTROL_DIR:-} ]]; then
        extra=(-o "ControlMaster=auto"
               -o "ControlPath=${AIT_SSH_CONTROL_DIR}/%C"
               -o "ControlPersist=${AIT_SSH_CONTROL_PERSIST:-10m}")
    fi
    local control_path
    control_path=$(command ssh -G "${extra[@]}" "$dest" 2>/dev/null \
        | sed -n 's/^controlpath //p' | head -n1)
    __ait_ssh_begin "$dest" "$control_path"
    if [[ -n ${AIT_SSH_BOOTSTRAP:-} ]]; then
        command ssh -t "${extra[@]}" "$@" "$(__ait_ssh_remote_cmd)"
    else
        command ssh -t "${extra[@]}" "$@"
    fi
    # `status` is zsh's read-only $? alias — `local` on it aborts the
    # function mid-wrap; `rc` preserves the ssh exit instead.
    local rc=$?
    __ait_ssh_end "$dest"
    # The remote session demoted this tab when its exit_shell landed;
    # the local shell is back — re-integrate it.
    __ait_hook bootstrapped
    return "$rc"
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
# Spec R09-T3: `exit_shell` on a hangup too — a dropped connection
# lands at HUP, not zshexit.
trap '__ait_zsh_exit; exit' HUP
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
