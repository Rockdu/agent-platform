//! task22 / AC-8.1 / AC-8.2 / AC-8.4 — host-side notification surface.
//!
//! Subscribes (indirectly, via `terminal_mesh::forward_events_to_webview`)
//! to per-PTY `NeedsAttention` events, deduplicates them at the host level
//! by `(plugin_id, terminal_id, event_kind)` per `docs/specs/terminal-events.md`
//! §"Dedup Arbiter Behavior", and fires:
//! - one native macOS notification per non-deduped event via
//!   `tauri-plugin-notification` (AC-8.1)
//! - one tray-entry record persisted in a bounded ring buffer for the
//!   TrayBottomCenter tray window to render (AC-8.1)
//!
//! Permission flow (AC-8.4): the first event that would fire a notification
//! lazily queries `permission_state()`; if `Prompt`, the service calls
//! `request_permission()` once. The state is cached in an `AtomicU8` so
//! subsequent fires skip the round-trip. When the resolved state is
//! `Denied`, the tray entry is STILL recorded — the tray window
//! continues to drive event surfacing per the AC-8.4 contract.
//!
//! Production wiring uses `RealNotifySink`, which holds the `AppHandle`
//! and calls into `tauri-plugin-notification`. Tests construct
//! `NotificationService::with_sink(...)` and pass a `RecordingNotifySink`
//! / `DenyingNotifySink` so they never invoke the real macOS notification
//! center.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use terminal_mesh_core::{
    dedup_key, AttentionKind, AttentionSeverity, DedupArbiter, TerminalEvent,
    TerminalEventEnvelope,
};

/// Hard cap on retained tray entries. The tray window renders the
/// most-recent slice; older entries are dropped when the ring fills.
pub const MAX_TRAY_ENTRIES: usize = 100;

/// Hard cap on retained papers tray entries. Papers ring is separate
/// from `TrayEntry` (which is terminal-event-shaped) so the daily
/// digest never collides with attention events.
pub const MAX_PAPERS_TRAY_ENTRIES: usize = 50;

/// Cached permission state. Stored as `AtomicU8` so concurrent fires
/// don't race on the lazy first-request flow.
const PERM_UNKNOWN: u8 = 0;
const PERM_GRANTED: u8 = 1;
const PERM_DENIED: u8 = 2;

/// Wire shape for the tray window's "recent events" list. Camel-cased
/// per project convention.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrayEntry {
    pub id: String,
    pub plugin_id: String,
    pub terminal_id: String,
    pub kind_name: String,
    pub severity: String,
    pub summary: String,
    pub fired_at_unix_ms: u128,
    pub suppressed_count: usize,
    /// task23 / AC-3.5: true when the event originated from the
    /// orchestrator's claude PTY (`forward_events_to_webview`
    /// resolves it from `OrchestratorState::snapshot()`). The
    /// frontend renders a 🤖 / "claude" badge on these rows so
    /// orchestrator events are visually distinct from regular tabs.
    pub is_orchestrator: bool,
}

/// Papers digest tray entry — kept distinct from `TrayEntry` because
/// `TrayEntry`'s shape encodes terminal events (terminal_id, severity,
/// suppressed_count) which do not apply to paper cards.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PapersTrayEntry {
    pub arxiv_id: String,
    pub title: String,
    pub abstract_snippet: String,
    pub abs_url: String,
    pub fetched_at: String,
}

/// Wire shape for `notification_get_permission_state`. `unknown` until
/// the first event-driven query; `granted`/`denied` thereafter.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionStateDto {
    Unknown,
    Granted,
    Denied,
}

