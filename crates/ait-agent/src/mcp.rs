//! The MCP client pool (Spec R10, REQUIREMENTS.md §5.10.1): one
//! [`McpPool`] per `AgentSession`, owned by the agent worker on its
//! existing current-thread tokio runtime — rmcp's stdio clients are
//! `Send` futures driven by the same `block_on`s that run the model
//! loop, so MCP never brings a second runtime into ait.
//!
//! Every task [`McpPool::reconcile`]s the merged `mcp.json` contract
//! T2's [`ait_config::load_mcp`] produces against the live pool: a
//! server whose declaration is unchanged keeps its connection, a
//! dropped or edited declaration shuts down, and a server that
//! failed to initialize — or died between tasks — retries, since the
//! next task's reconcile is the round's one recovery point (no
//! auto-restart). **User-tier** servers connect eagerly at task
//! start — spawn, the `initialize` handshake and the
//! tools/resources enumeration all inside [`CONNECT_TIMEOUT`] — and
//! any initialization failure marks the server unavailable in the
//! [`McpCatalog`] rather than sinking the task. **Project-tier**
//! entries are listed in the catalog but never spawned at reconcile
//! — a cloned repo cannot run code on the user's machine silently:
//! the pool connects them lazily inside the gated call itself
//! (Spec R10 us-8), so only an approved `call_mcp_tool`/
//! `read_mcp_resource` ever spawns the process.
//!
//! The two MCP tools — `call_mcp_tool` and `read_mcp_resource` —
//! dispatch through [`McpPool::execute`] after the permission gate,
//! running locally even on a remote (ssh) task: MCP is not shell
//! execution, so the §5.9 executor tiers never see it. `execute` is
//! reached only once the gate allowed — `mcp_allowlist`/
//! `always_allow` skip the ask but take the same lazy path — so a
//! denied call never spawns anything. Every failure mode — spawn,
//! initialize, call, timeout, crash — is a [`ToolOutput`] error:
//! model input, never a task wedge.
//!
//! All rmcp values are created and dropped inside `block_on`:
//! `TokioChildProcess`'s `Drop` kills the child through
//! `tokio::spawn`, which panics outside a runtime context — the
//! worker drains the pool inside `block_on` before its thread exits
//! for the same reason.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use ait_config::{load_mcp, McpServerEntry, McpServerOrigin};
use rmcp::model::{CallToolRequestParams, CallToolResult, ReadResourceRequestParams};
use rmcp::service::{RoleClient, RunningService, ServiceError};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use tokio::time::timeout;

use crate::files::ToolOutput;
use crate::tool_call::ToolCall;
use crate::tools::{CALL_MCP_TOOL, READ_MCP_RESOURCE};

/// One server's eager-initialization budget (Spec R10: "connect +
/// tools/resources enumeration under a connect timeout") — spawn,
/// the `initialize` handshake and both enumerations share it; a
/// server that cannot finish inside it is marked unavailable rather
/// than stalling task start.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// One `call_mcp_tool`/`read_mcp_resource` request's budget — a hung
/// server surfaces as a tool error the model reads (Spec R10 us-18),
/// never a wedged task. rmcp's convenience calls carry no timeout of
/// their own; dropping the in-flight future sends the server a
/// `notifications/cancelled`.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// The session's pool of MCP connections, keyed by `mcpServers`
/// name. Created empty in the worker beside the [`AgentPty`] — the
/// same per-session ownership precedent — and reconciled per task.
///
/// [`AgentPty`]: crate::pty::AgentPty
pub(crate) struct McpPool {
    /// Live slots by server name — `BTreeMap` so the catalog (and
    /// the system prompt built from it) is deterministic.
    servers: BTreeMap<String, ServerSlot>,
    /// The last `mcp.json` load failure — carried into the catalog so
    /// a malformed file surfaces in the prompt rather than silently
    /// behaving as "no servers" (Spec R10 us-4's visibility rule).
    /// The pool keeps its last-good connections on an error
    /// (ADR-0006); the user-side toast for the same `ConfigError`
    /// fires at the frontend's submit seam.
    load_error: Option<String>,
}

