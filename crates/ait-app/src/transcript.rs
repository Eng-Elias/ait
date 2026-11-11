//! The `AgentEvent` → [`TranscriptItem`] mirror (Spec R08 §5.8,
//! ticket R08-T3, user story 28): every event the frontend already
//! drains folds into the neutral vocabulary `ait-agent` owns, so
//! persistence needs no new channel surface. `task`, `steering` and
//! `answer` have no events — they are recorded at send time, in send
//! order.
//!
//! One [`Transcript`] lives per agent panel: the accumulated items
//! are both the store's append source (the per-drain `take_new`) and
//! the in-memory transcript a next task's `AgentRequest.prior` will
//! replay (R08-T4) — kept in full either way, whether or not a store
//! is live. The fold itself is shared
//! [`ait_agent::TranscriptMirror`] — R08-T7's worker keeps the same
//! mirror so a `summary` marker's `covers_seq` counts in this
//! transcript's own seq domain.

use ait_agent::{AgentEvent, TranscriptItem};

/// A conversation row's title source — the first task's first line,
/// truncated (Spec R08 us-5: "titled from the first task").
const TITLE_CHARS: usize = 60;

/// One panel's accumulated transcript — the shared
/// [`ait_agent::TranscriptMirror`] fold plus the per-drain flush
/// watermark the store's append batching needs.
#[derive(Default)]
pub struct Transcript {
    /// The whole accumulated transcript — the store's append source
    /// AND the `prior` continuity record (T4): a disabled store never
    /// truncates it. The fold rules live on `TranscriptMirror`.
    mirror: ait_agent::TranscriptMirror,
    /// Items already handed to a [`take_new`](Self::take_new) caller —
    /// the per-drain watermark; the take advances it whether or not
    /// the write landed (the unpersisted-run rule).
    flushed: usize,
}

impl Transcript {
    /// A transcript adopted whole — resume's restore (R08-T6): every
    /// item is already persisted, so `flushed` starts at the end and
    /// nothing re-appends on the next drain. The items seed the
    /// mirror verbatim via `record` (the fold's asks-dedup state
    /// starts empty: a restored `ask` is long recorded, and no
    /// in-flight call survives a restore — resume refuses while a
    /// task runs).
    pub fn restored(items: Vec<TranscriptItem>) -> Self {
        let mut mirror = ait_agent::TranscriptMirror::default();
        for item in items {
            mirror.record(item);
        }
        let flushed = mirror.items().len();
        Self { mirror, flushed }
    }

    /// The send-side record of a submitted task — first task of a
    /// flush is what [`conversation_title`] reads.
    pub fn task(&mut self, text: &str) {
        self.mirror.task(text);
    }

    /// The send-side record of a steering message — in send order,
    /// even while the worker only reads it inside the next task's
    /// conversation.
    pub fn steering(&mut self, text: &str) {
        self.mirror.steering(text);
    }

    /// The send-side record of an `ask_user_question` answer — the
    /// full text is known here; the matching `ToolUpdate` only ever
    /// carries a preview (and is skipped on it).
    pub fn answer(&mut self, text: &str) {
        self.mirror.answer(text);
    }

    /// Fold one drained [`AgentEvent`] into items — the shared
    /// mirror rules. Called for every event the panel applies.
    pub fn apply(&mut self, event: &AgentEvent) {
        self.mirror.apply(event);
    }

    /// The items accumulated since the last take — the store's
    /// per-drain batch. The watermark advances regardless of what
    /// the caller does with them (the unpersisted-run rule); the
    /// full vec stays — it is the `prior` continuity record too.
    pub fn take_new(&mut self) -> Vec<TranscriptItem> {
        let items = self.mirror.items();
        let new = items[self.flushed..].to_vec();
        self.flushed = items.len();
        new
    }

