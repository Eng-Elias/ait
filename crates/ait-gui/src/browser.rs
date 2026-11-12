//! The web browser pane (Spec R14 §5.13, ticket R14-T3): `Ctrl+Shift+B`
//! opens a minimal browser in the side region — an address entry,
//! back/forward, reload and an "open externally" control over a body
//! that is either a `wry` child webview (macOS, Linux/X11) or the
//! external-open surface that hands pages to the system browser
//! (Wayland, a `web`-less build, a runtime that failed to initialize).
//!
//! Everything decidable lives here in pure data: [`BrowserState`] —
//! the `Shared` field — owns the entry text, the navigation history,
//! the queue of [`NavCommand`]s the app layer drains each frame and
//! the [`Embed`] verdict the webview probe reports back. The `!Send`
//! `wry::WebView` itself is `crate::webview`'s deliberately untested
//! seam on `GuiApp`; this module never names it, so the pane, the
//! history and the fallback decision test the same in a feature-off
//! build.
//!
//! Local-first is structural (§5.13): the only strings ever queued
//! for loading are URLs the USER navigated to — `navigate`/`submit`/
//! `go_back`/`go_forward`/`open_external` are the sole producers, and
//! nothing in the app feeds terminal content into them.

use std::collections::VecDeque;

use ait_term::{Key, KeyEvent, Modifiers};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};

use crate::app::Shared;
use crate::grid::GridTheme;
use crate::pane::{Pane, PaneId};

/// What the pane's body hosts — the webview probe's verdict, written
/// back by `GuiApp` once the browser is first shown (the probe needs
/// a live frame for the window handle). `Unknown` is the pre-probe
/// frame only.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Embed {
    /// The pane has been shown but the probe hasn't reported yet —
    /// transient, one frame in practice.
    #[default]
    Unknown,
    /// A `wry` child webview is alive — the body rect is staged for it.
    Embedded,
    /// No embed — the pane shows the external-open surface and every
    /// navigation is handed to the system browser.
    External(WhyExternal),
}

/// Why the pane degrades to the external-open surface — each variant
/// carries the explanation line the surface paints, so the decision
/// and its honest reporting can never drift (§5.13 us-21/23).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// Feature-off only `FeatureOff` is ever constructed — the rest are
// the `web` probe's outcomes; the enum still compiles for `line()`.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub enum WhyExternal {
    /// `ait-gui` was built without the `web` feature — the whole
    /// webkit stack is absent at compile time.
    FeatureOff,
    /// `wry::WebViewBuilder::build_as_child` can't attach under
    /// Wayland (it needs a GTK container there; the egui window
    /// isn't one).
    Wayland,
    /// The frame reported no window handle — headless creation, or a
    /// backend that can't vend one.
    NoWindow,
    /// `gtk::init` or `build_as_child` failed at runtime — a missing
    /// or broken webview runtime is survivable, never a crash.
    InitFailed,
    /// A window-handle kind the embed matrix doesn't name.
    Unsupported,
}

impl WhyExternal {
    /// The fallback surface's first line — why there's no webview.
    pub fn line(self) -> &'static str {
        match self {
            Self::FeatureOff => "this build has no webview runtime",
            Self::Wayland => "embedded webviews aren't supported on Wayland",
            Self::NoWindow => "no window handle for a webview to attach to",
            Self::InitFailed => "the webview runtime failed to initialize",
            Self::Unsupported => "this platform can't host an embedded webview",
        }
    }
}

/// The windowing system a frame's raw window handle reports — the
/// `build_as_child` attach matrix's input. The `RawWindowHandle` →
/// `WindowKind` mapping lives in `crate::webview` (`raw-window-handle`
/// is a `web`-feature dep, so the decision itself stays dep-free and
/// identical in a feature-off build).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// The `RawWindowHandle` mapping is `web`-only — feature-off nothing
// constructs these, but the type stays so `embed_supported` (and its
// feature-off test) reads identically.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub enum WindowKind {
    /// X11 — `build_as_child` parents an X11 child window (manual
    /// `set_bounds` every frame; it won't auto-resize).
    X11,
    /// Wayland — child embedding is not supported.
    Wayland,
    /// macOS — an NSView subview of the window's content view.
    AppKit,
    /// Windows — a child window of the parent (§5.14's platform, not
    /// yet shipped — the decision is honest anyway).
    Windows,
    /// Any other handle — no embed.
    Other,
}

