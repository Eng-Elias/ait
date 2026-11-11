//! The `Alt+R` conversation picker (Spec R08 §5.8, ticket R08-T6,
//! user stories 5–12): the store's `conversations` listed newest-
//! first — title, age and item count — `▸`-highlighted like the
//! `Alt+P` switcher. Enter resumes the highlighted row into the
//! active tab's panel; `Ctrl+D` deletes it behind a one-key confirm;
//! Esc (or a second `Alt+R`) closes.
//!
//! Only bookkeeping lives here: the display rows, the wrap-around
//! highlight, the delete-confirm arm and the age column's bucketing.
//! The store reads, the panel rebuild and the paint are `app`'s
//! seams (`store.rs`, `agent_panel.rs`, `render.rs` — standard 9).

use std::time::SystemTime;

use ait_store::ListedConversation;

/// One picker row — a stored conversation's display form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationRow {
    /// The `conversations` row id resume and delete address.
    pub id: i64,
    /// The row's title — the first task's text, truncated at write
    /// time (us-5's "titled from the first task").
    pub title: String,
    /// `updated_at`'s age bucketed at open (`"now"`, `"5m"`, `"3h"`,
    /// `"12d"`) — the list is a snapshot; live-ticking ages are
    /// needless.
    pub age: String,
    /// How many `transcript_items` the conversation holds.
    pub items: i64,
}

/// The open picker (Spec R08 us-5): stored rows newest-first, the
/// highlight wrapping on ↑/↓, and `Ctrl+D`'s one-key confirm — the
/// first press arms the highlighted row, the second deletes it and
/// any other key disarms (us-11: delete is one gesture but never an
/// accident).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationPicker {
    /// The display rows, newest first.
    rows: Vec<ConversationRow>,
    /// The highlighted row's index into `rows`.
    selected: usize,
    /// `Ctrl+D`'s confirm arm — `true` between the first and second
    /// press on the same row.
    confirming: bool,
}

impl ConversationPicker {
    /// Rows for the store's listing: every conversation newest-first
    /// with its item count, the age bucketed against `now`.
    pub fn new(listed: &[ListedConversation], now: SystemTime) -> Self {
        Self {
            rows: listed
                .iter()
                .map(|row| ConversationRow {
                    id: row.conversation.id,
                    title: row.conversation.title.clone(),
                    age: age_label(row.conversation.updated_at, now),
                    items: row.items,
                })
                .collect(),
            selected: 0,
            confirming: false,
        }
    }

    /// The listed rows, newest first.
    pub fn rows(&self) -> &[ConversationRow] {
        &self.rows
    }

    /// The highlighted row's index.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The highlighted row — Enter's resume target, `Ctrl+D`'s
    /// delete target; `None` on an empty picker.
    pub fn selected_row(&self) -> Option<&ConversationRow> {
        self.rows.get(self.selected)
    }

    /// Whether `Ctrl+D` is armed on the highlighted row — the second
    /// press deletes, anything else disarms.
    pub fn confirming(&self) -> bool {
        self.confirming
    }

    /// Arm `Ctrl+D`'s one-key confirm on the highlighted row.
    pub fn arm_delete(&mut self) {
        self.confirming = true;
    }

    /// Drop the confirm arm — any key that isn't `Ctrl+D` does.
    pub fn disarm_delete(&mut self) {
        self.confirming = false;
    }

    /// ↑ — the highlight wraps to the tail from the top (the ask
    /// card's/`Alt+P` picker's wrap rule).
    pub fn prev(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = self.selected.checked_sub(1).unwrap_or(self.rows.len() - 1);
    }

    /// ↓ — the highlight wraps back to the top.
    pub fn next(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.rows.len();
    }

    /// Drop the highlighted row after its delete landed — returns it
    /// for the store call's aftermath; `None` on an empty picker.
    /// The highlight clamps to the new tail; the confirm arm is
    /// spent either way.
    pub fn remove_selected(&mut self) -> Option<ConversationRow> {
        self.confirming = false;
        if self.selected >= self.rows.len() {
            return None;
        }
        let row = self.rows.remove(self.selected);
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
        Some(row)
    }
}

