//! `ait` — AIT's TUI binary (REQUIREMENTS.md §5.2).
//!
//! Bare `ait` opens a full-screen terminal running `$SHELL`; `--init`
//! writes a commented default `settings.toml` and `--print-schema` emits
//! its JSON Schema (Spec R03, user stories 2–5). Everything the binary
//! does is presentation + event routing (CODING_STANDARDS.md §9) — the
//! terminal core lives in `ait-term`, the configuration model in
//! `ait-config`.

mod agent_panel;
mod app;
mod backend;
mod blocks;
mod config;
mod conversations;
mod ctl;
mod inband;
mod integration;
mod keymap;
mod keys;
mod profile;
mod remote;
mod render;
mod scroll;
mod selection;
mod shell_bridge;
mod skills;
mod status;
mod store;
mod suggest;
mod tabs;
mod terminal;
mod transcript;

use std::env;
use std::process::ExitCode;

const USAGE: &str = "ait — AI Terminal

USAGE:
    ait                    Open a full-screen terminal running $SHELL
    ait ctl <verb>         Drive a running ait instance over its
                           control socket:
                               open-tab
                               send [--tab <id>] [--no-enter] <text>
                               last-block [--tab <id>] [--command|--exit]
    ait --init [--force]   Write a commented default settings.toml
                           (refuses to overwrite unless --force)
    ait --print-schema     Print the settings JSON Schema and exit
    ait --version          Print the version and exit
    ait --help             Print this help
";

/// What `argv[1..]` asks the binary to do (Spec R03 `ait` CLI).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    /// Bare `ait` — the TUI.
    Run,
    /// `--version` / `-V`.
    Version,
    /// `--help` / `-h`.
    Help,
    /// `--init` — `force` spells `--init --force` (the overwrite form).
    Init { force: bool },
    /// `--print-schema`.
    PrintSchema,
    /// `ctl <verb>` — drive a running instance over its control
    /// socket (Spec R11 §5.10.5); the verb shape is [`ctl`]'s.
    Ctl(ctl::CtlVerb),
}

/// `argv[1..]` → the [`Command`] to run. `--init`'s `--force` is the
/// only two-flag form; anything unrecognized is a usage error naming
/// the offending argument. A leading `ctl` hands its whole argv tail
/// to [`ctl::parse_ctl_args`] — the subcommand surface this ticket
/// owns (later CLI arms add variants additively, never a second
/// leading-word parser).
fn parse_args(args: &[String]) -> Result<Command, String> {
    if args.first().is_some_and(|arg| arg == "ctl") {
        return ctl::parse_ctl_args(&args[1..]).map(Command::Ctl);
    }
    match args {
        [] => Ok(Command::Run),
        [arg] => match arg.as_str() {
            "--version" | "-V" => Ok(Command::Version),
            "--help" | "-h" => Ok(Command::Help),
            "--init" => Ok(Command::Init { force: false }),
            "--print-schema" => Ok(Command::PrintSchema),
            _ => Err(format!("unknown argument {arg:?}")),
        },
        [first, second]
            if (first == "--init" && second == "--force")
                || (first == "--force" && second == "--init") =>
        {
            Ok(Command::Init { force: true })
        }
        _ => Err(format!("unrecognized arguments {}", args.join(" "))),
    }
}

