//! The neutral persisted transcript vocabulary (Spec R08 §5.8,
//! ticket R08-T3): one serde-shaped [`TranscriptItem`] per
//! conversation event — the durable record a `transcript_items` row
//! persists verbatim and replay (`Conversation::from_transcript`,
//! R08-T4) rebuilds a per-schema IR from. The vocabulary is neutral
//! by design (user story 27): no schema's IR leaks in, so replay can
//! rebuild whichever schema the resolved `coding_model` speaks — a
//! mid-conversation endpoint switch strands nothing.
//!
//! The `ait` frontend mirrors the [`AgentEvent`](crate::AgentEvent)s
//! it already drains into this vocabulary (user story 28): `task`,
//! `steering` and `answer` items are recorded at send time (they
//! have no event), `TextDelta`s coalesce into one `assistant_text`
//! per assistant turn, and `ToolUpdate` terminal statuses become
//! `tool_result`s — `Running` is a card transition, not a record.
//! `plan` items mirror R08-T5's `PlanUpdated` event (one per
//! `update_plan` call); the `summary` kind still awaits R08-T7's
//! summarization marker — consumers are designed against this enum
//! verbatim.
//!
//! The store keeps payloads as opaque JSON; the vocabulary lives
//! here and only here.

use serde::{Deserialize, Serialize};

use crate::tool_call::ToolStatus;

/// One transcript record — the persisted form of a conversation
/// event. Serialized internally tagged (`{"kind":"task",…}`), so a
/// `transcript_items.payload_json` round-trips losslessly on its
/// own; the row's `kind` column just duplicates the tag for
/// SQL-level filtering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptItem {
    /// A submitted task — the user's words verbatim (the
    /// typed-request rule, §5.4 us-19). The first task's text titles
    /// the conversation row.
    Task {
        /// The task text.
        text: String,
    },
    /// A steering message — the user's words, recorded at send time
    /// in send order: a message queued while the worker idles
    /// precedes the next task's item here even though the model only
    /// reads it inside that task's conversation.
    Steering {
        /// The steering text.
        text: String,
    },
    /// One assistant turn's reply — a contiguous `TextDelta` run
    /// coalesced by the frontend mirror into a single item.
    AssistantText {
        /// The turn's full text.
        text: String,
    },
    /// A call the model made — emitted for the gated
    /// (`ApprovalRequest`) and ungated (`ToolCall`) cards alike.
    ToolCall {
        /// The provider-assigned call id the `ToolResult` answers.
        id: String,
        /// The registry name — `read_files`, `grep`, …
        name: String,
        /// The call's JSON arguments as the raw text the provider
        /// streamed — the same string `ToolCall::arguments` carries;
        /// replay re-parses it per schema exactly like the live
        /// path (OpenAI passes the string through, Anthropic parses
        /// it into `input`).
        args: String,
        /// The renderable argument summary the card showed.
        summary: String,
    },
    /// A call's terminal result — the full (already redacted) text
    /// that entered the model's conversation, not the card's clipped
    /// `detail` preview. A `tool_call` with no following result is a
    /// call the run never answered (a crash mid-run, an approve card
    /// parked at exit): replay closes it with a synthetic
    /// "interrupted" result (Spec R08 us-9).
    ToolResult {
        /// The `tool_call` this answers.
        id: String,
        /// The result text, verbatim — `Denied` results carry the
        /// refusal reason.
        text: String,
        /// How the call ended — `Done`, `Denied` or `Failed`;
        /// `Running` never reaches the transcript.
        status: ToolStatus,
    },
    /// `ask_user_question`'s card — the gate-exempt ask whose
    /// following `answer` item IS its tool result (the us-10
    /// precedent carried into the transcript: no `tool_call`/
    /// `tool_result` pair is recorded for it).
    Ask {
        /// The question the model asked.
        question: String,
        /// The offered choices; empty meant a free-text answer.
        choices: Vec<String>,
    },
    /// The user's answer to an `ask` — recorded at send time, where
    /// the full text is known (the worker's `ToolUpdate` only ever
    /// carries a preview).
    Answer {
        /// The answer text — the ask call's tool result verbatim.
        text: String,
    },
    /// A terminal-status line — the task's end state as the panel
    /// rendered it ("done", "cancelled", "step budget reached",
    /// "error: …"). Not model context: replay drops `status` items.
    Status {
        /// The status text.
        text: String,
    },
    /// A summarization marker (Spec R08 us-20/22): on replay `text`
    /// replaces the covered items as leading context while the
    /// newest tail and the current plan stay verbatim. Nothing emits
    /// it yet — R08-T7's summarized event lands here.
    Summary {
        /// The summary text injected as leading context on replay.
        text: String,
        /// How many leading transcript items (seq `0..covers_seq`)
        /// this summary replaces — a prefix count, since seqs run
        /// contiguous from zero and summarization always rewrites
        /// the oldest span.
        covers_seq: u64,
    },
    /// An `update_plan` snapshot (Spec R08 us-13/17): the TODO list
    /// the pinned plan block renders and the `plans` row upserts —
    /// one item per [`AgentEvent::PlanUpdated`](crate::AgentEvent::PlanUpdated).
    Plan {
        /// The whole TODO list — `update_plan` replaces it wholesale.
        todos: Vec<PlanTodo>,
    },
}

