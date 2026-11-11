//! `ait-gui` — the standalone GUI binary (Spec R13 §5.12): a thin
//! shell over [`ait_gui::run`]. The `ait gui` subcommand (T7 wiring)
//! reaches the same entry point.

use std::process::ExitCode;

fn main() -> ExitCode {
    match ait_gui::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            ait_gui::report_error(&error);
            ExitCode::FAILURE
        }
    }
}
