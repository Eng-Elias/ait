//! Tests at the [`builtins_for`] seam (Spec R11 §5.10.4, standard 11 —
//! pure, no PTY): the classifier's first-word index is built from the
//! detected shell's builtin table — the shared POSIX set plus that
//! shell's extras, keyed by the session program's basename (the same
//! string the status line shows).

use ait_shell::builtins_for;

fn contains(shell: &str, name: &str) -> bool {
    builtins_for(shell).any(|b| b == name)
}

/// Every supported shell — and every unrecognized one — resolves the
/// shared POSIX set: the builtins and reserved words a POSIX shell
/// interprets itself (§5.5 graceful degrade: unknown shells still get
/// the shared vocabulary, never an empty table).
#[test]
fn every_shell_gets_the_shared_posix_set() {
    for shell in ["bash", "zsh", "fish", "sh", "dash", "nu"] {
        for name in [
            ".", ":", "[", "alias", "bg", "break", "case", "cd", "command", "continue", "do",
            "done", "echo", "elif", "else", "esac", "eval", "exec", "exit", "export", "false",
            "fc", "fg", "fi", "for", "getopts", "hash", "if", "in", "jobs", "kill", "printf",
            "pwd", "read", "readonly", "return", "set", "shift", "test", "then", "times", "trap",
            "true", "type", "ulimit", "umask", "unalias", "unset", "until", "wait", "while",
        ] {
            assert!(contains(shell, name), "{shell} missing POSIX word {name:?}");
        }
    }
}

/// bash's own vocabulary rides on top of the POSIX set.
#[test]
fn bash_gets_its_extras() {
    for name in [
        "[[",
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
    ] {
        assert!(contains("bash", name), "bash missing builtin {name:?}");
    }
    // zsh-only words stay out of the bash table (standard 18 — a
    // table that over-matches ships a near-miss specimen).
    for name in ["zle", "setopt", "autoload"] {
        assert!(!contains("bash", name), "bash should not resolve {name:?}");
    }
}

/// zsh's own vocabulary rides on top of the POSIX set.
#[test]
fn zsh_gets_its_extras() {
    for name in [
        "[[",
        "autoload",
        "bindkey",
        "compadd",
        "compdef",
        "compinit",
        "declare",
        "dirs",
        "disown",
        "emulate",
        "float",
        "foreach",
        "function",
        "functions",
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
        "zle",
        "zmodload",
        "zstyle",
    ] {
        assert!(contains("zsh", name), "zsh missing builtin {name:?}");
    }
    for name in ["shopt", "mapfile", "argparse"] {
        assert!(!contains("zsh", name), "zsh should not resolve {name:?}");
    }
}

/// fish's vocabulary is its own — it shares much of the POSIX builtin
/// surface (`cd`, `set`, `read`, `test`, …) but none of the POSIX
/// keyword grammar (`end` closes blocks; `and`/`or`/`not` are
/// builtins, not `&&`/`||`/`!`).
#[test]
fn fish_gets_its_extras() {
    for name in [
        "abbr",
        "and",
        "argparse",
        "begin",
        "bind",
        "block",
        "breakpoint",
        "builtin",
        "case",
        "commandline",
        "contains",
        "count",
        "disown",
        "else",
        "emit",
        "end",
        "for",
        "function",
        "functions",
        "history",
        "if",
        "jobs",
        "math",
        "not",
        "or",
        "random",
        "read",
        "realpath",
        "set",
        "set_color",
        "source",
        "status",
        "string",
        "switch",
        "vared",
        "wait",
        "while",
    ] {
        assert!(contains("fish", name), "fish missing builtin {name:?}");
    }
    for name in ["shopt", "zle", "declare"] {
        assert!(!contains("fish", name), "fish should not resolve {name:?}");
    }
}

/// Tables key on the session program's basename — an unknown shell
/// resolves the shared POSIX set alone, never the empty set and never
/// another shell's extras.
#[test]
fn an_unknown_shell_gets_the_posix_set_alone() {
    assert!(contains("nushell", "cd"));
    assert!(contains("", "cd"));
    for name in ["zle", "shopt", "argparse"] {
        assert!(
            !contains("nushell", name),
            "nushell should not resolve {name:?}"
        );
    }
}