/// One pooled server: the merged declaration last reconciled plus
/// what the pool knows about the live connection.
struct ServerSlot {
    /// The effective `mcpServers.<name>` entry — compared per task;
    /// a changed declaration respawns the server wholesale.
    entry: McpServerEntry,
    /// The connection's state.
    state: ServerState,
}

/// Where one pooled server stands.
enum ServerState {
    /// Declared but not connected — what reconcile leaves every
    /// project-tier entry as; the gated call's lazy connect is the
    /// only way out (Spec R10 us-8).
    Declared,
    /// Connected and enumerated — `tools`/`resources` hold the names
    /// the catalog renders.
    Connected {
        /// The rmcp client — `Deref`s to the `Peer<RoleClient>` the
        /// request methods live on.
        service: RunningService<RoleClient, ()>,
        /// Enumerated tool names from `tools/list`.
        tools: Vec<String>,
        /// Enumerated resource URIs from `resources/list`.
        resources: Vec<String>,
    },
    /// Initialization or a later call failed — the reason the
    /// catalog renders.
    Unavailable(String),
}

impl McpPool {
    /// An empty pool — the worker builds it once; `reconcile` grows
    /// it from the merged `mcp.json`.
    pub(crate) fn new() -> Self {
        Self {
            servers: BTreeMap::new(),
            load_error: None,
        }
    }

    /// The per-task reconcile (Spec R10 us-6/15): re-read the merged
    /// `mcp.json` for `cwd` — the user layer plus the nearest
    /// `.ait/mcp.json` — and bring the pool in line with it.
    /// Connections to unchanged user-tier servers persist; dropped
    /// or changed declarations shut down; a server that is
    /// unavailable or dead retries (the recovery point). Project-tier
    /// entries join the catalog as [`ServerState::Declared`] and are
    /// never spawned here — their only connect is the gated call's
    /// lazy path, and a project server that already connected keeps
    /// its slot like a user-tier one.
    ///
    /// A malformed or unreadable `mcp.json` keeps the last working
    /// pool (ADR-0006's keep-last-good — a bad save must not drop
    /// healthy connections mid-session, Spec R10 us-4's "the
    /// terminal keeps working"): the [`McpCatalog`]'s `load_error`
    /// carries the `ConfigError` text into the prompt, and the
    /// frontend toasts the same error at its submit seam. The task
    /// continues either way.
    pub(crate) async fn reconcile(&mut self, cwd: &Path) -> McpCatalog {
        let merged = match load_mcp(cwd) {
            Ok(merged) => merged,
            Err(error) => {
                self.load_error = Some(error.message().to_string());
                return self.catalog();
            }
        };
        self.load_error = None;
        // The config is the source of truth: a name it no longer
        // declares — or declares differently — loses its slot; the
        // fresh declaration reconnects below.
        let stale: Vec<String> = self
            .servers
            .iter()
            .filter(|(name, slot)| merged.servers.get(*name) != Some(&slot.entry))
            .map(|(name, _)| name.clone())
            .collect();
        for name in stale {
            if let Some(mut slot) = self.servers.remove(&name) {
                slot.shutdown().await;
            }
        }
        for (name, entry) in merged.servers {
            match entry.origin {
                // Spec R10 us-8: project-tier entries are listed
                // but never spawned at task start — the gated
                // call's lazy connect is their only spawn path. An
                // already-connected project server keeps its slot
                // like a user-tier one.
                McpServerOrigin::Project => {
                    self.servers.entry(name).or_insert(ServerSlot {
                        entry,
                        state: ServerState::Declared,
                    });
                }
                McpServerOrigin::User => {
                    let slot = self.servers.entry(name).or_insert(ServerSlot {
                        entry,
                        state: ServerState::Declared,
                    });
                    // A live connection persists; anything else — a
                    // fresh `Declared` slot, an `Unavailable` retry,
                    // a server that died since the last task —
                    // connects now. The failed and dead cases ARE
                    // the reconcile-recovery point.
                    let alive = match &slot.state {
                        ServerState::Connected { service, .. } => !service.is_transport_closed(),
                        _ => false,
                    };
                    if !alive {
                        slot.connect().await;
                    }
                }
            }
        }
        self.catalog()
    }

