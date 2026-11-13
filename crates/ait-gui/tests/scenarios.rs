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

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ait_testkit::gui::{
    GuiE2e, GuiScenario, GuiSpawnConfig, PointerEventKind, PreviewDump, ScrollUnit,
};
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

/// R15-T4 — spawn `ait-gui` for a code-first leg with the suite's
/// feature/display skip conventions (the TOML runner's twin).
fn spawn_leg(leg: &str, env: Vec<(OsString, OsString)>) -> Option<GuiE2e> {
    if !cfg!(feature = "test-driver") {
        eprintln!("skipping {leg}: ait-gui was built without the `test-driver` feature");
        return None;
    }
    let bin = ait_gui_bin!();
    let mut config = GuiSpawnConfig::new(bin);
    config.env = env;
    match GuiE2e::spawn(config) {
        Ok(app) => app,
        Err(error) => panic!("{leg}: spawn ait-gui: {error}"),
    }
}

/// Wait on the dump's explorer block — `predicate` sees `&ExplorerDump`.
fn wait_explorer(
    app: &GuiE2e,
    description: &str,
    predicate: impl Fn(&ait_testkit::gui::ExplorerDump) -> bool,
) {
    app.wait_for_dump(description, |dump| {
        dump.explorer.as_ref().is_some_and(&predicate)
    });
}

/// The `(column, row)`-in-points of a grid cell — `grid`'s rect plus
/// the dump's cell metrics. Returns the cell's center.
fn cell_center(dump: &ait_testkit::gui::Dump, column: usize, row: usize) -> Option<(f32, f32)> {
    let grid = dump.hit("grid")?;
    let cell = dump.grid.as_ref()?.cell?;
    Some((
        grid.x + (column as f32 + 0.5) * cell.width,
        grid.y + (row as f32 + 0.5) * cell.height,
    ))
}

/// R15-T4 — the explorer's refresh and editor spawn, the two legs
/// TOML can't see: `r` re-reads the browsed dir (a new file appears
/// only then — no watcher, us-14), and `o`'s editor resolution
/// (`[gui].editor` → `$VISUAL` → `$EDITOR` → `platform_opener`)
/// lands on the harness's `stub-bin` shadow — its `xdg-open`
/// records the invocation to `stub_log` and never launches a real
/// application.
#[test]
fn explorer_editor_stub_and_refresh() {
    // VISUAL/EDITOR blanked so `resolve_editor` walks to the stub.
    let Some(app) = spawn_leg(
        "explorer_editor_stub",
        vec![
            (OsString::from("VISUAL"), OsString::from("")),
            (OsString::from("EDITOR"), OsString::from("")),
        ],
    ) else {
        return;
    };
    app.wait_for_dump("the shell prompt", |dump| dump.grid_text().contains('$'));
    app.text("mkdir -p fx && printf 'HEAD-LINE\\n' > fx/head.txt && printf 'FIX-%s\\n' UP");
    app.key("Enter");
    app.wait_for_dump("the fixture marker", |dump| {
        dump.grid_text().contains("FIX-UP")
    });
    // `cd` + an explicit OSC-7 report — the non-integrated `/bin/sh`
    // never reports, and `cwd_for`'s memo would keep the `/proc`
    // answer until the tab moves.
    app.text("cd fx && printf '\\033]7;file://h%s\\aCWD-%s\\n' \"$PWD\" OK");
    app.key("Enter");
    app.wait_for_dump("the cwd marker", |dump| dump.grid_text().contains("CWD-OK"));
    app.key("Ctrl+Shift+E");
    wait_explorer(&app, "the explorer rooted at the fixture", |explorer| {
        explorer.dir.ends_with("/fx") && explorer.entries.iter().any(|e| e.name == "head.txt")
    });

    // A file the listing hasn't seen stays absent until `r` — the
    // no-watcher contract. `key_twin` is the real press's shape:
    // `Key` + the composed `Text("r")` the pane's verb reads.
    let created = app.cwd().join("fx/newr.txt");
    fs::write(&created, "fresh\n").unwrap_or_else(|e| panic!("write {created:?}: {e}"));
    app.key_twin("r");
    wait_explorer(&app, "the refresh picking newr.txt up", |explorer| {
        explorer.entries.iter().any(|e| e.name == "newr.txt")
    });

    // Select the fixture file by its hit name, then `o` — the editor
    // resolution walks to `platform_opener`, whose stub records.
    app.wait_for_dump("the head.txt row's hit", |dump| {
        dump.hit("explorer.entry.head.txt").is_some()
    });
    app.click("explorer.entry.head.txt");
    wait_explorer(&app, "the head.txt selection + preview", |explorer| {
        explorer
            .selected
            .as_ref()
            .is_some_and(|entry| entry.name == "head.txt")
            && matches!(&explorer.preview, Some(PreviewDump::Text { .. }))
    });
    app.key_twin("o");

    // The stub's log records `xdg-open <path>` — bounded poll, the
    // harness's only sanctioned wait shape.
    let log = app.stub_log();
    let deadline = Instant::now() + Duration::from_secs(10);
    let invoked = loop {
        match fs::read_to_string(&log) {
            Ok(text)
                if text.lines().any(|line| {
                    line.contains("head.txt")
                        && (line.starts_with("xdg-open") || line.starts_with("open "))
                }) =>
            {
                break text;
            }
            _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => panic!(
                "the stub opener never logged a head.txt spawn\n--- open.log ---\n{}",
                fs::read_to_string(&log).unwrap_or_else(|e| format!("<unreadable: {e}>"))
            ),
        }
    };
    assert!(
        invoked.lines().all(|line| line.contains("head.txt")),
        "the stub opener should see only head.txt spawns, got {invoked:?}"
    );
    app.quit();
}

