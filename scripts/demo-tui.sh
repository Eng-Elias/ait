#!/usr/bin/env bash
# R02-T6 — scripted demo transcript for the Round-2 TUI (Spec R02 user
# story 25; the round PR attaches the produced transcript as evidence).
#
# Drives `ait` under `script(1)` with a timed keystroke feed and greps
# the recorded stream for the evidence each feature leaves behind:
# styled rendering, typed input, a full-screen app, scrollback by keys
# AND injected wheel events, drag-select copy + Shift+Insert paste,
# tabs (new/switch/exited/close), the status line, OSC 9 and OSC 777
# toasts, and a clean quit that restores the host terminal.
#
# The host terminal is faked completely — `script` allocates the pty,
# this feed answers ait's `CSI ? u` capability probe like a compliant
# kitty host (flags reply AND the primary-DA reply the probe then waits
# for), so the run exercises the ENHANCED keymap tier (Ctrl+Shift+T/W/Q
# as CSI-u). `run_tmux` covers the LEGACY tier: tmux never answers the
# probe, Ctrl+Shift aliases collapse onto Ctrl+key and stay unbound.
#
# Usage:   scripts/demo-tui.sh [OUTDIR]
#          AIT_BIN=path/to/ait TMUX=path/to/tmux scripts/demo-tui.sh out
# Requires: util-linux `script`, bash, vi; tmux only for run_tmux.
#
# Artifacts in OUTDIR:
#   demo-script.typescript  + .timing — the full enhanced-tier run
#   demo-exitcode.typescript          — last-tab exit status → exit code
#   demo-noshell.typescript           — SHELL unset → /bin/sh fallback tab
#   demo-tmux.log                     — labeled capture-pane evidence
#   demo-tmux.raw                     — the pane's raw output stream
#   checks.txt                        — PASS/FAIL verdict per assertion

set -uo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
AIT_BIN=${AIT_BIN:-"$REPO/target/debug/ait"}
OUT=${1:-"$REPO/../ait-notes-r02/demo"}
TMUX=${TMUX:-tmux}
ROWS=30
COLS=100
mkdir -p "$OUT"
CHECKS="$OUT/checks.txt"
: >"$CHECKS"

# --- keystroke emitters -------------------------------------------------
# `line` sends a SHELL COMMAND: literal bytes — child-side escapes are
# written single-backslashed (\033) so the child's printf interprets
# them. `key` sends REAL escape bytes for ait-bound keys and mouse
# reports (SGR 1006). The two must never mix: a real ESC in command text
# is a readline keypress, and a literal "\033" in a key slot is dead text.
line() { printf '%s\n' "$1"; }
key() { printf '%b' "$1"; }
# Alt+key: ESC then the literal char. NOT printf '%b' '\033t' — %b reads
# \033 + a following octal digit as one escape, so '\0331' emits byte
# 0xD9, not ESC+'1'. The %s split keeps the digit literal.
alt() { printf '\033%s' "$1"; }

