//! TOML acceptance scenarios (Spec E01, ticket E01-T4): every `*.toml`
//! file under `tests/scenarios/` is one scenario — a `feature` tag plus
//! ordered steps — run against the real `ait` binary in a real PTY.
//! Each scenario runs on a thread named after its file stem, so a
//! failure dumps evidence under `target/e2e-failures/<scenario>/` and
//! agent-step recordings resolve under `tests/recordings/<scenario>/`
//! — the same conventions a `#[test]` thread name produces.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use ait_testkit::{ait_bin, panic_text, E2e, Scenario};

/// The committed scenario set, sorted for deterministic order.
fn scenario_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read scenarios dir {}: {e}", dir.display()))
        .map(|entry| entry.expect("scenario dir entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "no scenarios committed under {}",
        dir.display()
    );
    files
}

/// One scenario end to end on its own named thread: load, spawn `ait`
/// with the scenario's `size`/`args`/`env`, run its steps. Any panic
/// propagates out of the thread (the join below collects it) while the
/// dropping `E2e` leaves the failure dump under this thread's name.
fn run_scenario(path: &Path, bin: &OsStr) {
    let scenario = Scenario::load(path).unwrap_or_else(|e| panic!("{e}"));
    // The `platforms` gate (Spec R16-T8): an excluded scenario skips
    // loudly rather than spawning `ait` — the file stays committed for
    // the OSes it targets, and this OS's leg says why it sat out.
    if !scenario.runs_on_current_platform() {
        eprintln!(
            "{}: skipped (platforms = {:?}, this OS is {})",
            scenario.name(),
            scenario.platforms(),
            std::env::consts::OS,
        );
        return;
    }
    let mut app = E2e::spawn(scenario.spawn_config(bin)).expect("spawn ait");
    scenario.run(&mut app);
}

/// Discover and run every committed scenario sequentially, then report
/// all failures at once — a failing scenario names its file and step in
/// the message and leaves its dump under its own name.
#[test]
fn scenarios() {
    let mut files = scenario_files();
    // `AIT_E2E_ONLY=<name>` — run just the scenario(s) whose file stem
    // contains it (one scenario to iterate on); a miss names the stems.
    // Mirrors `AIT_GUI_E2E_ONLY` in ait-gui's suite.
    if let Ok(only) = std::env::var("AIT_E2E_ONLY") {
        files.retain(|path| {
            path.file_stem()
                .is_some_and(|stem| stem.to_string_lossy().contains(&only))
        });
        assert!(
            !files.is_empty(),
            "AIT_E2E_ONLY={only:?} matched no scenario — the stems are: {}",
            scenario_files()
                .iter()
                .filter_map(|path| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let mut failures = Vec::new();
    for path in files {
        let name = path
            .file_stem()
            .expect("a scenario file has a stem")
            .to_string_lossy()
            .into_owned();
        let bin = ait_bin!();
        let handle = std::thread::Builder::new()
            .name(name.clone())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || run_scenario(&path, &bin))
            .expect("spawn the scenario's thread");
        if let Err(payload) = handle.join() {
            failures.push(format!("{name}: {}", panic_text(payload.as_ref())));
        }
    }
    assert!(
        failures.is_empty(),
        "{} scenario(s) failed:\n{}",
        failures.len(),
        failures.join("\n---\n")
    );
}
