//! crossterm → `ait_term` key translation (Spec R02 input seam).
//!
//! [`translate`] is pure: it maps a [`crossterm::event::KeyEvent`] to the
//! frontend-neutral [`ait_term::KeyEvent`], or `None` when the event carries
//! no key a terminal should send (key releases, bare modifiers, media keys).
//! Encoding to PTY bytes — including application cursor/keypad modes — is
//! `ait_term`'s job ([`LiveSession::send_key`](ait_term::LiveSession::send_key)).
//!
//! The app keymap ([`ait_app::keymap`]) resolves each key event **before** this
//! translation runs: only events it resolves
//! [`PassThrough`](ait_app::keymap::Resolution::PassThrough) ever reach
//! [`translate`] — bound actions never see the PTY.

use ait_term::{Key, KeyEvent, Keypad, Modifiers};
use crossterm::event::{KeyCode, KeyEventKind, KeyEventState, KeyModifiers};

/// Translate a crossterm key event into an `ait_term` key event.
///
/// `Press` and `Repeat` kinds translate; `Release` returns `None` — a
/// terminal sends bytes on key-down only. Modifier bits ait-term has no
/// concept of (`HYPER`, `META`) are dropped, not fatal.
pub fn translate(event: &crossterm::event::KeyEvent) -> Option<KeyEvent> {
    if event.kind == KeyEventKind::Release {
        return None;
    }
    let modifiers = translate_modifiers(event.modifiers);
    let keypad = event.state.contains(KeyEventState::KEYPAD);
    let key = match event.code {
        KeyCode::Char(c) => match keypad_char(c, keypad) {
            Some(key) => key,
            None => Key::Char(c),
        },
        KeyCode::Enter if keypad => Key::Keypad(Keypad::Enter),
        KeyCode::Enter => Key::Enter,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Tab => Key::Tab,
        // Shift+Tab arrives as its own code on legacy input; ait-term sees
        // it as Tab with SHIFT (the core encodes it as CSI Z).
        KeyCode::BackTab => Key::Tab,
        KeyCode::Esc => Key::Escape,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Insert => Key::Insert,
        KeyCode::Delete => Key::Delete,
        KeyCode::F(n) => Key::F(n),
        // Bare modifiers, media keys, locks, `KeypadBegin` (ait-term has no
        // "Begin" key) and `Null` send nothing.
        _ => return None,
    };
    let modifiers = if event.code == KeyCode::BackTab {
        modifiers | Modifiers::SHIFT
    } else {
        modifiers
    };
    Some(KeyEvent::new(key, modifiers))
}

/// crossterm modifier bits → ait-term [`Modifiers`]. `HYPER`/`META` have no
/// ait-term equivalent and are dropped (they can only arrive in the
/// enhanced tier — the kitty flags push in `terminal.rs` enables them).
fn translate_modifiers(modifiers: KeyModifiers) -> Modifiers {
    let mut out = Modifiers::NONE;
    if modifiers.contains(KeyModifiers::SHIFT) {
        out |= Modifiers::SHIFT;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        out |= Modifiers::ALT;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        out |= Modifiers::CONTROL;
    }
    if modifiers.contains(KeyModifiers::SUPER) {
        out |= Modifiers::SUPER;
    }
    out
}

