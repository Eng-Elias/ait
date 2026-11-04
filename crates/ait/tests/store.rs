//! e2e for §5.8 persistence — terminal-session + block metadata,
//! agent-conversation transcripts, `update_plan`, the `Alt+R` picker,
//! summarization and mid-call-crash resume (Spec R08, tickets
//! R08-T2/T3/T5/T6/T8). Code-first for the same reason the R03
//! fixtures are: a TOML scenario's `env` values are literal strings,
//! so `XDG_DATA_HOME` cannot be interpolated to a per-test tempdir —
//! and the store assertions live outside the screen anyway: the rows
//! are read back through [`ait_store::Store`] against the database
//! file the spawned app left in the fixture dir.
//!
//! The app runs real bash so shell integration emits the OSC 133
//! marks + `preexec` hooks a `blocks` row needs (the
//! `shell_integration.rs` precedent: `SHELL` selects the spawned
//! shell).

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use ait_agent::TranscriptItem;
use ait_llm::testing::{sse, Fixture};
use ait_store::Store;
use ait_testkit::{
    ait_bin, json_string, panel_has_line, wait_fresh_prompt, E2e, Size, SpawnConfig,
};

/// 80x24: row 0 is the tab bar, row 23 the status line.
const SIZE: Size = Size {
    columns: 80,
    lines: 24,
};
const STATUS_ROW: usize = 23;

/// Spawn `ait` on bash with `XDG_DATA_HOME` pinned to `xdg` — appended
/// after the harness's pinned env, so it applies. `xdg` is a fresh
/// tempdir; the app's `store.db` lands at `xdg/ait/store.db`.
fn spawn_ait_with_data(xdg: &Path) -> E2e {
    let mut config = SpawnConfig::new(ait_bin!(), &[], SIZE);
    config.env.push((
        OsString::from("XDG_DATA_HOME"),
        xdg.as_os_str().to_os_string(),
    ));
    config
        .env
        .push((OsString::from("SHELL"), OsString::from("/bin/bash")));
    E2e::spawn(config).expect("spawn the ait binary")
}

/// Type `command` + Enter and wait out its execution: its own echo
/// must render first — until it does, the previous bare prompt still
/// satisfies [`wait_fresh_prompt`] — then a fresh bare prompt, which
/// draws only after the command finished and its `D` mark sealed the
/// block (the `run_line` recipe from `shell_integration.rs`).
fn run_line(app: &mut E2e, command: &str, echo_probe: &str) {
    app.type_text(&format!("{command}\n"));
    app.wait_for_screen(&format!("the echo of `{command}`"), |app| {
        app.screen_text()
            .lines()
            .any(|line| line.contains(echo_probe))
    });
    wait_fresh_prompt(app, "the prompt back");
}

/// The `Store` the app left behind in the fixture dir — opened the
/// same way a second `ait` instance would (WAL makes the rows
/// visible).
fn open_store(xdg: &Path) -> Store {
    Store::open(xdg.join("ait").join("store.db")).expect("open the app's store.db")
}

