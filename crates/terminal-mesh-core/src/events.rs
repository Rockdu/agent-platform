//! `TerminalEvent` taxonomy + envelopes + dedup key formatting.
//!
//! Spec: `docs/specs/terminal-events.md` §"TerminalEvent Taxonomy" +
//! §"Event Payload Schema" + §"Dedup Arbiter Behavior".

use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The canonical Terminal Mesh `plugin_id` carried on every envelope.
pub const TERMINAL_MESH_PLUGIN_ID: &str = "terminal_mesh";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TerminalEvent {
    Output {
        #[serde(with = "serde_bytes_compat")]
        bytes: Vec<u8>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Exit {
        code: Option<i32>,
    },
    NeedsAttention {
        payload: NeedsAttentionPayload,
    },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AttentionKind {
    Completion { exit_code: i32 },
    NonZeroExit { exit_code: i32 },
    PromptWaiting,
    AgentMarker {
        summary: Option<String>,
        severity: AttentionSeverity,
    },
    /// Transport-layer disconnect after the remote shell was already
    /// running. Distinct from pre-shell `TransportError` variants,
    /// which surface as create-time errors and never reach the
    /// attention surface.
    Disconnect,
    /// User-program-emitted task-complete marker (OSC sequence parsed
    /// by the host). Carries the verbatim summary so the Done badge
    /// can render it inline. Disjoint from `AgentMarker` so the Done
    /// queue can distinguish "task finished" from general agent
    /// notifications.
    TaskComplete { summary: String },
}

impl AttentionKind {
    /// Canonical kind name used in `dedup_key`. Spec examples:
    /// "Completion", "NonZeroExit", "PromptWaiting", "AgentMarker",
    /// "Disconnect", "TaskComplete".
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Completion { .. } => "Completion",
            Self::NonZeroExit { .. } => "NonZeroExit",
            Self::PromptWaiting => "PromptWaiting",
            Self::AgentMarker { .. } => "AgentMarker",
            Self::Disconnect => "Disconnect",
            Self::TaskComplete { .. } => "TaskComplete",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttentionSeverity {
    Info,
    NeedsConfirm,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalEventEnvelope {
    pub terminal_id: Uuid,
    pub plugin_id: String,
    pub timestamp: SystemTime,
    pub event: TerminalEvent,
}

impl TerminalEventEnvelope {
    pub fn now(terminal_id: Uuid, event: TerminalEvent) -> Self {
        Self {
            terminal_id,
            plugin_id: TERMINAL_MESH_PLUGIN_ID.to_string(),
            timestamp: SystemTime::now(),
            event,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NeedsAttentionPayload {
    pub event_id: Uuid,
    pub dedup_key: String,
    pub kind: AttentionKind,
}

/// Status-channel struct emitted alongside `TerminalEvent` when the
/// ring buffer coalesces an overflow drop. NOT a `NeedsAttention`
/// variant — diagnostic-only per spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BufferTruncated {
    pub terminal_id: Uuid,
    pub plugin_id: String,
    pub timestamp: SystemTime,
    pub bytes_dropped: usize,
}

/// Spec format: `"{plugin_id}:{terminal_id}:{kind_name}"`.
pub fn dedup_key(plugin_id: &str, terminal_id: Uuid, kind: &AttentionKind) -> String {
    format!("{plugin_id}:{terminal_id}:{}", kind.kind_name())
}

mod serde_bytes_compat {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        // Always emit as an array of unsigned bytes so JSON consumers
        // can round-trip without base64; small per-Output payloads keep
        // this practical, and the wire bytes are still raw UTF-8 inside
        // the array for textual output.
        bytes.to_vec().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        Vec::<u8>::deserialize(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_key_matches_spec_example_for_completion() {
        let tid: Uuid = "550e8400-e29b-41d4-a716-446655440000".parse().unwrap();
        let key = dedup_key(
            TERMINAL_MESH_PLUGIN_ID,
            tid,
            &AttentionKind::Completion { exit_code: 0 },
        );
        assert_eq!(
            key,
            "terminal_mesh:550e8400-e29b-41d4-a716-446655440000:Completion"
        );
    }

    #[test]
    fn dedup_key_matches_spec_example_for_nonzero_exit() {
        let tid: Uuid = "550e8400-e29b-41d4-a716-446655440000".parse().unwrap();
        let key = dedup_key(
            TERMINAL_MESH_PLUGIN_ID,
            tid,
            &AttentionKind::NonZeroExit { exit_code: 7 },
        );
        assert_eq!(
            key,
            "terminal_mesh:550e8400-e29b-41d4-a716-446655440000:NonZeroExit"
        );
    }

    #[test]
    fn dedup_key_matches_spec_example_for_agent_marker() {
        let tid: Uuid = "550e8400-e29b-41d4-a716-446655440000".parse().unwrap();
        let key = dedup_key(
            TERMINAL_MESH_PLUGIN_ID,
            tid,
            &AttentionKind::AgentMarker {
                summary: Some("anything".into()),
                severity: AttentionSeverity::Info,
            },
        );
        assert_eq!(
            key,
            "terminal_mesh:550e8400-e29b-41d4-a716-446655440000:AgentMarker"
        );
    }

    #[test]
    fn kind_name_is_stable_across_variants() {
        assert_eq!(AttentionKind::Completion { exit_code: 0 }.kind_name(), "Completion");
        assert_eq!(AttentionKind::NonZeroExit { exit_code: 7 }.kind_name(), "NonZeroExit");
        assert_eq!(AttentionKind::PromptWaiting.kind_name(), "PromptWaiting");
        assert_eq!(
            AttentionKind::AgentMarker {
                summary: None,
                severity: AttentionSeverity::Info,
            }
            .kind_name(),
            "AgentMarker"
        );
        assert_eq!(AttentionKind::Disconnect.kind_name(), "Disconnect");
        assert_eq!(
            AttentionKind::TaskComplete {
                summary: "shipped".into(),
            }
            .kind_name(),
            "TaskComplete"
        );
    }

    #[test]
    fn dedup_key_matches_spec_example_for_disconnect_and_task_complete() {
        let tid: Uuid = "550e8400-e29b-41d4-a716-446655440000".parse().unwrap();
        assert_eq!(
            dedup_key(TERMINAL_MESH_PLUGIN_ID, tid, &AttentionKind::Disconnect),
            "terminal_mesh:550e8400-e29b-41d4-a716-446655440000:Disconnect"
        );
        assert_eq!(
            dedup_key(
                TERMINAL_MESH_PLUGIN_ID,
                tid,
                &AttentionKind::TaskComplete {
                    summary: "anything".into(),
                },
            ),
            "terminal_mesh:550e8400-e29b-41d4-a716-446655440000:TaskComplete"
        );
    }
}