    /// The whole accumulated transcript — what a resumed tab's next
    /// `AgentRequest.prior` hands the worker (R08-T4).
    pub fn items(&self) -> &[TranscriptItem] {
        self.mirror.items()
    }
}

/// The `conversations.title` for a flush: the first task's first
/// line in `batch`, trimmed and clipped to [`TITLE_CHARS`] — Spec
/// R08's "titled from the first task, truncated". A batch starting
/// mid-conversation (the row it continued was deleted — us-11's
/// detach) can hold no task at all, so `history` — the panel's
/// whole transcript — names the row from the conversation's own
/// first task instead; only a taskless history gets the generic.
pub fn conversation_title(batch: &[TranscriptItem], history: &[TranscriptItem]) -> String {
    let line = batch
        .iter()
        .chain(history)
        .find_map(|item| match item {
            TranscriptItem::Task { text } => Some(text.as_str()),
            _ => None,
        })
        .and_then(|text| text.lines().next())
        .unwrap_or_default()
        .trim();
    let mut title: String = line.chars().take(TITLE_CHARS).collect();
    if line.chars().count() > TITLE_CHARS {
        title.push('…');
    }
    if title.is_empty() {
        "untitled conversation".to_string()
    } else {
        title
    }
}

#[cfg(test)]
mod tests {
    //! The mirror seam (Spec R08 us-28): staged events and send-side
    //! records in, ordered [`TranscriptItem`]s out — the exact item
    //! list a `transcript_items` flush appends.

    use super::*;
    use ait_agent::{ToolCall, ToolStatus};

