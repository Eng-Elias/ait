# 15. The release pipeline: one workspace version, a semver-tag trigger, verify before announce

Date: 2026-11-06

## Status

Accepted

## Context

Round 12 (Spec R12, REQUIREMENTS.md §5.11) ships v1.0.0 and the
pipeline that ships every later release. The choices that are hard to
reverse once relied on:

1. **What a version and a release unit are.** With eight crates in one
   workspace, "the version" could be per-crate or workspace-wide, and a
   release could be cut by tag, by workflow dispatch, or by branch push.
   The wrong default lets `ait --version`, the tag and the CHANGELOG
   drift apart silently.
2. **Who may mint a release.** Tag pushes are the one repository write
   the `protected-branches` ruleset does not govern — it targets
   branches only — so "release by tag" needs its own enforcement or any
   tag on any commit ships binaries.
3. **How much of the workflow we own.** cargo-dist generates
   `.github/workflows/release.yml` and errors when the generated file
   is stale or hand-edited (unless `allow-dirty` opts out), so the
   pipeline had to be shaped through `[workspace.metadata.dist]` config
   and dist's supported custom-job slots, not by editing the file.

## Decision

- **One version, one tag, one release unit.** The version lives once in
  `[workspace.package].version` in the root manifest; every crate
  inherits via `version.workspace = true`, so `ait --version` (which
  prints `CARGO_PKG_VERSION`) is that number. A version tag on a `main`
  commit is the release unit — no dispatch, no branch trigger.
  `scripts/check-release-version.py` is the agreement gate: with a tag
  context (`RELEASE_TAG`, or `git describe --exact-match`) it asserts
  tag ↔ `[workspace.package]` ↔ top `## [x.y.z]` CHANGELOG heading ↔
  `ait --version` all agree; without one it still asserts the repo-side
  three-way, which is how `scripts/check.sh` runs it on dev checkouts.
- **cargo-dist is the pipeline, pinned.** `cargo-dist-version =
  "0.34.0"` in `[workspace.metadata.dist]` is the pin — release.yml's
  "Install dist" step curls that exact release's installer (ADR-0002's
  determinism rule applied to the release toolchain). The config lives
  in `[workspace.metadata.dist]` on the virtual root manifest, not a
  `dist-workspace.toml`. `packages = ["ait"]` restricts dist to the
  workspace's sole `[[bin]]`; the `repository` field on the `ait` crate
  is what lets dist compute artifact URLs at all.
- **The generated workflow is never edited — no `allow-dirty`.**
  `release.yml` stays `dist generate --check`-clean; every deviation
  lands through config. Two accepted consequences: the tag trigger is
  dist's own semver glob `'**[0-9]+.[0-9]+.[0-9]+*'` (broader than the
  spec's `v*` — `v1.0.0`, `1.0.0` and `releases/1.2.3` all fire; the
  guard enforces the rest, so the width is harmless) and there is no
  `concurrency` block (dist emits none for tag-triggered flows).
- **`pr-run-mode = "skip"`.** dist's default would run `dist plan` on
  every pull request — a release dry-run billing private-repo minutes
  on `dev` PRs. Our CI policy is main-only (ci.yml precedent); PRs get
  no release legs.
- **Customs are injected through dist's job-graph slots, not edits.**
  `plan-jobs = ["./guard"]` makes `custom-guard`
  (`.github/workflows/guard.yml`) run at pipeline start parallel to
  `plan` and puts it in `build-local-artifacts`' `needs:` — a bad tag
  dies before any billed build leg. It enforces the two preconditions
  dist cannot: the tagged commit is an ancestor of `origin/main`
  (spec us-26) and the four-way version agreement (us-27). Caveat:
  `plan` runs `dist host --steps=create` concurrently, which may
  already have created a draft release object when the guard fails — a
  failed guard leaves a deletable draft, never an announced release.
  `publish-jobs = ["./verify-install"]` makes `custom-verify-install`
  (`.github/workflows/verify-install.yml`, backed by
  `scripts/verify-install.sh`) need `plan` + `host` and gate
  `announce`. `host`'s `gh release create` has already published the
  release when verify runs, so a failed verify leaves a
  published-but-unannounced release that needs a manual yank — the
  spec's accepted fallback for a gate the generated graph cannot
  interleave earlier. publish-jobs default to escalated
  `id-token`/`packages` permissions, so
  `[workspace.metadata.dist.github-custom-job-permissions.verify-install]`
  pins it to `contents = "read"`.