/// The whole fallback decision in one pure function — the `web`
/// feature's compile-time flag wins before the platform is even
/// consulted, then the attach matrix: X11/AppKit/Windows embed,
/// Wayland and unknown handles don't. `GuiApp`'s probe is the only
/// caller; anything else (init failure) is reported by the probe
/// itself, not decided here.
// Feature-off the probe never calls this — the function still
// compiles so the decision and its tests exist in both builds.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub fn embed_supported(kind: WindowKind) -> Result<(), WhyExternal> {
    if !cfg!(feature = "web") {
        return Err(WhyExternal::FeatureOff);
    }
    match kind {
        WindowKind::X11 | WindowKind::AppKit | WindowKind::Windows => Ok(()),
        WindowKind::Wayland => Err(WhyExternal::Wayland),
        WindowKind::Other => Err(WhyExternal::Unsupported),
    }
}

/// One user-initiated navigation waiting for the app layer — the
/// queue entry `BrowserState` accumulates and `GuiApp::web` drains
/// each frame into the webview or the system browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavCommand {
    /// Load this normalized URL wherever the embed verdict puts it —
    /// `WebView::load_url` embedded, the system browser external.
    Navigate(String),
    /// Re-fetch the current page — webview-only; externally the page
    /// lives in the system browser, which owns its own reload.
    Reload,
    /// Always hand this URL to the system browser — the "open
    /// externally" control, regardless of embed verdict.
    OpenExternal(String),
}

/// The browser pane's mutable state — a `Shared` field, app-global
/// and ephemeral like the pane selection itself (us-31). The model is
/// deliberately webview-free: history is ours (index into `history`,
/// never the webview's own back/forward list), the entry mirrors the
/// find bar's append/backspace input model, and navigations leave as
/// [`NavCommand`]s for the app layer to sink.
#[derive(Debug, Default)]
pub struct BrowserState {
    /// The address entry's text — appended by `Event::Text`/`Paste`,
    /// backspaced by key, overwritten by every navigation so the bar
    /// always shows the page the webview is on.
    entry: String,
    /// The `not a URL` hint flag — a `submit`/`navigate` the
    /// normalizer rejected sets it; any edit or successful
    /// navigation clears it.
    bad_entry: bool,
    /// The visited, normalized URLs — `index` names the current one.
    history: Vec<String>,
    /// The current page's slot in `history` — back/forward move it,
    /// a fresh navigation truncates everything past it (browser
    /// history rules).
    index: usize,
    /// The webview probe's verdict — see [`Embed`].
    embed: Embed,
    /// Queued navigations the app layer hasn't drained yet.
    pending: VecDeque<NavCommand>,
    /// The body rect the webview should cover this frame — staged by
    /// the pane's paint while embedded, cleared by `GuiApp` before
    /// each frame's paint; `None` whenever the pane is closed or the
    /// body isn't a webview, which is also the hide signal.
    surface: Option<Rect>,
}

impl BrowserState {
    /// The address entry's current text.
    pub fn entry(&self) -> &str {
        &self.entry
    }

    /// Whether the last submit was rejected as not-a-URL — the
    /// chrome's hint flag.
    pub fn bad_entry(&self) -> bool {
        self.bad_entry
    }

    /// The current page's normalized URL, if any navigation has run.
    pub fn current(&self) -> Option<&str> {
        self.history.get(self.index).map(String::as_str)
    }

    /// The probe's verdict — `GuiApp` writes it via [`set_embed`].
    pub fn embed(&self) -> Embed {
        self.embed
    }

    /// Record the probe's verdict — the webview adapter's one write
    /// back into the model.
    pub fn set_embed(&mut self, embed: Embed) {
        self.embed = embed;
    }

    /// This frame's staged body rect — `GuiApp` clears it before the
    /// paint pass and reads it in the post-paint webview sync.
    pub fn surface(&self) -> Option<Rect> {
        self.surface
    }

