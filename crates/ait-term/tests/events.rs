//! Byte-fed tests for the OSC event stream at the public `Session` seam
//! (Spec R01 / issue #5): a parallel `vte` parser observes the same bytes fed
//! to `feed_bytes` — pure observation, nothing is stripped — and surfaces
//! `SessionEvent`s alongside the rendered grid.

use std::path::PathBuf;

use ait_term::{ClipboardSelection, PromptMarkKind, Session, SessionEvent, SessionOptions};

fn session() -> Session {
    Session::new(SessionOptions::default())
}

#[test]
fn osc7_file_uri_reports_the_working_directory() {
    let mut session = session();
    let events = session.feed_bytes(b"\x1b]7;file://some-host/home/user\x07");

    assert_eq!(
        events,
        vec![SessionEvent::CwdChanged(PathBuf::from("/home/user"))]
    );
}

#[test]
fn osc7_bare_path_reports_the_working_directory() {
    let mut session = session();
    let events = session.feed_bytes(b"\x1b]7;/home/user\x1b\\");

    assert_eq!(
        events,
        vec![SessionEvent::CwdChanged(PathBuf::from("/home/user"))]
    );
}

#[test]
fn osc7_percent_decodes_the_path() {
    let mut session = session();
    let events = session.feed_bytes(b"\x1b]7;file://host/home/user%20dir\x07");

    assert_eq!(
        events,
        vec![SessionEvent::CwdChanged(PathBuf::from("/home/user dir"))]
    );
}

#[test]
fn osc133_prompt_marks_carry_kind_and_payload() {
    let mut session = session();
    // BEL and ST (`ESC \`) both terminate; `D` carries the exit code.
    let events =
        session.feed_bytes(b"\x1b]133;A\x07\x1b]133;B\x1b\\\x1b]133;C\x07\x1b]133;D;3\x07");

    assert_eq!(
        events,
        vec![
            SessionEvent::PromptMark {
                kind: PromptMarkKind::PromptStart,
                payload: None,
            },
            SessionEvent::PromptMark {
                kind: PromptMarkKind::CommandStart,
                payload: None,
            },
            SessionEvent::PromptMark {
                kind: PromptMarkKind::OutputStart,
                payload: None,
            },
            SessionEvent::PromptMark {
                kind: PromptMarkKind::CommandEnd,
                payload: Some("3".to_owned()),
            },
        ]
    );
}

#[test]
fn osc9_produces_a_notification() {
    let mut session = session();
    let events = session.feed_bytes(b"\x1b]9;Build finished\x07");

    assert_eq!(
        events,
        vec![SessionEvent::Notification {
            title: None,
            message: "Build finished".to_owned(),
        }]
    );
}

#[test]
fn osc777_notify_produces_a_notification_with_a_title() {
    let mut session = session();
    // `OSC 777 ; notify ; <title> ; <body>` (rxvt-unicode convention).
    let events = session.feed_bytes(b"\x1b]777;notify;Build;Tests passed\x07");

    assert_eq!(
        events,
        vec![SessionEvent::Notification {
            title: Some("Build".to_owned()),
            message: "Tests passed".to_owned(),
        }]
    );
}

#[test]
fn osc777_notify_with_only_a_message_reports_no_title() {
    let mut session = session();
    // `OSC 777 ; notify ; <message>` — no body param. The `notify` verb is
    // part of the protocol, not the payload: it must never leak into the
    // message.
    let events = session.feed_bytes(b"\x1b]777;notify;Build finished\x07");

    assert_eq!(
        events,
        vec![SessionEvent::Notification {
            title: None,
            message: "Build finished".to_owned(),
        }]
    );
}

#[test]
fn osc777_bare_notify_is_malformed_and_surfaces_as_private() {
    let mut session = session();
    // `OSC 777 ; notify` with no payload at all: nothing to toast, so the
    // sequence is a recognized-but-malformed one — raw params preserved.
    let events = session.feed_bytes(b"\x1b]777;notify\x07");

    assert_eq!(
        events,
        vec![SessionEvent::Private {
            code: b"777".to_vec(),
            params: vec![b"notify".to_vec()],
        }]
    );
}

#[test]
fn osc52_is_silent_until_the_session_opts_in() {
    let mut session = session();
    // `aGk=` is base64 "hi". With the default options this must emit nothing.
    let events = session.feed_bytes(b"\x1b]52;c;aGk=\x07");

    assert_eq!(events, vec![]);
}