- **Five targets, chosen cheap.** `x86_64-unknown-linux-gnu`,
  `x86_64-unknown-linux-musl` (fully static — the dep tree is
  rustls-only with no OpenSSL, so musl costs nothing and covers
  Alpine/NixOS/old-glibc), `aarch64-unknown-linux-gnu` on GitHub's
  native `ubuntu-24.04-arm` runner via `github-custom-runners` (free on
  public repos — the label fails outright on private ones, which is why
  the public flip precedes the first tag), `aarch64-apple-darwin` and
  `x86_64-apple-darwin`. `unix-archive = ".tar.gz"` overrides dist
  0.34's `.tar.xz` default so the archives decompress everywhere the
  installer runs with no extra tooling. Windows stays out per §6.
- **The repo goes public before the first tag.** Anonymous installer
  downloads, `cargo install --git` and the free ARM64/macOS runner
  minutes all depend on it — private repos bill macOS at 10× and reject
  the `ubuntu-*-arm` labels entirely. The flip is the last pre-tag step
  of the release checklist in AGENTS.md.
- **crates.io is deferred.** `cargo install --git
  https://github.com/Eng-Elias/ait ait` is the v1 cargo path.
  Publishing needs `version` on every path-dep and publishes all eight
  workspace crates; a `Backlog` issue tracks it — the `ait` name was
  observed unclaimed on the index.

## Consequences

- The `uses: $/.github/workflows/*.yml` lines in `release.yml` are not
  placeholders: `$` is GitHub's self-repo reusable-workflow shorthand
  (runner ≥ 2.336) and is what dist 0.34 emits for custom jobs.
- The wider-than-`v*` tag glob is safe because both halves of the guard
  re-check what the glob admits: ancestry confines releases to `main`,
  and version agreement refuses a tag that doesn't match the workspace
  version — a stray `1.2.3` or `releases/9.9.9` tag dies in
  `custom-guard` before builds start.
- `dist generate` / `dist init` regenerate `release.yml` from the
  config, so the guard/verify injections, permissions pin, runner map
  and `pr-run-mode` survive regeneration; `dist generate --check`
  catches drift without writing.
- A prerelease-suffixed tag marks the release `--prerelease` and skips
  `custom-verify-install` (`announcement_is_prerelease` in its `if:`)
  — prerelease tags ship unverified unless `publish-prereleases` is
  set. Accepted: prereleases are not part of the release checklist.
- The generated `ait-installer.sh` cannot detect musl systems — musl
  tarball exists for manual download, but the curl|sh path picks the
  gnu archive on Alpine. Documented, not fixed, this round.
- A failed verify is the one release failure that is not retry-clean:
  the release object is already public, so the remedy is a manual yank
  (delete the release + tag) and re-tag after the fix — never a silent
  re-announce.

## Amendment — 2026-11-12 (Spec R13, ADR-0016)

`ait-gui` joined the release unit for §5.12: `packages` grew to
`["ait", "ait-gui"]` and dist emits a second generated installer,
`ait-gui-installer.sh`. The GUI ships on the four glibc/macOS targets
only — `x86_64-unknown-linux-musl` stays ait-only (a fully static GUI
is unsupportable; GPU drivers are never static), expressed through
dist's package-local `targets` in `ait-gui`'s `[package.metadata.dist]`.
The pre-tag checklist sanity in AGENTS.md now expects both artifact
sets: `ait` on all five targets, `ait-gui` on its four, musl absent
from the latter. The musl-detection caveat above applies to both
generated installers — the curl|sh path picks the gnu archive for
`ait-gui` on Alpine too, and no musl archive exists to fall back to.