    /// The prompt-facing snapshot of the pool — rebuilt after every
    /// reconcile; call sites hold it by value.
    pub(crate) fn catalog(&self) -> McpCatalog {
        McpCatalog {
            entries: self
                .servers
                .iter()
                .map(|(name, slot)| McpCatalogEntry {
                    name: name.clone(),
                    origin: slot.entry.origin,
                    availability: match &slot.state {
                        ServerState::Declared => McpAvailability::Declared,
                        ServerState::Connected {
                            tools, resources, ..
                        } => McpAvailability::Available {
                            tools: tools.clone(),
                            resources: resources.clone(),
                        },
                        ServerState::Unavailable(reason) => {
                            McpAvailability::Unavailable(reason.clone())
                        }
                    },
                })
                .collect(),
            load_error: self.load_error.clone(),
        }
    }

    /// Which config layer declared `server` — the approve card's
    /// layer annotation (Spec R10 us-9): a `project .ait/` server
    /// asks the user to run code the repository shipped.
    pub(crate) fn origin_of(&self, server: &str) -> Option<McpServerOrigin> {
        self.servers.get(server).map(|slot| slot.entry.origin)
    }

    /// Shut every connection down. MUST be awaited inside a runtime
    /// context — rmcp's child cleanup spawns a task — which is why
    /// the worker drains inside its final `block_on` rather than
    /// letting the pool `Drop` bare.
    pub(crate) async fn drain(&mut self) {
        for (_, mut slot) in std::mem::take(&mut self.servers) {
            slot.shutdown().await;
        }
    }

    /// The gated dispatch for both MCP tools (Spec R10): parse the
    /// call's arguments, resolve the named server, lazy-connect it
    /// when it is still only [`ServerState::Declared`] (project-tier
    /// — this is the spawn a cloned repo can never trigger without
    /// an allow), then run the request under [`CALL_TIMEOUT`].
    /// Reached only after the gate allowed — every failure is a
    /// [`ToolOutput`] error, model input, never a task wedge.
    pub(crate) async fn execute(&mut self, call: &ToolCall) -> ToolOutput {
        let args: serde_json::Value = match serde_json::from_str(&call.arguments) {
            Ok(args) => args,
            Err(error) => {
                return ToolOutput::error(format!(
                    "{}: arguments are not valid JSON: {error}",
                    call.name
                ))
            }
        };
        match call.name.as_str() {
            CALL_MCP_TOOL => self.call_tool(&args).await,
            READ_MCP_RESOURCE => self.read_resource(&args).await,
            other => ToolOutput::error(format!("unknown tool {other:?}")),
        }
    }

    /// `call_mcp_tool(server, tool, arguments)`: the tool's content
    /// blocks come back as the result text — `is_error` results are
    /// tool errors, so a server-side failure reads as one to the
    /// model. A dead transport demotes the server to unavailable for
    /// the rest of the task (the next reconcile retries).
    async fn call_tool(&mut self, args: &serde_json::Value) -> ToolOutput {
        let Some(server) = args.get("server").and_then(|v| v.as_str()) else {
            return ToolOutput::error(String::from("call_mcp_tool: `server` must be a string"));
        };
        let Some(tool) = args.get("tool").and_then(|v| v.as_str()) else {
            return ToolOutput::error(String::from("call_mcp_tool: `tool` must be a string"));
        };
        let Some(slot) = self.servers.get_mut(server) else {
            return ToolOutput::error(format!(
                "call_mcp_tool: no MCP server named {server:?} is configured"
            ));
        };
        slot.connect_if_declared().await;
        let Some(service) = slot.live_service() else {
            return ToolOutput::error(slot_error(CALL_MCP_TOOL, server, &slot.state));
        };
        let mut params = CallToolRequestParams::new(tool.to_string());
        if let Some(arguments) = args.get("arguments").and_then(|v| v.as_object()) {
            params = params.with_arguments(arguments.clone());
        }
        match timeout(CALL_TIMEOUT, service.call_tool(params)).await {
            Err(_) => ToolOutput::error(format!(
                "call_mcp_tool: {server}:{tool} timed out after {}s",
                CALL_TIMEOUT.as_secs()
            )),
            Ok(Err(error)) => {
                if matches!(error, ServiceError::TransportClosed) {
                    slot.state =
                        ServerState::Unavailable(format!("the connection failed: {error}"));
                }
                ToolOutput::error(format!("call_mcp_tool: {server}:{tool} failed: {error}"))
            }
            Ok(Ok(result)) => render_tool_result(result),
        }
    }

