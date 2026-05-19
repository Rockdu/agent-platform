//! Host-side per-tab lifecycle snapshot consumed by the inner-rail
//! Running/Done queue, the `terminal_mesh.list_tabs` MCP tool, and the
//! orchestrator-isolation auth filter.
//!
//! The snapshot is intentionally minimal: a transport descriptor, a
//! current status with optional done-reason discriminator, and a last
//! activity timestamp used for FIFO ordering. The notification path
//! mutates it via `TerminalMeshRegistry::update_snapshot` and then
//! publishes the update through [`emit_lifecycle_updated`].

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use terminal_mesh_core::AttentionKind;
use uuid::Uuid;

/// Tauri event topic for snapshot updates. The frontend subscribes to
/// this single topic and demultiplexes by the envelope's `terminalId`.
/// Wired by the notification path; today only the public constant is
/// reachable from outside this module.
#[allow(dead_code)]
pub const LIFECYCLE_UPDATED_TOPIC: &str = "lifecycle://updated";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum TabKind {
    Orchestrator,
    Workspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum TransportKind {
    Local,
    Ssh,
    SshDocker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum TabStatus {
    Running,
    Done,
}

/// The four reasons a tab transitions into the Done section.
/// `Disconnected` covers transport-level loss (SSH/Docker channel drop);
/// `TaskComplete` carries the OSC-emitted summary so the frontend can
/// render it inline with the badge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "PascalCase")]
pub enum DoneReason {
    CleanCompletion,
    NonZeroExit { code: i32 },
    Disconnected,
    TaskComplete { summary: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceLifecycleSnapshot {
    /// Set when the tab is bound to a persisted workspace record;
    /// `None` for transient terminals or the orchestrator slot.
    pub workspace_id: Option<String>,
    pub tab_kind: TabKind,
    pub transport_kind: TransportKind,
    pub status: TabStatus,
    /// Only meaningful when `status == Done`; `None` otherwise.
    pub done_reason: Option<DoneReason>,
    /// Milliseconds since Unix epoch. Updated on signal arrival
    /// (output, attention, user-initiated stdin); read by the inner
    /// rail to FIFO-order Running and Done sections.
    pub last_activity_at_unix_ms: i64,
    /// `true` while this tab's workspace is sitting in the
    /// `WorkspaceLaunchScheduler` pending queue (the global
    /// concurrency cap is saturated and this workspace is waiting
    /// for a slot). Cleared the moment the launch settles or the
    /// pending entry is canceled. Surfaces a `等待启动` badge in the
    /// inner rail so users can distinguish "running" from "waiting
    /// in line".
    #[serde(default)]
    pub pending_launch: bool,
    /// `true` while the terminal's shell/agent has displayed a prompt
    /// and is waiting for user input. Set when `PromptWaiting` fires;
    /// cleared when the user sends stdin (`userInitiated = true`).
    /// Drives the 完成区 / 运行区 rail split on the frontend — a
    /// terminal at the prompt is "done with its current task" even
    /// while `status == Running`, so it belongs in 完成区.
    #[serde(default)]
    pub agent_busy: bool,
}

impl WorkspaceLifecycleSnapshot {
    /// Default snapshot for a freshly-spawned local tab without a
    /// persisted workspace id. Kept as a convenience for tests + any
    /// caller that doesn't need workspace binding; the live spawn
    /// path uses `fresh_local_with_workspace_id`.
    #[allow(dead_code)]
    pub fn fresh_local(tab_kind: TabKind) -> Self {
        Self::fresh_local_with_workspace_id(tab_kind, None)
    }

    /// Variant that populates `workspace_id` from the persisted
    /// workspace record but hard-codes `transport_kind: Local`.
    /// Kept for test fixtures and any caller that genuinely needs a
    /// Local snapshot; production spawn paths use
    /// `fresh_for_workspace_with_kind` so Remote tabs are not
    /// mislabeled as Local in the rail and `list_tabs` projection.
    #[allow(dead_code)]
    pub fn fresh_local_with_workspace_id(
        tab_kind: TabKind,
        workspace_id: Option<String>,
    ) -> Self {
        Self::fresh_for_workspace_with_kind(tab_kind, workspace_id, TransportKind::Local)
    }

    /// Snapshot constructor that takes the resolved transport kind
    /// so SSH and Docker-over-SSH tabs surface with the right
    /// transport in the rail icon, status panel, and MCP
    /// `list_tabs` projection. Status is always `Running` and
    /// `last_activity_at_unix_ms` is seeded to now.
    pub fn fresh_for_workspace_with_kind(
        tab_kind: TabKind,
        workspace_id: Option<String>,
        transport_kind: TransportKind,
    ) -> Self {
        Self {
            workspace_id,
            tab_kind,
            transport_kind,
            status: TabStatus::Running,
            done_reason: None,
            last_activity_at_unix_ms: now_unix_ms(),
            pending_launch: false,
            agent_busy: false,
        }
    }
}

/// Envelope shape the frontend sees on `LIFECYCLE_UPDATED_TOPIC`.
///
/// `terminal_id` is `Some(uuid)` only when a real PTY has been
/// recorded for the tab. While a workspace is sitting in the launch
/// queue (pre-spawn placeholder), `terminal_id` is `None` and
/// `tab_id` carries the route. The frontend hook treats `None`
/// `terminal_id` as "snapshot data only — do NOT mark this tab as
/// resolved in `terminalIdByTabId`", so the real-id event that
/// arrives later still triggers the resolved-id update path
/// instead of being suppressed by an "already known" gate.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LifecycleUpdateEvent {
    pub terminal_id: Option<String>,
    pub tab_id: Option<String>,
    pub snapshot: WorkspaceLifecycleSnapshot,
}

/// Emit `LIFECYCLE_UPDATED_TOPIC` for a real terminal id. The
/// notification-driven update path calls this AFTER applying the
/// mutation under `TerminalMeshRegistry::update_snapshot`.
pub fn emit_lifecycle_updated(
    app: &AppHandle,
    terminal_id: Uuid,
    snapshot: &WorkspaceLifecycleSnapshot,
) {
    let envelope = LifecycleUpdateEvent {
        terminal_id: Some(terminal_id.to_string()),
        tab_id: None,
        snapshot: snapshot.clone(),
    };
    if let Err(e) = app.emit(LIFECYCLE_UPDATED_TOPIC, envelope) {
        tracing::warn!(
            %terminal_id,
            error = %e,
            "failed to emit lifecycle://updated"
        );
    }
}

/// Emit `LIFECYCLE_UPDATED_TOPIC` for a pending-launch placeholder
/// (no real PTY exists yet). The envelope carries `tab_id` for
/// routing and `terminal_id = None` so the frontend hook only
/// updates the snapshot map, not the resolved-terminal-id map.
pub fn emit_lifecycle_updated_for_placeholder(
    app: &AppHandle,
    tab_id: &str,
    snapshot: &WorkspaceLifecycleSnapshot,
) {
    let envelope = LifecycleUpdateEvent {
        terminal_id: None,
        tab_id: Some(tab_id.to_string()),
        snapshot: snapshot.clone(),
    };
    if let Err(e) = app.emit(LIFECYCLE_UPDATED_TOPIC, envelope) {
        tracing::warn!(
            tab_id = %tab_id,
            error = %e,
            "failed to emit lifecycle://updated for placeholder"
        );
    }
}

/// Classify an `AttentionKind` into the lifecycle Done reason it
/// should produce, if any. The Done queue transitions Running -> Done
/// on the first event that returns `Some(...)`. `PromptWaiting` and
/// `AgentMarker` deliberately return `None` so general agent activity
/// does not flicker tabs into Done — only Completion / NonZeroExit /
/// Disconnect / TaskComplete trigger the transition.
pub fn done_reason_from_attention(kind: &AttentionKind) -> Option<DoneReason> {
    match kind {
        AttentionKind::Completion { .. } => Some(DoneReason::CleanCompletion),
        AttentionKind::NonZeroExit { exit_code } => Some(DoneReason::NonZeroExit {
            code: *exit_code,
        }),
        AttentionKind::Disconnect => Some(DoneReason::Disconnected),
        AttentionKind::TaskComplete { summary } => Some(DoneReason::TaskComplete {
            summary: summary.clone(),
        }),
        AttentionKind::PromptWaiting => None,
        AttentionKind::AgentMarker { .. } => None,
    }
}

/// Apply a `NeedsAttention` event to the lifecycle snapshot in place.
/// Always refreshes `last_activity_at_unix_ms` so FIFO ordering in the
/// inner rail reflects signal arrival; additionally transitions to
/// `Done` with the matching `DoneReason` when the kind classifies to
/// one. Pure function — the caller (typically `on_terminal_attention`)
/// owns the lock + emit responsibilities.
pub fn apply_attention_to_snapshot(
    snap: &mut WorkspaceLifecycleSnapshot,
    kind: &AttentionKind,
    now_unix_ms: i64,
) {
    snap.last_activity_at_unix_ms = now_unix_ms;
    if let Some(reason) = done_reason_from_attention(kind) {
        snap.status = TabStatus::Done;
        snap.done_reason = Some(reason);
        snap.agent_busy = false;
    } else if matches!(kind, AttentionKind::PromptWaiting) {
        // PromptWaiting fires after 1 s of no new output (quiescence
        // mode — no shell-prompt regex required). When the terminal
        // goes quiet, the agent has finished its current response;
        // clear agent_busy so the tab moves to 完成区 (waiting for
        // the next instruction).
        snap.agent_busy = false;
    }
}

/// Registry-side half of the notification path: classify, mutate the
/// retained snapshot, and return the new value. Orchestrator-routed
/// terminals are skipped because the orchestrator slot lives in the
/// outer-tab strip, not the workspace Running/Done rail. Returns
/// `None` when no mutation should fire (orchestrator skip, or no
/// retained record because the tab was fully cleared via `forget`).
pub fn update_snapshot_for_attention(
    registry: &crate::terminal_mesh::TerminalMeshRegistry,
    terminal_id: Uuid,
    kind: &AttentionKind,
    is_orchestrator: bool,
) -> Option<WorkspaceLifecycleSnapshot> {
    if is_orchestrator {
        return None;
    }
    let now = now_unix_ms();
    registry.update_snapshot(terminal_id, |snap| {
        apply_attention_to_snapshot(snap, kind, now);
    })
}

/// Pure mutator: when the snapshot is currently `Done`, transition it
/// back to `Running`, clear `done_reason`, and refresh the activity
/// timestamp. Returns `true` if the snapshot actually changed,
/// `false` if it was already `Running`. Used by the user-initiated
/// stdin path to resume a Done tab without recreating its session.
/// Pure mutator called on user-initiated stdin. Sets `agent_busy =
/// true` (the user just submitted a task → terminal moves to 运行区).
/// Also transitions Done → Running when applicable. Returns `true`
/// when the snapshot actually changed and an event should be emitted.
pub fn resume_running_from_done(
    snap: &mut WorkspaceLifecycleSnapshot,
    now_unix_ms: i64,
) -> bool {
    let mut changed = false;
    // Mark agent busy: user submitted input → agent is now working.
    if !snap.agent_busy {
        snap.agent_busy = true;
        changed = true;
    }
    // Transition Done → Running when applicable.
    if matches!(snap.status, TabStatus::Done) {
        snap.status = TabStatus::Running;
        snap.done_reason = None;
        snap.last_activity_at_unix_ms = now_unix_ms;
        changed = true;
    }
    changed
}

/// Registry-side half of the user-stdin path: set `agent_busy = true`
/// and transition Done → Running if needed. Emits a lifecycle event
/// whenever either change fires so the frontend rail can update.
pub fn update_snapshot_for_user_stdin(
    registry: &crate::terminal_mesh::TerminalMeshRegistry,
    terminal_id: Uuid,
) -> Option<WorkspaceLifecycleSnapshot> {
    let now = now_unix_ms();
    let mut changed = false;
    let updated = registry.update_snapshot(terminal_id, |snap| {
        changed = resume_running_from_done(snap, now);
    })?;
    if changed {
        Some(updated)
    } else {
        None
    }
}

/// End-to-end user-stdin entry point: transition Done → Running on
/// the retained snapshot and emit `lifecycle://updated`. No-op when
/// the tab is already Running.
pub fn on_user_stdin(
    registry: &crate::terminal_mesh::TerminalMeshRegistry,
    app: &AppHandle,
    terminal_id: Uuid,
) {
    if let Some(updated) = update_snapshot_for_user_stdin(registry, terminal_id) {
        emit_lifecycle_updated(app, terminal_id, &updated);
    }
}

/// Registry-side helper that sets the snapshot's `pending_launch`
/// flag and returns the updated snapshot if the field changed. Used
/// by the `WorkspaceLaunchScheduler` integration to surface "waiting
/// in line" state in the inner rail. Returns `None` when the
/// retained record is gone or the flag value would not change.
pub fn update_snapshot_for_pending_launch(
    registry: &crate::terminal_mesh::TerminalMeshRegistry,
    terminal_id: Uuid,
    pending: bool,
) -> Option<WorkspaceLifecycleSnapshot> {
    let mut changed = false;
    let updated = registry.update_snapshot(terminal_id, |snap| {
        if snap.pending_launch != pending {
            snap.pending_launch = pending;
            changed = true;
        }
    })?;
    if changed {
        Some(updated)
    } else {
        None
    }
}

/// End-to-end pending-launch entry point: flip the flag and emit
/// `lifecycle://updated` so the rail row's Pending badge reconciles.
pub fn on_pending_launch_changed(
    registry: &crate::terminal_mesh::TerminalMeshRegistry,
    app: &AppHandle,
    terminal_id: Uuid,
    pending: bool,
) {
    if let Some(updated) = update_snapshot_for_pending_launch(registry, terminal_id, pending) {
        emit_lifecycle_updated(app, terminal_id, &updated);
    }
}

/// End-to-end notification-path entry point: classify, mutate, and
/// emit `lifecycle://updated` so the inner-rail React hook can
/// reconcile. Thin wrapper over `update_snapshot_for_attention` +
/// `emit_lifecycle_updated` — the split exists so unit tests can
/// exercise the registry mutation without a Tauri `AppHandle`.
pub fn on_terminal_attention(
    registry: &crate::terminal_mesh::TerminalMeshRegistry,
    app: &AppHandle,
    terminal_id: Uuid,
    kind: &AttentionKind,
    is_orchestrator: bool,
) {
    if let Some(updated) =
        update_snapshot_for_attention(registry, terminal_id, kind, is_orchestrator)
    {
        emit_lifecycle_updated(app, terminal_id, &updated);
    }
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_local_workspace_snapshot_is_running_with_local_transport() {
        let before = now_unix_ms();
        let snap = WorkspaceLifecycleSnapshot::fresh_local(TabKind::Workspace);
        let after = now_unix_ms();
        assert!(matches!(snap.tab_kind, TabKind::Workspace));
        assert!(matches!(snap.transport_kind, TransportKind::Local));
        assert!(matches!(snap.status, TabStatus::Running));
        assert!(snap.done_reason.is_none());
        assert!(snap.workspace_id.is_none());
        assert!(snap.last_activity_at_unix_ms >= before);
        assert!(snap.last_activity_at_unix_ms <= after);
    }

    #[test]
    fn fresh_local_orchestrator_snapshot_carries_orchestrator_tab_kind() {
        let snap = WorkspaceLifecycleSnapshot::fresh_local(TabKind::Orchestrator);
        assert!(matches!(snap.tab_kind, TabKind::Orchestrator));
    }

    #[test]
    fn lifecycle_update_event_serializes_with_camelcase_terminal_id() {
        let event = LifecycleUpdateEvent {
            terminal_id: Some("abc-123".into()),
            tab_id: None,
            snapshot: WorkspaceLifecycleSnapshot::fresh_local(TabKind::Workspace),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"terminalId\":\"abc-123\""), "got {json}");
        assert!(json.contains("\"tabId\":null"), "got {json}");
        assert!(json.contains("\"snapshot\":"), "got {json}");
        assert!(json.contains("\"tabKind\":\"Workspace\""), "got {json}");
    }

    /// Placeholder envelopes ride `terminal_id = null + tab_id =
    /// Some(...)` on the wire. The frontend hook keys its
    /// "resolved tab" decision on `terminal_id !== null`, so the
    /// shape must serialize literally; any accidental `Some` would
    /// poison `terminalIdByTabId` with a synthetic id.
    #[test]
    fn lifecycle_update_event_serializes_placeholder_with_null_terminal_id() {
        let event = LifecycleUpdateEvent {
            terminal_id: None,
            tab_id: Some("workspace-uuid".into()),
            snapshot: WorkspaceLifecycleSnapshot::fresh_local(TabKind::Workspace),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"terminalId\":null"), "got {json}");
        assert!(
            json.contains("\"tabId\":\"workspace-uuid\""),
            "got {json}"
        );
    }

    #[test]
    fn lifecycle_updated_topic_is_stable_lifecycle_scheme() {
        // Pin the topic string; the frontend subscribes to it directly.
        assert_eq!(LIFECYCLE_UPDATED_TOPIC, "lifecycle://updated");
    }

    #[test]
    fn done_reason_from_attention_maps_terminal_events_to_done_reasons() {
        use terminal_mesh_core::AttentionKind;
        assert!(matches!(
            done_reason_from_attention(&AttentionKind::Completion { exit_code: 0 }),
            Some(DoneReason::CleanCompletion)
        ));
        match done_reason_from_attention(&AttentionKind::NonZeroExit { exit_code: 7 }) {
            Some(DoneReason::NonZeroExit { code }) => assert_eq!(code, 7),
            other => panic!("expected NonZeroExit, got {other:?}"),
        }
        assert!(matches!(
            done_reason_from_attention(&AttentionKind::Disconnect),
            Some(DoneReason::Disconnected)
        ));
        match done_reason_from_attention(&AttentionKind::TaskComplete {
            summary: "shipped".into(),
        }) {
            Some(DoneReason::TaskComplete { summary }) => assert_eq!(summary, "shipped"),
            other => panic!("expected TaskComplete, got {other:?}"),
        }
    }

    #[test]
    fn apply_attention_to_snapshot_transitions_to_done_for_each_done_triggering_kind() {
        use terminal_mesh_core::AttentionKind;

        let cases = [
            (
                AttentionKind::Completion { exit_code: 0 },
                DoneReason::CleanCompletion,
            ),
            (
                AttentionKind::NonZeroExit { exit_code: 42 },
                DoneReason::NonZeroExit { code: 42 },
            ),
            (AttentionKind::Disconnect, DoneReason::Disconnected),
            (
                AttentionKind::TaskComplete {
                    summary: "shipped".into(),
                },
                DoneReason::TaskComplete {
                    summary: "shipped".into(),
                },
            ),
        ];

        for (kind, expected_reason) in cases {
            let mut snap = WorkspaceLifecycleSnapshot::fresh_local(TabKind::Workspace);
            assert!(matches!(snap.status, TabStatus::Running));
            apply_attention_to_snapshot(&mut snap, &kind, 1_700_000_000_111);
            assert!(matches!(snap.status, TabStatus::Done), "kind={kind:?}");
            assert_eq!(snap.done_reason, Some(expected_reason), "kind={kind:?}");
            assert_eq!(snap.last_activity_at_unix_ms, 1_700_000_000_111);
        }
    }

    #[test]
    fn apply_attention_to_snapshot_bumps_activity_without_changing_status_for_non_done_kinds() {
        use terminal_mesh_core::{AttentionKind, AttentionSeverity};

        for kind in [
            AttentionKind::PromptWaiting,
            AttentionKind::AgentMarker {
                summary: Some("partial progress".into()),
                severity: AttentionSeverity::Info,
            },
        ] {
            let mut snap = WorkspaceLifecycleSnapshot::fresh_local(TabKind::Workspace);
            let baseline = snap.last_activity_at_unix_ms;
            apply_attention_to_snapshot(&mut snap, &kind, baseline + 5_000);
            assert!(matches!(snap.status, TabStatus::Running), "kind={kind:?}");
            assert!(snap.done_reason.is_none(), "kind={kind:?}");
            assert_eq!(snap.last_activity_at_unix_ms, baseline + 5_000);
        }
    }

    #[test]
    fn apply_attention_to_snapshot_overwrites_earlier_done_reason_on_new_signal() {
        use terminal_mesh_core::AttentionKind;

        let mut snap = WorkspaceLifecycleSnapshot::fresh_local(TabKind::Workspace);
        apply_attention_to_snapshot(
            &mut snap,
            &AttentionKind::Completion { exit_code: 0 },
            1,
        );
        assert_eq!(snap.done_reason, Some(DoneReason::CleanCompletion));

        apply_attention_to_snapshot(
            &mut snap,
            &AttentionKind::NonZeroExit { exit_code: 9 },
            2,
        );
        assert_eq!(
            snap.done_reason,
            Some(DoneReason::NonZeroExit { code: 9 }),
            "later Done-triggering signal must overwrite earlier reason"
        );
        assert_eq!(snap.last_activity_at_unix_ms, 2);
    }

    #[test]
    fn resume_running_from_done_transitions_done_snapshot_and_returns_true() {
        let mut snap = WorkspaceLifecycleSnapshot::fresh_local(TabKind::Workspace);
        snap.status = TabStatus::Done;
        snap.done_reason = Some(DoneReason::CleanCompletion);
        snap.last_activity_at_unix_ms = 0;

        let did = resume_running_from_done(&mut snap, 1_700_000_000_111);

        assert!(did, "must report a transition happened");
        assert!(matches!(snap.status, TabStatus::Running));
        assert!(snap.done_reason.is_none());
        assert_eq!(snap.last_activity_at_unix_ms, 1_700_000_000_111);
    }

    #[test]
    fn resume_running_from_done_sets_agent_busy_for_running_snapshot() {
        let mut snap = WorkspaceLifecycleSnapshot::fresh_local(TabKind::Workspace);
        let baseline = snap.last_activity_at_unix_ms;
        assert!(matches!(snap.status, TabStatus::Running));
        assert!(!snap.agent_busy, "fresh terminal is idle");

        let did = resume_running_from_done(&mut snap, baseline + 9_999);

        // Running tab with agent_busy=false → sets busy, returns true (changed).
        assert!(did, "setting agent_busy counts as a change");
        assert!(matches!(snap.status, TabStatus::Running));
        assert!(snap.agent_busy, "must be marked busy after user stdin");
        // Activity timestamp is NOT bumped for already-Running tabs
        // (the timestamp is only bumped on Done→Running transition).
        assert_eq!(snap.last_activity_at_unix_ms, baseline);
    }

    #[test]
    fn update_snapshot_for_user_stdin_returns_some_when_transitioning_done_to_running() {
        use crate::terminal_mesh::TerminalMeshRegistry;
        use terminal_mesh_core::ActorCommand;
        use tokio::sync::mpsc;

        let registry = TerminalMeshRegistry::new();
        let id = Uuid::new_v4();
        let (tx, _rx) = mpsc::channel::<ActorCommand>(1);
        let scrollback = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        registry.record(id, tx, scrollback, None, TabKind::Workspace, None, TransportKind::Local);

        // Drive to Done first.
        registry
            .update_snapshot(id, |s| {
                s.status = TabStatus::Done;
                s.done_reason = Some(DoneReason::CleanCompletion);
            })
            .expect("recorded");

        let updated = update_snapshot_for_user_stdin(&registry, id)
            .expect("user stdin on Done tab must return Some");
        assert!(matches!(updated.status, TabStatus::Running));
        assert!(updated.done_reason.is_none());
    }

    /// First user-stdin on a Running-but-idle terminal sets agent_busy
    /// and emits (returns Some). Second call is a true no-op (already
    /// busy) and returns None.
    #[test]
    fn update_snapshot_for_user_stdin_marks_busy_on_idle_running_tab() {
        use crate::terminal_mesh::TerminalMeshRegistry;
        use terminal_mesh_core::ActorCommand;
        use tokio::sync::mpsc;

        let registry = TerminalMeshRegistry::new();
        let id = Uuid::new_v4();
        let (tx, _rx) = mpsc::channel::<ActorCommand>(1);
        let scrollback = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        registry.record(id, tx, scrollback, None, TabKind::Workspace, None, TransportKind::Local);

        // First user stdin: idle Running → busy Running. Emits.
        let result = update_snapshot_for_user_stdin(&registry, id);
        assert!(result.is_some(), "first stdin on idle tab must emit");
        let snap = result.unwrap();
        assert!(snap.agent_busy);
        assert!(matches!(snap.status, TabStatus::Running));

        // Second user stdin: already busy → no change, no emit.
        let result2 = update_snapshot_for_user_stdin(&registry, id);
        assert!(result2.is_none(), "already busy: no-op, no emit");
    }

    #[test]
    fn update_snapshot_for_user_stdin_returns_none_for_unknown_terminal() {
        use crate::terminal_mesh::TerminalMeshRegistry;
        let registry = TerminalMeshRegistry::new();
        let result = update_snapshot_for_user_stdin(&registry, Uuid::new_v4());
        assert!(result.is_none());
    }

    #[test]
    fn update_snapshot_for_attention_skips_when_orchestrator() {
        use crate::terminal_mesh::TerminalMeshRegistry;
        use terminal_mesh_core::{ActorCommand, AttentionKind};
        use tokio::sync::mpsc;

        let registry = TerminalMeshRegistry::new();
        let id = Uuid::new_v4();
        let (tx, _rx) = mpsc::channel::<ActorCommand>(1);
        let scrollback = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        registry.record(id, tx, scrollback, None, TabKind::Orchestrator, None, TransportKind::Local);
        let before = registry.snapshot_for_terminal(id).expect("recorded");

        let result = update_snapshot_for_attention(
            &registry,
            id,
            &AttentionKind::Completion { exit_code: 0 },
            true,
        );

        assert!(result.is_none(), "orchestrator path must skip the mutation");
        let after = registry.snapshot_for_terminal(id).expect("still present");
        assert!(matches!(after.status, TabStatus::Running));
        assert_eq!(
            before.last_activity_at_unix_ms,
            after.last_activity_at_unix_ms,
            "snapshot must NOT be touched when is_orchestrator=true"
        );
    }

    #[test]
    fn update_snapshot_for_attention_mutates_workspace_snapshot_to_done() {
        use crate::terminal_mesh::TerminalMeshRegistry;
        use terminal_mesh_core::{ActorCommand, AttentionKind};
        use tokio::sync::mpsc;

        let registry = TerminalMeshRegistry::new();
        let id = Uuid::new_v4();
        let (tx, _rx) = mpsc::channel::<ActorCommand>(1);
        let scrollback = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        registry.record(id, tx, scrollback, None, TabKind::Workspace, None, TransportKind::Local);

        let updated = update_snapshot_for_attention(
            &registry,
            id,
            &AttentionKind::TaskComplete {
                summary: "merged".into(),
            },
            false,
        )
        .expect("workspace path must mutate and return the snapshot");

        assert!(matches!(updated.status, TabStatus::Done));
        assert_eq!(
            updated.done_reason,
            Some(DoneReason::TaskComplete {
                summary: "merged".into(),
            })
        );

        let reread = registry.snapshot_for_terminal(id).expect("still present");
        assert!(matches!(reread.status, TabStatus::Done));
    }

    #[test]
    fn done_reason_from_attention_returns_none_for_non_done_triggering_kinds() {
        use terminal_mesh_core::{AttentionKind, AttentionSeverity};
        // PromptWaiting must NOT transition Running -> Done (long-running
        // tasks emit prompt-like output frequently; flicker would be bad).
        assert!(done_reason_from_attention(&AttentionKind::PromptWaiting).is_none());
        // AgentMarker is general agent activity, NOT a done signal. Only
        // the dedicated TaskComplete OSC marker should transition Done.
        assert!(done_reason_from_attention(&AttentionKind::AgentMarker {
            summary: Some("partial progress".into()),
            severity: AttentionSeverity::Info,
        })
        .is_none());
    }

    #[test]
    fn done_reason_serializes_with_kind_tag_and_payload_fields() {
        // Wire shape: serde-tag "kind" with PascalCase variant names + payload fields.
        let json = serde_json::to_string(&DoneReason::NonZeroExit { code: 7 }).unwrap();
        assert!(json.contains("\"kind\":\"NonZeroExit\""), "got {json}");
        assert!(json.contains("\"code\":7"), "got {json}");

        let json = serde_json::to_string(&DoneReason::TaskComplete {
            summary: "shipped".into(),
        })
        .unwrap();
        assert!(json.contains("\"kind\":\"TaskComplete\""), "got {json}");
        assert!(json.contains("\"summary\":\"shipped\""), "got {json}");
    }

    #[test]
    fn update_snapshot_for_pending_launch_marks_snapshot_pending() {
        use crate::terminal_mesh::TerminalMeshRegistry;
        use terminal_mesh_core::ActorCommand;
        use tokio::sync::mpsc;

        let registry = TerminalMeshRegistry::new();
        let id = Uuid::new_v4();
        let (tx, _rx) = mpsc::channel::<ActorCommand>(1);
        let scrollback = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        registry.record(id, tx, scrollback, None, TabKind::Workspace, None, TransportKind::Local);
        let baseline = registry.snapshot_for_terminal(id).expect("recorded");
        assert!(!baseline.pending_launch);

        let updated = update_snapshot_for_pending_launch(&registry, id, true)
            .expect("first flip must return Some");
        assert!(updated.pending_launch);
        let reread = registry.snapshot_for_terminal(id).expect("still present");
        assert!(reread.pending_launch);
    }

    #[test]
    fn update_snapshot_for_pending_launch_no_change_returns_none() {
        use crate::terminal_mesh::TerminalMeshRegistry;
        use terminal_mesh_core::ActorCommand;
        use tokio::sync::mpsc;

        let registry = TerminalMeshRegistry::new();
        let id = Uuid::new_v4();
        let (tx, _rx) = mpsc::channel::<ActorCommand>(1);
        let scrollback = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        registry.record(id, tx, scrollback, None, TabKind::Workspace, None, TransportKind::Local);

        // Snapshot starts with pending_launch = false; calling with false is
        // a no-op and must NOT emit a redundant lifecycle://updated.
        let result = update_snapshot_for_pending_launch(&registry, id, false);
        assert!(result.is_none(), "no-op flip must return None");
    }

    #[test]
    fn update_snapshot_for_pending_launch_clears_flag_when_settled() {
        use crate::terminal_mesh::TerminalMeshRegistry;
        use terminal_mesh_core::ActorCommand;
        use tokio::sync::mpsc;

        let registry = TerminalMeshRegistry::new();
        let id = Uuid::new_v4();
        let (tx, _rx) = mpsc::channel::<ActorCommand>(1);
        let scrollback = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        registry.record(id, tx, scrollback, None, TabKind::Workspace, None, TransportKind::Local);

        update_snapshot_for_pending_launch(&registry, id, true).expect("set");
        let cleared = update_snapshot_for_pending_launch(&registry, id, false)
            .expect("clearing must return Some");
        assert!(!cleared.pending_launch);
    }

    #[test]
    fn update_snapshot_for_pending_launch_for_unknown_terminal_returns_none() {
        use crate::terminal_mesh::TerminalMeshRegistry;
        let registry = TerminalMeshRegistry::new();
        let result = update_snapshot_for_pending_launch(&registry, Uuid::new_v4(), true);
        assert!(result.is_none());
    }
}
