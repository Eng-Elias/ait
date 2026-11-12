//! The `wry` adapter — Spec R14 §5.13's deliberately untested seam
//! (R14-T3): everything decidable lives in `crate::browser`; this
//! file owns the `!Send` `wry::WebView` (it stays on `GuiApp`, on the
//! UI thread — never on `Shared`), the `gtk::init`/glib pumping the
//! Linux X11 embed needs, and the platform-opener spawn the
//! external-open surface delegates to.
//!
//! [`WebSlot::sync`] runs once per frame, AFTER the paint pass: the
//! browser pane's paint stages its body rect on `Shared.browser`, and
//! the sync probes (once), drains the queued navigations into the
//! webview or the system browser, and repositions the child over the
//! staged rect — `None` is the hide signal. `feature = "web"` swaps
//! the real host in; feature-off builds get the stub that probes to
//! [`WhyExternal::FeatureOff`] and sinks every navigation to the
//! opener. The pump/repaint model is untouched: the sync adds no
//! blocking calls, and an idle window still paints zero frames.

use std::time::Instant;

use ait_app::status::Toast;
use eframe::egui;
#[cfg(feature = "web")]
use raw_window_handle::HasWindowHandle;

use crate::app::Shared;
#[cfg(feature = "web")]
use crate::browser::WindowKind;
use crate::browser::{Embed, NavCommand, WhyExternal};

/// `GuiApp`'s webview slot — `host` is `Some` only after a successful
/// probe; feature-off builds never hold one ([`Host`] is a stub type
/// there, so the sync body stays uniform).
pub struct WebSlot {
    /// The live child webview — frontend-local and `!Send`, which is
    /// exactly why it lives here and not on `Shared`.
    host: Option<Host>,
}

impl WebSlot {
    /// No webview yet — the first frame the browser pane shows probes.
    pub fn new() -> Self {
        Self { host: None }
    }

    /// The per-frame sync — probe once (the pane's first shown frame),
    /// drain the queued navigations into the right sink, keep the
    /// child's bounds on the staged rect (hiding it whenever no rect
    /// is staged), and pump the glib context the X11 webview lives
    /// on. Cheap when idle: `None` surfaces, empty queues and a
    /// `feature = "web"`-less build all fall through.
    pub fn sync(&mut self, frame: &eframe::Frame, shared: &mut Shared, browser_shown: bool) {
        // The one-shot probe — needs a live frame for the window
        // handle, so it runs only while the pane shows. Its verdict
        // lands on `Shared.browser` where the pane's paint reads it.
        if browser_shown && shared.browser.embed() == Embed::Unknown {
            match self.probe(frame, shared.browser.surface()) {
                Ok(host) => {
                    self.host = Some(host);
                    shared.browser.set_embed(Embed::Embedded);
                }
                Err(why) => shared.browser.set_embed(Embed::External(why)),
            }
        }

        // The drain — `drain_nav` already folded the embed verdict in
        // (Navigate→OpenExternal while non-embedded), so `Navigate`
        // here only ever means "to the webview".
        for command in shared.browser.drain_nav() {
            match command {
                NavCommand::Navigate(url) => {
                    if let Some(host) = &self.host {
                        host.load_url(&url);
                    } else {
                        open_external(&url, &mut shared.toast);
                    }
                }
                NavCommand::Reload => {
                    if let Some(host) = &self.host {
                        host.reload();
                    }
                }
                NavCommand::OpenExternal(url) => open_external(&url, &mut shared.toast),
            }
        }

        // Bounds + visibility — `set_bounds` every frame is the X11
        // contract ("the webview will not auto-resize"); macOS takes
        // the same call for free. A missing rect is the pane's hide
        // signal: pane closed, or the body isn't a webview.
        if let Some(host) = &self.host {
            match shared.browser.surface() {
                Some(rect) => {
                    host.set_bounds(rect);
                    host.set_visible(true);
                }
                None => host.set_visible(false),
            }
        }

        self.pump_glib();
    }

