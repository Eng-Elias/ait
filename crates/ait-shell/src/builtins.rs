//! Per-shell builtin tables for the §5.10.4 input classifier
//! (Spec R11-T1).
//!
//! The classifier's first-word index merges the detected shell's
//! builtin table with executable basenames harvested from `$PATH`
//! (`ait_agent::ProgramIndex`). The tables here cover the words each
//! shell interprets *itself* — builtins plus reserved keywords — so a
//! `for`-loop opener or a `shopt` call stays a command however the
//! rest of the line reads. Shell-private aliases and functions are
//! unknowable; the conservative rules keep them on the command path.
//!
//! Everything is keyed by the session program's basename — the same
//! string the status line shows — and an unrecognized shell resolves
//! the shared POSIX set alone (§5.5 graceful degrade).

/// The shared POSIX set: special builtins, the regular builtins every
/// supported shell provides, and the POSIX reserved words. `sh` gets
/// this alone; bash, zsh and fish get it plus their extras (fish's
/// grammar differs, but the POSIX *builtin* words — `cd`, `set`,
/// `read`, `test`, … — are all fish builtins too; a stray POSIX
/// keyword merely classifies a syntax error conservatively).
pub const POSIX: &[&str] = &[
    // POSIX special builtins.
    ".", ":", "break", "continue", "eval", "exec", "exit", "export", "readonly", "return", "set",
    "shift", "times", "trap", "unset",
    // Regular builtins common to the supported shells.
    "[", "alias", "bg", "cd", "command", "echo", "false", "fc", "fg", "getopts", "hash", "jobs",
    "kill", "printf", "pwd", "read", "test", "true", "type", "ulimit", "umask", "unalias", "wait",
    // POSIX reserved words.
    "!", "case", "do", "done", "elif", "else", "esac", "fi", "for", "if", "in", "then", "until",
    "while",
];

/// bash extras on top of [`POSIX`] — builtins and keywords POSIX
/// doesn't name.
pub const BASH: &[&str] = &[
    "[[",
    "]]",
    "bind",
    "builtin",
    "caller",
    "compgen",
    "complete",
    "compopt",
    "coproc",
    "declare",
    "dirs",
    "disown",
    "enable",
    "function",
    "help",
    "history",
    "let",
    "local",
    "logout",
    "mapfile",
    "popd",
    "pushd",
    "readarray",
    "select",
    "shopt",
    "source",
    "suspend",
    "time",
    "typeset",
];

/// zsh extras on top of [`POSIX`].
pub const ZSH: &[&str] = &[
    "[[",
    "]]",
    "autoload",
    "bindkey",
    "builtin",
    "bye",
    "compadd",
    "compdef",
    "compinit",
    "declare",
    "dirs",
    "disable",
    "disown",
    "emulate",
    "enable",
    "float",
    "foreach",
    "function",
    "functions",
    "history",
    "integer",
    "local",
    "logout",
    "popd",
    "print",
    "pushd",
    "rehash",
    "repeat",
    "sched",
    "select",
    "setopt",
    "source",
    "suspend",
    "time",
    "typeset",
    "unfunction",
    "unsetopt",
    "vared",
    "whence",
    "where",
    "which",
    "zle",
    "zmodload",
    "zstyle",
];

/// fish extras on top of [`POSIX`] — its keyword grammar (`begin`/
/// `end`/`switch`, `and`/`or`/`not` in place of `&&`/`||`/`!`) and its
/// own builtin surface.
pub const FISH: &[&str] = &[
    "abbr",
    "and",
    "argparse",
    "begin",
    "bind",
    "block",
    "breakpoint",
    "builtin",
    "commandline",
    "contains",
    "count",
    "disown",
    "emit",
    "end",
    "function",
    "functions",
    "history",
    "math",
    "not",
    "or",
    "random",
    "realpath",
    "set_color",
    "source",
    "status",
    "string",
    "switch",
    "vared",
];

/// Whether `shell_name` is a shell the §5.10.4 classifier may arm
/// on: the family whose line editing wipes the input on `Ctrl+U`
/// (`\x15`, kill-to-start-of-line) and reads `# ` as a comment —
/// what `rewrite_as_comment`'s NL rewrite relies on. A REPL or any
/// other program that merely echoes input fails this check, so the
/// trigger degrades to a verbatim submission instead of appending
/// `# <line>` onto an un-wiped line (the same desync-is-inert rule
/// unverifiable lines already get). Deliberately the narrow list —
/// a missed shell only ever costs the rewrite, never corrupts input.
pub fn is_known_shell(shell_name: &str) -> bool {
    matches!(shell_name, "bash" | "zsh" | "fish" | "sh" | "dash")
}

/// The words `shell_name` interprets itself: the shared [`POSIX`] set
/// chained with the shell's extras. `shell_name` is the session
/// program's basename (`bash`, `zsh`, `fish`, …) — the same string
/// the status line shows. An unrecognized shell resolves the POSIX
/// set alone.
pub fn builtins_for(shell_name: &str) -> impl Iterator<Item = &'static str> {
    let extras: &[&str] = match shell_name {
        "bash" => BASH,
        "zsh" => ZSH,
        "fish" => FISH,
        _ => &[],
    };
    POSIX.iter().chain(extras.iter()).copied()
}
