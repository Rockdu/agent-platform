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
}

impl WorkspaceLifecycleSnapshot {
    /// Default snapshot for a freshly-spawned local tab. Callers
    /// override `tab_kind` (the orchestrator path passes
    /// `TabKind::Orchestrator`) and `transport_kind` (remote tabs pass
    /// `Ssh` / `SshDocker`) as needed.
    pub fn fresh_local(tab_kind: TabKind) -> Self {
        Self {
            workspace_id: None,
            tab_kind,
            transport_kind: TransportKind::Local,
            status: TabStatus::Running,
            done_reason: None,
            last_activity_at_unix_ms: now_unix_ms(),
        }
    }
}

/// Envelope shape the frontend sees on `LIFECYCLE_UPDATED_TOPIC`. The
/// terminal-id is carried alongside the snapshot so the frontend can
/// demultiplex without a separate per-terminal subscription.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LifecycleUpdateEvent {
    pub terminal_id: String,
    pub snapshot: WorkspaceLifecycleSnapshot,
}

/// Emit `LIFECYCLE_UPDATED_TOPIC` with the supplied snapshot. The
/// notification-driven update path calls this AFTER applying the
/// mutation under `TerminalMeshRegistry::update_snapshot`. The helper
/// is declared here so the wiring change in the notification surface
/// becomes a pure call-site change.
#[allow(dead_code)]
pub fn emit_lifecycle_updated(
    app: &AppHandle,
    terminal_id: Uuid,
    snapshot: &WorkspaceLifecycleSnapshot,
) {
    let envelope = LifecycleUpdateEvent {
        terminal_id: terminal_id.to_string(),
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

/// Classify an `AttentionKind` into the lifecycle Done reason it
/// should produce, if any. The Done queue transitions Running -> Done
/// on the first event that returns `Some(...)`. `PromptWaiting` and
/// `AgentMarker` deliberately return `None` so general agent activity
/// does not flicker tabs into Done — only Completion / NonZeroExit /
/// Disconnect / TaskComplete trigger the transition.
#[allow(dead_code)]
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
            terminal_id: "abc-123".into(),
            snapshot: WorkspaceLifecycleSnapshot::fresh_local(TabKind::Workspace),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"terminalId\":\"abc-123\""), "got {json}");
        assert!(json.contains("\"snapshot\":"), "got {json}");
        assert!(json.contains("\"tabKind\":\"Workspace\""), "got {json}");
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
}