/// Spec R08 us-23 + ticket: running commands in a session leaves one
/// `terminal_sessions` row per spawned tab — `ended_at` stamped on tab
/// close (`Alt+W`) and on app exit (`Alt+Q`) — and one `blocks` row per
/// sealed block, carrying the command, exit code, duration and
/// wall-clock bounds.
#[test]
fn commands_leave_session_and_block_rows() {
    if !Path::new("/bin/bash").exists() {
        return; // bash isn't installed everywhere tests run
    }
    let xdg = tempfile::tempdir().expect("fixture tempdir");
    let mut app = spawn_ait_with_data(xdg.path());
    app.wait_for_screen("the bash chrome", |app| {
        app.line(0).contains("1:bash") && app.line(STATUS_ROW).contains("· bash ·")
    });
    wait_fresh_prompt(&mut app, "the first prompt");
    let spawn_cwd = app.cwd().to_path_buf();

    run_line(&mut app, "echo R08MARKER", "echo R08MARKER");
    run_line(&mut app, "false", "false");
    // `seq`'s output — `9000`, `9001`, `9002` — exists only as output;
    // its middle line is the no-output-text sentinel asserted below.
    run_line(&mut app, "seq 9000 9002", "seq 9000 9002");

    // A second tab gets its own session row; `Alt+W` closes it —
    // `ended_at` on tab close, before app exit.
    app.press("Alt+T");
    app.wait_for_screen("the second tab's chrome", |app| {
        app.line(0).contains("2:bash")
    });
    wait_fresh_prompt(&mut app, "the new tab's prompt");
    run_line(&mut app, "echo TAB2", "echo TAB2");
    app.press("Alt+W");
    app.wait_for_screen("the first tab active again", |app| {
        app.line(0).contains("1:bash") && !app.line(0).contains("2:bash")
    });

    // The block writes are synchronous per drain, so the rows are
    // durable the moment `Alt+Q` finishes app exit.
    app.press("Alt+Q");
    let status = app.wait_for_exit();
    assert!(status.success(), "expected a clean exit, got {status:?}");

    let store = open_store(xdg.path());
    let sessions = store.list_terminal_sessions().expect("list sessions");
    assert_eq!(sessions.len(), 2, "one row per spawned tab");
    let (first, second) = (&sessions[0], &sessions[1]);
    assert_eq!(first.shell, "bash");
    assert_eq!(first.cwd, spawn_cwd.to_string_lossy(), "cwd at spawn");
    for session in [&first, &second] {
        let ended = session
            .ended_at
            .expect("ended_at stamped — Alt+W or app exit");
        assert!(session.started_at <= ended, "bounds are ordered");
    }

    // Tab 1's blocks: the two commands, in order.
    let blocks = store.list_blocks(first.id).expect("list blocks");
    let commands: Vec<Option<&str>> = blocks.iter().map(|b| b.command.as_deref()).collect();
    let marker = commands
        .iter()
        .position(|c| *c == Some("echo R08MARKER"))
        .expect("the echo block persisted");
    assert_eq!(blocks[marker].exit, Some(0));
    assert_eq!(
        blocks[marker].cwd.as_deref(),
        Some(spawn_cwd.to_string_lossy().as_ref()),
        "the tab's cwd at preexec"
    );
    assert_eq!(
        commands[marker + 1],
        Some("false"),
        "the next sealed block is the second command"
    );
    assert_eq!(blocks[marker + 1].exit, Some(1), "false's exit code");
    for block in &blocks[marker..=marker + 1] {
        assert!(block.duration.is_some(), "D−C duration stored");
        let (started, ended) = (
            block.started_at.expect("started_at stored"),
            block.ended_at.expect("ended_at stored"),
        );
        assert!(started <= ended, "the wall-clock bounds are ordered");
    }

    // Tab 2's blocks live against its own session row.
    let second_blocks = store.list_blocks(second.id).expect("list tab-2 blocks");
    assert!(
        second_blocks
            .iter()
            .any(|b| b.command.as_deref() == Some("echo TAB2")),
        "tab 2's command persisted under its session: {second_blocks:?}"
    );

    // Metadata only: no stored command ever carries output text —
    // `seq`'s middle row only ever existed as output, never as a
    // typed line.
    assert!(
        commands
            .iter()
            .all(|c| !c.is_some_and(|c| c.contains("9001"))),
        "output text never lands in a block row: {commands:?}"
    );
}

/// Spec R08 us-25 + ticket: a store that can't open degrades to a
/// toast and a fully functional terminal — never a crash, never a
/// render stall. `XDG_DATA_HOME` pointed at a regular FILE makes
/// `create_dir_all` fail on every spawn.
#[test]
fn an_unopenable_store_toasts_and_the_terminal_still_works() {
    let xdg = tempfile::tempdir().expect("fixture tempdir");
    let blocker = xdg.path().join("blocked");
    fs::write(&blocker, b"not a directory").expect("write the blocker file");

    let mut config = SpawnConfig::new(ait_bin!(), &[], SIZE);
    config.env.push((
        OsString::from("XDG_DATA_HOME"),
        blocker.as_os_str().to_os_string(),
    ));
    let mut app = E2e::spawn(config).expect("spawn the ait binary");

    app.wait_for_screen("the store-unavailable toast", |app| {
        app.line(STATUS_ROW).contains("store unavailable")
    });

    // The app is a terminal first: commands still run after the toast.
    wait_fresh_prompt(&mut app, "the first prompt");
    run_line(&mut app, "echo STILL-WORKS", "echo STILL-WORKS");

    app.press("Alt+Q");
    let status = app.wait_for_exit();
    assert!(
        status.success(),
        "the unpersisted run still exits cleanly, got {status:?}"
    );
}

// ─── R08-T3: transcript mirroring (agent conversations) ─────────────

