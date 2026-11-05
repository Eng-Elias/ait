//! Skills integration tests at the `AgentSession` channel seam
//! (Spec R10, §5.10.2): a `SkillsCatalog` scanned from tempdir
//! fixture roots — injected onto the `AgentRequest` exactly like the
//! frontend's submit-time scan — driven end-to-end through real
//! tasks against the in-process `tiny_http` SSE fixture
//! (`ait_llm::testing`, `test-fixtures`). No network anywhere
//! (standard 11).
//!
//! The catalog rides `AgentRequest.skills`, so no test touches
//! `XDG_CONFIG_HOME` for skills — but every test still points it at
//! a tempdir on the shared lock, the agent_mcp.rs pattern: the
//! per-task MCP reconcile would otherwise read a real user
//! `mcp.json` and spawn real servers.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use ait_agent::{
    AgentContext, AgentEvent, AgentRequest, AgentSession, ApprovalVerdict, SkillsCatalog,
    ToolStatus,
};
use ait_config::{SecretStore, Settings};
use ait_llm::testing::{assert_post, sse, Fixture};

/// Tests never touch a real keyring — a missing secret is `None`, so
/// fixture requests go out with no `Authorization` header.
struct NoSecrets;

impl SecretStore for NoSecrets {
    fn get(&self, _service: &str, _account: &str) -> Option<String> {
        None
    }
}

/// Bound on "the worker answered" — loopback plus a tempdir scan is
/// fast; a hang is a failure, not a slow pass.
const BOUND: Duration = Duration::from_secs(15);

/// The env lock: `XDG_CONFIG_HOME` is process-global, so every test
/// that wants the user's real config out of the MCP reconcile's
/// picture holds this for its whole body. Poisoned guards still
/// guard.
static XDG: Mutex<()> = Mutex::new(());

/// Take the env lock, surviving a poisoned guard.
fn xdg_lock() -> std::sync::MutexGuard<'static, ()> {
    XDG.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn session() -> AgentSession {
    AgentSession::with_secret_store(NoSecrets)
}

/// A `settings.toml` snapshot binding `coding_model` to the chat
/// fixture, with `profile_line` appended to profile `default` (the
/// `read_files` level under test).
fn settings(base_url: &str, profile_line: &str) -> Settings {
    Settings::from_toml(&format!(
        r#"
[agents.custom_endpoints.local]
name = "Fixture"
base_url = "{base_url}"
schema = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name = "test-model"
alias = "Test Model"
config_key = "coding-key"

[agents.execution_profiles.default]
coding_model = "coding-key"
{profile_line}
"#
    ))
    .expect("test settings must parse")
}

/// Plant `<root>/<dir>/SKILL.md` with `contents`, returning the
/// skill's directory.
fn write_skill(root: &Path, dir: &str, contents: &str) -> std::path::PathBuf {
    let dir = root.join(dir);
    std::fs::create_dir_all(&dir).expect("skill dir");
    std::fs::write(dir.join("SKILL.md"), contents).expect("SKILL.md");
    dir
}

/// A minimal valid SKILL.md.
fn skill_md(name: &str, description: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n")
}

/// A task bound to `cwd` carrying `skills` — the catalog the
/// frontend's submit-time scan produced.
fn task(settings: Settings, cwd: &str, skills: SkillsCatalog) -> AgentRequest {
    AgentRequest {
        settings,
        profile: String::from("default"),
        task: String::from("use the skills"),
        context: AgentContext {
            cwd: cwd.to_owned(),
            shell: String::from("bash"),
            os: String::from("linux"),
            output_lines: Vec::new(),
        },
        prior: Vec::new(),
        bridge: None,
        skills,
    }
}

/// A `chat.completion.chunk` `data:` line carrying a tool-call
/// fragment — `id`/`name` on the announce fragment, `arguments`
/// afterwards.
fn sse_tool_fragment(index: u32, announce: Option<(&str, &str)>, args: &str) -> String {
    let tool_call = match announce {
        Some((id, name)) => serde_json::json!({
            "index": index,
            "id": id,
            "type": "function",
            "function": {"name": name, "arguments": args}
        }),
        None => serde_json::json!({
            "index": index,
            "function": {"arguments": args}
        }),
    };
    let chunk = serde_json::json!({
        "choices": [{"index": 0, "delta": {"tool_calls": [tool_call]}, "finish_reason": null}]
    });
    format!("data: {chunk}\n\n")
}

/// A `chat.completion.chunk` `data:` line carrying a content delta.
fn sse_delta(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}}}}]}}\n\n",
        serde_json::to_string(text).expect("json literal")
    )
}

