//! TOML acceptance scenarios for the GUI (Spec R15, ticket R15-T1):
//! every `*.toml` file under `tests/scenarios/` is one `GuiScenario` —
//! a `feature` tag plus ordered steps — run against the real `ait-gui`
//! binary under a display, driven over the test-driver socket.
//!
//! Skip conventions keep the default workspace gate clean:
//!
//! - Without the `test-driver` feature the binary binds no driver
//!   socket, so the whole suite reports itself skipped (plain
//!   `cargo test --workspace` hits this — the suite runs under
//!   `cargo test -p ait-gui --features test-driver`).
//! - `GuiE2e::spawn` returns `None` when no display is reachable
//!   (headless with no `xvfb-run` and no `DISPLAY`) — each scenario
//!   skips rather than fails.
//! - The dual-binary matrix (R15-T5): a scenario's `binary = "web"` /
//!   `"noweb"` declares which `ait-gui` build it needs. The leg
//!   resolves through `AIT_GUI_E2E_BIN_WEB`/`AIT_GUI_E2E_BIN_NOWEB`
//!   first, else the cargo-built binary when its feature set matches
//!   (`cfg!(feature = "web")`); a scenario whose leg can't resolve
//!   skips loudly — `cargo test -p ait-gui --features test-driver`
//!   runs the `web` leg, `--no-default-features --features
//!   test-driver` the `noweb` one, and either invocation plus the
//!   other leg's env var runs everything.
//!
//! Each scenario runs on a thread named after its file stem, so a
//! failure's `dump.json`/`actions.log` lands under
//! `target/e2e-failures/<scenario>/` — the PTY suite's convention.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;

use ait_llm::testing::{Fixture, Script};
use ait_testkit::gui::{GuiBinary, GuiE2e, GuiScenario, GuiSpawnConfig};
use ait_testkit::{ait_gui_bin, panic_text};

/// The env var naming a `web`+`test-driver` `ait-gui` — a
/// `binary = "web"` scenario's override when this test binary was
/// itself built feature-off (or when pointing at a non-default path).
const ENV_BIN_WEB: &str = "AIT_GUI_E2E_BIN_WEB";
/// The env var naming a `--no-default-features`+`test-driver`
/// `ait-gui` — a `binary = "noweb"` scenario's override.
const ENV_BIN_NOWEB: &str = "AIT_GUI_E2E_BIN_NOWEB";

/// Resolve the binary a scenario needs: the leg's env override
/// first, else the cargo-built binary when its feature set answers
/// the leg — `cfg!(feature = "web")` is the same flag the
/// `CARGO_BIN_EXE_ait-gui` binary was compiled with. `Err` is the
/// skip-loudly message naming how to supply the leg.
fn bin_for(binary: GuiBinary) -> Result<std::ffi::OsString, String> {
    // Env overrides absolutize: the spawn re-roots the app's cwd to
    // a throwaway dir, so a relative path would dissolve there —
    // `ait_gui_bin!` is already absolute.
    let leg = |var: &str| {
        std::env::var_os(var).and_then(|path| std::path::absolute(path).ok().map(Into::into))
    };
    match binary {
        GuiBinary::Any => Ok(ait_gui_bin!()),
        GuiBinary::Web => leg(ENV_BIN_WEB)
            .or_else(|| cfg!(feature = "web").then(|| ait_gui_bin!()))
            .ok_or_else(|| {
                format!(
                    "needs a `web`+`test-driver` ait-gui — set {ENV_BIN_WEB}, or \
                     run `cargo test -p ait-gui --features test-driver`"
                )
            }),
        GuiBinary::Noweb => leg(ENV_BIN_NOWEB)
            .or_else(|| (!cfg!(feature = "web")).then(|| ait_gui_bin!()))
            .ok_or_else(|| {
                format!(
                    "needs a `--no-default-features`+`test-driver` ait-gui — set \
                     {ENV_BIN_NOWEB}, or run `cargo test -p ait-gui \
                     --no-default-features --features test-driver`"
                )
            }),
    }
}

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

