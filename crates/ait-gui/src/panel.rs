//! The agent panel's paint planning (Spec R13 §5.12, ticket R13-T5):
//! [`AgentPanel`]'s shared state → styled display rows, the pinned
//! plan block, the parked gate card and the input row — everything
//! the egui painter draws decided here, in pure data, so the line
//! shapes are unit-testable without a GPU context.
//!
//! The functions are line-for-line ports of the TUI's
//! `ait::agent_panel` paint (`display_lines`/`plan_lines`/
//! `tool_card_lines`/`ask_card_lines`/`wrap`/`tail`) with ratatui
//! `Style`s replaced by [`Tone`]s — the frontend-neutral vocabulary
//! the painter resolves to `Color32`s (the `layout.rs` precedent:
//! frontend-local duplicates of a TUI formula, kept honest by
//! tests asserting the same shapes).

use ait_agent::{PlanTodo, PlanTodoStatus};
use ait_app::agent_panel::{self, AskCard, CardStatus, Entry, ToolCard};

// The shared display vocabulary — re-exported so the app's `panel::`
// call sites keep resolving while `ait-app` owns every literal
// (Spec R13 §5.12, CODING_STANDARDS §17: one source for the text
// both frontends draw).
pub use ait_app::agent_panel::{
    answer_choice, gate, tail, title, view_window, wrap, Gate, INPUT_PROMPT, PLAN_BLOCK_MAX,
    PROMPT_WIDTH,
};

/// A display row's color slot — the frontend-neutral replacement
/// for the TUI paint's ratatui `Style`s (bold folds into the tone:
/// egui's bundled monospace has no bold face, so the painter lifts
/// the foreground on `Strong`/`Alert` the way the grid's
/// `bold_color` degrade does).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Default foreground — assistant text, running card, pending
    /// todo, unhighlighted choice.
    Plain,
    /// Lifted foreground — the TUI's `Modifier::BOLD` rows: user
    /// entries (`› task`, `› steering`), the highlighted choice,
    /// the in-progress todo.
    Strong,
    /// Dimmed — terminal statuses, card details, the plan header,
    /// done todos, the answered-ask echo, diff file headers.
    Dim,
    /// The waiting gate's yellow — `?` cards and the prompt row.
    Alert,
    /// Green — `✓` done cards and answered asks, diff additions.
    Good,
    /// Red — `!` failed cards, diff deletions.
    Bad,
    /// Cyan — diff hunk headers (`@@`).
    Info,
}

/// One row the panel paints: text plus its tone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelLine {
    /// The row's text (already wrapped/clipped to the row width).
    pub text: String,
    /// How the painter colors it.
    pub tone: Tone,
}

/// The conversation flattened into display lines at `width`
/// columns: entries wrap char-wise (never mid-UTF-8), separated by
/// one blank line, and each role carries its tone — user entries
/// strong under `›`, assistant text plain, terminal statuses
/// dimmed. The TUI's `display_lines`, verbatim.
pub fn display_lines(entries: &[Entry], width: usize) -> Vec<PanelLine> {
    let mut lines = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            lines.push(line(String::new(), Tone::Plain));
        }
        let blocks: Vec<PanelLine> = match entry {
            Entry::Task(text) | Entry::Steering(text) => {
                vec![line(agent_panel::task_line(text), Tone::Strong)]
            }
            Entry::Text(text) => vec![line(text.clone(), Tone::Plain)],
            Entry::Tool(card) => tool_card_lines(card),
            Entry::Ask(card) => ask_card_lines(card),
            Entry::Status(text) => vec![line(text.clone(), Tone::Dim)],
        };
        for block in blocks {
            for wrapped in wrap(&block.text, width) {
                lines.push(line(wrapped, block.tone));
            }
        }
    }
    lines
}