fn main() -> ExitCode {
    match parse_args(&env::args().skip(1).collect::<Vec<_>>()) {
        Ok(Command::Run) => match app::run() {
            Ok(code) => code,
            Err(error) => {
                // The terminal guard has already restored the host
                // terminal by the time an error reaches here.
                eprintln!("ait: {error}");
                ExitCode::FAILURE
            }
        },
        Ok(Command::Version) => {
            println!("ait {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Command::Help) => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        // Thin shells over ait-config (standard 9): the crate owns the
        // template text, path, exists-check and schema; main maps the
        // outcome onto exit codes and stderr.
        Ok(Command::Init { force }) => {
            let path = ait_config::settings_path();
            match ait_config::init_settings(&path, force) {
                Ok(()) => {
                    println!("wrote {}", path.display());
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("ait: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        Ok(Command::PrintSchema) => {
            println!("{}", ait_config::json_schema());
            ExitCode::SUCCESS
        }
        // The ctl client arm (Spec R11): resolve the instance's
        // socket, exchange one request, print the verb's answer —
        // `run_client` owns stdout/stderr and the exit code.
        Ok(Command::Ctl(verb)) => ctl::run_client(&verb),
        Err(message) => {
            eprintln!("ait: {message}\n\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    //! The argv seam (Spec R03 `ait` CLI): `parse_args` turns `argv[1..]`
    //! into a [`Command`] — `--init`'s `--force` is the only two-flag
    //! form, and every unrecognized token is a usage error.
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn version_is_set() {
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }

    /// Bare `ait` runs the TUI; each known flag maps to its command.
    #[test]
    fn known_forms_parse_to_their_commands() {
        assert_eq!(parse_args(&args(&[])), Ok(Command::Run));
        assert_eq!(parse_args(&args(&["--version"])), Ok(Command::Version));
        assert_eq!(parse_args(&args(&["-V"])), Ok(Command::Version));
        assert_eq!(parse_args(&args(&["--help"])), Ok(Command::Help));
        assert_eq!(parse_args(&args(&["-h"])), Ok(Command::Help));
        assert_eq!(
            parse_args(&args(&["--init"])),
            Ok(Command::Init { force: false })
        );
        assert_eq!(
            parse_args(&args(&["--print-schema"])),
            Ok(Command::PrintSchema)
        );
    }

    /// `--init --force` in either order is the overwrite form (story 3).
    #[test]
    fn init_force_parses_in_both_orders() {
        for list in [&["--init", "--force"][..], &["--force", "--init"][..]] {
            assert_eq!(
                parse_args(&args(list)),
                Ok(Command::Init { force: true }),
                "{list:?}"
            );
        }
    }

    /// A stray `--force`, unknown flags and extra tokens are usage
    /// errors, never silently run-the-TUI.
    #[test]
    fn unrecognized_forms_are_errors() {
        for list in [
            &["--force"][..],
            &["--init", "--bogus"][..],
            &["--version", "--init"][..],
            &["wat"][..],
            &["--init", "--force", "extra"][..],
        ] {
            assert!(parse_args(&args(list)).is_err(), "{list:?}");
        }
        // The error names the offending argument for the user.
        assert_eq!(
            parse_args(&args(&["wat"])),
            Err("unknown argument \"wat\"".to_string())
        );
    }

    /// `ctl`'s argv tail lands on the client verbs (Spec R11 §5.10.5)
    /// — and its own malformed shapes stay usage errors, never a
    /// silent TUI launch.
    #[test]
    fn ctl_verbs_parse_to_their_commands() {
        assert_eq!(
            parse_args(&args(&["ctl", "open-tab"])),
            Ok(Command::Ctl(ctl::CtlVerb::OpenTab))
        );
        assert_eq!(
            parse_args(&args(&["ctl", "send", "--no-enter", "echo hi"])),
            Ok(Command::Ctl(ctl::CtlVerb::Send {
                tab: None,
                enter: false,
                text: "echo hi".to_owned(),
            }))
        );
        assert_eq!(
            parse_args(&args(&["ctl", "last-block", "--tab", "2", "--exit"])),
            Ok(Command::Ctl(ctl::CtlVerb::LastBlock {
                tab: Some("2".to_owned()),
                select: ctl::LastBlockSelect::Exit,
            }))
        );
        for list in [
            &["ctl"][..],
            &["ctl", "bogus"][..],
            &["ctl", "send"][..],
            &["ctl", "last-block", "--command", "--exit"][..],
        ] {
            assert!(parse_args(&args(list)).is_err(), "{list:?}");
        }
    }
}
