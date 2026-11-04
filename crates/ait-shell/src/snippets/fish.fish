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
    set -l sid (__ait_sid)
    __ait_emit "{\"hook\":\"bootstrapped\",\"value\":{\"session_id\":$sid}}"
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
    set -l sid (__ait_sid)
    __ait_emit "{\"hook\":\"$name\",\"value\":{\"session_id\":$sid}}"
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
    set -l sid (__ait_sid)
    __ait_emit "{\"hook\":\"preexec\",\"value\":{\"session_id\":$sid,\"command\":\"$escaped\"}}"
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

# --- §5.9 ssh integration -------------------------------------------------
#
# The same wrapper bash/zsh carry (Spec R09-T3): an interactive ssh to
# an allowed host is rewritten to `ssh -t <args> '<remote bootstrap>'`,
# bracketed by ssh_begin/ssh_end, with ControlMaster options attached
# when no user socket already multiplexes the host. Every other shape —
# `ssh host cmd`, denied hosts, `AIT_SSH_OFF`, control/config
# operations — is a plain `command ssh` passthrough.

# $argv[1] = destination token, $argv[2] = newline-separated glob list —
# OpenSSH Host-pattern semantics: `*`/`?` globs, a leading `!` negates.
# Exit 0 when a non-negated entry matched.
function __ait_ssh_match -a dest patterns
    set -l matched 1
    for pattern in (string split \n -- "$patterns")
        test -z "$pattern"; and continue
        if string match -q '!*' -- "$pattern"
            set -l negated (string sub -s 2 -- "$pattern")
            string match -q -- "$negated" "$dest"; and return 1
        else if string match -q -- "$pattern" "$dest"
            set matched 0
        end
    end
    return $matched
end

# Deny beats allow (§5.7's rule); an empty allowlist is open.
function __ait_ssh_allowed -a dest
    if __ait_ssh_match "$dest" "$AIT_SSH_DENY"
        return 1
    end
    if test -n "$AIT_SSH_ALLOW"; and not __ait_ssh_match "$dest" "$AIT_SSH_ALLOW"
        return 1
    end
    return 0
end

# Print the destination token of a plain interactive `ssh` argv —
# `ssh [opts] dest` — and exit 0. Anything that is not a bare
# interactive login exits 1: a remote command (a second positional),
# a missing destination, or an option the rewrite must not touch.
function __ait_ssh_dest
    set -l dest ""
    set -l expects 0
    for arg in $argv
        if test $expects -eq 1
            set expects 0
            continue
        end
        switch $arg
            case -G -N -T -n -f -M -V -O '-O*'
                # Control/config/proxy shapes never get a remote shell.
                return 1
            case -b -B -c -D -E -e -F -I -i -J -L -l -m -o -p -Q -R -S -W -w
                # Options whose argument is the NEXT token (bundled
                # forms like -p22 fall to -* → ignored).
                set expects 1
            case -- '-*'
                # A flag option or end-of-options.
            case '*'
                # A second positional is the remote command → not
                # interactive.
                test -n "$dest"; and return 1
                set dest $arg
        end
    end
    test -z "$dest"; and return 1
    printf '%s' "$dest"
end

# Does the user's own ssh config already multiplex `dest`? `ssh -G`
# answers with a real ControlPath and a multiplexing ControlMaster —
# reuse_existing_control_master=1 then adds nothing.
function __ait_ssh_user_master -a dest
    set -l conf (command ssh -G "$dest" 2>/dev/null)
    or return 1
    set -l master ""
    set -l path ""
    for line in $conf
        if string match -q 'controlmaster *' -- "$line"
            set master (string replace 'controlmaster ' '' -- "$line")
        else if string match -q 'controlpath *' -- "$line"
            set path (string replace 'controlpath ' '' -- "$line")
        end
    end
    switch $master
        case auto autoask yes ask true 1
            test -n "$path"; and test "$path" != none
        case '*'
            return 1
    end
end

function __ait_ssh_begin -a dest control_path
    set -l bootstrapped false
    set -q AIT_SSH_BOOTSTRAP; and set bootstrapped true
    set -l cp ""
    test -n "$control_path"; and set cp ",\"control_path\":\"$control_path\""
    set -l escaped (__ait_json_escape "$dest")
    set -l sid (__ait_sid)
    __ait_emit "{\"hook\":\"ssh_begin\",\"value\":{\"session_id\":$sid,\"target\":\"$escaped\",\"bootstrapped\":$bootstrapped$cp}}"
end

function __ait_ssh_end -a dest
    set -l escaped (__ait_json_escape "$dest")
    set -l sid (__ait_sid)
    __ait_emit "{\"hook\":\"ssh_end\",\"value\":{\"session_id\":$sid,\"target\":\"$escaped\"}}"
end

# The remote command: re-export AIT_SSH_BOOTSTRAP into the remote
# session (a nested ssh() sends the same script one hop further),
# then run it.
function __ait_ssh_remote_cmd
    set -l quoted (string replace -a "'" "'\\''" -- "$AIT_SSH_BOOTSTRAP")
    printf '%s' "export AIT_SSH_BOOTSTRAP='$quoted'
$AIT_SSH_BOOTSTRAP"
end

function ssh
    # Opt-outs first: AIT_SSH_OFF per session, the armed-off policy.
    if set -q AIT_SSH_OFF; and test -n "$AIT_SSH_OFF"
        command ssh $argv
        return
    end
    set -l enabled 1
    set -q AIT_SSH_ENABLED; and set enabled $AIT_SSH_ENABLED
    if test "$enabled" != 1
        command ssh $argv
        return
    end
    set -l dest (__ait_ssh_dest $argv)
    or begin
        command ssh $argv
        return
    end
    if not __ait_ssh_allowed "$dest"
        command ssh $argv
        return
    end
    set -l reuse 1
    set -q AIT_SSH_REUSE_CM; and set reuse $AIT_SSH_REUSE_CM
    set -l persist 10m
    set -q AIT_SSH_CONTROL_PERSIST; and set persist $AIT_SSH_CONTROL_PERSIST
    set -l extra
    if test "$reuse" = 1; and __ait_ssh_user_master "$dest"
        set extra
    else if test -n "$AIT_SSH_CONTROL_DIR"
        set extra -o ControlMaster=auto -o "ControlPath=$AIT_SSH_CONTROL_DIR/%C" -o "ControlPersist=$persist"
    end
    set -l gconf (command ssh -G $extra "$dest" 2>/dev/null)
    set -l control_path ""
    for line in $gconf
        string match -q 'controlpath *' -- "$line"; and set control_path (string replace 'controlpath ' '' -- "$line")
    end
    __ait_ssh_begin "$dest" "$control_path"
    if set -q AIT_SSH_BOOTSTRAP; and test -n "$AIT_SSH_BOOTSTRAP"
        command ssh -t $extra $argv (__ait_ssh_remote_cmd)
    else
        command ssh -t $extra $argv
    end
    set -l rc $status
    __ait_ssh_end "$dest"
    # The remote session demoted this tab when its exit_shell landed;
    # the local shell is back — re-integrate it.
    __ait_hook bootstrapped
    return $rc
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

# Spec R09-T3: `exit_shell` on a hangup too — a dropped connection
# lands at SIGHUP, not fish_exit.
function __ait_fish_hup --on-signal SIGHUP
    __ait_hook exit_shell
    exit
end

# --- install ------------------------------------------------------------

set -l sid (__ait_sid)
__ait_emit "{\"hook\":\"init_shell\",\"value\":{\"session_id\":$sid,\"shell\":\"fish\"}}"
set -g __AIT_INTEGRATED_FISH 1