/// One scenario end to end on its own named thread: load, pick the
/// binary its `binary` leg names, spawn `ait-gui` with the
/// scenario's `env`/`headed`/`stub_opener`, run its steps. Returns
/// `true` when the scenario actually ran — a `false` means a skip
/// (display or binary leg). Panics propagate to the join below while
/// the dropping `GuiE2e` leaves the evidence dump under the thread's
/// name.
fn run_scenario(path: &Path) -> bool {
    let scenario = GuiScenario::load(path).unwrap_or_else(|e| panic!("{e}"));
    let bin = match bin_for(scenario.binary()) {
        Ok(bin) => bin,
        Err(reason) => {
            eprintln!("{}: skipped ({reason})", scenario.name());
            return false;
        }
    };
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
        let handle = std::thread::Builder::new()
            .name(name.clone())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || run_scenario(&path))
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

/// R15-T5 — the in-page navigation sync (Spec R14 §5.13 us-18): a
/// page-initiated location change reports through the wry navigation
/// handler into `BrowserState::webview_navigated`, so the address
/// entry and our own history track the page the webview is actually
/// on — with NO second fetch queued (`loads` stays flat: the webview
/// already navigated). TOML can't write a `file://` fixture inside
/// the throwaway cwd, so this leg is code-first. Needs the `web`
/// binary — a feature-off build has no webview to navigate.
#[test]
fn browser_in_page_nav_syncs_address() {
    if !cfg!(feature = "test-driver") {
        eprintln!(
            "skipping browser_in_page_nav: ait-gui was built without the \
             `test-driver` feature"
        );
        return;
    }
    let bin = match bin_for(GuiBinary::Web) {
        Ok(bin) => bin,
        Err(reason) => {
            eprintln!("browser_in_page_nav: skipped ({reason})");
            return;
        }
    };
    let Some(app) =
        GuiE2e::spawn(GuiSpawnConfig::new(bin)).unwrap_or_else(|e| panic!("spawn ait-gui: {e}"))
    else {
        eprintln!("browser_in_page_nav: skipped (no display reachable)");
        return;
    };

    // The fixture pair lives in the app's own throwaway cwd — page A
    // carries both a script navigation and a meta-refresh fallback to
    // page B, so whichever WebKitGTK counts as a navigation action
    // fires the report.
    let dir = app.cwd().join("pages");
    std::fs::create_dir(&dir).unwrap();
    let page_b = format!("file://{}", dir.join("page-b.html").display());
    std::fs::write(
        dir.join("page-a.html"),
        "<!doctype html><meta charset=\"utf-8\"><title>a</title>\n\
         <script>location.href = 'page-b.html';</script>\n\
         <meta http-equiv=\"refresh\" content=\"0;url=page-b.html\">\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("page-b.html"),
        "<!doctype html><meta charset=\"utf-8\"><title>b</title><p>page b</p>\n",
    )
    .unwrap();

    app.wait_for_dump("the shell prompt", |dump| dump.grid_text().contains('$'));
    app.key("Ctrl+Shift+B");
    app.wait_for_dump("the embedded browser", |dump| {
        dump.pane.shown.as_deref() == Some("browser") && dump.browser.surface == "embedded"
    });
    let page_a = format!("file://{}", dir.join("page-a.html").display());
    app.text(&page_a);
    app.key("Enter");
    app.wait_for_dump("page a loaded", |dump| {
        dump.browser.current.as_deref() == Some(page_a.as_str())
    });
    let our_load = app.dump().browser.loads;

    // The page navigates itself — the entry and our history follow
    // the page the webview is on, and nothing re-fetches.
    app.wait_for_dump("the in-page navigation syncs the entry", |dump| {
        dump.browser.entry == page_b
            && dump.browser.current.as_deref() == Some(page_b.as_str())
            && dump.browser.history == 2
            && dump.browser.back
            && dump.browser.loads == our_load
    });
    // `‹`/`Alt+←` then returns to the pre-click page, not the typed
    // one — the history entry IS the reported page.
    app.key("Alt+Left");
    app.wait_for_dump("back returns to page a", |dump| {
        dump.browser.current.as_deref() == Some(page_a.as_str()) && dump.browser.entry == page_a
    });
    app.quit();
}

