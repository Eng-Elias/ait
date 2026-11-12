//! `ait-gui` — AIT's native GUI frontend (Spec R13 §5.12,
//! REQUIREMENTS.md §5.12): an eframe/egui window over the wgpu backend
//! (ADR-0017) rendering the same [`ait_app`] session model the TUI
//! renders — tabs, the shared keymap, blocks, the agent panel's layout
//! region, the ctl socket — with no GUI types crossing into
//! `ait-app`/`ait-term`.
//!
//! The pieces mirror the TUI's architecture one-to-one so the
//! frontends can't drift:
//!
//! - [`keys`] — egui `Event` → [`ait_term::KeyEvent`], the counterpart
//!   of `ait`'s crossterm shim.
//! - [`grid`] — the pure cell→styled-run planner (wide-char cover,
//!   combining-char ride-along, the cursor-only damage filter).
//! - [`paint`] — the `Painter` pass over the planner's output.
//! - [`panel`] — the agent panel's pure line planning (entries →
//!   styled rows, the pinned plan block, the parked gate card).
//! - [`pane`] — the side region's pane host (Spec R14 §5.13): the
//!   `Pane` trait, the static registry, the app-global selection
//!   state and the `Ctrl+Shift+E`/`Ctrl+Shift+B` chord interception.
//! - [`browser`] — the web browser pane (R14-T3): entry/history/nav
//!   model and the embed-vs-external fallback decision, all pure.
//! - [`webview`] — the `wry` adapter behind the `web` feature: the
//!   child-webview handle, the gtk/glib plumbing and the platform
//!   opener the external surface delegates to.
//! - [`layout`] — the five regions' pixel math (tab bar, content band,
//!   side panel, status strip, modal layer) and the winsize derivation.
//! - [`pump`] — the drain thread + `request_repaint` wakeups, the
//!   `headless` drain loop's windowed counterpart.
//! - [`app`] — the `eframe::App` itself: input dispatch, the shared
//!   state the pump owns between frames, the region painters.
//!
//! [`run`] is the binary's one call — `ait gui` (Spec R13's
//! subcommand seam, wired in T7) and the `ait-gui` binary share it.

// `pub(crate)` across the board: the crate's binary-facing surface
// is `run` + `report_error` below — the modules are the frontend's
// internals (`app`'s `GuiApp`/`Shared`, the pump, the painters),
// shared inside the crate and invisible outside it.
pub(crate) mod app;
pub(crate) mod browser;
pub(crate) mod grid;
pub(crate) mod keys;
pub(crate) mod layout;
pub(crate) mod paint;
pub(crate) mod pane;
pub(crate) mod panel;
pub(crate) mod pump;
pub(crate) mod webview;

use std::io::Write as _;

use app::GuiApp;

/// Open the native window and run the GUI frontend — the counterpart
/// of `ait`'s TUI `run`. Blocks until the window closes; a spawn
/// failure surfaces as the `run_native` error.
pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("ait")
            .with_inner_size([960.0, 540.0]),
        // wgpu is the renderer (ADR-0017); `Renderer::Wgpu` is enabled
        // by the manifest's `wgpu` feature.
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "ait-gui",
        options,
        Box::new(|cc| Ok(Box::new(GuiApp::new(cc)?))),
    )
}

/// The binary's error path: `run`'s failure text to stderr and a
/// nonzero exit — not the std print macros (the src-hygiene rule bans
/// them outside `ait/src/main.rs`), but `io::stderr().write_all`.
pub fn report_error(error: &eframe::Error) {
    let _ = writeln!(std::io::stderr(), "ait-gui: {error}");
}