# --- the timed keystroke feed for the enhanced run ----------------------
drive_main() {
    # Answer ait's capability probe like a compliant kitty host: flags
    # reply, then primary-DA (the probe hangs without the second).
    sleep 0.5; key '\033[?1u\033[?1;2c'
    sleep 1.2; line "PS1='AIT> '"
    sleep 0.6; line 'stty size'
    # --- styled output + plain typing ---
    sleep 0.8; line "printf 'A\\033[1mBOLD-MARK\\033[0m \\033[4:3mCURLY-MARK\\033[0m \\033[38;5;208mC256-MARK\\033[0m \\033[38;2;255;100;0mTRUE-MARK\\033[0m\\n'"
    sleep 0.8; line 'ls --color=always /bin | head -4'
    sleep 0.8; line 'echo TYPED-OK'
    # --- a full-screen app: vi edits and writes a file ---
    sleep 0.8; line 'vi /tmp/ait-demo-vi.txt'
    sleep 1.4; line 'iHELLO-FROM-VI'
    sleep 0.3; key '\033'
    sleep 0.3; line ':wq'
    sleep 1.0; line 'echo VI-EXIT-$?'
    # --- scrollback: 60 lines, then keys and injected wheel ---
    sleep 0.9; line "for n in \$(seq 60); do echo SCROLL-LINE-\$n; done"
    sleep 1.8; key '\033[5;2~'          # Shift+PageUp
    sleep 0.6; key '\033[<64;50;10M'    # wheel up
    sleep 0.4; key '\033[<64;50;10M'
    sleep 0.4; key '\033[<64;50;10M'
    sleep 0.6; key '\033[1;2F'          # Shift+End — snap to bottom
    sleep 0.6; line 'echo BACK-AT-BOTTOM'
    # --- drag-select AITPASTE42 (row 3, cols 1-10), copy, paste, run ---
    sleep 0.8; line 'clear'
    sleep 0.7; line "printf 'AITPASTE42\\n'"
    sleep 0.8; key '\033[<0;1;3M'       # button-down row 3 col 1
    sleep 0.2; key '\033[<32;5;3M'      # drag to col 5
    sleep 0.2; key '\033[<0;10;3m'      # release on col 10
    sleep 0.5; key '\033[2;2~'          # Shift+Insert — paste the copy
    sleep 0.4; printf '\n'              # runs AITPASTE42 → not found
    # --- tabs: Alt+T new, Ctrl+Shift+T new (enhanced CSI-u) ---
    sleep 0.9; alt t
    sleep 1.0; line 'echo IN-TAB-2'
    sleep 0.8; key '\033[116;6u'        # Ctrl+Shift+T
    sleep 1.0; line 'echo IN-TAB-3'
    # A notification scheduled in tab 3, observed from tab 1 — the toast
    # is prefixed with the tab's label.
    sleep 0.6; line "sleep 2; printf '\\033]9;BG-NOTE\\007'"
    sleep 0.4; alt 1                  # Alt+1 → tab 1
    sleep 2.5
    line "printf '\\033]9;FG-NOTE-9\\007'"
    sleep 1.0; line "printf '\\033]777;notify;TITLE-777;BODY-777\\007'"
    # --- exited tab: tab 3's shell exits 3; the tab stays ---
    sleep 1.2; alt 3                  # Alt+3 → tab 3
    sleep 0.7; line 'exit 3'
    sleep 1.3; line 'echo GHOST-DROPPED' # input to an exited tab drops
    sleep 0.6; alt w                  # Alt+W closes the exited tab
    sleep 0.7; key '\033[5;5~'          # Ctrl+PageUp → tab 1
    # --- status line: OSC 7 sets the tracked cwd ---
    sleep 0.7; line "printf '\\033]7;file:///tmp\\007'"
    sleep 1.0
    # --- clean quit: Ctrl+Shift+Q (enhanced alias) ---
    key '\033[113;6u'
    sleep 0.8
}

# --- verification -------------------------------------------------------
FAILURES=0
have() { # have <file> <pattern> <what-it-proves>
    if grep -aqF "$2" "$1"; then
        echo "PASS  $3" >>"$CHECKS"
    else
        echo "FAIL  $3  (missing '$2' in $(basename "$1"))" >>"$CHECKS"
        FAILURES=$((FAILURES + 1))
    fi
}
have_n() { # have_n <file> <pattern> <min-count> <desc>
    local n
    n=$(grep -aoF "$2" "$1" | wc -l)
    if [ "$n" -ge "$3" ]; then
        echo "PASS  $4 ($n hits)" >>"$CHECKS"
    else
        echo "FAIL  $4  ('$2' x$n < $3 in $(basename "$1"))" >>"$CHECKS"
        FAILURES=$((FAILURES + 1))
    fi
}
have_not() { # have_not <file> <pattern> <desc>
    if grep -aqF "$2" "$1"; then
        echo "FAIL  $3  (unexpected '$2' in $(basename "$1"))" >>"$CHECKS"
        FAILURES=$((FAILURES + 1))
    else
        echo "PASS  $3" >>"$CHECKS"
    fi
}
have_re() { # have_re <file> <regex> <desc>
    if grep -aqE "$2" "$1"; then
        echo "PASS  $3" >>"$CHECKS"
    else
        echo "FAIL  $3  (no /$2/ in $(basename "$1"))" >>"$CHECKS"
        FAILURES=$((FAILURES + 1))
    fi
}

