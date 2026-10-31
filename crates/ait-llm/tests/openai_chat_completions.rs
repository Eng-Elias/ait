//! Serde round-trips at the public IR seam of the `openai_chat_completions`
//! schema (Spec R03 acceptance: "All three IRs serde round-trip"). The IR's
//! serialized shape is the wire shape (`POST {base_url}/chat/completions`),
//! so wire field names are asserted where they carry meaning.

use ait_llm::openai_chat_completions as occ;

fn request() -> occ::Request {
    occ::Request {
        model: "qwen2.5-coder:14b".into(),
        messages: vec![
            occ::Message::System {
                content: "be terse".into(),
            },
            occ::Message::User {
                content: "list files".into(),
            },
            occ::Message::Assistant {
                content: None,
                tool_calls: vec![occ::ToolCall {
                    id: "call_1".into(),
                    kind: occ::ToolKind::Function,
                    function: occ::FunctionCall {
                        name: "run_command".into(),
                        arguments: "{\"cmd\":\"ls\"}".into(),
                    },
                }],
            },
            occ::Message::Tool {
                tool_call_id: "call_1".into(),
                content: "file1\nfile2".into(),
            },
        ],
        tools: vec![occ::Tool {
            kind: occ::ToolKind::Function,
            function: occ::ToolFunction {
                name: "run_command".into(),
                description: Some("run a shell command".into()),
                parameters: serde_json::json!({"type": "object"}),
            },
        }],
        temperature: Some(0.2),
        max_tokens: Some(512),
        stream: true,
    }
}

#[test]
fn request_round_trips_and_uses_wire_field_names() {
    let request = request();

    let json = serde_json::to_value(&request).unwrap();
    // The `role` tag is the wire discriminator (us-27).
    assert_eq!(json["messages"][0]["role"], "system");
    assert_eq!(json["messages"][1]["role"], "user");
    assert_eq!(json["messages"][2]["role"], "assistant");
    assert_eq!(json["messages"][2]["tool_calls"][0]["type"], "function");
    assert_eq!(json["messages"][3]["role"], "tool");
    assert_eq!(json["messages"][3]["tool_call_id"], "call_1");

    let back: occ::Request = serde_json::from_value(json).unwrap();
    assert_eq!(back, request);
}

#[test]
fn stream_events_cover_text_tool_call_finish_and_error() {
    let events = vec![
        occ::StreamEvent::TextDelta { text: "he".into() },
        // us-29: id, name and argument fragments arrive incrementally.
        occ::StreamEvent::ToolCallDelta(occ::ToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            name: Some("run_command".into()),
            arguments: "{\"cmd\":".into(),
        }),
        occ::StreamEvent::ToolCallDelta(occ::ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            arguments: "\"ls\"}".into(),
        }),
        occ::StreamEvent::Finish {
            reason: occ::FinishReason::ToolCalls,
            usage: Some(occ::Usage {
                prompt_tokens: 3,
                completion_tokens: 7,
                total_tokens: 10,
            }),
        },
        occ::StreamEvent::Error {
            message: "boom".into(),
            code: Some("rate_limit_exceeded".into()),
        },
    ];

    for event in events {
        let json = serde_json::to_value(&event).unwrap();
        let back: occ::StreamEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, event);
    }
}

#[test]
fn finish_reason_uses_wire_names() {
    let json = serde_json::to_value(occ::FinishReason::ToolCalls).unwrap();
    assert_eq!(json, "tool_calls");
}
