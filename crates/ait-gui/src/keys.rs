//! egui → `ait_term` key translation (Spec R02 input seam, Spec R13
//! §5.12): the GUI's counterpart to `ait`'s `keys.rs` crossterm shim.
//!
//! [`translate_key`] maps an [`egui::Event::Key`] press to the
//! frontend-neutral [`ait_term::KeyEvent`] — or `None` when the event
//! carries no key a terminal should send (releases, bare modifiers,
//! media keys). [`translate_text`] turns an [`egui::Event::Text`] into
//! one [`Key::Char`] event per character: composed text (dead keys,
//! IME, macOS Option sequences) arrives through `Text`, not `Key`, and
//! the PTY wants the resolved character.
//!
//! The app keymap ([`ait_app::keymap`]) resolves each translated event
//! **before** it can reach the PTY — bound actions never see the shell
//! (CODING_STANDARDS.md §7).

use ait_term::{Key, KeyEvent, Modifiers};
use eframe::egui;

/// egui's modifier set → ait-term [`Modifiers`]. `command` collapses
/// onto `ctrl` (Linux/Windows) or `mac_cmd` (macOS), so the source bits
/// are `ctrl`, `alt`, `shift` and `mac_cmd` — `mac_cmd` maps to SUPER,
/// the ait-term bit ait already uses for the Command/Windows key.
pub fn translate_modifiers(modifiers: egui::Modifiers) -> Modifiers {
    let mut out = Modifiers::NONE;
    if modifiers.shift {
        out |= Modifiers::SHIFT;
    }
    if modifiers.alt {
        out |= Modifiers::ALT;
    }
    if modifiers.ctrl {
        out |= Modifiers::CONTROL;
    }
    if modifiers.mac_cmd {
        out |= Modifiers::SUPER;
    }
    out
}

/// Translate one egui key press into an `ait_term` key event.
///
/// `pressed == false` (releases) and `repeat` is honored by the caller:
/// a release sends nothing — a terminal writes bytes on key-down only —
/// while a repeat re-fires (holding a key autorepeats, and re-fires a
/// bound action, matching the keymap's repeat semantics).
///
/// Keys that produce text (letters, digits, punctuation, space)
/// translate **only while Ctrl/Alt/Super is held**: their plain presses
/// already arrive as [`egui::Event::Text`], so passing them here too
/// would type every character twice. With a command modifier held egui
/// emits no `Text`, so the chord lands here exactly once.
pub fn translate_key(key: egui::Key, modifiers: egui::Modifiers) -> Option<KeyEvent> {
    let mods = translate_modifiers(modifiers);
    if let Some(key) = named_key(key) {
        return Some(KeyEvent::new(key, mods));
    }
    // Text-producing keys: chord-only, per the module docs.
    if !(modifiers.ctrl || modifiers.alt || modifiers.mac_cmd) {
        return None;
    }
    let c = char_key(key, modifiers.shift)?;
    Some(KeyEvent::new(Key::Char(c), mods))
}

/// One [`Key::Char`] event per character of an [`egui::Event::Text`]
/// payload — the composed, layout-resolved text, sent with no modifier
/// bits (egui reports none on `Text`; the character IS the input).
pub fn translate_text(text: &str) -> impl Iterator<Item = KeyEvent> + '_ {
    text.chars()
        .map(|c| KeyEvent::new(Key::Char(c), Modifiers::NONE))
}

/// Keys that never produce `Event::Text` — they always translate,
/// modifiers or not.
fn named_key(key: egui::Key) -> Option<Key> {
    let key = match key {
        egui::Key::Enter => Key::Enter,
        egui::Key::Backspace => Key::Backspace,
        egui::Key::Tab => Key::Tab,
        egui::Key::Escape => Key::Escape,
        egui::Key::ArrowUp => Key::Up,
        egui::Key::ArrowDown => Key::Down,
        egui::Key::ArrowLeft => Key::Left,
        egui::Key::ArrowRight => Key::Right,
        egui::Key::Home => Key::Home,
        egui::Key::End => Key::End,
        egui::Key::PageUp => Key::PageUp,
        egui::Key::PageDown => Key::PageDown,
        egui::Key::Insert => Key::Insert,
        egui::Key::Delete => Key::Delete,
        egui::Key::F1 => Key::F(1),
        egui::Key::F2 => Key::F(2),
        egui::Key::F3 => Key::F(3),
        egui::Key::F4 => Key::F(4),
        egui::Key::F5 => Key::F(5),
        egui::Key::F6 => Key::F(6),
        egui::Key::F7 => Key::F(7),
        egui::Key::F8 => Key::F(8),
        egui::Key::F9 => Key::F(9),
        egui::Key::F10 => Key::F(10),
        egui::Key::F11 => Key::F(11),
        egui::Key::F12 => Key::F(12),
        egui::Key::F13 => Key::F(13),
        egui::Key::F14 => Key::F(14),
        egui::Key::F15 => Key::F(15),
        egui::Key::F16 => Key::F(16),
        egui::Key::F17 => Key::F(17),
        egui::Key::F18 => Key::F(18),
        egui::Key::F19 => Key::F(19),
        egui::Key::F20 => Key::F(20),
        _ => return None,
    };
    Some(key)
}

