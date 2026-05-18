//! Terminal Mesh core: pure Rust types + actor that hosts a
//! [`portable-pty`]-backed child process, owns a 1 MB UTF-8/ANSI-safe
//! ring buffer, decodes OSC `1337;AgentTask` agent markers, debounces
//! prompt-waiting heuristics, and dedups NeedsAttention events.
//!
//! Spec: `docs/specs/terminal-events.md`. Target ACs: AC-4.1 + AC-4.2.

pub mod actor;
pub mod ansi_scanner;
pub mod dedup_arbiter;
pub mod events;
pub mod osc_agent_marker;
pub mod prompt_detector;
pub mod ring_buffer;
pub mod transport;
pub mod transport_docker_ssh;
pub mod transport_ssh;

pub use actor::{ActorCommand, ActorError, TerminalActor, TerminalHandle, TerminalSpec};
pub use ansi_scanner::{AnsiScanner, AnsiState};
pub use dedup_arbiter::{ArbiterDecision, DedupArbiter};
pub use events::{
    dedup_key, AttentionKind, AttentionSeverity, BufferTruncated, NeedsAttentionPayload,
    TerminalEvent, TerminalEventEnvelope, TERMINAL_MESH_PLUGIN_ID,
};
pub use osc_agent_marker::{
    AgentMarkerEvent, OscAttentionEvent, OscAttentionParser, OSC_MAX_PAYLOAD_BYTES,
};
pub use prompt_detector::PromptDetector;
pub use ring_buffer::{BufferTruncatedInfo, RingBuffer, RING_CAPACITY_BYTES};
pub use transport::{
    ContainerLocation, DisconnectReason, LocalTransport, PathBufOrRemote,
    PtySize as TransportPtySize, ShellCommand, ShutdownMode, Transport, TransportError,
    TransportExitStatus, TransportOutputStream, TransportResizeHandle, TransportSession,
    TransportShutdownHandle, TransportSpawnRequest, TransportStdinSink, WorkspaceLocation,
};
pub use transport_ssh::{
    build_sentinel_wrapper_script, build_ssh_argv, classify_phase_a_failure,
    cleanup_stale_master_sockets, init_control_master_dir, ssh_control_path_for, SentinelEvent,
    SentinelParser, SshLocation, SshTransport, SshTransportSession, REMOTE_PATH_INVALID_EXIT_CODE,
    SSH_OSC_EXIT_STATUS_PREFIX, SSH_OSC_NUMBER, SSH_OSC_SHELL_STARTED,
};
pub use transport_docker_ssh::{
    compose_docker_cleanup_command, compose_docker_remote_command, DockerLocation,
    DockerOverSshTransport, DockerOverSshTransportSession,
};
