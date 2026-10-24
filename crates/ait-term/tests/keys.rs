//! Key → PTY byte encoding tests at the public `Session` seam (issue #6):
//! frontends hand the core a `KeyEvent`; the session encodes it honoring the
//! terminal modes the application set through its own output (DECSET/DECRST
//! fed through `feed_bytes`). Expected byte strings come from the xterm /
//! alacritty encoding tables, not from the implementation.

use ait_term::{Key, KeyEvent, Keypad, Modifiers, Session, SessionOptions, Size};

fn session() -> Session {
    Session::new(SessionOptions {
        size: Size::new(80, 24),
        ..Default::default()
    })
}

fn enc(session: &Session, key: Key, modifiers: Modifiers) -> Vec<u8> {
    session.encode_key(KeyEvent::new(key, modifiers))
}

#[test]
fn plain_characters_encode_as_utf8_bytes() {
    let session = session();

    assert_eq!(enc(&session, Key::Char('a'), Modifiers::NONE), b"a");
    assert_eq!(
        enc(&session, Key::Char('é'), Modifiers::NONE),
        "é".as_bytes()
    );
    // The frontend resolves shifted text; Shift does not change the bytes.
    assert_eq!(
        enc(&session, Key::Char('A'), Modifiers::SHIFT),
        "A".as_bytes()
    );
    // Plain text never carries an escape sequence to the PTY.
    assert!(!enc(&session, Key::Char('a'), Modifiers::NONE).contains(&0x1b));
}

#[test]
fn control_characters_encode_as_c0_bytes() {
    let session = session();
    let ctrl = Modifiers::CONTROL;

    assert_eq!(enc(&session, Key::Char('a'), ctrl), b"\x01");
    assert_eq!(enc(&session, Key::Char('c'), ctrl), b"\x03");
    assert_eq!(enc(&session, Key::Char('z'), ctrl), b"\x1a");
    // Shift+Ctrl resolves like Ctrl alone (Ctrl+Shift+A == Ctrl+A).
    assert_eq!(
        enc(
            &session,
            Key::Char('A'),
            Modifiers::CONTROL | Modifiers::SHIFT
        ),
        b"\x01"
    );
    // The punctuation control space.
    assert_eq!(enc(&session, Key::Char(' '), ctrl), b"\x00");
    assert_eq!(enc(&session, Key::Char('@'), ctrl), b"\x00");
    assert_eq!(enc(&session, Key::Char('['), ctrl), b"\x1b");
    assert_eq!(enc(&session, Key::Char(']'), ctrl), b"\x1d");
    assert_eq!(enc(&session, Key::Char('?'), ctrl), b"\x7f");
}

#[test]
fn alt_prefixes_character_bytes_with_escape() {
    let session = session();

    assert_eq!(enc(&session, Key::Char('x'), Modifiers::ALT), b"\x1bx");
    assert_eq!(
        enc(
            &session,
            Key::Char('c'),
            Modifiers::CONTROL | Modifiers::ALT
        ),
        b"\x1b\x03"
    );
}

#[test]
fn enter_backspace_tab_and_escape_encode_as_control_bytes() {
    let session = session();

    assert_eq!(enc(&session, Key::Enter, Modifiers::NONE), b"\r");
    assert_eq!(enc(&session, Key::Enter, Modifiers::ALT), b"\x1b\r");

    assert_eq!(enc(&session, Key::Backspace, Modifiers::NONE), b"\x7f");
    assert_eq!(enc(&session, Key::Backspace, Modifiers::ALT), b"\x1b\x7f");
    assert_eq!(enc(&session, Key::Backspace, Modifiers::CONTROL), b"\x08");

    assert_eq!(enc(&session, Key::Tab, Modifiers::NONE), b"\t");
    assert_eq!(enc(&session, Key::Tab, Modifiers::SHIFT), b"\x1b[Z");
    assert_eq!(
        enc(&session, Key::Tab, Modifiers::SHIFT | Modifiers::ALT),
        b"\x1b\x1b[Z"
    );

    assert_eq!(enc(&session, Key::Escape, Modifiers::NONE), b"\x1b");
}

#[test]
fn arrows_encode_as_csi_in_normal_mode() {
    let session = session();

    assert_eq!(enc(&session, Key::Up, Modifiers::NONE), b"\x1b[A");
    assert_eq!(enc(&session, Key::Down, Modifiers::NONE), b"\x1b[B");
    assert_eq!(enc(&session, Key::Right, Modifiers::NONE), b"\x1b[C");
    assert_eq!(enc(&session, Key::Left, Modifiers::NONE), b"\x1b[D");
}

#[test]
fn modified_arrows_carry_the_xterm_modifier_param() {
    let session = session();

    assert_eq!(enc(&session, Key::Up, Modifiers::SHIFT), b"\x1b[1;2A");
    assert_eq!(enc(&session, Key::Right, Modifiers::ALT), b"\x1b[1;3C");
    assert_eq!(enc(&session, Key::Left, Modifiers::CONTROL), b"\x1b[1;5D");
    // Shift|Alt|Control = 1 + 1 + 2 + 4 = 8.
    assert_eq!(
        enc(
            &session,
            Key::Down,
            Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL
        ),
        b"\x1b[1;8B"
    );
}

#[test]
fn home_end_and_page_keys_encode_independent_of_cursor_mode() {
    let session = session();

    assert_eq!(enc(&session, Key::Home, Modifiers::NONE), b"\x1b[H");
    assert_eq!(enc(&session, Key::End, Modifiers::NONE), b"\x1b[F");
    assert_eq!(enc(&session, Key::Home, Modifiers::CONTROL), b"\x1b[1;5H");

    assert_eq!(enc(&session, Key::Insert, Modifiers::NONE), b"\x1b[2~");
    assert_eq!(enc(&session, Key::Delete, Modifiers::NONE), b"\x1b[3~");
    assert_eq!(enc(&session, Key::Delete, Modifiers::CONTROL), b"\x1b[3;5~");
    assert_eq!(enc(&session, Key::PageUp, Modifiers::NONE), b"\x1b[5~");
    assert_eq!(enc(&session, Key::PageDown, Modifiers::NONE), b"\x1b[6~");
}

