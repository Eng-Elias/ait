//! The app keymap (Spec R02 keymap seam, CODING_STANDARDS.md §7): resolved
//! **before** a key can reach the PTY.
//!
//! [`resolve`] is pure and table-driven: it maps a frontend-neutral
//! [`ait_term::KeyEvent`] plus the host's [`CapabilityTier`] to an
//! [`AppAction`] or [`Resolution::PassThrough`]. Bound keys never reach the
//! PTY; unbound keys — Ctrl+S and Ctrl+Q included — pass through to the
//! shell unchanged (as XOFF/XON).
//!
//! `Ctrl+Shift+T/W/Q` bind only under [`CapabilityTier::Enhanced`]: on
//! legacy input Ctrl+Shift+letter is byte-identical to Ctrl+letter, so the
//! binding would eat the plain Ctrl+key (Ctrl+W, Ctrl+Q, …) for every host
//! that cannot tell the difference — the legacy-collapse guard. The GUI is
//! always `Enhanced`: its toolkit reports the modifier set verbatim.
//!
//! Each frontend translates its native key events into
//! [`ait_term::KeyEvent`] before resolving — `ait`'s `keys.rs` is the
//! crossterm shim; releases never translate, so [`resolve`] sees presses
//! and repeats only (a repeat re-fires the bound action, which is what
//! holding `Shift+PageUp` should do).

use ait_term::{Key, KeyEvent, Keypad, Modifiers};

/// Which key encodings the host terminal can distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityTier {
    /// No kitty keyboard enhancement: Ctrl+Shift+letter collapses onto
    /// Ctrl+letter, so the enhanced-tier bindings stay unbound.
    Legacy,
    /// Keyboard enhancement confirmed at startup and the disambiguation
    /// flags pushed — Ctrl+Shift+letter arrives distinct from Ctrl+letter.
    /// The GUI's native event source has the same fidelity, so it always
    /// reports `Enhanced`.
    Enhanced,
}

/// What a bound key does.
///
/// The table models the whole Spec R02 keymap; every action has dispatch
/// in the frontend's event loop — paste in T3, tabs in T4.
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
    /// Scroll the view to the next block's start (`Alt+Down`;
    /// Spec R05 §5.5 block navigation).
    NextBlockStart,
    /// Select the current block's range through the existing selection
    /// machinery (`Alt+B`; Spec R05 §5.5 — copy rides the OSC-52 path).
    SelectBlock,
    /// Toggle the agent conversation panel — a right-side split on the
    /// active tab (`Alt+A`; Spec R06 §5.6 / issue #85): open focuses
    /// its input line, pressing it again while focused closes it.
    AgentPanel,
    /// Switch the active execution profile (`Alt+P`; Spec R07 §5.7 /
    /// issue #101): a picker over three or more declared profiles, a
    /// direct cycle between two, a toast when there is nothing to
    /// switch to — and a second press while the picker is open
    /// dismisses it.
    SwitchProfile,
    /// Open the conversation picker over the store's rows (`Alt+R`;
    /// Spec R08 §5.8, ticket R08-T6): newest-first, Enter resumes
    /// into the active tab's panel, `Ctrl+D` deletes behind a
    /// one-key confirm — and a second press while the picker is
    /// open dismisses it.
    Conversations,
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
/// `case_sensitive` rows match `Char` bindings by exact codepoint: an
/// uppercase report IS the folded SHIFT (kitty's alternate-key report
/// substitutes the shifted codepoint and clears the flag; the legacy
/// Alt-prefix encodes `ESC R` the same way), so a case-insensitive
/// `Char('r')` match would steal `Alt+Shift+R` for `Conversations`.
struct Binding {
    key: Key,
    modifiers: Modifiers,
    enhanced_only: bool,
    case_sensitive: bool,
    action: AppAction,
}

const SHIFT: Modifiers = Modifiers::SHIFT;
const CONTROL: Modifiers = Modifiers::CONTROL;
const ALT: Modifiers = Modifiers::ALT;
const CTRL_SHIFT: Modifiers = Modifiers::CONTROL.union(Modifiers::SHIFT);

