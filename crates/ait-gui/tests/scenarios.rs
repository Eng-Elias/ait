//! TOML acceptance scenarios for the GUI (Spec R15, ticket R15-T1):
//! every `*.toml` file under `tests/scenarios/` is one `GuiScenario` —
//! a `feature` tag plus ordered steps — run against the real `ait-gui`
//! binary under a display, driven over the test-driver socket.
//!
//! Two skip conventions keep the default workspace gate clean:
//!
//! - Without the `test-driver` feature the binary binds no driver
//!   socket, so the whole suite reports itself skipped (plain
//!   `cargo test --workspace` hits this — the suite runs under
//!   `cargo test -p ait-gui --features test-driver`).
//! - `GuiE2e::spawn` returns `None` when no display is reachable
//!   (headless with no `xvfb-run` and no `DISPLAY`) — each scenario
//!   skips rather than fails.
//!
//! Each scenario runs on a thread named after its file stem, so a
//! failure's `dump.json`/`actions.log` lands under
//! `target/e2e-failures/<scenario>/` — the PTY suite's convention.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use ait_testkit::gui::{GuiE2e, GuiScenario, GuiSpawnConfig};
use ait_testkit::{ait_gui_bin, panic_text};

/// The committed scenario set, sorted for deterministic order.
fn scenario_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read scenarios dir {}: {e}", dir.display()))
        .map(|entry| entry.expect("scenario dir entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    files.sort();
    files
}

/// One scenario end to end on its own named thread: load, spawn
/// `ait-gui` with the scenario's `env`/`headed`, run its steps.
/// Returns `true` when the scenario actually ran — a `false` means a
/// display-level skip. Panics propagate to the join below while the
/// dropping `GuiE2e` leaves the evidence dump under the thread's name.
fn run_scenario(path: &Path, bin: &OsStr) -> bool {
    let scenario = GuiScenario::load(path).unwrap_or_else(|e| panic!("{e}"));
    let Some(app) =
        GuiE2e::spawn(scenario.spawn_config(bin)).unwrap_or_else(|e| panic!("spawn ait-gui: {e}"))
    else {
        eprintln!("{}: skipped (no display reachable)", scenario.name());
        return false;
    };
    scenario.run(&app);
    true
}

/// Discover and run every committed scenario sequentially, then report
/// all failures at once — a failing scenario names its file and step in
/// the message and leaves its dump under its own name. A suite with no
/// runnable scenarios (all display-skipped) reports the skips and
/// passes — the gate's headless-leg convention.
#[test]
fn scenarios() {
    if !cfg!(feature = "test-driver") {
        eprintln!(
            "skipping GUI scenarios: ait-gui was built without the \
             `test-driver` feature — rerun with \
             `cargo test -p ait-gui --features test-driver`"
        );
        return;
    }
    let mut failures = Vec::new();
    let mut ran = 0;
    for path in scenario_files() {
        let name = path
            .file_stem()
            .expect("a scenario file has a stem")
            .to_string_lossy()
            .into_owned();
        let bin = ait_gui_bin!();
        let handle = std::thread::Builder::new()
            .name(name.clone())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || run_scenario(&path, &bin))
            .expect("spawn the scenario's thread");
        match handle.join() {
            Ok(true) => ran += 1,
            Ok(false) => {}
            Err(payload) => failures.push(format!("{name}: {}", panic_text(payload.as_ref()))),
        }
    }
    assert!(
        failures.is_empty(),
        "{} scenario(s) failed:\n{}",
        failures.len(),
        failures.join("\n---\n")
    );
    if ran == 0 {
        eprintln!("all GUI scenarios skipped: no display was reachable");
    }
}

/// R15-T2 — the `resize` verb and the winsize settle: an injected
/// `ViewportCommand::InnerSize` rides the real window-manager round
/// trip, and the session reflows only after the content band sits
/// still (`RESIZE_SETTLE`), never on the first frame that sees a new
/// size — the R13 resize-jank fix's contract. TOML can't express the
/// two-phase "changed window, unchanged winsize" observation, so this
/// leg is code-first.
#[test]
fn resize_settle_reflows_on_settle_not_per_frame() {
    if !cfg!(feature = "test-driver") {
        eprintln!(
            "skipping resize_settle: ait-gui was built without the \
             `test-driver` feature"
        );
        return;
    }
    let bin = ait_gui_bin!();
    let Some(app) =
        GuiE2e::spawn(GuiSpawnConfig::new(bin)).unwrap_or_else(|e| panic!("spawn ait-gui: {e}"))
    else {
        eprintln!("resize_settle: skipped (no display reachable)");
        return;
    };
    app.wait_for_dump("the shell prompt", |dump| dump.grid_text().contains('$'));
    let before = app.dump();
    let columns = before.grid.as_ref().expect("a tab is active").columns;
    let width = before.window.width;
    // A narrower window — plenty of cells' worth, and never
    // clamped by the virtual screen.
    app.resize(width - 200.0, before.window.height);

    // Phase one: a frame that already sees the smaller window still
    // holds the old session winsize — the band is pending its
    // settle, not reflowing per frame.
    app.wait_for_dump("the resized window before the reflow", |dump| {
        dump.window.width != width
            && dump
                .grid
                .as_ref()
                .is_some_and(|grid| grid.columns == columns)
    });

    // Phase two: the settle fires and the session reflows to the
    // new band.
    app.wait_for_dump("the settled reflow", |dump| {
        dump.grid
            .as_ref()
            .is_some_and(|grid| grid.columns != columns)
    });
    app.quit();
}