/// A chat-completions transcript: one tool turn per `(index, id,
/// name, arguments)` in `calls`, with `arguments` split into two
/// fragments so accumulation is exercised.
fn chat_tool_turn(calls: &[(u32, &str, &str, &str)]) -> String {
    let mut body = String::new();
    for (index, id, name, args) in calls {
        body.push_str(&sse_tool_fragment(*index, Some((id, name)), ""));
        let (first, second) = args.split_at(args.len() / 2);
        body.push_str(&sse_tool_fragment(*index, None, first));
        body.push_str(&sse_tool_fragment(*index, None, second));
    }
    body.push_str(
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    );
    body.push_str("data: [DONE]\n\n");
    body
}

/// A plain end-turn chat transcript.
fn chat_text_turn(text: &str) -> String {
    let mut body = sse_delta(text);
    body.push_str("data: [DONE]\n\n");
    body
}

/// The next event, or panic — every wait carries the bound.
fn next(session: &AgentSession) -> AgentEvent {
    session
        .events()
        .recv_timeout(BOUND)
        .expect("an event within the bound")
}

/// Drain events until the next `ApprovalRequest`, keeping every
/// skipped event in `drained` so completed calls' `ToolUpdate`s
/// stay assertable.
fn until_approval_collecting(
    session: &AgentSession,
    drained: &mut Vec<AgentEvent>,
) -> (String, String, String) {
    loop {
        match next(session) {
            AgentEvent::ApprovalRequest { call, summary } => return (call.id, call.name, summary),
            event @ (AgentEvent::Done
            | AgentEvent::Cancelled
            | AgentEvent::BudgetReached
            | AgentEvent::Error(_)) => panic!("expected an ApprovalRequest, got {event:?}"),
            event => drained.push(event),
        }
    }
}

/// Drain events until a terminal `Done`/`Cancelled`/`BudgetReached`/
/// `Error` or the bound.
fn collect(session: &AgentSession) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    loop {
        match session.events().recv_timeout(BOUND) {
            Ok(
                event @ (AgentEvent::Done
                | AgentEvent::Cancelled
                | AgentEvent::BudgetReached
                | AgentEvent::Error(_)),
            ) => {
                events.push(event);
                return events;
            }
            Ok(event) => events.push(event),
            Err(_) => panic!("timed out waiting for a terminal event; got {events:?}"),
        }
    }
}

/// The full result text of call `id`'s terminal `ToolUpdate`.
fn result_of<'a>(events: &'a [AgentEvent], id: &str) -> &'a str {
    events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolUpdate {
                call_id,
                result: Some(text),
                ..
            } if call_id == id => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no terminal ToolUpdate for {id}: {events:?}"))
}

/// The status of call `id`'s terminal `ToolUpdate`.
fn status_of(events: &[AgentEvent], id: &str) -> ToolStatus {
    events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolUpdate {
                call_id,
                status,
                result: Some(_),
                ..
            } if call_id == id => Some(*status),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no terminal ToolUpdate for {id}: {events:?}"))
}

