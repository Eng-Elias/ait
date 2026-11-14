//! Tests at the [`builtins_for`] seam (Spec R11 §5.10.4, standard 11 —
//! pure, no PTY): the classifier's first-word index is built from the
//! detected shell's builtin table — the shared POSIX set plus that
//! shell's extras, keyed by the session program's basename (the same
//! string the status line shows).

use ait_shell::{builtins_for, shell_family, ShellFamily};

fn contains(shell: &str, name: &str) -> bool {
    builtins_for(shell).any(|b| b == name)
}

/// §5.14's shell-family key — the one normalization the injection
/// planner, the classifier tables and the executor's Absent-tier
/// sentinel share (std 17): `pwsh`/`powershell` and `cmd` match
/// case-insensitively with one trailing `.exe` stripped; every POSIX
/// shell and every unrecognized name is `Posix` (the degrade).
#[test]
fn shell_family_normalizes_the_windows_names() {
    for name in [
        "pwsh",
        "powershell",
        "pwsh.exe",
        "PWSH.EXE",
        "powershell.exe",
        "PowerShell.EXE",
    ] {
        assert_eq!(shell_family(name), ShellFamily::PowerShell, "{name}");
    }
    for name in ["cmd", "cmd.exe", "CMD.EXE"] {
        assert_eq!(shell_family(name), ShellFamily::Cmd, "{name}");
    }
    for name in [
        "bash",
        "zsh",
        "fish",
        "sh",
        "dash",
        "nu",
        "",
        "pwsh.bat",
        "powershell7",
        "pwsh.exe.exe",
    ] {
        assert_eq!(shell_family(name), ShellFamily::Posix, "{name}");
    }
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

/// Both PowerShell hosts — and their `.exe`/case spellings — resolve
/// the same extras (§5.14): the language keywords plus the common
/// approved-verb cmdlets and aliases, so PowerShell syntax reads as
/// commands to the §5.10.4 classifier. The table is lowercase; the
/// case-insensitive lookup is the index's job.
#[test]
fn powershell_gets_its_extras() {
    for shell in [
        "pwsh",
        "powershell",
        "pwsh.exe",
        "PWSH.EXE",
        "powershell.exe",
    ] {
        for name in [
            // Keywords and statement words.
            "begin",
            "class",
            "catch",
            "dynamicparam",
            "end",
            "enum",
            "finally",
            "foreach",
            "function",
            "param",
            "process",
            "switch",
            "throw",
            "trap",
            "try",
            // Representative cmdlets across the approved verbs.
            "get-childitem",
            "get-content",
            "get-process",
            "set-location",
            "new-item",
            "copy-item",
            "move-item",
            "remove-item",
            "test-path",
            "write-output",
            "write-host",
            "read-host",
            "invoke-command",
            "invoke-expression",
            "start-process",
            "stop-process",
            "select-object",
            "where-object",
            "foreach-object",
            "sort-object",
            "measure-object",
            "out-file",
            "export-csv",
            "convertto-json",
            "convertfrom-json",
            "join-path",
            "resolve-path",
            "split-path",
            // Standard aliases.
            "gci",
            "ls",
            "ps",
            "iex",
            "irm",
            "iwr",
            "sasv",
            "spps",
        ] {
            assert!(
                contains(shell, name),
                "{shell} missing PowerShell word {name:?}"
            );
        }
        // POSIX shared set still applies — `cd`/`echo`/`set` are
        // real PowerShell commands too, and a stray POSIX word only
        // errs toward Command.
        assert!(contains(shell, "cd"), "{shell} keeps the POSIX base");
    }
    // POSIX shells must not see the PowerShell vocabulary.
    for name in ["get-childitem", "param", "foreach-object", "dynamicparam"] {
        assert!(!contains("bash", name), "bash should not resolve {name:?}");
    }
}

/// `cmd.exe` resolves the minimal builtin set (§5.14) — the words it
/// interprets itself stay commands however the rest of the line
/// reads — on the same normalized basename rule as PowerShell.
#[test]
fn cmd_gets_its_minimal_builtin_set() {
    for shell in ["cmd", "cmd.exe", "CMD.EXE"] {
        for name in [
            "call", "cd", "chdir", "cls", "copy", "del", "dir", "echo", "endlocal", "exit", "for",
            "goto", "if", "md", "mkdir", "move", "pause", "popd", "pushd", "rem", "ren", "rmdir",
            "set", "setlocal", "shift", "start", "title", "type", "ver", "vol", "xcopy",
        ] {
            assert!(
                contains(shell, name),
                "{shell} missing cmd builtin {name:?}"
            );
        }
    }
    // PowerShell vocabulary stays out of cmd's table.
    for name in ["get-childitem", "param", "foreach-object"] {
        assert!(!contains("cmd", name), "cmd should not resolve {name:?}");
    }
}

/// Windows spellings of the POSIX shells normalize too — Git-for-
/// Windows `bash.exe` classifies against the bash table; a stray
/// case/extension quirk errs toward Command, the safe direction.
#[test]
fn windows_spellings_of_the_posix_shells_normalize() {
    assert!(contains("bash.exe", "shopt"));
    assert!(contains("ZSH.EXE", "setopt"));
    assert!(contains("Fish.Exe", "argparse"));
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
