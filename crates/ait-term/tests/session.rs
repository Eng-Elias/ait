//! Byte-fed tests at the public `Session` seam (Spec R01 testing decisions):
//! feed PTY output bytes in, assert on the grid — no PTY, no TTY, no mocks of
//! the VT emulator.

use ait_term::{
    AnsiColor, CellKind, Color, CursorShape, Damage, DamageBounds, Rgb, Session, SessionOptions,
    Size, Underline,
};

fn session(columns: usize, lines: usize) -> Session {
    Session::new(SessionOptions {
        size: Size::new(columns, lines),
        ..Default::default()
    })
}

#[test]
fn fed_text_lands_in_the_grid_left_to_right() {
    let mut session = session(80, 24);
    session.feed_bytes(b"hi");

    let grid = session.grid();
    assert_eq!(grid.cell(0, 0).unwrap().character, 'h');
    assert_eq!(grid.cell(0, 1).unwrap().character, 'i');
    assert_eq!(grid.cell(0, 2).unwrap().character, ' ');
}

#[test]
fn cursor_advances_with_output() {
    let mut session = session(80, 24);
    session.feed_bytes(b"hi");

    let cursor = session.cursor();
    assert_eq!(cursor.line, 0);
    assert_eq!(cursor.column, 2);
    assert_eq!(cursor.shape, CursorShape::Block);
}

#[test]
fn carriage_return_and_linefeed_start_a_new_row() {
    let mut session = session(80, 24);
    session.feed_bytes(b"ab\r\ncd");

    let grid = session.grid();
    assert_eq!(grid.line(0).unwrap().text().trim_end(), "ab");
    assert_eq!(grid.line(1).unwrap().text().trim_end(), "cd");
}

#[test]
fn sgr_named_colors_land_on_cells() {
    let mut session = session(80, 24);
    // fg red, bg bright blue: `\x1b[31;104m`.
    session.feed_bytes(b"\x1b[31;104mX");

    let cell = session.grid().cell(0, 0).unwrap();
    assert_eq!(cell.character, 'X');
    assert_eq!(cell.fg, Color::Ansi(AnsiColor::Red));
    assert_eq!(cell.bg, Color::Ansi(AnsiColor::BrightBlue));
}

#[test]
fn sgr_reset_restores_default_colors() {
    let mut session = session(80, 24);
    session.feed_bytes(b"\x1b[31mX\x1b[0mY");

    let grid = session.grid();
    assert_eq!(grid.cell(0, 0).unwrap().fg, Color::Ansi(AnsiColor::Red));
    assert_eq!(grid.cell(0, 1).unwrap().fg, Color::Default);
}

#[test]
fn sgr_indexed_and_truecolor_cells() {
    let mut session = session(80, 24);
    // fg palette index 196, bg truecolor (10, 20, 30).
    session.feed_bytes(b"\x1b[38;5;196mA\x1b[48;2;10;20;30mB");

    let grid = session.grid();
    assert_eq!(grid.cell(0, 0).unwrap().fg, Color::Indexed(196));
    assert_eq!(
        grid.cell(0, 1).unwrap().bg,
        Color::Rgb(Rgb {
            r: 10,
            g: 20,
            b: 30
        })
    );
}

#[test]
fn sgr_text_attributes_land_on_cells() {
    let mut session = session(80, 24);
    session.feed_bytes(b"\x1b[1mb\x1b[3mi\x1b[4mu");

    let grid = session.grid();
    let bold = grid.cell(0, 0).unwrap();
    assert_eq!(bold.character, 'b');
    assert!(bold.style.bold);
    assert!(!bold.style.italic);
    assert_eq!(bold.style.underline, Underline::None);

    let italic = grid.cell(0, 1).unwrap();
    assert_eq!(italic.character, 'i');
    assert!(italic.style.bold); // SGR 3 adds italic; bold was never reset.
    assert!(italic.style.italic);
    assert_eq!(italic.style.underline, Underline::None);

    let underlined = grid.cell(0, 2).unwrap();
    assert_eq!(underlined.character, 'u');
    assert_eq!(underlined.style.underline, Underline::Single);
}