/// The Spec R02 keymap. Every binding is a byte-distinguishable combo so
/// nothing the shell expects (Ctrl+S XOFF, Ctrl+Q XON, plain typing,
/// arrows) is captured. Letter bindings match case-insensitively but
/// require their exact modifier set — `Alt+Shift+T` is NOT `Alt+T`.
const BINDINGS: &[Binding] = &[
    // Scrollback navigation — both tiers.
    Binding {
        key: Key::PageUp,
        modifiers: SHIFT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::ScrollPageUp,
    },
    Binding {
        key: Key::PageDown,
        modifiers: SHIFT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::ScrollPageDown,
    },
    Binding {
        key: Key::Home,
        modifiers: SHIFT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::ScrollTop,
    },
    Binding {
        key: Key::End,
        modifiers: SHIFT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::ScrollBottom,
    },
    // Tabs — both tiers.
    Binding {
        key: Key::PageUp,
        modifiers: CONTROL,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::PrevTab,
    },
    Binding {
        key: Key::PageDown,
        modifiers: CONTROL,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::NextTab,
    },
    Binding {
        key: Key::Char('t'),
        modifiers: ALT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::NewTab,
    },
    Binding {
        key: Key::Char('w'),
        modifiers: ALT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::CloseTab,
    },
    Binding {
        key: Key::Char('1'),
        modifiers: ALT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::JumpToTab(1),
    },
    Binding {
        key: Key::Char('2'),
        modifiers: ALT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::JumpToTab(2),
    },
    Binding {
        key: Key::Char('3'),
        modifiers: ALT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::JumpToTab(3),
    },
    Binding {
        key: Key::Char('4'),
        modifiers: ALT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::JumpToTab(4),
    },
    Binding {
        key: Key::Char('5'),
        modifiers: ALT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::JumpToTab(5),
    },
    Binding {
        key: Key::Char('6'),
        modifiers: ALT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::JumpToTab(6),
    },
    Binding {
        key: Key::Char('7'),
        modifiers: ALT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::JumpToTab(7),
    },
    Binding {
        key: Key::Char('8'),
        modifiers: ALT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::JumpToTab(8),
    },
    Binding {
        key: Key::Char('9'),
        modifiers: ALT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::JumpToTab(9),
    },
    // Paste own selection — T3.
    Binding {
        key: Key::Insert,
        modifiers: SHIFT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::Paste,
    },
    // Quit — both tiers.
    Binding {
        key: Key::Char('q'),
        modifiers: ALT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::Quit,
    },
    // Spec R04 `Alt+Enter` trigger — both tiers: `ESC CR` is distinct
    // from Enter's `CR` on every host (the legacy Alt-prefix and the
    // kitty modifier report agree), and the chord is unbound in default
    // readline, so the binding steals no shell input.
    Binding {
        key: Key::Enter,
        modifiers: ALT,
        enhanced_only: false,
        case_sensitive: false,
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
        key: Key::Up,
        modifiers: ALT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::PrevBlockStart,
    },
    Binding {
        key: Key::Down,
        modifiers: ALT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::NextBlockStart,
    },
    Binding {
        key: Key::Char('b'),
        modifiers: ALT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::SelectBlock,
    },
    // Spec R06 `Alt+A` agent-panel toggle — both tiers: `ESC a` vs `a`
    // is byte-distinct under the legacy Alt-prefix and the kitty
    // modifier report alike, and `M-a` is unbound in default readline,
    // so the binding steals no shell input (the same trade the
    // `Alt+Enter`/`Alt+B` comments record).
    Binding {
        key: Key::Char('a'),
        modifiers: ALT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::AgentPanel,
    },
    // Spec R07 §5.7 `Alt+P` profile switch — both tiers: `ESC p` vs
    // `p` is byte-distinct under the legacy Alt-prefix and the kitty
    // modifier report alike, and `M-p` is unbound in default
    // readline, so the binding steals no shell input (the same trade
    // the `Alt+A`/`Alt+B` comments record).
    Binding {
        key: Key::Char('p'),
        modifiers: ALT,
        enhanced_only: false,
        case_sensitive: false,
        action: AppAction::SwitchProfile,
    },
    // Spec R08 §5.8 `Alt+R` conversation picker — both tiers:
    // `ESC r` vs `r` is byte-distinct under the legacy Alt-prefix
    // and the kitty modifier report alike. `M-r` IS bound in
    // default readline (revert-line) — the same sanctioned trade
    // `Alt+B` makes with `M-b` (backward-word): the app-level
    // feature wins the chord where it applies. The match is
    // case-SENSITIVE, though: a `Char('R')` report is `Alt+Shift+R`
    // (alternate-key hosts fold SHIFT into the codepoint and clear
    // the flag) — that chord stays the shell's.
    Binding {
        key: Key::Char('r'),
        modifiers: ALT,
        enhanced_only: false,
        case_sensitive: true,
        action: AppAction::Conversations,
    },
    // Enhanced-tier aliases: on these hosts Ctrl+Shift+letter arrives as
    // `Char(letter)` + CONTROL|SHIFT via CSI-u — byte-distinct from
    // Ctrl+letter, so the collapse rule no longer applies.
    Binding {
        key: Key::Char('t'),
        modifiers: CTRL_SHIFT,
        enhanced_only: true,
        case_sensitive: false,
        action: AppAction::NewTab,
    },
    Binding {
        key: Key::Char('w'),
        modifiers: CTRL_SHIFT,
        enhanced_only: true,
        case_sensitive: false,
        action: AppAction::CloseTab,
    },
    Binding {
        key: Key::Char('q'),
        modifiers: CTRL_SHIFT,
        enhanced_only: true,
        case_sensitive: false,
        action: AppAction::Quit,
    },
];

