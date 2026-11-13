#!/usr/bin/env python3
"""ADR-0002 lint: every dependency in crates/*/Cargo.toml must have its
resolved release date documented — the crate's name followed by a
`(YYYY-MM-DD)` — in the manifest's comments. House style allows either a
per-entry comment or one shared block covering several entries.

Checks [dependencies], [dev-dependencies] and [build-dependencies]
(plus target-scoped variants) in every crates/*/Cargo.toml. Path and
workspace deps are exempt — they resolve in-tree.

Second check: ADR-0002's seven-day release-age rule. A dependency's
documented release date must be ≥7 days before the date the dep's
manifest line was last touched (git blame — the adoption proxy;
uncommitted lines count as today). A dep adopted too young stays
flagged while its line keeps that blame date — pin an older release
or mark it: `age-exempt: <reason>` in the dep's comment suppresses
the flag for that dep only.

Usage: scripts/check-dep-comments.py   (exit 1 + listing on violations)
"""

import re
import subprocess
import sys
from datetime import date, datetime
from pathlib import Path

MIN_AGE_DAYS = 7

DEP_SECTION = re.compile(r"^\[(?:target\.[^\]]+\.)?(?:dev-|build-)?dependencies\]")
ANY_SECTION = re.compile(r"^\[")
ENTRY = re.compile(r"^([A-Za-z0-9_-]+)\s*=")
# A dep is documented when its name appears in a comment with a release
# date within ~100 chars — covers `name 1.2.3 (2026-10-09)`, names split
# from their version+date across wrapped comment lines, and shared
# blocks listing several deps. `-`/`_` spellings both match.
def documented_date(name: str, comments: str) -> str | None:
    # `name` … `semver` … `(YYYY-MM-DD)` — gaps may cross wrapped
    # comment lines (comments joined on \n, matched with DOTALL).
    pattern = (
        rf"\b{re.escape(name).replace(chr(92) + '-', '[-_]')}\b"
        rf".{{0,120}}?\d+\.\d+[\d.]*.{{0,60}}?\((20\d\d-\d\d-\d\d)"
    )
    match = re.search(pattern, comments, re.DOTALL)
    return match.group(1) if match else None


def documented(name: str, comments: str) -> bool:
    return documented_date(name, comments) is not None


# `age-exempt: <reason>` in the dep's comment neighbourhood suppresses
# the release-age flag — the recorded-deviation escape hatch ADR-0002's
# procedure prescribes.
def age_exempt(name: str, comments: str) -> bool:
    dep = rf"\b{re.escape(name).replace(chr(92) + '-', '[-_]')}\b"
    pattern = rf"{dep}.{{0,200}}?age-exempt|age-exempt.{{0,200}}?{dep}"
    return re.search(pattern, comments, re.DOTALL) is not None


def blame_dates(root: Path, manifest: Path) -> dict[int, date]:
    """line number -> author date for each line; uncommitted -> today."""
    today = date.today()
    dates: dict[int, date] = {}
    try:
        proc = subprocess.run(
            ["git", "blame", "--line-porcelain", "--", str(manifest)],
            capture_output=True,
            text=True,
            cwd=root,
            check=True,
        )
    except (subprocess.CalledProcessError, FileNotFoundError):
        return dates
    lineno = sha = 0
    for line in proc.stdout.splitlines():
        if re.match(r"^[0-9a-f]{40} ", line):
            lineno += 1
            sha = int(line.split()[0], 16)
        elif line.startswith("author-time "):
            epoch = int(line.split()[1])
            dates[lineno] = today if sha == 0 else datetime.fromtimestamp(epoch).date()
    return dates


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
    too_fresh = []
    for manifest in sorted(root.glob("crates/*/Cargo.toml")):
        comments = "\n".join(
            line for line in manifest.read_text().splitlines() if line.strip().startswith("#")
        )
        rel = manifest.relative_to(root)
        adopted = blame_dates(root, rel)
        for name, number in dep_names(manifest):
            released_on = documented_date(name, comments)
            if released_on is None:
                failures.append(f"{rel}:{number}: {name}")
                continue
            adopted_on = adopted.get(number, date.today())
            released = date.fromisoformat(released_on)
            if (adopted_on - released).days < MIN_AGE_DAYS and not age_exempt(name, comments):
                too_fresh.append(
                    f"{rel}:{number}: {name} ({released_on}) adopted {adopted_on} — under {MIN_AGE_DAYS} days"
                )
    if failures:
        print("Dependencies without a documented release date (ADR-0002):", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    if too_fresh:
        print(
            "Dependencies adopted before their release was 7 days old (ADR-0002):",
            file=sys.stderr,
        )
        for failure in too_fresh:
            print(f"  {failure}", file=sys.stderr)
        print("  pin an older release, or mark the dep `age-exempt: <reason>`", file=sys.stderr)
        return 1
    print("All dependencies have documented release dates.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
