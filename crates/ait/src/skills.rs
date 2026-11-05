//! §5.10.2's "watched for changes" (Spec R10-T6): the frontend-owned
//! live skills state. One watcher covers BOTH skills roots — the user
//! config tree's `skills/` ([`ait_config::user_skills_dir`]) and the
//! nearest `.ait/skills/` resolved for the session cwd
//! ([`ait_config::project_skills_dir`]) — through the same `notify`
//! machinery `settings.toml`'s hot reload uses (ait-config's `watch`
//! feature): each layer's PARENT directory is subscribed recursively
//! ([`ait_config::watch_dirs`]), so a `SKILL.md` created, edited or
//! deleted at any depth — and a `skills/` directory appearing
//! mid-session under an already-watched `.ait` — settles into one
//! rescan after the debounce.
//!
//! The refreshed [`SkillsCatalog`] is what the NEXT `AgentRequest`
//! snapshots: [`Tab::submit_agent_task`](crate::tabs::Tab) calls
//! [`LiveSkills::snapshot`], which re-resolves the project root for
//! THAT task's cwd — a session whose nearest `.ait` changed, or whose
//! `.ait` appeared or disappeared mid-session, converges on that
//! snapshot. A rescan that finds a `SKILL.md` malformed keeps the
//! skill's last-good copy (ADR-0006's keep-last-good spirit — a bad
//! write must not drop skills mid-session,
//! [`SkillsCatalog::keep_last_good`]) and toasts the surfaced
//! `SkillError`; deleting the file drops the skill outright.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ait_agent::{SkillError, SkillsCatalog};
use ait_config::DirsWatcher;

use crate::status::Toast;

/// The status-line text for a catalog's skipped entries — shared by
/// the submit-time toast and the watcher drain (CODING_STANDARDS.md
/// standard 17: a status line's text is defined once). The first
/// error renders in full (`SkillError`'s Display is already
/// toast-ready); the rest collapse to a count.
pub(crate) fn skipped_text(errors: &[SkillError]) -> String {
    match errors.split_first() {
        None => String::new(),
        Some((first, [])) => format!("skill skipped: {first}"),
        Some((first, rest)) => format!("skill skipped: {first} (+{} more)", rest.len()),
    }
}

/// The last-good [`SkillsCatalog`] plus the watcher keeping it
/// fresh — the skills analog of [`LiveSettings`](crate::config::LiveSettings):
/// adopted wholesale by each refresh, snapshotted onto each outgoing
/// `AgentRequest`. `AppState` owns the single instance; the watcher's
/// channel drains once per app-loop tick via [`drain`](Self::drain).
/// A watcher the OS could not build degrades to `None` — snapshots
/// then rescan directly (T5's submit-time behavior), never stale.
pub struct LiveSkills {
    /// The user-layer root the catalog scans — constant for the
    /// process (the XDG resolution does not move mid-session).
    user_root: PathBuf,
    /// The project-layer root resolved for the last [`snapshot`](Self::snapshot)
    /// cwd — `None` while no `.ait` applies.
    project_root: Option<PathBuf>,
    /// Project-root discovery — [`ait_config::project_skills_dir`] in
    /// the app, a fixture in tests.
    discover: fn(&Path) -> Option<PathBuf>,
    /// The last-good catalog: refreshed on watcher events, swapped
    /// wholesale so derived state can never lag the scan it came
    /// from.
    catalog: SkillsCatalog,
    /// The directory watcher — `None` when the OS watcher could not
    /// be built; snapshots rescan instead so the catalog never
    /// serves stale content.
    watcher: Option<DirsWatcher>,
    /// The dirs the watcher currently covers — compared at each
    /// refresh/snapshot; a mismatch rebuilds the watcher.
    watched: Vec<PathBuf>,
}