/// End to end (Spec R10 us-23/24/25/29): the "Available skills"
/// prompt section names both layers' skills, `invoke_skill` rides
/// the approve card and returns the SKILL.md body — credential-
/// bearing, so the redacted result proves §5.10.3's tail covers
/// skills too — plus the bundled-file manifest, and a `read_files`
/// on one of those bundled files — OUTSIDE the session cwd —
/// succeeds because the skill's directory joined the task's read
/// roots.
#[test]
fn invoke_skill_returns_body_and_manifest_and_reaches_bundled_files() {
    let _lock = xdg_lock();
    let xdg = tempfile::tempdir().expect("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.path());

    // The two layers: a user-config skill and a project `.ait/`
    // skill, both outside the session cwd.
    let user_root = tempfile::tempdir().expect("user skills root");
    write_skill(
        user_root.path(),
        "personal",
        &skill_md("personal", "my own helper", "Personal body."),
    );
    let project_root = tempfile::tempdir().expect("project skills root");
    let deploy_dir = write_skill(
        project_root.path(),
        "deploy",
        &skill_md(
            "deploy",
            "Ship the build.",
            "Run the playbook. Key AKIAIOSFODNN7EXAMPLE stays secret.",
        ),
    );
    let bundled = deploy_dir.join("scripts").join("notes.txt");
    std::fs::create_dir_all(bundled.parent().expect("scripts dir")).expect("scripts");
    std::fs::write(&bundled, "BUNDLED-NOTE-7\n").expect("bundled file");
    let catalog = SkillsCatalog::scan_roots(user_root.path(), Some(project_root.path()));
    let cwd = tempfile::tempdir().expect("cwd");

    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "invoke_skill",
            r#"{"name":"deploy"}"#,
        )])),
        sse(&chat_tool_turn(&[(
            0,
            "call_2",
            "read_files",
            &format!(
                "{{\"paths\":[{}]}}",
                serde_json::to_string(&bundled.to_string_lossy()).expect("path")
            ),
        )])),
        sse(&chat_text_turn("all done")),
    ]);
    let session = session();
    // `read_files` unset → both calls ride the approve card.
    session
        .submit(task(
            settings(&fixture.base_url, ""),
            cwd.path().to_str().expect("utf-8 tempdir"),
            catalog,
        ))
        .expect("submit");

    // The invoke card renders the bare skill name.
    let mut events = Vec::new();
    let (id, name, summary) = until_approval_collecting(&session, &mut events);
    assert_eq!(name, "invoke_skill");
    assert_eq!(id, "call_1");
    assert_eq!(summary, "deploy");
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    // The bundled file lives outside cwd — it reads only because
    // the skill's directory joined the task's read roots (us-25).
    let (id, name, _summary) = until_approval_collecting(&session, &mut events);
    assert_eq!(name, "read_files");
    assert_eq!(id, "call_2");
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    events.extend(collect(&session));
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    let result = result_of(&events, "call_1");
    assert!(result.contains("Run the playbook."), "{result}");
    // §5.10.3's tail covers skill text like every other result
    // (us-29): the credential never reaches the conversation.
    assert!(result.contains("[REDACTED]"), "{result}");
    assert!(!result.contains("AKIAIOSFODNN7EXAMPLE"), "{result}");
    // The manifest lists the bundled file as an absolute,
    // read_files-ready path — SKILL.md itself excluded.
    assert!(result.contains(&bundled.display().to_string()), "{result}");
    assert!(!result.lines().any(|l| l.ends_with("SKILL.md")), "{result}");
    assert_eq!(status_of(&events, "call_1"), ToolStatus::Done);
    assert_eq!(
        result_of(&events, "call_2"),
        format!("{}:\nBUNDLED-NOTE-7\n", bundled.display()),
        "{events:?}"
    );

    // Request 1's system prompt carries the section: both layers,
    // name + description + declaring layer (us-23).
    let first = fixture.captured();
    assert_post(&first);
    assert!(first.body.contains("Available skills"), "{}", first.body);
    assert!(first.body.contains("deploy"), "{}", first.body);
    assert!(first.body.contains("Ship the build."), "{}", first.body);
    assert!(first.body.contains("project .ait/"), "{}", first.body);
    assert!(first.body.contains("personal"), "{}", first.body);
    assert!(first.body.contains("user config"), "{}", first.body);

    // Request 2 replays the invoke result redacted; request 3
    // carries the bundled file's contents.
    let second = fixture.captured();
    assert!(second.body.contains("invoke_skill"), "{}", second.body);
    assert!(second.body.contains("[REDACTED]"), "{}", second.body);
    assert!(
        !second.body.contains("AKIAIOSFODNN7EXAMPLE"),
        "the unredacted skill body never re-enters: {}",
        second.body
    );
    let third = fixture.captured();
    assert!(third.body.contains("BUNDLED-NOTE-7"), "{}", third.body);
    fixture.assert_no_more_requests();
}

/// `read_files = "never"` forbids skills outright (Spec R10 us-28):
/// the profile-denial text IS the tool result and no approve card
/// ever parks.
#[test]
fn read_files_never_denies_invoke_skill() {
    let _lock = xdg_lock();
    let xdg = tempfile::tempdir().expect("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.path());
    let root = tempfile::tempdir().expect("skills root");
    write_skill(
        root.path(),
        "deploy",
        &skill_md("deploy", "Ship it.", "Run the playbook."),
    );
    let catalog = SkillsCatalog::scan_roots(root.path(), None);
    let cwd = tempfile::tempdir().expect("cwd");

    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "invoke_skill",
            r#"{"name":"deploy"}"#,
        )])),
        sse(&chat_text_turn("understood")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "read_files = \"never\""),
            cwd.path().to_str().expect("utf-8 tempdir"),
            catalog,
        ))
        .expect("submit");

    let events = collect(&session);
    assert_eq!(
        result_of(&events, "call_1"),
        "denied by execution profile default"
    );
    assert_eq!(status_of(&events, "call_1"), ToolStatus::Denied);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
}