/// A numpad key reports like its typing equivalent for binding purposes:
/// `ait_term`'s translation distinguishes keypad keys (their PTY encoding
/// differs under application keypad mode), but the keymap resolves the
/// gesture — `Alt+Enter` is the suggest trigger from either Enter key,
/// `Alt+5` jumps to tab 5 from either `5`, exactly as the crossterm-level
/// table matched them before the re-type. Punctuation-only keypad keys
/// (`+`, `*`, `/`, `=`, `,`, `.`, `-`) likewise normalize to their
/// `Char` codepoint — no binding owns them today, so this only keeps the
/// door open rather than changing a verdict. `Tab::agent_key` routes
/// through the same map so a keypad Enter submits the panel's input
/// like Return.
pub fn normalize(key: Key) -> Key {
    let keypad = match key {
        Key::Keypad(keypad) => keypad,
        _ => return key,
    };
    match keypad {
        Keypad::Enter => Key::Enter,
        Keypad::Zero => Key::Char('0'),
        Keypad::One => Key::Char('1'),
        Keypad::Two => Key::Char('2'),
        Keypad::Three => Key::Char('3'),
        Keypad::Four => Key::Char('4'),
        Keypad::Five => Key::Char('5'),
        Keypad::Six => Key::Char('6'),
        Keypad::Seven => Key::Char('7'),
        Keypad::Eight => Key::Char('8'),
        Keypad::Nine => Key::Char('9'),
        Keypad::Decimal => Key::Char('.'),
        Keypad::Comma => Key::Char(','),
        Keypad::Minus => Key::Char('-'),
        Keypad::Plus => Key::Char('+'),
        Keypad::Multiply => Key::Char('*'),
        Keypad::Divide => Key::Char('/'),
        Keypad::Equal => Key::Char('='),
    }
}