    /// The one-shot embed attempt — the window handle picks the
    /// platform arm of the pure [`crate::browser::embed_supported`]
    /// matrix, then the runtime build at `rect` (the pane's staged
    /// body, or `None` while the paint ran pre-verdict — next frame's
    /// `set_bounds` corrects it). Feature-off: always
    /// [`WhyExternal::FeatureOff`].
    #[cfg(feature = "web")]
    fn probe(&self, frame: &eframe::Frame, rect: Option<egui::Rect>) -> Result<Host, WhyExternal> {
        let handle = frame.window_handle().map_err(|_| WhyExternal::NoWindow)?;
        crate::browser::embed_supported(WindowKind::of(handle.as_raw()))?;
        Host::new(frame, rect)
    }

    /// The feature-off probe — there is no runtime to try; the pane
    /// goes straight to the external surface.
    #[cfg(not(feature = "web"))]
    fn probe(
        &self,
        _frame: &eframe::Frame,
        _rect: Option<egui::Rect>,
    ) -> Result<Host, WhyExternal> {
        Err(WhyExternal::FeatureOff)
    }

    /// Advance the glib main context the X11 child webview lives on —
    /// wry's documented idiom, run per frame so gtk's signals never
    /// starve the event loop (unpumped, the webview wedges). Safe
    /// before `gtk::init` — `events_pending` just reports empty.
    #[cfg(all(feature = "web", target_os = "linux"))]
    fn pump_glib(&self) {
        while gtk::events_pending() {
            gtk::main_iteration_do(false);
        }
    }

    /// Non-Linux or feature-off — nothing to pump.
    #[cfg(not(all(feature = "web", target_os = "linux")))]
    fn pump_glib(&self) {}
}

/// The live `wry` child webview — `feature = "web"` only.
#[cfg(feature = "web")]
struct Host {
    /// The `!Send` wry handle — created and driven on the UI thread.
    webview: wry::WebView,
}

#[cfg(feature = "web")]
impl Host {
    /// Build the child webview under `frame`'s window. On Linux this
    /// is `gtk::init` first — `build_as_child` panics without it on
    /// the building thread — then the build itself inside
    /// `catch_unwind`, because wry also panics on handles it can't
    /// parent; every failure mode lands as `InitFailed` → the
    /// external surface, never a crash (us-23). The pane's `Embed`
    /// matrix has already rejected Wayland/unknown handles by now.
    fn new(frame: &eframe::Frame, rect: Option<egui::Rect>) -> Result<Self, WhyExternal> {
        #[cfg(target_os = "linux")]
        gtk::init().map_err(|_| WhyExternal::InitFailed)?;
        let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let builder = wry::WebViewBuilder::new();
            let builder = match rect {
                Some(rect) => builder.with_bounds(to_wry_rect(rect)),
                None => builder,
            };
            builder.build_as_child(frame)
        }));
        match built {
            Ok(Ok(webview)) => Ok(Self { webview }),
            Ok(Err(_)) | Err(_) => Err(WhyExternal::InitFailed),
        }
    }

    /// Load a user-navigated URL — the only fetch this host performs.
    fn load_url(&self, url: &str) {
        let _ = self.webview.load_url(url);
    }

    /// Re-fetch the current page.
    fn reload(&self) {
        let _ = self.webview.reload();
    }

    /// Reposition the child over the pane's staged body rect.
    fn set_bounds(&self, rect: egui::Rect) {
        let _ = self.webview.set_bounds(to_wry_rect(rect));
    }

    /// Show/hide — the pane-closed arm of the sync.
    fn set_visible(&self, visible: bool) {
        let _ = self.webview.set_visible(visible);
    }
}

/// The feature-off `Host` — no webview exists. `None` is the only
/// value it ever takes (the probe always fails), so the methods are
/// unreachable — they exist so the sync body compiles identically.
#[cfg(not(feature = "web"))]
struct Host;

