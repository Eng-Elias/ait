//! The smallest MCP stdio server ait-agent's tests can spawn — a
//! committed fixture binary, no network (CODING_STANDARDS §11):
//! `initialize`, `tools/list` (one `echo` tool), `tools/call`,
//! `resources/list` (one `fixture://one` resource),
//! `resources/read` and `ping`, speaking newline-delimited
//! JSON-RPC — one message per line, exactly what rmcp's
//! `AsyncRwTransport` reads.
//!
//! `cargo test -p ait-agent` builds the package's examples, so the
//! tests' `mcp.json` points `command` at
//! `target/debug/examples/mcp_fixture`. Logging goes to stderr —
//! stdout carries protocol frames only (the MCP spec's one hard
//! rule).
//!
//! Args: `--exit-on <method>` exits the process when a request for
//! that method arrives — the crash leg (`tools/call` kills the
//! server mid-call so the client reads `TransportClosed`).

use serde_json::{json, Value};
use std::io::{BufRead, Write};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let exit_on = args
        .windows(2)
        .find(|pair| pair[0] == "--exit-on")
        .map(|pair| pair[1].clone());

    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = msg["method"].as_str().unwrap_or("");
        if exit_on.as_deref() == Some(method) {
            // The crash leg: die mid-request without a reply so the
            // client's next read finds a closed transport.
            std::process::exit(3);
        }
        let id = msg.get("id").cloned();
        let reply = match (method, id) {
            ("initialize", Some(id)) => json!({"jsonrpc":"2.0","id":id,"result":{
                "protocolVersion":"2025-11-25",
                "capabilities":{"tools":{},"resources":{}},
                "serverInfo":{"name":"ait-mcp-fixture","version":"0.1.0"}}}),
            // A notification carries no id and gets no reply.
            ("notifications/initialized", _) => continue,
            ("tools/list", Some(id)) => json!({"jsonrpc":"2.0","id":id,"result":{"tools":[{
                "name":"echo","description":"Echoes `text` back",
                "inputSchema":{"type":"object",
                    "properties":{"text":{"type":"string"}},"required":["text"]}}]}}),
            ("tools/call", Some(id)) => {
                let text = msg["params"]["arguments"]["text"].as_str().unwrap_or("");
                json!({"jsonrpc":"2.0","id":id,"result":{
                    "content":[{"type":"text","text":format!("echo:{text}")}],
                    "isError":false}})
            }
            ("resources/list", Some(id)) => json!({"jsonrpc":"2.0","id":id,"result":{
                "resources":[{"uri":"fixture://one","name":"one","mimeType":"text/plain"}]}}),
            ("resources/read", Some(id)) => {
                let uri = msg["params"]["uri"].as_str().unwrap_or("");
                json!({"jsonrpc":"2.0","id":id,"result":{"contents":[{
                    "uri":uri,"mimeType":"text/plain","text":"FIXTURE-BODY"}]}})
            }
            ("ping", Some(id)) => json!({"jsonrpc":"2.0","id":id,"result":{}}),
            (_, Some(id)) => json!({"jsonrpc":"2.0","id":id,"error":{
                "code":-32601,"message":"method not found"}}),
            _ => continue,
        };
        writeln!(out, "{}", serde_json::to_string(&reply).unwrap()).unwrap();
        out.flush().unwrap();
    }
}