/// Trait the production sink (`RealNotifySink`) and test sinks
/// implement. Keeps the notification service decoupled from
/// `tauri-plugin-notification` for testability.
pub trait NotifySink: Send + Sync {
    /// Fire a native notification. Implementations should be best-
    /// effort — log on failure, never panic.
    fn fire(&self, title: &str, body: &str);
    /// Query the current OS permission state. Called lazily.
    fn permission_state(&self) -> NotifyPermissionState;
    /// Request permission. Called once on first fire when
    /// `permission_state` returns `Prompt`.
    fn request_permission(&self) -> NotifyPermissionState;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyPermissionState {
    Prompt,
    Granted,
    Denied,
}

/// Tauri-managed notification surface. Internally `Arc<Mutex>`-shared
/// so the bridge / tauri commands / terminal_mesh forwarder all hold
/// the same underlying state via cheap `Clone`.
#[derive(Clone)]
pub struct NotificationService {
    inner: Arc<NotificationInner>,
}

struct NotificationInner {
    arbiter: StdMutex<DedupArbiter>,
    entries: StdMutex<VecDeque<TrayEntry>>,
    papers_entries: StdMutex<VecDeque<PapersTrayEntry>>,
    permission: AtomicU8,
    sink: Arc<dyn NotifySink>,
}

impl NotificationService {
    /// Production constructor — wraps a `RealNotifySink` around the
    /// `AppHandle`. The handle is cheap to clone (it's `Arc`-shared
    /// inside Tauri).
    pub fn with_sink(sink: Arc<dyn NotifySink>) -> Self {
        Self {
            inner: Arc::new(NotificationInner {
                arbiter: StdMutex::new(DedupArbiter::for_production()),
                entries: StdMutex::new(VecDeque::with_capacity(MAX_TRAY_ENTRIES)),
                papers_entries: StdMutex::new(VecDeque::with_capacity(MAX_PAPERS_TRAY_ENTRIES)),
                permission: AtomicU8::new(PERM_UNKNOWN),
                sink,
            }),
        }
    }

    /// Append the daily-digest papers entries to the papers-only ring
    /// buffer and fire one native notification with `title` / `body`.
    /// No-op for the notification if `entries` is empty; the ring still
    /// records every entry passed in (cap enforced).
    ///
    /// Surfaces via the separate `notification_list_recent_papers_tray_entries`
    /// Tauri command so the tray window can render paper cards in a
    /// distinct section.
    pub fn push_papers_digest(
        &self,
        title: String,
        body: String,
        entries: Vec<PapersTrayEntry>,
    ) {
        if entries.is_empty() {
            return;
        }
        {
            let mut guard = self
                .inner
                .papers_entries
                .lock()
                .expect("papers tray entries poisoned");
            for entry in entries {
                guard.push_back(entry);
                while guard.len() > MAX_PAPERS_TRAY_ENTRIES {
                    guard.pop_front();
                }
            }
        }
        // Best-effort native notification. Permission denial is logged
        // by the sink itself; the entry is already in the ring.
        self.inner.sink.fire(&title, &body);
    }

    /// Snapshot of the recent papers tray entries (newest first).
    pub fn list_recent_papers_entries(&self) -> Vec<PapersTrayEntry> {
        let guard = self
            .inner
            .papers_entries
            .lock()
            .expect("papers tray entries poisoned");
        guard.iter().rev().cloned().collect()
    }

    /// Clear the papers tray ring.
    pub fn clear_papers_entries(&self) {
        let mut guard = self
            .inner
            .papers_entries
            .lock()
            .expect("papers tray entries poisoned");
        guard.clear();
    }

    /// Snapshot of the recent tray entries (newest first).
    pub fn list_recent_entries(&self) -> Vec<TrayEntry> {
        let guard = self.inner.entries.lock().expect("notification entries poisoned");
        guard.iter().rev().cloned().collect()
    }

    /// Clear all tray entries (UI "clear all" button).
    pub fn clear_entries(&self) {
        let mut guard = self.inner.entries.lock().expect("notification entries poisoned");
        guard.clear();
    }

    /// Cached permission state. `Unknown` until the first event-
    /// driven `on_needs_attention` runs; `Granted`/`Denied` thereafter.
    pub fn permission_state_dto(&self) -> PermissionStateDto {
        match self.inner.permission.load(Ordering::SeqCst) {
            PERM_GRANTED => PermissionStateDto::Granted,
            PERM_DENIED => PermissionStateDto::Denied,
            _ => PermissionStateDto::Unknown,
        }
    }