impl LiveSkills {
    /// The live catalog over the real roots: the XDG user skills dir
    /// and the nearest-`.ait` walk — watching starts at once so a
    /// write before the first task still lands in that task's
    /// snapshot.
    pub fn new() -> Self {
        Self::with_parts(
            ait_config::user_skills_dir(),
            ait_config::project_skills_dir,
        )
    }

    /// [`new`](Self::new) at caller-named roots — the seam tests and
    /// callers with their own discovery use, mirroring
    /// `config::watcher_at`.
    fn with_parts(user_root: PathBuf, discover: fn(&Path) -> Option<PathBuf>) -> Self {
        let mut live = Self {
            catalog: SkillsCatalog::scan_roots(&user_root, None),
            user_root,
            project_root: None,
            discover,
            watcher: None,
            watched: Vec::new(),
        };
        live.ensure_watcher();
        live
    }

    /// A catalog with no watcher and no roots — the test seam for
    /// harnesses that drive `agent_key`/`AppState` without touching
    /// the filesystem: the desired watch set computes empty, so
    /// `ensure_watcher` never builds and snapshots rescan the empty
    /// root into an empty catalog.
    #[cfg(test)]
    pub(crate) fn unwatched() -> Self {
        Self {
            user_root: PathBuf::new(),
            project_root: None,
            discover: |_| None,
            catalog: SkillsCatalog::default(),
            watcher: None,
            watched: Vec::new(),
        }
    }

    /// The catalog for the next [`AgentRequest`](ait_agent::AgentRequest)
    /// — re-resolving the project layer for `cwd` so a session whose
    /// nearest `.ait` changed (or whose `.ait` appeared/disappeared
    /// mid-session) converges on THIS snapshot. A root change rescans
    /// fresh — no last-good merge across different roots — while an
    /// unchanged root set serves the watcher-refreshed cache.
    pub fn snapshot(&mut self, cwd: &Path) -> SkillsCatalog {
        let project_root = (self.discover)(cwd);
        let root_changed = project_root != self.project_root;
        if root_changed {
            self.project_root = project_root;
            self.rescan(false);
        }
        self.ensure_watcher();
        if self.watcher.is_none() && !root_changed {
            // No live refresh — rescan so the snapshot can never go
            // stale; last-good still merges in case the read caught
            // a mid-write file. A root change already rescanned
            // fresh above — no second read.
            self.rescan(true);
        }
        self.catalog.clone()
    }

    /// Drain one tick of watcher events (the app-loop seam mirroring
    /// `drain_config_events`): any queued "rescan now" bursts
    /// coalesce into ONE refresh.
    pub fn drain(&mut self, toast: &mut Option<Toast>) {
        let pending = match &self.watcher {
            Some(watcher) => {
                let mut pending = false;
                while watcher.receiver().try_recv().is_ok() {
                    pending = true;
                }
                pending
            }
            None => false,
        };
        if pending {
            self.refresh(toast);
        }
    }

    /// [`drain`](Self::drain)'s blocking twin — the test seam for
    /// "wait until the next burst lands, then refresh": `recv_timeout`
    /// IS the wait, so no peeking at the channel is needed.
    #[cfg(test)]
    fn wait_and_drain(&mut self, timeout: std::time::Duration, toast: &mut Option<Toast>) {
        let pending = match &self.watcher {
            Some(watcher) => {
                let pending = watcher.receiver().recv_timeout(timeout).is_ok();
                // Coalesce any stragglers the same way drain does.
                while watcher.receiver().try_recv().is_ok() {}
                pending
            }
            None => false,
        };
        if pending {
            self.refresh(toast);
        }
    }