/// Write `contents` as `<dir>/ait/settings.toml`, creating the `ait/`
/// directory first (the `agent_panel.rs` precedent — the watcher
/// needs the parent to exist at spawn).
fn write_agent_settings(dir: &Path, contents: &str) {
    let config = dir.join("ait");
    fs::create_dir_all(&config).expect("create the fixture config directory");
    fs::write(config.join("settings.toml"), contents).expect("write fixture settings.toml");
}

/// A `settings.toml` whose `coding_model` binds one model at the
/// fixture's loopback `base_url` — no `api_key_env`, so requests go
/// out unauthenticated. `extra_profile` carries additional
/// `[agents.execution_profiles.default]` lines (a
/// `context_window_limit`, a tool level) when a test wants them.
fn agent_settings_with_profile(base_url: &str, extra_profile: &str) -> String {
    format!(
        r#"
[agents.custom_endpoints.local]
name     = "Fixture"
base_url = "{base_url}"
schema   = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name       = "fixture-model"
alias      = "Fixture Model"
config_key = "coding-key"

[agents.execution_profiles.default]
coding_model = "coding-key"
{extra_profile}
"#
    )
}

/// The plain fixture binding — no extra profile fields.
fn agent_settings_with_fixture(base_url: &str) -> String {
    agent_settings_with_profile(base_url, "")
}

/// Spawn `ait` with `XDG_CONFIG_HOME` (settings → fixture) and
/// `XDG_DATA_HOME` (the store) pinned to fresh tempdirs, on `/bin/sh`
/// — dash takes no injection, so `run_command` rides the typed-
/// sentinel tier (the `agent_panel.rs` precedent).
fn spawn_ait_for_agent(cfg: &Path, data: &Path) -> E2e {
    let mut config = SpawnConfig::new(ait_bin!(), &[], SIZE);
    for (key, value) in [
        (
            OsString::from("XDG_CONFIG_HOME"),
            cfg.as_os_str().to_os_string(),
        ),
        (
            OsString::from("XDG_DATA_HOME"),
            data.as_os_str().to_os_string(),
        ),
        (OsString::from("SHELL"), OsString::from("/bin/sh")),
    ] {
        config.env.push((key, value));
    }
    E2e::spawn(config).expect("spawn the ait binary")
}

/// `Alt+A` opens the split focused — the `agent_panel.rs` recipe.
fn open_panel(app: &mut E2e) {
    app.press("Alt+A");
    app.wait_for_screen("the open, focused agent panel", |app| {
        app.screen_text().contains('│') && app.screen_text().contains(" agent")
    });
}

/// Type `task` on the panel's input line and submit it.
fn submit_task(app: &mut E2e, task: &str) {
    app.type_text(task);
    app.wait_for_screen("the task on the input line", |app| {
        app.screen_text().contains(task)
    });
    app.press("Enter");
}

/// One `chat.completion.chunk` `data:` frame carrying a content delta
/// then the `[DONE]` sentinel — a plain end turn.
fn chat_text_turn(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}}}}]}}\n\ndata: [DONE]\n\n",
        json_string(text)
    )
}

/// A `chat.completion.chunk` turn calling `name` once with `args` —
/// the `agent_panel.rs` wire shape: `id`/`name` on the announce
/// fragment, the arguments string fragment after, `finish_reason:
/// "tool_calls"` closing.
fn tool_turn(call_id: &str, name: &str, args: &str) -> String {
    let mut body = format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":{},\"type\":\"function\",\"function\":{{\"name\":{},\"arguments\":\"\"}}}}],\"finish_reason\":null}}}}]}}\n\n",
        json_string(call_id),
        json_string(name),
    );
    body.push_str(&format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"arguments\":{}}}}}]}}}}]}}\n\n",
        json_string(args)
    ));
    body.push_str(
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    );
    body.push_str("data: [DONE]\n\n");
    body
}

/// The count of `done` status lines on the panel half — how a second
/// finished task is told from the first.
fn done_lines(app: &E2e) -> usize {
    app.screen_text()
        .lines()
        .filter(|row| row.split('│').nth(1).is_some_and(|h| h.trim() == "done"))
        .count()
}

