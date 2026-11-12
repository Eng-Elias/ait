# 18. Browser pane: embedded system webview with a runtime fallback

Date: 2026-11-13

## Status

Accepted

## Context

REQUIREMENTS.md §5.13 left the browser pane's toolkit **Proposed**:
"a system-webview embedding (e.g. `wry`/`tao`), decided in the
round's grilling and recorded as an ADR; opening links in the
external browser is the fallback if embedding is rejected." The
round had to settle it against five constraints:

1. **The release shape (§5.11, ADR-0015).** `ait-gui` ships as a
   per-target binary on two glibc Linux and two macOS legs. Whatever
   renders web content cannot add a vendored engine to that shape —
   a bundled Chromium dwarfs the rest of the artifact combined.
2. **The platform matrix.** `wry`'s `build_as_child` embeds a child
   webview on macOS (NSView subview) and Linux/X11 (X11 child
   window) — but explicitly not on Wayland, where embedding needs a
   GTK container the winit/egui window is not. Any honest decision
   is a matrix, not a boolean.
3. **§6 perf.** The webview shares the window's event loop and UI
   thread; a stalled webview wedges repaint and input, and wry has
   no off-thread mode. The embed must add no blocking calls to the
   frame path.
4. **Local-first (§5.13).** The webview fetches only pages the user
   navigates to; terminal content never reaches the webview host.
5. **ADR-0002 / ADR-0016.** The dep must be ≥7 days old, sit behind
   a cargo feature, and never enter `ait`'s tree.

Candidates: a system webview (`wry`); CEF (vendored Chromium);
Blitz (the pure-Rust engine); external-browser-only.

This ADR also answers the question ADR-0017 leaves open: it
rejected a system webview *as the GUI shell* — rendering the
`ait-term` grid through a browser engine puts local-first review
burden on the engine for zero feature gain. Rendering *web pages* —
the pane's actual payload — is the one thing a webview is for.

## Decision

- **`wry` 0.57.2, a child webview over the pane's body rect.** The
  browser pane calls `wry::WebViewBuilder::build_as_child` on the
  eframe window's `RawWindowHandle` and repositions the child over
  the rect the pane stages each frame (`set_bounds` — the X11
  contract, where child webviews do not auto-resize; macOS takes
  the same call as an NSView placement). `>= 0.55` pins past the
  macOS-26 `build_as_child` ObjC crash (wry#1705).
- **The embed decision is a runtime matrix, not a compile-time
  one.** The frame's raw handle picks the arm — `Xlib`/`Xcb` and
  `AppKit` embed; `Wayland` and anything unlisted get the
  external-open surface (wry can't attach a child under Wayland).
  On Linux the adapter runs `gtk::init` on the UI thread and pumps
  the glib main context per frame — wry's documented idiom — and
  wraps the build in `catch_unwind`, so a failed or panicking
  `build_as_child` degrades to the same surface instead of crashing
  (us-23).
- **`wry` sits behind `ait-gui`'s `web` cargo feature, default
  on.** `--no-default-features` drops wry, gtk, dpi and
  raw-window-handle with it — the build needs no webkit2gtk/gtk
  system packages and the pane reports `this build has no webview
  runtime` on the external surface. Nothing reaches `ait`: the
  §5.11 lean-TUI evidence check (`cargo tree -p ait`) shows zero
  GUI/webview crates.
- **The fallback is the product, not an error path.** The
  external-open surface keeps the whole chrome — address entry,
  back/forward over our own history, reload, "open externally" —
  and hands every navigation to the platform opener (`open` /
  `xdg-open`), spawned detached. Where the pane can't embed, it is
  still the browser pane.
- **Recorded deviation (R14-T3): a *missing* webkit runtime is a
  process-start failure, not an init failure.** `webkit2gtk-sys`
  links `libwebkit2gtk-4.1.so.0` as a hard `DT_NEEDED` — it does
  not `dlopen`. On a webkit-less Linux the `web`-feature binary
  dies in the ELF loader before `main`; the `InitFailed` verdict
  covers only a runtime that *loads* but fails (`gtk::init` error,
  `build_as_child` error or panic). The `web` feature is the real
  mitigation for webkit-less environments — build
  `--no-default-features`. `docs/gui-verification.md`'s
  missing-runtime checklist item reflects this.
- **Rejected.** **CEF** vendors a whole Chromium — multi-hundred-MB
  artifacts, a second release pipeline, and sandboxing work, all
  breaking the §5.11 shape — to render pages the OS already has an
  engine for. **Blitz** has no JavaScript and upstream
  self-describes as not ready for embedding; docs sites are JS
  first-class. **External-only** (no embed) stays the fallback
  surface — it is what Wayland, feature-off and failed-init builds
  run — but on X11 and macOS the embed is one adapter file's worth
  of code for the core us-20 story (docs beside the terminal), and
  rejecting it would ship the degraded experience to the two
  platforms that can do better.

## Consequences

- §5.13's Toolkit line flips `Proposed`→`Decided` in the same PR
  (ADR-wins rule, ADR-0003 precedent).
- `wry`, `gtk` (Linux-only), `dpi` and `raw-window-handle` are
  `ait-gui`-only optional deps with ADR-0002 version comments;
  `cargo build -p ait` is untouched.
- The `wry::WebView` is `!Send` and lives on `GuiApp` (frontend-
  local), never on `Shared` — the pump thread can't touch it; the
  deliberately-untested seam is the adapter, and everything
  decidable (the embed matrix, navigation queue, history) is pure
  in `browser.rs`.
- The pump/repaint model is unregressed: the per-frame sync only
  drains a queue and calls `set_bounds`; an idle window still
  paints zero frames. A genuinely stalled webview would wedge the
  UI thread — accepted, manual-verified via the checklist's
  idle-frame/webview-stall item; there is no off-thread
  alternative in wry.
- Wayland users get the external-open surface — honest degradation,
  and the pane's features all still work. Windows enters the embed
  matrix's `Ok` arm already but ships with §5.14, not before.
- Webkit-less Linux systems need `--no-default-features` builds
  (the `DT_NEEDED` deviation above) — release packaging for the
  glibc legs carries the webkit2gtk dependency; a webkit-less
  variant is a build flag away, not a code change.