    /// Entry point called by `terminal_mesh::forward_events_to_webview`
    /// for every `NeedsAttention` envelope. Pure side-effect free
    /// otherwise (no IO except via the sink + the optional `app.emit`
    /// for the tray refresh notification, which the caller handles).
    ///
    /// Returns `true` if the event fired a native notification + a
    /// fresh tray entry, `false` if it was deduped (suppressed_count
    /// on the existing entry was bumped) or if permission was denied
    /// (tray entry recorded; native fire skipped).
    pub fn on_needs_attention(
        &self,
        envelope: &TerminalEventEnvelope,
        is_orchestrator: bool,
    ) -> NotificationDecision {
        let TerminalEvent::NeedsAttention { ref payload } = envelope.event else {
            return NotificationDecision::SkippedNonAttention;
        };
        let plugin_id = envelope.plugin_id.as_str();
        let kind_str = kind_name_str(&payload.kind);
        let key = dedup_key(plugin_id, envelope.terminal_id, &payload.kind);

        // Dedup arbiter under a single mutex so concurrent fires for
        // the same key don't both pass.
        let decision = {
            let mut arb = self.inner.arbiter.lock().expect("dedup arbiter poisoned");
            arb.try_record(&key, payload.event_id, Instant::now())
        };
        match decision {
            terminal_mesh_core::ArbiterDecision::Suppress { .. } => {
                // Bump suppressed_count on the most-recent matching
                // entry if any (the user has visibility into "X more
                // events of this type were suppressed").
                let mut guard = self.inner.entries.lock().expect("entries poisoned");
                if let Some(e) = guard.iter_mut().rev().find(|e| {
                    e.plugin_id == plugin_id
                        && e.terminal_id == envelope.terminal_id.to_string()
                        && e.kind_name == kind_str
                }) {
                    e.suppressed_count += 1;
                }
                return NotificationDecision::DedupedSuppressed;
            }
            terminal_mesh_core::ArbiterDecision::Fire => {}
        }

        // Lazy permission check / request on the FIRST fire only.
        let perm = self.ensure_permission_resolved();

        let (summary, severity) = summarize(&payload.kind);
        let entry = TrayEntry {
            id: payload.event_id.to_string(),
            plugin_id: plugin_id.to_string(),
            terminal_id: envelope.terminal_id.to_string(),
            kind_name: kind_str.to_string(),
            severity,
            summary,
            fired_at_unix_ms: system_time_to_unix_ms(envelope.timestamp),
            suppressed_count: 0,
            is_orchestrator,
        };
        {
            let mut guard = self.inner.entries.lock().expect("entries poisoned");
            guard.push_back(entry.clone());
            while guard.len() > MAX_TRAY_ENTRIES {
                guard.pop_front();
            }
        }

        match perm {
            NotifyPermissionState::Granted => {
                let (title, body) = format_notification(
                    plugin_id,
                    &entry.kind_name,
                    &payload.kind,
                    &entry.summary,
                    is_orchestrator,
                );
                self.inner.sink.fire(&title, &body);
                NotificationDecision::FiredNativeAndTray
            }
            NotifyPermissionState::Denied | NotifyPermissionState::Prompt => {
                // `ensure_permission_resolved` only returns `Prompt`
                // if the sink itself returned `Prompt` from both
                // `permission_state` and `request_permission` — which
                // on macOS shouldn't happen because the OS resolves
                // to Granted/Denied after the user dismisses the
                // prompt. Treat as record-without-notify so we don't
                // lose the event.
                NotificationDecision::TrayOnlyPermissionDenied
            }
        }
    }

