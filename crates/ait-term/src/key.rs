//! The frontend-neutral key model and its PTY encoder (REQUIREMENTS.md §5.1).
//!
//! Frontends translate their native key events into [`KeyEvent`] and never
//! see an escape sequence; the core owns the encoding.
//! [`Session::encode_key`](crate::Session::encode_key) maps an event to the
//! bytes a real terminal would send, honoring the modes the application set
//! through its own output: application cursor keys (DECCKM, `?1`) and
//! application keypad (DECKPAM/DECKPNM, `ESC =` / `ESC >`).

const ESC: u8 = 0x1b;

/// Modifier bitmask for a [`KeyEvent`], in xterm's `modifyCursorKeys` order.
///
/// Combine with `|`:
///
/// ```
/// use ait_term::Modifiers;
/// let mods = Modifiers::CONTROL | Modifiers::ALT;
/// assert!(mods.contains(Modifiers::CONTROL));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers(u8);

impl Modifiers {
    /// No modifiers held.
    pub const NONE: Self = Self(0);
    pub const SHIFT: Self = Self(1);
    pub const ALT: Self = Self(2);
    pub const CONTROL: Self = Self(4);
    pub const SUPER: Self = Self(8);

    /// True when no modifier bits are set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// True when every bit in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The xterm modifier parameter: `1 + shift?1 + alt?2 + ctrl?4 + super?8`.
    fn xterm_param(self) -> u8 {
        self.0 + 1
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// A numpad key.
///
/// Numpad keys are distinct from [`Key::Char`]/[`Key::Enter`] because their
/// encoding changes under application keypad mode (DECKPAM): digits, Enter,
/// `.`, `,` and `-` gain SS3 forms, while `+`, `*`, `/` and `=` stay literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keypad {
    Enter,
    Zero,
    One,
    Two,
    Three,
    Four,
    Five,
    Six,
    Seven,
    Eight,
    Nine,
    /// The `.` key.
    Decimal,
    Comma,
    Minus,
    Plus,
    Multiply,
    Divide,
    Equal,
}

/// One key press, without modifiers — the only contract a frontend needs.
///
/// `Char` carries the already-resolved text character: a frontend reports
/// `Char('A')` with `SHIFT`, not `Char('a')` plus an expectation that the
/// core uppercases it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A text character.
    Char(char),
    Enter,
    Backspace,
    Tab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// A function key, F1–F12. Other numbers encode to nothing.
    F(u8),
    /// A numpad key; see [`Keypad`].
    Keypad(Keypad),
}

/// A key press plus the modifiers held while it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub key: Key,
    pub modifiers: Modifiers,
}

impl KeyEvent {
    pub const fn new(key: Key, modifiers: Modifiers) -> Self {
        Self { key, modifiers }
    }
}

/// The terminal modes that change key encodings — a private snapshot of the
/// emulator's mode flags, read by [`Session::encode_key`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct EncodingModes {
    /// DECCKM (`?1`): unmodified arrows/Home/End send SS3 instead of CSI.
    pub app_cursor: bool,
    /// DECKPAM (`ESC =`): numpad keys send SS3 instead of literals.
    pub app_keypad: bool,
}

/// Encode `event` into PTY bytes under `modes`. An empty result means the
/// key sends nothing (e.g. a function key outside F1–F12).
pub(crate) fn encode(event: &KeyEvent, modes: EncodingModes) -> Vec<u8> {
    let mods = event.modifiers;
    match event.key {
        Key::Char(c) => encode_char(c, mods),
        Key::Enter => alt_prefixed(b"\r", mods),
        Key::Escape => alt_prefixed(&[ESC], mods),
        Key::Backspace => {
            // Ctrl+Backspace is BS; everything else is DEL.
            let byte = if mods.contains(Modifiers::CONTROL) {
                0x08
            } else {
                0x7f
            };
            alt_prefixed(&[byte], mods)
        }
        Key::Tab => {
            if mods.contains(Modifiers::SHIFT) {
                // Shift+Tab is CSI Z; Alt escapes it rather than folding in.
                alt_prefixed(&[ESC, b'[', b'Z'], mods)
            } else {
                alt_prefixed(b"\t", mods)
            }
        }
        Key::Up => cursor_key(b'A', modes.app_cursor, mods),
        Key::Down => cursor_key(b'B', modes.app_cursor, mods),
        Key::Right => cursor_key(b'C', modes.app_cursor, mods),
        Key::Left => cursor_key(b'D', modes.app_cursor, mods),
        Key::Home => cursor_key(b'H', modes.app_cursor, mods),
        Key::End => cursor_key(b'F', modes.app_cursor, mods),
        Key::Insert => tilde_key(2, mods),
        Key::Delete => tilde_key(3, mods),
        Key::PageUp => tilde_key(5, mods),
        Key::PageDown => tilde_key(6, mods),
        Key::F(n) => function_key(n, mods),
        Key::Keypad(key) => keypad_key(key, modes.app_keypad),
    }
}