    /// `read_mcp_resource(server, uri)`: text contents join as the
    /// result; a blob reports itself instead of dumping base64 into
    /// the conversation.
    async fn read_resource(&mut self, args: &serde_json::Value) -> ToolOutput {
        let Some(server) = args.get("server").and_then(|v| v.as_str()) else {
            return ToolOutput::error(String::from("read_mcp_resource: `server` must be a string"));
        };
        let Some(uri) = args.get("uri").and_then(|v| v.as_str()) else {
            return ToolOutput::error(String::from("read_mcp_resource: `uri` must be a string"));
        };
        let Some(slot) = self.servers.get_mut(server) else {
            return ToolOutput::error(format!(
                "read_mcp_resource: no MCP server named {server:?} is configured"
            ));
        };
        slot.connect_if_declared().await;
        let Some(service) = slot.live_service() else {
            return ToolOutput::error(slot_error(READ_MCP_RESOURCE, server, &slot.state));
        };
        match timeout(
            CALL_TIMEOUT,
            service.read_resource(ReadResourceRequestParams::new(uri)),
        )
        .await
        {
            Err(_) => ToolOutput::error(format!(
                "read_mcp_resource: {server} {uri} timed out after {}s",
                CALL_TIMEOUT.as_secs()
            )),
            Ok(Err(error)) => {
                if matches!(error, ServiceError::TransportClosed) {
                    slot.state =
                        ServerState::Unavailable(format!("the connection failed: {error}"));
                }
                ToolOutput::error(format!("read_mcp_resource: {server} {uri} failed: {error}"))
            }
            Ok(Ok(result)) => {
                let mut parts = Vec::new();
                for content in &result.contents {
                    match content {
                        rmcp::model::ResourceContents::TextResourceContents { text, .. } => {
                            parts.push(text.clone())
                        }
                        rmcp::model::ResourceContents::BlobResourceContents {
                            uri, blob, ..
                        } => parts.push(format!(
                            "[binary resource {uri} — {} base64 characters, contents omitted]",
                            blob.len()
                        )),
                        _ => parts.push(String::from("[unsupported resource content]")),
                    }
                }
                if parts.is_empty() {
                    ToolOutput::ok(String::from("the resource returned no contents"))
                } else {
                    ToolOutput::ok(parts.join("\n"))
                }
            }
        }
    }
}

impl ServerSlot {
    /// The slot's live connection, or `None` — the guard both MCP
    /// tools run after name resolution. A transport that closed
    /// since the last reconcile demotes the slot to
    /// [`ServerState::Unavailable`] on the spot.
    fn live_service(&mut self) -> Option<&RunningService<RoleClient, ()>> {
        if let ServerState::Connected { service, .. } = &self.state {
            if service.is_transport_closed() {
                self.state = ServerState::Unavailable(String::from("the server exited"));
            }
        }
        match &self.state {
            ServerState::Connected { service, .. } => Some(service),
            _ => None,
        }
    }

    /// §5.10.1's lazy consent connect (Spec R10 us-8): a
    /// [`ServerState::Declared`] slot — what reconcile leaves every
    /// project-tier entry as — spawns + initializes inside the
    /// gated call itself, under `connect`'s ordinary
    /// [`CONNECT_TIMEOUT`]. [`McpPool::execute`] is reached only
    /// after the permission gate allowed, so a denied call never
    /// spawns the process; `mcp_allowlist`/`always_allow` skip the
    /// ask but take this same lazy path on the first call.
    async fn connect_if_declared(&mut self) {
        if matches!(self.state, ServerState::Declared) {
            self.connect().await;
        }
    }

