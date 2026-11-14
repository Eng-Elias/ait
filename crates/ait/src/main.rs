//! `ait` — AIT's TUI binary (REQUIREMENTS.md §5.2).
//!
//! Bare `ait` opens a full-screen terminal running the user's shell;
//! `--init`
//! writes a commented default `settings.toml` and `--print-schema` emits
//! its JSON Schema (Spec R03, user stories 2–5). Everything the binary
//! does is presentation + event routing (CODING_STANDARDS.md §9) — the
//! terminal core lives in `ait-term`, the configuration model in
//! `ait-config`, and the frontend-agnostic session model in `ait-app`
//! (Spec R13 §5.12): the modules here are only the TUI loop, the
//! crossterm/ratatui paint and the host-terminal plumbing.

mod agent_panel;
mod app;
mod backend;
mod ctl;
mod keys;
mod render;
mod terminal;

use std::env;
use std::process::ExitCode;

use ait_app::headless;

/// `--help`'s text and the tail of every usage error (Spec R12
/// §5.11, us-13–16): man-page layout — NAME/SYNOPSIS/DESCRIPTION/
/// OPTIONS — covering every arm [`parse_args`] accepts, including
/// headless's stdout/stderr/exit-code contract. Documentation only;
/// the accepted forms are the parser's.
const USAGE: &str = "NAME
    ait — AI terminal: your shell in a full-screen TUI, with an
    agent that sees and drives it

SYNOPSIS
    ait
    ait gui
    ait ctl open-tab
    ait ctl send [--tab <id>] [--no-enter] <text>
    ait ctl last-block [--tab <id>] [--command|--exit]
    ait --headless \"<task>\" [--profile <name>]
    ait --init [--force]
    ait --print-schema
    ait --version
    ait --help

DESCRIPTION
    Bare ait opens a full-screen terminal running your shell — tabs,
    command blocks, an agent sidebar. Every other form performs
    one action and exits. An unrecognized argument is a usage
    error: the message and this text land on stderr and the exit
    code is non-zero.

OPTIONS
    gui, --gui
        Launch the GUI frontend (Spec R13 §5.12) in-process.
        Only `--features gui` builds carry it — other builds
        exit non-zero naming the gap (use `ait-gui` instead).

    ctl <verb>
        Drive a running instance over its control socket (unix
        only): $AIT_CTL_SOCKET when set, else the one live
        instance's socket — spawned shells inherit the variable,
        so `ait ctl` inside a tab drives its own instance. Verb
        answers go to stdout; failures exit non-zero with the
        message on stderr.

        open-tab
            Open a tab and print the response's JSON line — it
            carries the new tab's session id.

        send [--tab <id>] [--no-enter] <text>
            Write <text> into the tab's shell input and submit
            it; --no-enter leaves the text on the input line
            instead. <id> is the tab's AIT_SESSION_ID — unset
            targets the active tab.

        last-block [--tab <id>] [--command|--exit]
            Print the newest sealed block's output; --command or
            --exit prints that field instead (the two can't
            combine).

    --headless \"<task>\" [--profile <name>]
        Run one agent task non-interactively: the answer's text
        streams to stdout (pipe-safe) while tool and status
        activity goes to stderr; approval gates are auto-denied
        and questions get a canned no-interactive answer.
        --profile names an [agents.execution_profiles.*] entry —
        unset arms the launch default. Exit codes: 0 done, 1
        error (startup failures — bad settings, unknown profile,
        no model — and mid-run errors alike), 2 step budget
        reached, 130 interrupted by SIGINT (the headless Esc).

    --init [--force]
        Write a commented default settings.toml under
        $XDG_CONFIG_HOME/ait/ (%APPDATA%\\ait on Windows); an
        existing file is kept unless --force (either order works).

    --print-schema
        Print the settings JSON Schema.

    --version
        Print the version. -V is a synonym.

    --help
        Print this text. -h is a synonym.
";

/// What `argv[1..]` asks the binary to do (Spec R03 `ait` CLI).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    /// Bare `ait` — the TUI.
    Run,
    /// `gui` / `--gui` — the GUI frontend in-process (Spec R13
    /// §5.12), when the build opted into the `gui` feature.
    Gui,
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
    /// `--headless "<task>" [--profile <name>]` — the non-interactive
    /// agent run (Spec R11 §5.10.5).
    Headless {
        /// The task text, verbatim.
        task: String,
        /// The `--profile <name>` selection — `None` arms the
        /// launch default.
        profile: Option<String>,
    },
}

