//! Serde round-trips at the public IR seam of the `openai_responses`
//! schema (Spec R03: `POST {base_url}/responses`). The IR's serialized
//! shape is the wire shape, so wire field names are asserted where they
//! carry meaning.

use ait_llm::openai_responses as oresp;

fn request() -> oresp::Request {
    oresp::Request {
        model: "gpt-5.2".into(),
        instructions: Some("be terse".into()),
        input: vec![
            oresp::Input::Message(oresp::Message {
                role: oresp::Role::User,
                content: vec![oresp::Content::InputText {
                    text: "list files".into(),
                }],
            }),
            oresp::Input::FunctionCall(oresp::FunctionCall {
                call_id: "call_1".into(),
                name: "run_command".into(),
                arguments: "{\"cmd\":\"ls\"}".into(),
            }),
            oresp::Input::FunctionCallOutput {
                call_id: "call_1".into(),
                output: "file1\nfile2".into(),
            },
        ],
        tools: vec![oresp::Tool {
            kind: oresp::ToolKind::Function,
            name: "run_command".into(),
            description: Some("run a shell command".into()),
            parameters: serde_json::json!({"type": "object"}),
            strict: None,
        }],
        temperature: Some(0.2),
        max_output_tokens: Some(512),
        stream: true,
    }
}

#[test]
fn request_round_trips_and_uses_wire_field_names() {
    let request = request();

    let json = serde_json::to_value(&request).unwrap();
    // `type` tags the input items; `role` tags the message (us-27).
    assert_eq!(json["input"][0]["type"], "message");
    assert_eq!(json["input"][0]["role"], "user");
    assert_eq!(json["input"][0]["content"][0]["type"], "input_text");
    assert_eq!(json["input"][1]["type"], "function_call");
    assert_eq!(json["input"][1]["call_id"], "call_1");
    assert_eq!(json["input"][2]["type"], "function_call_output");
    // The system prompt is top-level `instructions`, not a message.
    assert_eq!(json["instructions"], "be terse");
    assert_eq!(json["tools"][0]["type"], "function");

    let back: oresp::Request = serde_json::from_value(json).unwrap();
    assert_eq!(back, request);
}

#[test]
fn stream_events_cover_text_tool_call_finish_and_error() {
    let events = vec![
        oresp::StreamEvent::TextDelta { text: "he".into() },
        // us-29: the call is announced with id + name, then argument
        // fragments stream in.
        oresp::StreamEvent::ToolCallDelta(oresp::ToolCallDelta {
            item_id: Some("fc_1".into()),
            call_id: Some("call_1".into()),
            name: Some("run_command".into()),
            arguments: String::new(),
        }),
        oresp::StreamEvent::ToolCallDelta(oresp::ToolCallDelta {
            item_id: Some("fc_1".into()),
            call_id: None,
            name: None,
            arguments: "{\"cmd\":\"ls\"}".into(),
        }),
        oresp::StreamEvent::Finish {
            reason: oresp::FinishReason::Completed,
            usage: Some(oresp::Usage {
                input_tokens: 3,
                output_tokens: 7,
                total_tokens: 10,
            }),
        },
        oresp::StreamEvent::Error {
            message: "boom".into(),
            code: Some("rate_limit_exceeded".into()),
        },
    ];

    for event in events {
        let json = serde_json::to_value(&event).unwrap();
        let back: oresp::StreamEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, event);
    }
}

#[test]
fn finish_reason_uses_wire_names() {
    let json = serde_json::to_value(oresp::FinishReason::Incomplete).unwrap();
    assert_eq!(json, "incomplete");
}