/// R15-T5 — the webview child's bounds contract (Spec R14 §5.13):
/// the staged body rect IS what the X11 child is repositioned over
/// every frame (`build_as_child` won't auto-resize), so a window
/// resize must move it, and closing the pane must drop it — `None`
/// is the hide signal the sync turns into `set_visible(false)`.
/// Needs the `web` binary.
#[test]
fn browser_webview_tracks_resize_and_hides() {
    if !cfg!(feature = "test-driver") {
        eprintln!(
            "skipping browser_webview_tracks: ait-gui was built without the \
             `test-driver` feature"
        );
        return;
    }
    let bin = match bin_for(GuiBinary::Web) {
        Ok(bin) => bin,
        Err(reason) => {
            eprintln!("browser_webview_tracks: skipped ({reason})");
            return;
        }
    };
    let Some(app) =
        GuiE2e::spawn(GuiSpawnConfig::new(bin)).unwrap_or_else(|e| panic!("spawn ait-gui: {e}"))
    else {
        eprintln!("browser_webview_tracks: skipped (no display reachable)");
        return;
    };

    app.wait_for_dump("the shell prompt", |dump| dump.grid_text().contains('$'));
    app.key("Ctrl+Shift+B");
    app.wait_for_dump("the embedded browser with a staged rect", |dump| {
        dump.pane.shown.as_deref() == Some("browser")
            && dump.browser.surface == "embedded"
            && dump.browser.webview_rect.is_some()
    });
    let staged = app.dump().browser.webview_rect.unwrap();
    let window = app.dump().window;

    // Resize the window — the side region reflows and the staged
    // rect follows it (the sync's per-frame `set_bounds` target).
    app.resize(window.width - 160.0, window.height);
    app.wait_for_dump("the staged rect tracks the resize", |dump| {
        dump.browser.webview_rect.is_some_and(|rect| {
            (rect.x, rect.y, rect.w, rect.h) != (staged.x, staged.y, staged.w, staged.h)
        })
    });

    // The pane's own chord closes it — the rect clears (the hide
    // signal) and stays cleared.
    app.key("Ctrl+Shift+B");
    app.wait_for_dump("the webview hides on close", |dump| {
        dump.pane.shown.is_none() && dump.browser.webview_rect.is_none()
    });
    app.quit();
}

/// A `settings.toml` whose `command_model` resolves to the loopback
/// stub — the TUI e2e's fixture shape (no `api_key_env` and no
/// keyring entry resolve to no secret, so the request goes out
/// unauthenticated).
fn settings_with_stub(base_url: &str) -> String {
    format!(
        r#"
[agents.custom_endpoints.local]
name     = "Stub"
base_url = "{base_url}"
schema   = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name       = "stub-model"
alias      = "Stub Model"
config_key = "cmd-key"

[agents.execution_profiles.default]
command_model = "cmd-key"
"#
    )
}

/// Write `contents` as `<xdg>/ait/settings.toml` — the layout
/// `Settings::load` resolves under the pinned `XDG_CONFIG_HOME`.
fn write_settings(xdg: &Path, contents: &str) {
    let dir = xdg.join("ait");
    fs::create_dir_all(&dir).expect("create fixture config dir");
    fs::write(dir.join("settings.toml"), contents).expect("write fixture settings.toml");
}

/// One mid-stream provider error frame — a `{"error":{…}}` payload,
/// the shape the adapter's `Decoder` turns into `StreamEvent::Error`
/// → `SuggestEvent::Error` → the status toast.
fn error_chunk() -> Vec<u8> {
    br#"data: {"error":{"message":"stub said no","code":"fixture_err"}}

"#
    .to_vec()
}

