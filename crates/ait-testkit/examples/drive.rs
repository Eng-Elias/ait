//! Interactive harness demo (Spec E01): drive the spawned program from
//! real stdin and watch the harness's grid after every action — the
//! same screen `E2e` assertions see, printed to your terminal.
//!
//! ```bash
//! cargo build -p ait
//! cargo run -p ait-testkit --example drive            # spawns ait
//! AIT_E2E_BIN=/path/to/bin cargo run -p ait-testkit --example drive
//! ```
//!
//! One command per line (Enter runs it):
//!
//! ```text
//! key Alt+T            press a named chord (several allowed: key Alt+T Enter)
//! text echo hi         literal text to stdin (no implicit newline)
//! bytes 1b5b353e       raw bytes, hex (od -tx1 spelling)
//! resize 100x30        new PTY size
//!                      empty line — just repaint the screen
//! dump                 write a failure-style dump to target/e2e-failures/
//! exit                 wait for the program to exit, print its status
//! quit                 end the demo (child is killed on drop)
//! ```

use std::io::{self, BufRead, Write};

use ait_testkit::{E2e, Size, SpawnConfig};

fn bin() -> String {
    if let Ok(path) = std::env::var("AIT_E2E_BIN") {
        return path;
    }
    // The example lives in ait-testkit's tree; two levels up is the
    // workspace root where `cargo build -p ait` leaves the binary.
    let default = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/debug/ait");
    if std::path::Path::new(default).exists() {
        default.to_string()
    } else {
        eprintln!("no ait binary at {default} — `cargo build -p ait` or set AIT_E2E_BIN");
        std::process::exit(2);
    }
}

fn show(app: &E2e) {
    let size = app.size();
    println!("+{}+", "-".repeat(usize::from(size.columns)));
    for row in 0..size.lines {
        println!(
            "|{:<width$}|",
            app.line(row),
            width = usize::from(size.columns)
        );
    }
    println!("+{}+", "-".repeat(usize::from(size.columns)));
    let cursor = app.cursor();
    println!("cursor: ({}, {})", cursor.line, cursor.column);
    if let Some(status) = app.exited() {
        println!("exited: {status:?}");
    }
}

fn main() {
    let program = bin();
    let mut app = E2e::spawn(SpawnConfig::new(&program, &[], Size::new(80, 24))).expect("spawn");

    // Let the first frame render, then show it.
    app.wait_for_screen("first output", |a| !a.screen_text().trim().is_empty());
    show(&app);

    print!("drive> ");
    io::stdout().flush().unwrap();
    for line in io::stdin().lock().lines() {
        let line = line.expect("read stdin");
        let (verb, rest) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
        match verb {
            "key" => {
                for chord in rest.split_whitespace() {
                    app.press(chord);
                }
            }
            "text" => app.type_text(rest),
            "bytes" => {
                let hex: String = rest.split_whitespace().collect();
                let bytes: Vec<u8> = (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex pair"))
                    .collect();
                app.send_bytes(&bytes);
            }
            "resize" => {
                let (w, h) = rest.split_once('x').expect("resize WxH");
                app.resize(Size::new(w.parse().unwrap(), h.parse().unwrap()));
            }
            "dump" => println!("{}", app.dump_evidence("drive").unwrap().display()),
            "exit" => println!("{:?}", app.wait_for_exit()),
            "quit" => break,
            "" => {}
            other => println!("unknown verb {other:?} (key|text|bytes|resize|dump|exit|quit)"),
        }
        // Give the program ~200ms to render, then repaint — this is a
        // demo, not a test, so a short settle beats wait_for_screen.
        for _ in 0..10 {
            app.wait_for_screen("a beat", |_| true);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        show(&app);
        if app.exited().is_some() {
            break;
        }
        print!("drive> ");
        io::stdout().flush().unwrap();
    }
}
