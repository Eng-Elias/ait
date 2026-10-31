#!/usr/bin/env python3
"""ADR-0002 lint: every dependency in crates/*/Cargo.toml must have its
resolved release date documented — the crate's name followed by a
`(YYYY-MM-DD)` — in the manifest's comments. House style allows either a
per-entry comment or one shared block covering several entries.

Checks [dependencies], [dev-dependencies] and [build-dependencies]
(plus target-scoped variants) in every crates/*/Cargo.toml. Path and
workspace deps are exempt — they resolve in-tree.

Usage: scripts/check-dep-comments.py   (exit 1 + listing on violations)
"""

import re
import sys
from pathlib import Path

DEP_SECTION = re.compile(r"^\[(?:target\.[^\]]+\.)?(?:dev-|build-)?dependencies\]")
ANY_SECTION = re.compile(r"^\[")
ENTRY = re.compile(r"^([A-Za-z0-9_-]+)\s*=")
# A dep is documented when its name appears in a comment with a release
# date within ~100 chars — covers `name 1.2.3 (2026-10-09)`, names split
# from their version+date across wrapped comment lines, and shared
# blocks listing several deps. `-`/`_` spellings both match.
def documented(name: str, comments: str) -> bool:
    # `name` … `semver` … `(YYYY-MM-DD)` — gaps may cross wrapped
    # comment lines (comments joined on \n, matched with DOTALL).
    pattern = (
        rf"\b{re.escape(name).replace(chr(92) + '-', '[-_]')}\b"
        rf".{{0,120}}?\d+\.\d+[\d.]*.{{0,60}}?\(20\d\d-\d\d-\d\d"
    )
    return re.search(pattern, comments, re.DOTALL) is not None


def dep_names(path: Path) -> list[tuple[str, int]]:
    """Dependency names + line numbers in `path`'s dep sections."""
    names, in_deps = [], False
    for number, line in enumerate(path.read_text().splitlines(), start=1):
        stripped = line.strip()
        if DEP_SECTION.match(stripped):
            in_deps = True
            continue
        if ANY_SECTION.match(stripped):
            in_deps = False
            continue
        if not in_deps or stripped.startswith("#"):
            continue
        match = ENTRY.match(stripped)
        if match and "=" in line and "path =" not in line and "workspace" not in line:
            names.append((match.group(1), number))
    return names


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    failures = []
    for manifest in sorted(root.glob("crates/*/Cargo.toml")):
        comments = "\n".join(
            line for line in manifest.read_text().splitlines() if line.strip().startswith("#")
        )
        for name, number in dep_names(manifest):
            if not documented(name, comments):
                failures.append(f"{manifest.relative_to(root)}:{number}: {name}")
    if failures:
        print("Dependencies without a documented release date (ADR-0002):", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    print("All dependencies have documented release dates.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