/// Spec R08-T3 / us-4 + us-28: a scripted task leaves one
/// `conversations` row titled from the first task and an ordered
/// `transcript_items` trail covering every emitted event — the task,
/// the gated `run_command` call and its full result, the ask card
/// and its answer, the coalesced reply and the terminal status — and
/// a second task in the same tab appends to the SAME row.
#[test]
fn agent_tasks_leave_a_transcript_and_the_second_appends() {
    if !Path::new("/bin/sh").exists() {
        return;
    }
    let fixture = Fixture::serve(vec![
        sse(&tool_turn(
            "call_1",
            "run_command",
            r#"{"command":"echo T3-MARK"}"#,
        )),
        sse(&tool_turn(
            "call_2",
            "ask_user_question",
            r#"{"question":"Ship it?","choices":["yes","no"]}"#,
        )),
        sse(&chat_text_turn("marker noted")),
        sse(&chat_text_turn("second task done")),
    ]);
    let cfg = tempfile::tempdir().expect("config fixture tempdir");
    let data = tempfile::tempdir().expect("data fixture tempdir");
    write_agent_settings(cfg.path(), &agent_settings_with_fixture(&fixture.base_url));

    let mut app = spawn_ait_for_agent(cfg.path(), data.path());
    app.wait_for_screen("the tab bar and status line", |app| {
        app.line(0).contains("1:sh") && app.line(STATUS_ROW).contains("· sh ·")
    });
    wait_fresh_prompt(&mut app, "the first prompt");
    let spawn_cwd = app.cwd().to_path_buf();

    // Task one: run_command gates on the approve card (`a` = allow
    // once), then the ask card's first choice answers on Enter.
    open_panel(&mut app);
    submit_task(&mut app, "run my marker command");
    app.wait_for_screen("the parked approve card", |app| {
        app.screen_text().contains("approve run_command")
    });
    app.type_text("a");
    app.wait_for_screen("the ask card", |app| app.screen_text().contains("Ship it?"));
    app.press("Enter"); // select "yes"
    app.wait_for_screen("task one's done status", |app| {
        done_lines(app) == 1 && panel_has_line(app, "marker noted")
    });

    // Task two in the same tab.
    submit_task(&mut app, "say done again");
    app.wait_for_screen("task two's done status", |app| done_lines(app) == 2);

    // Writes are synchronous per drain — durable the moment the
    // status painted, so quitting now loses nothing.
    app.press("Alt+Q");
    let status = app.wait_for_exit();
    assert!(status.success(), "expected a clean exit, got {status:?}");

    let store = open_store(data.path());
    let conversations = store.list_conversations().expect("list conversations");
    assert_eq!(
        conversations.len(),
        1,
        "one row — task two appended, never created a second"
    );
    let conversation = &conversations[0];
    assert_eq!(
        conversation.title, "run my marker command",
        "titled from the first task"
    );
    assert_eq!(conversation.cwd, spawn_cwd.to_string_lossy());
    assert_eq!(conversation.status, "open");

    let rows = store
        .load_transcript(conversation.id)
        .expect("load the transcript");
    assert_eq!(
        rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
        (0..rows.len() as i64).collect::<Vec<_>>(),
        "seqs contiguous in append order: {rows:?}"
    );
    let items: Vec<TranscriptItem> = rows
        .iter()
        .map(|row| {
            assert_eq!(
                row.kind,
                serde_json::from_str::<serde_json::Value>(&row.payload_json)
                    .expect("payload is JSON")["kind"]
                    .as_str()
                    .expect("kind tag"),
                "the kind column duplicates the payload's tag"
            );
            serde_json::from_str(&row.payload_json).expect("payload deserializes")
        })
        .collect();
    let kinds: Vec<&str> = items.iter().map(TranscriptItem::kind).collect();
    assert_eq!(
        kinds,
        [
            "task",
            "tool_call",
            "tool_result",
            "ask",
            "answer",
            "assistant_text",
            "status",
            "task",
            "assistant_text",
            "status",
        ],
        "every event mirrored in order: {items:?}"
    );
    assert_eq!(
        items[0],
        TranscriptItem::Task {
            text: "run my marker command".into()
        }
    );
    // The call row keeps the raw argument text; the result row keeps
    // the FULL text the model read — not the card's ≤120-char preview.
    assert!(
        matches!(&items[1], TranscriptItem::ToolCall { id, name, args, .. }
            if id == "call_1" && name == "run_command"
                && args == r#"{"command":"echo T3-MARK"}"#),
        "{:?}",
        items[1]
    );
    assert!(
        matches!(&items[2], TranscriptItem::ToolResult { id, text, status }
            if id == "call_1" && *status == ait_agent::ToolStatus::Done
                && text.contains("T3-MARK") && text.contains("exit code: 0")),
        "the full result text lands verbatim: {:?}",
        items[2]
    );
    assert_eq!(
        items[3],
        TranscriptItem::Ask {
            question: "Ship it?".into(),
            choices: vec!["yes".into(), "no".into()],
        }
    );
    assert_eq!(
        items[4],
        TranscriptItem::Answer { text: "yes".into() },
        "the answer recorded at send time, full text"
    );
    assert_eq!(
        items[5],
        TranscriptItem::AssistantText {
            text: "marker noted".into()
        },
        "the turn's deltas coalesced into one item"
    );
    assert_eq!(
        items[7],
        TranscriptItem::Task {
            text: "say done again".into()
        },
        "the second task appended to the same row"
    );
}

// ─── R08-T5: update_plan — pinned block + plans row ──────────────

/// Spec R08-T5 / us-13–17: two scripted `update_plan` calls in one
/// task render the pinned plan block under the panel title — `✓`
/// done, `▸` in progress, `○` pending — with NO approval prompt (the
/// task completes with no verdict keypress; the card IS the effect),
/// an ordinary tool card per call in the scrollback, and the second
/// call replacing the block's contents. The `plans` row keeps only
/// the latest snapshot while `plan` transcript items keep the audit
/// trail.
#[test]
fn update_plan_pins_the_block_and_upserts_the_latest_snapshot() {
    if !Path::new("/bin/sh").exists() {
        return;
    }
    let fixture = Fixture::serve(vec![
        sse(&tool_turn(
            "call_1",
            "update_plan",
            r#"{"todos":[
                {"content":"reproduce it","status":"done"},
                {"content":"fix it","status":"in_progress"},
                {"content":"verify it","status":"pending"}
            ]}"#,
        )),
        sse(&tool_turn(
            "call_2",
            "update_plan",
            r#"{"todos":[
                {"content":"reproduce it","status":"done"},
                {"content":"fix it","status":"done"}
            ]}"#,
        )),
        sse(&chat_text_turn("planned")),
    ]);
    let cfg = tempfile::tempdir().expect("config fixture tempdir");
    let data = tempfile::tempdir().expect("data fixture tempdir");
    write_agent_settings(cfg.path(), &agent_settings_with_fixture(&fixture.base_url));

    let mut app = spawn_ait_for_agent(cfg.path(), data.path());
    app.wait_for_screen("the tab bar and status line", |app| {
        app.line(0).contains("1:sh") && app.line(STATUS_ROW).contains("· sh ·")
    });
    wait_fresh_prompt(&mut app, "the first prompt");

    open_panel(&mut app);
    submit_task(&mut app, "plan the fix");
    // The task finishing without a single keypress IS the
    // gate-exemption proof — a parked approve card would wait
    // forever — and no approve prompt ever painted.
    app.wait_for_screen("the second snapshot pinned", |app| {
        done_lines(app) == 1
            && panel_has_line(app, "plan")
            && panel_has_line(app, "✓ reproduce it")
            && panel_has_line(app, "✓ fix it")
            && !app.screen_text().contains("approve update_plan")
    });
    // The block holds the LATEST list — the first call's
    // "verify it" todo is gone — while both calls' cards audit in
    // the scrollback (us-15).
    assert!(
        !panel_has_line(&app, "verify it"),
        "the replaced list's todo left the block:\n{}",
        app.screen_text()
    );
    assert!(
        panel_has_line(&app, "✓ update_plan 3 todos")
            && panel_has_line(&app, "✓ update_plan 2 todos"),
        "each call's ordinary card landed in scrollback:\n{}",
        app.screen_text()
    );

    app.press("Alt+Q");
    let status = app.wait_for_exit();
    assert!(status.success(), "expected a clean exit, got {status:?}");

    let store = open_store(data.path());
    let id = store.list_conversations().expect("list conversations")[0].id;
    // The `plans` row holds ONLY the latest snapshot.
    let payload = store
        .load_plan(id)
        .expect("load the plan")
        .expect("a plans row");
    let plan: TranscriptItem = serde_json::from_str(&payload).expect("payload deserializes");
    assert_eq!(
        plan,
        TranscriptItem::Plan {
            todos: vec![
                ait_agent::PlanTodo {
                    content: "reproduce it".into(),
                    status: ait_agent::PlanTodoStatus::Done,
                },
                ait_agent::PlanTodo {
                    content: "fix it".into(),
                    status: ait_agent::PlanTodoStatus::Done,
                },
            ]
        },
        "the row is the second call's snapshot, verbatim"
    );
    // …while the transcript kept both snapshots plus the call/result
    // pairs — the audit trail (us-15) mirrored like every event.
    let kinds: Vec<String> = store
        .load_transcript(id)
        .expect("load the transcript")
        .iter()
        .map(|row| row.kind.clone())
        .collect();
    assert_eq!(
        kinds,
        [
            "task",
            "tool_call",
            "plan",
            "tool_result",
            "tool_call",
            "plan",
            "tool_result",
            "assistant_text",
            "status",
        ],
        "plan items mirror in event order: {kinds:?}"
    );
}