    fn ensure_permission_resolved(&self) -> NotifyPermissionState {
        let cached = self.inner.permission.load(Ordering::SeqCst);
        if cached == PERM_GRANTED {
            return NotifyPermissionState::Granted;
        }
        if cached == PERM_DENIED {
            return NotifyPermissionState::Denied;
        }
        // Unknown: query the sink, then maybe request.
        let mut state = self.inner.sink.permission_state();
        if matches!(state, NotifyPermissionState::Prompt) {
            state = self.inner.sink.request_permission();
        }
        let cached_value = match state {
            NotifyPermissionState::Granted => PERM_GRANTED,
            NotifyPermissionState::Denied => PERM_DENIED,
            NotifyPermissionState::Prompt => PERM_UNKNOWN,
        };
        // CAS to avoid clobbering a concurrent fire that may have
        // already set the state. We only set if still Unknown.
        let _ = self.inner.permission.compare_exchange(
            PERM_UNKNOWN,
            cached_value,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        state
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationDecision {
    FiredNativeAndTray,
    TrayOnlyPermissionDenied,
    DedupedSuppressed,
    /// Returned when called with a non-NeedsAttention envelope —
    /// defensive only; the call site only invokes this method on
    /// NeedsAttention.
    SkippedNonAttention,
}

/// task22 Round 41: pure helper extracted from the tray-icon click
/// handler so the toggle decision (which depends only on the tray
/// window's current visibility) is unit-testable without spinning up
/// a Tauri webview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayToggleAction {
    Show,
    Hide,
}

pub fn compute_tray_toggle_action(currently_visible: bool) -> TrayToggleAction {
    if currently_visible {
        TrayToggleAction::Hide
    } else {
        TrayToggleAction::Show
    }
}

/// Stable camelCase wire string for the `AttentionKind` discriminant,
/// matching `kind_name()` from terminal-mesh-core but lower-camel for
/// the JSON wire shape.
fn kind_name_str(kind: &AttentionKind) -> &'static str {
    match kind {
        AttentionKind::Completion { .. } => "completion",
        AttentionKind::NonZeroExit { .. } => "nonZeroExit",
        AttentionKind::PromptWaiting => "promptWaiting",
        AttentionKind::AgentMarker { .. } => "agentMarker",
        AttentionKind::Disconnect => "disconnect",
        AttentionKind::TaskComplete { .. } => "taskComplete",
    }
}

fn severity_name(severity: AttentionSeverity) -> String {
    match severity {
        AttentionSeverity::Info => "info".to_string(),
        AttentionSeverity::NeedsConfirm => "needsConfirm".to_string(),
        AttentionSeverity::Error => "error".to_string(),
    }
}

/// Per `docs/specs/terminal-events.md` §"Summary Strings": derive a
/// short human-readable summary + severity from the attention kind.
/// Only `AgentMarker` carries an explicit summary/severity; other
/// variants get a deterministic short string.
fn summarize(kind: &AttentionKind) -> (String, String) {
    match kind {
        AttentionKind::Completion { exit_code } => (
            format!("Completed (exit {exit_code})"),
            severity_name(AttentionSeverity::Info),
        ),
        AttentionKind::NonZeroExit { exit_code } => (
            format!("Exited with code {exit_code}"),
            severity_name(AttentionSeverity::Error),
        ),
        AttentionKind::PromptWaiting => (
            "Waiting for input".to_string(),
            severity_name(AttentionSeverity::NeedsConfirm),
        ),
        AttentionKind::AgentMarker { summary, severity } => (
            summary.clone().unwrap_or_else(|| "Agent marker".to_string()),
            severity_name(*severity),
        ),
        AttentionKind::Disconnect => (
            "Disconnected".to_string(),
            severity_name(AttentionSeverity::Error),
        ),
        AttentionKind::TaskComplete { summary } => (
            summary.clone(),
            severity_name(AttentionSeverity::Info),
        ),
    }
}

fn notification_title(plugin_id: &str, kind_name: &str) -> String {
    format!("{plugin_id}: {kind_name}")
}

/// task23 / AC-3.5: route orchestrator AgentMarker events through a
/// distinct "claude:" prefix so claude's verbatim task summary
/// surfaces as e.g. `"claude: 标记 3 封邮件为已读 + 新增 2 条 arXiv
/// 推荐"`. Non-AgentMarker kinds in the orchestrator (Completion,
/// NonZeroExit, PromptWaiting) continue to use the regular plugin-
/// prefixed format — this preserves AC-3.5's negative-test contract
/// that a non-OSC exit in the orchestrator PTY fires the regular
/// completion notification, NOT the semantic-summary path.
pub fn format_notification(
    plugin_id: &str,
    kind_name: &str,
    kind: &AttentionKind,
    summary: &str,
    is_orchestrator: bool,
) -> (String, String) {
    let is_agent_marker = matches!(kind, AttentionKind::AgentMarker { .. });
    if is_orchestrator && is_agent_marker {
        // Title carries the verbatim summary; body repeats it so
        // macOS notification rendering (which can hide the body
        // under summary-only conditions) is informative either way.
        (format!("claude: {summary}"), summary.to_string())
    } else {
        (notification_title(plugin_id, kind_name), summary.to_string())
    }
}

fn system_time_to_unix_ms(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

// ----- Production sink wrapping tauri-plugin-notification -----

/// Production `NotifySink` that holds a Tauri `AppHandle` and routes
/// to `tauri-plugin-notification`. Cheap to clone (AppHandle is
/// internally Arc-shared).
pub struct RealNotifySink {
    app: tauri::AppHandle,
}

impl RealNotifySink {
    pub fn new(app: tauri::AppHandle) -> Self {
        Self { app }
    }
}

impl NotifySink for RealNotifySink {
    fn fire(&self, title: &str, body: &str) {
        use tauri_plugin_notification::NotificationExt;
        let res = self
            .app
            .notification()
            .builder()
            .title(title)
            .body(body)
            .show();
        if let Err(err) = res {
            tracing::warn!(error = %err, title = %title, "notification fire failed");
        }
    }

    fn permission_state(&self) -> NotifyPermissionState {
        use tauri_plugin_notification::NotificationExt;
        match self.app.notification().permission_state() {
            Ok(tauri::plugin::PermissionState::Granted) => NotifyPermissionState::Granted,
            Ok(tauri::plugin::PermissionState::Denied) => NotifyPermissionState::Denied,
            Ok(_) => NotifyPermissionState::Prompt,
            Err(err) => {
                tracing::warn!(error = %err, "notification permission_state query failed");
                NotifyPermissionState::Prompt
            }
        }
    }

    fn request_permission(&self) -> NotifyPermissionState {
        use tauri_plugin_notification::NotificationExt;
        match self.app.notification().request_permission() {
            Ok(tauri::plugin::PermissionState::Granted) => NotifyPermissionState::Granted,
            Ok(tauri::plugin::PermissionState::Denied) => NotifyPermissionState::Denied,
            Ok(_) => NotifyPermissionState::Prompt,
            Err(err) => {
                tracing::warn!(error = %err, "notification request_permission failed");
                NotifyPermissionState::Prompt
            }
        }
    }
}

// ----- Tauri commands -----

#[tauri::command]
pub fn notification_list_recent_tray_entries(
    service: tauri::State<'_, NotificationService>,
) -> Vec<TrayEntry> {
    service.list_recent_entries()
}

#[tauri::command]
pub fn notification_clear_tray_entries(service: tauri::State<'_, NotificationService>) {
    service.clear_entries();
}

#[tauri::command]
pub fn notification_get_permission_state(
    service: tauri::State<'_, NotificationService>,
) -> PermissionStateDto {
    service.permission_state_dto()
}

#[tauri::command]
pub fn notification_list_recent_papers_tray_entries(
    service: tauri::State<'_, NotificationService>,
) -> Vec<PapersTrayEntry> {
    service.list_recent_papers_entries()
}

#[tauri::command]
pub fn notification_clear_papers_tray_entries(service: tauri::State<'_, NotificationService>) {
    service.clear_papers_entries();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::thread;
    use std::time::Duration;
    use terminal_mesh_core::{
        AttentionKind, AttentionSeverity, NeedsAttentionPayload, TerminalEvent, TerminalEventEnvelope,
    };
    use uuid::Uuid;

    /// In-process sink that records fires + lets tests pre-program
    /// the permission state.
    struct RecordingSink {
        fires: Mutex<Vec<(String, String)>>,
        permission: Mutex<NotifyPermissionState>,
    }

    impl RecordingSink {
        fn new(initial: NotifyPermissionState) -> Arc<Self> {
            Arc::new(Self {
                fires: Mutex::new(Vec::new()),
                permission: Mutex::new(initial),
            })
        }
        fn fire_count(&self) -> usize {
            self.fires.lock().unwrap().len()
        }
    }

    impl NotifySink for RecordingSink {
        fn fire(&self, title: &str, body: &str) {
            self.fires
                .lock()
                .unwrap()
                .push((title.to_string(), body.to_string()));
        }
        fn permission_state(&self) -> NotifyPermissionState {
            *self.permission.lock().unwrap()
        }
        fn request_permission(&self) -> NotifyPermissionState {
            *self.permission.lock().unwrap()
        }
    }

    fn fake_attention_envelope(
        terminal_id: Uuid,
        kind: AttentionKind,
    ) -> TerminalEventEnvelope {
        TerminalEventEnvelope::now(
            terminal_id,
            TerminalEvent::NeedsAttention {
                payload: NeedsAttentionPayload {
                    event_id: Uuid::new_v4(),
                    dedup_key: String::new(),
                    kind,
                },
            },
        )
    }

    #[test]
    fn dedup_arbiter_suppresses_repeat_within_window() {
        let sink = RecordingSink::new(NotifyPermissionState::Granted);
        let svc = NotificationService::with_sink(sink.clone());
        let tid = Uuid::new_v4();
        let env1 = fake_attention_envelope(tid, AttentionKind::PromptWaiting);
        let env2 = fake_attention_envelope(tid, AttentionKind::PromptWaiting);

        let d1 = svc.on_needs_attention(&env1, false);
        let d2 = svc.on_needs_attention(&env2, false);
        assert_eq!(d1, NotificationDecision::FiredNativeAndTray);
        assert_eq!(d2, NotificationDecision::DedupedSuppressed);
        assert_eq!(sink.fire_count(), 1, "native fire only once per dedup window");
        let entries = svc.list_recent_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].suppressed_count, 1);
    }

