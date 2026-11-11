# Known flaky tests

PTY/timing-sensitive tests that intermittently fail under `cargo test
--workspace` parallel load and pass in isolation. A gate failure in one
of these is probably a flake, not your change — but verify before
dismissing it.

## Recognition rule

Fails under load, passes alone. Before assuming real breakage, rerun
just the named test:

```bash
cargo test -p ait <exact-test-name> -- --exact
```

or the whole package serially (`--test-threads=1`). Passes → flake;
record it below if it isn't listed. Fails again → real breakage —
investigate, don't retry-loop the gate hoping for green.

## The list

| Test | Status |
|---|---|
| `app::tests::esc_after_the_rewrite_only_cancels_the_suggestion` | fixed in R12 (wait_until wrap); was ~3/8 failures under load |
| `tabs::tests::sealed_blocks_surface_once_as_persisted_rows` | open — flake seen R12, untouched since R11 |
| `shell_integration::bash_blocks_copy_and_inband_roundtrip` | open — flake seen R12, untouched since R05 |
| `ait-agent pty::tests::the_agent_shell_arms_the_ssh_policy` | open — flake seen R13-T2 (fails while other test binaries still compile; passes isolated and under a quiet parallel load), file untouched since R09 |

The underlying PTY races are tracked in the `Backlog: PTY-timing
flakes under parallel test load` issue — the entries above leave the
list when that issue (or a fix) lands.

## Adding an entry

A test earns a row when it fails under load and passes isolated twice —
two sightings, not one bad run. Note the round it was seen in and the
last commit that touched its file; a flake in freshly edited code is a
bug in the round, not a flake.