#[cfg(not(feature = "web"))]
#[allow(dead_code)] // `host` is always `None` feature-off — see above
impl Host {
    fn load_url(&self, _url: &str) {}

    fn reload(&self) {}

    fn set_bounds(&self, _rect: egui::Rect) {}

    fn set_visible(&self, _visible: bool) {}
}

/// egui's logical rect → wry's `Rect` — `dpi` logical units, the same
/// coordinate space egui points and winit logical sizes share.
#[cfg(feature = "web")]
fn to_wry_rect(rect: egui::Rect) -> wry::Rect {
    wry::Rect {
        position: dpi::LogicalPosition::new(rect.left() as f64, rect.top() as f64).into(),
        size: dpi::LogicalSize::new(rect.width().max(1.0) as f64, rect.height().max(1.0) as f64)
            .into(),
    }
}

/// The `RawWindowHandle` → [`WindowKind`] mapping — the embed
/// matrix's input. `raw-window-handle` is a `web`-feature dep, so
/// this half of the decision lives in the adapter while the decision
/// itself stays pure in `crate::browser`.
#[cfg(feature = "web")]
impl WindowKind {
    /// Classify a raw handle — `Xcb` counts as X11 (wry parents X11
    /// children either way); every unlisted handle is `Other` and
    /// gets no embed.
    fn of(raw: raw_window_handle::RawWindowHandle) -> Self {
        match raw {
            raw_window_handle::RawWindowHandle::Xlib(_)
            | raw_window_handle::RawWindowHandle::Xcb(_) => Self::X11,
            raw_window_handle::RawWindowHandle::Wayland(_) => Self::Wayland,
            raw_window_handle::RawWindowHandle::AppKit(_) => Self::AppKit,
            raw_window_handle::RawWindowHandle::Win32(_)
            | raw_window_handle::RawWindowHandle::WinRt(_) => Self::Windows,
            _ => Self::Other,
        }
    }
}

/// Hand `url` to the system browser via the platform opener — spawned
/// detached (stdio nulled) so the pane never blocks on the child's
/// lifetime; a spawn failure toasts like every other GUI-side
/// failure rather than passing silently.
fn open_external(url: &str, toast: &mut Option<Toast>) {
    if let Err(error) = opener_command(url).spawn() {
        *toast = Some(Toast::new(
            format!("couldn't open the system browser: {error}"),
            Instant::now(),
        ));
    }
}

/// The platform opener command — `open` on macOS, `xdg-open` on the
/// freedesktop desktops, `cmd /c start` on Windows (§5.14, not yet
/// shipped); the URL is the single argument. Built here so the verb
/// and its argument shape stay unit-testable; spawning it is the
/// untestable half.
fn opener_command(url: &str) -> std::process::Command {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(url);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("cmd");
        // `start` reads its first quoted arg as a window title —
        // the empty one keeps the URL from being eaten as it.
        command.args(["/c", "start", "", url]);
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        command
    };
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
}

#[cfg(test)]
mod tests {
    //! R14-T3 at the adapter's only testable edge — the opener
    //! command's verb and argument shape per platform. The `wry`
    //! handle itself is the agreed-untested seam: it needs a live
    //! window and is covered by `docs/gui-verification.md`.

    use super::*;

    /// The opener is the platform's verb plus the URL as the sole
    /// payload argument — detached stdio so the child can't hold the
    /// terminal's pipes.
    #[test]
    fn opener_names_the_platform_verb_with_the_url_as_its_arg() {
        let command = opener_command("https://docs.rs/ait");
        #[cfg(target_os = "macos")]
        {
            assert_eq!(command.get_program(), "open");
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                ["https://docs.rs/ait"]
            );
        }
        #[cfg(target_os = "windows")]
        {
            assert_eq!(command.get_program(), "cmd");
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                ["/c", "start", "", "https://docs.rs/ait"]
            );
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            assert_eq!(command.get_program(), "xdg-open");
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                ["https://docs.rs/ait"]
            );
        }
    }
}
