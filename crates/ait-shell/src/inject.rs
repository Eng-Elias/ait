//! The injection planner (REQUIREMENTS.md §5.5, Spec R05 / issue #71).
//!
//! [`plan`] maps a *resolved* program path to an [`Injection`] purely on
//! the basename — `bash`, `zsh` and `fish` get argv/env rewrites plus the
//! files a per-session `0700` dir must hold; every other program plans
//! to `None` (§5.5 graceful degrade: unknown shells get nothing, the
//! terminal keeps working).
//!
//! The planner never touches the user's dotfiles: bash gets a `--rcfile`
//! wrapper that sources `~/.bashrc` first, zsh gets a temp `ZDOTDIR`
//! whose forwarders source the real files from the original `ZDOTDIR`
//! (then restore it so nested zsh behaves), and fish gets
//! `--init-command 'source <snippet>'` which runs after its own config.
//!
//! [`Injection::materialize_in`] writes the files into a fresh `0700`
//! session dir and returns a [`Materialized`] guard that removes it on
//! drop — integration leaves no litter (Spec R05 us-30).

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::snippets;

/// Env var carrying the session id every hook payload echoes (§5.5).
/// Allocating it is the caller's job (the spawn ticket) — the planner
/// only reserves the slot via [`Materialized::set_session_id`].
pub const ENV_SESSION_ID: &str = "AIT_SESSION_ID";

/// Env var passing the user's *original* `ZDOTDIR` into the temp-dir
/// forwarders — the spawned zsh's own `ZDOTDIR` already points at us.
const ENV_ZDOTDIR_ORIG: &str = "AIT_ZDOTDIR_ORIG";

/// The shell family an [`Injection`] targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    /// bash — injected via `--rcfile <wrapper>`.
    Bash,
    /// zsh — injected via a temp `ZDOTDIR` forwarder dir.
    Zsh,
    /// fish — injected via `--init-command 'source <snippet>'`.
    Fish,
}

/// A planned integration for one supported shell — pure data until
/// [`Injection::materialize_in`] writes it out. All accessors take the
/// session `dir` the files will live in so the rewrite is fully
/// inspectable before (and without) touching the filesystem.
#[derive(Debug)]
pub struct Injection {
    kind: ShellKind,
    /// The user's original `ZDOTDIR` at plan time (zsh only).
    orig_zdotdir: Option<OsString>,
}

/// Plan shell integration for a resolved program path. Returns `None`
/// for any basename that isn't bash, zsh or fish — `sh`, `nu`, `sudo -i`
/// and dumb remotes all degrade to a plain terminal (§5.5).
pub fn plan(resolved_program: &Path) -> Option<Injection> {
    plan_with_zdotdir(resolved_program, std::env::var_os("ZDOTDIR"))
}

/// [`plan`] with the user's original `ZDOTDIR` passed explicitly — the
/// seam tests drive, and what a caller uses when it captured the
/// environment earlier. Only meaningful for zsh plans.
pub fn plan_with_zdotdir(
    resolved_program: &Path,
    orig_zdotdir: Option<OsString>,
) -> Option<Injection> {
    let basename = resolved_program.file_name()?.to_str()?;
    let kind = match basename {
        "bash" => ShellKind::Bash,
        "zsh" => ShellKind::Zsh,
        "fish" => ShellKind::Fish,
        _ => return None,
    };
    // An empty ZDOTDIR means `$HOME` in practice — record it as absent.
    let orig_zdotdir = orig_zdotdir.filter(|v| !v.is_empty());
    Some(Injection { kind, orig_zdotdir })
}

impl Injection {
    /// Which shell family this injection targets.
    pub fn kind(&self) -> ShellKind {
        self.kind
    }

