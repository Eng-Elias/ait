#!/usr/bin/env python3
"""Release-version agreement (Spec R12, us-24/27): the version a release
ships must be one number in every place it appears.

Sources compared:
  * tag       — RELEASE_TAG env var if set, else
                `git describe --tags --exact-match` (absent is fine:
                dev checkouts carry no tag, so no tag context is
                expected under `scripts/check.sh` on dev)
  * workspace — [workspace.package].version in the root Cargo.toml
  * changelog — the top `## [x.y.z]` heading in CHANGELOG.md
                (`## [Unreleased]` does not match and is skipped)
  * scenario  — the `expect_text = "ait x.y.z"` literal in
                crates/ait/tests/scenarios/version.toml (the e2e
                fixture's second copy of the version)
  * binary    — `ait --version` of target/debug/ait, rebuilt via
                `cargo build -p ait` when missing or older than sources

Without a tag context the repo-side four-way (workspace ↔ changelog ↔
scenario ↔ binary) is still asserted; a tag adds the fifth. Any
disagreement exits 1 and names every source and its value.

Usage: scripts/check-release-version.py   (exit 1 + listing on mismatch)
"""

import os
import re
import subprocess
import sys
from pathlib import Path

# `## [x.y.z]` — semver core, optional -pre.release/+build suffix. The
# first matching heading is the top release; `## [Unreleased]` is not
# a version and never matches.
HEADING = re.compile(r"^## \[(\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?)\]")
VERSION_ENTRY = re.compile(r'^version\s*=\s*"([^"]+)"')
# Semver-shaped tag (`v` prefix optional). `git describe` answers ANY
# exact-match tag — a stray local `wip` tag on HEAD is not a release
# context and must not break `check.sh`; only semver tags count.
TAG = re.compile(r"^v?(\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?)$")


def workspace_version(root: Path) -> str | None:
    """[workspace.package].version from the root manifest."""
    in_section = False
    for line in (root / "Cargo.toml").read_text().splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_section = stripped == "[workspace.package]"
            continue
        if in_section:
            match = VERSION_ENTRY.match(stripped)
            if match:
                return match.group(1)
    return None


def changelog_version(root: Path) -> str | None:
    """Version of the first `## [x.y.z]` heading in CHANGELOG.md."""
    changelog = root / "CHANGELOG.md"
    if not changelog.exists():
        return None
    for line in changelog.read_text().splitlines():
        match = HEADING.match(line.strip())
        if match:
            return match.group(1)
    return None


def tag_version(root: Path) -> str | None:
    """The release tag's version, or None when there is no tag context."""
    tag = os.environ.get("RELEASE_TAG", "").strip()
    if not tag:
        try:
            result = subprocess.run(
                ["git", "describe", "--tags", "--exact-match"],
                cwd=root,
                capture_output=True,
                text=True,
                timeout=15,
            )
        except (OSError, subprocess.TimeoutExpired):
            return None
        if result.returncode != 0:
            return None
        tag = result.stdout.strip()
        # A stray non-semver tag on HEAD is not a release context.
        if not TAG.match(tag):
            return None
    elif not TAG.match(tag):
        # An explicit RELEASE_TAG is caller intent — a non-semver
        # value is a caller error, reported loudly, never ignored.
        sys.exit(f"check-release-version: RELEASE_TAG {tag!r} is not semver-shaped")
    value = tag[1:] if tag.startswith("v") else tag
    return value or None


def scenario_version(root: Path) -> str | None:
    """The `expect_text = "ait x.y.z"` literal in the version e2e
    scenario — the one place the workspace version is re-literal'd
    (the scenario file has no interpolation, so it must hand-match)."""
    scenario = root / "crates" / "ait" / "tests" / "scenarios" / "version.toml"
    if not scenario.exists():
        return None
    for line in scenario.read_text().splitlines():
        match = re.match(r'^expect_text\s*=\s*"ait\s+(\S+)"', line.strip())
        if match:
            return match.group(1)
    return None


def binary_stale(binary: Path, root: Path) -> bool:
    """True when the binary is absent or older than any version-affecting
    source: manifests, lockfile, or crates/*/src."""
    if not binary.exists():
        return True
    built = binary.stat().st_mtime
    sources = [root / "Cargo.toml", root / "Cargo.lock"]
    sources += root.glob("crates/*/Cargo.toml")
    sources += root.glob("crates/*/src/**/*.rs")
    return any(
        source.exists() and source.stat().st_mtime > built for source in sources
    )


def binary_version(root: Path) -> str:
    """`ait --version`'s version, rebuilding target/debug/ait first when
    it is missing or stale. Honors CARGO_TARGET_DIR — the ext4 sidestep
    for ntfs3 link EINVAL — the same way cargo itself resolves it
    (relative paths anchor at the build cwd, i.e. `root`)."""
    target_dir = Path(os.environ.get("CARGO_TARGET_DIR", root / "target"))
    if not target_dir.is_absolute():
        target_dir = root / target_dir
    binary = target_dir / "debug" / "ait"
    if binary_stale(binary, root):
        subprocess.run(
            ["cargo", "build", "-p", "ait"], cwd=root, check=True
        )
    output = subprocess.run(
        [str(binary), "--version"],
        cwd=root,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    match = re.match(r"^ait\s+(\S+)$", output)
    if not match:
        raise RuntimeError(f"unexpected `ait --version` output: {output!r}")
    return match.group(1)


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    sources = {}
    tag = tag_version(root)
    if tag is not None:
        # Optional fourth source — absent on untagged dev checkouts.
        sources["RELEASE_TAG/git describe"] = tag
    sources["Cargo.toml [workspace.package]"] = workspace_version(root)
    sources["CHANGELOG.md top entry"] = changelog_version(root)
    sources["version.toml expect_text"] = scenario_version(root)
    try:
        sources["`ait --version`"] = binary_version(root)
    except (OSError, subprocess.CalledProcessError, RuntimeError) as error:
        print(
            f"check-release-version: could not obtain `ait --version`: {error}",
            file=sys.stderr,
        )
        return 1

    missing = [name for name, value in sources.items() if value is None]
    if missing:
        print("check-release-version: missing version sources:", file=sys.stderr)
        for name in missing:
            print(f"  {name}", file=sys.stderr)
        return 1

    if len(set(sources.values())) != 1:
        print(
            "check-release-version: disagreeing version sources:", file=sys.stderr
        )
        for name, value in sources.items():
            print(f"  {name}: {value}", file=sys.stderr)
        return 1

    version = next(iter(sources.values()))
    note = "" if tag is not None else " (no tag context — repo-side four-way)"
    print(f"All version sources agree: {version}{note}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
