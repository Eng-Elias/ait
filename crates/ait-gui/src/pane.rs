//! The side region's pane host (Spec R14 §5.13, ticket R14-T1): the
//! region the agent panel has always occupied generalizes into a host
//! that shows ONE pane at a time — the active tab's agent panel, or
//! one of the registered auxiliary panes (the explorer and browser
//! slots — both landed: R14-T2 and R14-T3).
//!
//! Everything a pane is lives here, inside `ait-gui`: the [`Pane`]
//! trait, the static [`PANES`] registry it enumerates, the
//! [`PaneHost`] selection state `Shared` carries (app-global and
//! ephemeral — `ait-app` learns nothing about panes), and the
//! [`pane_chord`] interceptor (`Ctrl+Shift+E`/`Ctrl+Shift+B`, the
//! `find_open_chord` precedent — no keymap entry, nothing reaches
//! the PTY).
//!
//! The precedence rule — "the agent panel claims the region while
//! open" — is [`PaneHost::visible`]'s whole job: an open agent panel
//! always shows; the aux selection only fills the region the claim
//! leaves. A chord or switcher click that picks an aux pane while a
//! claim holds is the app layer's cue to close the panel (the
//! conversation survives; `Alt+A` reopens it), so the claim and the
//! switch can never disagree about what's on screen.

use ait_term::KeyEvent;
use eframe::egui::{self, Rect};

use crate::app::Shared;
use crate::grid::GridTheme;

/// An auxiliary pane slot's stable identifier — the selection state,
/// the chord routing, the registry lookup and the egui ids all key
/// off it. The agent panel is deliberately NOT a [`PaneId`]: its open
/// state stays per-tab inside `ait-app`, and the host treats it as a
/// claim on the region rather than a registry entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneId {
    /// The file explorer slot — `Ctrl+Shift+E` (R14-T2).
    Explorer,
    /// The web browser slot — `Ctrl+Shift+B` (R14-T3).
    Browser,
}

/// What the side region shows — at most one at a time
/// ([`PaneHost::visible`]'s return).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidePane {
    /// The active tab's agent panel — the region's claimant while
    /// its per-tab `ait-app` state holds it open.
    Agent,
    /// A registered auxiliary pane.
    Aux(PaneId),
}

/// A pane the host can show — `ait-gui`-local, registered into
/// [`PANES`]. Implementations are stateless `&'static` handles: all
/// mutable pane state lives on [`Shared`] and reaches the methods
/// through the `shared` argument (the same shape the agent panel's
/// per-tab `ait-app` state already uses). `Sync` only so the static
/// registry can share the handles — panes hold no state of their own.
pub trait Pane: Sync {
    /// The slot this pane claims — one registry entry per id.
    fn id(&self) -> PaneId;
    /// The switcher chip's label.
    fn title(&self) -> &'static str;
    /// The key half of the pane's toggle chord — the modifiers are
    /// the host's uniform `Ctrl+Shift` ([`pane_chord`]).
    fn chord(&self) -> egui::Key;
    /// One unbound key routed to the pane while it shows (the find
    /// bar's focus rule: an open aux pane owns unbound input; the
    /// host itself keeps the toggle chord and plain Esc).
    fn key(&self, shared: &mut Shared, key: KeyEvent);
    /// Composed text routed to the pane while it shows —
    /// `Event::Text`/`Event::Paste` payloads land here, never on the
    /// PTY.
    fn text(&self, shared: &mut Shared, text: &str);
    /// The pane's body paint inside `rect` (the region below the
    /// switcher strip).
    fn paint(&self, ui: &mut egui::Ui, shared: &mut Shared, rect: Rect, theme: GridTheme);
}

/// The static registry — the host's enumeration of auxiliary panes,
/// in switcher order. Both slots are real: the explorer landed in
/// R14-T2, the browser in R14-T3 — no placeholders remain.
pub static PANES: &[&'static dyn Pane] = &[&crate::explorer::EXPLORER, &crate::browser::BROWSER];

/// The registry entry for a slot — the paint and input paths'
/// `PaneId` → `Pane` resolution.
pub fn pane(id: PaneId) -> &'static dyn Pane {
    PANES
        .iter()
        .find(|pane| pane.id() == id)
        .copied()
        .expect("every PaneId is registered")
}

