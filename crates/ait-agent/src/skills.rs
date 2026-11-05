//! The §5.10.2 skills loader (Spec R10, ticket R10-T5): a
//! [`SkillsCatalog`] scans two roots — the user config tree's
//! `skills/` ([`ait_config::user_skills_dir`]) and the nearest
//! project `.ait/skills/` ([`ait_config::project_skills_dir`]) — for
//! `<dir>/SKILL.md` entries, parses each one's frontmatter (`name`
//! and `description` required), and hands the agent three things
//! from one snapshot: the system prompt's "Available skills"
//! section ([`SkillsCatalog::prompt_section`]), the `invoke_skill`
//! tool's answers (body + bundled-file manifest,
//! [`SkillsCatalog::invoke`]), and the skill directories the task's
//! `ToolCwd` adds to its read roots so `read_files` reaches a
//! skill's bundled files without `directory_allowlist` edits.
//!
//! The catalog is a SNAPSHOT — the caller builds it (the frontend
//! snapshots its watcher-refreshed live catalog at submit time,
//! Spec R10-T6) and it rides
//! [`AgentRequest`](crate::AgentRequest) verbatim: what the model
//! sees is exactly what the request carried. A `SKILL.md` that is
//! unreadable or whose frontmatter is missing or malformed is
//! skipped and recorded in [`SkillsCatalog::errors`] — a surfaced,
//! toast-ready [`SkillError`], never a silent load and never a task
//! wedge. A project skill shadows a same-named user skill: the more
//! specific layer wins (Spec R10 us-26).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::files::ToolOutput;
use crate::tool_call::ToolCall;

/// The file every skill directory carries — `<dir>/SKILL.md`.
const SKILL_MD: &str = "SKILL.md";

/// How many bundled files one `invoke_skill` manifest lists before
/// noting the truncation — the same cap instinct `glob` applies.
const MANIFEST_CAP: usize = 200;

/// One task's snapshot of the skills on disk: the effective skills
/// keyed by name (sorted — the prompt section and unknown-name
/// errors render deterministically) plus every skipped entry's
/// surfaced error. Build it through [`SkillsCatalog::scan`] — the
/// env-aware constructor — or [`SkillsCatalog::scan_roots`], the
/// explicit-roots seam tests and callers with their own discovery
/// use.
#[derive(Debug, Default, Clone)]
pub struct SkillsCatalog {
    /// Effective skills by frontmatter `name` — post-shadowing, so
    /// this is also the set of directories `read_files` reaches.
    skills: BTreeMap<String, Skill>,
    /// Every entry the scan skipped, in scan order — surfaced to the
    /// frontend's toast and the prompt section, never dropped
    /// silently.
    errors: Vec<SkillError>,
}

/// One loaded skill: its parsed frontmatter, the layer that declared
/// it, its directory, and the snapshot of what `invoke_skill`
/// returns — the `SKILL.md` body plus the relative paths of the
/// files bundled beside it (the manifest excludes `SKILL.md` itself;
/// its body is already in the result).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// The frontmatter `name` — the `invoke_skill` argument and the
    /// shadowing key.
    pub name: String,
    /// The frontmatter `description` — what the prompt section
    /// renders so the model knows when to invoke.
    pub description: String,
    /// Which layer declared this skill.
    pub origin: SkillOrigin,
    /// The skill's directory — the root `read_files` gains and the
    /// base every bundled-file path in the manifest hangs from.
    pub dir: PathBuf,
    /// The `SKILL.md` body — everything after the frontmatter,
    /// snapshotted at scan time.
    body: String,
    /// Paths of the bundled files, relative to [`Skill::dir`],
    /// sorted — `SKILL.md` itself excluded.
    files: Vec<String>,
}

/// Which layer declared a [`Skill`] — the same user/project split
/// `mcp.json` uses (GLOSSARY's "project-local config layer").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillOrigin {
    /// Declared under `$XDG_CONFIG_HOME/ait/skills/`.
    User,
    /// Declared under the nearest `.ait/skills/` — shadows a
    /// same-named user skill.
    Project,
}