/// `argv[1..]` → the [`Command`] to run. `--init`'s `--force` and
/// `--headless`'s `--profile` are the only flag forms; anything
/// unrecognized is a usage error naming the offending argument. A
/// leading `ctl` hands its whole argv tail to
/// [`ctl::parse_ctl_args`] — the subcommand surface R11-T3 owns
/// (later CLI arms add variants additively, never a second
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
            "gui" | "--gui" => Ok(Command::Gui),
            "--init" => Ok(Command::Init { force: false }),
            "--print-schema" => Ok(Command::PrintSchema),
            "--headless" => Err("the --headless form needs a task".to_string()),
            _ => Err(format!("unknown argument {arg:?}")),
        },
        [first, second]
            if (first == "--init" && second == "--force")
                || (first == "--force" && second == "--init") =>
        {
            Ok(Command::Init { force: true })
        }
        [first, task] if first == "--headless" => Ok(Command::Headless {
            task: task.clone(),
            profile: None,
        }),
        [first, task, flag, name] if first == "--headless" && flag == "--profile" => {
            Ok(Command::Headless {
                task: task.clone(),
                profile: Some(name.clone()),
            })
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
        // The GUI frontend (Spec R13 §5.12): `ait gui`/`--gui`
        // reaches `ait_gui::run` in-process in builds that opted
        // into the `gui` feature; every other build answers with a
        // clear error — the parse is unconditional so the gap
        // surfaces as a message, not an "unknown argument".
        Ok(Command::Gui) => run_gui(),
        // The non-interactive agent run (Spec R11 §5.10.5) — the
        // driver owns settings load, the hidden shell and the
        // stdout/stderr/exit-code contract.
        Ok(Command::Headless { task, profile }) => headless::run(&task, profile.as_deref()),
        Err(message) => {
            eprintln!("ait: {message}\n\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

/// `ait gui`/`--gui` with the `gui` feature: launch the GUI
/// frontend in-process — the same [`ait_gui::run`] the `ait-gui`
/// binary calls.
#[cfg(feature = "gui")]
fn run_gui() -> ExitCode {
    match ait_gui::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ait: {error}");
            ExitCode::FAILURE
        }
    }
}

/// `ait gui`/`--gui` without the feature (the default and every
/// release build): fail helpfully — the packaged GUI is the
/// `ait-gui` binary.
#[cfg(not(feature = "gui"))]
fn run_gui() -> ExitCode {
    eprintln!(
        "ait: this build has no GUI frontend — `ait gui` requires a build \
         with `--features gui`; the packaged GUI is the `ait-gui` binary"
    );
    ExitCode::FAILURE
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

    /// Spec R13 §5.12: `ait gui` and `ait --gui` both select the GUI
    /// frontend — the parse is unconditional (a feature-less build
    /// still parses it, then fails at dispatch naming the gap), and
    /// trailing arguments stay usage errors.
    #[test]
    fn gui_forms_parse_to_the_gui_command() {
        assert_eq!(parse_args(&args(&["gui"])), Ok(Command::Gui));
        assert_eq!(parse_args(&args(&["--gui"])), Ok(Command::Gui));
        for list in [&["gui", "extra"][..], &["--gui", "--verbose"][..]] {
            assert!(parse_args(&args(list)).is_err(), "{list:?}");
        }
    }

    /// Spec R11 §5.10.5: `--headless "<task>"` carries the task
    /// verbatim; `--profile <name>` selects the profile; the task
    /// itself may look like a flag (it is a value, not parsed).
    #[test]
    fn headless_parses_task_and_optional_profile() {
        assert_eq!(
            parse_args(&args(&["--headless", "fix the tests"])),
            Ok(Command::Headless {
                task: "fix the tests".to_string(),
                profile: None,
            })
        );
        assert_eq!(
            parse_args(&args(&["--headless", "fix the tests", "--profile", "work"])),
            Ok(Command::Headless {
                task: "fix the tests".to_string(),
                profile: Some("work".to_string()),
            })
        );
        // A flag-shaped task stays verbatim.
        assert_eq!(
            parse_args(&args(&["--headless", "--verbose"])),
            Ok(Command::Headless {
                task: "--verbose".to_string(),
                profile: None,
            })
        );
    }

    /// The headless forms' failure modes: a bare `--headless` names
    /// the missing task, a trailing `--profile` with no name or an
    /// unrecognized flag is a usage error.
    #[test]
    fn headless_malformed_forms_are_errors() {
        assert_eq!(
            parse_args(&args(&["--headless"])),
            Err("the --headless form needs a task".to_string())
        );
        for list in [
            &["--headless", "task", "--profile"][..],
            &["--headless", "task", "--bogus", "name"][..],
            &["--headless", "task", "--profile", "name", "extra"][..],
        ] {
            assert!(parse_args(&args(list)).is_err(), "{list:?}");
        }
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
