#!/usr/bin/env python3
"""std.15's mechanical floor: an e2e wait that a typed command's own
echo can satisfy isn't a wait for the command at all.

Scans crates/*/tests/**/*.rs and crates/*/tests/**/*.toml.

A substring probe literal (`contains("…")`, `wait_text`,
`expect_text`) is flagged when it appears mid-string inside a
typed-input literal (`type_text("…")`, `send_bytes(b"…")`, TOML
`text = "…"`) evaluated earlier on the same screen — earlier lines of
the same fn for Rust, earlier `[[steps]]` of the same scenario for
TOML. `expect_regex` is compiled and evaluated as a real regex against
the would-be echo. This is the `contains("R09REMOTE")` bug shape from
the R09 review, mechanized.

Exempt by construction:

- A probe at position 0 of the typed literal reproduces the command
  verbatim — the scenario suite's documented echo fences
  (`expect_regex = "echo R05MARKER"`) and the "wait until my typing
  rendered" input gates. Their *purpose* is the echo; they gate typing
  safety, never execution.
- Whole-line equality probes (`line.trim() == "…"`, `panel_has_line`)
  can't match a command echo carrying its prompt prefix — not
  collected.
- `press`/`keys`/`bytes` send chords or hex, not echoable text.
- Typed text routed through `format!`/variables is invisible — the
  arg must literally begin with a quote.

Deliberate exceptions — an input-render gate that means to key on
echo, a non-screen `contains` (a captured fixture body) whose literal
rides inside a typed command — carry `sentinel-ok` on the probe's
line; the marker doubles as the reason's label.

Deliberately shallow: fn boundaries and execution order are
approximated by line order, loops can defeat the ordering, and
regexes are evaluated with Python semantics. A floor, not a proof.

Usage: scripts/check-e2e-sentinels.py   (exit 1 + listing on violations)
"""

import re
import sys
from pathlib import Path

STRING_RE = re.compile(r'r(#+)"(.*?)"\1|(?:b|r)?"((?:[^"\\]|\\.)*)"')
FN_RE = re.compile(
    r"^\s*(?:pub(?:\s*\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+\w+"
)
TYPED_RS = re.compile(r"\b(?:type_text|send_bytes)\s*\(")
PROBE_RS = re.compile(r"\b(?:contains|expect_text|wait_text)\s*\(")
REGEX_PROBE_RS = re.compile(r"\bexpect_regex\s*\(")
TYPED_TOML = re.compile(r"\btext\s*=")
PROBE_TOML = re.compile(r"\b(?:wait_text|expect_text)\s*=")
REGEX_PROBE_TOML = re.compile(r"\bexpect_regex\s*=")

ESCAPES = {
    "n": "\n",
    "r": "\r",
    "t": "\t",
    "0": "\0",
    "b": "\b",
    "f": "\f",
    "\\": "\\",
    '"': '"',
    "'": "'",
}


def unescape(raw: str) -> str:
    """Decode the common Rust/TOML basic-string escapes."""
    out, i = [], 0
    while i < len(raw):
        ch = raw[i]
        if ch != "\\" or i + 1 >= len(raw):
            out.append(ch)
            i += 1
            continue
        nxt = raw[i + 1]
        if nxt in ESCAPES:
            out.append(ESCAPES[nxt])
            i += 2
        elif nxt == "x" and i + 3 < len(raw):
            out.append(chr(int(raw[i + 2 : i + 4], 16)))
            i += 4
        elif nxt == "u" and raw.startswith("{", i + 2):  # \u{…} (Rust)
            end = raw.find("}", i + 3)
            if end == -1:
                out.append(ch)
                i += 1
            else:
                out.append(chr(int(raw[i + 3 : end], 16)))
                i = end + 1
        elif nxt == "u" and i + 5 < len(raw):  # \uXXXX (TOML)
            out.append(chr(int(raw[i + 2 : i + 6], 16)))
            i += 6
        else:  # unknown escape — keep both chars literally
            out.append(ch)
            i += 1
    return "".join(out)


