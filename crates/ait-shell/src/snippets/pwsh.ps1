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
# runs as a script block: session functions, aliases and cwd all apply
# (the same "user's own shell" guarantee the Unix `eval` gives), with
# every stream merged `*>&1` like bash's `2>&1` capture. Replies on OSC
# 7701: A, then R frames of <=500 decoded bytes each (a 1000-char hex
# slice keeps every raw OSC under the 1024-byte vte budget), then B.
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

__ait_emit ('{"hook":"init_shell","value":{"session_id":' + (__ait_sid) +
    ',"shell":"pwsh"' + (__ait_hop) + '}}')
$global:__AIT_INTEGRATED_PWSH = $true
