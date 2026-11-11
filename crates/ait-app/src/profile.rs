//! The active execution profile (Spec R07 §5.7 / issue #101): the one
//! app-global `String` the `ait` binary owns — initialized to
//! [`DEFAULT_PROFILE`] at launch, switched with `Alt+P`, and NEVER
//! persisted (the file stays user-authored; a forgotten permissive
//! pick cannot survive a restart, per the spec's ephemerality rule).
//!
//! This module holds only the selection's bookkeeping — which names
//! are declared, what the switch chord does with them, the picker's
//! highlight, the status slot's visibility rule, and the reload
//! fail-safe's toast condition. The permission engine and the model
//! roles the selection drives live in `ait-agent`/`ait-config`
//! (standard 9 — the binary stores the selection and passes the name).

use std::time::Instant;

use ait_config::Settings;
use ait_term::{Key, KeyEvent};

use crate::keymap;
use crate::status::Toast;

/// The profile every launch starts on (Spec R07 us-27: the selection
/// is ephemeral — nothing ever writes it back to `settings.toml`).
pub const DEFAULT_PROFILE: &str = "default";

/// What the `Alt+P` chord does with the profiles `settings.toml`
/// declares — the count decides the UX (Spec R07 us-23): a picker
/// over three or more names, a direct cycle between two, a toast
/// when there is nothing to switch to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Switch {
    /// Three or more declared — open a [`ProfilePicker`] over them.
    Picker,
    /// Exactly two declared — arm the other name at once, no picker.
    Cycle(String),
    /// One or none declared — nothing to switch to.
    Unavailable,
}

/// The chord's decision over the declared profile `names` (sorted —
/// `execution_profiles` is a `BTreeMap`) and the currently armed
/// `active` name. A two-profile cycle arms the name that is not the
/// active one; an `active` missing from the list (a reload dropped it)
/// cycles to the first declared name — landing on a real profile is
/// the honest recovery.
pub fn switch(names: &[String], active: &str) -> Switch {
    match names.len() {
        0 | 1 => Switch::Unavailable,
        2 => Switch::Cycle(
            names
                .iter()
                .find(|name| name.as_str() != active)
                .cloned()
                .expect("two distinct names always differ from active"),
        ),
        _ => Switch::Picker,
    }
}

/// The status line's profile segment (Spec R07 us-24): `Some(name)`
/// only when the armed profile is not `default` — `default` hides the
/// slot entirely so the R03 layout is byte-for-byte what it was.
pub fn status_slot(active: &str) -> Option<&str> {
    (active != DEFAULT_PROFILE).then_some(active)
}

/// The `Alt+P` dispatch (Spec R07 us-23): a second press dismisses an
/// open picker; otherwise the declared-profile count decides — a
/// picker over three or more, a direct cycle between two, a toast
/// when there is nothing to switch to.
pub fn switch_profile(
    settings: &Settings,
    picker: &mut Option<ProfilePicker>,
    profile: &mut String,
    toast: &mut Option<Toast>,
) {
    if picker.take().is_some() {
        return;
    }
    let names: Vec<String> = settings.agents.execution_profiles.keys().cloned().collect();
    match switch(&names, profile) {
        Switch::Picker => *picker = Some(ProfilePicker::new(names, profile)),
        Switch::Cycle(name) => activate_profile(profile, name, toast),
        Switch::Unavailable => {
            *toast = Some(Toast::new(
                "no other execution profiles declared",
                Instant::now(),
            ));
        }
    }
}

/// Arm `name` as the active profile — a real switch confirms with
/// the spec's toast; arming the already-active name is a no-op
/// (us-23's picker-Enter on the same row stays silent).
pub fn activate_profile(profile: &mut String, name: String, toast: &mut Option<Toast>) {
    if name == *profile {
        return;
    }
    *toast = Some(Toast::new(
        format!("execution profile switched to \"{name}\""),
        Instant::now(),
    ));
    *profile = name;
}