    /// Stage (or clear) the webview's target rect — `None` is the
    /// hide signal: pane closed, or body isn't embedded.
    pub fn set_surface(&mut self, rect: Option<Rect>) {
        self.surface = rect;
    }

    /// Whether back can move — the history has a page behind this one.
    pub fn can_go_back(&self) -> bool {
        self.index > 0
    }

    /// Whether forward can move — navigation truncated entries remain.
    pub fn can_go_forward(&self) -> bool {
        self.index + 1 < self.history.len()
    }

    /// Composed text or a paste into the entry — append and clear the
    /// rejection hint (the edit supersedes it).
    pub fn push_text(&mut self, text: &str) {
        self.entry.push_str(text);
        self.bad_entry = false;
    }

    /// One backspace — the entry is append-only like the find bar's
    /// query; the caret never leaves the tail.
    pub fn pop_char(&mut self) {
        self.entry.pop();
        self.bad_entry = false;
    }

    /// `Ctrl+U`/clear-the-line — empty the entry wholesale.
    pub fn clear_entry(&mut self) {
        self.entry.clear();
        self.bad_entry = false;
    }

    /// Enter on the entry — `navigate` with the typed text.
    pub fn submit(&mut self) {
        let raw = self.entry.clone();
        self.navigate(&raw);
    }

    /// Navigate to `raw` (normalized): truncate the forward half of
    /// the history, push the URL as the new current page, sync the
    /// entry to what actually loads, and queue the navigation. A
    /// rejected input sets the `not a URL` hint and queues nothing —
    /// only user-navigable URLs are ever fetched (§5.13).
    pub fn navigate(&mut self, raw: &str) {
        let Some(url) = normalize_url(raw) else {
            self.bad_entry = true;
            return;
        };
        self.bad_entry = false;
        self.history.truncate(self.index + 1);
        self.history.push(url.clone());
        self.index = self.history.len() - 1;
        self.entry = url.clone();
        self.pending.push_back(NavCommand::Navigate(url));
    }

    /// Back one page — the history index steps down and the page it
    /// lands on queues as a fresh navigation (our history drives the
    /// webview, not the webview's own back list — one code path for
    /// embedded and external alike).
    pub fn go_back(&mut self) {
        if self.can_go_back() {
            self.index -= 1;
            self.requeue_current();
        }
    }

    /// Forward one page — the symmetric step.
    pub fn go_forward(&mut self) {
        if self.can_go_forward() {
            self.index += 1;
            self.requeue_current();
        }
    }

    /// The shared back/forward tail: the entry re-syncs to the landed
    /// page and the navigation queues.
    fn requeue_current(&mut self) {
        let url = self.history[self.index].clone();
        self.entry = url.clone();
        self.pending.push_back(NavCommand::Navigate(url));
    }

    /// A webview-reported navigation — a link click or script-driven
    /// location change the PAGE initiated (the wry adapter's
    /// `with_navigation_handler` report; us-18). The webview is
    /// already loading it, so history records it without queueing
    /// another `NavCommand` — the entry and back/forward then track
    /// the page the webview is actually on instead of the last typed
    /// URL. A report naming the current page is a no-op: our own
    /// `load_url`s report back through the same hook on every
    /// platform, and a page's same-URL refresh must not double-count.
    /// The URL lands verbatim — the webview already fetched it, so
    /// the `normalize_url` gate (which exists to keep unfetchable
    /// text out of the fetch path) doesn't apply.
    pub fn webview_navigated(&mut self, url: &str) {
        let url = url.trim();
        if url.is_empty() || self.current() == Some(url) {
            return;
        }
        self.bad_entry = false;
        self.history.truncate(self.index + 1);
        self.history.push(url.to_owned());
        self.index = self.history.len() - 1;
        self.entry = url.to_owned();
    }

    /// Re-fetch the current page — a no-op with no page loaded.
    pub fn reload(&mut self) {
        if self.current().is_some() {
            self.pending.push_back(NavCommand::Reload);
        }
    }

    /// The "open externally" control — hands the current page (or a
    /// typed-but-unsubmitted URL) to the system browser, whichever
    /// verdict the pane runs under.
    pub fn open_external(&mut self) {
        let url = self
            .current()
            .map(str::to_owned)
            .or_else(|| normalize_url(&self.entry));
        if let Some(url) = url {
            self.pending.push_back(NavCommand::OpenExternal(url));
        }
    }

