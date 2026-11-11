//! The status line's model: `cwd · shell · model · [profile ·] state`
//! for the active tab, overridden ~3 s at a time by notification toasts
//! (Spec R02: "a status line shows cwd, shell, model and session
//! state" + "notifications surface as toasts"). The `profile` segment
//! is conditional (Spec R07 us-24): it exists only while the active
//! execution profile is not `default`.
//!
//! `status` holds only presentation state — pure text and a deadline —
//! so the whole thing is testable without a PTY (standard 11). [`app`]
//! keeps one [`Toast`] at a time (the newest wins) and asks for
//! [`status_text`] every loop tick: while the toast is live its text
//! renders — reversed — on the status row; past its deadline the normal
//! session summary returns. Expiry therefore rides the existing 8 ms
//! `event::poll` tick and needs no thread (Spec R02 runtime decision).
//! The computed text also sits in the frame snapshot, so a toast
//! appearing or expiring — and a session exiting — always repaints.

use std::path::Path;
use std::time::{Duration, Instant};

use ait_term::ExitStatus;

use crate::blocks::BlockStatus;

/// How long a toast stays on the status line before the normal
/// `cwd · shell · model · state` rendering returns (Spec R02: ~3 s).
pub const TOAST_DURATION: Duration = Duration::from_secs(3);

/// A transient message on the status line: `text` until `deadline`,
/// then gone. One toast at a time — a new one replaces the old.
pub struct Toast {
    text: String,
    deadline: Instant,
}

impl Toast {
    /// A toast showing `text` until `now + TOAST_DURATION`. The caller
    /// passes `now` (the loop passes `Instant::now()`) so deadline
    /// logic is testable without sleeping.
    pub fn new(text: impl Into<String>, now: Instant) -> Self {
        Self {
            text: text.into(),
            deadline: now + TOAST_DURATION,
        }
    }

    /// Whether the toast still shows at `now` — past `deadline` the
    /// status line reverts to the session summary.
    pub fn is_live(&self, now: Instant) -> bool {
        now < self.deadline
    }

    /// The toast's text.
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// The status summary's inputs for one tab — bundled so
/// [`status_text`] takes three arguments instead of seven.
pub struct StatusLine<'a> {
    /// The tab's tracked working directory (OSC 7, else spawn cwd).
    pub cwd: &'a Path,
    /// The tab's program basename.
    pub shell: &'a str,
    /// The resolved `base_model`'s display name (`—` when unset —
    /// Spec R03 story 24).
    pub model: &'a str,
    /// The active execution profile's name — `Some` only when it is
    /// not `default`, which is what keeps the segment out of the R03
    /// layout (Spec R07 us-24).
    pub profile: Option<&'a str>,
    /// The newest finished block's status (`✓/✗ <code> · <dur>` —
    /// Spec R05 us-15; absent when no block has reported an exit code).
    pub block: Option<BlockStatus>,
    /// The live remote session's destination token (Spec R09-T4:
    /// `ssh:<target>` while an `ssh()`-wrapped session is open;
    /// absent on a local shell — the R03 layout is unchanged).
    pub remote: Option<&'a str>,
    /// The session's exit status once `LiveEvent::Exited` surfaced.
    pub exit: Option<&'a ExitStatus>,
}

/// The status line's content at `now`: a live toast wins outright;
/// otherwise `cwd · shell · model · [profile ·] [block ·] [ssh:·]
/// state` for the tab `line` describes.
pub fn status_text(line: &StatusLine<'_>, toast: Option<&Toast>, now: Instant) -> String {
    if let Some(toast) = toast.filter(|toast| toast.is_live(now)) {
        return toast.text().to_string();
    }
    let profile = line
        .profile
        .map_or_else(String::new, |name| format!("{name} · "));
    let block = line
        .block
        .map_or_else(String::new, |status| format!("{} · ", block_text(status)));
    let remote = line
        .remote
        .map_or_else(String::new, |target| format!("ssh:{target} · "));
    format!(
        "{} · {} · {} · {profile}{block}{remote}{}",
        line.cwd.display(),
        line.shell,
        line.model,
        state_text(line.exit),
    )
}

/// The block segment: `✓ <code>` when the command exited zero, `✗
/// <code>` otherwise — then `· <dur>` (Spec R05's `✓ 0 · 1.2s`).
fn block_text(status: BlockStatus) -> String {
    let mark = if status.exit == 0 { '✓' } else { '✗' };
    format!(
        "{mark} {} · {}",
        status.exit,
        duration_text(status.duration)
    )
}

