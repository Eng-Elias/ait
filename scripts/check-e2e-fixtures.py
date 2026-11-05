#!/usr/bin/env python3
"""The e2e job's `-p` test selection builds no dependency's examples:
a test reaching into `target/<profile>/examples/<name>` needs that
example built by a step in the same job, or it passes only where a
fuller build already ran (the R10 `mcp_fixture` CI break, mechanized).

Reads .github/workflows/ci.yml's `e2e:` job, takes the `-p` packages
off its `cargo test` line, scans `crates/<pkg>/tests/**/*.rs` for
example-binary resolution — `.join("examples")` followed by a
`.join(<literal or format!("name…")>)` — and asserts each referenced
`<name>` has a `cargo build … --example <name>` (or `--examples`)
step ahead of the test step.

Exempt by construction:

- `cargo test` lines without `-p` (`--workspace`, bare) build every
  example themselves — no selection to check.
- Example names resolved through variables or string surgery the
  literal scan can't see — deliberately shallow: a floor, not a
  proof, matching check-e2e-sentinels.py.

Usage: scripts/check-e2e-fixtures.py   (exit 1 + listing on violations)
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"

JOB_RE = re.compile(r"^  (\w[\w-]*):\s*$", re.M)
TEST_RE = re.compile(r"run:\s*cargo test\b(.*)")
PKG_RE = re.compile(r"-p\s+([\w-]+)")
BUILD_RE = re.compile(
    r"run:\s*cargo build\b.*--examples?\b(?:\s+([\w-]+))?"
)
EXAMPLES_JOIN_RE = re.compile(r'join\(\s*"examples"\s*\)')
EXAMPLE_NAME_RE = re.compile(
    r'\.join\(\s*(?:format!\s*\(\s*)?"([A-Za-z0-9_-]+)'
)


def e2e_block(text: str) -> str:
    """The `e2e:` job's lines — up to the next job key at its indent."""
    match = re.search(r"^  e2e:\s*$", text, re.M)
    if not match:
        sys.exit("ci.yml has no `e2e:` job — nothing to check")
    rest = text[match.end() :]
    nxt = JOB_RE.search(rest)
    return rest[: nxt.start()] if nxt else rest


def test_packages(block: str) -> list[str]:
    """The `-p` selections on the job's `cargo test` run lines."""
    pkgs: list[str] = []
    for line in block.splitlines():
        test = TEST_RE.search(line)
        if test:
            pkgs += PKG_RE.findall(test.group(1))
    return pkgs


def example_references(pkg: str) -> dict[str, str]:
    """Example names a package's tests resolve, name → file:line."""
    found: dict[str, str] = {}
    tests = ROOT / "crates" / pkg / "tests"
    if not tests.is_dir():
        return found
    for rs in sorted(tests.rglob("*.rs")):
        lines = rs.read_text().splitlines()
        for i, line in enumerate(lines):
            start = EXAMPLES_JOIN_RE.search(line)
            if not start:
                continue
            window = line[start.end() :] + "\n" + "\n".join(lines[i + 1 : i + 4])
            name = EXAMPLE_NAME_RE.search(window)
            rel = rs.relative_to(ROOT)
            found[name.group(1) if name else "<unresolved>"] = (
                f"{rel}:{i + 1}"
            )
    return found


def main() -> int:
    block = e2e_block(WORKFLOW.read_text())
    pkgs = test_packages(block)
    if not pkgs:
        print("e2e job has no package-scoped `cargo test` — nothing to check")
        return 0

    lines = block.splitlines()
    builds = [
        (m.group(1), i)
        for i, line in enumerate(lines)
        for m in [BUILD_RE.search(line)]
        if m
    ]
    test_at = min(i for i, line in enumerate(lines) if TEST_RE.search(line))

    violations = []
    for pkg in pkgs:
        for name, site in sorted(example_references(pkg).items()):
            ok = any(
                pos < test_at and built in (name, None)
                for built, pos in builds
            )
            if not ok:
                violations.append(
                    f"{site}: tests resolve target/**/examples/{name} but the "
                    f"e2e job builds no such example before `cargo test` — add "
                    f"`cargo build --example {name}` (package owning it) ahead "
                    f"of the test step"
                )
    if violations:
        print("e2e fixture gaps:\n  " + "\n  ".join(violations))
        return 1
    print(
        f"Every example referenced by the e2e packages' tests "
        f"({', '.join(pkgs)}) is built in the e2e job."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
