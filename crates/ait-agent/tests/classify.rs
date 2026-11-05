//! Table-driven tests at the `classify()` pure seam (Spec R11 §5.10.4,
//! testing decisions — no PTY, no network): the verified input line
//! plus a [`ProgramIndex`] decides `Command` vs `NaturalLanguage`.
//! The rule order is the spec's: (1) empty/single-token → Command;
//! (2) first word resolves in the index → Command; (3) unquoted shell
//! syntax → Command; (4) positive sentence evidence → NaturalLanguage;
//! (5) everything else → Command. The heuristic is conservative:
//! anything a shell could plausibly run stays a command.

use ait_agent::{classify, InputClass, ProgramIndex};

/// The test index: bash's builtin table plus a seeded `$PATH` set —
/// `with_path_entries` is the explicit-construction seam; `harvest`
/// reads the real `$PATH` and is exercised separately below.
fn index() -> ProgramIndex {
    ProgramIndex::with_path_entries(
        "bash",
        ["cargo", "docker", "git", "grep", "ls", "mycli", "rm", "tar"],
    )
}

fn assert_class(cases: &[(&str, InputClass)]) {
    let index = index();
    for (line, want) in cases {
        assert_eq!(classify(line, &index), *want, "classify({line:?})");
    }
}

/// Rule 1: empty and whitespace-only lines, and every single-token
/// line — abbreviations, shell-private aliases, bare builtins — are
/// commands (Spec us-7).
#[test]
fn empty_and_single_token_lines_are_commands() {
    assert_class(&[
        ("", InputClass::Command),
        ("   ", InputClass::Command),
        ("x", InputClass::Command),
        ("myalias", InputClass::Command),
        ("whats", InputClass::Command),
        ("show", InputClass::Command),
    ]);
}

/// Rule 2a: a first word the shell's builtin table resolves is a
/// command however the rest reads — even with sentence-shaped
/// arguments (Spec us-5).
#[test]
fn a_builtin_first_word_is_a_command() {
    assert_class(&[
        ("cd my projects", InputClass::Command),
        ("export EDITOR", InputClass::Command),
        ("set the mode", InputClass::Command),
        ("echo hello there", InputClass::Command),
        // Shell *keywords* resolve through the table too — `for` is a
        // reserved word, so `my` never reaches the evidence rules.
        ("for my files", InputClass::Command),
        // A builtin opener beats sentence evidence — `help` is a bash
        // builtin, so the request-shaped line stays a command.
        ("help me find the config file", InputClass::Command),
    ]);
}

/// Rule 2b: a first word resolving to a `$PATH` basename is a command
/// — `tar my files` is the spec's own near-miss: `my` is sentence
/// evidence, but the first word resolves first (Spec us-5, ac).
#[test]
fn a_path_first_word_is_a_command() {
    assert_class(&[
        ("tar my files", InputClass::Command),
        ("docker run the image", InputClass::Command),
        ("mycli do the thing", InputClass::Command),
        ("cargo test all of them", InputClass::Command),
    ]);
}

/// Rule 3: every unquoted shell-syntax class keeps the line a command
/// (Spec us-6).
#[test]
fn unquoted_shell_syntax_is_a_command() {
    assert_class(&[
        ("unknowncmd | grep x", InputClass::Command),
        ("unknowncmd && ls", InputClass::Command),
        ("unknowncmd || ls", InputClass::Command),
        ("unknowncmd ; ls", InputClass::Command),
        ("unknowncmd > out.txt", InputClass::Command),
        ("unknowncmd < in.txt", InputClass::Command),
        ("cat $(file) now", InputClass::Command),
        ("unknowncmd `date`", InputClass::Command),
        ("FOO=bar unknowncmd", InputClass::Command),
        ("unknowncmd a=b", InputClass::Command),
        ("unknowncmd *.rs", InputClass::Command),
        ("unknowncmd [ab]x", InputClass::Command),
        ("unknowncmd (a)", InputClass::Command),
        ("unknowncmd {a,b}", InputClass::Command),
    ]);
}

/// Rule 3's flag class: `-flag`-shaped tokens anywhere in the line —
/// `myalias --verbose` is the spec's near-miss (Spec us-8).
#[test]
fn flag_shaped_tokens_are_commands() {
    assert_class(&[
        ("myalias --verbose", InputClass::Command),
        ("unknowncmd -v", InputClass::Command),
        ("unknowncmd -xvf archive", InputClass::Command),
        ("-x unknowncmd", InputClass::Command),
    ]);
}

/// Shell syntax inside quotes is literal text — it must not disarm the
/// sentence evidence around it.
#[test]
fn quoted_shell_syntax_is_not_syntax() {
    assert_class(&[
        ("what does \"a | b\" mean", InputClass::NaturalLanguage),
        ("show me 'a ; b' results", InputClass::NaturalLanguage),
    ]);
}

/// Rule 4's leading-word evidence: question and auxiliary openers.
#[test]
fn question_and_auxiliary_openers_are_natural_language() {
    let index = index();
    for line in [
        "how do I undo my last commit",
        "what files are in this directory",
        "where did my file go",
        "which process is using the port",
        "why is the disk full",
        "when was the file changed",
        "can you show me my disk usage",
        "could you list the running containers",
        "should I remove the old files",
        "does the build pass",
        "is the server still running",
    ] {
        assert_eq!(
            classify(line, &index),
            InputClass::NaturalLanguage,
            "classify({line:?})"
        );
    }
}