impl SkillOrigin {
    /// The prompt's layer phrase — the same labels
    /// [`McpServerOrigin::label`](ait_config::McpServerOrigin::label)
    /// renders for `mcp.json` entries (§17: one phrase, one
    /// definition site — ait-config's layer-label constants).
    fn label(self) -> &'static str {
        match self {
            Self::User => ait_config::USER_LAYER_LABEL,
            Self::Project => ait_config::PROJECT_LAYER_LABEL,
        }
    }
}

/// A `SKILL.md` the scan skipped: the file that failed plus why —
/// the toast-ready text the frontend renders (`{path}: {message}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillError {
    /// The file — or the skills root itself when the root could not
    /// be listed.
    pub path: PathBuf,
    /// What went wrong — missing or malformed frontmatter, an
    /// unreadable file, an unlistable directory.
    pub message: String,
}

impl std::fmt::Display for SkillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

impl SkillsCatalog {
    /// Scan both layers for `cwd` (Spec R10 us-20/21): the user root
    /// under the XDG config tree plus the nearest `.ait/skills/` —
    /// the same layer resolution `load_mcp` applies to `mcp.json`.
    /// The frontend calls this at submit time; the catalog then
    /// rides the request unchanged.
    pub fn scan(cwd: &Path) -> Self {
        Self::scan_roots(
            &ait_config::user_skills_dir(),
            ait_config::project_skills_dir(cwd).as_deref(),
        )
    }

    /// [`scan`](Self::scan) at explicit roots — the seam callers
    /// with their own discovery (tests, a watcher refreshing one
    /// layer) use. `project_root` is the `.ait/skills/` directory
    /// itself, `None` when no `.ait` exists. The project layer scans
    /// last so its skills shadow same-named user skills.
    pub fn scan_roots(user_root: &Path, project_root: Option<&Path>) -> Self {
        let mut catalog = Self::default();
        catalog.scan_layer(user_root, SkillOrigin::User);
        if let Some(root) = project_root {
            catalog.scan_layer(root, SkillOrigin::Project);
        }
        catalog
    }