impl TranscriptItem {
    /// The `transcript_items.kind` column value — the serde tag,
    /// duplicated for SQL filtering (the payload carries it too).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Task { .. } => "task",
            Self::Steering { .. } => "steering",
            Self::AssistantText { .. } => "assistant_text",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::Ask { .. } => "ask",
            Self::Answer { .. } => "answer",
            Self::Status { .. } => "status",
            Self::Summary { .. } => "summary",
            Self::Plan { .. } => "plan",
        }
    }
}

/// One TODO in a `plan` snapshot — the shape `update_plan`'s
/// `{todos: [{content, status}]}` parameter takes (Spec R08;
/// replace-whole-list semantics).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTodo {
    /// What the step is.
    pub content: String,
    /// Where the step stands.
    pub status: PlanTodoStatus,
}

/// A TODO's state — the `update_plan` tool's status vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanTodoStatus {
    /// Not started.
    Pending,
    /// The step the agent is on.
    InProgress,
    /// Finished.
    Done,
}

#[cfg(test)]
mod tests {
    //! The persisted contract (Spec R08 testing decisions): every
    //! kind round-trips losslessly through serde, and the wire shape
    //! is golden-tested — replay (T4), `update_plan` (T5) and
    //! summarization (T7) consume this exact JSON.

    use super::*;

    /// Every kind, populated — the round-trip corpus.
    fn corpus() -> Vec<TranscriptItem> {
        vec![
            TranscriptItem::Task {
                text: "find why the tests fail".into(),
            },
            TranscriptItem::Steering {
                text: "and quickly".into(),
            },
            TranscriptItem::AssistantText {
                text: "the tests fail because".into(),
            },
            TranscriptItem::ToolCall {
                id: "call_1".into(),
                name: "read_files".into(),
                args: "{\"paths\":[\"src/main.rs\"]}".into(),
                summary: "src/main.rs".into(),
            },
            TranscriptItem::ToolResult {
                id: "call_1".into(),
                text: "fn main() {}".into(),
                status: ToolStatus::Done,
            },
            TranscriptItem::ToolResult {
                id: "call_2".into(),
                text: "denied by execution profile default".into(),
                status: ToolStatus::Denied,
            },
            TranscriptItem::Ask {
                question: "Ship it?".into(),
                choices: vec!["yes".into(), "no".into()],
            },
            TranscriptItem::Answer { text: "no".into() },
            TranscriptItem::Status {
                text: "done".into(),
            },
            TranscriptItem::Summary {
                text: "the first half, summarized".into(),
                covers_seq: 7,
            },
            TranscriptItem::Plan {
                todos: vec![
                    PlanTodo {
                        content: "reproduce".into(),
                        status: PlanTodoStatus::Done,
                    },
                    PlanTodo {
                        content: "fix".into(),
                        status: PlanTodoStatus::InProgress,
                    },
                    PlanTodo {
                        content: "verify".into(),
                        status: PlanTodoStatus::Pending,
                    },
                ],
            },
        ]
    }