/// egui punctuation/letter/digit keys → the character a PTY expects.
/// Letters case-fold on `shift` (egui's `Key::A` is caseless); egui's
/// logical punctuation variants (`Colon`, `Questionmark`, …) already
/// carry the shift resolution, so they map to their own character.
fn char_key(key: egui::Key, shift: bool) -> Option<char> {
    let c = match key {
        egui::Key::Space => ' ',
        egui::Key::Num0 => '0',
        egui::Key::Num1 => '1',
        egui::Key::Num2 => '2',
        egui::Key::Num3 => '3',
        egui::Key::Num4 => '4',
        egui::Key::Num5 => '5',
        egui::Key::Num6 => '6',
        egui::Key::Num7 => '7',
        egui::Key::Num8 => '8',
        egui::Key::Num9 => '9',
        egui::Key::Colon => ':',
        egui::Key::Comma => ',',
        egui::Key::Backslash => '\\',
        egui::Key::Slash => '/',
        egui::Key::Pipe => '|',
        egui::Key::Questionmark => '?',
        egui::Key::Exclamationmark => '!',
        egui::Key::OpenBracket => '[',
        egui::Key::CloseBracket => ']',
        egui::Key::OpenCurlyBracket => '{',
        egui::Key::CloseCurlyBracket => '}',
        egui::Key::Backtick => '`',
        egui::Key::Minus => '-',
        egui::Key::Period => '.',
        egui::Key::Plus => '+',
        egui::Key::Equals => '=',
        egui::Key::Semicolon => ';',
        egui::Key::Quote => '\'',
        letter => {
            let base = match letter {
                egui::Key::A => 'a',
                egui::Key::B => 'b',
                egui::Key::C => 'c',
                egui::Key::D => 'd',
                egui::Key::E => 'e',
                egui::Key::F => 'f',
                egui::Key::G => 'g',
                egui::Key::H => 'h',
                egui::Key::I => 'i',
                egui::Key::J => 'j',
                egui::Key::K => 'k',
                egui::Key::L => 'l',
                egui::Key::M => 'm',
                egui::Key::N => 'n',
                egui::Key::O => 'o',
                egui::Key::P => 'p',
                egui::Key::Q => 'q',
                egui::Key::R => 'r',
                egui::Key::S => 's',
                egui::Key::T => 't',
                egui::Key::U => 'u',
                egui::Key::V => 'v',
                egui::Key::W => 'w',
                egui::Key::X => 'x',
                egui::Key::Y => 'y',
                egui::Key::Z => 'z',
                _ => return None,
            };
            if shift {
                base.to_ascii_uppercase()
            } else {
                base
            }
        }
    };
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(ctrl: bool, alt: bool, shift: bool) -> egui::Modifiers {
        egui::Modifiers {
            alt,
            ctrl,
            shift,
            mac_cmd: false,
            command: ctrl,
        }
    }

    fn key(key: Key, modifiers: Modifiers) -> Option<KeyEvent> {
        Some(KeyEvent::new(key, modifiers))
    }

    /// Plain typing keys translate to `None` — their characters arrive
    /// as `Event::Text`, and passing them here too would double every
    /// keystroke.
    #[test]
    fn unmodified_text_keys_defer_to_text_events() {
        assert_eq!(translate_key(egui::Key::A, egui::Modifiers::NONE), None);
        assert_eq!(translate_key(egui::Key::Num5, egui::Modifiers::NONE), None);
        assert_eq!(translate_key(egui::Key::Minus, egui::Modifiers::NONE), None);
        assert_eq!(translate_key(egui::Key::Space, egui::Modifiers::NONE), None);
    }

    /// Text events carry the resolved character — one `Char` event per
    /// char, modifiers empty (composed input, e.g. `é` or an IME commit).
    #[test]
    fn text_events_become_char_events() {
        let events: Vec<KeyEvent> = translate_text("aÉ9").collect();
        assert_eq!(
            events,
            vec![
                KeyEvent::new(Key::Char('a'), Modifiers::NONE),
                KeyEvent::new(Key::Char('É'), Modifiers::NONE),
                KeyEvent::new(Key::Char('9'), Modifiers::NONE),
            ]
        );
    }

    /// `Alt+T` must reach the keymap as `Char('t') + ALT` → `NewTab`;
    /// `Ctrl+Shift+Q` as `Char('q') + CTRL|SHIFT` → `Quit`. Modifier
    /// chords never produce `Event::Text`, so they land only here.
    #[test]
    fn modifier_chords_translate_as_chars() {
        assert_eq!(
            translate_key(egui::Key::T, mods(false, true, false)),
            key(Key::Char('t'), Modifiers::ALT)
        );
        assert_eq!(
            translate_key(egui::Key::Q, mods(true, false, true)),
            key(Key::Char('Q'), Modifiers::CONTROL | Modifiers::SHIFT)
        );
        // Letters case-fold on shift so case-sensitive bindings still
        // see the shifted codepoint (Alt+Shift+R vs Alt+R).
        assert_eq!(
            translate_key(egui::Key::R, mods(false, true, true)),
            key(Key::Char('R'), Modifiers::ALT | Modifiers::SHIFT)
        );
        assert_eq!(
            translate_key(egui::Key::C, mods(true, false, false)),
            key(Key::Char('c'), Modifiers::CONTROL)
        );
    }

    /// Named keys translate regardless of modifiers — Shift+PageUp is
    /// `ScrollPageUp`, plain arrows drive the child.
    #[test]
    fn named_keys_translate_with_modifiers() {
        assert_eq!(
            translate_key(egui::Key::ArrowUp, egui::Modifiers::NONE),
            key(Key::Up, Modifiers::NONE)
        );
        assert_eq!(
            translate_key(egui::Key::PageUp, mods(false, false, true)),
            key(Key::PageUp, Modifiers::SHIFT)
        );
        assert_eq!(
            translate_key(egui::Key::PageDown, mods(true, false, false)),
            key(Key::PageDown, Modifiers::CONTROL)
        );
        assert_eq!(
            translate_key(egui::Key::Enter, mods(false, true, false)),
            key(Key::Enter, Modifiers::ALT)
        );
        assert_eq!(
            translate_key(egui::Key::Backspace, mods(true, false, false)),
            key(Key::Backspace, Modifiers::CONTROL)
        );
        assert_eq!(
            translate_key(egui::Key::F5, egui::Modifiers::NONE),
            key(Key::F(5), Modifiers::NONE)
        );
    }

    /// Real events carry the source bits: Linux Ctrl arrives as
    /// `ctrl + command`, macOS ⌘ as `mac_cmd + command` — the logical
    /// `command` bit alone (egui's `Modifiers::COMMAND` constant) is
    /// NOT a source bit and maps to nothing.
    #[test]
    fn modifiers_map_source_bits() {
        let linux_ctrl = egui::Modifiers {
            ctrl: true,
            command: true,
            ..egui::Modifiers::NONE
        };
        assert_eq!(translate_modifiers(linux_ctrl), Modifiers::CONTROL);
        let mac_cmd = egui::Modifiers {
            mac_cmd: true,
            command: true,
            ..egui::Modifiers::NONE
        };
        assert_eq!(translate_modifiers(mac_cmd), Modifiers::SUPER);
        assert_eq!(
            translate_modifiers(egui::Modifiers::COMMAND),
            Modifiers::NONE
        );
    }

    /// Keys a terminal has no concept of — locks, media, browser
    /// navigation, the modifier keys themselves — send nothing.
    #[test]
    fn non_terminal_keys_send_nothing() {
        for key in [
            egui::Key::CapsLock,
            egui::Key::NumLock,
            egui::Key::ScrollLock,
            egui::Key::PrintScreen,
            egui::Key::Pause,
            egui::Key::Menu,
            egui::Key::BrowserBack,
            egui::Key::MediaTrackNext,
            egui::Key::AudioVolumeMute,
            egui::Key::LaunchMail,
            egui::Key::ShiftLeft,
            egui::Key::ControlLeft,
            egui::Key::AltLeft,
            egui::Key::SuperLeft,
        ] {
            assert_eq!(translate_key(key, egui::Modifiers::NONE), None, "{key:?}");
        }
    }
}