    /// The effective skills, sorted by name.
    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values()
    }

    /// The effective skill `name`, or `None`.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    /// Every entry the scan skipped — surfaced, toast-ready.
    pub fn errors(&self) -> &[SkillError] {
        &self.errors
    }

    /// ADR-0006 keep-last-good (Spec R10-T6): for every entry THIS
    /// scan skipped as unreadable, reinsert `previous`'s copy when it
    /// lived in the same directory — a `SKILL.md` that can no longer
    /// be parsed serves its last-good version instead of dropping the
    /// skill mid-session. Only parse/read failures merge: a deleted
    /// `SKILL.md` is absent rather than errored (deletion drops the
    /// skill for real), and `or_insert` means a fresh same-named
    /// skill loaded from another directory always beats the stale
    /// copy. The surfaced errors stay recorded — the toast and the
    /// prompt section still name the bad file.
    pub fn keep_last_good(&mut self, previous: &SkillsCatalog) {
        for error in &self.errors {
            if error.path.file_name().and_then(|name| name.to_str()) != Some(SKILL_MD) {
                continue;
            }
            let Some(dir) = error.path.parent() else {
                continue;
            };
            for skill in previous.iter().filter(|skill| skill.dir == dir) {
                self.skills
                    .entry(skill.name.clone())
                    .or_insert_with(|| skill.clone());
            }
        }
    }

    /// One layer's skills: each `<dir>/SKILL.md` under `root`.
    /// Missing roots contribute nothing (a first run is not an
    /// error); a root that cannot be listed, an entry that cannot
    /// be stat'd and a `SKILL.md` that fails to parse each record a
    /// [`SkillError`] and the scan moves on — one bad entry never
    /// sinks the layer. Entries sort before parsing so a same-layer
    /// name collision resolves the same way every run.
    fn scan_layer(&mut self, root: &Path, origin: SkillOrigin) {
        let read = match fs::read_dir(root) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => {
                self.errors.push(SkillError {
                    path: root.to_path_buf(),
                    message: format!("cannot list the skills directory: {error}"),
                });
                return;
            }
        };
        let mut dirs = Vec::new();
        for entry in read {
            match entry {
                Ok(entry) => dirs.push(entry.path()),
                Err(error) => self.errors.push(SkillError {
                    path: root.to_path_buf(),
                    message: format!("cannot list the skills directory: {error}"),
                }),
            }
        }
        dirs.sort();
        for dir in dirs {
            let path = dir.join(SKILL_MD);
            // Only `<dir>/SKILL.md` pairs count — a bare file or a
            // directory without one is not a skill and never an
            // error.
            if !path.is_file() {
                continue;
            }
            match load_skill(&dir, &path, origin) {
                Ok(skill) => {
                    // `insert` IS the shadow: the project layer scans
                    // last, so its entry replaces a same-named user
                    // skill (Spec R10 us-26).
                    self.skills.insert(skill.name.clone(), skill);
                }
                Err(error) => self.errors.push(error),
            }
        }
    }

    /// `invoke_skill(name)`'s answer (Spec R10 us-24): the skill's
    /// `SKILL.md` body plus the manifest of files bundled in its
    /// directory — rendered as absolute paths the model can hand
    /// `read_files` directly (the directory is a task read root).
    /// An unknown name errors naming what IS loaded.
    pub fn invoke(&self, name: &str) -> Result<String, String> {
        let Some(skill) = self.skills.get(name) else {
            let mut error = format!("no skill named {name:?} is loaded");
            if !self.skills.is_empty() {
                error.push_str(&format!(
                    " — available: {}",
                    self.skills
                        .keys()
                        .map(|name| format!("{name:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            return Err(error);
        };
        Ok(skill.render())
    }

    /// The tool call's dispatch arm (Spec R10): parse `name` out of
    /// the call's arguments and [`invoke`](Self::invoke) it. Every
    /// failure — bad JSON, a missing `name`, an unknown skill — is a
    /// [`ToolOutput`] error: model input, never a task wedge.
    pub(crate) fn invoke_call(&self, call: &ToolCall) -> ToolOutput {
        let args: serde_json::Value = match serde_json::from_str(&call.arguments) {
            Ok(args) => args,
            Err(error) => {
                return ToolOutput::error(format!(
                    "invoke_skill: arguments are not valid JSON: {error}"
                ))
            }
        };
        let Some(name) = args.get("name").and_then(|value| value.as_str()) else {
            return ToolOutput::error(String::from("invoke_skill: `name` must be a string"));
        };
        match self.invoke(name) {
            Ok(text) => ToolOutput::ok(text),
            Err(error) => ToolOutput::error(format!("invoke_skill: {error}")),
        }
    }

    /// The system prompt's "Available skills" section (Spec R10
    /// us-23): one line per skill — name, the layer that declared it
    /// and its description — so the model knows what it can invoke.
    /// Skipped entries render their error too: a malformed
    /// `SKILL.md` surfaces rather than silently absenting itself
    /// (the same visibility rule the MCP section's `load_error`
    /// follows). `None` when nothing loaded and nothing failed, so a
    /// session with no skills ships the Round 9 prompt verbatim.
    pub(crate) fn prompt_section(&self) -> Option<String> {
        if self.skills.is_empty() && self.errors.is_empty() {
            return None;
        }
        let mut text =
            String::from("Available skills — load one with invoke_skill to get its instructions:");
        for skill in self.skills.values() {
            let origin = skill.origin.label();
            // One line per skill — collapse any whitespace in the
            // description so a verbose value cannot break the list.
            let description = skill
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            text.push_str(&format!("\n- {} ({origin}): {description}", skill.name));
        }
        for error in &self.errors {
            text.push_str(&format!("\n- skipped {error}"));
        }
        Some(text)
    }

    /// The read roots the task's `ToolCwd` gains at construction
    /// (Spec R10 us-25): every loaded skill's directory, so
    /// `read_files` reaches bundled scripts and templates with no
    /// `directory_allowlist` editing. Only EFFECTIVE skills
    /// contribute — a shadowed user skill's directory does not leak
    /// into confinement.
    pub(crate) fn skill_dirs(&self) -> Vec<String> {
        self.skills
            .values()
            .map(|skill| skill.dir.to_string_lossy().into_owned())
            .collect()
    }
}

impl Skill {
    /// What `invoke_skill` returns: the `SKILL.md` body, then the
    /// bundled-files manifest as absolute `read_files`-ready paths —
    /// capped and honestly empty-stated so a body-only skill reads
    /// cleanly.
    fn render(&self) -> String {
        let mut text = self.body.trim_end().to_string();
        if self.files.is_empty() {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str("(this skill bundles no other files)");
        } else {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!(
                "Bundled files under {} — read them with read_files:",
                self.dir.display()
            ));
            for file in self.files.iter().take(MANIFEST_CAP) {
                text.push_str(&format!("\n- {}", self.dir.join(file).display()));
            }
            if self.files.len() > MANIFEST_CAP {
                text.push_str(&format!(
                    "\n- …and {} more",
                    self.files.len() - MANIFEST_CAP
                ));
            }
        }
        text
    }
}

/// Read and parse one `<dir>/SKILL.md`, snapshotting the body and
/// the bundled-file manifest alongside it. An unreadable file and a
/// frontmatter failure are [`SkillError`]s naming the file.
fn load_skill(dir: &Path, path: &Path, origin: SkillOrigin) -> Result<Skill, SkillError> {
    let text = fs::read_to_string(path).map_err(|error| SkillError {
        path: path.to_path_buf(),
        message: format!("cannot read {SKILL_MD}: {error}"),
    })?;
    let (name, description, body) = parse_skill(path, &text)?;
    let mut files = Vec::new();
    collect_files(dir, dir, &mut files);
    files.sort();
    files.dedup();
    Ok(Skill {
        name,
        description,
        origin,
        dir: dir.to_path_buf(),
        body,
        files,
    })
}

/// The co-located file manifest (Spec R10 us-24): every regular
/// file under the skill directory — recursively, `SKILL.md` itself
/// excluded (its body is already the result's head) — relative to
/// `root` so the render can re-root it absolutely. Unreadable
/// directories and dangling symlinks simply contribute nothing: the
/// manifest is a convenience listing, not a confinement check —
/// `ToolCwd` still decides what `read_files` may open.
fn collect_files(root: &Path, dir: &Path, files: &mut Vec<String>) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect_files(root, &path, files);
        } else if kind.is_file() && path != root.join(SKILL_MD) {
            if let Ok(relative) = path.strip_prefix(root) {
                files.push(relative.to_string_lossy().into_owned());
            }
        }
    }
}