/// Rule 4's imperative/request openers and the spec's headline example.
/// The openers are words no real `$PATH` harvest resolves — an opener
/// that IS a binary (`find`, `print`, `count`, …) resolves at rule 2
/// on a live system, so it could never exercise the evidence rule.
#[test]
fn request_openers_are_natural_language() {
    assert_class(&[
        ("show me the largest files", InputClass::NaturalLanguage),
        ("list all the processes", InputClass::NaturalLanguage),
        ("tell me the current directory", InputClass::NaturalLanguage),
        ("explain the last error", InputClass::NaturalLanguage),
        ("describe the failing test", InputClass::NaturalLanguage),
        (
            "please delete all the temp files",
            InputClass::NaturalLanguage,
        ),
        ("undo my last commit", InputClass::NaturalLanguage),
        ("summarize the build log", InputClass::NaturalLanguage),
    ]);
}

/// A `'` between word characters is an apostrophe, not a quote open:
/// `what's`/`how's` stay single tokens and normalize onto the
/// `whats`/`hows` openers (previously the apostrophe opened
/// single-quote state and the whole line folded into ONE token —
/// `Command` by rule 1). Quoting elsewhere keeps its POSIX semantics:
/// a real quote still swallows metachars and the line resolves by
/// the unquoted words around it.
#[test]
fn contractions_are_sentence_text_not_quotes() {
    assert_class(&[
        ("what's the time", InputClass::NaturalLanguage),
        ("how's it going", InputClass::NaturalLanguage),
        // A contracted non-opener still gets no evidence — `its`,
        // `dont`, `cant` are in neither table, so the conservative
        // Command default holds.
        ("it's done now", InputClass::Command),
        // Real quoting is untouched: a resolvable first word is a
        // command at rule 2 whatever the quotes carry, and quoted
        // metachars never count as syntax.
        ("echo 'a|b'", InputClass::Command),
        ("foo 'bar|baz'", InputClass::Command),
    ]);
}

/// Rule 4's stopword and trailing-punctuation evidence: a `?`/`.`
/// closer or an English stopword marks a sentence.
#[test]
fn stopwords_and_trailing_punctuation_are_natural_language() {
    assert_class(&[
        ("how do I undo my last commit?", InputClass::NaturalLanguage),
        ("unknowncmd are the files", InputClass::NaturalLanguage),
        ("unknowncmd all of them", InputClass::NaturalLanguage),
        ("frobnicate the widgets", InputClass::NaturalLanguage),
        ("frobnicate my output", InputClass::NaturalLanguage),
        ("please run this.", InputClass::NaturalLanguage),
        ("tell me now?", InputClass::NaturalLanguage),
    ]);
}

/// Rule 5: an unknown first word without any positive sentence
/// evidence stays a command — typos and shell-private aliases behave
/// exactly as today (Spec us-8).
#[test]
fn unknown_words_without_evidence_are_commands() {
    assert_class(&[
        ("foo bar", InputClass::Command),
        ("gti status", InputClass::Command),
        ("myalias verbose", InputClass::Command),
        ("frobnicate widgets", InputClass::Command),
        ("mycustomtool input.txt", InputClass::Command),
        // `..` is a path, not a sentence stop.
        ("uplevel ..", InputClass::Command),
    ]);
}

/// The index itself: builtins come from the shell's table, `$PATH`
/// entries are merged in, and `contains` is exact — `lsd` must not
/// resolve `ls`.
#[test]
fn the_index_merges_builtins_and_path_entries() {
    let index = ProgramIndex::with_path_entries("bash", ["mybin"]);
    assert!(index.contains("cd")); // bash builtin via the POSIX set
    assert!(index.contains("shopt")); // bash extra
    assert!(index.contains("mybin")); // seeded $PATH entry
    assert!(!index.contains("cdx"));
    assert!(!index.contains("zle")); // zsh-only
    assert!(!index.contains("nosuchbin"));
}

/// `harvest` builds the same shape from the real environment —
/// builtins always resolve; a temp-dir `$PATH` contributes its
/// executable basenames and nothing else.
#[test]
fn harvest_merges_builtins_with_path_basenames() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("ait-harvest-testbin");
    std::fs::File::create(&bin)
        .unwrap()
        .write_all(b"#!/bin/sh\n")
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let not_bin = dir.path().join("ait-harvest-notbin");
    std::fs::File::create(&not_bin)
        .unwrap()
        .write_all(b"plain text\n")
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&not_bin, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    let saved = std::env::var_os("PATH");
    std::env::set_var("PATH", dir.path());
    let index = ProgramIndex::harvest("bash");
    match saved {
        Some(path) => std::env::set_var("PATH", path),
        None => std::env::remove_var("PATH"),
    }

    assert!(index.contains("cd"), "builtins always resolve");
    assert!(index.contains("ait-harvest-testbin"));
    // Non-unix treats every regular file as executable — the
    // negative specimen is a unix check.
    #[cfg(unix)]
    assert!(
        !index.contains("ait-harvest-notbin"),
        "non-executables are not commands"
    );
    // A `PATH`-seeded first word flips the verdict.
    assert_eq!(
        classify("ait-harvest-testbin the thing", &index),
        InputClass::Command
    );
}