    /// One rescan against the same roots — keeping last-good copies
    /// of entries the fresh scan could not parse — plus the
    /// malformed-write toast. A CHANGE in the surfaced error set
    /// toasts `skill skipped: …`; errors that were already there
    /// stay silent (a still-broken file must not re-toast on every
    /// unrelated write under the root). The watched set may also
    /// have shrunk under us — a removed `.ait` leaves dead inotify
    /// watches; rebuild covers what exists.
    fn refresh(&mut self, toast: &mut Option<Toast>) {
        let errors_before = self.catalog.errors().to_vec();
        self.rescan(true);
        self.ensure_watcher();
        let errors = self.catalog.errors();
        if !errors.is_empty() && errors != errors_before.as_slice() {
            *toast = Some(Toast::new(skipped_text(errors), Instant::now()));
        }
    }

    /// Rescan the current roots into the cached catalog — `last_good`
    /// merges the previous catalog's copies for entries the fresh
    /// scan could not parse (ADR-0006); `false` is a clean context
    /// switch (a new project root keeps nothing of the old).
    fn rescan(&mut self, last_good: bool) {
        let mut catalog = SkillsCatalog::scan_roots(&self.user_root, self.project_root.as_deref());
        if last_good {
            catalog.keep_last_good(&self.catalog);
        }
        self.catalog = catalog;
    }

    /// The directories the watcher must cover for the current roots
    /// — each layer's PARENT, watched recursively (the settings
    /// watcher's parent-dir rule generalized): `~/.config/ait` for
    /// the user root, `.ait` itself for the project root. Watching
    /// the parent catches a `skills/` directory appearing
    /// mid-session with no re-watch.
    fn desired_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        // The user layer's parent must exist before it can be
        // watched — the same first-run justification
        // `config::watcher` gives for creating the config dir.
        if let Some(parent) = self
            .user_root
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            let _ = fs::create_dir_all(parent);
            if parent.is_dir() {
                dirs.push(parent.to_path_buf());
            }
        }
        if let Some(ait_dir) = self.project_root.as_deref().and_then(Path::parent) {
            if ait_dir.is_dir() {
                dirs.push(ait_dir.to_path_buf());
            }
        }
        dirs
    }

    /// Rebuild the watcher when the desired dir set differs from the
    /// covered one — a `.ait` resolved or removed, a first successful
    /// build after a failure. A failed build stays `None` (and
    /// retries next call) rather than latching an unwatched set.
    fn ensure_watcher(&mut self) {
        let desired = self.desired_dirs();
        if desired == self.watched {
            return;
        }
        if desired.is_empty() {
            // Nothing left to watch — drop the stale watcher outright.
            self.watcher = None;
            self.watched.clear();
            return;
        }
        self.watcher = ait_config::watch_dirs(&desired).ok();
        if self.watcher.is_some() {
            self.watched = desired;
        }
    }
}

#[cfg(test)]
mod tests {
    //! The `LiveSkills` seam over `tempfile` roots (Spec R10-T6
    //! testing decisions): per-cwd project-root convergence, the
    //! watcher-driven refresh, keep-last-good on malformed writes and
    //! deletion dropping skills — real `notify`, real filesystem, no
    //! TTY (standard 11).

    use super::*;
    use ait_agent::SkillOrigin;
    use std::time::Duration;

    /// Generous bound on "the watch event eventually arrives" — the
    /// same budget the settings-watcher tests use, far past the
    /// ~300 ms debounce so a loaded CI machine cannot flake.
    const EVENT_WAIT: Duration = Duration::from_secs(10);

