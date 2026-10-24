//! The status line's model: `cwd · shell · model · state` for the
//! active tab, overridden ~3 s at a time by notification toasts (Spec
//! R02: "a status line shows cwd, shell, model and session state" +
//! "notifications surface as toasts").
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

/// How long a toast stays on the status line before the normal
/// `cwd · shell · model · state` rendering returns (Spec R02: ~3 s).
pub const TOAST_DURATION: Duration = Duration::from_secs(3);

/// The model slot until Round 3 wires a real endpoint (Spec R02).
const MODEL_PLACEHOLDER: &str = "—";

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

/// The status line's content at `now`: a live toast wins outright;
/// otherwise `cwd · shell · model · state` for the described tab.
///
/// `cwd` is the tab's tracked working directory (OSC 7, else spawn
/// cwd), `shell` its program basename, `exit` the session's exit
/// status once `LiveEvent::Exited` surfaced.
pub fn status_text(
    cwd: &Path,
    shell: &str,
    exit: Option<&ExitStatus>,
    toast: Option<&Toast>,
    now: Instant,
) -> String {
    if let Some(toast) = toast.filter(|toast| toast.is_live(now)) {
        return toast.text().to_string();
    }
    format!(
        "{} · {shell} · {MODEL_PLACEHOLDER} · {}",
        cwd.display(),
        state_text(exit),
    )
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

    /// The spec's exact layout: `cwd · shell · model · state`, model
    /// `—` until Round 3, `running` while the session lives.
    #[test]
    fn status_line_is_cwd_shell_model_state() {
        let text = status_text(
            Path::new("/home/u/project"),
            "bash",
            None,
            None,
            Instant::now(),
        );
        assert_eq!(text, "/home/u/project · bash · — · running");
    }

    /// `Exited` flips the state slot to `exited <code>` — the text
    /// change is what forces the repaint.
    #[test]
    fn exited_replaces_running() {
        let now = Instant::now();
        let code3 = exit(3, None);
        assert_eq!(
            status_text(Path::new("/tmp"), "sh", Some(&code3), None, now),
            "/tmp · sh · — · exited 3"
        );
        // A signaled exit keeps its description readable.
        let hung_up = exit(1, Some("Terminated by SIGHUP"));
        assert_eq!(
            status_text(Path::new("/tmp"), "sh", Some(&hung_up), None, now),
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
            status_text(Path::new("/tmp"), "sh", None, Some(&toast), now),
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
            Path::new("/tmp"),
            "sh",
            None,
            Some(&toast),
            now + TOAST_DURATION,
        );
        assert_eq!(summary, "/tmp · sh · — · running");
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