    /// One initialization attempt: spawn the declared command,
    /// run the `initialize` handshake (`serve` performs it), then
    /// enumerate tools and resources — all inside the caller's
    /// [`CONNECT_TIMEOUT`]. A prior connection shuts down first.
    /// Every failure leaves the slot [`ServerState::Unavailable`]
    /// with the reason.
    async fn connect(&mut self) {
        // Drop a stale or dead connection inside the runtime before
        // respawning — the child's kill-spawn needs the context.
        self.shutdown().await;
        let attempt = timeout(CONNECT_TIMEOUT, connect_and_enumerate(&self.entry)).await;
        self.state = match attempt {
            Err(_) => ServerState::Unavailable(format!(
                "initialization timed out after {}s",
                CONNECT_TIMEOUT.as_secs()
            )),
            Ok(Err(reason)) => ServerState::Unavailable(reason),
            Ok(Ok((service, tools, resources))) => ServerState::Connected {
                service,
                tools,
                resources,
            },
        };
    }

    /// Gracefully close a connected slot — `close` waits out rmcp's
    /// shutdown; the slot ends [`ServerState::Declared`] regardless
    /// (reconnect is `connect`'s job).
    async fn shutdown(&mut self) {
        if let ServerState::Connected { service, .. } = &mut self.state {
            let _ = service.close().await;
        }
        self.state = ServerState::Declared;
    }
}

/// The [`ToolOutput`] error text for a slot that could not serve a
/// request — reached only after `connect_if_declared` ran, so the
/// slot is always [`ServerState::Unavailable`] here: the recorded
/// reason (spawn, handshake, enumeration or timeout failure)
/// replays to the model.
fn slot_error(tool: &str, server: &str, state: &ServerState) -> String {
    match state {
        ServerState::Unavailable(reason) => {
            format!("{tool}: MCP server {server:?} is unavailable: {reason}")
        }
        ServerState::Declared => {
            unreachable!("the gated call lazy-connects a declared slot before dispatch")
        }
        ServerState::Connected { .. } => unreachable!("a live service never errors here"),
    }
}

/// Spawn + handshake + enumerate one declared server — the body of
/// [`ServerSlot::connect`], run under [`CONNECT_TIMEOUT`]. The
/// `env` additions pass through literally (Spec R10 us-19 — no
/// expansion); stderr is inherited so a server's own logging never
/// corrupts the NDJSON stdout channel.
async fn connect_and_enumerate(
    entry: &McpServerEntry,
) -> Result<(RunningService<RoleClient, ()>, Vec<String>, Vec<String>), String> {
    let command = tokio::process::Command::new(&entry.server.command).configure(|cmd| {
        cmd.args(&entry.server.args);
        for (key, value) in &entry.server.env {
            cmd.env(key, value);
        }
    });
    // `TokioChildProcess::new` spawns eagerly — a missing binary is
    // an `io::Error` here, before any handshake.
    let transport = TokioChildProcess::new(command)
        .map_err(|error| format!("could not spawn {:?}: {error}", entry.server.command))?;
    // `()` is rmcp's stock `ClientHandler`: `serve` runs the legacy
    // `initialize` → `notifications/initialized` handshake itself.
    let service = ().serve(transport).await.map_err(|error| format!("initialize failed: {error}"))?;
    // `RunningService` derefs to the peer; `list_all_*` walks every
    // page. An enumeration failure still counts as "failed to
    // initialize" — the catalog would otherwise list a server whose
    // surface is unknown.
    let tools = service
        .list_all_tools()
        .await
        .map_err(|error| format!("tools/list failed: {error}"))?
        .iter()
        .map(|tool| tool.name.to_string())
        .collect();
    let resources = service
        .list_all_resources()
        .await
        .map_err(|error| format!("resources/list failed: {error}"))?
        .iter()
        .map(|resource| resource.uri.clone())
        .collect();
    Ok((service, tools, resources))
}