/// The pinned plan block's display rows (Spec R08-T5, us-14): a
/// `plan` header, then one row per todo — `✓` done, `▸` in
/// progress, `○` pending — and a trailing blank separating the
/// block from the scrollback. More rows than `cap` folds the tail
/// into a `… N more` note, so a long plan can never crowd the
/// conversation out of the panel; below a useful minimum (header +
/// one todo + separator) the rows are better spent on scrollback
/// and the block sits out.
pub fn plan_lines(plan: &[PlanTodo], cap: usize) -> Vec<PanelLine> {
    if plan.is_empty() || cap < 3 {
        return Vec::new();
    }
    let mut lines = vec![line(String::from(agent_panel::PLAN_HEADER), Tone::Dim)];
    // `cap - 2` todo rows at most — one for the header, one for the
    // separator/overflow row.
    let visible = (cap - 2).min(plan.len());
    for todo in &plan[..visible] {
        let tone = match todo.status {
            PlanTodoStatus::Done => Tone::Dim,
            PlanTodoStatus::InProgress => Tone::Strong,
            PlanTodoStatus::Pending => Tone::Plain,
        };
        lines.push(line(agent_panel::todo_line(todo), tone));
    }
    if plan.len() > visible {
        // The row this note replaces counted as hidden too.
        let hidden = plan.len() - visible + 1;
        lines.pop();
        lines.push(line(agent_panel::plan_overflow(hidden), Tone::Dim));
    }
    lines.push(line(String::new(), Tone::Plain));
    lines
}

/// A tool card's display rows (Spec R06 us-5): a status-marked
/// header — `✓` done, `…` running, `?` awaiting approval, `✗`
/// denied, `!` failed — carrying `name summary` inline; a
/// multi-line summary (`edit_files`' diff, us-22) instead explodes
/// into one toned row per line under a name-only header. The
/// detail (result preview, error or denial text) follows indented
/// and dimmed when the update carried one.
fn tool_card_lines(card: &ToolCard) -> Vec<PanelLine> {
    let tone = match card.status {
        CardStatus::AwaitingApproval => Tone::Alert,
        CardStatus::Running => Tone::Plain,
        CardStatus::Done => Tone::Good,
        CardStatus::Denied => Tone::Dim,
        CardStatus::Failed => Tone::Bad,
    };
    let mut lines = vec![line(agent_panel::card_header(card), tone)];
    if card.summary.contains('\n') {
        // A multi-line summary — `edit_files`' unified diff
        // (Spec R06 us-22): each diff line paints its own row so
        // `+`/`-` color as additions/deletions instead of wrapping
        // as one blob.
        lines.extend(
            card.summary
                .lines()
                .map(|diff| line(agent_panel::diff_line(diff), diff_tone(diff))),
        );
    }
    if let Some(detail) = &card.detail {
        lines.push(line(agent_panel::detail_line(detail), Tone::Dim));
    }
    lines
}

/// The ask card's display rows (us-10): while it waits, `?` marks
/// the question and each choice lists beneath — `▸` on the
/// highlighted one; answered, `✓` marks the question and the answer
/// indents dimmed like a tool result's detail.
fn ask_card_lines(card: &AskCard) -> Vec<PanelLine> {
    match &card.answer {
        Some(answer) => vec![
            line(agent_panel::ask_question(card), Tone::Good),
            line(agent_panel::detail_line(answer), Tone::Dim),
        ],
        None => {
            let mut lines = vec![line(agent_panel::ask_question(card), Tone::Alert)];
            for (index, choice) in card.choices.iter().enumerate() {
                lines.push(if index == card.selected {
                    line(agent_panel::choice_line(choice, true), Tone::Strong)
                } else {
                    line(agent_panel::choice_line(choice, false), Tone::Plain)
                });
            }
            lines
        }
    }
}