    /// Drain the queued navigations into sink-ready commands — the
    /// fallback decision's second half: under `Embedded` every
    /// command drains verbatim, while anything else (an `External`
    /// verdict, or a navigation queued before the probe reported)
    /// turns `Navigate` into `OpenExternal` and drops `Reload` — the
    /// system browser owns its page's refresh.
    pub fn drain_nav(&mut self) -> Vec<NavCommand> {
        let embedded = self.embed == Embed::Embedded;
        self.pending
            .drain(..)
            .filter_map(|command| match command {
                NavCommand::Navigate(url) if !embedded => Some(NavCommand::OpenExternal(url)),
                NavCommand::Reload if !embedded => None,
                command => Some(command),
            })
            .collect()
    }
}

/// `raw` → a navigable URL: an explicit `http(s)://`/`file://` URL
/// passes through verbatim; host-looking text — a dotted domain,
/// `localhost`, an IPv4 literal, each optionally carrying
/// port/path/query — gains `https://`. Anything else (spaces, bare
/// words, unknown schemes) is `None` rather than a guess at some
/// other fetch the user didn't name (local-first, §5.13).
pub fn normalize_url(raw: &str) -> Option<String> {
    let text = raw.trim();
    if text.is_empty() || text.chars().any(char::is_whitespace) {
        return None;
    }
    if let Some((scheme, rest)) = text.split_once("://") {
        return (matches!(scheme, "http" | "https" | "file") && !rest.is_empty())
            .then(|| text.to_string());
    }
    // Schemeless — host-looking only. Strip userinfo, then read the
    // host up to the first port/path boundary.
    let head = text.split(['/', '?', '#']).next().unwrap_or("");
    let host = head.rsplit('@').next().unwrap_or("");
    let host = host.split(':').next().unwrap_or("");
    let hostlike = host == "localhost"
        || host.parse::<std::net::Ipv4Addr>().is_ok()
        || (host.contains('.') && !host.starts_with('.') && !host.ends_with('.'));
    hostlike.then(|| format!("https://{text}"))
}

/// The `Ctrl+Shift+B` registry entry — a stateless `&'static` handle
/// like every pane: the mutable model is [`Shared::browser`], the
/// webview handle is `GuiApp`'s. Paint draws the chrome row and the
/// body surface; input edits the entry or drives history.
pub struct BrowserPane;

/// The [`PaneId::Browser`] registry entry — replaces T1's placeholder.
pub static BROWSER: BrowserPane = BrowserPane;

/// The chrome row's height — fixed like the host's switcher strip so
/// zoom never reflows it.
const CHROME_H: f32 = 26.0;

impl Pane for BrowserPane {
    fn id(&self) -> PaneId {
        PaneId::Browser
    }

