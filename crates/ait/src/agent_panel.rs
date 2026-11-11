//! The agent conversation panel's TUI paint — the ratatui half of
//! the widget whose state lives in `ait_app::agent_panel` (Spec R13
//! §5.12: the frontend-agnostic state moved with the crate; what
//! stays here is exactly the `Buffer` writing).
//!
//! [`paint_agent_panel`] reads the shared state through its public
//! getters — `entries()`, the pinned `plan()` block, `input()`, the
//! parked gates — and publishes the scroll window it actually showed
//! back through `set_painted_view`, so the shared wheel handlers
//! bound against the real layout.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RtColor, Modifier, Style};

use ait_agent::{PlanTodo, PlanTodoStatus};
use ait_app::agent_panel::{AgentPanel, AskCard, CardStatus, Entry, ToolCard};

/// The input line's prompt marker — user text (a submitted task and a
/// mid-run steering message alike) carries the same marker in the
/// scrollback.
const INPUT_PROMPT: &str = "› ";

/// [`INPUT_PROMPT`]'s cell width — `›` is three bytes but one column;
/// input-line math works in cells, never `str::len`.
const PROMPT_WIDTH: usize = 2;

/// The pinned plan block's row cap — header, visible todos and the
/// trailing separator together (Spec R08-T5). A longer plan folds
/// into a `… N more` row rather than crowding the scrollback out of
/// the panel.
const PLAN_BLOCK_MAX: usize = 10;

/// The host cursor's screen position while the panel is focused: the
/// end of the input line — the input row's tail-clip means it can sit
/// at the row's right edge on a long entry. A `Rect` argument is the
/// frontend's own layout, which is why this sits beside the paint
/// rather than on the shared widget.
pub fn panel_cursor_position(panel: &AgentPanel, area: Rect) -> (u16, u16) {
    let inner_x = area.x + 1;
    let inner_width = usize::from(area.width.saturating_sub(1));
    let room = inner_width.saturating_sub(PROMPT_WIDTH);
    let shown = tail(panel.input(), room);
    let x = inner_x + PROMPT_WIDTH as u16 + shown.chars().count() as u16;
    (x.min(area.right().saturating_sub(1)), area.bottom() - 1)
}

