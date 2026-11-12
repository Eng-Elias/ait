# 17. GUI toolkit: eframe/egui on wgpu, with a grid-only paint-callback escape hatch

Date: 2026-11-12

## Status

Accepted

## Context

REQUIREMENTS.md §5.12 left the GUI toolkit **Proposed**: "a
GPU-accelerated Rust stack rendering the `ait-term` grid directly
(e.g. `wgpu`-based); `egui` or a webview shell are the fallbacks." The
round's grilling had to settle it against four constraints:

1. **§6 perf.** The grid pass must hold ≥30 fps under `cat`-flood
   output — the one hard number a toolkit choice can miss.
2. **Real chrome, cheap.** Tab bar, find bar, agent panel, permission
   modals, ask cards, status line — a toolkit that makes us build
   widgets turns Round 13 into a widget-toolkit round.
3. **The §5.11 target set.** Native windows on x11/wayland glibc Linux
   and both macOS arches, cross-buildable under dist's runners.
4. **ADR-0002.** The pinned release must be ≥7 days old; and the dep
   must never enter `ait`'s tree (ADR-0016).

Candidates: raw `wgpu` + a self-built UI; `egui` (on `egui-wgpu` or
`glow`); a system webview (`wry`/`tao`); `iced`; `slint`.

## Decision

- **`eframe`/`egui` 0.37 on the wgpu backend.** `eframe` is egui's
  native-shell crate: it owns the winit event loop and swaps in
  `egui-wgpu` as the renderer (`eframe::Renderer::Wgpu`), covering
  Metal, Vulkan/GLES and future DX12 through one backend — the whole
  §5.11 GUI target set with no per-OS graphics code. The manifest
  trims to `wgpu`, `x11`, `wayland`, `default_fonts`,
  `system_fonts`, `clipboard`, `accesskit` on
  `default-features = false`.
- **Chrome is stock egui; the grid is a `Painter` pass.** Tabs, the
  find bar, the agent panel and modals use ordinary egui widgets.
  The terminal grid is *not* a widget: a pure cell→styled-run planner
  (`grid.rs`, unit-tested headless) feeds `egui::Painter` glyph runs
  clipped to the grid pane, and repaints are driven by `ait-term`'s
  `damage()` — cursor-only damage doesn't even repaint. Pump threads
  `try_recv` each event source on a short sleep and call
  `egui::Context::request_repaint()` on activity (the `headless.rs`
  drain precedent), so the window idles at zero frames.
- **The escape hatch is recorded, not hypothetical.** If profiling
  misses the §6 bar, the grid pass alone swaps to a custom
  `egui_wgpu` paint callback — an `egui::PaintCallback` rendering the
  grid as textured glyph quads inside the same egui frame. Chrome
  stays egui; the swap is contained to `grid.rs`/`paint.rs` and does
  not reopen this ADR. What *would* reopen it is a chrome/toolkit
  replacement, not a grid-perf fix.
- **Rejected.** Raw `wgpu` + self-built UI buys nothing the paint
  callback can't buy later, at the cost of a widget toolkit and the
  accesskit a11y tree egui ships for free. A system webview renders
  text non-natively, drags a per-OS webview dependency into the
  static-ish release shape and puts local-first review burden on a
  browser engine for zero feature gain. `glow` was the runner-up
  backend — wgpu covers more of the target set with one code path.
  `iced`/`slint` offer no advantage over egui's immediate-mode fit
  for a pull-model session core.

## Consequences

- egui's immediate mode matches the session core's pull API: no
  retained widget tree to keep in sync with `TabManager`, and the
  repaint-on-activity model keeps a flood of PTY output from
  queuing frames faster than they render (§6, spec us-22).
- `accesskit` ships on by default — the a11y tree costs nothing and
  was a hidden requirement of the "native window" story.
- The escape hatch bounds the toolkit risk: a §6 miss becomes a
  contained grid-pass rewrite, not a Round-13 do-over.
- `eframe` is a dep of `ait-gui` only (plus `ait`'s optional `gui`
  feature, ADR-0016); `cargo build -p ait` pulls zero GUI crates —
  the PR-evidence `cargo tree` check enforces it.
- Bundled egui monospace supplies the grid's cell metrics
  (`default_fonts`) with `system_fonts` as the missing-glyph
  fallback; custom font selection is out of scope this round.