#[test]
fn osc52_opt_in_requests_a_clipboard_write() {
    let mut session = Session::new(SessionOptions {
        osc52: true,
        ..Default::default()
    });
    let events = session.feed_bytes(b"\x1b]52;c;aGk=\x07");

    // The event is a request: the payload arrives decoded ("hi"), and the
    // frontend performs the actual clipboard write.
    assert_eq!(
        events,
        vec![SessionEvent::ClipboardWrite {
            selection: ClipboardSelection::Clipboard,
            contents: "hi".to_owned(),
        }]
    );
}

#[test]
fn osc52_other_selections_map_to_selection() {
    let mut session = Session::new(SessionOptions {
        osc52: true,
        ..Default::default()
    });
    let events = session.feed_bytes(b"\x1b]52;p;eWVz\x07");

    assert_eq!(
        events,
        vec![SessionEvent::ClipboardWrite {
            selection: ClipboardSelection::Selection,
            contents: "yes".to_owned(),
        }]
    );
}

#[test]
fn ait_private_osc_surfaces_raw_params_and_text_still_renders() {
    let mut session = session();
    // OSC 7700 is the AIT hook channel (REQUIREMENTS.md §5.5); a hex payload
    // must arrive intact so Round 5 can decode it with zero parser changes.
    let events = session.feed_bytes(b"ls \x1b]7700;deadbeef\x07done");

    assert_eq!(
        events,
        vec![SessionEvent::Private {
            code: b"7700".to_vec(),
            params: vec![b"deadbeef".to_vec()],
        }]
    );

    // Pure observation: nothing was stripped — surrounding text rendered.
    assert_eq!(session.grid().line(0).unwrap().text().trim_end(), "ls done");
}

#[test]
fn other_osc_codes_surface_as_private() {
    let mut session = session();
    // OSC 0/2 (window title) is handled natively by the emulator but still
    // observed; a made-up code lands in the same variant.
    let events = session.feed_bytes(b"\x1b]2;my title\x07\x1b]999;x;y\x07");

    assert_eq!(
        events,
        vec![
            SessionEvent::Private {
                code: b"2".to_vec(),
                params: vec![b"my title".to_vec()],
            },
            SessionEvent::Private {
                code: b"999".to_vec(),
                params: vec![b"x".to_vec(), b"y".to_vec()],
            },
        ]
    );
}

#[test]
fn recognized_but_malformed_osc_surfaces_as_private() {
    let mut session = session();
    // OSC 7 with no path, an unknown OSC 133 mark, and an OSC with no code
    // at all — raw params are preserved, nothing panics.
    let events = session.feed_bytes(b"\x1b]7\x07\x1b]133;X\x07\x1b];;\x07");

    assert_eq!(
        events,
        vec![
            SessionEvent::Private {
                code: b"7".to_vec(),
                params: vec![],
            },
            SessionEvent::Private {
                code: b"133".to_vec(),
                params: vec![b"X".to_vec()],
            },
            SessionEvent::Private {
                code: b"".to_vec(),
                params: vec![b"".to_vec(), b"".to_vec()],
            },
        ]
    );
}

#[test]
fn malformed_osc_bytes_never_panic_or_corrupt_the_grid() {
    let mut session = session();
    // A non-UTF8 payload inside OSC 9 — tolerated and lossy-decoded, never a
    // panic, and the text around it renders untouched.
    let events = session.feed_bytes(b"ok \x1b]9;pa\xffrtial\x07fine!");

    assert_eq!(
        session.grid().line(0).unwrap().text().trim_end(),
        "ok fine!"
    );
    assert_eq!(
        events,
        vec![SessionEvent::Notification {
            title: None,
            message: "pa\u{fffd}rtial".to_owned(),
        }]
    );
}

#[test]
fn osc_sequences_split_across_feeds_complete_later() {
    let mut session = session();
    // A PTY read boundary can split an OSC mid-sequence; the parser buffers
    // it and no event fires until the terminator arrives.
    let first = session.feed_bytes(b"pwd: \x1b]7;file://host/ho");
    assert_eq!(first, vec![]);

    let second = session.feed_bytes(b"me/u\x07");
    assert_eq!(
        second,
        vec![SessionEvent::CwdChanged(PathBuf::from("/home/u"))]
    );
}