/// The pane toggle chords — `Ctrl+Shift+<chord>` per registered
/// pane, intercepted frontend-locally exactly like `find_open_chord`:
/// the keymap can't own them (the TUI has no panes) and they never
/// reach the PTY. `Alt` stays excluded (Option/Alt chords keep
/// passing through) and `⌘` is not a pane chord.
pub fn pane_chord(key: egui::Key, modifiers: egui::Modifiers) -> Option<PaneId> {
    if !modifiers.ctrl || !modifiers.shift || modifiers.alt || modifiers.mac_cmd {
        return None;
    }
    PANES
        .iter()
        .find(|pane| pane.chord() == key)
        .map(|pane| pane.id())
}

/// The host's app-global, ephemeral selection state — the `Shared`
/// field. `selected` is which auxiliary pane would fill the region
/// once the agent claim releases; `None` means no aux pane is up.
/// Nothing here persists (us-31) and the selection follows the
/// active tab: it's one selection for the window, while the agent
/// claim resolves per tab.
#[derive(Debug, Default)]
pub struct PaneHost {
    selected: Option<PaneId>,
}

impl PaneHost {
    /// The selected auxiliary pane, if any — visible only once the
    /// agent claim releases ([`visible`](Self::visible)).
    pub fn selected(&self) -> Option<PaneId> {
        self.selected
    }

    /// Select an auxiliary pane — idempotent (the "the claim must
    /// yield" half of a toggle is the caller's job).
    pub fn select(&mut self, id: PaneId) {
        self.selected = Some(id);
    }

    /// Drop the aux selection — the shown pane's second chord press
    /// and its Esc both land here.
    pub fn clear(&mut self) {
        self.selected = None;
    }

    /// What the region shows given the active tab's agent-open flag:
    /// the agent panel while it claims the region, else the selected
    /// aux pane, else nothing (the region closes).
    pub fn visible(&self, agent_open: bool) -> Option<SidePane> {
        if agent_open {
            Some(SidePane::Agent)
        } else {
            self.selected.map(SidePane::Aux)
        }
    }

    /// Whether the region occupies space at all — the `panel_open`
    /// flag `split_content`/`session_size` generalize to: agent
    /// claim OR aux selection.
    pub fn region_open(&self, agent_open: bool) -> bool {
        self.visible(agent_open).is_some()
    }

    /// Whether `id` is what the region SHOWS for `agent_open` — the
    /// toggle's "is it already up" test: a selection hidden behind
    /// the agent claim is not showing, so its chord toggles the pane
    /// ON (the claim yields), never silently off.
    pub fn showing(&self, id: PaneId, agent_open: bool) -> bool {
        self.visible(agent_open) == Some(SidePane::Aux(id))
    }
}

#[cfg(test)]
mod tests {
    //! The host's state machine at its own seam — selection,
    //! precedence and the chord predicate, the same pure-module
    //! style `layout.rs`/`panel.rs` test.

    use super::*;

    fn mods(ctrl: bool, shift: bool, alt: bool, mac_cmd: bool) -> egui::Modifiers {
        egui::Modifiers {
            alt,
            ctrl,
            shift,
            mac_cmd,
            command: ctrl || mac_cmd,
        }
    }

    /// `Ctrl+Shift+E` picks the explorer slot, `Ctrl+Shift+B` the
    /// browser slot — the registry drives the resolution, so a new
    /// pane's chord works the day it's registered.
    #[test]
    fn chords_resolve_to_their_slots() {
        assert_eq!(
            pane_chord(egui::Key::E, mods(true, true, false, false)),
            Some(PaneId::Explorer)
        );
        assert_eq!(
            pane_chord(egui::Key::B, mods(true, true, false, false)),
            Some(PaneId::Browser)
        );
    }