// ─── R08-T6: Alt+R picker — list, resume, delete ───────────────────

/// Spec R08-T6 + ticket: scripted `update_plan` task → `Alt+Q` →
/// relaunch on the same data dir → `Alt+R` lists the conversation
/// newest-first (title, age, item count) → Enter rebuilds the panel
/// — task/text/status entries plus the pinned plan — and the next
/// task's captured request carries the replayed transcript →
/// `Ctrl+D` twice deletes the row for good.
#[test]
fn alt_r_lists_resumes_across_restart_and_ctrl_d_deletes() {
    if !Path::new("/bin/sh").exists() {
        return;
    }
    let fixture = Fixture::serve(vec![
        // Task one: the plan call, then the closing turn — two
        // requests. The resumed task takes the third.
        sse(&tool_turn(
            "call_1",
            "update_plan",
            r#"{"todos":[
                {"content":"reproduce it","status":"done"},
                {"content":"fix it","status":"in_progress"}
            ]}"#,
        )),
        sse(&chat_text_turn("planned")),
        sse(&chat_text_turn("continued")),
    ]);
    let cfg = tempfile::tempdir().expect("config fixture tempdir");
    let data = tempfile::tempdir().expect("data fixture tempdir");
    write_agent_settings(cfg.path(), &agent_settings_with_fixture(&fixture.base_url));

    let mut app = spawn_ait_for_agent(cfg.path(), data.path());
    app.wait_for_screen("the tab bar and status line", |app| {
        app.line(0).contains("1:sh") && app.line(STATUS_ROW).contains("· sh ·")
    });
    wait_fresh_prompt(&mut app, "the first prompt");
    open_panel(&mut app);
    submit_task(&mut app, "plan the fix");
    app.wait_for_screen("task one's done + pinned plan", |app| {
        done_lines(app) == 1 && panel_has_line(app, "▸ fix it")
    });
    app.press("Alt+Q");
    assert!(app.wait_for_exit().success(), "the first run exits cleanly");

    // Relaunch on the SAME data dir — the conversation outlived the
    // app (us-7).
    let mut app = spawn_ait_for_agent(cfg.path(), data.path());
    app.wait_for_screen("the relaunched chrome", |app| {
        app.line(0).contains("1:sh") && app.line(STATUS_ROW).contains("· sh ·")
    });
    wait_fresh_prompt(&mut app, "the relaunched prompt");

    // `Alt+R` lists it newest-first — title, age bucket, item count.
    app.press("Alt+R");
    app.wait_for_screen("the picker row", |app| {
        app.screen_text().contains("plan the fix")
            && app.screen_text().contains("items")
            && app.screen_text().contains("enter: resume")
    });

    // Enter resumes: the panel opens with the rebuilt scrollback —
    // the task entry, the reply, the `done` status — and the pinned
    // plan block restored from the `plans` row.
    app.press("Enter");
    app.wait_for_screen("the resumed panel", |app| {
        panel_has_line(app, "› plan the fix")
            && panel_has_line(app, "planned")
            && panel_has_line(app, "done")
            && panel_has_line(app, "✓ reproduce it")
            && panel_has_line(app, "▸ fix it")
    });

    // The next task continues the SAME conversation — its request
    // replays the stored transcript (the third captured request is
    // the resumed task's first).
    submit_task(&mut app, "continue the work");
    app.wait_for_screen("the continued task's done", |app| done_lines(app) == 2);
    fixture.captured();
    fixture.captured();
    let third = fixture.captured();
    assert!(
        third.body.contains("plan the fix"),
        "the prior transcript replayed into the request: {}",
        third.body
    );
    assert!(
        third.body.contains("reproduce it"),
        "the persisted plan rode the replay: {}",
        third.body
    );

    // `Ctrl+D` once arms — nothing deletes — a second confirms and
    // the row (items + plan, cascaded) is gone for good.
    app.press("Alt+R");
    app.wait_for_screen("the picker again", |app| {
        app.screen_text().contains("plan the fix")
    });
    app.press("Ctrl+D");
    app.wait_for_screen("the armed confirm", |app| {
        app.screen_text().contains("ctrl+d again deletes")
    });
    let store = open_store(data.path());
    assert_eq!(
        store.list_conversations().unwrap().len(),
        1,
        "the first Ctrl+D only armed"
    );
    drop(store);
    app.press("Ctrl+D");
    app.wait_for_screen("the picker emptied and closed", |app| {
        !app.screen_text().contains("items")
    });
    // A reopened picker finds nothing left to list.
    app.press("Alt+R");
    app.wait_for_screen("the empty-store toast", |app| {
        app.line(STATUS_ROW).contains("no stored conversations")
    });
    app.press("Alt+Q");
    assert!(app.wait_for_exit().success());

    let store = open_store(data.path());
    assert!(
        store.list_conversations().unwrap().is_empty(),
        "the row deleted for good"
    );
}