/// A `tools/call` result → the result text the conversation carries:
/// text content blocks joined; `structured_content` fills in only
/// when no text arrived; `is_error` makes it a tool error so the
/// card paints `Failed` and the model reads the server's own
/// message.
fn render_tool_result(result: CallToolResult) -> ToolOutput {
    let text = result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n");
    let text = if text.is_empty() {
        match result.structured_content {
            Some(structured) => serde_json::to_string_pretty(&structured)
                .unwrap_or_else(|_| String::from("the tool returned unrenderable content")),
            None => String::from("the tool returned no content"),
        }
    } else {
        text
    };
    if result.is_error == Some(true) {
        ToolOutput::error(text)
    } else {
        ToolOutput::ok(text)
    }
}

/// The prompt-facing snapshot [`McpPool::reconcile`] returns — the
/// system prompt's "MCP servers" section (Spec R10 us-6/7) and the
/// shape later tickets consume for lazy connects and watching.
pub(crate) struct McpCatalog {
    /// One entry per declared server, sorted by name.
    pub(crate) entries: Vec<McpCatalogEntry>,
    /// `mcp.json` failed to load this task — the pool kept its
    /// last-good connections (ADR-0006); the section renders the
    /// error rather than pretending no servers exist.
    pub(crate) load_error: Option<String>,
}

/// One server's line in the catalog: name, the layer that declared
/// it, and availability.
pub(crate) struct McpCatalogEntry {
    /// The `mcpServers` name.
    pub(crate) name: String,
    /// User config or the project `.ait/` layer.
    pub(crate) origin: McpServerOrigin,
    /// Connected surface, declared-only, or the failure reason.
    pub(crate) availability: McpAvailability,
}

/// A catalog entry's availability.
pub(crate) enum McpAvailability {
    /// Connected and enumerated — the names the model may call.
    Available {
        /// Tool names from `tools/list`.
        tools: Vec<String>,
        /// Resource URIs from `resources/list`.
        resources: Vec<String>,
    },
    /// Project-tier — listed, unconnected; the gated call lazy-
    /// connects it.
    Declared,
    /// Initialization or a call failed — the reason.
    Unavailable(String),
}

impl McpCatalog {
    /// The system prompt's "MCP servers" section (Spec R10: "server
    /// names, origins and availability", tool/resource names once
    /// known): one line per entry — `available` lists the surface
    /// the model may name in `call_mcp_tool`/`read_mcp_resource`,
    /// `declared` marks a project-tier server that connects lazily
    /// on the first allowed call, `unavailable` carries the
    /// failure. `None` when nothing is
    /// declared and nothing failed, so a session with no `mcp.json`
    /// ships the Round 9 prompt verbatim.
    pub(crate) fn prompt_section(&self) -> Option<String> {
        if self.entries.is_empty() && self.load_error.is_none() {
            return None;
        }
        let mut text = String::from(
            "MCP servers declared in mcp.json — reach them with call_mcp_tool \
             and read_mcp_resource:",
        );
        if let Some(error) = &self.load_error {
            text.push_str(&format!(
                "\n- mcp.json failed to load: {error} — the last working set stays in effect"
            ));
        }
        for entry in &self.entries {
            let origin = entry.origin.label();
            match &entry.availability {
                McpAvailability::Available { tools, resources } => {
                    text.push_str(&format!("\n- {} ({origin}): available", entry.name));
                    match (tools.is_empty(), resources.is_empty()) {
                        (true, true) => text.push_str(" — no tools or resources"),
                        (false, true) => text.push_str(&format!(" — tools: {}", tools.join(", "))),
                        (true, false) => {
                            text.push_str(&format!(" — resources: {}", resources.join(", ")))
                        }
                        (false, false) => text.push_str(&format!(
                            " — tools: {}; resources: {}",
                            tools.join(", "),
                            resources.join(", ")
                        )),
                    }
                }
                McpAvailability::Declared => text.push_str(&format!(
                    "\n- {} ({origin}): declared — not connected; the first allowed \
                     call connects it",
                    entry.name
                )),
                McpAvailability::Unavailable(reason) => text.push_str(&format!(
                    "\n- {} ({origin}): unavailable — {reason}",
                    entry.name
                )),
            }
        }
        Some(text)
    }
}