    fn title(&self) -> &'static str {
        "browser"
    }

    fn chord(&self) -> egui::Key {
        egui::Key::B
    }

    /// Unbound keys while the pane shows: Enter submits the entry,
    /// Backspace/`Ctrl+U` edit it, `Alt+←`/`Alt+→` walk history,
    /// `Ctrl+R` reloads — the rest is swallowed (the host owns Esc
    /// and the toggle chord itself).
    fn key(&self, shared: &mut Shared, key: KeyEvent) {
        let browser = &mut shared.browser;
        match (key.key, key.modifiers) {
            (Key::Enter, m) if m.is_empty() => browser.submit(),
            (Key::Backspace, m) if m.is_empty() => browser.pop_char(),
            (Key::Char('u'), m) if m == Modifiers::CONTROL => browser.clear_entry(),
            (Key::Char('r'), m) if m == Modifiers::CONTROL => browser.reload(),
            (Key::Left, m) if m == Modifiers::ALT => browser.go_back(),
            (Key::Right, m) if m == Modifiers::ALT => browser.go_forward(),
            _ => {}
        }
    }

    /// Composed text and pastes — the entry's append path.
    fn text(&self, shared: &mut Shared, text: &str) {
        shared.browser.push_text(text);
    }

    /// The chrome row — `‹`/`›` history, `⟳` reload, the address
    /// entry, `↗` open-externally — then the body: the staged webview
    /// rect while embedded, else the external-open surface. Clicks
    /// apply straight to `shared.browser` — the responses borrow `ui`
    /// only, so no action queue is needed (unlike the panel's, the
    /// targets here are already `Shared`'s).
    fn paint(&self, ui: &mut egui::Ui, shared: &mut Shared, rect: Rect, theme: GridTheme) {
        let ctx = ui.ctx().clone();
        let painter = ui.painter().with_clip_rect(rect);
        let font = FontId::monospace(13.0);
        let dim = Color32::from_rgb(0x77, 0x77, 0x77);
        let idle_fg = Color32::from_rgb(0xAA, 0xAA, 0xAA);

        // The chrome row: nav buttons left, entry mid, external right.
        let bar = Rect::from_min_size(rect.left_top(), Vec2::new(rect.width(), CHROME_H));
        painter.line_segment(
            [bar.left_bottom(), bar.right_bottom()],
            Stroke::new(1.0, Color32::from_rgb(0x33, 0x33, 0x33)),
        );
        // One chrome button at `x`: paints and hit-tests; `enabled`
        // dims and disarms it (the no-history back arrow). Returns
        // (clicked, width to advance).
        let button = |x: f32,
                      label: &'static str,
                      enabled: bool,
                      tip: &'static str,
                      ui: &mut egui::Ui|
         -> (bool, f32) {
            let width = painter
                .layout_no_wrap(label.to_string(), font.clone(), Color32::WHITE)
                .size()
                .x
                + 14.0;
            let hit_rect = Rect::from_min_size(
                Pos2::new(x, bar.top() + 3.0),
                Vec2::new(width, bar.height() - 6.0),
            );
            let hit = ui.interact(
                hit_rect,
                egui::Id::unique(("browser-btn", label)),
                Sense::click(),
            );
            let (bg, fg) = if !enabled {
                (Color32::TRANSPARENT, dim)
            } else if hit.hovered() {
                (Color32::from_rgb(0x2A, 0x2A, 0x2A), theme.fg)
            } else {
                (Color32::TRANSPARENT, idle_fg)
            };
            painter.rect_filled(hit_rect, 3.0, bg);
            painter.text(
                hit_rect.center(),
                Align2::CENTER_CENTER,
                label,
                font.clone(),
                fg,
            );
            if enabled && hit.hovered() {
                hit.clone().on_hover_text(tip);
            }
            (enabled && hit.clicked(), width + 4.0)
        };

        let browser = &mut shared.browser;
        let mut x = bar.left() + 6.0;
        let (back, advance) = button(x, "‹", browser.can_go_back(), "back (Alt+←)", ui);
        x += advance;
        let (forward, advance) = button(x, "›", browser.can_go_forward(), "forward (Alt+→)", ui);
        x += advance;
        let (reload, advance) = button(x, "⟳", browser.current().is_some(), "reload (Ctrl+R)", ui);
        x += advance + 4.0;

        // The external-open button right-anchors so the entry takes
        // the middle slack.
        let ext_label = "ext ↗";
        let ext_w = painter
            .layout_no_wrap(ext_label.to_string(), font.clone(), Color32::WHITE)
            .size()
            .x
            + 14.0;
        let (external, _) = button(
            bar.right() - ext_w - 4.0,
            ext_label,
            browser.current().is_some() || !browser.entry().is_empty(),
            "open in the system browser",
            ui,
        );

        // The address entry — the find bar's model: appended text,
        // end-anchored caret; a rejected submit turns the text amber
        // as the `not a URL` hint.
        let entry_rect = Rect::from_min_max(
            Pos2::new(x, bar.top() + 3.0),
            Pos2::new(bar.right() - ext_w - 10.0, bar.bottom() - 3.0),
        );
        painter.rect_filled(entry_rect, 3.0, Color32::from_rgb(0x26, 0x26, 0x26));
        let cell_w = ctx
            .fonts_mut(|fonts| fonts.glyph_width(&font, 'M'))
            .max(1.0);
        let room = ((entry_rect.width() - 12.0) / cell_w).floor().max(1.0) as usize;
        let entry = browser.entry();
        let (shown, fg) = if entry.is_empty() {
            ("type a URL — Enter goes".to_string(), dim)
        } else {
            (
                crate::panel::tail(entry, room).to_string(),
                if browser.bad_entry() {
                    Color32::from_rgb(0xE0, 0xC0, 0x40)
                } else {
                    theme.fg
                },
            )
        };
        let galley_x = entry_rect.left() + 6.0;
        painter.text(
            Pos2::new(galley_x, entry_rect.center().y),
            Align2::LEFT_CENTER,
            &shown,
            font.clone(),
            fg,
        );
        if !entry.is_empty() {
            let galley = painter.layout_no_wrap(shown, font.clone(), fg);
            let caret_x = galley_x + galley.size().x + 2.0;
            painter.line_segment(
                [
                    Pos2::new(caret_x, entry_rect.top() + 2.0),
                    Pos2::new(caret_x, entry_rect.bottom() - 2.0),
                ],
                Stroke::new(1.5, theme.fg),
            );
        }

        if back {
            browser.go_back();
        }
        if forward {
            browser.go_forward();
        }
        if reload {
            browser.reload();
        }
        if external {
            browser.open_external();
        }

        // The body: the webview's staging rect while embedded, the
        // external-open surface otherwise.
        let body = Rect::from_min_max(Pos2::new(rect.left(), bar.bottom()), rect.right_bottom());
        match browser.embed() {
            Embed::Embedded => {
                // The native child covers this rect — stage it so
                // `GuiApp`'s post-paint sync repositions the webview
                // over `body` (Linux/X11 won't auto-resize). The fill
                // keeps any one-frame gap dark rather than themed.
                painter.rect_filled(body, 0.0, Color32::from_rgb(0x11, 0x11, 0x11));
                browser.set_surface(Some(body));
            }
            Embed::Unknown => {
                painter.text(
                    body.center(),
                    Align2::CENTER_CENTER,
                    "checking webview support…",
                    font,
                    dim,
                );
            }
            Embed::External(why) => {
                let line_h = ctx.fonts_mut(|fonts| fonts.row_height(&font)).max(1.0);
                let lines = 4;
                let mut y = body.center().y - (lines as f32 * line_h) / 2.0;
                for (text, color) in [
                    (why.line(), Color32::from_rgb(0xE0, 0xC0, 0x40)),
                    (
                        "pages open in your system browser — type a URL and press Enter",
                        dim,
                    ),
                    (browser.current().unwrap_or(""), theme.fg),
                    ("\"ext ↗\" promotes the page any time", dim),
                ] {
                    painter.text(
                        Pos2::new(body.center().x, y),
                        Align2::CENTER_CENTER,
                        text,
                        font.clone(),
                        color,
                    );
                    y += line_h;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! R14-T3 at the pane's pure seam: the model ([`BrowserState`]),
    //! the URL normalizer and the fallback decision — every rule the
    //! ticket names testable ("URL normalization, nav state/history,
    //! fallback decision") sits here; the wry handle itself is the
    //! adapter's untested edge.

    use super::*;

    /// `normalize_url` — the address bar's one parser: real schemes
    /// pass, host-looking text gains https, everything else is not a
    /// URL (and therefore never fetched).
    #[test]
    fn normalizes_only_navigable_urls() {
        for (raw, expected) in [
            ("https://a.b/c", Some("https://a.b/c")),
            ("  http://x.y  ", Some("http://x.y")),
            ("file:///tmp/x", Some("file:///tmp/x")),
            ("example.com", Some("https://example.com")),
            (
                "example.com:8080/p?q=1",
                Some("https://example.com:8080/p?q=1"),
            ),
            ("localhost:3000", Some("https://localhost:3000")),
            ("127.0.0.1:8080", Some("https://127.0.0.1:8080")),
            ("user@example.com", Some("https://user@example.com")),
            // Not URLs — never navigable.
            ("", None),
            ("   ", None),
            ("not a url", None),
            ("ftp://files", None),
            ("javascript:alert(1)", None),
            ("http://", None),
            (".hidden", None),
            ("single-word", None),
        ] {
            assert_eq!(normalize_url(raw).as_deref(), expected, "{raw:?}");
        }
    }

    /// The fallback decision (feature-on): the embed matrix admits
    /// X11/AppKit/Windows and rejects Wayland + unknown handles — the
    /// compile-time `web` flag is the first gate, so a feature-off
    /// build reports `FeatureOff` whatever the platform.
    #[cfg(feature = "web")]
    #[test]
    fn embed_matrix_admits_child_capable_handles() {
        for kind in [WindowKind::X11, WindowKind::AppKit, WindowKind::Windows] {
            assert_eq!(embed_supported(kind), Ok(()), "{kind:?}");
        }
        assert_eq!(
            embed_supported(WindowKind::Wayland),
            Err(WhyExternal::Wayland)
        );
        assert_eq!(
            embed_supported(WindowKind::Other),
            Err(WhyExternal::Unsupported)
        );
    }

    /// The feature-off build's whole decision — the flag beats the
    /// platform, so even an X11 window gets the external surface.
    #[cfg(not(feature = "web"))]
    #[test]
    fn feature_off_reports_feature_off() {
        assert_eq!(
            embed_supported(WindowKind::X11),
            Err(WhyExternal::FeatureOff)
        );
    }

    /// Navigate pushes history, syncs the entry to the loaded page
    /// and queues the navigation — a rejected input sets the hint
    /// flag and queues nothing (nothing unfetchable is fetched).
    #[test]
    fn navigate_pushes_history_and_queues_the_load() {
        let mut browser = BrowserState::default();
        browser.navigate("example.com");
        assert_eq!(browser.current(), Some("https://example.com"));
        assert_eq!(browser.entry(), "https://example.com");
        assert!(!browser.bad_entry());
        assert!(!browser.can_go_back() && !browser.can_go_forward());
        assert_eq!(
            browser.drain_nav(),
            vec![NavCommand::OpenExternal("https://example.com".into())],
            "embed Unknown drains to the external sink"
        );

        browser.navigate("  not a url  ");
        assert!(browser.bad_entry(), "the rejection flag is the hint");
        assert_eq!(browser.current(), Some("https://example.com"));
        assert!(browser.drain_nav().is_empty(), "nothing was queued");
    }

    /// Back/forward walk the index and re-queue the landed page; a
    /// fresh navigate truncates the forward half — browser history
    /// rules, driven by ours (never the webview's own list).
    #[test]
    fn history_walks_and_truncates_like_a_browser() {
        let mut browser = BrowserState::default();
        for url in ["https://a.dev", "https://b.dev", "https://c.dev"] {
            browser.navigate(url);
        }
        let _ = browser.drain_nav();

        browser.go_back();
        browser.go_back();
        assert_eq!(browser.current(), Some("https://a.dev"));
        assert!(!browser.can_go_back());
        assert!(browser.can_go_forward());
        assert_eq!(browser.entry(), "https://a.dev");

        browser.go_forward();
        assert_eq!(browser.current(), Some("https://b.dev"));
        // The queued commands replay the walk — b, a, then b again.
        assert_eq!(
            browser.drain_nav(),
            vec![
                NavCommand::OpenExternal("https://b.dev".into()),
                NavCommand::OpenExternal("https://a.dev".into()),
                NavCommand::OpenExternal("https://b.dev".into()),
            ]
        );

        // A fresh navigation truncates the forward half — c is gone.
        browser.navigate("https://d.dev");
        assert!(!browser.can_go_forward());
        assert_eq!(browser.current(), Some("https://d.dev"));
        browser.go_back();
        assert_eq!(browser.current(), Some("https://b.dev"));
        assert!(browser.can_go_forward());
    }

    /// Reload queues only with a page loaded; the external-open
    /// control hands over the current page (or a typed-but-
    /// unsubmitted URL), whatever the embed verdict.
    #[test]
    fn reload_and_external_queue_the_right_commands() {
        let mut browser = BrowserState::default();
        browser.set_embed(Embed::Embedded);

        browser.reload();
        assert!(browser.drain_nav().is_empty(), "no page, no reload");
        browser.open_external();
        assert!(browser.drain_nav().is_empty(), "no page, nothing to open");

        browser.navigate("https://docs.rs");
        browser.reload();
        browser.open_external();
        assert_eq!(
            browser.drain_nav(),
            vec![
                NavCommand::Navigate("https://docs.rs".into()),
                NavCommand::Reload,
                NavCommand::OpenExternal("https://docs.rs".into()),
            ],
            "embedded: verbatim commands in queue order"
        );

        // A typed-but-unsubmitted entry is the external-open target.
        let mut browser = BrowserState::default();
        browser.set_embed(Embed::External(WhyExternal::Wayland));
        browser.push_text("example.com");
        browser.open_external();
        assert_eq!(
            browser.drain_nav(),
            vec![NavCommand::OpenExternal("https://example.com".into())]
        );
    }

    /// The sink mapping — the fallback decision's second half:
    /// `Navigate` drains verbatim only while embedded; under any
    /// `External` verdict (or the pre-probe `Unknown`) it becomes
    /// `OpenExternal`, and `Reload` drops — the system browser owns
    /// its page's refresh.
    #[test]
    fn drain_maps_navigations_to_the_embed_verdict() {
        let mut browser = BrowserState::default();
        browser.navigate("https://a.dev");
        browser.reload();
        assert_eq!(
            browser.drain_nav(),
            vec![NavCommand::OpenExternal("https://a.dev".into())],
            "Unknown verdict: navigate→external, reload dropped"
        );

        browser.set_embed(Embed::Embedded);
        browser.navigate("https://b.dev");
        browser.reload();
        assert_eq!(
            browser.drain_nav(),
            vec![
                NavCommand::Navigate("https://b.dev".into()),
                NavCommand::Reload,
            ]
        );

        browser.set_embed(Embed::External(WhyExternal::InitFailed));
        browser.navigate("https://c.dev");
        browser.reload();
        browser.open_external();
        assert_eq!(
            browser.drain_nav(),
            vec![
                NavCommand::OpenExternal("https://c.dev".into()),
                NavCommand::OpenExternal("https://c.dev".into()),
            ],
            "External verdict: navigate→external, reload dropped"
        );
    }

    /// us-18: a webview-reported navigation lands as a real history
    /// entry — entry synced, forward half truncated — while queueing
    /// NO load of its own (the webview is already there). Our own
    /// queued navigations reporting back dedupe against the current
    /// page, and an empty report is ignored outright.
    #[test]
    fn webview_navigations_sync_entry_and_history_without_queueing() {
        let mut browser = BrowserState::default();
        browser.set_embed(Embed::Embedded);
        browser.navigate("https://a.dev");
        let _ = browser.drain_nav();

        // Our own `load_url` reporting back dedupes — no second
        // history entry, no new command.
        browser.webview_navigated("https://a.dev");
        assert_eq!(browser.current(), Some("https://a.dev"));
        assert!(browser.drain_nav().is_empty());
        browser.webview_navigated("  ");
        assert_eq!(browser.current(), Some("https://a.dev"));

        // A link click inside the page is a fresh history entry.
        browser.webview_navigated("https://a.dev/docs");
        assert_eq!(browser.current(), Some("https://a.dev/docs"));
        assert_eq!(browser.entry(), "https://a.dev/docs");
        assert!(browser.can_go_back());
        assert!(
            browser.drain_nav().is_empty(),
            "no re-fetch queued — the webview already navigated"
        );

        // Back still replays the pre-click page; a reported
        // navigation mid-history truncates the forward half.
        browser.go_back();
        assert_eq!(browser.current(), Some("https://a.dev"));
        let _ = browser.drain_nav();
        browser.webview_navigated("https://else.dev");
        assert!(!browser.can_go_forward());
        assert_eq!(browser.current(), Some("https://else.dev"));
    }

    /// The entry's edit surface: text appends, Backspace pops,
    /// `Ctrl+U` clears — and any edit clears the not-a-URL hint.
    #[test]
    fn entry_edits_clear_the_rejection_hint() {
        let mut browser = BrowserState::default();
        browser.push_text("nope nope");
        browser.submit();
        assert!(browser.bad_entry());
        browser.pop_char();
        assert!(!browser.bad_entry());
        browser.push_text(" also bad");
        browser.submit();
        assert!(browser.bad_entry());
        browser.clear_entry();
        assert_eq!(browser.entry(), "");
        assert!(!browser.bad_entry());
    }
}