/// The frontmatter contract (Spec R10 us-22): `SKILL.md` must open
/// with a `---` line, carry `key: value` lines until a closing `---`,
/// and declare non-empty `name` and `description`. Unknown keys are
/// tolerated — the ecosystem's files already carry more than this
/// contract needs (`disable-model-invocation`, `argument-hint`, …)
/// and a stricter parser would reject every one of them. The body is
/// everything after the closing `---`, leading blank lines dropped.
/// Any violation is a [`SkillError`] naming the file: the skill is
/// skipped, never silently loaded.
fn parse_skill(path: &Path, text: &str) -> Result<(String, String, String), SkillError> {
    let error = |message: String| SkillError {
        path: path.to_path_buf(),
        message,
    };
    let mut lines = text.lines().enumerate();
    match lines.next() {
        Some((_, line)) if line.trim_end() == "---" => {}
        _ => {
            return Err(error(format!(
                "missing frontmatter — {SKILL_MD} must open with a `---` line"
            )))
        }
    }
    let mut name = None;
    let mut description = None;
    loop {
        let Some((number, line)) = lines.next() else {
            return Err(error(String::from(
                "unterminated frontmatter — no closing `---` line",
            )));
        };
        if line.trim_end() == "---" {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(error(format!(
                "frontmatter line {} is not a `key: value` pair",
                number + 1
            )));
        };
        match key.trim() {
            "name" => name = Some(unquote(value.trim())),
            "description" => description = Some(unquote(value.trim())),
            _ => {}
        }
    }
    let name = match name {
        Some(name) if !name.is_empty() => name,
        _ => {
            return Err(error(String::from(
                "frontmatter `name` is required and must not be empty",
            )))
        }
    };
    let description = match description {
        Some(description) if !description.is_empty() => description,
        _ => {
            return Err(error(String::from(
                "frontmatter `description` is required and must not be empty",
            )))
        }
    };
    let body: Vec<&str> = lines
        .map(|(_, line)| line)
        .skip_while(|line| line.trim().is_empty())
        .collect();
    Ok((name, description, body.join("\n")))
}

