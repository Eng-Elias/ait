//! The app keymap (Spec R02 keymap seam, CODING_STANDARDS.md §7): resolved
//! **before** a key can reach the PTY.
//!
//! [`resolve`] is pure and table-driven: it maps a
//! [`crossterm::event::KeyEvent`] plus the host's [`CapabilityTier`] to an
//! [`AppAction`] or [`Resolution::PassThrough`]. Bound keys never reach the
//! PTY; unbound keys — Ctrl+S and Ctrl+Q included — pass through to the
//! shell unchanged (as XOFF/XON).
//!
//! `Ctrl+Shift+T/W/Q` bind only under [`CapabilityTier::Enhanced`]: on
//! legacy input Ctrl+Shift+letter is byte-identical to Ctrl+letter, so the
//! binding would eat the plain Ctrl+key (Ctrl+W, Ctrl+Q, …) for every host
//! that cannot tell the difference — the legacy-collapse guard.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Which key encodings the host terminal can distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityTier {
    /// No kitty keyboard enhancement: Ctrl+Shift+letter collapses onto
    /// Ctrl+letter, so the enhanced-tier bindings stay unbound.
    Legacy,
    /// Keyboard enhancement confirmed at startup and the disambiguation
    /// flags pushed — Ctrl+Shift+letter arrives distinct from Ctrl+letter.
    Enhanced,
}

/// What a bound key does.
///
/// The table models the whole Spec R02 keymap; every action has dispatch
/// in `app` — paste in T3, tabs in T4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppAction {
    /// Exit `ait`, restoring the host terminal (`Alt+Q`; enhanced alias
    /// `Ctrl+Shift+Q`).
    Quit,
    /// One viewport-height step into scrollback (`Shift+PageUp`).
    ScrollPageUp,
    /// One viewport-height step toward the bottom (`Shift+PageDown`).
    ScrollPageDown,
    /// Jump to the oldest scrollback line (`Shift+Home`).
    ScrollTop,
    /// Jump back to the live viewport (`Shift+End`).
    ScrollBottom,
    /// Paste ait's own copied selection (`Shift+Insert`).
    Paste,
    /// New tab (`Alt+T`; enhanced alias `Ctrl+Shift+T`).
    NewTab,
    /// Close tab (`Alt+W`; enhanced alias `Ctrl+Shift+W`).
    CloseTab,
    /// Previous tab (`Ctrl+PageUp`).
    PrevTab,
    /// Next tab (`Ctrl+PageDown`).
    NextTab,
    /// Jump to tab N (`Alt+1..=9`, 1-based).
    JumpToTab(usize),
    /// Treat the current input line as a command request — the Spec R04
    /// `Alt+Enter` trigger (`ESC CR` vs `CR` is byte-distinct in both
    /// tiers; ADR-0005).
    SuggestCommand,
    /// Scroll the view to the previous block's start (`Alt+Up`;
    /// Spec R05 §5.5 block navigation).
    PrevBlockStart,
    /// Scroll the view to the next block's start (`Alt+Down`).
    NextBlockStart,
    /// Select the current block's range through the existing selection
    /// machinery (`Alt+B`; Spec R05 §5.5 — copy rides the OSC-52 path).
    SelectBlock,
    /// Toggle the agent conversation panel — a right-side split on the
    /// active tab (`Alt+A`; Spec R06 §5.6 / issue #85): open focuses
    /// its input line, pressing it again while focused closes it.
    AgentPanel,
}

/// The keymap's verdict for one key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Bound — run the app action; the key never reaches the PTY.
    Action(AppAction),
    /// Unbound — encode and forward to the PTY as before.
    PassThrough,
}

/// One row of the binding table. `enhanced_only` rows require the kitty
/// keyboard enhancement — see the module docs for the collapse rule.
struct Binding {
    code: KeyCode,
    modifiers: KeyModifiers,
    enhanced_only: bool,
    action: AppAction,
}

const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
const CONTROL: KeyModifiers = KeyModifiers::CONTROL;
const ALT: KeyModifiers = KeyModifiers::ALT;
const CTRL_SHIFT: KeyModifiers = KeyModifiers::CONTROL.union(KeyModifiers::SHIFT);

