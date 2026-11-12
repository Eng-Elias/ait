# Known flaky tests

PTY/timing-sensitive tests that intermittently fail under `cargo test
--workspace` parallel load and pass in isolation. `scripts/check.sh`
skips them in the parallel sweep and re-runs each one serially — the
sweep red means real breakage; a red serial leg means the flake became
deterministic or is a real failure.

The machine-readable list check.sh consumes is
`scripts/flaky-tests.txt`; the table below carries the provenance and
status behind each entry.

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
| `ait-app tabs::tests::sealed_blocks_surface_once_as_persisted_rows` | open — flake seen R12 (file moved `ait` → `ait-app` in R13) |
| `shell_integration::bash_blocks_copy_and_inband_roundtrip` | open — flake seen R12, untouched since R05 |
| `ait-agent pty::tests::the_agent_shell_arms_the_ssh_policy` | open — flake seen R13-T2 (fails while other test binaries still compile; passes isolated and under a quiet parallel load), file untouched since R09 |
| `ait-agent pty::tests::prompt_marks_settle_an_injected_shell` | open — flake seen R13-T6 (the `QUIET_WINDOW + SETTLE_POLL * 8` bound tripped under compile load, 718 ms; passes serially and isolated), file untouched since R09 |
| `ait-agent write_to_pty_first_write_asks_then_the_session_runs_free` | open — flake seen R14-T2 (the third captured request raced the shell's `READ-hello` echo under sweep load, twice; passes isolated twice), file untouched since R10 |
| `ait-term the_output_mirror_receives_bytes_as_the_reader_reads_them` | open — flake seen R14-T5 (the reader's mirror tap lost the race to `early-mark`'s grid write under sweep load; passes isolated three times), file untouched since E01 |

The underlying PTY races are tracked in the `Backlog: PTY-timing
flakes under parallel test load` issue — the entries above leave the
list when that issue (or a fix) lands. Older R08-era sightings lived
in `docs/using-the-testkit.md`'s retired local list (removed R13);
their tests were one-off reruns, not recurring flakes.

## Adding an entry

A test earns a row when it fails under load and passes isolated twice —
two sightings, not one bad run. Note the round it was seen in and the
last commit that touched its file; a flake in freshly edited code is a
bug in the round, not a flake. Append the `<crate> <test-name>` line to
`scripts/flaky-tests.txt` in the same commit so check.sh picks it up.
