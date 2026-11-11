//! `dispatch` — the frontend-neutral half of each frontend's input
//! dispatch (Spec R13 §5.12 / ADR-0016): the pieces the TUI's
//! `handle_event` and the GUI's `dispatch_key` carried as separate
//! copies — overlay routing for the pickers, the pass-through tail
//! (focused panel → in-flight suggestion keys → `# `/classifier
//! triggers → PTY), the notification-event fold, and the tab/store
//! lifecycle pairs every action arm performs.
//!
//! Frontend-specific seams stay frontend: native event translation,
//! the GUI's find bar and both clipboards, the TUI's `last_exit`
//! capture — the fns here take plain `ait_app` state so either
//! frontend drives them (CODING_STANDARDS.md §9).

use std::time::Instant;

use ait_agent::CommandSuggester;
use ait_config::Settings;
use ait_term::{Key, KeyEvent, Keypad, LiveEvent, SessionEvent, Size};

use crate::conversations::{self, ConversationPicker};
use crate::profile::{self, ProfilePicker};
use crate::skills::LiveSkills;
use crate::status::{notification_text, Toast};
use crate::store::StoreHandle;
use crate::suggest::{self, SuggestionPin};
use crate::tabs::{TabEvent, TabManager};

/// `Alt+T`'s two steps (and ctl `open_tab`'s follow-up): spawn the
/// tab, then register its `terminal_sessions` row. A spawn failure
/// toasts rather than taking the session down.
pub fn new_tab(tabs: &mut TabManager, store: &mut StoreHandle, toast: &mut Option<Toast>) {
    if let Err(error) = tabs.new_tab() {
        *toast = Some(Toast::new(
            format!("new tab failed: {error}"),
            Instant::now(),
        ));
    } else {
        register_active_session(tabs, store, toast);
    }
}

/// The `terminal_sessions` row the (new or first) active tab
/// registers (Spec R08) — `Alt+T`, ctl's `open_tab` and the boot's
/// first tab all land here. A disabled store or failed insert
/// leaves the tab unpersisted.
pub fn register_active_session(
    tabs: &mut TabManager,
    store: &mut StoreHandle,
    toast: &mut Option<Toast>,
) {
    if let Some(tab) = tabs.active_mut() {
        if let Some(id) = store.session_started(toast, tab.title(), tab.cwd()) {
            tab.set_store_session(id);
        }
    }
}

/// `Alt+W`'s bookkeeping plus the close itself: `ended_at` on the
/// dying tab's `terminal_sessions` row, `closed` on its bound
/// conversation — then `close_active` drops it. `false` means the
/// tab set is empty; the caller exits (standard 6 — index only
/// through `active_mut`).
pub fn close_active_tab(
    tabs: &mut TabManager,
    store: &mut StoreHandle,
    toast: &mut Option<Toast>,
) -> bool {
    if let Some(id) = tabs.active().and_then(|tab| tab.store_session()) {
        store.session_ended(toast, id);
    }
    if let Some(id) = tabs.active().and_then(|tab| tab.conversation()) {
        store.conversation_closed(toast, id);
    }
    tabs.close_active()
}

/// `Alt+A` (Spec R06 §5.6): toggle the active tab's agent panel —
/// open focuses, a second press while focused closes — then
/// re-derive THIS tab's winsize through the frontend's
/// `session_size` (panels are per-tab; only this tab's PTY
/// resizes).
pub fn toggle_agent_panel(tabs: &mut TabManager, session_size: impl Fn(Size, bool) -> Size) {
    let Some(tab) = tabs.active_mut() else {
        return;
    };
    tab.clear_selection();
    tab.toggle_agent_panel();
    tabs.resize_active(session_size);
}

/// A drained session event's frontend-neutral fold: an OSC 9/777
/// `Notification` becomes the toast — a background tab's prefixed
/// with its `N:title` label for attribution (Spec R02 us-16).
/// `true` when the event was consumed here; `ClipboardWrite` and
/// the rest stay the frontend's.
pub fn notification_toast(event: &TabEvent, tabs: &TabManager, toast: &mut Option<Toast>) -> bool {
    if let LiveEvent::Osc(SessionEvent::Notification { title, message }) = &event.event {
        let source = if event.tab == tabs.active_index() {
            None
        } else {
            tabs.tabs().get(event.tab).map(|tab| tab.label(event.tab))
        };
        *toast = Some(Toast::new(
            notification_text(source.as_deref(), title.as_deref(), message),
            Instant::now(),
        ));
        true
    } else {
        false
    }
}

/// Overlay routing for a pass-through key: an open profile or
/// conversation picker owns it whole. `true` = consumed (the key
/// never reaches the PTY); `false` = no overlay open — the caller
/// routes on (the GUI's find bar next, then the PTY path).
#[allow(clippy::too_many_arguments)]
pub fn overlay_key(
    key: &KeyEvent,
    picker: &mut Option<ProfilePicker>,
    conversations: &mut Option<ConversationPicker>,
    tabs: &mut TabManager,
    store: &mut StoreHandle,
    profile: &mut String,
    toast: &mut Option<Toast>,
    session_size: impl Fn(Size, bool) -> Size,
) -> bool {
    if picker.is_some() {
        profile::picker_key(key, picker, profile, toast);
        return true;
    }
    if conversations.is_some() {
        conversations::picker_key(key, conversations, tabs, store, toast, session_size);
        return true;
    }
    false
}

/// The pass-through tail after the overlays (and, in the GUI, the
/// find bar) routed: a focused agent panel owns the key, an
/// in-flight suggestion swallows Enter/cancels on Esc, the
/// `# `+Enter and classifier triggers arm the suggester — and
/// anything left encodes through the input shadow to the PTY,
/// byte-for-byte the path both frontends ran.
#[allow(clippy::too_many_arguments)]
pub fn passthrough_key(
    key: Option<KeyEvent>,
    tabs: &mut TabManager,
    suggester: &CommandSuggester,
    suggestion: &mut SuggestionPin,
    settings: &Settings,
    profile: &str,
    skills: &mut LiveSkills,
    toast: &mut Option<Toast>,
) {
    let index = tabs.active_index();
    let suggestion_here = suggestion.is_live_for(tabs, index);
    let Some(tab) = tabs.active_mut() else {
        return;
    };
    tab.clear_selection();
    if tab.agent_focused() {
        if let Some(key) = key {
            tab.agent_key(&key, settings, profile, skills, toast);
        }
        return;
    }
    tab.snap_to_bottom();
    if suggestion_here {
        match key.map(|key| key.key) {
            Some(Key::Escape) if key.is_some_and(|key| key.modifiers.is_empty()) => {
                suggestion.cancel(suggester, tab);
                return;
            }
            Some(Key::Enter) | Some(Key::Keypad(Keypad::Enter)) => {
                return;
            }
            _ => {}
        }
    }
    if tab.exit_status().is_none() {
        if let Some(key_event) = key {
            // The `# `+Enter and classifier triggers replay verified
            // input against the grid echo before arming a request —
            // the suggestion pin suppresses double-fires while one
            // is live.
            if suggestion.is_none() && suggest::is_submission(&key_event) {
                if let Some(request) = suggest::request_for(tab, settings, profile) {
                    suggestion.fire(suggester, tab.id, request, toast);
                } else if let Some((line, request)) =
                    suggest::classified_request(tab, settings, profile)
                {
                    if suggestion.fire(suggester, tab.id, request, toast) {
                        suggest::rewrite_as_comment(tab, &line);
                        return;
                    }
                }
            }
            tab.shadow.update(&key_event);
            let _ = tab.session.send_key(key_event);
        }
    }
}
