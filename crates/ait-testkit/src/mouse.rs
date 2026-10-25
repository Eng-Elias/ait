//! SGR (1006) mouse sequences — the raw-bytes escape hatch for mouse
//! input (Spec E01: wheel scrolls and drag-selects driven end to end).

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// One mouse event at a screen cell.
///
/// `Up` doesn't encode which button released — the SGR wire sends code 3
/// for any release; the button is kept in the API so callers write what
/// they mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEvent {
    /// Button pressed at the cell.
    Down(MouseButton),
    /// Motion while the button is held (the drag reports hosts emit).
    Drag(MouseButton),
    /// Button released at the cell.
    Up(MouseButton),
    /// Wheel scrolled up at the cell.
    WheelUp,
    /// Wheel scrolled down at the cell.
    WheelDown,
}

/// The SGR button-code base for a press/drag.
fn button_code(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// Encode `event` at the 1-based screen cell `(column, row)` — the wire's
/// own coordinates: `ESC [ < code ; column ; row M` for presses, drags and
/// the wheel, `m` for releases.
pub(crate) fn encode(event: MouseEvent, column: u16, row: u16) -> Vec<u8> {
    let (code, final_byte) = match event {
        MouseEvent::Down(button) => (button_code(button), b'M'),
        MouseEvent::Drag(button) => (button_code(button) | 32, b'M'),
        MouseEvent::Up(_) => (3, b'm'),
        MouseEvent::WheelUp => (64, b'M'),
        MouseEvent::WheelDown => (65, b'M'),
    };
    let mut bytes = format!("\x1b[<{code};{column};{row}").into_bytes();
    bytes.push(final_byte);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sgr_encodings_match_the_spec() {
        assert_eq!(
            encode(MouseEvent::Down(MouseButton::Left), 10, 5),
            b"\x1b[<0;10;5M"
        );
        assert_eq!(
            encode(MouseEvent::Down(MouseButton::Middle), 1, 1),
            b"\x1b[<1;1;1M"
        );
        assert_eq!(
            encode(MouseEvent::Drag(MouseButton::Left), 10, 6),
            b"\x1b[<32;10;6M"
        );
        assert_eq!(
            encode(MouseEvent::Up(MouseButton::Left), 10, 6),
            b"\x1b[<3;10;6m"
        );
        assert_eq!(encode(MouseEvent::WheelUp, 40, 12), b"\x1b[<64;40;12M");
        assert_eq!(encode(MouseEvent::WheelDown, 40, 12), b"\x1b[<65;40;12M");
    }
}