    /// The files the session `dir` must hold: `(relative name, rendered
    /// contents)` pairs. Wrapper contents interpolate `dir` so they can
    /// source the embedded snippet by absolute path.
    pub fn files(&self, dir: &Path) -> Vec<(&'static str, String)> {
        match self.kind {
            ShellKind::Bash => vec![
                ("bashrc", self.bashrc_wrapper(dir)),
                ("snippet.bash", snippets::BASH.to_owned()),
            ],
            ShellKind::Zsh => vec![
                (".zshenv", zsh_forwarder(".zshenv")),
                (".zprofile", zsh_forwarder(".zprofile")),
                (".zshrc", self.zshrc_wrapper(dir)),
                ("snippet.zsh", snippets::ZSH.to_owned()),
            ],
            ShellKind::Fish => vec![("snippet.fish", snippets::FISH.to_owned())],
        }
    }

    /// Args to prepend to the spawned program's existing args.
    pub fn args(&self, dir: &Path) -> Vec<OsString> {
        match self.kind {
            ShellKind::Bash => vec![
                OsString::from("--rcfile"),
                dir.join("bashrc").into_os_string(),
            ],
            // zsh needs no argv — the ZDOTDIR env forwarder does it all.
            ShellKind::Zsh => Vec::new(),
            // fish's `--init-command` runs after its own config loads.
            ShellKind::Fish => vec![
                OsString::from("--init-command"),
                OsString::from(format!(
                    "source {}",
                    fish_single_quote(&dir.join("snippet.fish").to_string_lossy())
                )),
            ],
        }
    }

    /// Env vars to layer onto the spawned shell. Always contains an
    /// empty `AIT_SESSION_ID` so a session id inherited from an outer
    /// integrated shell can never leak into this one — the caller fills
    /// it via [`Materialized::set_session_id`].
    pub fn env(&self, dir: &Path) -> Vec<(OsString, OsString)> {
        let mut env = vec![(OsString::from(ENV_SESSION_ID), OsString::new())];
        if self.kind == ShellKind::Zsh {
            env.push((OsString::from("ZDOTDIR"), dir.as_os_str().to_os_string()));
            if let Some(orig) = &self.orig_zdotdir {
                env.push((OsString::from(ENV_ZDOTDIR_ORIG), orig.clone()));
            }
        }
        // fish ≥4 ships built-in OSC 133 marking (`A;click_events=1`,
        // `C;cmdline_url=…`): its mid-cycle `A` abandons our open block
        // before `D` lands, so an integrated fish ≥4 renders no blocks.
        // `mark-prompt` is a feature flag read once at fish startup —
        // only an env import reaches it; the snippet itself is too late.
        if self.kind == ShellKind::Fish {
            env.push((
                OsString::from("fish_features"),
                OsString::from("no-mark-prompt"),
            ));
        }
        env
    }

    /// Write this plan into a fresh `0700` session dir under the system
    /// temp dir and return the live bundle.
    pub fn materialize(&self) -> io::Result<Materialized> {
        self.materialize_in(&std::env::temp_dir())
    }

