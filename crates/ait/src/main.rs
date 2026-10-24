//! `ait` — AIT's TUI binary (REQUIREMENTS.md §5.2, Round 2).
//!
//! Bare `ait` opens a full-screen terminal running `$SHELL`; `ait
//! --version` prints the scaffold's version. Everything the binary does is
//! presentation + event routing (CODING_STANDARDS.md §9) — the terminal
//! core lives in `ait-term`.

mod app;
mod backend;
mod keymap;
mod keys;
mod render;
mod scroll;
mod selection;
mod terminal;

use std::env;
use std::process::ExitCode;

const USAGE: &str = "ait — AI Terminal

USAGE:
    ait              Open a full-screen terminal running $SHELL
    ait --version    Print the version and exit
    ait --help       Print this help
";

fn main() -> ExitCode {
    match env::args().nth(1).as_deref() {
        None => match app::run() {
            Ok(code) => code,
            Err(error) => {
                // The terminal guard has already restored the host
                // terminal by the time an error reaches here.
                eprintln!("ait: {error}");
                ExitCode::FAILURE
            }
        },
        Some("--version") | Some("-V") => {
            println!("ait {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("--help") | Some("-h") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(arg) => {
            eprintln!("ait: unknown argument {arg:?}\n\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_set() {
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }
}