/// The Spec R02 keymap. Every binding is a byte-distinguishable combo so
/// nothing the shell expects (Ctrl+S XOFF, Ctrl+Q XON, plain typing,
/// arrows) is captured. Letter bindings match case-insensitively but
/// require their exact modifier set — `Alt+Shift+T` is NOT `Alt+T`.
const BINDINGS: &[Binding] = &[
    // Scrollback navigation — both tiers.
    Binding {
        code: KeyCode::PageUp,
        modifiers: SHIFT,
        enhanced_only: false,
        action: AppAction::ScrollPageUp,
    },
    Binding {
        code: KeyCode::PageDown,
        modifiers: SHIFT,
        enhanced_only: false,
        action: AppAction::ScrollPageDown,
    },
    Binding {
        code: KeyCode::Home,
        modifiers: SHIFT,
        enhanced_only: false,
        action: AppAction::ScrollTop,
    },
    Binding {
        code: KeyCode::End,
        modifiers: SHIFT,
        enhanced_only: false,
        action: AppAction::ScrollBottom,
    },
    // Tabs — both tiers.
    Binding {
        code: KeyCode::PageUp,
        modifiers: CONTROL,
        enhanced_only: false,
        action: AppAction::PrevTab,
    },
    Binding {
        code: KeyCode::PageDown,
        modifiers: CONTROL,
        enhanced_only: false,
        action: AppAction::NextTab,
    },
    Binding {
        code: KeyCode::Char('t'),
        modifiers: ALT,
        enhanced_only: false,
        action: AppAction::NewTab,
    },
    Binding {
        code: KeyCode::Char('w'),
        modifiers: ALT,
        enhanced_only: false,
        action: AppAction::CloseTab,
    },
    Binding {
        code: KeyCode::Char('1'),
        modifiers: ALT,
        enhanced_only: false,
        action: AppAction::JumpToTab(1),
    },
    Binding {
        code: KeyCode::Char('2'),
        modifiers: ALT,
        enhanced_only: false,
        action: AppAction::JumpToTab(2),
    },
    Binding {
        code: KeyCode::Char('3'),
        modifiers: ALT,
        enhanced_only: false,
        action: AppAction::JumpToTab(3),
    },
    Binding {
        code: KeyCode::Char('4'),
        modifiers: ALT,
        enhanced_only: false,
        action: AppAction::JumpToTab(4),
    },
    Binding {
        code: KeyCode::Char('5'),
        modifiers: ALT,
        enhanced_only: false,
        action: AppAction::JumpToTab(5),
    },
    Binding {
        code: KeyCode::Char('6'),
        modifiers: ALT,
        enhanced_only: false,
        action: AppAction::JumpToTab(6),
    },
    Binding {
        code: KeyCode::Char('7'),
        modifiers: ALT,
        enhanced_only: false,
        action: AppAction::JumpToTab(7),
    },
    Binding {
        code: KeyCode::Char('8'),
        modifiers: ALT,
        enhanced_only: false,
        action: AppAction::JumpToTab(8),
    },
    Binding {
        code: KeyCode::Char('9'),
        modifiers: ALT,
        enhanced_only: false,
        action: AppAction::JumpToTab(9),
    },
    // Paste own selection — T3.
    Binding {
        code: KeyCode::Insert,
        modifiers: SHIFT,
        enhanced_only: false,
        action: AppAction::Paste,
    },
    // Quit — both tiers.
    Binding {
        code: KeyCode::Char('q'),
        modifiers: ALT,
        enhanced_only: false,
        action: AppAction::Quit,
    },
    // Spec R04 `Alt+Enter` trigger — both tiers: `ESC CR` is distinct
    // from Enter's `CR` on every host (the legacy Alt-prefix and the
    // kitty modifier report agree), and the chord is unbound in default
    // readline, so the binding steals no shell input.
    Binding {
        code: KeyCode::Enter,
        modifiers: ALT,
        enhanced_only: false,
        action: AppAction::SuggestCommand,
    },
    // Spec R05 §5.5 block UX — both tiers: `Alt+Up`/`Alt+Down` report
    // as `ESC [1;3A`-style sequences (the kitty modifier report agrees)
    // vs the plain arrows' `ESC [A`, and `Alt+B` is `ESC b` vs `b` —
    // distinguishable on every host, so no tier gate. None of the three
    // is a default readline editing key (`M-b`'s backward-word is the
    // chord the spec trades for block select — the grid-side feature
    // wins per the spec's keymap decision).
    Binding {
        code: KeyCode::Up,
        modifiers: ALT,
        enhanced_only: false,
        action: AppAction::PrevBlockStart,
    },
    Binding {
        code: KeyCode::Down,
        modifiers: ALT,
        enhanced_only: false,
        action: AppAction::NextBlockStart,
    },
    Binding {
        code: KeyCode::Char('b'),
        modifiers: ALT,
        enhanced_only: false,
        action: AppAction::SelectBlock,
    },
    // Spec R06 `Alt+A` agent-panel toggle — both tiers: `ESC a` vs `a`
    // is byte-distinct under the legacy Alt-prefix and the kitty
    // modifier report alike, and `M-a` is unbound in default readline,
    // so the binding steals no shell input (the same trade the
    // `Alt+Enter`/`Alt+B` comments record).
    Binding {
        code: KeyCode::Char('a'),
        modifiers: ALT,
        enhanced_only: false,
        action: AppAction::AgentPanel,
    },
    // Enhanced-tier aliases: on these hosts Ctrl+Shift+letter arrives as
    // `Char(letter)` + CONTROL|SHIFT via CSI-u — byte-distinct from
    // Ctrl+letter, so the collapse rule no longer applies.
    Binding {
        code: KeyCode::Char('t'),
        modifiers: CTRL_SHIFT,
        enhanced_only: true,
        action: AppAction::NewTab,
    },
    Binding {
        code: KeyCode::Char('w'),
        modifiers: CTRL_SHIFT,
        enhanced_only: true,
        action: AppAction::CloseTab,
    },
    Binding {
        code: KeyCode::Char('q'),
        modifiers: CTRL_SHIFT,
        enhanced_only: true,
        action: AppAction::Quit,
    },
];

