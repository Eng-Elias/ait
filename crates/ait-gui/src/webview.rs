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
//! opener. While a live webview has a staged surface the sync also
//! schedules a bounded next-frame repaint — the X11 pump only
//! advances while egui paints, and a static-looking page can still
//! animate — but with no host or no staged surface nothing is
//! scheduled and the idle window still paints zero frames.

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
    /// is staged), pump the glib context the X11 webview lives on,
    /// and — while a live host has a staged surface — schedule the
    /// bounded repaint that keeps the pump fed. Cheap when idle:
    /// `None` surfaces, empty queues and a `feature = "web"`-less
    /// build all fall through, and nothing schedules a frame.
    pub fn sync(
        &mut self,
        ctx: &egui::Context,
        frame: &eframe::Frame,
        shared: &mut Shared,
        browser_shown: bool,
    ) {
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

        // The reverse direction — navigations the WEBVIEW initiated
        // (a link click inside the page) land on `BrowserState` so
        // the address entry and history track the page it's actually
        // on (us-18). Reports ride the frame's cadence: on X11 the
        // handler fires inside `pump_glib` below and lands this same
        // sync; elsewhere it surfaces next frame at the latest.
        if let Some(host) = &self.host {
            for url in host.drain_navigations() {
                shared.browser.webview_navigated(&url);
            }
        }

        // Bounds + visibility — `set_bounds` every frame is the X11
        // contract ("the webview will not auto-resize"); macOS and
        // Windows take the same call for free (WebView2's controller
        // bounds track the parent through it). A missing rect is the
        // pane's hide signal: pane closed, or the body isn't a
        // webview.
        if let Some(host) = &self.host {
            match shared.browser.surface() {
                Some(rect) => {
                    host.set_bounds(rect);
                    host.set_visible(true);
                }
                None => host.set_visible(false),
            }
        }

        // Keep frames flowing while a live webview has a staged
        // surface — the glib pump below (and wry's own callbacks on
        // every platform) only advance while egui repaints, and a
        // static-looking page can still run video or JS. A bounded
        // `request_repaint_after` (~30 fps) instead of a raw
        // `request_repaint` keeps an idle-but-visible webview from
        // pinning a core; with no host or no staged surface — pane
        // closed — nothing is scheduled and the window still paints
        // zero frames at idle.
        if self.host.is_some() && shared.browser.surface().is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }

        // The glib pump is only safe — and only needed — while a
        // webview lives: `gtk::events_pending`/`main_iteration_do`
        // assert `gtk::init` already ran, which happens inside
        // `Host::new`. Pumping before a successful probe panics on
        // Linux (R14-T5 manual-pass sighting: process died on the
        // first frame before any pane was ever opened).
        if self.host.is_some() {
            self.pump_glib();
        }
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
    /// starve the event loop (unpumped, the webview wedges). NOT safe
    /// before `gtk::init` — `events_pending` asserts gtk is
    /// initialized and panics, so `sync` calls this only while a live
    /// host proves init succeeded.
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
    /// Webview-initiated navigations the navigation handler queued —
    /// link clicks the pane's own `load_url` didn't start (plus our
    /// own loads reporting back, which `BrowserState` dedupes). The
    /// handler can't touch `Shared` — on Linux it fires inside the
    /// glib pump, which runs under the UI's lock — so it reports
    /// through this channel and `sync` folds the URLs into the
    /// model.
    nav_rx: std::sync::mpsc::Receiver<String>,
}

#[cfg(feature = "web")]
impl Host {
    /// Build the child webview under `frame`'s window. On Linux this
    /// is `gtk::init` first — `build_as_child` panics without it on
    /// the building thread — then the build itself inside
    /// `catch_unwind`, because wry also panics on handles it can't
    /// parent; every failure mode lands as `InitFailed` → the
    /// external surface, never a crash (us-23). On Windows wry loads
    /// WebView2 itself at this point — a missing runtime surfaces as
    /// the same `InitFailed` (§5.14/R16-T7: unlike Linux's hard
    /// `DT_NEEDED` on webkit, WebView2's absence is a runtime error,
    /// not a loader crash). The pane's `Embed` matrix has already
    /// rejected Wayland/unknown handles by now.
    fn new(frame: &eframe::Frame, rect: Option<egui::Rect>) -> Result<Self, WhyExternal> {
        #[cfg(target_os = "linux")]
        gtk::init().map_err(|_| WhyExternal::InitFailed)?;
        let (nav_tx, nav_rx) = std::sync::mpsc::channel();
        let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Webview-initiated navigations — a link click, a script's
            // location change — queue for `BrowserState` so the
            // address entry and history track the page the webview is
            // actually on (us-18). `true` always allows: the handler
            // observes, never gates — our own `load_url`s report
            // through the same hook, and `webview_navigated` dedupes
            // them. Honest residue: WebKitGTK/WKWebView fire this only
            // for navigation ACTIONS — a mid-load redirect hop doesn't
            // report, so the entry can lag a hop behind until the next
            // navigation (WebView2 reports redirects too).
            let builder = wry::WebViewBuilder::new().with_navigation_handler(move |url| {
                let _ = nav_tx.send(url);
                true
            });
            let builder = match rect {
                Some(rect) => builder.with_bounds(to_wry_rect(rect)),
                None => builder,
            };
            builder.build_as_child(frame)
        }));
        match built {
            Ok(Ok(webview)) => Ok(Self { webview, nav_rx }),
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

    /// Webview-initiated navigations since the last drain — the
    /// navigation handler's queued reports.
    fn drain_navigations(&self) -> Vec<String> {
        let mut urls = Vec::new();
        while let Ok(url) = self.nav_rx.try_recv() {
            urls.push(url);
        }
        urls
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

    fn drain_navigations(&self) -> Vec<String> {
        Vec::new()
    }

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

/// Hand `url` to the system browser via the shared
/// [`crate::platform_opener`] — spawned detached (stdio nulled) so
/// the pane never blocks on the child's lifetime, and reaped on a
/// helper thread since a spawned-and-dropped child stays a zombie
/// until it exits; a spawn failure toasts like every other GUI-side
/// failure rather than passing silently.
fn open_external(url: &str, toast: &mut Option<Toast>) {
    match crate::platform_opener(std::ffi::OsStr::new(url)).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(error) => {
            *toast = Some(Toast::new(
                format!("couldn't open the system browser: {error}"),
                Instant::now(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    //! R14-T3 at the adapter's only testable edge — the shared
    //! opener command's verb and argument shape per platform. The
    //! `wry` handle itself is the agreed-untested seam: it needs a
    //! live window and is covered by `docs/gui-verification.md`.

    /// The shared opener is the platform's verb plus the URL as the
    /// sole payload argument — detached stdio so the child can't hold
    /// the terminal's pipes.
    #[test]
    fn opener_names_the_platform_verb_with_the_url_as_its_arg() {
        let command = crate::platform_opener(std::ffi::OsStr::new("https://docs.rs/ait"));
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
