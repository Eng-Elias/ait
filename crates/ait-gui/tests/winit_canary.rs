//! R15-T7 — the winit canary (Spec R15's "winit canary" decision):
//! the driver socket injects `egui::Event`s at frame start, so every
//! scenario skips exactly one layer — winit's delivery of real X
//! input into egui. This test is the probe for that layer and
//! nothing more: `xdotool` sends a chord and a click *externally*
//! over XTest, and the `dump` asserts they landed. It is a canary,
//! never a gate for checklist behavior — the checklist coverage
//! lives in the scenarios and code-first tests beside it.
//!
//! Gating is deliberately two-deep:
//!
//! - `AIT_GUI_E2E_XTEST=1` opts the canary in — absent, it reports
//!   itself skipped (the default `cargo test -p ait-gui` run never
//!   reaches for external input tooling).
//! - The tools must exist: `xdotool` to inject and `Xvfb` to host a
//!   *private* display — the canary spawns its own `Xvfb :N` and pins
//!   the app to it via `GuiSpawnConfig::display`, because injecting
//!   synthetic input at a shared `DISPLAY` could click real windows.
//!   A missing tool skips loudly; it never fails.
//!
//! Once it is running, the assertions are real: a chord winit never
//! delivers leaves `pane.shown` empty and a click XTest never lands
//! opens no tab — either means the one un-driven layer broke.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use ait_testkit::ait_gui_bin;
use ait_testkit::gui::{env_flag, GuiE2e, GuiSpawnConfig, WAIT_TIMEOUT};

/// The opt-in env var — the canary runs only when named.
const ENV_XTEST: &str = "AIT_GUI_E2E_XTEST";

/// `tool` resolves on `PATH` — the tool gate's check.
fn on_path(tool: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(tool).is_file()))
}

/// A private `Xvfb` on a display number the scan picked — the canary
/// must own the X server end to end so its XTest input can never
/// reach another session's windows. Drop kills the server.
struct Xvfb {
    child: Child,
    display: String,
}