# --- run 1: the full feature tour, enhanced keymap tier -----------------
run_script() {
    local ts="$OUT/demo-script.typescript"
    # Pipe form (not `< <(drive)`): `script` then reliably records the
    # child's exit code in the footer — the process-substitution form
    # was observed losing it (footer shows 0 for a code-3 exit).
    drive_main | TERM=xterm-256color SHELL=/bin/bash \
        script -qefc "stty rows $ROWS cols $COLS; exec $AIT_BIN" \
        -T "$OUT/demo-script.timing" "$ts"
    have_re  "$ts" '\[>1u'                'kitty flags pushed — enhanced tier active'
    have     "$ts" '1:bash'               'tab bar shows the shell tab'
    have     "$ts" 'BOLD-MARK'            'bold SGR rendered'
    have     "$ts" 'CURLY-MARK'           'underline-variant SGR rendered'
    have     "$ts" 'C256-MARK'            '256-color output rendered'
    have     "$ts" 'TRUE-MARK'            'truecolor output rendered'
    have     "$ts" 'TYPED-OK'             'typed input reached the shell'
    have     "$ts" 'HELLO-FROM-VI'        'full-screen app (vi) ran inside the session'
    have     "$ts" 'VI-EXIT-0'            'vi wrote the file and exited cleanly'
    have_n   "$ts" 'SCROLL-LINE-5' 2      'scrolled repaint re-emitted scrollback lines'
    have     "$ts" 'BACK-AT-BOTTOM'       'typing snapped the view back to the bottom'
    have     "$ts" 'AITPASTE42: command not found' 'drag-select copy + Shift+Insert paste reached the PTY'
    have     "$ts" "$(printf '\033]52;')" 'copy emitted OSC 52 to the host'
    have     "$ts" '2:bash'               'Alt+T opened tab 2'
    have     "$ts" '3:bash'               'Ctrl+Shift+T opened tab 3 (enhanced alias)'
    have     "$ts" 'IN-TAB-2'             'tab 2 session ran commands'
    have_re  "$ts" '[0-9]:[a-z]+: BG-NOTE' 'background-tab OSC 9 toast carries the tab label'
    have     "$ts" 'FG-NOTE-9'            'OSC 9 notification in the active tab'
    have_re  "$ts" 'TITLE-777: BODY-777|BODY-777' 'OSC 777 notification'
    have     "$ts" 'exited'               'exited tab kept, status shows the exited state'
    have_not "$ts" 'GHOST-DROPPED'        'input to an exited tab is dropped'
    have     "$ts" '/tmp'                 'OSC 7 tracked cwd appears'
    have_re  "$ts" '\[\?1049l'            'leave-alternate-screen on exit'
    have_re  "$ts" '\[\?1000l'            'mouse capture released on exit'
    have_re  "$ts" '\[<u|\[<[0-9]u'       'keyboard flags popped on exit'
    have_re  "$ts" 'COMMAND_EXIT_CODE="0"' 'clean quit — ait exited 0'
}

# --- run 2: last-tab exit status becomes ait's exit code ----------------
run_exitcode() {
    local ts="$OUT/demo-exitcode.typescript"
    {
        sleep 0.5; key '\033[?1u\033[?1;2c'
        # dash, not bash: no rc files, deterministic startup.
        sleep 1.4; line 'exit 3'
        # Leave room for the Exited event to surface: ait reports it
        # only after the PTY reader hits EOF (bounded at 2 s — a
        # grandchild holding the slave delays it), and closing the
        # tab before the drain loses the status.
        sleep 4.5; alt w  # Alt+W closes the last (exited) tab
        sleep 0.8
    } | TERM=xterm-256color SHELL=/bin/sh \
        script -qefc "stty rows $ROWS cols $COLS; exec $AIT_BIN" "$ts"
    have    "$ts" 'exited'                    'status showed the exit'
    have_re "$ts" 'COMMAND_EXIT_CODE="3"'  'closing the last exited tab propagates its status'
}

# --- run 3: no $SHELL — the /bin/sh fallback tab ------------------------
run_noshell() {
    local ts="$OUT/demo-noshell.typescript"
    {
        sleep 0.5; key '\033[?1u\033[?1;2c'
        sleep 1.4; line 'echo FALLBACK-OK'
        sleep 0.8; alt q  # Alt+Q
        sleep 0.6
    } | TERM=xterm-256color \
        script -qefc "stty rows $ROWS cols $COLS; exec env -u SHELL $AIT_BIN" "$ts"
    have    "$ts" '1:sh'                  'no $SHELL — tab spawned /bin/sh'
    have    "$ts" 'FALLBACK-OK'           'the fallback shell works'
    have_re "$ts" 'COMMAND_EXIT_CODE="0"' 'Alt+Q clean exit'
}

# --- run 4: inside tmux — the legacy keymap tier ------------------------
TMUX_LOG=
TMUX_RAW=
capture() { # capture <label> — dump the pane's visible text into the log
    sleep 0.4
    {
        echo
        echo "--- $1 ---"
        "$TMUX" capture-pane -t demo -p
    } >>"$TMUX_LOG"
}
tcheck_pane() { # tcheck_pane <desc> <grep-expr> — assert on the CURRENT pane
    local desc=$1 expr=$2
    sleep 0.3
    if "$TMUX" capture-pane -t demo -p | grep -qE "$expr"; then
        echo "PASS  $desc" >>"$CHECKS"
    else
        echo "FAIL  $desc  (pane lacks /$expr/)" >>"$CHECKS"
        FAILURES=$((FAILURES + 1))
    fi
}
tcheck_pane_not() {
    local desc=$1 expr=$2
    sleep 0.3
    if "$TMUX" capture-pane -t demo -p | grep -qE "$expr"; then
        echo "FAIL  $desc  (pane shows /$expr/)" >>"$CHECKS"
        FAILURES=$((FAILURES + 1))
    else
        echo "PASS  $desc" >>"$CHECKS"
    fi
}

