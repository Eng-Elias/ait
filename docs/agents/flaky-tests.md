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
| `ait-agent write_to_pty_gates_then_round_trips_through_the_agent_pty` | open — flake seen R16-T1 (the recorded-requests assertion raced the PTY round trip under sweep load, twice in two gate runs; passes isolated four times), file untouched since R10 |
| `ait-term the_output_mirror_receives_bytes_as_the_reader_reads_them` | open — flake seen R14-T5 (the reader's mirror tap lost the race to `early-mark`'s grid write under sweep load; passes isolated three times), file untouched since E01 |
| `ait-agent demotion_never_replays_and_never_recovers` | open — flake seen r14-review-fixes (the ssh stub answered zero of the expected one failed run under sweep load; passes isolated twice and serially), file untouched since R09 |
| `ait-agent control_master_runs_commands_over_the_socket` | open — flake seen R15-T1 (the `ssh-cm` stub spawn hit `ETXTBSY` (os error 26) under sweep load; passes isolated three times), file untouched since R09 |
| `ait scenarios` (the whole TOML runner — name granularity skips the suite, the serial leg re-runs it) | open — flake seen R15-T6 (`agent-tab`'s recorded `act "open a new tab"` drifted: the second tab's spawn lagged the agent step's 500 ms settle under sweep load; passes isolated twice), file untouched since E01-T4 |
| `ait-gui scenarios` (the whole GUI TOML runner — name granularity skips the suite, the serial leg re-runs it) | open — flake seen R15-T5 (`scroll-pill-click`: a cold `noweb` run timed out with `scroll_offset` stuck at 9 under first-boot wgpu/Xvfb load; passed every re-run), file added in R15 |

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
