//! `ait-gui` — the standalone GUI binary (Spec R13 §5.12): a thin
//! shell over [`ait_gui::run`] whose CLI surface is run +
//! `--version` + `--help` and nothing else — `--init`,
//! `--print-schema`, `--headless` and the ctl client stay on `ait`
//! (the spec's out-of-scope list). The `ait gui`/`ait --gui`
//! subcommand (T7's `gui` feature) reaches the same entry point.
//!
//! Output goes through `io::stdout`/`io::stderr` writes rather than
//! the std print macros — `check-src-hygiene.sh` exempts only
//! `ait/src/main.rs` from the print-macro ban.

use std::env;
use std::io::{self, Write};
use std::process::ExitCode;

/// `--help`'s text and the tail of every usage error — `ait`'s
/// man-page layout (NAME/SYNOPSIS/DESCRIPTION/OPTIONS) covering
/// the binary's whole surface, which is just these three arms.
const USAGE: &str = "NAME
    ait-gui — AI terminal, GUI frontend: $SHELL sessions in a
    native desktop window

SYNOPSIS
    ait-gui
    ait-gui --version
    ait-gui --help

DESCRIPTION
    Bare ait-gui opens a native window running $SHELL — tabs,
    command blocks, an agent sidebar — over the same session
    model ait's TUI renders. Every other form performs one
    action and exits. An unrecognized argument is a usage
    error: the message and this text land on stderr and the
    exit code is non-zero.

    --init, --print-schema, --headless and the ctl client are
    `ait` forms — this binary's surface is run, --version and
    --help only (Spec R13 §5.12).

OPTIONS
    --version
        Print the version. -V is a synonym.

    --help
        Print this text. -h is a synonym.
";

/// What `argv[1..]` asks the binary to do — the spec's three-arm
/// surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    /// Bare `ait-gui` — open the window.
    Run,
    /// `--version` / `-V`.
    Version,
    /// `--help` / `-h`.
    Help,
}

/// `argv[1..]` → the [`Command`] to run — the mirror of `ait`'s
/// `parse_args`: anything unrecognized is a usage error naming
/// the offending arguments, never a silent window launch.
fn parse_args(args: &[String]) -> Result<Command, String> {
    match args {
        [] => Ok(Command::Run),
        [arg] => match arg.as_str() {
            "--version" | "-V" => Ok(Command::Version),
            "--help" | "-h" => Ok(Command::Help),
            _ => Err(format!("unknown argument {arg:?}")),
        },
        _ => Err(format!("unrecognized arguments {}", args.join(" "))),
    }
}

fn main() -> ExitCode {
    match parse_args(&env::args().skip(1).collect::<Vec<_>>()) {
        Ok(Command::Run) => match ait_gui::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                ait_gui::report_error(&error);
                ExitCode::FAILURE
            }
        },
        Ok(Command::Version) => {
            let _ = writeln!(io::stdout(), "ait-gui {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Command::Help) => {
            let _ = write!(io::stdout(), "{USAGE}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            let _ = write!(io::stderr(), "ait-gui: {message}\n\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    //! The argv seam (Spec R13 §5.12's CLI-surface rule): `parse_args`
    //! maps the three accepted forms and every other token — including
    //! the `ait`-only forms — is a usage error.
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn version_is_set() {
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }

    /// Bare `ait-gui` runs the window; `--version`/`-V` and
    /// `--help`/`-h` are the only flags.
    #[test]
    fn the_three_forms_parse_to_their_commands() {
        assert_eq!(parse_args(&args(&[])), Ok(Command::Run));
        assert_eq!(parse_args(&args(&["--version"])), Ok(Command::Version));
        assert_eq!(parse_args(&args(&["-V"])), Ok(Command::Version));
        assert_eq!(parse_args(&args(&["--help"])), Ok(Command::Help));
        assert_eq!(parse_args(&args(&["-h"])), Ok(Command::Help));
    }

    /// Everything else is a usage error — the `ait`-only forms
    /// (`--init`, `--print-schema`, `--headless`, `ctl`) included,
    /// never silently launched as a window.
    #[test]
    fn ait_only_forms_and_unknown_args_are_errors() {
        for list in [
            &["--init"][..],
            &["--print-schema"][..],
            &["--headless", "task"][..],
            &["ctl", "open-tab"][..],
            &["gui"][..],
            &["wat"][..],
            &["--version", "--help"][..],
            &["--help", "extra"][..],
        ] {
            assert!(parse_args(&args(list)).is_err(), "{list:?}");
        }
        // The error names the offending argument for the user.
        assert_eq!(
            parse_args(&args(&["wat"])),
            Err("unknown argument \"wat\"".to_string())
        );
    }
}