#[test]
fn lines_scrolled_off_land_in_scrollback() {
    let mut session = session(80, 3);
    // Four lines on a 3-line screen pushes the first into scrollback.
    session.feed_bytes(b"one\r\ntwo\r\nthree\r\nfour");

    let grid = session.grid();
    assert_eq!(grid.scrollback(), 1);
    assert_eq!(grid.line(-1).unwrap().text().trim_end(), "one");
    assert_eq!(grid.line(0).unwrap().text().trim_end(), "two");
    assert_eq!(grid.line(2).unwrap().text().trim_end(), "four");
}

#[test]
fn scrollback_is_capped_by_the_configured_limit() {
    let mut session = Session::new(SessionOptions {
        size: Size::new(80, 2),
        scrollback: 5,
        ..Default::default()
    });
    for i in 0..10 {
        session.feed_bytes(format!("line{i}\r\n").as_bytes());
    }

    let grid = session.grid();
    assert_eq!(grid.scrollback(), 5);
    // Nine lines scrolled off; the cap keeps the five most recent, so the
    // oldest retained is `line4` and `line0..line3` were evicted.
    assert_eq!(
        grid.scrollback_rows().next().unwrap().text().trim_end(),
        "line4"
    );
}

#[test]
fn scrollback_defaults_to_ten_thousand_lines() {
    let mut session = Session::new(SessionOptions {
        size: Size::new(80, 2),
        ..Default::default()
    });
    assert_eq!(ait_term::DEFAULT_SCROLLBACK, 10_000);

    // Push more lines than the default cap; scrollback must stop at it.
    for i in 0..10_020 {
        session.feed_bytes(format!("x{i}\r\n").as_bytes());
    }
    assert_eq!(session.grid().scrollback(), 10_000);
}

#[test]
fn wide_characters_occupy_two_cells() {
    let mut session = session(80, 24);
    // U+4E16 and U+754C: full-width CJK characters, two cells each.
    session.feed_bytes("\u{4e16}\u{754c}".as_bytes());

    let grid = session.grid();
    let lead = grid.cell(0, 0).unwrap();
    assert_eq!(lead.character, '\u{4e16}');
    assert_eq!(lead.kind, CellKind::WideLead);

    assert_eq!(grid.cell(0, 1).unwrap().kind, CellKind::WideSpacer);

    let second = grid.cell(0, 2).unwrap();
    assert_eq!(second.character, '\u{754c}');
    assert_eq!(second.kind, CellKind::WideLead);

    // Row text renders each wide char once; the cursor lands past both.
    assert_eq!(grid.line(0).unwrap().text().trim_end(), "\u{4e16}\u{754c}");
    assert_eq!(session.cursor().column, 4);
}

#[test]
fn wide_emoji_occupy_two_cells() {
    let mut session = session(80, 24);
    session.feed_bytes("a🙂b".as_bytes());

    let grid = session.grid();
    assert_eq!(grid.cell(0, 0).unwrap().character, 'a');
    let emoji = grid.cell(0, 1).unwrap();
    assert_eq!(emoji.character, '🙂');
    assert_eq!(emoji.kind, CellKind::WideLead);
    assert_eq!(grid.cell(0, 2).unwrap().kind, CellKind::WideSpacer);
    assert_eq!(grid.cell(0, 3).unwrap().character, 'b');
}

#[test]
fn wrapped_text_wraps_at_the_grid_edge() {
    let mut session = session(10, 24);
    session.feed_bytes(b"abcdefghijklmnop");

    let grid = session.grid();
    assert_eq!(grid.line(0).unwrap().text(), "abcdefghij");
    assert!(grid.line(0).unwrap().wrapped());
    assert_eq!(grid.line(1).unwrap().text().trim_end(), "klmnop");
    assert!(!grid.line(1).unwrap().wrapped());
}