/// Per-line tone for a unified-diff summary: `+` green, `-` red,
/// `@@` cyan, the `\ No newline` marker dimmed. `---`/`+++` file
/// headers stay dim rather than deletion-red/addition-green —
/// they label files, not churn.
fn diff_tone(text: &str) -> Tone {
    if text.starts_with("---") || text.starts_with("+++") {
        Tone::Dim
    } else if text.starts_with('+') {
        Tone::Good
    } else if text.starts_with('-') {
        Tone::Bad
    } else if text.starts_with('@') {
        Tone::Info
    } else if text.starts_with('\\') {
        Tone::Dim
    } else {
        Tone::Plain
    }
}

fn line(text: String, tone: Tone) -> PanelLine {
    PanelLine { text, tone }
}

#[cfg(test)]
mod tests {
    //! The planning seams (Spec R13's "pure planning half" rule):
    //! every shape the painter draws asserted on staged
    //! [`AgentPanel`]s — no egui, no GPU.

    use super::*;
    use ait_agent::{AgentEvent, AgentSession, ToolCall};
    use ait_app::agent_panel::{prompt_text, AgentPanel};
    use ait_app::config::NoSecrets;

    /// A panel over a session whose worker resolves nothing — the
    /// channel is real, the model is never reached (the `ait-app`
    /// tests' own fixture).
    fn panel() -> AgentPanel {
        AgentPanel::with_session(AgentSession::with_secret_store(NoSecrets))
    }