/// Resolve one host key event against the keymap.
///
/// Key releases never reach here — the frontend's translation drops
/// them (a terminal sends bytes on key-down only); a key *repeat*
/// re-fires the bound action, which is what holding `Shift+PageUp`
/// should do.
pub fn resolve(event: &KeyEvent, tier: CapabilityTier) -> Resolution {
    let key = normalize(event.key);
    for binding in BINDINGS {
        if binding.enhanced_only && tier == CapabilityTier::Legacy {
            continue;
        }
        if binding.modifiers != event.modifiers {
            continue;
        }
        let matched = match (binding.key, key) {
            // Letter bindings are case-insensitive: terminals differ on
            // whether Alt+letter reports the shifted codepoint — except
            // `case_sensitive` rows, where the shifted codepoint IS the
            // folded shift and must not bind (see `struct Binding`).
            (Key::Char(expected), Key::Char(actual)) => {
                if binding.case_sensitive {
                    expected == actual
                } else {
                    expected.eq_ignore_ascii_case(&actual)
                }
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

    fn press(key: Key, modifiers: Modifiers) -> KeyEvent {
        KeyEvent::new(key, modifiers)
    }

    fn assert_action(key: Key, modifiers: Modifiers, tier: CapabilityTier, action: AppAction) {
        assert_eq!(
            resolve(&press(key, modifiers), tier),
            Resolution::Action(action),
            "{key:?} + {modifiers:?} in {tier:?}"
        );
    }

    fn assert_pass(key: Key, modifiers: Modifiers, tier: CapabilityTier) {
        assert_eq!(
            resolve(&press(key, modifiers), tier),
            Resolution::PassThrough,
            "{key:?} + {modifiers:?} in {tier:?}"
        );
    }

    /// Scrollback bindings exist in both tiers — they sit on
    /// byte-distinguishable keys (standard 7).
    #[test]
    fn scrollback_bindings_both_tiers() {
        for tier in [CapabilityTier::Legacy, CapabilityTier::Enhanced] {
            assert_action(Key::PageUp, SHIFT, tier, AppAction::ScrollPageUp);
            assert_action(Key::PageDown, SHIFT, tier, AppAction::ScrollPageDown);
            assert_action(Key::Home, SHIFT, tier, AppAction::ScrollTop);
            assert_action(Key::End, SHIFT, tier, AppAction::ScrollBottom);
        }
    }

    /// Alt+Q quits on every host.
    #[test]
    fn alt_q_quits_both_tiers() {
        for tier in [CapabilityTier::Legacy, CapabilityTier::Enhanced] {
            assert_action(Key::Char('q'), ALT, tier, AppAction::Quit);
        }
    }

    /// The tab/paste bindings exist in the table.
    #[test]
    fn tab_and_paste_bindings_resolve() {
        for tier in [CapabilityTier::Legacy, CapabilityTier::Enhanced] {
            assert_action(Key::PageUp, CONTROL, tier, AppAction::PrevTab);
            assert_action(Key::PageDown, CONTROL, tier, AppAction::NextTab);
            assert_action(Key::Char('t'), ALT, tier, AppAction::NewTab);
            assert_action(Key::Char('w'), ALT, tier, AppAction::CloseTab);
            assert_action(Key::Insert, SHIFT, tier, AppAction::Paste);
            for digit in 1..=9 {
                let key = Key::Char(char::from_digit(digit, 10).unwrap());
                assert_eq!(
                    resolve(&press(key, ALT), tier),
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
                Key::Char(c),
                Modifiers::CONTROL | Modifiers::SHIFT,
                CapabilityTier::Enhanced,
                action,
            );
            assert_pass(
                Key::Char(c),
                Modifiers::CONTROL | Modifiers::SHIFT,
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
            Key::Char('T'),
            Modifiers::CONTROL | Modifiers::SHIFT,
            CapabilityTier::Enhanced,
            AppAction::NewTab,
        );
        assert_pass(Key::Char('t'), CONTROL, CapabilityTier::Enhanced);
        assert_pass(Key::Char('q'), CONTROL, CapabilityTier::Enhanced);
    }

    /// Letter bindings match the letter, not the case — hosts disagree on
    /// whether Alt+letter reports 'q' or 'Q'.
    #[test]
    fn letter_bindings_ignore_case() {
        assert_action(Key::Char('Q'), ALT, CapabilityTier::Legacy, AppAction::Quit);
    }

    /// Bound keys are exact combos: adding a modifier unbinds them, so
    /// `Alt+Shift+Q` and `Ctrl+Alt+Q` flow through to the shell.
    #[test]
    fn extra_modifiers_unbind() {
        for tier in [CapabilityTier::Legacy, CapabilityTier::Enhanced] {
            assert_pass(Key::Char('q'), Modifiers::ALT | Modifiers::SHIFT, tier);
            assert_pass(Key::Char('q'), Modifiers::ALT | Modifiers::CONTROL, tier);
            assert_pass(Key::PageUp, Modifiers::SHIFT | Modifiers::CONTROL, tier);
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
            assert_action(Key::Up, ALT, tier, AppAction::PrevBlockStart);
            assert_action(Key::Down, ALT, tier, AppAction::NextBlockStart);
            assert_action(Key::Char('b'), ALT, tier, AppAction::SelectBlock);
            assert_pass(Key::Up, Modifiers::NONE, tier);
            assert_pass(Key::Char('b'), Modifiers::NONE, tier);
            assert_pass(Key::Up, Modifiers::ALT | Modifiers::SHIFT, tier);
        }
    }

    /// The `Alt+A`/`Alt+P`/`Alt+R` app chords bind in both tiers — and
    /// `Alt+R`'s case-sensitive match leaves `Alt+Shift+R` to the shell.
    #[test]
    fn panel_profile_and_conversations_chords() {
        for tier in [CapabilityTier::Legacy, CapabilityTier::Enhanced] {
            assert_action(Key::Char('a'), ALT, tier, AppAction::AgentPanel);
            assert_action(Key::Char('p'), ALT, tier, AppAction::SwitchProfile);
            assert_action(Key::Char('r'), ALT, tier, AppAction::Conversations);
            assert_pass(Key::Char('R'), ALT, tier);
        }
    }

    /// `Alt+Enter` fires the suggest trigger in both tiers — and a
    /// keypad Enter reports the same (`ESC CR` is the chord; which
    /// physical Enter produced it is a PTY-encoding detail, not a
    /// binding distinction).
    #[test]
    fn alt_enter_suggests_both_tiers() {
        for tier in [CapabilityTier::Legacy, CapabilityTier::Enhanced] {
            assert_action(Key::Enter, ALT, tier, AppAction::SuggestCommand);
            assert_action(
                Key::Keypad(Keypad::Enter),
                ALT,
                tier,
                AppAction::SuggestCommand,
            );
            assert_pass(Key::Enter, Modifiers::NONE, tier);
        }
    }

    /// Numpad digits bind like their typing equivalents — `Alt+5` jumps
    /// to tab 5 off either `5` key, the gesture the crossterm-level
    /// table resolved before the re-type.
    #[test]
    fn keypad_digits_bind_like_chars() {
        assert_action(
            Key::Keypad(Keypad::Five),
            ALT,
            CapabilityTier::Enhanced,
            AppAction::JumpToTab(5),
        );
    }
}