    /// The chord is exactly Ctrl+Shift: bare `Ctrl+E`/`Ctrl+B` are
    /// PTY input (readline), Alt or ⌘ chords pass through, a plain
    /// shifted letter is just text, and an unregistered key under
    /// the right modifiers is nothing.
    #[test]
    fn chords_reject_every_other_modifier_set() {
        for (key, modifiers) in [
            (egui::Key::E, mods(false, false, false, false)),
            (egui::Key::E, mods(true, false, false, false)),
            (egui::Key::E, mods(false, true, false, false)),
            (egui::Key::E, mods(true, true, true, false)),
            (egui::Key::E, mods(false, false, false, true)),
            (egui::Key::E, mods(false, true, false, true)),
            (egui::Key::B, mods(true, false, false, false)),
            // No registered pane owns this chord.
            (egui::Key::G, mods(true, true, false, false)),
        ] {
            assert_eq!(pane_chord(key, modifiers), None, "{key:?} {modifiers:?}");
        }
    }

    /// The registry holds exactly the two declared slots — one entry
    /// per id, both resolvable through [`pane`].
    #[test]
    fn registry_lists_each_slot_once() {
        assert_eq!(PANES.len(), 2);
        for id in [PaneId::Explorer, PaneId::Browser] {
            assert_eq!(pane(id).id(), id);
            assert!(!pane(id).title().is_empty());
        }
        assert_ne!(
            pane(PaneId::Explorer).chord(),
            pane(PaneId::Browser).chord(),
            "chords collide"
        );
    }

    /// Nothing selected and no claim: the region is closed — and the
    /// agent panel's per-tab flag alone opens it (the claim needs no
    /// aux selection to hold the region).
    #[test]
    fn host_defaults_to_the_claim_or_closed() {
        let host = PaneHost::default();
        assert_eq!(host.selected(), None);
        assert_eq!(host.visible(false), None);
        assert!(!host.region_open(false));
        assert_eq!(host.visible(true), Some(SidePane::Agent));
        assert!(host.region_open(true));
    }

    /// Select → the region shows that pane; re-select is idempotent;
    /// clear releases it. The selection survives nothing: it's the
    /// ephemeral us-31 state.
    #[test]
    fn select_shows_and_clear_releases() {
        let mut host = PaneHost::default();
        host.select(PaneId::Explorer);
        assert_eq!(host.selected(), Some(PaneId::Explorer));
        assert_eq!(host.visible(false), Some(SidePane::Aux(PaneId::Explorer)));
        host.select(PaneId::Browser);
        assert_eq!(host.visible(false), Some(SidePane::Aux(PaneId::Browser)));
        host.clear();
        assert_eq!(host.visible(false), None);
    }

    /// The precedence rule: an open agent panel claims the region
    /// even over an aux selection — the selection waits underneath
    /// and re-emerges when the claim ends (switching to a tab whose
    /// panel is closed, or the panel closing).
    #[test]
    fn agent_claim_hides_the_selection() {
        let mut host = PaneHost::default();
        host.select(PaneId::Explorer);
        assert_eq!(host.visible(true), Some(SidePane::Agent));
        assert!(host.region_open(true));
        assert!(!host.showing(PaneId::Explorer, true));
        // The claim released — the same selection shows.
        assert_eq!(host.visible(false), Some(SidePane::Aux(PaneId::Explorer)));
        assert!(host.showing(PaneId::Explorer, false));
    }

    /// `showing` is the toggle's on/off test: a selected-but-hidden
    /// pane is not showing, so its chord must turn it ON (the app
    /// layer closes the claiming panel) rather than silently
    /// deselecting.
    #[test]
    fn showing_distinguishes_hidden_from_shown() {
        let mut host = PaneHost::default();
        host.select(PaneId::Explorer);
        // Agent claiming: selected but not shown.
        assert!(!host.showing(PaneId::Explorer, true));
        // Claim gone: shown.
        assert!(host.showing(PaneId::Explorer, false));
        // A different slot is never "showing" this one.
        assert!(!host.showing(PaneId::Browser, false));
    }
}