    #[test]
    fn dedup_arbiter_fires_again_after_window_elapses() {
        // The terminal-mesh-core arbiter uses Instant; we can't fast-
        // forward time. Sleep just past the 2s window. Accepts the
        // 2.1s cost for the regression confidence.
        let sink = RecordingSink::new(NotifyPermissionState::Granted);
        let svc = NotificationService::with_sink(sink.clone());
        let tid = Uuid::new_v4();
        let env1 = fake_attention_envelope(tid, AttentionKind::PromptWaiting);
        let env2 = fake_attention_envelope(tid, AttentionKind::PromptWaiting);

        let _ = svc.on_needs_attention(&env1, false);
        thread::sleep(Duration::from_millis(2100));
        let d2 = svc.on_needs_attention(&env2, false);
        assert_eq!(d2, NotificationDecision::FiredNativeAndTray);
        assert_eq!(sink.fire_count(), 2);
    }

    #[test]
    fn permission_denied_records_tray_entry_without_notify() {
        let sink = RecordingSink::new(NotifyPermissionState::Denied);
        let svc = NotificationService::with_sink(sink.clone());
        let tid = Uuid::new_v4();
        let env = fake_attention_envelope(tid, AttentionKind::PromptWaiting);

        let d = svc.on_needs_attention(&env, false);
        assert_eq!(d, NotificationDecision::TrayOnlyPermissionDenied);
        assert_eq!(
            sink.fire_count(),
            0,
            "native notification must NOT fire when permission is denied"
        );
        assert_eq!(
            svc.list_recent_entries().len(),
            1,
            "tray entry MUST still be recorded so the tray window drives surfacing"
        );
        assert!(matches!(svc.permission_state_dto(), PermissionStateDto::Denied));
    }