/// Paint the panel into `area`: a `│` separator down the shared edge,
/// the title row (reversed while focused — the focus affordance the
/// ticket pins), the wrapped conversation between, and the input line
/// on the last row. All writes reset their span first, matching every
/// other painter's contract.
pub fn paint_agent_panel(buf: &mut Buffer, area: Rect, panel: &AgentPanel) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    for y in area.y..area.bottom() {
        let cell = &mut buf[(area.x, y)];
        cell.reset();
        cell.set_char('│');
    }
    let inner = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(1),
        ..area
    };
    if inner.width == 0 {
        return;
    }
    // Title row: the focus affordance — reversed while the panel owns
    // the keyboard — plus the in-flight hint while a task runs.
    reset_span(buf, inner.y, inner.x..inner.right());
    let title = if panel.is_running() {
        " agent — working "
    } else {
        " agent "
    };
    let width = usize::from(inner.width);
    buf.set_stringn(inner.x, inner.y, title, width, Style::default());
    if panel.is_focused() {
        for x in inner.x..inner.right() {
            buf[(x, inner.y)].modifier |= Modifier::REVERSED;
        }
    }
    if inner.height < 2 {
        return;
    }
    // The input line owns the last row: `› ` plus the tail of the
    // buffer (long input clips from the left — the cursor end is the
    // half the user is editing). A parked card takes the row just
    // above — the approve card while the gate waits, the ask card's
    // prompt while a question waits — it is never hidden, and the
    // conversation loses that row for the duration.
    let input_y = inner.bottom() - 1;
    reset_span(buf, input_y, inner.x..inner.right());
    let room = width.saturating_sub(PROMPT_WIDTH);
    let shown = tail(panel.input(), room);
    buf.set_stringn(
        inner.x,
        input_y,
        format!("{INPUT_PROMPT}{shown}"),
        width,
        Style::default(),
    );
    let prompt_text = if let Some((name, denying)) = panel.pending_approval() {
        Some(if denying {
            format!(" deny {name} — type a reason · enter: deny ")
        } else {
            format!(" approve {name} — a: once · t: task · d: deny ")
        })
    } else if panel.awaiting_answer() {
        Some(if panel.ask_has_choices() {
            " ask — ↑/↓ choose · enter: answer ".to_string()
        } else {
            " ask — type your answer · enter: send ".to_string()
        })
    } else {
        None
    };
    let prompt_rows = if let (Some(text), true) = (prompt_text, input_y > inner.y + 1) {
        let y = input_y - 1;
        reset_span(buf, y, inner.x..inner.right());
        buf.set_stringn(
            inner.x,
            y,
            text,
            width,
            Style::default()
                .fg(RtColor::Yellow)
                .add_modifier(Modifier::BOLD),
        );
        1
    } else {
        0
    };
    if inner.height < 3 {
        return;
    }
    // The pinned plan block (Spec R08-T5, us-14): directly under the
    // title — each `update_plan` snapshot replaces its contents — and
    // the conversation scrolls beneath. The cap keeps a long plan
    // from crowding the scrollback out of the panel.
    let available = usize::from(input_y - (inner.y + 1)).saturating_sub(prompt_rows);
    let block = plan_lines(
        panel.plan(),
        PLAN_BLOCK_MAX.min(available.saturating_sub(1)),
    );
    let plan_rows = block.len() as u16;
    for (index, (text, style)) in block.iter().enumerate() {
        let y = inner.y + 1 + index as u16;
        reset_span(buf, y, inner.x..inner.right());
        buf.set_stringn(inner.x, y, text, width, *style);
    }
    // The conversation between them, pinned to the tail unless the
    // user scrolled up — the paint pass owns the wrap, so it also
    // owns the scroll clamp: `top`/`painted` are what the wheel
    // handlers bound against.
    let conversation = display_lines(panel.entries(), width);
    let height = available - block.len();
    let max_top = conversation.len().saturating_sub(height);
    let top = if panel.is_pinned() {
        max_top
    } else {
        panel.view_top().min(max_top)
    };
    panel.set_painted_view((top, max_top));
    for row in 0..height {
        let y = inner.y + 1 + plan_rows + row as u16;
        reset_span(buf, y, inner.x..inner.right());
        if let Some((text, style)) = conversation.get(top + row) {
            buf.set_stringn(inner.x, y, text, width, *style);
        }
    }
}

/// The conversation flattened into display lines at `width` columns:
/// entries wrap char-wise (never mid-UTF-8), separated by one blank
/// line, and each role carries its style — user entries bold under
/// `›`, assistant text plain, terminal statuses dimmed. The tool-card
/// ticket plugs its `Entry` variants in here.
fn display_lines(entries: &[Entry], width: usize) -> Vec<(String, Style)> {
    let mut lines = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            lines.push((String::new(), Style::default()));
        }
        let blocks: Vec<(String, Style)> = match entry {
            Entry::Task(text) | Entry::Steering(text) => vec![(
                format!("{INPUT_PROMPT}{text}"),
                Style::default().add_modifier(Modifier::BOLD),
            )],
            Entry::Text(text) => vec![(text.clone(), Style::default())],
            Entry::Tool(card) => tool_card_lines(card),
            Entry::Ask(card) => ask_card_lines(card),
            Entry::Status(text) => vec![(text.clone(), Style::default().fg(RtColor::DarkGray))],
        };
        for (text, style) in blocks {
            for line in wrap(&text, width) {
                lines.push((line, style));
            }
        }
    }
    lines
}