/// One key while the picker is open: ↑/↓ wrap the highlight, Enter
/// (either key — `keymap::normalize` folds the keypad's in) arms the
/// highlighted name, plain Esc dismisses, everything else swallows.
pub fn picker_key(
    key: &KeyEvent,
    picker: &mut Option<ProfilePicker>,
    profile: &mut String,
    toast: &mut Option<Toast>,
) {
    let Some(p) = picker.as_mut() else {
        return;
    };
    if keymap::normalize(key.key) == Key::Enter {
        let name = p.selected_name().to_string();
        *picker = None;
        activate_profile(profile, name, toast);
        return;
    }
    match key.key {
        Key::Up => p.prev(),
        Key::Down => p.next(),
        Key::Escape if key.modifiers.is_empty() => {
            *picker = None;
        }
        _ => {}
    }
}

/// The reload fail-safe's toast condition (Spec R07 us-28): the armed
/// name resolves to nothing under the freshly adopted `new` settings,
/// so every subsequent tool call asks — the Round 6 interim gate the
/// worker evaluates a missing profile as. `old` is the previous
/// last-good: a name it declared that `new` no longer does is a real
/// deletion and toasts — `default` included, since an entirely
/// absent `execution_profiles` table cannot tell "deleted" from
/// "never declared" on its own. When `old` never declared the name
/// either, an empty new table only counts against a non-`default`
/// armed name: a user who never declared a profile sits on the same
/// fail-safe they always had, and toasting it on every save would
/// cry wolf.
pub fn active_missing(old: &Settings, new: &Settings, active: &str) -> bool {
    let new_profiles = &new.agents.execution_profiles;
    if new_profiles.contains_key(active) {
        return false;
    }
    old.agents.execution_profiles.contains_key(active)
        || !new_profiles.is_empty()
        || active != DEFAULT_PROFILE
}

/// The open switcher (Spec R07 us-23): the declared names and the
/// highlighted index — the ask card's `▸` selection idiom as an
/// app-global list. `Alt+P` opens it over three or more declared
/// profiles, ↑/↓ wrap the highlight, Enter arms the highlighted
/// name, Esc (or a second `Alt+P`) dismisses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfilePicker {
    /// The declared profile names, sorted — the picker's rows.
    names: Vec<String>,
    /// The highlighted row's index into `names`.
    selected: usize,
}

impl ProfilePicker {
    /// A picker over `names` with `active` pre-highlighted (index `0`
    /// when the armed name is not in the list — a vanished profile
    /// still opens on the top row rather than panicking).
    pub fn new(names: Vec<String>, active: &str) -> Self {
        let selected = names.iter().position(|name| name == active).unwrap_or(0);
        Self { names, selected }
    }

    /// The listed names, in order.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// The highlighted row's index.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The highlighted name — Enter's selection.
    pub fn selected_name(&self) -> &str {
        &self.names[self.selected]
    }

    /// ↑ — the highlight wraps to the tail from the top (the ask
    /// card's wrap rule).
    pub fn prev(&mut self) {
        if self.names.is_empty() {
            return;
        }
        self.selected = self.selected.checked_sub(1).unwrap_or(self.names.len() - 1);
    }

    /// ↓ — the highlight wraps back to the top.
    pub fn next(&mut self) {
        if self.names.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.names.len();
    }