/// Text input: the character's UTF-8 bytes, a C0 control byte under Ctrl, all
/// of it ESC-prefixed under Alt.
fn encode_char(c: char, mods: Modifiers) -> Vec<u8> {
    let mut out = Vec::new();
    if mods.contains(Modifiers::ALT) {
        out.push(ESC);
    }
    if mods.contains(Modifiers::CONTROL) {
        if let Some(byte) = control_byte(c) {
            out.push(byte);
            return out;
        }
        // No C0 mapping (e.g. Ctrl+é): fall back to the plain character.
    }
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    out
}

/// The C0 control byte for `c`, or `None` when the character has none.
/// Uppercase letters map like their lowercase form (Ctrl+Shift+A == Ctrl+A).
fn control_byte(c: char) -> Option<u8> {
    match c.to_ascii_lowercase() {
        ' ' | '@' => Some(0x00),
        c @ 'a'..='z' => Some(c as u8 - b'a' + 1),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

/// `ESC`-prefix `base` under Alt — for keys whose encoding is a plain byte or
/// fixed sequence rather than a modifier-param form.
fn alt_prefixed(base: &[u8], mods: Modifiers) -> Vec<u8> {
    let mut out = Vec::with_capacity(base.len() + 1);
    if mods.contains(Modifiers::ALT) {
        out.push(ESC);
    }
    out.extend_from_slice(base);
    out
}

/// Arrows and Home/End: SS3 iff application cursor mode is on *and* no
/// modifiers are held; CSI with the xterm modifier param otherwise.
fn cursor_key(final_byte: u8, app_cursor: bool, mods: Modifiers) -> Vec<u8> {
    if mods.is_empty() {
        let introducer = if app_cursor { b'O' } else { b'[' };
        vec![ESC, introducer, final_byte]
    } else {
        // `CSI 1;{m}X`; modifiers never produce the SS3 form.
        csi(&modifier_params(mods), final_byte)
    }
}

/// The `1;{m}` parameter pair for modified keys whose unmodified form is a
/// plain `CSI X` (arrows, Home/End, F1–F4).
fn modifier_params(mods: Modifiers) -> String {
    format!("1;{}", mods.xterm_param())
}

/// `CSI {n}` or `CSI {n};{m}` terminated by `~` (Insert, Delete, PageUp,
/// PageDown, F5–F12).
fn tilde_key(n: u8, mods: Modifiers) -> Vec<u8> {
    let params = if mods.is_empty() {
        n.to_string()
    } else {
        format!("{n};{}", mods.xterm_param())
    };
    csi(&params, b'~')
}

/// F1–F4 are SS3 unmodified and `CSI 1;{m}X` modified; F5–F12 are tilde
/// sequences in both cases.
fn function_key(n: u8, mods: Modifiers) -> Vec<u8> {
    match n {
        1..=4 => {
            let final_byte = b'P' + n - 1;
            if mods.is_empty() {
                vec![ESC, b'O', final_byte]
            } else {
                csi(&modifier_params(mods), final_byte)
            }
        }
        5..=12 => {
            let param = [15, 17, 18, 19, 20, 21, 23, 24][usize::from(n - 5)];
            tilde_key(param, mods)
        }
        _ => Vec::new(),
    }
}

/// Numpad keys: literal bytes in numeric mode, the xterm SS3 table in
/// application keypad mode. `+`, `*`, `/` and `=` have no SS3 form.
fn keypad_key(key: Keypad, app_keypad: bool) -> Vec<u8> {
    let (literal, ss3) = match key {
        Keypad::Enter => (b"\r".as_slice(), Some(b'M')),
        Keypad::Zero => (b"0".as_slice(), Some(b'p')),
        Keypad::One => (b"1".as_slice(), Some(b'q')),
        Keypad::Two => (b"2".as_slice(), Some(b'r')),
        Keypad::Three => (b"3".as_slice(), Some(b's')),
        Keypad::Four => (b"4".as_slice(), Some(b't')),
        Keypad::Five => (b"5".as_slice(), Some(b'u')),
        Keypad::Six => (b"6".as_slice(), Some(b'v')),
        Keypad::Seven => (b"7".as_slice(), Some(b'w')),
        Keypad::Eight => (b"8".as_slice(), Some(b'x')),
        Keypad::Nine => (b"9".as_slice(), Some(b'y')),
        Keypad::Decimal => (b".".as_slice(), Some(b'n')),
        Keypad::Comma => (b",".as_slice(), Some(b'l')),
        Keypad::Minus => (b"-".as_slice(), Some(b'm')),
        Keypad::Plus => (b"+".as_slice(), None),
        Keypad::Multiply => (b"*".as_slice(), None),
        Keypad::Divide => (b"/".as_slice(), None),
        Keypad::Equal => (b"=".as_slice(), None),
    };
    match (app_keypad, ss3) {
        (true, Some(final_byte)) => vec![ESC, b'O', final_byte],
        _ => literal.to_vec(),
    }
}

/// `ESC [` + `params` + `final_byte`.
fn csi(params: &str, final_byte: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(params.len() + 3);
    out.extend_from_slice(&[ESC, b'[']);
    out.extend_from_slice(params.as_bytes());
    out.push(final_byte);
    out
}