/// The pinned plan block's display rows (Spec R08-T5, us-14): a
/// `plan` header, then one row per todo — `✓` done, `▸` in progress,
/// `○` pending — and a trailing blank separating the block from the
/// scrollback. More rows than `cap` folds the tail into a `… N more`
/// note, so a long plan can never crowd the conversation out of the
/// panel; below a useful minimum (header + one todo + separator) the
/// rows are better spent on scrollback and the block sits out.
fn plan_lines(plan: &[PlanTodo], cap: usize) -> Vec<(String, Style)> {
    if plan.is_empty() || cap < 3 {
        return Vec::new();
    }
    let mut lines = vec![(
        String::from(" plan"),
        Style::default().fg(RtColor::DarkGray),
    )];
    // `cap - 2` todo rows at most — one for the header, one for the
    // separator/overflow row.
    let visible = (cap - 2).min(plan.len());
    for todo in &plan[..visible] {
        let (mark, style) = match todo.status {
            PlanTodoStatus::Done => ("✓", Style::default().fg(RtColor::DarkGray)),
            PlanTodoStatus::InProgress => ("▸", Style::default().add_modifier(Modifier::BOLD)),
            PlanTodoStatus::Pending => ("○", Style::default()),
        };
        lines.push((format!("{mark} {}", todo.content), style));
    }
    if plan.len() > visible {
        // The row this note replaces counted as hidden too.
        let hidden = plan.len() - visible + 1;
        lines.pop();
        lines.push((
            format!("    … {hidden} more"),
            Style::default().fg(RtColor::DarkGray),
        ));
    }
    lines.push((String::new(), Style::default()));
    lines
}

/// A tool card's display blocks (Spec R06 us-5): a status-marked
/// header — `✓` done, `…` running, `?` awaiting approval, `✗` denied,
/// `!` failed — carrying `name summary` inline; a multi-line summary
/// (`edit_files`' diff, us-22) instead explodes into one styled row
/// per line under a name-only header. The detail (result preview,
/// error or denial text) follows indented and dimmed when the
/// update carried one.
fn tool_card_lines(card: &ToolCard) -> Vec<(String, Style)> {
    let (mark, style) = match card.status {
        CardStatus::AwaitingApproval => (
            "?",
            Style::default()
                .fg(RtColor::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        CardStatus::Running => ("…", Style::default()),
        CardStatus::Done => ("✓", Style::default().fg(RtColor::Green)),
        CardStatus::Denied => ("✗", Style::default().fg(RtColor::DarkGray)),
        CardStatus::Failed => ("!", Style::default().fg(RtColor::Red)),
    };
    let mut lines = Vec::new();
    if card.summary.contains('\n') {
        // A multi-line summary — `edit_files`' unified diff
        // (Spec R06 us-22): the header carries the name alone and
        // each diff line paints its own row so `+`/`-` color as
        // additions/deletions instead of wrapping as one blob.
        lines.push((format!("{mark} {}", card.name), style));
        lines.extend(
            card.summary
                .lines()
                .map(|line| (format!("  {line}"), diff_line_style(line))),
        );
    } else {
        let summary = if card.summary.is_empty() {
            card.name.clone()
        } else {
            format!("{} {}", card.name, card.summary)
        };
        lines.push((format!("{mark} {summary}"), style));
    }
    if let Some(detail) = &card.detail {
        lines.push((
            format!("    {detail}"),
            Style::default().fg(RtColor::DarkGray),
        ));
    }
    lines
}

/// The ask card's display blocks (us-10): while it waits, `?` marks
/// the question and each choice lists beneath — `▸` on the
/// highlighted one; answered, `✓` marks the question and the answer
/// indents dimmed like a tool result's detail.
fn ask_card_lines(card: &AskCard) -> Vec<(String, Style)> {
    match &card.answer {
        Some(answer) => vec![
            (
                format!("✓ {}", card.question),
                Style::default().fg(RtColor::Green),
            ),
            (
                format!("    {answer}"),
                Style::default().fg(RtColor::DarkGray),
            ),
        ],
        None => {
            let mut lines = vec![(
                format!("? {}", card.question),
                Style::default()
                    .fg(RtColor::Yellow)
                    .add_modifier(Modifier::BOLD),
            )];
            for (index, choice) in card.choices.iter().enumerate() {
                lines.push(if index == card.selected {
                    (
                        format!("    ▸ {choice}"),
                        Style::default().add_modifier(Modifier::BOLD),
                    )
                } else {
                    (format!("      {choice}"), Style::default())
                });
            }
            lines
        }
    }
}

/// Per-line styling for a unified-diff summary: `+` green, `-` red,
/// `@@` cyan, the `\ No newline` marker dimmed. `---`/`+++` file
/// headers stay dim rather than deletion-red/addition-green —
/// they label files, not churn.
fn diff_line_style(line: &str) -> Style {
    if line.starts_with("---") || line.starts_with("+++") {
        Style::default().fg(RtColor::DarkGray)
    } else if line.starts_with('+') {
        Style::default().fg(RtColor::Green)
    } else if line.starts_with('-') {
        Style::default().fg(RtColor::Red)
    } else if line.starts_with('@') {
        Style::default().fg(RtColor::Cyan)
    } else if line.starts_with('\\') {
        Style::default().fg(RtColor::DarkGray)
    } else {
        Style::default()
    }
}

/// Char-wrap `text` to `width` columns honoring embedded newlines;
/// empty input still yields one (empty) line.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for logical in text.split('\n') {
        let mut line = String::new();
        let mut columns = 0;
        for c in logical.chars() {
            if columns == width {
                lines.push(std::mem::take(&mut line));
                columns = 0;
            }
            line.push(c);
            columns += 1;
        }
        lines.push(line);
    }
    lines
}