/// Spawn `ait-gui` with `XDG_CONFIG_HOME` pointed at `xdg` — the
/// `env` pairs apply after the harness's pins and therefore override
/// them (the PTY suite's `spawn_ait_with_xdg` convention).
fn spawn_gui_with_xdg(xdg: &Path) -> Option<GuiE2e> {
    let bin = ait_gui_bin!();
    let mut config = GuiSpawnConfig::new(bin);
    config.env.push((
        OsString::from("XDG_CONFIG_HOME"),
        xdg.as_os_str().to_os_string(),
    ));
    GuiE2e::spawn(config).unwrap_or_else(|e| panic!("spawn ait-gui: {e}"))
}

/// R15-T6 — the suggestion triggers' user-visible arc on the GUI's
/// own status surface: `# `+Enter and `Alt+Enter` each arm the
/// suggestion (`suggesting` + the status strip's `suggesting`
/// segment) while the model call is in flight, and a stream error
/// clears the pin into a live toast — no real model, no recording,
/// no driver: the loopback `Fixture` IS the whole provider, and its
/// drip-fed SSE stream makes the in-flight window a deterministic
/// hold rather than a race against an immediate error.
#[test]
fn suggest_triggers_arm_then_clear_on_error() {
    if !cfg!(feature = "test-driver") {
        eprintln!(
            "skipping suggest_triggers: ait-gui was built without the \
             `test-driver` feature"
        );
        return;
    }
    // One held stream per trigger leg — `DripSse` answers `200
    // text/event-stream` and then blocks on the channel: the request
    // stays in flight until the test drips the error frame and drops
    // the sender (EOF flushes the buffered chunk to the client).
    let (drip_hash, hold_hash) = mpsc::channel::<Vec<u8>>();
    let (drip_alt, hold_alt) = mpsc::channel::<Vec<u8>>();
    let stub = Fixture::serve(vec![Script::DripSse(hold_hash), Script::DripSse(hold_alt)]);
    let xdg = tempfile::tempdir().expect("fixture tempdir");
    write_settings(xdg.path(), &settings_with_stub(&stub.base_url));

    let Some(app) = spawn_gui_with_xdg(xdg.path()) else {
        eprintln!("suggest_triggers: skipped (no display reachable)");
        return;
    };
    app.wait_for_dump("the shell prompt", |dump| dump.grid_text().contains('$'));

    // Leg one — `# `+Enter: the request submits as a comment and the
    // pin arms while the stub's stream hangs open.
    app.text("# list files");
    app.wait_for_dump("the echoed request line", |dump| {
        dump.grid_text().contains("# list files")
    });
    app.key("Enter");
    app.wait_for_dump("the armed suggestion", |dump| {
        dump.suggesting && dump.status.contains("suggesting")
    });
    // The stream answers a provider error: the pin clears and the
    // error lands as a live toast on the status line.
    drip_hash.send(error_chunk()).expect("drip the error frame");
    drop(drip_hash);
    app.wait_for_dump("the cleared pin and error toast", |dump| {
        !dump.suggesting
            && dump
                .toast
                .as_ref()
                .is_some_and(|toast| toast.live && toast.text.contains("stub said no"))
    });
    // The toast paints on the status strip — the dump's `status` is
    // the same string the paint pass draws.
    app.wait_for_dump("the toast on the status strip", |dump| {
        dump.status.contains("stub said no")
    });

    // Leg two — `Alt+Enter` on a bare typed line: the line itself is
    // the request, armed against the second held stream.
    app.wait_for_dump("a fresh prompt", |dump| dump.grid_text().contains('$'));
    app.text("undo my last commit");
    app.wait_for_dump("the echoed bare line", |dump| {
        dump.grid_text().contains("undo my last commit")
    });
    app.key("Alt+Enter");
    app.wait_for_dump("the armed alt-enter suggestion", |dump| {
        dump.suggesting && dump.status.contains("suggesting")
    });
    drip_alt.send(error_chunk()).expect("drip the error frame");
    drop(drip_alt);
    app.wait_for_dump("the cleared pin and error toast again", |dump| {
        !dump.suggesting
            && dump
                .toast
                .as_ref()
                .is_some_and(|toast| toast.live && toast.text.contains("stub said no"))
    });
    app.quit();
}