/// `agent_decides` honors the call's own `safety` flag on
/// `invoke_skill` like every `read_files` tool (Spec R10 us-28):
/// `"safe"` runs ungated, `"unsafe"` parks the approve card — and
/// the declared schema carries the flag.
#[test]
fn agent_decides_honors_the_safety_flag_on_invoke_skill() {
    let _lock = xdg_lock();
    let xdg = tempfile::tempdir().expect("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.path());
    let root = tempfile::tempdir().expect("skills root");
    write_skill(
        root.path(),
        "deploy",
        &skill_md("deploy", "Ship it.", "Run the playbook."),
    );
    let catalog = SkillsCatalog::scan_roots(root.path(), None);
    let cwd = tempfile::tempdir().expect("cwd");

    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "invoke_skill",
            r#"{"name":"deploy","safety":"safe"}"#,
        )])),
        sse(&chat_tool_turn(&[(
            0,
            "call_2",
            "invoke_skill",
            r#"{"name":"deploy","safety":"unsafe"}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "read_files = \"agent_decides\""),
            cwd.path().to_str().expect("utf-8 tempdir"),
            catalog,
        ))
        .expect("submit");

    // The `"safe"` call runs ungated; the `"unsafe"` one asks.
    let mut events = Vec::new();
    let (id, name, _summary) = until_approval_collecting(&session, &mut events);
    assert_eq!(name, "invoke_skill");
    assert_eq!(id, "call_2");
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    events.extend(collect(&session));
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    assert!(result_of(&events, "call_1").contains("Run the playbook."));
    assert!(result_of(&events, "call_2").contains("Run the playbook."));

    // The injected `safety` argument rode the declaration (R07-T2's
    // transform applies to invoke_skill like every read tool).
    let first = fixture.captured();
    assert!(first.body.contains("invoke_skill"), "{}", first.body);
    assert!(first.body.contains("safety"), "{}", first.body);
}

/// An unknown skill name is a tool error — model input, never a
/// task wedge (Spec R10, acceptance: the task finishes and the model
/// reads which skills exist).
#[test]
fn an_unknown_skill_is_a_tool_error_not_a_wedge() {
    let _lock = xdg_lock();
    let xdg = tempfile::tempdir().expect("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.path());
    let root = tempfile::tempdir().expect("skills root");
    write_skill(
        root.path(),
        "deploy",
        &skill_md("deploy", "Ship it.", "Run the playbook."),
    );
    let catalog = SkillsCatalog::scan_roots(root.path(), None);
    let cwd = tempfile::tempdir().expect("cwd");

    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "invoke_skill",
            r#"{"name":"ghost"}"#,
        )])),
        sse(&chat_text_turn("no such skill, moving on")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "read_files = \"always_allow\""),
            cwd.path().to_str().expect("utf-8 tempdir"),
            catalog,
        ))
        .expect("submit");

    let events = collect(&session);
    let result = result_of(&events, "call_1");
    assert!(result.contains("no skill named \"ghost\""), "{result}");
    assert!(result.contains("\"deploy\""), "available listed: {result}");
    assert_eq!(status_of(&events, "call_1"), ToolStatus::Failed);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
}

/// A malformed SKILL.md is skipped with a surfaced error — the
/// prompt section names it rather than silently absenting it — and
/// the valid sibling still loads and invokes (Spec R10 us-22).
#[test]
fn a_malformed_skill_surfaces_in_the_prompt_and_never_wedges() {
    let _lock = xdg_lock();
    let xdg = tempfile::tempdir().expect("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.path());
    let root = tempfile::tempdir().expect("skills root");
    write_skill(root.path(), "broken", "no frontmatter here\n");
    write_skill(
        root.path(),
        "good",
        &skill_md("good", "the valid one", "Good body."),
    );
    let catalog = SkillsCatalog::scan_roots(root.path(), None);
    assert_eq!(catalog.errors().len(), 1);
    let cwd = tempfile::tempdir().expect("cwd");

    let fixture = Fixture::serve(vec![sse(&chat_text_turn("noted"))]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "read_files = \"always_allow\""),
            cwd.path().to_str().expect("utf-8 tempdir"),
            catalog,
        ))
        .expect("submit");

    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    let first = fixture.captured();
    assert!(first.body.contains("Available skills"), "{}", first.body);
    assert!(first.body.contains("good"), "{}", first.body);
    assert!(first.body.contains("skipped"), "{}", first.body);
    assert!(first.body.contains("broken"), "{}", first.body);
}