/// Resolve one host key event against the keymap.
///
/// `Release` events never bind (they also never arrive — `ait` pushes only
/// `DISAMBIGUATE_ESCAPE_CODES`); `Repeat` re-fires the bound action, which
/// is what holding `Shift+PageUp` should do.
pub fn resolve(event: &KeyEvent, tier: CapabilityTier) -> Resolution {
    if event.kind == KeyEventKind::Release {
        return Resolution::PassThrough;
    }
    for binding in BINDINGS {
        if binding.enhanced_only && tier == CapabilityTier::Legacy {
            continue;
        }
        if binding.modifiers != event.modifiers {
            continue;
        }
        let matched = match (binding.code, event.code) {
            // Letter bindings are case-insensitive: terminals differ on
            // whether Alt+letter reports the shifted codepoint.
            (KeyCode::Char(expected), KeyCode::Char(actual)) => {
                expected.eq_ignore_ascii_case(&actual)
            }
            (expected, actual) => expected == actual,
        };
        if matched {
            return Resolution::Action(binding.action);
        }
    }
    Resolution::PassThrough
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent as CtKeyEvent, KeyEventState};

    fn press(code: KeyCode, modifiers: KeyModifiers) -> CtKeyEvent {
        CtKeyEvent::new(code, modifiers)
    }

    fn assert_action(
        code: KeyCode,
        modifiers: KeyModifiers,
        tier: CapabilityTier,
        action: AppAction,
    ) {
        assert_eq!(
            resolve(&press(code, modifiers), tier),
            Resolution::Action(action),
            "{code:?} + {modifiers:?} in {tier:?}"
        );
    }

    fn assert_pass(code: KeyCode, modifiers: KeyModifiers, tier: CapabilityTier) {
        assert_eq!(
            resolve(&press(code, modifiers), tier),
            Resolution::PassThrough,
            "{code:?} + {modifiers:?} in {tier:?}"
        );
    }

    /// Scrollback bindings exist in both tiers — they sit on
    /// byte-distinguishable keys (standard 7).
    #[test]
    fn scrollback_bindings_both_tiers() {
        for tier in [CapabilityTier::Legacy, CapabilityTier::Enhanced] {
            assert_action(KeyCode::PageUp, SHIFT, tier, AppAction::ScrollPageUp);
            assert_action(KeyCode::PageDown, SHIFT, tier, AppAction::ScrollPageDown);
            assert_action(KeyCode::Home, SHIFT, tier, AppAction::ScrollTop);
            assert_action(KeyCode::End, SHIFT, tier, AppAction::ScrollBottom);
        }
    }

    /// Alt+Q quits on every host.
    #[test]
    fn alt_q_quits_both_tiers() {
        for tier in [CapabilityTier::Legacy, CapabilityTier::Enhanced] {
            assert_action(KeyCode::Char('q'), ALT, tier, AppAction::Quit);
        }
    }

    /// The tab/paste bindings exist in the table.
    #[test]
    fn tab_and_paste_bindings_resolve() {
        for tier in [CapabilityTier::Legacy, CapabilityTier::Enhanced] {
            assert_action(KeyCode::PageUp, CONTROL, tier, AppAction::PrevTab);
            assert_action(KeyCode::PageDown, CONTROL, tier, AppAction::NextTab);
            assert_action(KeyCode::Char('t'), ALT, tier, AppAction::NewTab);
            assert_action(KeyCode::Char('w'), ALT, tier, AppAction::CloseTab);
            assert_action(KeyCode::Insert, SHIFT, tier, AppAction::Paste);
            for digit in 1..=9 {
                let code = KeyCode::Char(char::from_digit(digit, 10).unwrap());
                assert_eq!(
                    resolve(&press(code, ALT), tier),
                    Resolution::Action(AppAction::JumpToTab(digit as usize)),
                );
            }
        }
    }

    /// The legacy-collapse guard: without the enhancement, Ctrl+Shift+T/W/Q
    /// must NOT bind — the host cannot tell them from Ctrl+letter, so the
    /// same keypresses that type SIGTSTP-class bytes keep flowing.
    #[test]
    fn ctrl_shift_aliases_need_enhancement() {
        for (c, action) in [
            ('t', AppAction::NewTab),
            ('w', AppAction::CloseTab),
            ('q', AppAction::Quit),
        ] {
            assert_action(
                KeyCode::Char(c),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                CapabilityTier::Enhanced,
                action,
            );
            assert_pass(
                KeyCode::Char(c),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                CapabilityTier::Legacy,
            );
        }
    }

    /// Kitty sends the shifted codepoint too (`Char('T')` with CONTROL|SHIFT
    /// under alternate-key reporting); the alias still matches — but a
    /// plain Ctrl+T in the enhanced tier stays unbound and reaches the PTY.
    #[test]
    fn enhanced_tier_matches_shifted_codepoint() {
        assert_action(
            KeyCode::Char('T'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            CapabilityTier::Enhanced,
            AppAction::NewTab,
        );
        assert_pass(KeyCode::Char('t'), CONTROL, CapabilityTier::Enhanced);
        assert_pass(KeyCode::Char('q'), CONTROL, CapabilityTier::Enhanced);
    }

    /// Letter bindings match the letter, not the case — hosts disagree on
    /// whether Alt+letter reports 'q' or 'Q'.
    #[test]
    fn letter_bindings_ignore_case() {
        assert_action(
            KeyCode::Char('Q'),
            ALT,
            CapabilityTier::Legacy,
            AppAction::Quit,
        );
    }

    /// Bound keys are exact combos: adding a modifier unbinds them, so
    /// `Alt+Shift+Q` and `Ctrl+Alt+Q` flow through to the shell.
    #[test]
    fn extra_modifiers_unbind() {
        for tier in [CapabilityTier::Legacy, CapabilityTier::Enhanced] {
            assert_pass(
                KeyCode::Char('q'),
                KeyModifiers::ALT | KeyModifiers::SHIFT,
                tier,
            );
            assert_pass(
                KeyCode::Char('q'),
                KeyModifiers::ALT | KeyModifiers::CONTROL,
                tier,
            );
            assert_pass(
                KeyCode::PageUp,
                KeyModifiers::SHIFT | KeyModifiers::CONTROL,
                tier,
            );
        }
    }

    /// The Spec R05 block-UX chords bind in both tiers: `Alt+Up`/
    /// `Alt+Down` arrive as `ESC [1;3A`-style sequences (the kitty
    /// modifier report agrees) — byte-distinct from the plain arrows'
    /// `ESC [A` — and `Alt+B` is `ESC b` vs `b`. `ESC`-prefixing makes
    /// every one distinguishable without the enhancement, so there is
    /// no tier gate. Plain arrows and typing still pass through, and
    /// added modifiers unbind the chords.
    #[test]
    fn block_chords_bind_in_both_tiers() {
        for tier in [CapabilityTier::Legacy, CapabilityTier::Enhanced] {
            assert_action(KeyCode::Up, ALT, tier, AppAction::PrevBlockStart);
            assert_action(KeyCode::Down, ALT, tier, AppAction::NextBlockStart);
            assert_action(KeyCode::Char('b'), ALT, tier, AppAction::SelectBlock);
            // The un-modified keys are shell input, not app actions.
            assert_pass(KeyCode::Up, KeyModifiers::NONE, tier);
            assert_pass(KeyCode::Down, KeyModifiers::NONE, tier);
            assert_pass(KeyCode::Char('b'), KeyModifiers::NONE, tier);
        }
    }

    /// Bound means the exact combo: `Alt+Shift`/`Ctrl+Alt` variants and
    /// a shifted codepoint with an extra modifier all fall through to
    /// the PTY (same rule `extra_modifiers_unbind` enforces).
    #[test]
    fn block_chords_unbind_with_extra_modifiers() {
        for tier in [CapabilityTier::Legacy, CapabilityTier::Enhanced] {
            assert_pass(KeyCode::Up, KeyModifiers::ALT | KeyModifiers::SHIFT, tier);
            assert_pass(
                KeyCode::Down,
                KeyModifiers::ALT | KeyModifiers::CONTROL,
                tier,
            );
            assert_pass(
                KeyCode::Char('b'),
                KeyModifiers::ALT | KeyModifiers::SHIFT,
                tier,
            );
            assert_pass(
                KeyCode::Char('b'),
                KeyModifiers::ALT | KeyModifiers::CONTROL,
                tier,
            );
        }
    }

    /// `Alt+A` is the Spec R06 agent-panel toggle (`AgentPanel`):
    /// `ESC a` vs `a` is byte-distinct on every host — no tier gate —
    /// and `M-a` is unbound in default readline, so binding it steals
    /// no shell input. Plain `a` still passes through, and the exact-
    /// modifier rule keeps `Alt+Shift+A`/`Ctrl+Alt+A` for the PTY.
    #[test]
    fn alt_a_binds_in_both_tiers() {
        for tier in [CapabilityTier::Legacy, CapabilityTier::Enhanced] {
            assert_action(KeyCode::Char('a'), ALT, tier, AppAction::AgentPanel);
            // Case-insensitive like every letter binding.
            assert_action(KeyCode::Char('A'), ALT, tier, AppAction::AgentPanel);
            assert_pass(KeyCode::Char('a'), KeyModifiers::NONE, tier);
            assert_pass(
                KeyCode::Char('a'),
                KeyModifiers::ALT | KeyModifiers::SHIFT,
                tier,
            );
            assert_pass(
                KeyCode::Char('a'),
                KeyModifiers::ALT | KeyModifiers::CONTROL,
                tier,
            );
        }
    }

    /// `Alt+Enter` is the Spec R04 second trigger (`SuggestCommand`):
    /// `ESC CR` vs `CR` is byte-distinct on every host — no tier gate —
    /// and unbound in default readline, so binding it steals nothing
    /// the shell expects (ADR-0005). Plain Enter still passes through.
    #[test]
    fn alt_enter_binds_in_both_tiers() {
        for tier in [CapabilityTier::Legacy, CapabilityTier::Enhanced] {
            assert_action(KeyCode::Enter, ALT, tier, AppAction::SuggestCommand);
            assert_pass(KeyCode::Enter, KeyModifiers::NONE, tier);
        }
    }

    /// The pass-through contract: ordinary typing, arrows, and the
    /// Ctrl+S/Ctrl+Q flow-control keys are never captured.
    #[test]
    fn unbound_keys_pass_through() {
        for tier in [CapabilityTier::Legacy, CapabilityTier::Enhanced] {
            assert_pass(KeyCode::Char('a'), KeyModifiers::NONE, tier);
            assert_pass(KeyCode::Char('s'), CONTROL, tier); // XOFF
            assert_pass(KeyCode::Char('q'), CONTROL, tier); // XON
            assert_pass(KeyCode::Enter, KeyModifiers::NONE, tier);
            assert_pass(KeyCode::Up, KeyModifiers::NONE, tier);
            assert_pass(KeyCode::Tab, KeyModifiers::NONE, tier);
            assert_pass(KeyCode::F(1), KeyModifiers::NONE, tier);
            // Unmodified navigation keys are input, not scrolling.
            assert_pass(KeyCode::PageUp, KeyModifiers::NONE, tier);
            assert_pass(KeyCode::Home, KeyModifiers::NONE, tier);
        }
    }

    /// Releases never bind; repeats re-fire the action (hold Shift+PageUp
    /// to keep paging).
    #[test]
    fn release_passes_repeat_binds() {
        let release = CtKeyEvent::new_with_kind_and_state(
            KeyCode::PageUp,
            SHIFT,
            KeyEventKind::Release,
            KeyEventState::NONE,
        );
        assert_eq!(
            resolve(&release, CapabilityTier::Enhanced),
            Resolution::PassThrough
        );

        let repeat = CtKeyEvent::new_with_kind_and_state(
            KeyCode::PageUp,
            SHIFT,
            KeyEventKind::Repeat,
            KeyEventState::NONE,
        );
        assert_eq!(
            resolve(&repeat, CapabilityTier::Enhanced),
            Resolution::Action(AppAction::ScrollPageUp)
        );
    }
}