/// Sub-minute durations render tenths (`1.2s`, the spec's shape);
/// minute-plus durations collapse to `Mm SSs` so a long build stays
/// readable without a float tail.
fn duration_text(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        format!("{:.1}s", duration.as_secs_f64())
    } else {
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

/// `running` while the session lives; `exited <code>` once `Exited`
/// surfaced — a signal appends its platform description.
fn state_text(exit: Option<&ExitStatus>) -> String {
    match exit {
        None => "running".into(),
        Some(status) => match &status.signal {
            Some(signal) => format!("exited {} ({signal})", status.code),
            None => format!("exited {}", status.code),
        },
    }
}

/// The toast text for an OSC 9/777 `Notification` event: parts joined
/// with `": "` — `source` (the notifying tab's tab-bar label `N:title`
/// when it sits in the background, so the toast still surfaces with
/// attribution), `title` (OSC 777's `notify;<title>;<body>` only), then
/// `message`.
pub fn notification_text(source: Option<&str>, title: Option<&str>, message: &str) -> String {
    let mut parts = Vec::new();
    if let Some(source) = source {
        parts.push(source);
    }
    if let Some(title) = title {
        parts.push(title);
    }
    parts.push(message);
    parts.join(": ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exit(code: u32, signal: Option<&str>) -> ExitStatus {
        ExitStatus {
            code,
            signal: signal.map(str::to_string),
        }
    }

    /// The spec's exact layout: `cwd · shell · model · state` —
    /// `model` is the resolved `base_model`'s display name (Spec R03
    /// story 24), `running` while the session lives.
    #[test]
    fn status_line_is_cwd_shell_model_state() {
        let text = status_text(
            &StatusLine {
                cwd: Path::new("/home/u/project"),
                shell: "bash",
                model: "Qwen Coder",
                profile: None,
                block: None,
                remote: None,
                exit: None,
            },
            None,
            Instant::now(),
        );
        assert_eq!(text, "/home/u/project · bash · Qwen Coder · running");

        let text = status_text(
            &StatusLine {
                cwd: Path::new("/home/u/project"),
                shell: "bash",
                model: "—",
                profile: None,
                block: None,
                remote: None,
                exit: None,
            },
            None,
            Instant::now(),
        );
        assert_eq!(text, "/home/u/project · bash · — · running");
    }

    /// Spec R05 us-15: a finished block adds its `✓/✗ <code> · <dur>`
    /// segment ahead of the session state — `✓` on exit 0, `✗` on
    /// anything else; no block leaves the layout untouched.
    #[test]
    fn block_status_renders_between_model_and_state() {
        let now = Instant::now();
        let ok = BlockStatus {
            exit: 0,
            duration: Duration::from_millis(1200),
        };
        assert_eq!(
            status_text(
                &StatusLine {
                    cwd: Path::new("/tmp"),
                    shell: "sh",
                    model: "—",
                    profile: None,
                    block: Some(ok),
                    remote: None,
                    exit: None,
                },
                None,
                now
            ),
            "/tmp · sh · — · ✓ 0 · 1.2s · running"
        );

        let failed = BlockStatus {
            exit: 1,
            duration: Duration::from_millis(60),
        };
        assert_eq!(
            status_text(
                &StatusLine {
                    cwd: Path::new("/tmp"),
                    shell: "sh",
                    model: "—",
                    profile: None,
                    block: Some(failed),
                    remote: None,
                    exit: None,
                },
                None,
                now
            ),
            "/tmp · sh · — · ✗ 1 · 0.1s · running"
        );
    }

    /// The duration slot: tenths under a minute (`1.2s`, the spec's
    /// example shape), `Mm SSs` past it so a long command stays
    /// readable.
    #[test]
    fn block_duration_shapes() {
        assert_eq!(duration_text(Duration::from_millis(40)), "0.0s");
        assert_eq!(duration_text(Duration::from_millis(1234)), "1.2s");
        assert_eq!(duration_text(Duration::from_secs(59)), "59.0s");
        assert_eq!(duration_text(Duration::from_secs(65)), "1m 05s");
        assert_eq!(duration_text(Duration::from_secs(600)), "10m 00s");
    }

    /// `Exited` flips the state slot to `exited <code>` — the text
    /// change is what forces the repaint.
    #[test]
    fn exited_replaces_running() {
        let now = Instant::now();
        let code3 = exit(3, None);
        assert_eq!(
            status_text(
                &StatusLine {
                    cwd: Path::new("/tmp"),
                    shell: "sh",
                    model: "—",
                    profile: None,
                    block: None,
                    remote: None,
                    exit: Some(&code3),
                },
                None,
                now
            ),
            "/tmp · sh · — · exited 3"
        );
        // A signaled exit keeps its description readable.
        let hung_up = exit(1, Some("Terminated by SIGHUP"));
        assert_eq!(
            status_text(
                &StatusLine {
                    cwd: Path::new("/tmp"),
                    shell: "sh",
                    model: "—",
                    profile: None,
                    block: None,
                    remote: None,
                    exit: Some(&hung_up),
                },
                None,
                now
            ),
            "/tmp · sh · — · exited 1 (Terminated by SIGHUP)"
        );
    }

    /// A live toast replaces the whole line — the notification gets
    /// the user's attention without needing its own row.
    #[test]
    fn live_toast_overrides_the_summary() {
        let now = Instant::now();
        let toast = Toast::new("build finished", now);
        assert_eq!(
            status_text(
                &StatusLine {
                    cwd: Path::new("/tmp"),
                    shell: "sh",
                    model: "—",
                    profile: None,
                    block: None,
                    remote: None,
                    exit: None,
                },
                Some(&toast),
                now
            ),
            "build finished"
        );
    }

    /// Expiry is pure deadline logic: before the deadline the toast is
    /// live, at/after it the summary returns — no sleeping involved.
    #[test]
    fn expired_toast_reverts_to_the_summary() {
        let now = Instant::now();
        let toast = Toast::new("ping", now);
        assert!(toast.is_live(now));
        assert!(toast.is_live(now + TOAST_DURATION - Duration::from_millis(1)));
        assert!(
            !toast.is_live(now + TOAST_DURATION),
            "deadline is exclusive"
        );

        let summary = status_text(
            &StatusLine {
                cwd: Path::new("/tmp"),
                shell: "sh",
                model: "—",
                profile: None,
                block: None,
                remote: None,
                exit: None,
            },
            Some(&toast),
            now + TOAST_DURATION,
        );
        assert_eq!(summary, "/tmp · sh · — · running");
    }

    /// Spec R07 us-24: a non-`default` active profile adds its segment
    /// right after the model slot — `None` (the armed `default`) keeps
    /// the R03 layout byte-for-byte.
    #[test]
    fn a_non_default_profile_adds_its_segment() {
        let text = status_text(
            &StatusLine {
                cwd: Path::new("/tmp"),
                shell: "sh",
                model: "—",
                profile: Some("work"),
                block: None,
                remote: None,
                exit: None,
            },
            None,
            Instant::now(),
        );
        assert_eq!(text, "/tmp · sh · — · work · running");

        // …and the block segment still lands after it.
        let text = status_text(
            &StatusLine {
                cwd: Path::new("/tmp"),
                shell: "sh",
                model: "—",
                profile: Some("work"),
                block: Some(BlockStatus {
                    exit: 0,
                    duration: Duration::from_millis(1200),
                }),
                remote: None,
                exit: None,
            },
            None,
            Instant::now(),
        );
        assert_eq!(text, "/tmp · sh · — · work · ✓ 0 · 1.2s · running");
    }

    /// Spec R09-T4: a live remote session adds its `ssh:<target>`
    /// segment right before the state slot — `None` (a local shell)
    /// keeps the R03 layout byte-for-byte.
    #[test]
    fn a_remote_session_adds_its_ssh_segment() {
        let text = status_text(
            &StatusLine {
                cwd: Path::new("/home/u"),
                shell: "bash",
                model: "—",
                profile: None,
                block: None,
                remote: Some("user@prod"),
                exit: None,
            },
            None,
            Instant::now(),
        );
        assert_eq!(text, "/home/u · bash · — · ssh:user@prod · running");
    }

    /// OSC 9 carries only a message; OSC 777's `notify;<title>;<body>`
    /// reads `title: body`; a background tab's notification is prefixed
    /// with its title for attribution.
    #[test]
    fn notification_text_shapes() {
        assert_eq!(notification_text(None, None, "done"), "done");
        assert_eq!(
            notification_text(None, Some("build"), "succeeded"),
            "build: succeeded"
        );
        assert_eq!(
            notification_text(Some("2:vim"), None, "swap file found"),
            "2:vim: swap file found"
        );
        assert_eq!(
            notification_text(Some("2:sh"), Some("job"), "finished"),
            "2:sh: job: finished"
        );
    }
}