/// Numpad digit/punctuation characters when `KeyEventState::KEYPAD` is set
/// (kitty-style input). `+`, `*`, `/`, `=` and `,` have no SS3 form but are
/// still keypad keys; anything unmapped stays a plain [`Key::Char`].
fn keypad_char(c: char, keypad: bool) -> Option<Key> {
    if !keypad {
        return None;
    }
    let key = match c {
        '0' => Keypad::Zero,
        '1' => Keypad::One,
        '2' => Keypad::Two,
        '3' => Keypad::Three,
        '4' => Keypad::Four,
        '5' => Keypad::Five,
        '6' => Keypad::Six,
        '7' => Keypad::Seven,
        '8' => Keypad::Eight,
        '9' => Keypad::Nine,
        '.' => Keypad::Decimal,
        ',' => Keypad::Comma,
        '-' => Keypad::Minus,
        '+' => Keypad::Plus,
        '*' => Keypad::Multiply,
        '/' => Keypad::Divide,
        '=' => Keypad::Equal,
        _ => return None,
    };
    Some(Key::Keypad(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent as CtKeyEvent, KeyEventKind, KeyEventState, MediaKeyCode};

    /// Build a crossterm event the way legacy input reports it: a code plus
    /// modifiers, kind `Press`, no state.
    fn press(code: KeyCode, modifiers: KeyModifiers) -> CtKeyEvent {
        CtKeyEvent::new(code, modifiers)
    }

    fn keypad_press(code: KeyCode) -> CtKeyEvent {
        CtKeyEvent::new_with_kind_and_state(
            code,
            KeyModifiers::NONE,
            KeyEventKind::Press,
            KeyEventState::KEYPAD,
        )
    }

    fn key(key: Key, modifiers: Modifiers) -> Option<KeyEvent> {
        Some(KeyEvent::new(key, modifiers))
    }

    /// Plain text characters pass through with their character intact —
    /// crossterm reports the already-shifted character (e.g. `Char('A')`
    /// with SHIFT), which matches ait-term's `Key::Char` contract.
    #[test]
    fn characters_translate_directly() {
        assert_eq!(
            translate(&press(KeyCode::Char('a'), KeyModifiers::NONE)),
            key(Key::Char('a'), Modifiers::NONE)
        );
        assert_eq!(
            translate(&press(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            key(Key::Char('A'), Modifiers::SHIFT)
        );
        assert_eq!(
            translate(&press(KeyCode::Char('1'), KeyModifiers::NONE)),
            key(Key::Char('1'), Modifiers::NONE)
        );
        // UTF-8 text is a Char too.
        assert_eq!(
            translate(&press(KeyCode::Char('é'), KeyModifiers::NONE)),
            key(Key::Char('é'), Modifiers::NONE)
        );
    }

    /// Ctrl+letter arrives as `Char` + CONTROL; the core's encoder owns the
    /// C0-byte folding (Ctrl+Shift+S collapses to Ctrl+S on legacy input —
    /// which is why no binding may depend on distinguishing them).
    #[test]
    fn ctrl_letters_keep_modifier_and_char() {
        assert_eq!(
            translate(&press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            key(Key::Char('c'), Modifiers::CONTROL)
        );
        assert_eq!(
            translate(&press(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            key(Key::Char('s'), Modifiers::CONTROL)
        );
        assert_eq!(
            translate(&press(
                KeyCode::Char('q'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            key(Key::Char('q'), Modifiers::CONTROL | Modifiers::SHIFT)
        );
    }

    #[test]
    fn editing_keys() {
        assert_eq!(
            translate(&press(KeyCode::Enter, KeyModifiers::NONE)),
            key(Key::Enter, Modifiers::NONE)
        );
        assert_eq!(
            translate(&press(KeyCode::Backspace, KeyModifiers::NONE)),
            key(Key::Backspace, Modifiers::NONE)
        );
        assert_eq!(
            translate(&press(KeyCode::Tab, KeyModifiers::NONE)),
            key(Key::Tab, Modifiers::NONE)
        );
        assert_eq!(
            translate(&press(KeyCode::Esc, KeyModifiers::NONE)),
            key(Key::Escape, Modifiers::NONE)
        );
        assert_eq!(
            translate(&press(KeyCode::Insert, KeyModifiers::NONE)),
            key(Key::Insert, Modifiers::NONE)
        );
        assert_eq!(
            translate(&press(KeyCode::Delete, KeyModifiers::NONE)),
            key(Key::Delete, Modifiers::NONE)
        );
    }

    /// Shift+Tab surfaces as `BackTab`; it translates to Tab + SHIFT so the
    /// encoder emits `CSI Z`.
    #[test]
    fn backtab_is_shift_tab() {
        assert_eq!(
            translate(&press(KeyCode::BackTab, KeyModifiers::NONE)),
            key(Key::Tab, Modifiers::SHIFT)
        );
        // Some hosts set SHIFT on the BackTab event already — idempotent.
        assert_eq!(
            translate(&press(KeyCode::BackTab, KeyModifiers::SHIFT)),
            key(Key::Tab, Modifiers::SHIFT)
        );
    }

    #[test]
    fn arrows_and_navigation_keys() {
        for (code, key) in [
            (KeyCode::Up, Key::Up),
            (KeyCode::Down, Key::Down),
            (KeyCode::Left, Key::Left),
            (KeyCode::Right, Key::Right),
            (KeyCode::Home, Key::Home),
            (KeyCode::End, Key::End),
            (KeyCode::PageUp, Key::PageUp),
            (KeyCode::PageDown, Key::PageDown),
        ] {
            assert_eq!(
                translate(&press(code, KeyModifiers::NONE)),
                self::key(key, Modifiers::NONE)
            );
        }
    }

    #[test]
    fn modifiers_combine() {
        let mods = KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT;
        assert_eq!(
            translate(&press(KeyCode::Right, mods)),
            key(
                Key::Right,
                Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT
            )
        );
        assert_eq!(
            translate(&press(KeyCode::F(5), KeyModifiers::SUPER)),
            key(Key::F(5), Modifiers::SUPER)
        );
        // HYPER/META have no ait-term bit; the event survives without them.
        assert_eq!(
            translate(&press(
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::HYPER
            )),
            key(Key::Char('x'), Modifiers::CONTROL)
        );
    }

    #[test]
    fn function_keys() {
        for n in 1..=12 {
            assert_eq!(
                translate(&press(KeyCode::F(n), KeyModifiers::NONE)),
                key(Key::F(n), Modifiers::NONE)
            );
        }
        // F13+ — crossterm reports them; the core encodes nothing for them.
        assert_eq!(
            translate(&press(KeyCode::F(13), KeyModifiers::NONE)),
            key(Key::F(13), Modifiers::NONE)
        );
    }

    /// Numpad keys are reported as `Char`/`Enter` with the `KEYPAD` state
    /// flag once keyboard enhancements are enabled; until then they arrive
    /// as ordinary characters and translate identically.
    #[test]
    fn keypad_keys_use_state_flag() {
        assert_eq!(
            translate(&keypad_press(KeyCode::Char('5'))),
            key(Key::Keypad(Keypad::Five), Modifiers::NONE)
        );
        assert_eq!(
            translate(&keypad_press(KeyCode::Char('0'))),
            key(Key::Keypad(Keypad::Zero), Modifiers::NONE)
        );
        assert_eq!(
            translate(&keypad_press(KeyCode::Enter)),
            key(Key::Keypad(Keypad::Enter), Modifiers::NONE)
        );
        for (c, keypad) in [
            ('.', Keypad::Decimal),
            (',', Keypad::Comma),
            ('-', Keypad::Minus),
            ('+', Keypad::Plus),
            ('*', Keypad::Multiply),
            ('/', Keypad::Divide),
            ('=', Keypad::Equal),
        ] {
            assert_eq!(
                translate(&keypad_press(KeyCode::Char(c))),
                key(Key::Keypad(keypad), Modifiers::NONE)
            );
        }
        // Without the KEYPAD flag the same characters are plain input.
        assert_eq!(
            translate(&press(KeyCode::Char('5'), KeyModifiers::NONE)),
            key(Key::Char('5'), Modifiers::NONE)
        );
        // A keypad character with no numpad meaning stays a Char.
        assert_eq!(
            translate(&keypad_press(KeyCode::Char('x'))),
            key(Key::Char('x'), Modifiers::NONE)
        );
    }

    /// Repeat events resend the key (auto-repeat); releases send nothing —
    /// a terminal never writes bytes on key-up.
    #[test]
    fn repeat_sends_release_does_not() {
        let repeat = CtKeyEvent::new_with_kind_and_state(
            KeyCode::Char('j'),
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
            KeyEventState::NONE,
        );
        assert_eq!(translate(&repeat), key(Key::Char('j'), Modifiers::NONE));

        let release = CtKeyEvent::new_with_kind_and_state(
            KeyCode::Char('j'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
            KeyEventState::NONE,
        );
        assert_eq!(translate(&release), None);
    }

    /// Keys that carry no PTY meaning translate to `None`.
    #[test]
    fn non_terminal_keys_send_nothing() {
        for code in [
            KeyCode::Null,
            KeyCode::CapsLock,
            KeyCode::ScrollLock,
            KeyCode::NumLock,
            KeyCode::PrintScreen,
            KeyCode::Pause,
            KeyCode::Menu,
            KeyCode::KeypadBegin,
            KeyCode::Media(MediaKeyCode::Play),
            KeyCode::Modifier(crossterm::event::ModifierKeyCode::LeftControl),
        ] {
            assert_eq!(translate(&press(code, KeyModifiers::NONE)), None);
        }
    }
}