// ─── R08-T8: summarization entry + killed-mid-call resume ─────────

/// Whether any panel-half row carries `needle` — for text whose
/// painted row clips inside the split (a tool card's `detail`
/// outruns the panel width).
fn panel_contains(app: &E2e, needle: &str) -> bool {
    app.screen_text()
        .lines()
        .any(|row| row.split('│').nth(1).is_some_and(|h| h.contains(needle)))
}

/// Spec R08 us-18/19/22 + ticket: a `context_window_limit`-crossing
/// estimate fires one summarize call mid-run — the panel shows the
/// "conversation summarized" entry (never silent), the next model
/// request carries the marker's text where the covered items were,
/// and the transcript persists the `summary` item.
#[test]
fn a_crossing_estimate_shows_and_persists_the_summary_marker() {
    if !Path::new("/bin/sh").exists() {
        return;
    }
    // Request 1 = the scripted tool turn; request 2 = the summarize
    // call itself; request 3 = the compacted turn that ends the task.
    let fixture = Fixture::serve(vec![
        sse(&tool_turn(
            "call_1",
            "read_files",
            r#"{"paths":["note.txt"]}"#,
        )),
        sse(&chat_text_turn("SUMMARY-MARKER the oldest half")),
        sse(&chat_text_turn("final answer")),
    ]);
    let cfg = tempfile::tempdir().expect("config fixture tempdir");
    let data = tempfile::tempdir().expect("data fixture tempdir");
    // A 200-token cap → the 0.8 threshold trips once the ~780-char
    // read lands; `read_files` ungated keeps the run keypress-free.
    write_agent_settings(
        cfg.path(),
        &agent_settings_with_profile(
            &fixture.base_url,
            "context_window_limit = 200\nread_files = \"always_allow\"",
        ),
    );

    let mut app = spawn_ait_for_agent(cfg.path(), data.path());
    app.wait_for_screen("the tab bar and status line", |app| {
        app.line(0).contains("1:sh") && app.line(STATUS_ROW).contains("· sh ·")
    });
    wait_fresh_prompt(&mut app, "the first prompt");
    fs::write(app.cwd().join("note.txt"), "NOTE-CONTENT ".repeat(60)).expect("seed the big file");

    open_panel(&mut app);
    submit_task(&mut app, "read the note");
    app.wait_for_screen("the summarized entry and the done status", |app| {
        done_lines(app) == 1
            && panel_has_line(app, "conversation summarized")
            && panel_has_line(app, "final answer")
    });

    // The summarize call is its own request — the rendered span,
    // no tools — and the turn after it runs on the rewritten
    // context: marker text in, the covered task item out, the
    // newest tail (call + result) verbatim.
    let first = fixture.captured();
    assert!(first.body.contains("read the note"), "{}", first.body);
    let summarize_call = fixture.captured();
    assert!(
        summarize_call.body.contains("<transcript>"),
        "{}",
        summarize_call.body
    );
    let compacted = fixture.captured();
    assert!(
        compacted.body.contains("SUMMARY-MARKER"),
        "{}",
        compacted.body
    );
    assert!(
        !compacted.body.contains("read the note"),
        "the covered task item left the context: {}",
        compacted.body
    );
    assert!(
        compacted.body.contains("NOTE-CONTENT"),
        "the kept tail stayed verbatim: {}",
        compacted.body
    );

    app.press("Alt+Q");
    let status = app.wait_for_exit();
    assert!(status.success(), "expected a clean exit, got {status:?}");

    // The marker persisted as a `summary` transcript item between
    // the result it compacted and the reply that followed.
    let store = open_store(data.path());
    let id = store.list_conversations().expect("list conversations")[0].id;
    let rows = store.load_transcript(id).expect("load the transcript");
    let kinds: Vec<&str> = rows.iter().map(|row| row.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "task",
            "tool_call",
            "tool_result",
            "summary",
            "assistant_text",
            "status",
        ],
        "the summary item mirrored in event order: {kinds:?}"
    );
    let summary: TranscriptItem =
        serde_json::from_str(&rows[3].payload_json).expect("payload deserializes");
    assert!(
        matches!(&summary, TranscriptItem::Summary { text, covers_seq }
            if text.contains("SUMMARY-MARKER") && *covers_seq == 1),
        "the marker covers the oldest item: {summary:?}"
    );
}