    #[test]
    fn tray_entry_ring_caps_at_max_size() {
        let sink = RecordingSink::new(NotifyPermissionState::Granted);
        let svc = NotificationService::with_sink(sink.clone());
        for _ in 0..(MAX_TRAY_ENTRIES + 10) {
            let env = fake_attention_envelope(Uuid::new_v4(), AttentionKind::PromptWaiting);
            let _ = svc.on_needs_attention(&env, false);
        }
        let entries = svc.list_recent_entries();
        assert_eq!(entries.len(), MAX_TRAY_ENTRIES);
    }

    #[test]
    fn clear_entries_empties_tray_ring() {
        let sink = RecordingSink::new(NotifyPermissionState::Granted);
        let svc = NotificationService::with_sink(sink.clone());
        for _ in 0..3 {
            let env = fake_attention_envelope(Uuid::new_v4(), AttentionKind::PromptWaiting);
            let _ = svc.on_needs_attention(&env, false);
        }
        assert_eq!(svc.list_recent_entries().len(), 3);
        svc.clear_entries();
        assert_eq!(svc.list_recent_entries().len(), 0);
    }

    #[test]
    fn permission_state_dto_starts_unknown_then_resolves_on_first_event() {
        let sink = RecordingSink::new(NotifyPermissionState::Granted);
        let svc = NotificationService::with_sink(sink.clone());
        assert!(matches!(svc.permission_state_dto(), PermissionStateDto::Unknown));
        let env = fake_attention_envelope(Uuid::new_v4(), AttentionKind::PromptWaiting);
        let _ = svc.on_needs_attention(&env, false);
        assert!(matches!(svc.permission_state_dto(), PermissionStateDto::Granted));
    }

    #[test]
    fn non_attention_envelope_is_skipped() {
        let sink = RecordingSink::new(NotifyPermissionState::Granted);
        let svc = NotificationService::with_sink(sink.clone());
        let env = TerminalEventEnvelope::now(
            Uuid::new_v4(),
            TerminalEvent::Output { bytes: vec![1, 2, 3] },
        );
        let d = svc.on_needs_attention(&env, false);
        assert_eq!(d, NotificationDecision::SkippedNonAttention);
        assert_eq!(sink.fire_count(), 0);
        assert_eq!(svc.list_recent_entries().len(), 0);
    }