/// R15-T4 — the explorer's cwd tracking across a tab switch: tab 1
/// roots at its OSC-7-reported cwd, the fresh tab 2 inherits that
/// cwd (the `TabManager`'s new-tab rule — its `/proc` answer IS the
/// same dir, so the root holds), tab 2's own `cd` + report re-roots
/// it, and switching back re-roots to tab 1's report — the
/// `cwd_memo` key moves with the tab.
#[test]
fn explorer_reroots_on_tab_switch() {
    let Some(app) = spawn_leg("explorer_tab_reroot", Vec::new()) else {
        return;
    };
    app.wait_for_dump("the shell prompt", |dump| dump.grid_text().contains('$'));
    app.text("mkdir -p fx && printf 'FIX-%s\\n' UP");
    app.key("Enter");
    app.wait_for_dump("the fixture marker", |dump| {
        dump.grid_text().contains("FIX-UP")
    });
    app.text("cd fx && printf '\\033]7;file://h%s\\aCWD-%s\\n' \"$PWD\" OK");
    app.key("Enter");
    app.wait_for_dump("the cwd marker", |dump| dump.grid_text().contains("CWD-OK"));
    app.key("Ctrl+Shift+E");
    wait_explorer(&app, "tab 1's reported root", |explorer| {
        explorer.root.as_deref().is_some_and(|r| r.ends_with("/fx"))
    });

    // Alt+T opens tab 2 — its shell spawns in the ACTIVE tab's
    // tracked cwd (`fx`), so the root correctly holds.
    app.key("Alt+T");
    app.wait_for_dump("tab 2's prompt", |dump| dump.grid_text().contains('$'));
    wait_explorer(&app, "tab 2 inheriting tab 1's cwd", |explorer| {
        explorer.root.as_deref().is_some_and(|r| r.ends_with("/fx"))
    });
    // Tab 2's own cd + report moves its root to the parent — the
    // per-tab memo re-evaluates on the new report. The shown pane
    // owns composed text while it's up, so `Esc` drops the region
    // for the shell command and the chord brings it back.
    app.key("Escape");
    app.text("cd .. && printf '\\033]7;file://h%s\\aCWD2-%s\\n' \"$PWD\" OK");
    app.key("Enter");
    app.wait_for_dump("tab 2's cwd marker", |dump| {
        dump.grid_text().contains("CWD2-OK")
    });
    app.key("Ctrl+Shift+E");
    let spawn = fs::canonicalize(app.cwd()).expect("canonicalize the harness cwd");
    wait_explorer(&app, "tab 2's reported root", |explorer| {
        explorer
            .root
            .as_deref()
            .is_some_and(|r| fs::canonicalize(r).is_ok_and(|root| root == spawn))
    });

    // Back to tab 1 — `tab.0`'s chip is the registry's name for it —
    // and its reported `fx` re-roots the pane.
    app.wait_for_dump("the first tab chip", |dump| dump.hit("tab.0").is_some());
    app.click("tab.0");
    wait_explorer(&app, "tab 1's root restored", |explorer| {
        explorer.root.as_deref().is_some_and(|r| r.ends_with("/fx"))
            && explorer.dir.ends_with("/fx")
    });
    app.quit();
}