    /// Row `index`'s display text — `▸` on the highlight (the ask
    /// card's selection idiom). Both frontends paint this verbatim
    /// (Spec R13 §5.12, CODING_STANDARDS §17).
    pub fn row_text(&self, index: usize) -> String {
        let name = &self.names[index];
        if index == self.selected {
            format!("▸ {name}")
        } else {
            format!("  {name}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// Spec R07 us-23: the chord's three outcomes by declared count —
    /// a picker over 3+, a direct cycle between 2, nothing to switch
    /// to at ≤1.
    #[test]
    fn the_chord_picks_by_profile_count() {
        assert_eq!(
            switch(&names(&[]), DEFAULT_PROFILE),
            Switch::Unavailable,
            "zero declared — nothing exists to switch to"
        );
        assert_eq!(
            switch(&names(&["default"]), DEFAULT_PROFILE),
            Switch::Unavailable,
            "one declared — nowhere to go"
        );
        assert_eq!(
            switch(&names(&["default", "work"]), DEFAULT_PROFILE),
            Switch::Cycle("work".to_string()),
            "two declared cycle straight to the other"
        );
        assert_eq!(
            switch(&names(&["default", "work"]), "work"),
            Switch::Cycle("default".to_string()),
            "the cycle lands on the name that is not armed"
        );
        assert_eq!(
            switch(&names(&["default", "scratch", "work"]), "default"),
            Switch::Picker,
            "three or more open the picker"
        );
    }

    /// A two-profile cycle while the armed name is missing (a reload
    /// deleted it) lands on the first declared name — recovering onto
    /// a real profile rather than toggling between ghosts.
    #[test]
    fn a_two_profile_cycle_recovers_a_missing_active() {
        assert_eq!(
            switch(&names(&["default", "work"]), "gone"),
            Switch::Cycle("default".to_string())
        );
    }

    /// Spec R07 us-24: the slot shows the armed name only when it is
    /// not `default` — `default` keeps the R03 layout untouched.
    #[test]
    fn the_status_slot_hides_default() {
        assert_eq!(status_slot("work"), Some("work"));
        assert_eq!(status_slot(DEFAULT_PROFILE), None);
    }

    /// Spec R07 us-28: a reload that drops or renames the armed
    /// profile toasts the vanished name — but a user who never
    /// declared profiles sits on the same fail-safe silently, and a
    /// still-declared name never toasts.
    #[test]
    fn the_vanished_active_toasts_only_when_a_profile_was_lost() {
        let declared = Settings::from_toml(
            "[agents.execution_profiles.default]\n[agents.execution_profiles.work]\n",
        )
        .unwrap();
        assert!(!active_missing(&declared, &declared, "work"));
        assert!(!active_missing(&declared, &declared, DEFAULT_PROFILE));

        let renamed = Settings::from_toml("[agents.execution_profiles.other]\n").unwrap();
        assert!(
            active_missing(&declared, &renamed, "work"),
            "the armed name is gone while others remain"
        );
        assert!(
            active_missing(&declared, &renamed, DEFAULT_PROFILE),
            "default counts too when profiles exist without it"
        );

        // Zero declared profiles in the NEW file: the OLD file is
        // what tells "the whole table was deleted" from "nothing was
        // ever declared". An armed `default` that WAS declared
        // toasts its deletion; a `default` that never was stays
        // silent — a profile-less user sits on the same
        // ask-everything fail-safe they always had, and toasting on
        // every save would cry wolf. A user-picked name armed
        // against an empty table toasts either way.
        let bare = Settings::default();
        assert!(
            active_missing(&declared, &bare, DEFAULT_PROFILE),
            "a declared default deleted with the whole table toasts"
        );
        assert!(
            !active_missing(&bare, &bare, DEFAULT_PROFILE),
            "a never-declared default is not a deletion"
        );
        assert!(active_missing(&bare, &bare, "work"));
        assert!(active_missing(&declared, &bare, "work"));
    }

    /// The picker opens on the armed name, wraps both directions, and
    /// Enter's `selected_name` tracks the highlight.
    #[test]
    fn the_picker_highlights_and_wraps() {
        let mut picker = ProfilePicker::new(names(&["default", "scratch", "work"]), "work");
        assert_eq!(picker.selected(), 2);
        assert_eq!(picker.selected_name(), "work");

        picker.next();
        assert_eq!(picker.selected(), 0, "↓ past the tail wraps to the top");
        picker.prev();
        assert_eq!(picker.selected(), 2, "↑ past the top wraps to the tail");
        picker.prev();
        assert_eq!(picker.selected_name(), "scratch");

        // An armed name missing from the list opens on the first row
        // rather than indexing past it.
        let picker = ProfilePicker::new(names(&["default", "work"]), "gone");
        assert_eq!(picker.selected(), 0);
    }
}