/// Spec R08 us-9 + ticket: a task whose approve card was parked when
/// `ait` exited leaves a `tool_call` with no `tool_result` — resume
/// paints the card closed `interrupted`, and the next task's request
/// carries the synthetic result so no provider sees a dangling call.
/// The stored row itself is never rewritten: the synthetic result
/// exists only in the replayed context.
#[test]
fn a_conversation_killed_mid_call_resumes_with_the_interrupted_result() {
    if !Path::new("/bin/sh").exists() {
        return;
    }
    // Run 1 takes the tool turn and parks on the approve card;
    // run 2's resumed task takes the second script.
    let fixture = Fixture::serve(vec![
        sse(&tool_turn(
            "call_dying",
            "run_command",
            r#"{"command":"echo DYING"}"#,
        )),
        sse(&chat_text_turn("resumed answer")),
    ]);
    let cfg = tempfile::tempdir().expect("config fixture tempdir");
    let data = tempfile::tempdir().expect("data fixture tempdir");
    write_agent_settings(cfg.path(), &agent_settings_with_fixture(&fixture.base_url));

    let mut app = spawn_ait_for_agent(cfg.path(), data.path());
    app.wait_for_screen("the tab bar and status line", |app| {
        app.line(0).contains("1:sh") && app.line(STATUS_ROW).contains("· sh ·")
    });
    wait_fresh_prompt(&mut app, "the first prompt");
    open_panel(&mut app);
    submit_task(&mut app, "run the dying command");
    app.wait_for_screen("the parked approve card", |app| {
        app.screen_text().contains("approve run_command")
    });
    // The card is still parked — the run dies mid-call.
    app.press("Alt+Q");
    let status = app.wait_for_exit();
    assert!(status.success(), "expected a clean exit, got {status:?}");

    // Relaunch on the same data dir and resume the dead conversation.
    let mut app = spawn_ait_for_agent(cfg.path(), data.path());
    app.wait_for_screen("the relaunched chrome", |app| {
        app.line(0).contains("1:sh") && app.line(STATUS_ROW).contains("· sh ·")
    });
    wait_fresh_prompt(&mut app, "the relaunched prompt");
    app.press("Alt+R");
    app.wait_for_screen("the picker row", |app| {
        app.screen_text().contains("run the dying command")
    });
    app.press("Enter");
    // The rebuilt card closed `interrupted` — never an eternal `…`
    // spinner, never a still-awaiting approve card.
    app.wait_for_screen("the interrupted card", |app| {
        panel_contains(app, "run_command")
            && panel_contains(app, "interrupted")
            && !panel_contains(app, "approve run_command")
    });

    // The next task replays the stored transcript: the unanswered
    // call arrives paired with the synthetic interrupted result.
    submit_task(&mut app, "continue anyway");
    app.wait_for_screen("the continued task's done", |app| {
        done_lines(app) == 1 && panel_has_line(app, "resumed answer")
    });
    fixture.captured(); // run 1's tool turn
    let continued = fixture.captured();
    assert!(
        continued
            .body
            .contains("interrupted — the run ended before this call produced a result"),
        "the synthetic result rode the replay: {}",
        continued.body
    );
    assert!(
        continued.body.contains("call_dying"),
        "the result pairs with the dead call's id: {}",
        continued.body
    );

    app.press("Alt+Q");
    let status = app.wait_for_exit();
    assert!(status.success(), "expected a clean exit, got {status:?}");

    // Resume never rewrote the stored row (us-12): the dead call is
    // still unanswered on disk — the interrupted result exists only
    // in the rebuilt model context.
    let store = open_store(data.path());
    let id = store.list_conversations().expect("list conversations")[0].id;
    let rows = store.load_transcript(id).expect("load the transcript");
    let kinds: Vec<&str> = rows.iter().map(|row| row.kind.as_str()).collect();
    assert_eq!(
        kinds,
        ["task", "tool_call", "task", "assistant_text", "status"],
        "the unanswered call stays unanswered in the record: {kinds:?}"
    );
}