    /// Plant `<root>/<dir>/SKILL.md` — the ait-agent fixture's shape.
    fn write_skill(root: &Path, dir: &str, name: &str) {
        let dir = root.join(dir);
        fs::create_dir_all(&dir).expect("skill dir");
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: for {name}\n---\n\nDo it.\n"),
        )
        .expect("SKILL.md");
    }

    /// A `LiveSkills` rooted at `user_root`, discovering `.ait` by
    /// the real [`ait_config::project_skills_dir`] walk — a tempdir
    /// cwd drives it.
    fn live_at(user_root: &Path) -> LiveSkills {
        LiveSkills::with_parts(user_root.to_path_buf(), ait_config::project_skills_dir)
    }

    /// Block until the next watcher burst arrives, then refresh —
    /// the deterministic "tick" for tests.
    fn wait_and_drain(live: &mut LiveSkills, toast: &mut Option<Toast>) {
        live.wait_and_drain(EVENT_WAIT, toast);
    }

    /// Per-cwd convergence (Spec R10-T6): the project layer is the
    /// nearest `.ait/skills/` for THAT task's cwd — different cwds
    /// see different catalogs, and a `.ait` appearing mid-session
    /// lands on the next snapshot.
    #[test]
    fn snapshot_resolves_the_project_root_for_each_cwd() {
        let dirs = tempfile::tempdir().unwrap();
        let user_root = dirs.path().join("user").join("skills");
        write_skill(&user_root, "mine", "mine");
        let cwd = dirs.path().join("project");
        let other_cwd = dirs.path().join("elsewhere");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&other_cwd).unwrap();

        let mut live = live_at(&user_root);

        // No .ait anywhere: user skills only.
        let catalog = live.snapshot(&cwd);
        assert!(catalog.get("mine").is_some());
        assert_eq!(catalog.iter().count(), 1);

        // `.ait` appears mid-session — the NEXT snapshot picks it up.
        write_skill(&cwd.join(".ait").join("skills"), "repo", "repo");
        let catalog = live.snapshot(&cwd);
        let skill = catalog.get("repo").expect("the project skill joined");
        assert_eq!(skill.origin, SkillOrigin::Project);

        // A different cwd converges to ITS own nearest `.ait` — none
        // here, so the project skill stays out of its snapshot.
        let catalog = live.snapshot(&other_cwd);
        assert!(catalog.get("repo").is_none(), "other cwd has no .ait");
        assert!(catalog.get("mine").is_some());
    }

    /// Creating a `SKILL.md` under a watched root refreshes the
    /// cached catalog — the next snapshot carries the new skill with
    /// no restart. The user layer's parent watch covers `skills/`
    /// appearing too (here the skill dir is created under an
    /// already-watched parent).
    #[test]
    fn a_write_under_a_watched_root_refreshes_the_catalog() {
        let dirs = tempfile::tempdir().unwrap();
        let user_root = dirs.path().join("user").join("skills");
        fs::create_dir_all(&user_root).unwrap();
        let mut live = live_at(&user_root);
        let mut toast = None;

        write_skill(&user_root, "fresh", "fresh");
        wait_and_drain(&mut live, &mut toast);

        let catalog = live.snapshot(Path::new("/nowhere"));
        assert!(
            catalog.get("fresh").is_some(),
            "the write refreshed the cached catalog"
        );
    }

    /// ADR-0006 keep-last-good: a malformed `SKILL.md` write toasts
    /// its surfaced error AND keeps serving the previous copy — a bad
    /// save never drops a working skill mid-session.
    #[test]
    fn a_malformed_write_keeps_last_good_and_toasts() {
        let dirs = tempfile::tempdir().unwrap();
        let user_root = dirs.path().join("user").join("skills");
        write_skill(&user_root, "demo", "demo");
        let mut live = live_at(&user_root);
        let mut toast = None;

        // Load the good copy, then break the file mid-session.
        assert!(live.snapshot(Path::new("/nowhere")).get("demo").is_some());
        fs::write(user_root.join("demo").join("SKILL.md"), "no markers\n").unwrap();
        wait_and_drain(&mut live, &mut toast);

        let catalog = live.snapshot(Path::new("/nowhere"));
        let skill = catalog.get("demo").expect("last-good serves the skill");
        assert_eq!(skill.description, "for demo");
        assert_eq!(catalog.errors().len(), 1, "the error still surfaces");
        let toast = toast.expect("the malformed write toasted");
        assert!(
            toast.text().starts_with("skill skipped:"),
            "{}",
            toast.text()
        );
    }

    /// Deleting a `SKILL.md` is a real removal — absent, not a parse
    /// failure — so the skill leaves the next snapshot.
    #[test]
    fn deleting_a_skill_drops_it_on_the_next_drain() {
        let dirs = tempfile::tempdir().unwrap();
        let user_root = dirs.path().join("user").join("skills");
        write_skill(&user_root, "demo", "demo");
        let mut live = live_at(&user_root);
        let mut toast = None;
        assert!(live.snapshot(Path::new("/nowhere")).get("demo").is_some());

        fs::remove_dir_all(user_root.join("demo")).unwrap();
        wait_and_drain(&mut live, &mut toast);

        assert!(
            live.snapshot(Path::new("/nowhere")).get("demo").is_none(),
            "a deleted skill must not come back"
        );
    }

    /// The project root's parent (`.ait`) is watched recursively, so
    /// a `skills/` directory appearing mid-session refreshes on the
    /// drain itself — the next snapshot carries the new skill.
    #[test]
    fn a_skills_dir_appearing_under_a_watched_ait_is_seen() {
        let dirs = tempfile::tempdir().unwrap();
        let user_root = dirs.path().join("user").join("skills");
        fs::create_dir_all(&user_root).unwrap();
        let cwd = dirs.path().join("project");
        // `.ait` exists but has no skills dir yet.
        fs::create_dir_all(cwd.join(".ait")).unwrap();

        let mut live = live_at(&user_root);
        let mut toast = None;
        assert_eq!(live.snapshot(&cwd).iter().count(), 0, "no skills yet");

        write_skill(&cwd.join(".ait").join("skills"), "repo", "repo");
        wait_and_drain(&mut live, &mut toast);

        assert!(
            live.snapshot(&cwd).get("repo").is_some(),
            "the appeared skills dir refreshed the catalog"
        );
    }

    /// A `.ait` removed mid-session drops its skills: the dead watch
    /// is rebuilt covering only what still exists, and the next
    /// snapshot re-resolves to no project layer.
    #[test]
    fn a_removed_ait_drops_its_skills() {
        let dirs = tempfile::tempdir().unwrap();
        let user_root = dirs.path().join("user").join("skills");
        fs::create_dir_all(&user_root).unwrap();
        let cwd = dirs.path().join("project");
        write_skill(&cwd.join(".ait").join("skills"), "repo", "repo");

        let mut live = live_at(&user_root);
        let mut toast = None;
        assert!(live.snapshot(&cwd).get("repo").is_some());

        fs::remove_dir_all(cwd.join(".ait")).unwrap();
        // The watched dir's own removal surfaces as an event on most
        // platforms, but the snapshot converges either way.
        live.wait_and_drain(EVENT_WAIT, &mut toast);

        let catalog = live.snapshot(&cwd);
        assert!(
            catalog.get("repo").is_none(),
            "a removed .ait drops its project skills"
        );
    }

    /// `unwatched` never builds a watcher and never scans — the
    /// empty catalog harnesses drive `agent_key` with.
    #[test]
    fn unwatched_stays_empty_and_quiet() {
        let mut live = LiveSkills::unwatched();
        assert!(live.snapshot(Path::new("/nowhere")).iter().next().is_none());
        let mut toast = None;
        live.drain(&mut toast);
        assert!(toast.is_none());
    }

    /// The shared toast text: one error renders it in full, extras
    /// collapse to a count.
    #[test]
    fn skipped_text_names_the_first_error_and_counts_the_rest() {
        let error = |name: &str| SkillError {
            path: PathBuf::from(format!("/x/{name}/SKILL.md")),
            message: String::from("bad frontmatter"),
        };
        let one = vec![error("a")];
        assert_eq!(skipped_text(&one), format!("skill skipped: {}", one[0]));
        let two = vec![error("a"), error("b")];
        assert!(
            skipped_text(&two).ends_with("(+1 more)"),
            "{}",
            skipped_text(&two)
        );
    }
}