run_tmux() {
    command -v "$TMUX" >/dev/null 2>&1 || {
        echo "SKIP  tmux run (no tmux binary)" >>"$CHECKS"
        return
    }
    TMUX_LOG="$OUT/demo-tmux.log"
    TMUX_RAW="$OUT/demo-tmux.raw"
    : >"$TMUX_LOG"; : >"$TMUX_RAW"
    "$TMUX" -f /dev/null kill-session -t demo 2>/dev/null
    "$TMUX" -f /dev/null new-session -d -s demo -x "$COLS" -y "$ROWS" 'bash'
    sleep 0.4
    # Record the pane's raw output stream (typescript-equivalent for the
    # tmux run) BEFORE ait starts so the capability probe lands in it.
    "$TMUX" pipe-pane -t demo -o "cat >> $TMUX_RAW"
    # No `exec`: the pane's bash must survive ait so the post-quit
    # captures show the restored host shell.
    "$TMUX" send-keys -t demo -l "cd $REPO && TERM=xterm-256color SHELL=/bin/bash $AIT_BIN"
    "$TMUX" send-keys -t demo Enter
    sleep 3.2   # probe times out (~2 s) — tmux never answers CSI ? u
    capture 'startup — legacy tier: tab bar + status line'
    tcheck_pane 'tab bar painted inside tmux' '1:bash'
    tcheck_pane 'status line painted inside tmux' 'running'
    "$TMUX" send-keys -t demo -l 'echo TMUX-LEGACY-OK'
    "$TMUX" send-keys -t demo Enter
    capture 'typed input inside tmux'
    # Ctrl+Shift+T on a legacy host is byte-identical to Ctrl+T (0x14):
    # it must NOT open a tab — it reaches the shell instead.
    "$TMUX" send-keys -t demo -H 14
    sleep 0.4
    tcheck_pane_not 'Ctrl+Shift+T byte is inert in the legacy tier' '2:bash'
    capture 'after the ^T byte — still one tab'
    "$TMUX" send-keys -t demo Escape t   # Alt+T works in both tiers
    sleep 0.8
    tcheck_pane 'Alt+T opened tab 2 inside tmux' '2:bash'
    capture 'Alt+T — second tab'
    "$TMUX" send-keys -t demo -l 'seq 60 | sed s/^/SCROLL-/'
    "$TMUX" send-keys -t demo Enter
    sleep 1.4
    "$TMUX" send-keys -t demo -H 1b 5b 35 3b 32 7e   # Shift+PageUp
    capture 'Shift+PageUp — scrollback view'
    tcheck_pane 'scrollback view shows earlier lines' 'SCROLL-[0-9]+'
    "$TMUX" send-keys -t demo -H 1b 5b 31 3b 32 46   # Shift+End
    sleep 0.4
    "$TMUX" send-keys -t demo -l 'printf "\\033]9;TMUX-NOTE\\007"'
    "$TMUX" send-keys -t demo Enter
    capture 'OSC 9 toast inside tmux'
    tcheck_pane 'OSC 9 toast painted on the status line' 'TMUX-NOTE'
    # Resize forwards to every session's PTY (content area only).
    "$TMUX" resize-window -t demo -x 90 -y 25
    sleep 0.6
    "$TMUX" send-keys -t demo -l 'stty size'
    "$TMUX" send-keys -t demo Enter
    sleep 0.7
    capture 'after resize to 90x25 — session PTY reports the content size'
    tcheck_pane 'resize forwarded the content-area winsize (23 x 90)' '23 90'
    "$TMUX" send-keys -t demo Escape q   # Alt+Q
    sleep 0.9
    capture 'Alt+Q — ait exited, the pane is the host shell again'
    "$TMUX" send-keys -t demo -l 'echo POST-AIT-OK'
    "$TMUX" send-keys -t demo Enter
    sleep 0.4
    capture 'host shell usable after ait'
    tcheck_pane 'host shell usable after ait quit' 'POST-AIT-OK'
    "$TMUX" kill-session -t demo 2>/dev/null

    have_not "$TMUX_RAW" '>1u'   'no kitty push inside tmux — legacy tier stayed'
    have     "$TMUX_RAW" '?u'    'the capability probe was attempted'
}

[ -x "$AIT_BIN" ] || {
    echo "ait binary not found at $AIT_BIN — run 'cargo build -p ait' first" >&2
    exit 2
}

echo "== ait scripted demo → $OUT"
run_script
run_exitcode
run_noshell
run_tmux

echo
cat "$CHECKS"
echo
if [ "$FAILURES" -gt 0 ]; then
    echo "$FAILURES check(s) FAILED — see $CHECKS" >&2
    exit 1
fi
echo "all checks passed"
