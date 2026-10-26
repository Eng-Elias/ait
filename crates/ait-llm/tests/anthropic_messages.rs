//! Serde round-trips at the public IR seam of the `anthropic_messages`
//! schema (Spec R03: `POST {base_url}/messages`). The IR's serialized shape
//! is the wire shape, so wire field names are asserted where they carry
//! meaning.

use ait_llm::anthropic_messages as anth;

fn request() -> anth::Request {
    anth::Request {
        model: "claude-sonnet-4-5".into(),
        system: Some("be terse".into()),
        messages: vec![
            anth::Message {
                role: anth::Role::User,
                content: vec![anth::ContentBlock::Text {
                    text: "list files".into(),
                }],
            },
            anth::Message {
                role: anth::Role::Assistant,
                content: vec![anth::ContentBlock::ToolUse {
                    id: "toolu_1".into(),
                    name: "run_command".into(),
                    input: serde_json::json!({"cmd": "ls"}),
                }],
            },
            anth::Message {
                role: anth::Role::User,
                content: vec![anth::ContentBlock::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "file1\nfile2".into(),
                }],
            },
        ],
        tools: vec![anth::Tool {
            name: "run_command".into(),
            description: Some("run a shell command".into()),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        max_tokens: 1024,
        temperature: Some(0.2),
        stream: true,
    }
}

#[test]
fn request_round_trips_and_uses_wire_field_names() {
    let request = request();

    let json = serde_json::to_value(&request).unwrap();
    // The system prompt is top-level `system`, never a message (us-27).
    assert_eq!(json["system"], "be terse");
    assert_eq!(json["messages"][0]["role"], "user");
    assert_eq!(json["messages"][0]["content"][0]["type"], "text");
    assert_eq!(json["messages"][1]["role"], "assistant");
    assert_eq!(json["messages"][1]["content"][0]["type"], "tool_use");
    assert_eq!(json["messages"][1]["content"][0]["id"], "toolu_1");
    assert_eq!(json["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(json["messages"][2]["content"][0]["tool_use_id"], "toolu_1");
    assert_eq!(json["max_tokens"], 1024);

    let back: anth::Request = serde_json::from_value(json).unwrap();
    assert_eq!(back, request);
}

#[test]
fn stream_events_cover_text_tool_call_finish_and_error() {
    let events = vec![
        anth::StreamEvent::TextDelta { text: "he".into() },
        // us-29: a tool_use block opens with id + name, then `partial_json`
        // argument fragments stream in, all addressed by block index.
        anth::StreamEvent::ToolCallDelta(anth::ToolCallDelta {
            index: 1,
            id: Some("toolu_1".into()),
            name: Some("run_command".into()),
            arguments: String::new(),
        }),
        anth::StreamEvent::ToolCallDelta(anth::ToolCallDelta {
            index: 1,
            id: None,
            name: None,
            arguments: "{\"cmd\":".into(),
        }),
        anth::StreamEvent::Finish {
            reason: anth::FinishReason::ToolUse,
            usage: Some(anth::Usage {
                input_tokens: Some(3),
                output_tokens: Some(7),
            }),
        },
        anth::StreamEvent::Error {
            message: "boom".into(),
            code: Some("overloaded_error".into()),
        },
    ];

    for event in events {
        let json = serde_json::to_value(&event).unwrap();
        let back: anth::StreamEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, event);
    }
}

#[test]
fn finish_reason_uses_wire_names() {
    let json = serde_json::to_value(anth::FinishReason::EndTurn).unwrap();
    assert_eq!(json, "end_turn");
}
