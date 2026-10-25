//! Named chord strings (`"Alt+T"`, `"Ctrl+PageDown"`) → [`KeyEvent`].
//!
//! Only what the Legacy capability tier expresses matters here: the
//! harness's PTY never offers kitty keyboard (`kitty_keyboard` stays off
//! in `ait-term`'s emulator), so a spawned `ait` always resolves
//! `CapabilityTier::Legacy` (ADR-0005) and the `Ctrl+Shift+*` aliases are
//! unreachable by construction.

use ait_term::{Key, KeyEvent, Modifiers};

/// A chord string [`parse_chord`] could not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChordError {
    chord: String,
    reason: &'static str,
}

impl std::fmt::Display for ChordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid chord {:?}: {}", self.chord, self.reason)
    }
}

impl std::error::Error for ChordError {}

/// Parse a chord like `"Alt+T"` or `"Shift+PageUp"` into a [`KeyEvent`].
///
/// `+`-separated modifier names lead (`Alt`, `Ctrl`, `Shift`, `Super`);
/// the final segment is the key — a single character, or a named key
/// (`Enter`, `Escape`, `Tab`, `Backspace`, `Space`, the arrows, `Home`,
/// `End`, `PageUp`, `PageDown`, `Insert`, `Delete`, `F1`–`F12`). Names are
/// case-insensitive.
///
/// Character casing follows what a real keypress sends: `Shift` means
/// the character arrives shifted (`"Shift+a"` is `Char('A')`), and an
/// uppercase letter under a non-Shift modifier means the *key*, not the
/// shifted text — `"Alt+T"` encodes `ESC t` because `ESC T` arrives at
/// the app as `Char('T')` with `SHIFT|ALT`, a deliberately different
/// chord. `"Alt+Shift+T"` keeps the shifted form on purpose.
pub fn parse_chord(chord: &str) -> Result<KeyEvent, ChordError> {
    let fail = |reason: &'static str| ChordError {
        chord: chord.to_string(),
        reason,
    };
    let (mods_part, key_part) = match chord.rsplit_once('+') {
        Some((mods, key)) if !mods.is_empty() => (mods, key),
        _ => ("", chord),
    };

    let mut modifiers = Modifiers::NONE;
    for name in mods_part.split('+').filter(|part| !part.is_empty()) {
        match name.to_ascii_lowercase().as_str() {
            "alt" => modifiers |= Modifiers::ALT,
            "ctrl" | "control" => modifiers |= Modifiers::CONTROL,
            "shift" => modifiers |= Modifiers::SHIFT,
            "super" | "meta" | "cmd" => modifiers |= Modifiers::SUPER,
            _ => return Err(fail("unknown modifier name")),
        }
    }

    let key = named_key(key_part)
        .or_else(|| function_key(key_part))
        .or_else(|| char_key(key_part, modifiers))
        .ok_or_else(|| fail("unknown key name"))?;
    Ok(KeyEvent::new(key, modifiers))
}

/// The named (non-character) keys.
fn named_key(name: &str) -> Option<Key> {
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => Key::Enter,
        "escape" | "esc" => Key::Escape,
        "tab" => Key::Tab,
        "backspace" => Key::Backspace,
        "space" => Key::Char(' '),
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "insert" => Key::Insert,
        "delete" | "del" => Key::Delete,
        _ => return None,
    })
}

/// `F1`–`F12` (other numbers encode to nothing, so they are rejected).
fn function_key(name: &str) -> Option<Key> {
    let number: u8 = name
        .strip_prefix('f')
        .or_else(|| name.strip_prefix('F'))?
        .parse()
        .ok()?;
    (1..=12).contains(&number).then_some(Key::F(number))
}

/// A single character, cased the way a real keypress would send it (see
/// [`parse_chord`]'s casing rule).
fn char_key(name: &str, modifiers: Modifiers) -> Option<Key> {
    let mut chars = name.chars();
    let c = match (chars.next(), chars.next()) {
        (Some(c), None) => c,
        _ => return None,
    };
    let c = if modifiers.contains(Modifiers::SHIFT) {
        c.to_ascii_uppercase()
    } else if !modifiers.is_empty() {
        c.to_ascii_lowercase()
    } else {
        c
    };
    Some(Key::Char(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(chord: &str) -> KeyEvent {
        parse_chord(chord).unwrap_or_else(|e| panic!("{e}"))
    }

    /// The §5.2 legacy-tier chords the harness must drive.
    #[test]
    fn the_legacy_binding_chords_parse() {
        for (chord, expected) in [
            ("Alt+T", KeyEvent::new(Key::Char('t'), Modifiers::ALT)),
            ("Alt+W", KeyEvent::new(Key::Char('w'), Modifiers::ALT)),
            ("Alt+Q", KeyEvent::new(Key::Char('q'), Modifiers::ALT)),
            ("Alt+1", KeyEvent::new(Key::Char('1'), Modifiers::ALT)),
            ("Alt+9", KeyEvent::new(Key::Char('9'), Modifiers::ALT)),
            (
                "Ctrl+PageDown",
                KeyEvent::new(Key::PageDown, Modifiers::CONTROL),
            ),
            (
                "Ctrl+PageUp",
                KeyEvent::new(Key::PageUp, Modifiers::CONTROL),
            ),
            ("Shift+PageUp", KeyEvent::new(Key::PageUp, Modifiers::SHIFT)),
            (
                "Shift+PageDown",
                KeyEvent::new(Key::PageDown, Modifiers::SHIFT),
            ),
            ("Shift+Home", KeyEvent::new(Key::Home, Modifiers::SHIFT)),
            ("Shift+End", KeyEvent::new(Key::End, Modifiers::SHIFT)),
            ("Shift+Insert", KeyEvent::new(Key::Insert, Modifiers::SHIFT)),
        ] {
            assert_eq!(key(chord), expected, "{chord}");
        }
    }

    /// `Alt+T` is the key combo, so the char goes out lowercase — `ESC t`
    /// is what a real Alt+T press emits; `ESC T` would report `SHIFT|ALT`.
    /// With Shift held the shifted character is the faithful one.
    #[test]
    fn char_casing_follows_real_keypresses() {
        assert_eq!(key("t"), KeyEvent::new(Key::Char('t'), Modifiers::NONE));
        assert_eq!(key("T"), KeyEvent::new(Key::Char('T'), Modifiers::NONE));
        assert_eq!(key("alt+t"), key("Alt+T"));
        assert_eq!(
            key("Alt+Shift+T"),
            KeyEvent::new(Key::Char('T'), Modifiers::ALT | Modifiers::SHIFT)
        );
        assert_eq!(
            key("Shift+a"),
            KeyEvent::new(Key::Char('A'), Modifiers::SHIFT)
        );
    }

    /// Named keys and multi-modifier chords.
    #[test]
    fn named_keys_and_modifier_sets() {
        assert_eq!(key("Enter"), KeyEvent::new(Key::Enter, Modifiers::NONE));
        assert_eq!(key("F5"), KeyEvent::new(Key::F(5), Modifiers::NONE));
        assert_eq!(
            key("ctrl+shift+left"),
            KeyEvent::new(Key::Left, Modifiers::CONTROL | Modifiers::SHIFT)
        );
        assert_eq!(key("+"), KeyEvent::new(Key::Char('+'), Modifiers::NONE));
    }

    /// Unknown names and out-of-range function keys fail loudly.
    #[test]
    fn unparseable_chords_error() {
        for chord in ["Bogus", "Ctrl+Bogus", "Alt+F13", "F99", "Ctrl+"] {
            assert!(parse_chord(chord).is_err(), "{chord} should fail");
        }
    }
}
