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

/// The session-model context a key event may touch — the tab set,
/// the overlays, the store and toast slots, the profile, and the
/// suggestion machinery — bundled once rather than trailed through
/// every dispatch fn as half a dozen positional `&mut`s. Frontends
/// build one per event from their `Shared`/`AppState`; a field a
/// stage doesn't need simply goes unread.
pub struct KeyCtx<'a> {
    /// The armed-execution-profile picker — open, it owns the key.
    pub picker: &'a mut Option<ProfilePicker>,
    /// The conversation picker — ditto.
    pub conversations: &'a mut Option<ConversationPicker>,
    /// The session model.
    pub tabs: &'a mut TabManager,
    /// The persistence facade.
    pub store: &'a mut StoreHandle,
    /// The active profile's name — pickers rewrite it.
    pub profile: &'a mut String,
    /// The toast slot every failure path writes.
    pub toast: &'a mut Option<Toast>,
    /// The NL→command suggester.
    pub suggester: &'a CommandSuggester,
    /// The pinned in-flight suggestion.
    pub suggestion: &'a mut SuggestionPin,
    /// The live settings (classifier mode, model roles).
    pub settings: &'a Settings,
    /// The skills watcher — `agent_key` materializes prompts from it.
    pub skills: &'a mut LiveSkills,
}

/// Overlay routing for a pass-through key: an open profile or
/// conversation picker owns it whole. `true` = consumed (the key
/// never reaches the PTY); `false` = no overlay open — the caller
/// routes on (the GUI's find bar next, then the PTY path).
pub fn overlay_key(
    key: &KeyEvent,
    ctx: &mut KeyCtx<'_>,
    session_size: impl Fn(Size, bool) -> Size,
) -> bool {
    if ctx.picker.is_some() {
        profile::picker_key(key, ctx.picker, ctx.profile, ctx.toast);
        return true;
    }
    if ctx.conversations.is_some() {
        conversations::picker_key(
            key,
            ctx.conversations,
            ctx.tabs,
            ctx.store,
            ctx.toast,
            session_size,
        );
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
pub fn passthrough_key(key: Option<KeyEvent>, ctx: &mut KeyCtx<'_>) {
    let index = ctx.tabs.active_index();
    let suggestion_here = ctx.suggestion.is_live_for(ctx.tabs, index);
    let Some(tab) = ctx.tabs.active_mut() else {
        return;
    };
    tab.clear_selection();
    if tab.agent_focused() {
        if let Some(key) = key {
            tab.agent_key(&key, ctx.settings, ctx.profile, ctx.skills, ctx.toast);
        }
        return;
    }
    tab.snap_to_bottom();
    if suggestion_here {
        match key.map(|key| key.key) {
            Some(Key::Escape) if key.is_some_and(|key| key.modifiers.is_empty()) => {
                ctx.suggestion.cancel(ctx.suggester, tab);
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
            if ctx.suggestion.is_none() && suggest::is_submission(&key_event) {
                if let Some(request) = suggest::request_for(tab, ctx.settings, ctx.profile) {
                    ctx.suggestion
                        .fire(ctx.suggester, tab.id, request, ctx.toast);
                } else if let Some((line, request)) =
                    suggest::classified_request(tab, ctx.settings, ctx.profile)
                {
                    if ctx
                        .suggestion
                        .fire(ctx.suggester, tab.id, request, ctx.toast)
                    {
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