    // Round 41 (task22 remediation): real-path shape regression.
    // The terminal-mesh-core actor no longer pre-dedups, so two
    // back-to-back classifications produce two distinct
    // `NeedsAttention` envelopes with fresh `event_id`s. The host
    // `NotificationService` MUST be the single dedup point: one
    // native fire + one tray entry whose `suppressed_count` reaches
    // 1 after the second envelope.
    #[test]
    fn host_service_dedups_repeated_envelopes_with_fresh_event_ids() {
        let sink = RecordingSink::new(NotifyPermissionState::Granted);
        let svc = NotificationService::with_sink(sink.clone());
        let tid = Uuid::new_v4();
        // Build TWO envelopes with the SAME (plugin_id, terminal_id,
        // kind) but FRESH event_ids — this is exactly what the
        // post-Round-41 actor emits.
        let env1 = fake_attention_envelope(tid, AttentionKind::Completion { exit_code: 0 });
        let env2 = fake_attention_envelope(tid, AttentionKind::Completion { exit_code: 0 });
        // Sanity: event_ids differ (proves the actor would have
        // pre-deduped before Round 41 because the key matches).
        let TerminalEvent::NeedsAttention { payload: ref p1 } = env1.event else {
            unreachable!();
        };
        let TerminalEvent::NeedsAttention { payload: ref p2 } = env2.event else {
            unreachable!();
        };
        assert_ne!(p1.event_id, p2.event_id, "fresh event_ids per envelope");

        let d1 = svc.on_needs_attention(&env1, false);
        let d2 = svc.on_needs_attention(&env2, false);
        assert_eq!(d1, NotificationDecision::FiredNativeAndTray);
        assert_eq!(d2, NotificationDecision::DedupedSuppressed);
        assert_eq!(sink.fire_count(), 1, "host arbiter is the single dedup point");
        let entries = svc.list_recent_entries();
        assert_eq!(entries.len(), 1, "single tray entry after dedup");
        assert_eq!(
            entries[0].suppressed_count, 1,
            "suppressed_count tracks deduped repeats"
        );
    }

    // ----- task23 / AC-3.5 + AC-8.3: orchestrator semantic-summary -----

    fn agent_marker_envelope(terminal_id: Uuid, summary: &str) -> TerminalEventEnvelope {
        fake_attention_envelope(
            terminal_id,
            AttentionKind::AgentMarker {
                summary: Some(summary.to_string()),
                severity: AttentionSeverity::Info,
            },
        )
    }

    #[test]
    fn orchestrator_agent_marker_fires_semantic_summary_notification() {
        // The exact AC-3.5 example phrasing — claude emits its
        // verbatim summary via OSC AgentMarker; the host service
        // wraps it as `claude: <summary>` for the native title.
        let sink = RecordingSink::new(NotifyPermissionState::Granted);
        let svc = NotificationService::with_sink(sink.clone());
        let summary = "标记 3 封邮件为已读 + 新增 2 条 arXiv 推荐";
        let env = agent_marker_envelope(Uuid::new_v4(), summary);

        let d = svc.on_needs_attention(&env, true);
        assert_eq!(d, NotificationDecision::FiredNativeAndTray);

        let fires = sink.fires.lock().unwrap();
        assert_eq!(fires.len(), 1, "exactly one native fire");
        let (title, body) = &fires[0];
        assert_eq!(
            title,
            &format!("claude: {summary}"),
            "title MUST be `claude: <verbatim summary>` per AC-3.5"
        );
        assert_eq!(body, summary, "body repeats the verbatim summary");
        drop(fires);

        let entries = svc.list_recent_entries();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_orchestrator, "tray entry tagged orchestrator");
        assert_eq!(entries[0].summary, summary);
        assert_eq!(entries[0].kind_name, "agentMarker");
    }

