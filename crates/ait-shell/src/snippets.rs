//! The rc snippet assets — one per supported shell, authored in-house
//! (CODING_STANDARDS.md 10) and embedded verbatim. The injection
//! planner ([`crate::Injection`]) materializes them into the per-session
//! `0700` dir; each emits OSC 133 `A`/`B`/`C`/`D`, OSC 7, all five
//! OSC 7700 hooks, and defines `__ait_run`/`__ait_emit` (§5.5, §5.5.1).

/// bash integration snippet, sourced via the `--rcfile` wrapper.
pub const BASH: &str = include_str!("snippets/bash.sh");

/// zsh integration snippet, sourced via the temp-`ZDOTDIR` `.zshrc`.
pub const ZSH: &str = include_str!("snippets/zsh.zsh");

/// fish integration snippet, sourced via `--init-command`.
pub const FISH: &str = include_str!("snippets/fish.fish");