/// Strip one layer of matching `'`/`"` quotes from a frontmatter
/// scalar — `name: "deploy"` and `name: deploy` name the same
/// skill.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        return value[1..value.len() - 1].to_string();
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    //! The catalog seam over `tempfile` roots (Spec R10 testing
    //! decisions): discovery order, the frontmatter contract,
    //! project-shadows-user, the surfaced errors, the invoke render
    //! and the prompt section.

    use super::*;

    /// Plant `<root>/<dir>/SKILL.md` with `contents`.
    fn write_skill(root: &Path, dir: &str, contents: &str) {
        let dir = root.join(dir);
        fs::create_dir_all(&dir).expect("skill dir");
        fs::write(dir.join(SKILL_MD), contents).expect("SKILL.md");
    }

    /// A minimal valid `SKILL.md` — `name`/`description` taken from
    /// the directory for brevity.
    fn skill_md(name: &str, description: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n\nDo the thing.\n")
    }

    /// Both layers scan: the user root contributes its skills, the
    /// project root adds its own AND shadows a same-named user skill
    /// — the effective entry keeps the project's description,
    /// directory and `Project` origin (Spec R10 us-26).
    #[test]
    fn both_layers_scan_and_project_shadows_user() {
        let user = tempfile::tempdir().expect("user root");
        let project = tempfile::tempdir().expect("project root");
        write_skill(user.path(), "shared", &skill_md("shared", "the user copy"));
        write_skill(
            user.path(),
            "user-only",
            &skill_md("user-only", "only mine"),
        );
        write_skill(
            project.path(),
            "shared",
            &skill_md("shared", "the project copy"),
        );
        write_skill(
            project.path(),
            "project-only",
            &skill_md("project-only", "the repo's"),
        );

        let catalog = SkillsCatalog::scan_roots(user.path(), Some(project.path()));
        assert!(catalog.errors().is_empty(), "{:?}", catalog.errors());
        let names: Vec<&str> = catalog.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(names, ["project-only", "shared", "user-only"]);
        let shared = catalog.get("shared").expect("shared loads");
        assert_eq!(shared.description, "the project copy");
        assert_eq!(shared.origin, SkillOrigin::Project);
        assert_eq!(shared.dir, project.path().join("shared"));
        assert_eq!(
            catalog.get("user-only").expect("user-only").origin,
            SkillOrigin::User
        );
    }

    /// A missing root scans empty with no error — the first run
    /// must not toast "your skills directory does not exist". A
    /// `None` project root contributes nothing.
    #[test]
    fn missing_roots_scan_empty_without_error() {
        let catalog = SkillsCatalog::scan_roots(Path::new("/nonexistent/ait-skills"), None);
        assert!(catalog.iter().next().is_none());
        assert!(catalog.errors().is_empty());
        assert_eq!(catalog.prompt_section(), None);
    }

    /// The frontmatter contract (us-22): missing, unterminated or
    /// `key: value`-violating frontmatter and absent/empty `name` /
    /// `description` each skip the skill with a surfaced error
    /// naming the file — while a valid sibling still loads and a
    /// `SKILL.md` carrying extra keys loads fine.
    #[test]
    fn malformed_skills_skip_with_surfaced_errors() {
        let root = tempfile::tempdir().expect("root");
        write_skill(root.path(), "no-frontmatter", "body only, no markers\n");
        write_skill(
            root.path(),
            "unterminated",
            "---\nname: x\ndescription: y\nextra: pair\n",
        );
        write_skill(root.path(), "no-name", "---\ndescription: y\n---\nbody\n");
        write_skill(root.path(), "no-description", "---\nname: x\n---\nbody\n");
        write_skill(
            root.path(),
            "empty-description",
            "---\nname: x\ndescription: \n---\nbody\n",
        );
        write_skill(
            root.path(),
            "not-a-pair",
            "---\nname: x\ndescription: y\njust words\n---\nbody\n",
        );
        // Extra keys are tolerated — the ecosystem's files carry
        // more than the required pair.
        write_skill(
            root.path(),
            "good",
            "---\nname: good\ndescription: fine\nargument-hint: \"why\"\n---\nbody\n",
        );

        let catalog = SkillsCatalog::scan_roots(root.path(), None);
        let names: Vec<&str> = catalog.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(names, ["good"], "only the valid skill loads");
        let errors = catalog.errors();
        assert_eq!(errors.len(), 6, "{errors:?}");
        for error in errors {
            let text = error.to_string();
            assert!(
                text.contains("SKILL.md"),
                "the error names the file: {text}"
            );
            assert!(error.path.is_file(), "the path is the SKILL.md: {error:?}");
        }
        // Errors land in the scan's sorted directory order.
        assert!(errors[0].message.contains("`description` is required"));
        assert!(errors[1].message.contains("`description` is required"));
        assert!(errors[2].message.contains("missing frontmatter"));
        assert!(errors[3].message.contains("`name` is required"));
        assert!(errors[4].message.contains("key: value"));
        assert!(errors[5].message.contains("unterminated"));
    }

    /// A directory without `SKILL.md` — and a bare file in the root
    /// — are not skills: silently ignored, never errors.
    #[test]
    fn non_skill_entries_are_ignored_not_errored() {
        let root = tempfile::tempdir().expect("root");
        fs::create_dir_all(root.path().join("not-a-skill")).expect("dir");
        fs::write(root.path().join("README.txt"), "hi").expect("file");
        write_skill(root.path(), "real", &skill_md("real", "loads"));

        let catalog = SkillsCatalog::scan_roots(root.path(), None);
        assert_eq!(catalog.iter().count(), 1);
        assert!(catalog.errors().is_empty());
    }

    /// `invoke` renders the SKILL.md body then the bundled-file
    /// manifest as absolute `read_files`-ready paths — `SKILL.md`
    /// itself excluded (its body is already the head), nested files
    /// listed. Quoted frontmatter scalars unquote.
    #[test]
    fn invoke_returns_the_body_and_bundled_manifest() {
        let root = tempfile::tempdir().expect("root");
        let dir = root.path().join("deploy");
        fs::create_dir_all(dir.join("scripts")).expect("scripts dir");
        fs::write(
            dir.join(SKILL_MD),
            "---\nname: \"deploy\"\ndescription: 'Ship it.'\n---\n\nRun the steps.\n",
        )
        .expect("SKILL.md");
        fs::write(dir.join("scripts").join("deploy.sh"), "#!/bin/sh\n").expect("script");
        fs::write(dir.join("checklist.md"), "- [ ] go\n").expect("checklist");

        let catalog = SkillsCatalog::scan_roots(root.path(), None);
        let skill = catalog.get("deploy").expect("quoted name unquotes");
        assert_eq!(skill.description, "Ship it.");
        let text = catalog.invoke("deploy").expect("invoke");
        assert!(text.starts_with("Run the steps."), "{text}");
        assert!(
            text.contains(&dir.join("scripts/deploy.sh").display().to_string()),
            "{text}"
        );
        assert!(
            text.contains(&dir.join("checklist.md").display().to_string()),
            "{text}"
        );
        assert!(
            !text.lines().any(|line| line.ends_with(SKILL_MD)),
            "the manifest excludes SKILL.md itself: {text}"
        );
    }

    /// A body-only skill renders honestly — no phantom manifest —
    /// and an unknown name errors naming the skill plus what IS
    /// loaded, so the model can pick a real one.
    #[test]
    fn invoke_empty_manifest_and_unknown_names() {
        let root = tempfile::tempdir().expect("root");
        write_skill(root.path(), "solo", &skill_md("solo", "alone"));
        let catalog = SkillsCatalog::scan_roots(root.path(), None);

        let text = catalog.invoke("solo").expect("invoke");
        assert!(text.contains("Do the thing."));
        assert!(text.contains("bundles no other files"), "{text}");

        let error = catalog.invoke("nope").expect_err("unknown name");
        assert!(error.contains("\"nope\""), "{error}");
        assert!(
            error.contains("\"solo\""),
            "available skills listed: {error}"
        );
        let error = SkillsCatalog::default().invoke("nope").expect_err("empty");
        assert!(error.contains("no skill named"), "{error}");
        assert!(!error.contains("available"), "{error}");
    }

    /// The prompt section (us-23): name, declaring layer and
    /// description per skill — plus the skipped entries' errors —
    /// `None` only when the catalog is fully empty.
    #[test]
    fn prompt_section_lists_skills_layers_and_skipped_errors() {
        let user = tempfile::tempdir().expect("user root");
        let project = tempfile::tempdir().expect("project root");
        write_skill(user.path(), "personal", &skill_md("personal", "mine"));
        write_skill(project.path(), "repo", &skill_md("repo", "the repo's"));
        write_skill(project.path(), "broken", "no markers here\n");

        let catalog = SkillsCatalog::scan_roots(user.path(), Some(project.path()));
        let section = catalog.prompt_section().expect("a section renders");
        assert!(section.contains("Available skills"), "{section}");
        assert!(
            section.contains("- personal (user config): mine"),
            "{section}"
        );
        assert!(
            section.contains("- repo (project .ait/): the repo's"),
            "{section}"
        );
        assert!(section.contains("skipped"), "{section}");
        assert!(section.contains("broken"), "{section}");
    }

    /// `invoke_call` is the tool-facing arm: a good call answers the
    /// rendered skill; bad JSON, a missing `name` and an unknown
    /// name are all tool errors — model input, never a wedge.
    #[test]
    fn invoke_call_errors_are_tool_errors() {
        let root = tempfile::tempdir().expect("root");
        write_skill(root.path(), "one", &skill_md("one", "the only"));
        let catalog = SkillsCatalog::scan_roots(root.path(), None);
        let call = |args: &str| ToolCall {
            id: String::new(),
            name: String::from("invoke_skill"),
            arguments: args.into(),
        };

        let output = catalog.invoke_call(&call(r#"{"name":"one"}"#));
        assert!(
            !output.is_error && output.text.contains("Do the thing."),
            "{:?}",
            output.text
        );
        for (args, needle) in [
            ("{not json", "not valid JSON"),
            ("{}", "`name` must be a string"),
            (r#"{"name":7}"#, "`name` must be a string"),
            (r#"{"name":"nope"}"#, "no skill named"),
        ] {
            let output = catalog.invoke_call(&call(args));
            assert!(output.is_error, "{args}");
            assert!(output.text.contains(needle), "{args}: {}", output.text);
        }
    }

    /// Only EFFECTIVE skills contribute read roots: the shadowed
    /// user skill's directory stays out — the project copy is the
    /// one the model can invoke and read (us-25).
    #[test]
    fn skill_dirs_are_the_effective_skills_roots() {
        let user = tempfile::tempdir().expect("user root");
        let project = tempfile::tempdir().expect("project root");
        write_skill(user.path(), "shared", &skill_md("shared", "user copy"));
        write_skill(user.path(), "other", &skill_md("other", "mine"));
        write_skill(
            project.path(),
            "shared",
            &skill_md("shared", "project copy"),
        );

        let catalog = SkillsCatalog::scan_roots(user.path(), Some(project.path()));
        let mut dirs = catalog.skill_dirs();
        dirs.sort();
        let mut expected = [
            project.path().join("shared").to_string_lossy().into_owned(),
            user.path().join("other").to_string_lossy().into_owned(),
        ]
        .into_iter()
        .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(dirs, expected);
    }

    /// Spec R10-T6's keep-last-good (ADR-0006's spirit): a `SKILL.md`
    /// that can no longer be parsed must not drop the skill
    /// mid-session — the merged catalog serves the previous scan's
    /// copy AND keeps the surfaced error (the toast and the prompt
    /// section still name the bad file).
    #[test]
    fn keep_last_good_restores_a_skill_whose_write_went_malformed() {
        let root = tempfile::tempdir().expect("root");
        write_skill(root.path(), "demo", &skill_md("demo", "the good version"));
        let previous = SkillsCatalog::scan_roots(root.path(), None);
        assert!(previous.errors().is_empty());

        fs::write(root.path().join("demo").join(SKILL_MD), "no markers here\n")
            .expect("malformed write");

        let mut scan = SkillsCatalog::scan_roots(root.path(), None);
        assert!(scan.get("demo").is_none(), "the malformed parse skipped");
        assert_eq!(scan.errors().len(), 1);

        scan.keep_last_good(&previous);
        let skill = scan.get("demo").expect("last-good copy serves");
        assert_eq!(skill.description, "the good version");
        assert_eq!(
            scan.errors().len(),
            1,
            "the surfaced error survives the merge"
        );
    }

    /// Only PARSE failures merge: a deleted `SKILL.md` — absent, not
    /// errored — drops the skill for real, and a fresh same-named
    /// skill loaded from another directory always beats the stale
    /// copy.
    #[test]
    fn keep_last_good_never_revives_a_deleted_or_outcompeted_skill() {
        let root = tempfile::tempdir().expect("root");
        write_skill(root.path(), "demo", &skill_md("demo", "the good version"));
        let previous = SkillsCatalog::scan_roots(root.path(), None);

        // Deletion is not a parse failure — no error, no merge.
        fs::remove_dir_all(root.path().join("demo")).expect("delete");
        let mut scan = SkillsCatalog::scan_roots(root.path(), None);
        assert!(scan.errors().is_empty());
        scan.keep_last_good(&previous);
        assert!(
            scan.get("demo").is_none(),
            "a deleted skill must not come back"
        );

        // A fresh same-named skill wins: the project's `shared` goes
        // malformed, the fresh scan still carries the user's copy —
        // restoring the stale project version would re-shadow it.
        let user = tempfile::tempdir().expect("user root");
        let project = tempfile::tempdir().expect("project root");
        write_skill(user.path(), "shared", &skill_md("shared", "user copy"));
        write_skill(
            project.path(),
            "shared",
            &skill_md("shared", "project copy"),
        );
        let previous = SkillsCatalog::scan_roots(user.path(), Some(project.path()));
        assert_eq!(previous.get("shared").unwrap().description, "project copy");

        fs::write(
            project.path().join("shared").join(SKILL_MD),
            "no markers here\n",
        )
        .expect("malformed write");
        let mut scan = SkillsCatalog::scan_roots(user.path(), Some(project.path()));
        scan.keep_last_good(&previous);
        assert_eq!(
            scan.get("shared").unwrap().description,
            "user copy",
            "the fresh same-named skill keeps the slot"
        );
        assert_eq!(scan.errors().len(), 1);
    }
}