/// The age column's coarse bucketing: `"now"` under a minute,
/// minutes under an hour, hours under two days, days after.
/// `updated_at` is a wall clock, so a future skew renders `now`
/// rather than a nonsense age.
pub fn age_label(then: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(then).unwrap_or_default().as_secs();
    if secs < 60 {
        "now".to_string()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 48 * 3600 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    //! The picker's seams: the store listing → display rows, the
    //! wrap-around highlight, the delete-confirm lifecycle and the
    //! age buckets — all pure state, no store, no TTY.

    use super::*;
    use ait_store::Conversation;
    use std::time::Duration;

    /// A `ListedConversation` fixture: `id`, title and items, aged
    /// `age_secs` before `now`.
    fn listed(id: i64, title: &str, items: i64, age_secs: u64) -> ListedConversation {
        let at = now() - Duration::from_secs(age_secs);
        ListedConversation {
            conversation: Conversation {
                id,
                title: title.to_string(),
                cwd: "/w".to_string(),
                status: "open".to_string(),
                created_at: at,
                updated_at: at,
            },
            items,
        }
    }

    const NOW_SECS: u64 = 1_000_000;

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(NOW_SECS)
    }

    /// The listing folds into display rows newest-first — id, title
    /// and count carried, the age bucketed at open.
    #[test]
    fn the_listing_becomes_display_rows() {
        let rows = vec![
            listed(7, "fix the tests", 12, 120),
            listed(3, "plan the work", 30, 90_000),
        ];
        let picker = ConversationPicker::new(&rows, now());
        assert_eq!(
            picker.rows(),
            &[
                ConversationRow {
                    id: 7,
                    title: "fix the tests".to_string(),
                    age: "2m".to_string(),
                    items: 12,
                },
                ConversationRow {
                    id: 3,
                    title: "plan the work".to_string(),
                    age: "25h".to_string(),
                    items: 30,
                },
            ]
        );
        assert_eq!(picker.selected(), 0, "the newest row highlights");
        assert_eq!(picker.selected_row().unwrap().id, 7);
    }

    /// ↑/↓ wrap the highlight at both ends — the `Alt+P` idiom.
    #[test]
    fn the_highlight_wraps() {
        let rows = vec![listed(1, "a", 1, 0), listed(2, "b", 2, 0)];
        let mut picker = ConversationPicker::new(&rows, now());
        picker.prev();
        assert_eq!(picker.selected(), 1, "↑ past the top wraps to the tail");
        picker.next();
        assert_eq!(picker.selected(), 0, "↓ past the tail wraps to the top");
    }

    /// us-11's one-key confirm: the first `Ctrl+D` arms, a non-`Ctrl+D`
    /// key disarms without deleting, and `remove_selected` drops the
    /// row — the highlight clamping to the new tail and the arm spent.
    #[test]
    fn delete_arms_disarms_and_removes() {
        let rows = vec![
            listed(1, "a", 1, 0),
            listed(2, "b", 2, 0),
            listed(3, "c", 3, 0),
        ];
        let mut picker = ConversationPicker::new(&rows, now());
        picker.next(); // highlight "b"

        picker.arm_delete();
        assert!(picker.confirming());
        picker.disarm_delete();
        assert!(!picker.confirming(), "a stray key cancels the arm");
        assert_eq!(picker.selected_row().unwrap().id, 2, "nothing removed");

        picker.arm_delete();
        let removed = picker.remove_selected().expect("the highlighted row");
        assert_eq!(removed.id, 2);
        assert!(!picker.confirming(), "the arm is spent");
        assert_eq!(
            picker.rows().iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(picker.selected_row().unwrap().id, 3, "clamped to the tail");

        // Removing the last row empties the picker.
        picker.next(); // wraps to row 0 ("a")
        assert_eq!(picker.selected_row().unwrap().id, 1);
        picker.remove_selected();
        picker.remove_selected();
        assert!(picker.rows().is_empty());
        assert!(picker.selected_row().is_none());
        assert!(picker.remove_selected().is_none(), "empty stays empty");
    }

    /// The age column: `now` under a minute, minutes under an hour,
    /// hours under two days, days after — and a future timestamp
    /// (clock skew) reads `now`, never a nonsense age.
    #[test]
    fn age_buckets() {
        let at = |secs_ago: u64| now() - Duration::from_secs(secs_ago);
        assert_eq!(age_label(at(0), now()), "now");
        assert_eq!(age_label(at(59), now()), "now");
        assert_eq!(age_label(at(60), now()), "1m");
        assert_eq!(age_label(at(3599), now()), "59m");
        assert_eq!(age_label(at(3600), now()), "1h");
        assert_eq!(age_label(at(47 * 3600), now()), "47h");
        assert_eq!(age_label(at(48 * 3600), now()), "2d");
        assert_eq!(age_label(at(90 * 86_400), now()), "90d");
        assert_eq!(
            age_label(now() + Duration::from_secs(60), now()),
            "now",
            "clock skew never ages backwards"
        );
    }
}