    /// Write this plan into a fresh `0700` session dir under `base` and
    /// return the live bundle. The name is unique per attempt
    /// (pid + counter + clock) so concurrent sessions never collide.
    pub fn materialize_in(&self, base: &Path) -> io::Result<Materialized> {
        fs::create_dir_all(base)?;
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        for _ in 0..16 {
            let tick = COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = base.join(format!("ait-shell-{}-{tick}-{nanos:x}", std::process::id()));
            match fs::create_dir(&dir) {
                Ok(()) => {
                    set_private_dir_permissions(&dir)?;
                    for (name, contents) in self.files(&dir) {
                        fs::write(dir.join(name), contents)?;
                    }
                    return Ok(Materialized {
                        args: self.args(&dir),
                        env: self.env(&dir),
                        dir,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique ait-shell session dir",
        ))
    }

    /// The `--rcfile` wrapper bash reads instead of `~/.bashrc`: source
    /// the user's real rc first (Spec R05 us-2), then the snippet.
    fn bashrc_wrapper(&self, dir: &Path) -> String {
        format!(
            "# Generated by ait's shell-integration injection — do not edit.\n\
             # Loads the user's real rc, then the ait snippet (REQUIREMENTS §5.5).\n\
             if [ -r \"$HOME/.bashrc\" ]; then\n\
             \x20   . \"$HOME/.bashrc\"\n\
             fi\n\
             . \"{}\"\n",
            sh_double_quote(&dir.join("snippet.bash").to_string_lossy())
        )
    }

    /// The temp-`ZDOTDIR` `.zshrc`: user's real rc, then the snippet,
    /// then restore `ZDOTDIR` so a nested zsh reads user dotfiles
    /// normally (Spec R05 us-27).
    fn zshrc_wrapper(&self, dir: &Path) -> String {
        format!(
            "{forwarder}\
             # ait shell integration (REQUIREMENTS §5.5)\n\
             source \"{}\"\n\n\
             # Restore the real ZDOTDIR so nested zsh behaves normally.\n\
             export ZDOTDIR=\"${{AIT_ZDOTDIR_ORIG:-$HOME}}\"\n",
            sh_double_quote(&dir.join("snippet.zsh").to_string_lossy()),
            forwarder = zsh_forwarder(".zshrc"),
        )
    }
}

/// A materialized injection: the session `dir` on disk (removed on
/// drop), plus the argv/env rewrite the spawn ticket applies to
/// `LiveSessionOptions`.
#[derive(Debug)]
pub struct Materialized {
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    dir: PathBuf,
}

impl Materialized {
    /// Args to prepend to the spawned program's existing args.
    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    /// Env vars to layer onto the spawned shell's environment.
    pub fn env(&self) -> &[(OsString, OsString)] {
        &self.env
    }

    /// The `0700` session dir holding snippet + wrappers; removed on
    /// drop.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Record the caller-allocated session id the hooks echo (§5.5's
    /// `session_id`). Replaces the scrub entry planted by
    /// [`Injection::env`].
    pub fn set_session_id(&mut self, id: impl Into<OsString>) {
        let key = OsStr::new(ENV_SESSION_ID);
        let value = id.into();
        if let Some(slot) = self.env.iter_mut().find(|(k, _)| k == key) {
            slot.1 = value;
        } else {
            self.env.push((key.to_os_string(), value));
        }
    }
}

impl Drop for Materialized {
    /// Best-effort cleanup — the session dir holds no user data, so a
    /// failure is safe to ignore (a few bytes in tmp outlive us).
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A zsh startup-file forwarder: source the user's real file from the
/// *original* `ZDOTDIR` (`AIT_ZDOTDIR_ORIG`, default `$HOME`). Never
/// restores `ZDOTDIR` itself — `.zshrc` does that after the snippet, so
/// startup files keep resolving through this dir until then.
fn zsh_forwarder(file: &str) -> String {
    format!(
        "# Generated by ait's shell-integration injection — do not edit.\n\
         # Forwards to the user's real {file} (REQUIREMENTS §5.5).\n\
         if [[ -r \"${{AIT_ZDOTDIR_ORIG:-$HOME}}/{file}\" ]]; then\n\
         \x20   source \"${{AIT_ZDOTDIR_ORIG:-$HOME}}/{file}\"\n\
         fi\n"
    )
}

/// `0700` on unix; a no-op elsewhere (other platforms get whatever the
/// filesystem gives `create_dir`).
#[cfg(unix)]
fn set_private_dir_permissions(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// Non-unix fallback — per-user temp dirs already carry the right ACLs.
#[cfg(not(unix))]
fn set_private_dir_permissions(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Double-quote a path for interpolation into bash/zsh script text.
fn sh_double_quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    for ch in text.chars() {
        if matches!(ch, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Single-quote a path for interpolation into a fish command string —
/// inside fish single quotes only `\'` and `\\` escape.
fn fish_single_quote(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}