    /// A staged call.
    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: "{}".into(),
        }
    }

    /// A done-update carrying the full result.
    fn done_update(id: &str, result: &str) -> AgentEvent {
        AgentEvent::ToolUpdate {
            call_id: id.into(),
            status: ToolStatus::Done,
            detail: Some(result.lines().next().unwrap_or_default().into()),
            result: Some(result.into()),
        }
    }

    /// Spec R08 us-28 + ticket: a contiguous `TextDelta` run is ONE
    /// `assistant_text`; a turn's tool turn ends the run and the next
    /// deltas open a fresh one.
    #[test]
    fn deltas_coalesce_into_one_item_per_assistant_turn() {
        let mut transcript = Transcript::default();
        transcript.apply(&AgentEvent::TextDelta("The tests ".into()));
        transcript.apply(&AgentEvent::TextDelta("fail because".into()));
        transcript.apply(&AgentEvent::ToolCall {
            call: call("call_1", "read_files"),
            summary: "a.rs".into(),
        });
        transcript.apply(&done_update("call_1", "contents"));
        transcript.apply(&AgentEvent::TextDelta("all read".into()));

        assert_eq!(
            transcript.items(),
            &[
                TranscriptItem::AssistantText {
                    text: "The tests fail because".into(),
                },
                TranscriptItem::ToolCall {
                    id: "call_1".into(),
                    name: "read_files".into(),
                    args: "{}".into(),
                    summary: "a.rs".into(),
                },
                TranscriptItem::ToolResult {
                    id: "call_1".into(),
                    text: "contents".into(),
                    status: ToolStatus::Done,
                },
                TranscriptItem::AssistantText {
                    text: "all read".into(),
                },
            ]
        );
    }

    /// The gated card records the same `tool_call` the ungated one
    /// does — the approve/ungated distinction is frontend state.
    #[test]
    fn approval_request_records_a_tool_call() {
        let mut transcript = Transcript::default();
        transcript.apply(&AgentEvent::ApprovalRequest {
            call: call("call_1", "run_command"),
            summary: "\"rm x\"".into(),
        });
        transcript.apply(&AgentEvent::ToolUpdate {
            call_id: "call_1".into(),
            status: ToolStatus::Denied,
            detail: Some("not today".into()),
            result: Some("not today".into()),
        });
        assert_eq!(
            transcript.items(),
            &[
                TranscriptItem::ToolCall {
                    id: "call_1".into(),
                    name: "run_command".into(),
                    args: "{}".into(),
                    summary: "\"rm x\"".into(),
                },
                TranscriptItem::ToolResult {
                    id: "call_1".into(),
                    text: "not today".into(),
                    status: ToolStatus::Denied,
                },
            ]
        );
    }

    /// `Running` carries no `result` — it is a card transition and
    /// leaves no item; the call stays unanswered until a terminal
    /// update (which is what replay's "interrupted" rule keys on).
    #[test]
    fn running_updates_leave_no_item() {
        let mut transcript = Transcript::default();
        transcript.apply(&AgentEvent::ToolCall {
            call: call("call_1", "glob"),
            summary: "s".into(),
        });
        transcript.apply(&AgentEvent::ToolUpdate {
            call_id: "call_1".into(),
            status: ToolStatus::Running,
            detail: None,
            result: None,
        });
        assert_eq!(transcript.items().len(), 1, "only the tool_call");
    }

    /// The ask card's `Done` update would duplicate the `answer` —
    /// the pair on record is `ask` + `answer`, no `tool_result`.
    #[test]
    fn an_ask_records_ask_and_answer_not_a_result() {
        let mut transcript = Transcript::default();
        transcript.apply(&AgentEvent::AskRequest {
            call: call("call_1", "ask_user_question"),
            question: "Ship it?".into(),
            choices: vec!["yes".into(), "no".into()],
        });
        transcript.answer("no");
        transcript.apply(&AgentEvent::ToolUpdate {
            call_id: "call_1".into(),
            status: ToolStatus::Done,
            detail: Some("no".into()),
            result: Some("no".into()),
        });
        assert_eq!(
            transcript.items(),
            &[
                TranscriptItem::Ask {
                    question: "Ship it?".into(),
                    choices: vec!["yes".into(), "no".into()],
                },
                TranscriptItem::Answer { text: "no".into() },
            ]
        );
    }

    /// Spec R08-T5: a `PlanUpdated` folds into a `plan` item — the
    /// whole snapshot, so the row append and the `plans` upsert read
    /// the same list (each call's item is a new row: the evolution
    /// stays auditable like every other item).
    #[test]
    fn a_plan_update_records_a_plan_item() {
        let mut transcript = Transcript::default();
        let todos = || {
            vec![
                ait_agent::PlanTodo {
                    content: "reproduce".into(),
                    status: ait_agent::PlanTodoStatus::Done,
                },
                ait_agent::PlanTodo {
                    content: "fix".into(),
                    status: ait_agent::PlanTodoStatus::InProgress,
                },
            ]
        };
        transcript.apply(&AgentEvent::PlanUpdated { todos: todos() });
        transcript.apply(&AgentEvent::PlanUpdated { todos: vec![] });
        assert_eq!(
            transcript.items(),
            &[
                TranscriptItem::Plan { todos: todos() },
                TranscriptItem::Plan { todos: vec![] },
            ]
        );
    }

    /// Every terminal event lands its status line — the text the
    /// panel renders.
    #[test]
    fn terminal_events_record_status_lines() {
        let mut transcript = Transcript::default();
        transcript.apply(&AgentEvent::Done);
        transcript.apply(&AgentEvent::Cancelled);
        transcript.apply(&AgentEvent::BudgetReached);
        transcript.apply(&AgentEvent::Error("endpoint down".into()));
        assert_eq!(
            transcript.items(),
            &[
                TranscriptItem::Status {
                    text: "done".into()
                },
                TranscriptItem::Status {
                    text: "cancelled".into(),
                },
                TranscriptItem::Status {
                    text: "step budget reached".into(),
                },
                TranscriptItem::Status {
                    text: "error: endpoint down".into(),
                },
            ]
        );
    }

    /// Send-order is the transcript's order: a steering message sent
    /// while the worker idles precedes the next task's item even
    /// though the model reads it inside that task.
    #[test]
    fn send_side_items_record_in_send_order() {
        let mut transcript = Transcript::default();
        transcript.task("first");
        transcript.steering("while you think");
        transcript.task("second");
        assert_eq!(
            transcript.items(),
            &[
                TranscriptItem::Task {
                    text: "first".into(),
                },
                TranscriptItem::Steering {
                    text: "while you think".into(),
                },
                TranscriptItem::Task {
                    text: "second".into(),
                },
            ]
        );
    }

    /// Spec R08-T6: a transcript adopted whole yields nothing to the
    /// store — every item is already persisted — while `items()`
    /// hands the whole thing to the next task's `prior`.
    #[test]
    fn a_restored_transcript_flushes_nothing_but_carries_everything() {
        let items = vec![
            TranscriptItem::Task {
                text: "fix it".into(),
            },
            TranscriptItem::Status {
                text: "done".into(),
            },
        ];
        let mut transcript = Transcript::restored(items.clone());
        assert!(
            transcript.take_new().is_empty(),
            "already-persisted items never re-append"
        );
        assert_eq!(transcript.items(), items.as_slice());
        // New sends record on top and DO flush.
        transcript.task("and the lint");
        assert_eq!(
            transcript.take_new(),
            vec![TranscriptItem::Task {
                text: "and the lint".into()
            }]
        );
    }

    /// `take_new` is the per-drain watermark: each take yields only
    /// what arrived since the last — while `items()` keeps the whole
    /// transcript for T4's `prior`.
    #[test]
    fn take_new_yields_each_item_once_but_keeps_the_whole() {
        let mut transcript = Transcript::default();
        transcript.task("fix it");
        transcript.apply(&AgentEvent::TextDelta("on it".into()));

        let first = transcript.take_new();
        assert_eq!(first.len(), 2);
        assert!(
            transcript.take_new().is_empty(),
            "drained items are not re-yielded"
        );
        transcript.apply(&AgentEvent::Done);
        assert_eq!(
            transcript.take_new(),
            vec![TranscriptItem::Status {
                text: "done".into()
            }]
        );
        assert_eq!(
            transcript.items().len(),
            3,
            "the full transcript outlives the flushes"
        );
    }

    /// The row title: the batch's first task's first line,
    /// truncated — the transcript's own first task names a batch
    /// that starts mid-conversation (post-detach), and a fully
    /// taskless history falls back to a nameable row.
    #[test]
    fn conversation_title_takes_the_first_task_first_line() {
        let empty: &[TranscriptItem] = &[];
        assert_eq!(
            conversation_title(
                &[
                    TranscriptItem::Task {
                        text: "fix the tests\nplease".into(),
                    },
                    TranscriptItem::Task {
                        text: "never the second".into(),
                    },
                ],
                empty,
            ),
            "fix the tests"
        );
        let long = "x".repeat(TITLE_CHARS + 10);
        assert_eq!(
            conversation_title(&[TranscriptItem::Task { text: long }], empty),
            format!("{}…", "x".repeat(TITLE_CHARS)),
            "clipped with an ellipsis"
        );
        // A mid-conversation batch: the detached row's first task is
        // the title (the new row continues the same conversation).
        assert_eq!(
            conversation_title(
                &[TranscriptItem::Status { text: "s".into() }],
                &[
                    TranscriptItem::Task {
                        text: "the detached row's task".into(),
                    },
                    TranscriptItem::Status { text: "s".into() },
                ],
            ),
            "the detached row's task"
        );
        // …but a batch task still wins over history's.
        assert_eq!(
            conversation_title(
                &[TranscriptItem::Task {
                    text: "fresh work".into(),
                }],
                &[TranscriptItem::Task {
                    text: "the detached row's task".into(),
                }],
            ),
            "fresh work"
        );
        assert_eq!(
            conversation_title(&[TranscriptItem::Status { text: "s".into() }], empty),
            "untitled conversation"
        );
    }
}