/// The trailing `max` characters of `text` — the input row's view of
/// a buffer longer than its width (the tail is the end being edited).
fn tail(text: &str, max: usize) -> &str {
    let count = text.chars().count();
    if count <= max {
        return text;
    }
    text.char_indices()
        .nth(count - max)
        .map(|(index, _)| &text[index..])
        .unwrap_or("")
}

/// `paint_session`'s row-blanking primitive, duplicated locally —
/// `render` keeps it private; a `pub` share would be one more
/// cross-module surface for a three-line helper.
fn reset_span(buf: &mut Buffer, y: u16, xs: std::ops::Range<u16>) {
    for x in xs {
        buf[(x, y)].reset();
    }
}

#[cfg(test)]
mod tests {
    //! The paint asserted on a ratatui `Buffer` — the same state the
    //! `ait-app` tests drive, painted the way `render`'s tests assert.

    use super::*;
    use ait_agent::{AgentEvent, AgentSession};
    use ait_app::config::NoSecrets;

    /// A panel over a session whose worker resolves nothing (default
    /// `Settings` hold no model) — the channel is real, the model is
    /// never reached.
    fn panel() -> AgentPanel {
        AgentPanel::with_session(AgentSession::with_secret_store(NoSecrets))
    }

    /// A staged `ToolCall` for the card tests.
    fn call(id: &str, name: &str) -> ait_agent::ToolCall {
        ait_agent::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: String::new(),
        }
    }

    /// A staged `AskRequest` for the ask-card tests.
    fn ask(id: &str, question: &str, choices: &[&str]) -> AgentEvent {
        AgentEvent::AskRequest {
            call: ait_agent::ToolCall {
                id: id.into(),
                name: "ask_user_question".into(),
                arguments: String::new(),
            },
            question: question.into(),
            choices: choices.iter().map(|c| c.to_string()).collect(),
        }
    }

    /// `wrap` honors embedded newlines and hard-wraps long runs —
    /// the conversation's display-line math.
    #[test]
    fn wrap_splits_on_newlines_and_width() {
        assert_eq!(wrap("ab\ncd", 5), vec!["ab", "cd"]);
        assert_eq!(wrap("abcd", 2), vec!["ab", "cd"]);
        assert_eq!(wrap("", 2), vec![""]);
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
    }

    /// Paint on a small rect: the `│` separator, the title row
    /// (reversed while focused), the conversation in between and the
    /// input line at the bottom.
    #[test]
    fn paint_lays_out_separator_title_conversation_and_input() {
        let mut panel = panel();
        panel.apply(AgentEvent::TextDelta("reply".into()));
        panel.apply(AgentEvent::Done);
        panel.push_input('q');

        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 6));
        let area = Rect::new(10, 0, 10, 6);
        paint_agent_panel(&mut buf, area, &panel);

        // The separator fills the shared edge top to bottom.
        for y in 0..6 {
            assert_eq!(buf[(10, y)].symbol(), "│");
        }
        // Title text on row 0, reversed while focused.
        let title: String = (11..17).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(title, " agent");
        assert!(buf[(11, 0)].modifier.contains(Modifier::REVERSED));
        // The conversation fills its three rows: the reply, the
        // entry-separating blank and the `done` status.
        let row1: String = (11..20).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        assert_eq!(row1.trim_end(), "reply");
        let row3: String = (11..20).map(|x| buf[(x, 3)].symbol().to_string()).collect();
        assert_eq!(row3.trim_end(), "done");
        // The input line's `› ` prompt and text on the last row.
        let input_row: String = (11..14).map(|x| buf[(x, 5)].symbol().to_string()).collect();
        assert_eq!(input_row, "› q");
        // The cursor sits after the input text.
        assert_eq!(panel_cursor_position(&panel, area), (14, 5));
    }

    /// An unfocused panel paints its title row plain — the open-vs-
    /// focused distinction the ticket's render criterion names.
    #[test]
    fn unfocused_panel_paints_a_plain_title() {
        let mut panel = panel();
        panel.escape(); // idle → unfocused, still open

        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 4));
        paint_agent_panel(&mut buf, Rect::new(0, 0, 12, 4), &panel);

        assert!(!buf[(1, 0)].modifier.contains(Modifier::REVERSED));
    }

    /// More lines than the view fits scroll the window: pinned shows
    /// the tail; a wheel-up holds the user's place while appends keep
    /// landing below; a wheel-down to the bottom re-pins.
    #[test]
    fn conversation_scrolls_and_re_pins() {
        let mut panel = panel();
        for n in 0..10 {
            panel.apply(AgentEvent::TextDelta(format!("line{n}\n")));
        }

        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 5));
        let area = Rect::new(0, 0, 12, 5); // 3 conversation rows
        paint_agent_panel(&mut buf, area, &panel);
        assert!(panel.is_pinned());
        // Rows 1..3 show the tail: lines 8, 9 and the trailing blank
        // of "line9\n".
        let tail_row: String = (1..6).map(|x| buf[(x, 2)].symbol().to_string()).collect();
        assert_eq!(tail_row.trim_end(), "line9");

        panel.scroll_up(2);
        paint_agent_panel(&mut buf, area, &panel);
        assert!(!panel.is_pinned());
        let top_row: String = (1..6).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        assert_eq!(top_row.trim_end(), "line6", "scrolled two lines up");

        // Appends can't shift a scrolled view: the same lines stay.
        panel.apply(AgentEvent::TextDelta("more\n".into()));
        paint_agent_panel(&mut buf, area, &panel);
        let same_top: String = (1..6).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        assert_eq!(same_top.trim_end(), "line6");

        panel.scroll_down(10);
        paint_agent_panel(&mut buf, area, &panel);
        assert!(panel.is_pinned(), "landing on the bottom re-pins");
    }

    /// Spec R08 us-13–15: a `PlanUpdated` pins the TODO block under
    /// the title — one styled row per todo — the call's ordinary
    /// card still sits in the scrollback, and the NEXT snapshot
    /// replaces the block's contents in place (never appends).
    #[test]
    fn plan_updates_pin_a_block_that_replaces_in_place() {
        use ait_agent::{PlanTodo, PlanTodoStatus};
        let todos = |items: &[(&str, PlanTodoStatus)]| {
            items
                .iter()
                .map(|(content, status)| PlanTodo {
                    content: (*content).into(),
                    status: *status,
                })
                .collect::<Vec<_>>()
        };

        let mut panel = panel();
        panel.apply(AgentEvent::ToolCall {
            call: call("call_1", "update_plan"),
            summary: "3 todos".into(),
        });
        panel.apply(AgentEvent::PlanUpdated {
            todos: todos(&[
                ("reproduce", PlanTodoStatus::Done),
                ("fix", PlanTodoStatus::InProgress),
                ("verify", PlanTodoStatus::Pending),
            ]),
        });
        panel.apply(AgentEvent::ToolUpdate {
            call_id: "call_1".into(),
            status: ait_agent::ToolStatus::Done,
            detail: Some("plan updated — 3 todos".into()),
            result: Some("plan updated — 3 todos".into()),
        });

        // The call's ordinary card landed in the scrollback like
        // every other tool call.
        assert_eq!(
            panel.entries(),
            &[Entry::Tool(ToolCard {
                id: "call_1".into(),
                name: "update_plan".into(),
                summary: "3 todos".into(),
                status: CardStatus::Done,
                detail: Some("plan updated — 3 todos".into()),
            })]
        );

        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 12));
        let area = Rect::new(0, 0, 30, 12);
        paint_agent_panel(&mut buf, area, &panel);
        // Column 0 is the `│` separator — content starts at 1.
        fn row(buf: &Buffer, y: u16) -> String {
            (1..30).map(|x| buf[(x, y)].symbol().to_string()).collect()
        }
        // Row 0 is the title; the pinned block owns the next rows.
        assert_eq!(row(&buf, 1).trim_end(), " plan");
        assert_eq!(row(&buf, 2).trim_end(), "✓ reproduce");
        assert_eq!(row(&buf, 3).trim_end(), "▸ fix");
        assert_eq!(row(&buf, 4).trim_end(), "○ verify");
        // …and the scrollback conversation begins beneath it.
        assert_eq!(row(&buf, 5).trim_end(), "");
        assert_eq!(row(&buf, 6).trim_end(), "✓ update_plan 3 todos");

        // A second call replaces the block's contents — nothing of
        // the first list survives.
        panel.apply(AgentEvent::ToolCall {
            call: call("call_2", "update_plan"),
            summary: "1 todo".into(),
        });
        panel.apply(AgentEvent::PlanUpdated {
            todos: todos(&[("ship it", PlanTodoStatus::InProgress)]),
        });
        paint_agent_panel(&mut buf, area, &panel);
        assert_eq!(row(&buf, 2).trim_end(), "▸ ship it");
        assert_eq!(row(&buf, 3).trim_end(), "", "the old rows cleared");
        assert!(panel.plan().iter().all(|todo| todo.content != "verify"));
    }

    /// An empty snapshot clears the block — the newest plan IS
    /// "no plan".
    #[test]
    fn an_empty_plan_snapshot_clears_the_block() {
        let mut panel = panel();
        panel.apply(AgentEvent::PlanUpdated {
            todos: vec![ait_agent::PlanTodo {
                content: "step".into(),
                status: ait_agent::PlanTodoStatus::Pending,
            }],
        });
        assert_eq!(panel.plan().len(), 1);
        panel.apply(AgentEvent::PlanUpdated { todos: vec![] });
        assert!(panel.plan().is_empty());

        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 8));
        paint_agent_panel(&mut buf, Rect::new(0, 0, 20, 8), &panel);
        let row: String = (1..20).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        assert_eq!(row.trim_end(), "", "no block under the title");
    }

    /// The ask card paints question and choices in the conversation —
    /// the highlighted option marked — plus its prompt row above the
    /// input line; once answered it renders `✓` + the answer.
    #[test]
    fn paint_shows_the_ask_card_and_its_prompt_row() {
        let mut panel = panel();
        panel.apply(ask("call_1", "Which approach?", &["rewrite", "patch"]));

        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 8));
        paint_agent_panel(&mut buf, Rect::new(0, 0, 60, 8), &panel);

        let row =
            |y: u16| -> String { (1..60).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        assert_eq!(row(1).trim_end(), "? Which approach?");
        assert_eq!(
            row(2).trim_end(),
            "    ▸ rewrite",
            "the first option is selected"
        );
        assert_eq!(row(3).trim_end(), "      patch");
        // The prompt row owns the line above the input row.
        assert!(row(6).contains("enter"), "{:?}", row(6));
        assert!(row(6).contains("choose"), "{:?}", row(6));

        // Answered: the card transitions to `✓` + the answer text.
        let mut toast = None;
        panel.submit_answer(&mut toast);
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 8));
        paint_agent_panel(&mut buf, Rect::new(0, 0, 60, 8), &panel);
        let row =
            |y: u16| -> String { (1..60).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        assert_eq!(row(1).trim_end(), "✓ Which approach?");
        assert_eq!(row(2).trim(), "rewrite");
        assert!(!row(6).contains("choose"), "the prompt row released");
    }

    /// The approve card paints its row above the input line while the
    /// gate waits — and the tool card renders mark + name + summary.
    #[test]
    fn paint_shows_tool_cards_and_the_approve_row() {
        let mut panel = panel();
        panel.apply(AgentEvent::ApprovalRequest {
            call: call("call_1", "read_files"),
            summary: "note.txt".into(),
        });

        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 7));
        paint_agent_panel(&mut buf, Rect::new(0, 0, 60, 7), &panel);

        // The tool card on the first conversation row.
        let card_row: String = (1..25).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        assert_eq!(card_row.trim_end(), "? read_files note.txt");
        // The approve card on the row above the input line.
        let approve_row: String = (1..60).map(|x| buf[(x, 5)].symbol().to_string()).collect();
        assert!(
            approve_row.contains("approve read_files"),
            "{approve_row:?}"
        );
        assert!(approve_row.contains("d: deny"), "{approve_row:?}");
        // The input line still owns the last row.
        let input_row: String = (1..3).map(|x| buf[(x, 6)].symbol().to_string()).collect();
        assert_eq!(input_row, "› ");

        // Once resolved the card transitions and the row returns.
        panel.apply(AgentEvent::ToolUpdate {
            call_id: "call_1".into(),
            status: ait_agent::ToolStatus::Done,
            detail: Some("note.txt: contents".into()),
            result: Some("note.txt: contents".into()),
        });
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 7));
        paint_agent_panel(&mut buf, Rect::new(0, 0, 60, 7), &panel);
        let card_row: String = (1..25).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        assert_eq!(card_row.trim_end(), "✓ read_files note.txt");
        let detail_row: String = (1..25).map(|x| buf[(x, 2)].symbol().to_string()).collect();
        assert_eq!(detail_row.trim(), "note.txt: contents");
        let input_row_above: String = (1..20).map(|x| buf[(x, 5)].symbol().to_string()).collect();
        assert!(
            !input_row_above.contains("approve"),
            "the row released: {input_row_above:?}"
        );
    }

    /// An `edit_files` approve card renders its multi-line unified
    /// diff one styled row per line — `+` green, `-` red, `@@` cyan,
    /// file headers dim — under a name-only header (us-22).
    #[test]
    fn edit_files_card_renders_its_diff_per_line() {
        let summary = "--- a/note.txt\n+++ b/note.txt\n@@ -1,2 +1,2 @@\n old\n-gone\n+new\n\\ No newline at end of file";
        let card = ToolCard {
            id: "call_1".into(),
            name: "edit_files".into(),
            summary: summary.into(),
            status: CardStatus::AwaitingApproval,
            detail: None,
        };
        let lines = tool_card_lines(&card);
        assert_eq!(lines[0].0, "? edit_files");
        // Each diff line is its own display row with its own style.
        let rows: Vec<&str> = lines[1..].iter().map(|(t, _)| t.trim_start()).collect();
        assert_eq!(
            rows,
            vec![
                "--- a/note.txt",
                "+++ b/note.txt",
                "@@ -1,2 +1,2 @@",
                "old",
                "-gone",
                "+new",
                "\\ No newline at end of file"
            ]
        );
        assert_eq!(
            lines[3].1,
            Style::default().fg(RtColor::Cyan),
            "the @@ header"
        );
        assert_eq!(lines[5].1, Style::default().fg(RtColor::Red), "a - line");
        assert_eq!(lines[6].1, Style::default().fg(RtColor::Green), "a + line");
        assert_eq!(
            lines[1].1,
            Style::default().fg(RtColor::DarkGray),
            "--- stays dim, not deletion-red"
        );

        // …and the painted card shows the diff rows verbatim.
        let mut panel = panel();
        panel.apply(AgentEvent::ApprovalRequest {
            call: call("call_1", "edit_files"),
            summary: summary.into(),
        });
        // Twelve rows fit title + 8 card lines + approve row + input.
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 12));
        paint_agent_panel(&mut buf, Rect::new(0, 0, 40, 12), &panel);
        let row: String = (1..40).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        assert_eq!(row.trim_end(), "? edit_files");
        let row: String = (1..40).map(|x| buf[(x, 2)].symbol().to_string()).collect();
        assert_eq!(row.trim_end(), "  --- a/note.txt");
        let row: String = (1..40).map(|x| buf[(x, 7)].symbol().to_string()).collect();
        assert_eq!(row.trim_end(), "  +new");
        assert_eq!(buf[(3, 7)].fg, RtColor::Green);
    }
}