#[test]
fn function_keys_encode_like_xterm() {
    let session = session();

    // F1–F4 are SS3 unmodified, CSI with a modifier param when modified.
    assert_eq!(enc(&session, Key::F(1), Modifiers::NONE), b"\x1bOP");
    assert_eq!(enc(&session, Key::F(2), Modifiers::NONE), b"\x1bOQ");
    assert_eq!(enc(&session, Key::F(3), Modifiers::NONE), b"\x1bOR");
    assert_eq!(enc(&session, Key::F(4), Modifiers::NONE), b"\x1bOS");
    assert_eq!(enc(&session, Key::F(1), Modifiers::CONTROL), b"\x1b[1;5P");

    // F5–F12 are tilde sequences; the modifier param slots inside.
    assert_eq!(enc(&session, Key::F(5), Modifiers::NONE), b"\x1b[15~");
    assert_eq!(enc(&session, Key::F(6), Modifiers::NONE), b"\x1b[17~");
    assert_eq!(enc(&session, Key::F(7), Modifiers::NONE), b"\x1b[18~");
    assert_eq!(enc(&session, Key::F(8), Modifiers::NONE), b"\x1b[19~");
    assert_eq!(enc(&session, Key::F(9), Modifiers::NONE), b"\x1b[20~");
    assert_eq!(enc(&session, Key::F(10), Modifiers::NONE), b"\x1b[21~");
    assert_eq!(enc(&session, Key::F(11), Modifiers::NONE), b"\x1b[23~");
    assert_eq!(enc(&session, Key::F(12), Modifiers::NONE), b"\x1b[24~");
    assert_eq!(
        enc(&session, Key::F(12), Modifiers::SHIFT | Modifiers::ALT),
        b"\x1b[24;4~"
    );
}

#[test]
fn application_cursor_mode_switches_arrows_to_ss3() {
    let mut session = session();

    // DECSET ?1 (DECCKM) — an app like vim turns application cursor keys on.
    session.feed_bytes(b"\x1b[?1h");

    assert_eq!(enc(&session, Key::Up, Modifiers::NONE), b"\x1bOA");
    assert_eq!(enc(&session, Key::Down, Modifiers::NONE), b"\x1bOB");
    assert_eq!(enc(&session, Key::Right, Modifiers::NONE), b"\x1bOC");
    assert_eq!(enc(&session, Key::Left, Modifiers::NONE), b"\x1bOD");
    assert_eq!(enc(&session, Key::Home, Modifiers::NONE), b"\x1bOH");
    assert_eq!(enc(&session, Key::End, Modifiers::NONE), b"\x1bOF");

    // SS3 applies only to unmodified keys: Ctrl+Left stays the CSI form.
    assert_eq!(enc(&session, Key::Left, Modifiers::CONTROL), b"\x1b[1;5D");

    // DECRST ?1 switches the encoding back.
    session.feed_bytes(b"\x1b[?1l");
    assert_eq!(enc(&session, Key::Up, Modifiers::NONE), b"\x1b[A");
    assert_eq!(enc(&session, Key::Home, Modifiers::NONE), b"\x1b[H");
}

#[test]
fn application_keypad_mode_switches_numpad_to_ss3() {
    let mut session = session();

    // Numeric keypad mode (the default): keys send their literal bytes.
    assert_eq!(
        enc(&session, Key::Keypad(Keypad::Five), Modifiers::NONE),
        b"5"
    );
    assert_eq!(
        enc(&session, Key::Keypad(Keypad::Enter), Modifiers::NONE),
        b"\r"
    );
    assert_eq!(
        enc(&session, Key::Keypad(Keypad::Decimal), Modifiers::NONE),
        b"."
    );

    // DECKPAM (`ESC =`) turns application keypad mode on.
    session.feed_bytes(b"\x1b=");

    assert_eq!(
        enc(&session, Key::Keypad(Keypad::Enter), Modifiers::NONE),
        b"\x1bOM"
    );
    assert_eq!(
        enc(&session, Key::Keypad(Keypad::Zero), Modifiers::NONE),
        b"\x1bOp"
    );
    assert_eq!(
        enc(&session, Key::Keypad(Keypad::Five), Modifiers::NONE),
        b"\x1bOu"
    );
    assert_eq!(
        enc(&session, Key::Keypad(Keypad::Nine), Modifiers::NONE),
        b"\x1bOy"
    );
    assert_eq!(
        enc(&session, Key::Keypad(Keypad::Minus), Modifiers::NONE),
        b"\x1bOm"
    );
    assert_eq!(
        enc(&session, Key::Keypad(Keypad::Comma), Modifiers::NONE),
        b"\x1bOl"
    );
    assert_eq!(
        enc(&session, Key::Keypad(Keypad::Decimal), Modifiers::NONE),
        b"\x1bOn"
    );
    // These keys have no SS3 form; they stay literal in both modes.
    assert_eq!(
        enc(&session, Key::Keypad(Keypad::Plus), Modifiers::NONE),
        b"+"
    );

    // DECKPNM (`ESC >`) switches back to numeric mode.
    session.feed_bytes(b"\x1b>");
    assert_eq!(
        enc(&session, Key::Keypad(Keypad::Five), Modifiers::NONE),
        b"5"
    );
}