    #[test]
    fn orchestrator_completion_uses_regular_path_not_semantic_summary() {
        // AC-3.5 negative test: a raw exit in the orchestrator PTY
        // (no OSC AgentMarker) MUST fire the regular completion
        // notification, NOT the semantic-summary path.
        let sink = RecordingSink::new(NotifyPermissionState::Granted);
        let svc = NotificationService::with_sink(sink.clone());
        let env = fake_attention_envelope(
            Uuid::new_v4(),
            AttentionKind::Completion { exit_code: 0 },
        );

        let d = svc.on_needs_attention(&env, true);
        assert_eq!(d, NotificationDecision::FiredNativeAndTray);

        let fires = sink.fires.lock().unwrap();
        let (title, body) = &fires[0];
        assert!(
            !title.starts_with("claude:"),
            "non-AgentMarker orchestrator events MUST NOT use the semantic-summary prefix; got: {title}"
        );
        assert_eq!(title, "terminal_mesh: completion");
        assert_eq!(body, "Completed (exit 0)");
        drop(fires);

        let entries = svc.list_recent_entries();
        assert!(
            entries[0].is_orchestrator,
            "tray entry still tagged orchestrator (informational); only the formatting differs"
        );
    }

    #[test]
    fn non_orchestrator_agent_marker_uses_regular_path() {
        // A regular tab's AgentMarker (e.g., a non-orchestrator
        // claude or another agent-emitting tool) MUST NOT use the
        // `claude:` prefix — that prefix is reserved for the
        // orchestrator's privileged claude session.
        let sink = RecordingSink::new(NotifyPermissionState::Granted);
        let svc = NotificationService::with_sink(sink.clone());
        let env = agent_marker_envelope(Uuid::new_v4(), "some plugin task");

        let d = svc.on_needs_attention(&env, false);
        assert_eq!(d, NotificationDecision::FiredNativeAndTray);

        let fires = sink.fires.lock().unwrap();
        let (title, _) = &fires[0];
        assert!(
            !title.starts_with("claude:"),
            "non-orchestrator AgentMarker MUST NOT use the claude prefix; got: {title}"
        );
        assert_eq!(title, "terminal_mesh: agentMarker");
        drop(fires);

        let entries = svc.list_recent_entries();
        assert!(!entries[0].is_orchestrator);
    }

    #[test]
    fn orchestrator_agent_marker_dedups_under_window() {
        // The AC-8.3 path inherits the AC-8.1 dedup contract — two
        // back-to-back orchestrator AgentMarker envelopes with the
        // same dedup key within the 2s window yield one native fire
        // and one tray entry with suppressed_count=1.
        let sink = RecordingSink::new(NotifyPermissionState::Granted);
        let svc = NotificationService::with_sink(sink.clone());
        let tid = Uuid::new_v4();
        let env1 = agent_marker_envelope(tid, "first summary");
        let env2 = agent_marker_envelope(tid, "second summary");

        let d1 = svc.on_needs_attention(&env1, true);
        let d2 = svc.on_needs_attention(&env2, true);
        assert_eq!(d1, NotificationDecision::FiredNativeAndTray);
        assert_eq!(d2, NotificationDecision::DedupedSuppressed);
        assert_eq!(sink.fire_count(), 1);

        let entries = svc.list_recent_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].suppressed_count, 1);
        assert!(entries[0].is_orchestrator);
        // First-emitted summary wins — the dedup-suppress path only
        // bumps suppressed_count; it does not replace the entry's
        // body. UX-wise: the user sees the first event's summary
        // with a "+1" badge indicating more were suppressed.
        assert_eq!(entries[0].summary, "first summary");
    }

    // Round 41 (task22 remediation): unit-testable tray-toggle
    // decision separated from the live Tauri Window handle.
    #[test]
    fn compute_tray_toggle_action_hides_when_visible() {
        assert_eq!(
            compute_tray_toggle_action(true),
            TrayToggleAction::Hide
        );
    }

    #[test]
    fn compute_tray_toggle_action_shows_when_hidden() {
        assert_eq!(
            compute_tray_toggle_action(false),
            TrayToggleAction::Show
        );
    }

    #[test]
    fn summary_for_each_attention_kind_is_deterministic() {
        assert_eq!(summarize(&AttentionKind::Completion { exit_code: 0 }).0, "Completed (exit 0)");
        assert_eq!(summarize(&AttentionKind::NonZeroExit { exit_code: 7 }).0, "Exited with code 7");
        assert_eq!(summarize(&AttentionKind::PromptWaiting).0, "Waiting for input");
        let (s, sev) = summarize(&AttentionKind::AgentMarker {
            summary: Some("done".into()),
            severity: AttentionSeverity::Error,
        });
        assert_eq!(s, "done");
        assert_eq!(sev, "error");
    }
}