/// R15-T6 — the test-driver socket and the ctl control socket
/// coexist under the spawned app's pinned `XDG_RUNTIME_DIR`: the
/// driver stays bound and servicing dumps while the real `ait`
/// binary's `ctl open-tab`/`ctl send` drive the same window through
/// the pump's ctl drain — the two sockets never fight.
#[test]
fn ctl_and_driver_sockets_coexist() {
    if !cfg!(feature = "test-driver") {
        eprintln!(
            "skipping ctl_coexistence: ait-gui was built without the \
             `test-driver` feature"
        );
        return;
    }
    let gui_bin = ait_gui_bin!();
    // The `ait` ctl client binary: the `AIT_E2E_BIN` override first,
    // else the `ait-gui` binary's sibling in the workspace target
    // dir (`cargo test --workspace` builds both). Under a lone
    // `cargo test -p ait-gui` the sibling may not exist — skip like
    // the display convention rather than fail on a packaging gap.
    let ait = std::env::var_os("AIT_E2E_BIN")
        .map(PathBuf::from)
        .or_else(|| {
            let sibling = Path::new(&gui_bin).parent()?.join("ait");
            sibling.exists().then_some(sibling)
        });
    let Some(ait) = ait else {
        eprintln!("ctl_coexistence: skipped (no `ait` binary — set AIT_E2E_BIN)");
        return;
    };
    let Some(app) = GuiE2e::spawn(GuiSpawnConfig::new(gui_bin))
        .unwrap_or_else(|e| panic!("spawn ait-gui: {e}"))
    else {
        eprintln!("ctl_coexistence: skipped (no display reachable)");
        return;
    };
    app.wait_for_dump("the shell prompt", |dump| dump.grid_text().contains('$'));

    // `ait ctl` scans `$XDG_RUNTIME_DIR/ait` for a live socket — the
    // dir the spawn pinned, where the app's ctl socket sits beside
    // this harness's driver socket. Each call IS the coexistence
    // proof: it connects to the ctl socket while the driver's
    // `dump` traffic flows above and below it.
    let ctl = |args: &[&str]| -> std::process::Output {
        Command::new(&ait)
            .arg("ctl")
            .args(args)
            .env("XDG_RUNTIME_DIR", app.runtime_dir())
            .output()
            .expect("run ait ctl")
    };

    let opened = ctl(&["open-tab"]);
    assert!(
        opened.status.success(),
        "ait ctl open-tab failed: {}",
        String::from_utf8_lossy(&opened.stderr)
    );
    app.wait_for_dump("the ctl-opened tab active", |dump| {
        dump.tabs.titles.len() == 2 && dump.tabs.active == 1
    });

    let sent = ctl(&["send", "echo from-ctl"]);
    assert!(
        sent.status.success(),
        "ait ctl send failed: {}",
        String::from_utf8_lossy(&sent.stderr)
    );
    // ctl bytes land in the active tab's PTY — the echoed command
    // and its output render on exactly the grid the driver dumps.
    app.wait_for_dump("the ctl-typed command and its output", |dump| {
        let text = dump.grid_text();
        text.contains("echo from-ctl") && text.matches("from-ctl").count() >= 2
    });
    // `last-block` round-trips the same coexisting socket: an
    // interactive `sh` seals no blocks (no OSC 133 marks), so the
    // honest answer is the dispatch's `no sealed blocks` refusal —
    // reaching it at all IS the coexistence proof for this verb.
    let block = ctl(&["last-block"]);
    assert!(
        String::from_utf8_lossy(&block.stderr).contains("no sealed blocks")
            || String::from_utf8_lossy(&block.stdout).contains("no sealed blocks"),
        "ait ctl last-block answered neither a block nor the expected \
         refusal: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&block.stdout),
        String::from_utf8_lossy(&block.stderr)
    );
    app.quit();
}