def code_part(line: str, comment: str) -> str:
    """The line minus its comment tail (`//` for Rust, `#` for TOML) —
    the marker counts only outside a string literal."""
    in_str = escaped = False
    i = 0
    while i < len(line):
        ch = line[i]
        if in_str:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                in_str = False
        elif ch == '"':
            in_str = True
        elif comment == "#" and ch == "#":
            return line[:i]
        elif comment == "/" and line.startswith("//", i):
            return line[:i]
        i += 1
    return line


def literal(match: re.Match) -> str:
    """The matched literal's content — raw strings verbatim, others
    unescaped."""
    if match.group(2) is not None:  # r#"…"# / r##"…"##
        return match.group(2)
    if match.group(0).startswith('r"'):  # r"…"
        return match.group(3)
    return unescape(match.group(3))


def literals(code: str) -> list[str]:
    """Every string literal on the code part of a line."""
    return [literal(m) for m in STRING_RE.finditer(code)]


def arg_literal(code: str, verb_end: int) -> str | None:
    """The call/assignment's first-arg literal — only when the arg
    literally begins with a quote (an optional `&` allowed); a
    `format!`/variable arg yields None."""
    rest = code[verb_end:]
    lead = re.match(r"\s*&?\s*", rest)
    match = STRING_RE.match(rest, lead.end())
    return literal(match) if match else None


def collect(path: Path):
    """(typed, probes) as lists of (line, scope, text) — scope is the
    enclosing fn's start line for .rs, the file itself for .toml."""
    comment = "#" if path.suffix == ".toml" else "/"
    typed_re = TYPED_TOML if path.suffix == ".toml" else TYPED_RS
    probe_re = PROBE_TOML if path.suffix == ".toml" else PROBE_RS
    regex_re = REGEX_PROBE_TOML if path.suffix == ".toml" else REGEX_PROBE_RS

    typed, probes = [], []
    scope = 0
    for number, line in enumerate(path.read_text().splitlines(), start=1):
        code = code_part(line, comment)
        if FN_RE.match(code):
            scope = number
        suppressed = "sentinel-ok" in line
        for verb in typed_re.finditer(code):
            found = arg_literal(code, verb.end())
            if found is not None:
                typed.append((number, scope, found))
        if not suppressed:
            for verb in probe_re.finditer(code):
                found = arg_literal(code, verb.end())
                if found is not None:
                    probes.append((number, scope, "substring", found))
            for verb in regex_re.finditer(code):
                found = arg_literal(code, verb.end())
                if found is not None:
                    probes.append((number, scope, "regex", found))
    return typed, probes


def echo_of(typed: str) -> str:
    """What the terminal renders for typed input — the text minus the
    Enter (a trailing newline isn't a rendered character)."""
    return typed.rstrip("\r\n")


def violations(path: Path) -> list[str]:
    typed, probes = collect(path)
    findings = []
    for line_no, scope, kind, probe in probes:
        if not probe:
            continue
        for typed_no, typed_scope, typed_text in typed:
            if typed_scope != scope or typed_no > line_no:
                continue
            echo = echo_of(typed_text)
            if kind == "regex":
                try:
                    match = re.search(probe, echo)
                except re.error:
                    continue
                hit = match is not None and match.start() > 0
            else:
                hit = echo.find(probe) > 0
            if hit:
                findings.append(
                    f"{path}:{line_no}: probe {probe!r} also appears inside the "
                    f"typed input at :{typed_no} ({typed_text!r}) — the wait can "
                    "pass on echo (std.15); probe output-only text or mark the "
                    "line sentinel-ok if the echo gate is deliberate"
                )
                break
    return findings


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    files = sorted(
        p
        for p in root.glob("crates/*/tests/**/*")
        if p.suffix in (".rs", ".toml") and p.is_file()
    )
    findings = [finding for path in files for finding in violations(path)]
    if findings:
        print("E2E probes a typed echo can satisfy (std.15):", file=sys.stderr)
        for finding in findings:
            print(f"  {finding}", file=sys.stderr)
        return 1
    print(f"No echo-satisfiable probes in {len(files)} test files.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
