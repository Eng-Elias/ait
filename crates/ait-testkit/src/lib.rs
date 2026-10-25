//! `ait-testkit` — AIT's e2e testing harness (Spec E01, ticket E01-T1).
//!
//! Dev-dependency only: no product crate may depend on it. [`E2e`] spawns a
//! real program — the `ait` binary via [`ait_bin!`], or `/bin/sh` in this
//! crate's own self-tests — in a real PTY driven by `ait-term`'s
//! `LiveSession`: named chords, text, raw bytes, SGR mouse events and
//! resizes in; the rendered grid, cursor and exit status out — the same
//! observables a user has (bounded waits only, never fixed sleeps).

mod chord;
mod harness;
mod mouse;
mod tempdir;

pub use ait_term::{Cursor, ExitStatus, GridView, Size};
pub use chord::{parse_chord, ChordError};
pub use harness::{E2e, SpawnConfig};
pub use mouse::{MouseButton, MouseEvent};

/// Resolve the `ait` binary under test (Spec E01): the `AIT_E2E_BIN`
/// environment override first, else the `CARGO_BIN_EXE_ait` cargo exports
/// to the `ait` package's own test targets.
///
/// `CARGO_BIN_EXE_ait` is a *compile-time* variable that only exists in
/// crates whose package builds an `ait` binary — which is why this is a
/// macro: `option_env!` must expand at the call site, in the consuming
/// crate's test target. Expands to an [`std::ffi::OsString`].
///
/// ```
/// # fn demo() {
/// let bin = ait_testkit::ait_bin!();
/// # let _ = bin;
/// # }
/// ```
#[macro_export]
macro_rules! ait_bin {
    () => {{
        ::std::env::var_os("AIT_E2E_BIN").unwrap_or_else(|| {
            ::std::ffi::OsString::from(option_env!("CARGO_BIN_EXE_ait").unwrap_or_else(|| {
                panic!(
                    "no `ait` binary to test: set AIT_E2E_BIN, or run under \
                     `cargo test -p ait` where cargo exports CARGO_BIN_EXE_ait"
                )
            }))
        })
    }};
}
