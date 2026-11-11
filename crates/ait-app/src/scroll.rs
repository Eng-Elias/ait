//! Scroll-offset state (Spec R02 scroll seam): how far above the live
//! viewport the view sits.
//!
//! `offset` is the number of grid lines the viewport is shifted into
//! scrollback — screen row `i` displays `grid.line(i - offset)` —
//! `0` means the live viewport (bottom). The rules the spec asks for:
//!
//! - clamped at both ends (never past the oldest line, never below 0);
//! - a scrolled-up view stays **pinned** on its lines while new output
//!   arrives — the offset follows scrollback growth ([`ScrollState::on_output`]);
//! - typing snaps to the bottom ([`ScrollState::snap_to_bottom`]).

/// Lines scrolled per mouse-wheel tick (Spec R02: 3 lines/tick).
pub const WHEEL_LINES: usize = 3;

/// The viewport's scroll position.
///
/// The state is pure — it knows nothing of sessions or grids; callers feed
/// it the current scrollback depth and page size.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScrollState {
    /// Grid lines the view is shifted into scrollback; 0 = live viewport.
    offset: usize,
    /// Scrollback depth at the last [`on_output`](Self::on_output) call —
    /// its growth is how many lines a pinned view must follow.
    last_scrollback: usize,
}

impl ScrollState {
    /// A fresh state pinned to the bottom.
    pub fn new() -> Self {
        Self::default()
    }

    /// Lines the viewport is shifted into scrollback; 0 = bottom.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// New output arrived: feed the grid's current
    /// [`scrollback()`](ait_term::GridView::scrollback) depth. Returns how
    /// many lines scrolled into history this call — the shift every grid
    /// line index underwent (mouse selections follow it the same way the
    /// pinned view does).
    ///
    /// A scrolled-up view stays pinned on the same lines, so its offset
    /// follows scrollback growth; at the bottom nothing moves. When the
    /// history buffer is full its depth stops growing and the pinned view
    /// drifts slowly — `GridView` exposes no per-line identity, so drift
    /// at the scrollback cap is the accepted bound.
    pub fn on_output(&mut self, scrollback: usize) -> usize {
        let grown = scrollback.saturating_sub(self.last_scrollback);
        if self.offset > 0 {
            self.offset = (self.offset + grown).min(scrollback);
        }
        self.last_scrollback = scrollback;
        grown
    }

    /// Scroll `lines` further into history, clamped at the oldest line.
    pub fn scroll_up(&mut self, lines: usize, scrollback: usize) {
        self.offset = (self.offset + lines).min(scrollback);
    }

    /// Scroll `lines` toward the live viewport, clamped at the bottom.
    pub fn scroll_down(&mut self, lines: usize) {
        self.offset = self.offset.saturating_sub(lines);
    }

    /// Jump to the oldest scrollback line.
    pub fn jump_to_top(&mut self, scrollback: usize) {
        self.offset = scrollback;
    }

    /// Snap back to the live viewport — what any keypress does.
    pub fn snap_to_bottom(&mut self) {
        self.offset = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wheel/page steps clamp at the oldest scrollback line and at the
    /// bottom — never past either end.
    #[test]
    fn offset_clamps_at_both_ends() {
        let mut scroll = ScrollState::new();

        scroll.scroll_up(3, 10);
        assert_eq!(scroll.offset(), 3);
        scroll.scroll_up(20, 10);
        assert_eq!(scroll.offset(), 10, "clamped at scrollback depth");
        scroll.scroll_up(1, 10);
        assert_eq!(scroll.offset(), 10);

        scroll.scroll_down(4);
        assert_eq!(scroll.offset(), 6);
        scroll.scroll_down(20);
        assert_eq!(scroll.offset(), 0, "clamped at the bottom");
        assert_eq!(scroll.offset(), 0);
        scroll.scroll_down(1);
        assert_eq!(scroll.offset(), 0);
    }

    #[test]
    fn top_and_bottom_jumps() {
        let mut scroll = ScrollState::new();
        scroll.jump_to_top(42);
        assert_eq!(scroll.offset(), 42);
        scroll.snap_to_bottom();
        assert_eq!(scroll.offset(), 0);
    }

    /// The pin rule: while scrolled up, the offset follows scrollback
    /// growth so the same lines stay on screen as output scrolls under
    /// them. At the bottom, output changes nothing.
    #[test]
    fn output_pins_scrolled_view() {
        let mut scroll = ScrollState::new();
        scroll.on_output(10); // baseline depth
        scroll.scroll_up(4, 10);
        assert_eq!(scroll.offset(), 4);

        scroll.on_output(13); // three more lines scrolled off
        assert_eq!(scroll.offset(), 7, "offset follows scrollback growth");

        scroll.on_output(13); // no growth — nothing moves
        assert_eq!(scroll.offset(), 7);
    }

    #[test]
    fn output_at_bottom_stays_at_bottom() {
        let mut scroll = ScrollState::new();
        scroll.on_output(10);
        scroll.on_output(50);
        assert_eq!(scroll.offset(), 0, "live view never drifts");
        assert_eq!(scroll.offset(), 0);
    }

    /// Pinning never pushes past the scrollback depth, and a shrinking
    /// depth (resize reflow) clamps the offset rather than stranding it
    /// beyond the oldest line.
    #[test]
    fn pinning_respects_depth_bounds() {
        let mut scroll = ScrollState::new();
        scroll.on_output(5);
        scroll.scroll_up(4, 5);
        scroll.on_output(10);
        assert_eq!(scroll.offset(), 9, "4 + 5 new lines");

        scroll.on_output(3); // depth shrank below the offset
        assert_eq!(scroll.offset(), 3, "clamped to the new depth");
    }

    /// Snap-on-keypress: any forwarded key returns the view to the live
    /// viewport regardless of depth.
    #[test]
    fn keypress_snaps_to_bottom() {
        let mut scroll = ScrollState::new();
        scroll.on_output(30);
        scroll.scroll_up(25, 30);
        assert_eq!(scroll.offset(), 25);
        scroll.snap_to_bottom();
        assert_eq!(scroll.offset(), 0);
    }
}
