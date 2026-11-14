//! Tests at the `mcp.json` contract seam (Spec R10 / §5.10.1–2):
//! `McpConfig::from_json` is the pure parse/validate seam mirroring
//! `Settings::from_toml`; `mcp_path` resolves beside `settings_path`;
//! `project_ait_dir` walks up from a session cwd like `find_agents_md`
//! walks for `AGENTS.md`; `load_mcp` merges the user layer with the
//! project `.ait/mcp.json` into one origin-labeled view.
//!
//! Env-overriding tests share `ENV_LOCK` — `mcp_path` reads the process
//! environment, so tests that mutate it must not run concurrently.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ait_config::{load_mcp, mcp_path, project_ait_dir, McpConfig, McpServerOrigin};

/// Serializes tests that mutate the config-path environment.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Runs `body` with the config-path env pointed at the given dirs —
/// `XDG_CONFIG_HOME` → `xdg`, `%APPDATA%` (the Windows config root —
/// inert elsewhere) → `appdata`, and the home lookup → `home` (`HOME`
/// plus `USERPROFILE`, so `env::home_dir()` is pinned on Windows too).
/// `None` unsets. Previous values restore even on panic, so a failure
/// cannot poison the next test.
fn with_env<T>(
    xdg: Option<&Path>,
    appdata: Option<&Path>,
    home: Option<&Path>,
    body: impl FnOnce() -> T,
) -> T {
    // `into_inner` rides out a poisoned lock — one panicking env test
    // must not cascade into the rest.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let saved =
        ["XDG_CONFIG_HOME", "APPDATA", "HOME", "USERPROFILE"].map(|name| (name, env::var_os(name)));
    let home = home.map(|p| p.as_os_str().to_os_string());
    set_or_remove("XDG_CONFIG_HOME", xdg.map(|p| p.as_os_str().to_os_string()));
    set_or_remove("APPDATA", appdata.map(|p| p.as_os_str().to_os_string()));
    set_or_remove("HOME", home.clone());
    set_or_remove("USERPROFILE", home);

    struct Restore([(&'static str, Option<OsString>); 4]);
    impl Drop for Restore {
        fn drop(&mut self) {
            for (name, value) in std::mem::take(&mut self.0) {
                set_or_remove(name, value);
            }
        }
    }
    let _restore = Restore(saved);

    body()
}

fn set_or_remove(name: &str, value: Option<OsString>) {
    match value {
        Some(value) => env::set_var(name, value),
        None => env::remove_var(name),
    }
}

/// Writes `contents` to `dir.join("mcp.json")`, creating `dir`.
fn write_mcp_json(dir: &Path, contents: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("mcp.json");
    std::fs::write(&path, contents).unwrap();
    path
}

// --- McpConfig::from_json — the pure seam -------------------------

#[test]
fn from_json_parses_the_standard_shape() {
    let config = McpConfig::from_json(
        r#"{
            "mcpServers": {
                "db": {
                    "command": "pg-mcp",
                    "args": ["--dsn", "postgres://localhost/app"],
                    "env": {"PGPASS": "secret"}
                },
                "fs": {"command": "fs-mcp"}
            }
        }"#,
    )
    .unwrap();

    let db = &config.servers["db"];
    assert_eq!(db.command, "pg-mcp");
    assert_eq!(db.args, ["--dsn", "postgres://localhost/app"]);
    assert_eq!(db.env["PGPASS"], "secret");

    // `args`/`env` are optional.
    let fs = &config.servers["fs"];
    assert_eq!(fs.command, "fs-mcp");
    assert!(fs.args.is_empty());
    assert!(fs.env.is_empty());
}

#[test]
fn from_json_missing_mcpservers_yields_no_servers() {
    assert!(McpConfig::from_json("{}").unwrap().servers.is_empty());
}

#[test]
fn from_json_missing_command_names_the_server() {
    let err = McpConfig::from_json(r#"{"mcpServers": {"db": {"args": []}}}"#).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("db"), "actual: {msg}");
    assert!(msg.contains("command"), "actual: {msg}");
}

#[test]
fn from_json_empty_command_names_the_key_path() {
    for command in ["", "   "] {
        let input = format!(r#"{{"mcpServers": {{"db": {{"command": {command:?}}}}}}}"#);
        let err = McpConfig::from_json(&input).unwrap_err();
        assert_eq!(err.to_string(), "mcpServers.db.command: must not be empty");
    }
}

#[test]
fn from_json_unknown_server_field_is_denied() {
    let err =
        McpConfig::from_json(r#"{"mcpServers": {"db": {"command": "pg-mcp", "timeout": 30}}}"#)
            .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("mcpServers.db"), "actual: {msg}");
    assert!(msg.contains("timeout"), "actual: {msg}");
}

#[test]
fn from_json_unknown_root_field_is_denied() {
    let err = McpConfig::from_json(r#"{"mcpServers": {}, "mcp_apps": {}}"#).unwrap_err();
    assert!(err.to_string().contains("mcp_apps"), "actual: {err}");
}

#[test]
fn from_json_url_entry_fails_stdio_only_naming_the_server() {
    let err =
        McpConfig::from_json(r#"{"mcpServers": {"db": {"url": "https://mcp.example.com/sse"}}}"#)
            .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("stdio"), "actual: {msg}");
    assert!(msg.contains("db"), "actual: {msg}");
}

#[test]
fn from_json_malformed_json_is_an_error() {
    let err = McpConfig::from_json(r#"{"mcpServers": {"db": "#).unwrap_err();
    assert!(!err.to_string().is_empty());
}

#[test]
fn from_json_non_object_server_is_an_error() {
    let err = McpConfig::from_json(r#"{"mcpServers": {"db": "pg-mcp"}}"#).unwrap_err();
    assert!(err.to_string().contains("db"), "actual: {err}");
}

// --- mcp_path — XDG resolution beside settings_path ----------------

#[test]
fn mcp_path_resolves_beside_settings() {
    let dirs = tempfile::tempdir().unwrap();
    let xdg = dirs.path().join("xdg");
    let appdata = dirs.path().join("appdata");
    let home = dirs.path().join("home");
    with_env(Some(&xdg), Some(&appdata), Some(&home), || {
        assert_eq!(mcp_path(), xdg.join("ait").join("mcp.json"));
    });
}

/// The platform default when XDG is unset: `~/.config` on unix,
/// `%APPDATA%` verbatim on Windows (§5.14) — mcp.json always lands
/// beside `settings.toml`.
#[test]
fn mcp_path_uses_the_platform_config_root() {
    let dirs = tempfile::tempdir().unwrap();
    let appdata = dirs.path().join("appdata");
    let home = dirs.path().join("home");
    with_env(None, Some(&appdata), Some(&home), || {
        let root = if cfg!(windows) {
            appdata.clone()
        } else {
            home.join(".config")
        };
        assert_eq!(mcp_path(), root.join("ait").join("mcp.json"));
    });
}

// --- project_ait_dir — the nearest-.ait upward walk -----------------

#[test]
fn project_ait_dir_finds_the_nearest_ait_dir() {
    let dirs = tempfile::tempdir().unwrap();
    let outer = dirs.path().join(".ait");
    let inner = dirs.path().join("a").join(".ait");
    let cwd = dirs.path().join("a").join("b").join("c");
    std::fs::create_dir_all(&outer).unwrap();
    std::fs::create_dir_all(&inner).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();

    // The *nearest* .ait wins over one further up the tree.
    assert_eq!(project_ait_dir(&cwd).as_deref(), Some(inner.as_path()));
}

#[test]
fn project_ait_dir_accepts_cwd_itself() {
    let dirs = tempfile::tempdir().unwrap();
    let ait = dirs.path().join(".ait");
    std::fs::create_dir_all(&ait).unwrap();
    assert_eq!(project_ait_dir(dirs.path()).as_deref(), Some(ait.as_path()));
}

#[test]
fn project_ait_dir_is_none_without_one() {
    let dirs = tempfile::tempdir().unwrap();
    let cwd = dirs.path().join("a").join("b");
    std::fs::create_dir_all(&cwd).unwrap();
    assert_eq!(project_ait_dir(&cwd), None);
}

#[test]
fn project_ait_dir_ignores_a_file_named_ait() {
    let dirs = tempfile::tempdir().unwrap();
    // A regular file named `.ait` is not the project config layer.
    std::fs::write(dirs.path().join(".ait"), "not a dir").unwrap();
    let cwd = dirs.path().join("sub");
    std::fs::create_dir_all(&cwd).unwrap();
    assert_eq!(project_ait_dir(&cwd), None);
}

// --- load_mcp — the merged, origin-labeled view ---------------------

#[test]
fn load_mcp_missing_everything_is_empty() {
    let dirs = tempfile::tempdir().unwrap();
    let xdg = dirs.path().join("xdg");
    let cwd = dirs.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    with_env(Some(&xdg), None, None, || {
        assert!(load_mcp(&cwd).unwrap().servers.is_empty());
    });
}

#[test]
fn load_mcp_reads_the_user_layer() {
    let dirs = tempfile::tempdir().unwrap();
    let xdg = dirs.path().join("xdg");
    write_mcp_json(
        &xdg.join("ait"),
        r#"{"mcpServers": {"db": {"command": "pg-mcp"}}}"#,
    );
    let cwd = dirs.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();

    with_env(Some(&xdg), None, None, || {
        let merged = load_mcp(&cwd).unwrap();
        assert_eq!(merged.servers.len(), 1);
        assert_eq!(merged.servers["db"].origin, McpServerOrigin::User);
        assert_eq!(merged.servers["db"].server.command, "pg-mcp");
    });
}

#[test]
fn load_mcp_reads_the_project_layer() {
    let dirs = tempfile::tempdir().unwrap();
    let xdg = dirs.path().join("xdg");
    write_mcp_json(
        &dirs.path().join("project").join(".ait"),
        r#"{"mcpServers": {"lint": {"command": "lint-mcp", "args": ["--strict"]}}}"#,
    );
    let cwd = dirs.path().join("project").join("src").join("deep");
    std::fs::create_dir_all(&cwd).unwrap();

    with_env(Some(&xdg), None, None, || {
        let merged = load_mcp(&cwd).unwrap();
        assert_eq!(merged.servers.len(), 1);
        assert_eq!(merged.servers["lint"].origin, McpServerOrigin::Project);
        assert_eq!(merged.servers["lint"].server.args, ["--strict"]);
    });
}

#[test]
fn load_mcp_merges_both_layers() {
    let dirs = tempfile::tempdir().unwrap();
    let xdg = dirs.path().join("xdg");
    write_mcp_json(
        &xdg.join("ait"),
        r#"{"mcpServers": {"db": {"command": "pg-mcp"}}}"#,
    );
    write_mcp_json(
        &dirs.path().join("project").join(".ait"),
        r#"{"mcpServers": {"lint": {"command": "lint-mcp"}}}"#,
    );
    let cwd = dirs.path().join("project").join("src");
    std::fs::create_dir_all(&cwd).unwrap();

    with_env(Some(&xdg), None, None, || {
        let merged = load_mcp(&cwd).unwrap();
        assert_eq!(merged.servers.len(), 2);
        assert_eq!(merged.servers["db"].origin, McpServerOrigin::User);
        assert_eq!(merged.servers["lint"].origin, McpServerOrigin::Project);
    });
}

#[test]
fn load_mcp_project_shadows_a_same_named_user_server() {
    let dirs = tempfile::tempdir().unwrap();
    let xdg = dirs.path().join("xdg");
    write_mcp_json(
        &xdg.join("ait"),
        r#"{"mcpServers": {"db": {"command": "user-pg-mcp"}}}"#,
    );
    write_mcp_json(
        &dirs.path().join("project").join(".ait"),
        r#"{"mcpServers": {"db": {"command": "project-pg-mcp"}}}"#,
    );
    let cwd = dirs.path().join("project").join("src");
    std::fs::create_dir_all(&cwd).unwrap();

    with_env(Some(&xdg), None, None, || {
        let merged = load_mcp(&cwd).unwrap();
        // The more specific layer wins the name collision.
        assert_eq!(merged.servers.len(), 1);
        assert_eq!(merged.servers["db"].origin, McpServerOrigin::Project);
        assert_eq!(merged.servers["db"].server.command, "project-pg-mcp");
    });
}

#[test]
fn load_mcp_malformed_user_file_names_its_path() {
    let dirs = tempfile::tempdir().unwrap();
    let xdg = dirs.path().join("xdg");
    let bad = write_mcp_json(&xdg.join("ait"), "{not json");
    let cwd = dirs.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();

    with_env(Some(&xdg), None, None, || {
        let err = load_mcp(&cwd).unwrap_err();
        assert!(
            err.to_string().contains(&*bad.to_string_lossy()),
            "actual: {err}"
        );
    });
}

#[test]
fn load_mcp_invalid_project_file_names_its_path() {
    // Valid JSON but an invalid model still names the file it came from.
    let dirs = tempfile::tempdir().unwrap();
    let xdg = dirs.path().join("xdg");
    let bad = write_mcp_json(
        &dirs.path().join("project").join(".ait"),
        r#"{"mcpServers": {"db": {"url": "https://mcp.example.com"}}}"#,
    );
    let cwd = dirs.path().join("project");
    std::fs::create_dir_all(cwd.join("src")).unwrap();

    with_env(Some(&xdg), None, None, || {
        let err = load_mcp(&cwd.join("src")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(&*bad.to_string_lossy()), "actual: {msg}");
        assert!(msg.contains("stdio"), "actual: {msg}");
    });
}

#[test]
fn load_mcp_unreadable_file_is_an_error_naming_the_path() {
    let dirs = tempfile::tempdir().unwrap();
    let xdg = dirs.path().join("xdg");
    // A directory at the mcp.json path fails reads on every platform.
    let path: PathBuf = xdg.join("ait").join("mcp.json");
    std::fs::create_dir_all(&path).unwrap();
    let cwd = dirs.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();

    with_env(Some(&xdg), None, None, || {
        let err = load_mcp(&cwd).unwrap_err();
        assert!(err.to_string().contains("mcp.json"), "actual: {err}");
    });
}
