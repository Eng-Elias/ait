//! The skills roots (REQUIREMENTS.md §5.10.2, Spec R10): where
//! `SKILL.md` directories live. Two layers, mirroring `mcp.json`'s —
//! the user config tree and the project-local `.ait/` layer the
//! nearest-`.ait` walk finds.
//!
//! This module resolves the roots only; scanning, frontmatter
//! parsing and the catalog itself live in ait-agent's `skills`
//! module (the spec's "`ait-agent` — skills module" decision), which
//! consumes these paths per task snapshot.

use std::env;
use std::path::{Path, PathBuf};

use crate::load::config_dir;
use crate::mcp::project_ait_dir;

/// The `skills/` directory name under either layer's root.
const SKILLS_DIR: &str = "skills";

/// The user-layer skills root (§5.10.2):
/// `$XDG_CONFIG_HOME/ait/skills/`, falling back to
/// `~/.config/ait/skills/` on unix and `%APPDATA%\ait\skills\` on
/// Windows (§5.14) — beside [`settings_path`](crate::settings_path)
/// and [`mcp_path`](crate::mcp_path), resolved by the same
/// [`config_dir`] machinery.
pub fn user_skills_dir() -> PathBuf {
    config_dir(
        env::var_os("XDG_CONFIG_HOME"),
        env::var_os("APPDATA"),
        env::home_dir().unwrap_or_default(),
        cfg!(windows),
    )
    .join("ait")
    .join(SKILLS_DIR)
}

/// The project-layer skills root (§5.10.2): `skills/` under the
/// nearest `.ait` directory to `cwd` — the same
/// [`project_ait_dir`] discovery `mcp.json` uses. `None` when no
/// `.ait` exists.
pub fn project_skills_dir(cwd: &Path) -> Option<PathBuf> {
    project_ait_dir(cwd).map(|dir| dir.join(SKILLS_DIR))
}
