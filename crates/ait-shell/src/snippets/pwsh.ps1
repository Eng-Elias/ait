# ait shell integration for PowerShell — REQUIREMENTS.md §5.5 / §5.5.1 /
# §5.14. One snippet serves pwsh 7 and Windows PowerShell 5.1 alike;
# every piece below is PS 5.1-safe. Dot-sourced via
# `-NoExit -ExecutionPolicy Bypass -Command ". '<dir>\snippet.ps1'"`
# after the user's profiles have loaded, so the hooks install on top
# of the real environment. Emits OSC 133 A/B/C/D prompt marks, OSC 7
# cwd reports, and the five OSC 7700 hooks as hex-JSON; defines
# __ait_run for in-band exec.
#
# Re-sourcing in the same session is a no-op: the guard fires once, so
# a second dot-source reports `bootstrapped` (still integrated) and
# bails (Spec R05 us-27 parity).
if ($global:__AIT_INTEGRATED_PWSH -and
    (Get-Command __ait_emit -ErrorAction SilentlyContinue)) {
    __ait_emit ('{"hook":"bootstrapped","value":{"session_id":' +
        (__ait_sid) + (__ait_hop) + '}}')
    return
}

# UTF-8 on the console so hook JSON and cwd text survive the round
# trip; each setter fails soft on hosts that reject it (redirected
# streams, Server Core).
try { [Console]::InputEncoding  = [System.Text.Encoding]::UTF8 } catch { }
try { [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 } catch { }
$OutputEncoding = [System.Text.Encoding]::UTF8

$global:__AIT_ESC = [char]27
$global:__AIT_BEL = [char]7

# --- emit helpers -------------------------------------------------------

# OSC bytes go straight to the console stream: inside `prompt` any
# function *output* would render as prompt text instead of reaching
# the terminal.
function global:__ait_osc {
    param([string]$Body)
    [Console]::Out.Write("$global:__AIT_ESC]$Body$global:__AIT_BEL")
}

# Hex-encode a string as lowercase UTF-8 hex — the only alphabet the
# 7700 codec reads. The Unix snippets' `od`/`xxd` don't exist on
# Windows, so the encoder is all .NET.
function global:__ait_hex {
    param([AllowNull()][AllowEmptyString()][string]$Text)
    if ($null -eq $Text) { return '' }
    -join ([System.Text.Encoding]::UTF8.GetBytes($Text) |
        ForEach-Object { $_.ToString('x2') })
}

# Decode hex back to a UTF-8 string; odd-length input yields '' — a
# torn `__ait_run` line degrades to a no-op exec, never a panic (§5.5).
function global:__ait_unhex {
    param([AllowNull()][AllowEmptyString()][string]$Hex)
    if ($null -eq $Hex -or ($Hex.Length % 2) -ne 0) { return '' }
    $bytes = New-Object 'byte[]' ($Hex.Length / 2)
    for ($i = 0; $i -lt $Hex.Length; $i += 2) {
        $bytes[[int]($i / 2)] = [Convert]::ToByte($Hex.Substring($i, 2), 16)
    }
    [System.Text.Encoding]::UTF8.GetString($bytes)
}

# OSC 7700 hook event; $Json is a complete {"hook":…,"value":…} object.
function global:__ait_emit {
    param([string]$Json)
    __ait_osc ('7700;' + (__ait_hex $Json))
}

# Escape a string for embedding in JSON: quote, backslash and the named
# controls; strip the remaining C0/DEL bytes — a sanitized command
# beats an invalid payload (the grid B→C slice stays the fallback).
function global:__ait_json_escape {
    param([AllowNull()][string]$Text)
    if ($null -eq $Text) { return '' }
    $Text = $Text.Replace('\', '\\')
    $Text = $Text.Replace('"', '\"')
    $Text = $Text.Replace("`b", '\b')
    $Text = $Text.Replace("`f", '\f')
    $Text = $Text.Replace("`n", '\n')
    $Text = $Text.Replace("`r", '\r')
    $Text = $Text.Replace("`t", '\t')
    $Text -replace "[\x00-\x08\x0b\x0e-\x1f\x7f]", ''
}

# The session id as a JSON string literal — always quoted, so a
# non-numeric or unset value can never produce invalid JSON (an
# undecodable payload moves nothing — the tab would silently never
# integrate). Unset reads as `""`, which matches no allocated id.
function global:__ait_sid {
    '"' + (__ait_json_escape "$env:AIT_SESSION_ID") + '"'
}

# The `,"hop":"<id>"` tail a remote shell's hooks carry — the
# wrapping ssh() exported AIT_SSH_HOP into this session so every
# nested level's emissions attribute to their level (§5.9). Empty on
# a local shell.
function global:__ait_hop {
    if ($env:AIT_SSH_HOP) {
        ',"hop":"' + (__ait_json_escape $env:AIT_SSH_HOP) + '"'
    }
}

# A named hook whose value is just the session id (plus the hop
# attribution a remote shell adds).
function global:__ait_hook {
    param([string]$Name)
    __ait_emit ('{"hook":"' + $Name + '","value":{"session_id":' +
        (__ait_sid) + (__ait_hop) + '}}')
}

# preexec carries the typed command text alongside the session id.
function global:__ait_hook_preexec {
    param([string]$Command)
    __ait_emit ('{"hook":"preexec","value":{"session_id":' +
        (__ait_sid) + (__ait_hop) + ',"command":"' +
        (__ait_json_escape $Command) + '"}}')
}

# --- §5.5.1 in-band exec ------------------------------------------------

# `__ait_run <id> <hex(command)>` — ait types this line on the prompt so
# in-band execution always leaves a trace (us-23). The decoded command
# runs as a script block inside this function's CHILD scope: session
# functions, aliases and cwd all resolve for it (the same "user's own
# shell" guarantee the Unix `eval` gives), and a payload Set-Location
# persists — location is session-global, not scope-bound — but
# variables and functions the payload defines die with the call
# (literal session scope isn't reachable from a function in
# PowerShell; fine for run_command payloads, which are command lines,
# not session setup). Every stream merges `*>&1` like bash's `2>&1`
# capture. Replies on OSC 7701: an `A` opener, then R frames of
# <=500 decoded bytes each (a 1000-char hex slice keeps every raw
# OSC under the 1024-byte vte budget), then `B`.
# The concatenated R payload is `<id>;<merged>;<exit_code>` — merged
# output may itself contain `;`, the codec splits first and last only.
# Exit code: the native `$LASTEXITCODE` when the payload ran one, else
# 0/1 from `$?` (a cmdlet error is 1). PowerShell's stream formatting
# makes byte-exactness best-effort; the contract is merged output +
# exact exit code.
function global:__ait_run {
    param([string]$Id, [string]$HexCmd)
    $cmd = __ait_unhex $HexCmd
    $merged = ''
    $code = 0
    $global:LASTEXITCODE = $null
    try {
        $merged = (& ([ScriptBlock]::Create($cmd)) *>&1 |
            Out-String -Width 4096)
        if ($null -ne $global:LASTEXITCODE) {
            $code = [int]$global:LASTEXITCODE
        } elseif (-not $?) {
            $code = 1
        }
    } catch {
        $merged = ($_ | Out-String -Width 4096)
        $code = 1
    }
    $hex = __ait_hex ($Id + ';' + $merged + ';' + $code)
    __ait_osc '7701;A'
    for ($pos = 0; $pos -lt $hex.Length; $pos += 1000) {
        $frag = $hex.Substring($pos, [Math]::Min(1000, $hex.Length - $pos))
        __ait_osc ('7701;R;' + ($frag.Length / 2) + ';' + $frag)
    }
    __ait_osc '7701;B'
}

# --- §5.9 ssh integration -------------------------------------------------
#
# When the [ssh] policy env is armed at spawn (`AIT_SSH_*`), `ssh` is
# a function: an interactive ssh to an allowed host is rewritten to
# `ssh -t <args> '<remote bootstrap>'`, bracketed by the ssh_begin /
# ssh_end hooks. Windows OpenSSH has no connection multiplexing, so
# the Unix wrappers' attach leg does not exist here — the wrapper
# emits no `Control*` options, runs no `-G` config probe (there is no
# socket to discover) and carries no control-path field in ssh_begin;
# the reuse/control-dir env stays armed-but-unread. The remote
# bootstrap itself is POSIX sh on the far side — unchanged (§5.14).
# Every other shape — `ssh host cmd`, denied hosts, a non-empty
# `AIT_SSH_OFF`, control/config operations — is a plain passthrough,
# and spelling the client `ssh.exe` always bypasses the wrapper (the
# function shadows the bare `ssh` name only).

# The real ssh client, resolved per call through Get-Command's
# Application type — a bare name lookup would find this `ssh`
# function and recurse; per-call resolution also picks up PATH edits
# mid-session and PATHEXT shims (a `ssh.cmd` ahead of OpenSSH's exe).
function global:__ait_ssh_program {
    $app = Get-Command ssh -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -ne $app) { return $app.Source }
    # No ssh on PATH at all — the literal spells the same
    # not-recognized error an unintegrated session would show.
    return 'ssh.exe'
}

# One OpenSSH Host-pattern against a token: `*`/`?` globs only.
# PowerShell's -like adds `[…]` classes OpenSSH doesn't have, so the
# pattern's brackets and backticks are escaped first — and -clike
# keeps the case-sensitive read the Unix `[[ == ]]` globs make.
function global:__ait_ssh_glob {
    param([string]$Text, [string]$Pattern)
    $escaped = $Pattern.Replace('`', '``').Replace('[', '`[').Replace(']', '`]')
    return ($Text -clike $escaped)
}

# Match $Dest against the newline-separated glob list $Patterns —
# the Unix `__ait_ssh_match` semantics: a matching `!` entry vetoes
# outright, any non-negated match returns $true.
function global:__ait_ssh_match {
    param(
        [string]$Dest,
        [AllowNull()][AllowEmptyString()][string]$Patterns
    )
    if ([string]::IsNullOrEmpty($Patterns)) { return $false }
    $matched = $false
    foreach ($pattern in ($Patterns -split "`n")) {
        $pattern = $pattern.TrimEnd("`r")
        if ($pattern.Length -eq 0) { continue }
        if ($pattern.StartsWith('!')) {
            if (__ait_ssh_glob $Dest ($pattern.Substring(1))) { return $false }
        } elseif (__ait_ssh_glob $Dest $pattern) {
            $matched = $true
        }
    }
    return $matched
}

# The armed allow/deny decision for one destination token — deny
# beats allow (§5.7's rule); an empty allowlist is open.
function global:__ait_ssh_allowed {
    param([string]$Dest)
    if (__ait_ssh_match $Dest $env:AIT_SSH_DENY) { return $false }
    if (-not [string]::IsNullOrEmpty($env:AIT_SSH_ALLOW) -and
        -not (__ait_ssh_match $Dest $env:AIT_SSH_ALLOW)) {
        return $false
    }
    return $true
}

# The destination token of a plain interactive `ssh` argv —
# `ssh [opts] dest` — or $null for anything the rewrite must not
# touch: a remote command (any token after the destination), a
# missing destination, or a control/config shape. Case matters —
# `-v` is a flag, `-V` is the version passthrough — so the
# comparisons below are the case-sensitive spellings.
function global:__ait_ssh_dest {
    param([string[]]$Argv)
    $expectArg = $false
    for ($i = 0; $i -lt $Argv.Count; $i++) {
        $arg = $Argv[$i]
        if ($expectArg) { $expectArg = $false; continue }
        # Control/config/proxy shapes never get a remote shell.
        if ($arg -cin @('-G', '-N', '-T', '-n', '-f', '-M', '-V') -or
            $arg -clike '-O*') {
            return $null
        }
        if ($arg -match '^-[bBcDEeFIiJLlmopQRSWw]$') {
            # Options whose argument is the NEXT token.
            $expectArg = $true
            continue
        }
        if ($arg -match '^-[bBcDEeFIiJLlmopQRSWw]') {
            # Bundled-argument options (-p22, -oKey=val, …).
            continue
        }
        if ($arg.StartsWith('-')) {
            # `--` and every flag option (-t -v -4 …) — skipped.
            continue
        }
        # First non-option token: the destination. Anything after it
        # is the remote command → not interactive.
        if ($i + 1 -lt $Argv.Count) { return $null }
        return $arg
    }
    return $null
}

# ssh_begin: session id, the typed destination, this hop's id and
# whether a bootstrap is sent — never a control-path field: Windows
# OpenSSH cannot multiplex, so there is no socket to report.
function global:__ait_ssh_begin {
    param([string]$Target, [string]$Hop)
    $bootstrapped = 'false'
    if (-not [string]::IsNullOrEmpty($env:AIT_SSH_BOOTSTRAP)) {
        $bootstrapped = 'true'
    }
    __ait_emit ('{"hook":"ssh_begin","value":{"session_id":' + (__ait_sid) +
        ',"target":"' + (__ait_json_escape $Target) + '","hop":"' +
        (__ait_json_escape $Hop) + '","bootstrapped":' + $bootstrapped + '}}')
}

# ssh_end: the local `ssh` returned — the remote session is over
# (cleanly or by a dropped connection).
function global:__ait_ssh_end {
    param([string]$Target, [string]$Hop)
    __ait_emit ('{"hook":"ssh_end","value":{"session_id":' + (__ait_sid) +
        ',"target":"' + (__ait_json_escape $Target) + '","hop":"' +
        (__ait_json_escape $Hop) + '"}}')
}

# The remote command: export this hop's id and re-export
# AIT_SSH_BOOTSTRAP into the remote session — POSIX single-quoted,
# the only self-reference the protocol needs — then run it. A nested
# ssh() on the far side sends this same text one hop further.
function global:__ait_ssh_remote_cmd {
    param([string]$Hop)
    $quoted = $env:AIT_SSH_BOOTSTRAP.Replace("'", "'\''")
    $qhop = $Hop.Replace("'", "'\''")
    return "export AIT_SSH_HOP='$qhop'`nexport AIT_SSH_BOOTSTRAP='$quoted'`n$env:AIT_SSH_BOOTSTRAP"
}

# The §5.9 wrapper, ported to PowerShell (§5.14): the same opt-outs,
# argv classifier and policy as the Unix `ssh()` — minus the
# multiplex leg Windows OpenSSH cannot serve.
function global:ssh {
    # Opt-outs first: `AIT_SSH_OFF=<non-empty>` per session — POSIX
    # `-n` semantics, so "0" still opts out — and the armed-off
    # policy. The wrapper must never change what the user asked for.
    if (-not [string]::IsNullOrEmpty($env:AIT_SSH_OFF) -or
        (-not [string]::IsNullOrEmpty($env:AIT_SSH_ENABLED) -and
         $env:AIT_SSH_ENABLED -ne '1')) {
        & (__ait_ssh_program) @args
        return
    }
    $dest = __ait_ssh_dest ([string[]]$args)
    if ($null -eq $dest -or -not (__ait_ssh_allowed $dest)) {
        & (__ait_ssh_program) @args
        return
    }
    # This hop's identity — attributes this session's ssh_begin /
    # ssh_end and (via the remote env) the remote shell's own hooks
    # to this level of a possibly-nested chain.
    $global:__AIT_SSH_SEQ += 1
    $__ait_hop_sid = $env:AIT_SESSION_ID
    if ([string]::IsNullOrEmpty($__ait_hop_sid)) { $__ait_hop_sid = '0' }
    $hop = "$__ait_hop_sid.$PID.$global:__AIT_SSH_SEQ"
    __ait_ssh_begin $dest $hop
    if (-not [string]::IsNullOrEmpty($env:AIT_SSH_BOOTSTRAP)) {
        & (__ait_ssh_program) -t @args (__ait_ssh_remote_cmd $hop)
    } else {
        & (__ait_ssh_program) -t @args
    }
    # $LASTEXITCODE keeps the ssh exit — the emissions below are
    # function calls, which never touch it, so the caller still
    # reads the real status.
    __ait_ssh_end $dest $hop
    # The remote session demoted this tab when its exit_shell landed
    # (or a drop killed it); the local shell is back at the prompt —
    # re-integrate it.
    __ait_hook 'bootstrapped'
}

# --- prompt/command hooks -----------------------------------------------

# OSC 7 cwd report — `]7;<path>` carries a bare local path:
# `file://h/C:/x` would lose the
# drive letter (`cwd_from_uri` takes `/C:/x`'s authority slice as the
# path), so the payload is the percent-escaped path itself. Skipped on
# non-filesystem drives (HKCU:, Env:) where there is no shell cwd.
function global:__ait_osc7 {
    if ($PWD.Provider.Name -ne 'FileSystem') { return }
    __ait_osc ('7;' + $PWD.ProviderPath.Replace('%', '%25'))
}

# precmd lives inside `prompt`: close the previous block (133;D;<exit>),
# report cwd (OSC 7), open the next prompt (133;A) and fire the hook —
# the same order as the Unix precmds, minus `133;B`/`133;C`, which the
# PSReadLine Enter wrapper owns (the prompt can't see the typed line;
# only the key handler can). `$?`/`$LASTEXITCODE` are captured before
# anything else runs — every statement mutates them. The exit code is
# the native `$LASTEXITCODE` when one's pending, else 0/1 from `$?`.
$global:__AIT_ORIG_PROMPT =
    Get-Item function:prompt -ErrorAction SilentlyContinue

function global:prompt {
    $__ait_ok = $?
    $__ait_last = $global:LASTEXITCODE
    $__ait_code = 0
    if ($null -ne $__ait_last -and $__ait_last -ne 0) {
        $__ait_code = $__ait_last
    } elseif (-not $__ait_ok) {
        $__ait_code = 1
    }
    __ait_osc ("133;D;" + $__ait_code)
    __ait_osc7
    __ait_osc '133;A'
    __ait_hook 'precmd'
    # The user's original prompt renders last so the marks land before
    # its text; an absent or null-returning one falls back to PS-style.
    if ($global:__AIT_ORIG_PROMPT) {
        $__ait_text = & $global:__AIT_ORIG_PROMPT
        if ($null -ne $__ait_text) {
            return "$__ait_text"
        }
    }
    "PS $($executionContext.SessionState.Path.CurrentLocation)> "
}

# --- install ------------------------------------------------------------

# 133;B/133;C + preexec live in a PSReadLine Enter wrapper: read the
# typed line via GetBufferState before AcceptLine runs it. A buffer
# with parse errors is a continuation (`>>`), not a command — it adds
# no marks and AcceptLine does its own continuation handling. With no
# PSReadLine the session keeps the prompt's marks+hooks (§5.14
# MarksOnly territory) — the wrapper's absence never breaks the prompt.
try {
    if ((Get-Command Set-PSReadLineKeyHandler -ErrorAction SilentlyContinue) -and
        ('Microsoft.PowerShell.PSConsoleReadLine' -as [type])) {
        Set-PSReadLineKeyHandler -Key Enter -ScriptBlock {
            try {
                $ast = $null; $tokens = $null; $errors = $null; $cursor = $null
                [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState(
                    [ref]$ast, [ref]$tokens, [ref]$errors, [ref]$cursor)
                $line = ''
                if ($null -ne $ast -and $null -ne $ast.Extent) {
                    $line = $ast.Extent.Text
                }
                if ((-not $errors) -and $line.Trim().Length -gt 0) {
                    __ait_osc '133;B'
                    __ait_osc '133;C'
                    __ait_hook_preexec $line
                }
            } catch { }
            [Microsoft.PowerShell.PSConsoleReadLine]::AcceptLine()
        }
    }
} catch { }

# exit_shell on a clean quit (`exit`, Ctrl+D) — a killed ConPTY child
# fires nothing, matching the Unix EXIT trap's limits (§5.14).
try {
    $null = Register-EngineEvent -SourceIdentifier 'PowerShell.Exiting' -Action {
        try { __ait_hook 'exit_shell' } catch { }
    }
} catch { }

# The wrapped-ssh hop counter — per session, like the Unix snippets'
# seq (§5.9).
$global:__AIT_SSH_SEQ = 0

__ait_emit ('{"hook":"init_shell","value":{"session_id":' + (__ait_sid) +
    ',"shell":"pwsh"' + (__ait_hop) + '}}')
$global:__AIT_INTEGRATED_PWSH = $true