    /// Every kind serializes and deserializes back to itself —
    /// payload_json alone is the whole record.
    #[test]
    fn every_kind_round_trips_through_serde() {
        for item in corpus() {
            let json = serde_json::to_string(&item).expect("serialize");
            let back: TranscriptItem = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("{item:?} did not round-trip ({e}): {json}"));
            assert_eq!(back, item);
        }
    }

    /// The exact wire shape: internally tagged `{"kind":…, fields…}`
    /// in snake_case — what `payload_json` holds and what the `kind`
    /// column duplicates.
    #[test]
    fn the_serialized_shape_is_the_golden_contract() {
        let cases: Vec<(TranscriptItem, &str)> = vec![
            (
                TranscriptItem::Task {
                    text: "fix it".into(),
                },
                r#"{"kind":"task","text":"fix it"}"#,
            ),
            (
                TranscriptItem::Steering {
                    text: "quickly".into(),
                },
                r#"{"kind":"steering","text":"quickly"}"#,
            ),
            (
                TranscriptItem::AssistantText { text: "hi".into() },
                r#"{"kind":"assistant_text","text":"hi"}"#,
            ),
            (
                TranscriptItem::ToolCall {
                    id: "call_1".into(),
                    name: "glob".into(),
                    args: "{\"pattern\":\"**/*.rs\"}".into(),
                    summary: "**/*.rs".into(),
                },
                "{\"kind\":\"tool_call\",\"id\":\"call_1\",\"name\":\"glob\",\
                 \"args\":\"{\\\"pattern\\\":\\\"**/*.rs\\\"}\",\"summary\":\"**/*.rs\"}",
            ),
            (
                TranscriptItem::ToolResult {
                    id: "call_1".into(),
                    text: "oops".into(),
                    status: ToolStatus::Failed,
                },
                r#"{"kind":"tool_result","id":"call_1","text":"oops","status":"failed"}"#,
            ),
            (
                TranscriptItem::Ask {
                    question: "which?".into(),
                    choices: vec![],
                },
                r#"{"kind":"ask","question":"which?","choices":[]}"#,
            ),
            (
                TranscriptItem::Answer {
                    text: "the second".into(),
                },
                r#"{"kind":"answer","text":"the second"}"#,
            ),
            (
                TranscriptItem::Status {
                    text: "cancelled".into(),
                },
                r#"{"kind":"status","text":"cancelled"}"#,
            ),
            (
                TranscriptItem::Summary {
                    text: "older half".into(),
                    covers_seq: 4,
                },
                r#"{"kind":"summary","text":"older half","covers_seq":4}"#,
            ),
            (
                TranscriptItem::Plan {
                    todos: vec![PlanTodo {
                        content: "ship".into(),
                        status: PlanTodoStatus::InProgress,
                    }],
                },
                r#"{"kind":"plan","todos":[{"content":"ship","status":"in_progress"}]}"#,
            ),
        ];
        for (item, golden) in cases {
            assert_eq!(serde_json::to_string(&item).expect("serialize"), golden);
            // …and `kind()` names the same tag the column stores.
            let tag = serde_json::from_str::<serde_json::Value>(golden).unwrap()["kind"]
                .as_str()
                .unwrap()
                .to_owned();
            assert_eq!(item.kind(), tag);
        }
    }

    /// `ToolStatus` serializes snake_case — `done`, `denied`,
    /// `failed` — inside `tool_result` payloads.
    #[test]
    fn tool_status_serializes_snake_case() {
        for (status, tag) in [
            (ToolStatus::Running, "running"),
            (ToolStatus::Done, "done"),
            (ToolStatus::Denied, "denied"),
            (ToolStatus::Failed, "failed"),
        ] {
            let json = serde_json::to_string(&status).expect("serialize");
            assert_eq!(json, format!("\"{tag}\""));
            assert_eq!(
                serde_json::from_str::<ToolStatus>(&json).expect("deserialize"),
                status
            );
        }
    }
}