#[test]
fn shrinking_the_grid_reflows_wrapped_lines() {
    let mut session = session(10, 24);
    session.feed_bytes(b"abcdefghijklmnop");
    session.resize(Size::new(5, 24));

    let grid = session.grid();
    // The cursor keeps its screen row on reflow, so the two wrapped rows
    // above it move into scrollback and the rest stay in the viewport.
    assert_eq!(grid.scrollback(), 2);
    assert_eq!(grid.line(-2).unwrap().text(), "abcde");
    assert_eq!(grid.line(-1).unwrap().text(), "fghij");
    assert!(grid.line(-2).unwrap().wrapped());
    assert!(grid.line(-1).unwrap().wrapped());
    assert_eq!(grid.line(0).unwrap().text(), "klmno");
    assert_eq!(grid.line(1).unwrap().text().trim_end(), "p");
    assert!(!grid.line(1).unwrap().wrapped());

    // The full logical line survives intact across the boundary.
    let mut text = grid
        .scrollback_rows()
        .map(|row| row.text())
        .collect::<String>();
    text.extend(grid.visible_rows().map(|row| row.text()));
    assert!(text.starts_with("abcdefghijklmnop"));
}

#[test]
fn growing_the_grid_unwraps_lines() {
    let mut session = session(5, 24);
    session.feed_bytes(b"abcdefghijklmnop");
    assert_eq!(session.grid().line(0).unwrap().text(), "abcde");

    session.resize(Size::new(20, 24));
    let grid = session.grid();
    assert_eq!(grid.line(0).unwrap().text().trim_end(), "abcdefghijklmnop");
    assert!(!grid.line(0).unwrap().wrapped());
}

#[test]
fn resize_reports_the_new_size() {
    let mut session = session(80, 24);
    session.resize(Size::new(40, 10));

    assert_eq!(session.size(), Size::new(40, 10));
    assert_eq!(session.grid().columns(), 40);
    assert_eq!(session.grid().lines(), 10);
}

#[test]
fn a_fresh_session_is_fully_damaged() {
    let mut session = session(80, 24);
    assert_eq!(session.damage(), Damage::Full);
}

#[test]
fn damage_reports_only_the_lines_that_changed() {
    let mut session = session(80, 24);
    assert_eq!(session.damage(), Damage::Full);

    session.feed_bytes(b"hi");
    match session.damage() {
        Damage::Partial(lines) => {
            let line0 = lines.iter().find(|line| line.line == 0).unwrap();
            assert_eq!(line0.left, 0);
            // Cells 0-1 written plus the cell the cursor moved to.
            assert_eq!(line0.right, 2);
        }
        Damage::Full => panic!("incremental write should be partial damage"),
    }

    // Nothing new fed: only the cursor cell is reported — a renderer must
    // always repaint the cursor.
    assert_eq!(
        session.damage(),
        Damage::Partial(vec![DamageBounds {
            line: 0,
            left: 2,
            right: 2
        }])
    );
}

#[test]
fn terminal_queries_produce_pty_responses_for_a_driver() {
    let mut session = session(80, 24);
    // DSR: report device status → the terminal answers `\x1b[0n`.
    session.feed_bytes(b"\x1b[5n");

    assert_eq!(session.take_pty_responses(), vec!["\x1b[0n"]);
    assert!(session.take_pty_responses().is_empty());
}

#[test]
fn bracketed_paste_mode_reports_what_the_application_set() {
    let mut session = session(80, 24);
    assert!(!session.bracketed_paste(), "off on a fresh session");

    // Application enables bracketed paste: `CSI ? 2004 h`.
    session.feed_bytes(b"\x1b[?2004h");
    assert!(session.bracketed_paste());

    // And disables it again: `CSI ? 2004 l`.
    session.feed_bytes(b"\x1b[?2004l");
    assert!(!session.bracketed_paste());
}