impl Xvfb {
    /// Start `Xvfb :N` on the first free display number and wait for
    /// its socket — `None` when no candidate came up in time.
    fn start() -> Option<Self> {
        for n in 90..200 {
            let lock = format!("/tmp/.X{n}-lock");
            let socket = format!("/tmp/.X11-unix/X{n}");
            if Path::new(&lock).exists() || Path::new(&socket).exists() {
                continue;
            }
            let Ok(mut child) = Command::new("Xvfb")
                .arg(format!(":{n}"))
                .args(["-screen", "0", "1280x800x24"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            else {
                continue;
            };
            let deadline = Instant::now() + Duration::from_secs(10);
            let up = loop {
                if Path::new(&socket).exists() {
                    break true;
                }
                match child.try_wait() {
                    // The server died — a stolen display number or a
                    // broken install; try the next candidate.
                    Ok(Some(_)) | Err(_) => break false,
                    Ok(None) => {}
                }
                if Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(25));
            };
            if up {
                return Some(Self {
                    child,
                    display: format!(":{n}"),
                });
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        None
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One `xdotool` invocation against the canary's private display —
/// `XAUTHORITY` scrubbed so an inherited cookie for another display
/// can't confuse the connection.
fn xdo(display: &str, args: &[&str]) -> Output {
    Command::new("xdotool")
        .args(args)
        .env("DISPLAY", display)
        .env_remove("XAUTHORITY")
        .output()
        .expect("spawn xdotool")
}

/// Assert an `xdotool` call succeeded — a tool failure mid-run is a
/// canary failure, not a skip (the tool gate already ran).
fn xdo_ok(display: &str, args: &[&str]) {
    let out = xdo(display, args);
    assert!(
        out.status.success(),
        "xdotool {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The app's X window id — `_NET_WM_PID` first (a pinned-display
/// spawn's child IS `ait-gui`, so its pid names the window), the
/// `WM_CLASS` and title as fallbacks (winit names the window `ait`,
/// class `ait-gui`). Bounded by [`WAIT_TIMEOUT`]; the window maps
/// during boot and the poll outlives slow first frames.
fn window_id(display: &str, app: &GuiE2e) -> u64 {
    let pid = app.pid().to_string();
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        for args in [
            vec!["search", "--pid", pid.as_str()],
            vec!["search", "--class", "ait-gui"],
            vec!["search", "--name", "ait"],
        ] {
            let out = xdo(display, &args);
            if out.status.success() {
                if let Some(id) = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .find_map(|line| line.trim().parse::<u64>().ok())
                {
                    return id;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "xdotool never found the ait-gui window on {display}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The canary: one external chord and one external click, both
/// asserted through the dump. `ctrl+shift+e` proves winit delivers a
/// named key event (the explorer pane shows), `Escape` proves a
/// plain key (the pane releases), and an XTest pointer over the
/// `tab.new` chip proves button delivery (a second tab opens).
#[test]
fn xtest_key_and_click_reach_winit() {
    if !cfg!(feature = "test-driver") {
        eprintln!(
            "skipping winit_canary: ait-gui was built without the \
             `test-driver` feature"
        );
        return;
    }
    if !env_flag(ENV_XTEST) {
        eprintln!("winit_canary: skipped (set {ENV_XTEST}=1 to run the xdotool canary)");
        return;
    }
    if !on_path("xdotool") {
        eprintln!("winit_canary: skipped ({ENV_XTEST} is set but no `xdotool` on PATH)");
        return;
    }
    if !on_path("Xvfb") {
        eprintln!(
            "winit_canary: skipped (no `Xvfb` on PATH — the canary \
             never injects into a shared DISPLAY)"
        );
        return;
    }
    let Some(xvfb) = Xvfb::start() else {
        eprintln!("winit_canary: skipped (Xvfb never raised a private display)");
        return;
    };
    let mut config = GuiSpawnConfig::new(ait_gui_bin!());
    config.display = Some(OsString::from(&xvfb.display));
    config.env = vec![
        // X11 only — a leaked WAYLAND_DISPLAY would divert the window
        // off the display xdotool is aiming at; the harness also
        // env-removes the wayland vars for a pinned display.
        (OsString::from("WINIT_UNIX_BACKEND"), OsString::from("x11")),
        (OsString::from("XAUTHORITY"), OsString::new()),
    ];
    let Some(app) =
        GuiE2e::spawn(config).unwrap_or_else(|e| panic!("winit_canary: spawn ait-gui: {e}"))
    else {
        eprintln!("winit_canary: skipped (no display reachable)");
        return;
    };
    app.wait_for_dump("the shell prompt", |dump| dump.grid_text().contains('$'));

    let window = window_id(&xvfb.display, &app).to_string();
    xdo_ok(&xvfb.display, &["windowactivate", "--sync", &window]);

    // The key leg: text typed over XTest lands on the prompt line…
    xdo_ok(&xvfb.display, &["type", "--delay", "15", "CANARYXTEST"]);
    app.wait_for_dump("the externally typed marker", |dump| {
        dump.grid_text().contains("CANARYXTEST")
    });
    // …and a named chord drives the app — the pane host answers a
    // real `Ctrl+Shift+E`.
    xdo_ok(&xvfb.display, &["key", "ctrl+shift+e"]);
    app.wait_for_dump("the explorer pane after the external chord", |dump| {
        dump.pane.shown.as_deref() == Some("explorer")
    });
    // Release it so the click leg's chip is the only live target.
    xdo_ok(&xvfb.display, &["key", "Escape"]);
    app.wait_for_dump("the pane released", |dump| dump.pane.shown.is_none());

    // The click leg: the hit registry's `tab.new` chip in window
    // coordinates — `mousemove --window` positions over it and XTest
    // presses the button.
    let (x, y) = app
        .dump()
        .hit("tab.new")
        .map(|rect| rect.center())
        .expect("the new-tab chip registered a hit target");
    xdo_ok(
        &xvfb.display,
        &[
            "mousemove",
            "--window",
            &window,
            &format!("{x:.0}"),
            &format!("{y:.0}"),
        ],
    );
    xdo_ok(&xvfb.display, &["click", "1"]);
    app.wait_for_dump("the externally clicked new tab", |dump| {
        dump.tabs.titles.len() == 2 && dump.tabs.active == 1
    });
    app.quit();
}