/// R15-T4 — the Ctrl-hover affordance's negative legs (Spec R14
/// us-24, ticket #194): a plain Ctrl+hover over a live token
/// affords, the same hover over a NONEXISTENT path doesn't (the
/// token never even registers a hit name), a `Ctrl+Shift` chord
/// doesn't (only plain Ctrl — chords keep their other meanings),
/// and a scrollback row doesn't (a scrollback path resolves against
/// a cwd the row never knew). The scrollback leg reuses the live
/// row's exact coordinates: the same cells afforded at offset 0,
/// so the same position refusing after the scroll IS the gate.
#[test]
fn ctrl_hover_ignores_missing_paths_chords_and_scrollback() {
    let Some(app) = spawn_leg("ctrl_hover_negatives", Vec::new()) else {
        return;
    };
    app.wait_for_dump("the shell prompt", |dump| dump.grid_text().contains('$'));
    // Row 0 the prompt, row 1 this echo, row 2 the token line — the
    // `%s`-split keeps the echo token-free (std.15).
    app.text("printf 'htt%s://x.co/a /missing-t4-nope\\n' ps");
    app.key("Enter");
    app.wait_for_dump("the url hit", |dump| {
        dump.hit("detect.url.https://x.co/a").is_some()
    });

    let before = app.dump();
    let grid = before.grid.as_ref().expect("a tab is active");
    let row = grid
        .rows
        .iter()
        .position(|line| line.contains("https://x.co/a"))
        .expect("the token row is in the live viewport");
    let url_col = grid.rows[row].find("https://x.co/a").unwrap() + 1;
    let miss_col = grid.rows[row].find("/missing-t4-nope").unwrap() + 1;
    // The nonexistent path never registers — filtered at token
    // scan, not just at affordance time.
    assert!(
        before.hit("detect.path./missing-t4-nope").is_none(),
        "a nonexistent path must not register a detect hit"
    );

    let (x, y) = cell_center(&before, url_col, row).expect("cell metrics painted");
    let (mx, my) = cell_center(&before, miss_col, row).expect("cell metrics painted");

    // Positive control at offset 0 — the same coordinates must
    // afford BEFORE the row rolls into scrollback.
    app.pointer_modifiers("Ctrl");
    app.pointer_at(PointerEventKind::Move, x, y);
    app.wait_for_dump("the live url affording", |dump| {
        dump.detect.as_ref().is_some_and(|detect| {
            detect.ctrl
                && detect
                    .token
                    .as_ref()
                    .is_some_and(|token| token.target == "url:https://x.co/a")
        })
    });

    // The nonexistent path's cell: Ctrl-hover resolves nothing.
    app.pointer_at(PointerEventKind::Move, mx, my);
    app.wait_for_dump("the missing path not affording", |dump| {
        dump.detect
            .as_ref()
            .is_some_and(|detect| detect.ctrl && detect.token.is_none())
    });

    // A chorded hover keeps the other meanings — `ctrl` flips false
    // only once the Ctrl+Shift frame is processed, so this wait is
    // a real transition, not a trivial pass.
    app.pointer_on(
        PointerEventKind::Move,
        "detect.url.https://x.co/a",
        0.0,
        0.0,
        Some("Ctrl+Shift"),
    );
    app.wait_for_dump("the chorded hover not affording", |dump| {
        dump.detect
            .as_ref()
            .is_some_and(|detect| !detect.ctrl && detect.token.is_none())
    });
    app.pointer_modifiers("Ctrl");

    // Push the token row into scrollback, scroll to the bottom of
    // history, and hover the same coordinates — the scrollback row
    // shows the same text but the affordance gate rejects it.
    app.pointer_at(PointerEventKind::Move, x, y);
    app.wait_for_dump("affording again before the scroll", |dump| {
        dump.detect
            .as_ref()
            .is_some_and(|detect| detect.ctrl && detect.token.is_some())
    });
    app.text("seq 1 60");
    app.key("Enter");
    app.wait_for_dump("the scrollback filling", |dump| {
        dump.grid.as_ref().is_some_and(|grid| grid.scrollback >= 30)
    });
    app.scroll(ScrollUnit::Line, 0.0, 500.0);
    app.wait_for_dump("scrolled to the oldest rows", |dump| {
        dump.grid
            .as_ref()
            .is_some_and(|grid| grid.scroll_offset >= grid.scrollback && grid.scrollback >= 30)
    });
    // The token's scrollback ordinal is its old live row index —
    // every row before it scrolled off first — so the same screen
    // coordinates hover the scrollback copy.
    app.pointer_at(PointerEventKind::Move, x, y);
    app.wait_for_dump("the scrollback row not affording", |dump| {
        dump.detect
            .as_ref()
            .is_some_and(|detect| detect.ctrl && detect.token.is_none())
    });
    // And a Ctrl+click there routes nothing — the pane stays shut.
    app.click_at(x, y);
    std::thread::sleep(Duration::from_millis(300));
    let settled = app.dump();
    assert!(
        settled.pane.shown.is_none(),
        "a scrollback Ctrl+click opened a pane: {:?}",
        settled.pane
    );
    app.pointer_modifiers("none");
    app.quit();
}