    /// A staged `ToolCall` for the card tests.
    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: String::new(),
        }
    }

    /// A staged `AskRequest` for the ask-card tests.
    fn ask(id: &str, question: &str, choices: &[&str]) -> AgentEvent {
        AgentEvent::AskRequest {
            call: ToolCall {
                id: id.into(),
                name: "ask_user_question".into(),
                arguments: String::new(),
            },
            question: question.into(),
            choices: choices.iter().map(|c| c.to_string()).collect(),
        }
    }

    fn todo(content: &str, status: PlanTodoStatus) -> PlanTodo {
        PlanTodo {
            content: content.into(),
            status,
        }
    }

    /// `wrap` honors embedded newlines and hard-wraps long runs —
    /// the same cases the TUI's paint tests pin.
    #[test]
    fn wrap_splits_on_newlines_and_width() {
        assert_eq!(wrap("ab\ncd", 5), vec!["ab", "cd"]);
        assert_eq!(wrap("abcd", 2), vec!["ab", "cd"]);
        assert_eq!(wrap("", 2), vec![""]);
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
    }

    /// `tail` keeps the edited end of an over-long buffer.
    #[test]
    fn tail_keeps_the_edited_end() {
        assert_eq!(tail("hello", 10), "hello");
        assert_eq!(tail("hello", 3), "llo");
        assert_eq!(tail("", 5), "");
    }

    /// Entries flatten into toned, wrapped rows: `›`-marked user
    /// entries strong, assistant text plain, statuses dimmed, a
    /// blank line between entries.
    #[test]
    fn display_lines_shape_the_conversation() {
        let entries = vec![
            Entry::Task("fix the tests".into()),
            Entry::Text("on it — reading".into()),
            Entry::Status("done".into()),
        ];
        let lines = display_lines(&entries, 40);
        assert_eq!(lines[0], line("› fix the tests".into(), Tone::Strong));
        assert_eq!(lines[1], line(String::new(), Tone::Plain));
        assert_eq!(lines[2], line("on it — reading".into(), Tone::Plain));
        assert_eq!(lines[3], line(String::new(), Tone::Plain));
        assert_eq!(lines[4], line("done".into(), Tone::Dim));

        // Wrapping: a long entry folds char-wise at the row width.
        let entries = vec![Entry::Text("abcdefgh".into())];
        let lines = display_lines(&entries, 4);
        assert_eq!(
            lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            vec!["abcd", "efgh"]
        );
    }

    /// A tool card renders its status mark, name+summary inline,
    /// and the dimmed detail line when the update carried one.
    #[test]
    fn tool_card_lines_carry_status_and_detail() {
        let card = ToolCard {
            id: "c1".into(),
            name: "run_command".into(),
            summary: "\"ls -la\"".into(),
            status: CardStatus::AwaitingApproval,
            detail: None,
        };
        assert_eq!(
            tool_card_lines(&card),
            vec![line("? run_command \"ls -la\"".into(), Tone::Alert)]
        );

        let card = ToolCard {
            id: "c1".into(),
            name: "grep".into(),
            summary: "\"TODO\"".into(),
            status: CardStatus::Done,
            detail: Some("3 matches".into()),
        };
        assert_eq!(
            tool_card_lines(&card),
            vec![
                line("✓ grep \"TODO\"".into(), Tone::Good),
                line("    3 matches".into(), Tone::Dim),
            ]
        );
    }

    /// A multi-line summary (`edit_files`' diff) explodes into
    /// per-line tones under a name-only header — `+`/`@@`/`-`
    /// colored as addition/hunk/deletion, `---`/`+++` file headers
    /// dimmed.
    #[test]
    fn diff_summaries_color_per_line() {
        let card = ToolCard {
            id: "c1".into(),
            name: "edit_files".into(),
            summary: "--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n-old\n+new".into(),
            status: CardStatus::Done,
            detail: None,
        };
        let lines = tool_card_lines(&card);
        assert_eq!(lines[0], line("✓ edit_files".into(), Tone::Good));
        assert_eq!(lines[1], line("  --- a/x.rs".into(), Tone::Dim));
        assert_eq!(lines[2], line("  +++ b/x.rs".into(), Tone::Dim));
        assert_eq!(lines[3], line("  @@ -1 +1 @@".into(), Tone::Info));
        assert_eq!(lines[4], line("  -old".into(), Tone::Bad));
        assert_eq!(lines[5], line("  +new".into(), Tone::Good));
    }

    /// A waiting ask renders `?` + choices with `▸` on the
    /// highlighted one; an answered one renders `✓` + the dimmed
    /// echo.
    #[test]
    fn ask_card_lines_wait_then_answer() {
        let mut card = AskCard {
            id: "a1".into(),
            question: "Which?".into(),
            choices: vec!["rewrite".into(), "patch".into()],
            selected: 1,
            answer: None,
        };
        let lines = ask_card_lines(&card);
        assert_eq!(lines[0], line("? Which?".into(), Tone::Alert));
        assert_eq!(lines[1], line("      rewrite".into(), Tone::Plain));
        assert_eq!(lines[2], line("    ▸ patch".into(), Tone::Strong));

        card.answer = Some("patch".into());
        assert_eq!(
            ask_card_lines(&card),
            vec![
                line("✓ Which?".into(), Tone::Good),
                line("    patch".into(), Tone::Dim),
            ]
        );
    }

    /// The plan block: header + one row per status + the trailing
    /// blank — and a plan longer than the cap folds its tail into a
    /// `… N more` note.
    #[test]
    fn plan_lines_pin_and_fold() {
        let plan = vec![
            todo("scan", PlanTodoStatus::Done),
            todo("edit", PlanTodoStatus::InProgress),
            todo("test", PlanTodoStatus::Pending),
        ];
        let lines = plan_lines(&plan, PLAN_BLOCK_MAX);
        assert_eq!(lines[0], line(" plan".into(), Tone::Dim));
        assert_eq!(lines[1], line("✓ scan".into(), Tone::Dim));
        assert_eq!(lines[2], line("▸ edit".into(), Tone::Strong));
        assert_eq!(lines[3], line("○ test".into(), Tone::Plain));
        assert_eq!(lines[4], line(String::new(), Tone::Plain));

        // Ten todos under a cap of five rows: header + 2 visible +
        // the fold note + blank — the last visible todo is replaced
        // by the note, which counts it as hidden too.
        let long: Vec<PlanTodo> = (0..10)
            .map(|n| todo(&format!("step{n}"), PlanTodoStatus::Pending))
            .collect();
        let lines = plan_lines(&long, 5);
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[3].text, "    … 8 more");
        // Below the useful minimum the block sits out entirely.
        assert!(plan_lines(&plan, 2).is_empty());
        assert!(plan_lines(&[], 10).is_empty());
    }

    /// `gate` reports the parked approve card (and its deny-reason
    /// mode), the parked ask card, and `None` when neither waits.
    #[test]
    fn gate_reports_the_parked_card() {
        let mut panel = panel();
        assert!(gate(&panel).is_none());

        panel.apply(AgentEvent::ApprovalRequest {
            call: call("call_1", "write_file"),
            summary: "x.rs".into(),
        });
        assert_eq!(
            gate(&panel),
            Some(Gate::Approve {
                name: "write_file",
                denying: false
            })
        );
        panel.begin_deny_reason();
        assert_eq!(
            gate(&panel),
            Some(Gate::Approve {
                name: "write_file",
                denying: true
            })
        );
        // A verdict clears the gate.
        let mut toast = None;
        panel.decide(ait_agent::ApprovalVerdict::Deny("no".into()), &mut toast);
        assert!(gate(&panel).is_none());

        panel.apply(ask("call_2", "Which?", &["a", "b"]));
        match gate(&panel) {
            Some(Gate::Ask(card)) => {
                assert_eq!(card.question, "Which?");
                assert_eq!(card.choices.len(), 2);
            }
            other => panic!("expected the ask gate, got {other:?}"),
        }
    }

    /// `prompt_text` mirrors the TUI's hint row: the three-state
    /// approve prompt, the ask prompts, `None` while idle.
    #[test]
    fn prompt_text_names_the_keys() {
        let mut panel = panel();
        assert!(prompt_text(&panel).is_none());
        panel.apply(AgentEvent::ApprovalRequest {
            call: call("c", "glob"),
            summary: "\"*\"".into(),
        });
        assert_eq!(
            prompt_text(&panel).as_deref(),
            Some(" approve glob — a: once · t: task · d: deny ")
        );
        panel.begin_deny_reason();
        assert_eq!(
            prompt_text(&panel).as_deref(),
            Some(" deny glob — type a reason · enter: deny ")
        );
    }

    /// The painted window: pinned rides the tail; unpinned clamps
    /// `view_top` — and appends can never outrun the clamp.
    #[test]
    fn view_window_pins_or_clamps() {
        let mut panel = panel();
        // 20 lines, 5 visible: max_top = 15.
        assert_eq!(view_window(&panel, 20, 5), (15, 15));
        // The paint publishes what it showed; a wheel up unpins off
        // that bound.
        panel.set_painted_view((15, 15));
        panel.scroll_up(4); // unpins → top 11
        assert_eq!(view_window(&panel, 20, 5), (11, 15));
        // Content shrinks or height grows: the clamp catches a
        // stale top.
        assert_eq!(view_window(&panel, 12, 5), (7, 7));
    }

    /// `answer_choice` answers the parked card with the clicked
    /// index — the click's counterpart of ↑/↓ + Enter.
    #[test]
    fn answer_choice_answers_the_clicked_row() {
        let mut panel = panel();
        panel.apply(ask("call_1", "Which?", &["a", "b", "c"]));

        let mut toast = None;
        answer_choice(&mut panel, 2, &mut toast);
        assert!(!panel.awaiting_answer());
        assert!(matches!(
            panel.entries().last(),
            Some(Entry::Ask(card)) if card.answer.as_deref() == Some("c")
        ));

        // No parked card → a no-op (stale click).
        let mut idle = AgentPanel::with_session(AgentSession::with_secret_store(NoSecrets));
        answer_choice(&mut idle, 0, &mut toast);
        assert!(toast.is_none());
    }
}
