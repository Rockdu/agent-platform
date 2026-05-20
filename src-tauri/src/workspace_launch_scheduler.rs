//! Concurrency-bounded FIFO queue for auto-launching claude on
//! freshly created Local workspaces.
//!
//! Why this lives separately from `terminal_mesh.rs`:
//! - The cap (default 4) is a global concurrency bound, not a
//!   per-tab or per-workspace property. It must observe state across
//!   all workspaces simultaneously.
//! - Cancellation is needed at workspace-close time without coupling
//!   `workspaces.rs` to terminal-mesh internals.
//! - Tests should be able to stub the spawn side-effect; the
//!   `LaunchExecutor` trait is the seam for that.
//!
//! State machine:
//! - `enqueue` adds an entry. If `launching.len() < cap`, the entry
//!   is moved into `launching` immediately and the executor is
//!   invoked. Otherwise it is pushed to the back of `pending`.
//! - `cancel(workspace_id)` removes a pending entry. It does NOT
//!   interrupt an in-flight launch — `launching` entries are not
//!   cancelable at this layer.
//! - `notify_launch_settled(workspace_id)` removes from `launching`
//!   and drains the head of `pending` (if any) by promoting it to
//!   `launching` and invoking the executor.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, State};
use uuid::Uuid;

use crate::claude_discovery::{ClaudeDiscoveryError, DiscoveryCache};
use crate::terminal_mesh::TerminalMeshRegistry;
use crate::workspace_lifecycle::{
    emit_lifecycle_updated_for_placeholder, TabKind, WorkspaceLifecycleSnapshot,
};
use crate::workspaces::{WorkspaceLocation, WorkspaceRegistry};

pub const DEFAULT_LAUNCH_CAP: usize = 4;

#[derive(Debug, Clone)]
pub struct PendingLaunch {
    pub workspace_id: Uuid,
    pub tab_id: String,
    /// Full workspace location (Local or Remote with optional
    /// container) so the executor can route to `LocalTransport`,
    /// `SshTransport`, or `DockerOverSshTransport` per the Remote-routing contract.
    pub workspace_location: WorkspaceLocation,
    pub claude_argv: Vec<String>,
    /// Provided by callers; the scheduler itself does not read this,
    /// but it preserves enqueue-time provenance so the rail UI or
    /// any future "pending since N ago" diagnostic can use it.
    #[allow(dead_code)]
    pub enqueued_at_unix_ms: i64,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[allow(dead_code)]
pub enum SchedulerState {
    Pending,
    Launching,
    NotPresent,
}

/// Side-effecting hook the scheduler invokes when a launch is ready
/// to fire (either immediately on `enqueue` if a slot is free, or
/// later from `notify_launch_settled` as a slot drains).
///
/// Production impl spawns a tokio task that calls
/// `terminal_mesh::spawn_into_registry` and arranges to call
/// `WorkspaceLaunchScheduler::notify_launch_settled` when the spawn
/// completes. Test impls record invocation order.
pub trait LaunchExecutor: Send + Sync + 'static {
    fn execute(&self, launch: PendingLaunch);
}

#[derive(Clone)]
pub struct WorkspaceLaunchScheduler {
    inner: Arc<Mutex<SchedulerInner>>,
    executor: Arc<dyn LaunchExecutor>,
}

struct SchedulerInner {
    cap: usize,
    pending: VecDeque<PendingLaunch>,
    launching: HashSet<Uuid>,
}

impl WorkspaceLaunchScheduler {
    pub fn new(cap: usize, executor: Arc<dyn LaunchExecutor>) -> Self {
        assert!(cap > 0, "launch cap must be at least 1");
        Self {
            inner: Arc::new(Mutex::new(SchedulerInner {
                cap,
                pending: VecDeque::new(),
                launching: HashSet::new(),
            })),
            executor,
        }
    }

    /// Add a launch request. If a slot is free, the executor fires
    /// immediately and the entry is recorded as `Launching`.
    /// Otherwise the entry is appended to the FIFO pending queue.
    ///
    /// Idempotent on duplicate `workspace_id`: a second enqueue for a
    /// workspace already in `pending` or `launching` is a no-op.
    pub fn enqueue(&self, launch: PendingLaunch) {
        let to_fire = {
            let mut guard = self.inner.lock().expect("scheduler mutex poisoned");
            if guard.launching.contains(&launch.workspace_id)
                || guard
                    .pending
                    .iter()
                    .any(|p| p.workspace_id == launch.workspace_id)
            {
                None
            } else if guard.launching.len() < guard.cap {
                guard.launching.insert(launch.workspace_id);
                Some(launch)
            } else {
                guard.pending.push_back(launch);
                None
            }
        };
        if let Some(l) = to_fire {
            self.executor.execute(l);
        }
    }

    /// Remove a pending entry. In-flight launches are not cancelable
    /// at this layer and return `false`.
    pub fn cancel(&self, workspace_id: Uuid) -> bool {
        let mut guard = self.inner.lock().expect("scheduler mutex poisoned");
        if let Some(pos) = guard
            .pending
            .iter()
            .position(|p| p.workspace_id == workspace_id)
        {
            guard.pending.remove(pos);
            true
        } else {
            false
        }
    }

    #[allow(dead_code)]
    pub fn state(&self, workspace_id: Uuid) -> SchedulerState {
        let guard = self.inner.lock().expect("scheduler mutex poisoned");
        if guard.launching.contains(&workspace_id) {
            SchedulerState::Launching
        } else if guard
            .pending
            .iter()
            .any(|p| p.workspace_id == workspace_id)
        {
            SchedulerState::Pending
        } else {
            SchedulerState::NotPresent
        }
    }

    /// Mark a launch as settled (success or error from the executor
    /// side) and drain the head of `pending` if a slot opened up.
    pub fn notify_launch_settled(&self, workspace_id: Uuid) {
        let to_fire = {
            let mut guard = self.inner.lock().expect("scheduler mutex poisoned");
            let removed = guard.launching.remove(&workspace_id);
            if !removed {
                return;
            }
            if guard.launching.len() < guard.cap {
                if let Some(next) = guard.pending.pop_front() {
                    guard.launching.insert(next.workspace_id);
                    Some(next)
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(l) = to_fire {
            self.executor.execute(l);
        }
    }
}

// ---------------------------------------------------------------------------
// Tauri command surface
// ---------------------------------------------------------------------------

/// Wire-shape errors for the auto-launch enqueue command. Mirrors the
/// other Tauri error DTOs in the repo: tagged-union with camelCase
/// `kind` discriminator.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum AutoLaunchErrorDto {
    /// Workspace's profile has `auto_launch_claude = false` — the
    /// user opted out, so silently skip the enqueue.
    AutoLaunchDisabled { workspace_id: String },
    /// Workspace location is Remote — auto-launch is Local-only in
    /// v1 (remote claude runs as a normal remote process without
    /// this app's MCP plugins).
    #[allow(dead_code)]
    RemoteWorkspaceNotEligible { workspace_id: String },
    /// Workspace id does not parse or no record matches.
    WorkspaceNotFound { workspace_id: String },
    /// `DiscoveryCache` is not in the Ready state (claude binary not
    /// found, not executable, or version-probe failed). The frontend
    /// MUST surface this to the user instead of silently dropping the
    /// auto-launch — the spec forbids the silent fallback path.
    ClaudeDiscoveryNotReady { discovery_kind: String, message: String },
}

/// Pure logic for `request_workspace_auto_launch`. Decides whether to
/// enqueue based on workspace existence + location + auto-launch
/// flag. Extracted from the Tauri command so unit tests can exercise
/// the missing/remote/disabled rejection paths without a Tauri app
/// harness.
pub fn try_build_pending_launch(
    workspace_id_str: &str,
    tab_id: String,
    workspace: Option<&crate::workspaces::WorkspaceRecord>,
    enqueued_at_unix_ms: i64,
) -> Result<PendingLaunch, AutoLaunchErrorDto> {
    let record = workspace.ok_or_else(|| AutoLaunchErrorDto::WorkspaceNotFound {
        workspace_id: workspace_id_str.to_string(),
    })?;
    // The frontend fires `requestWorkspaceAutoLaunch` fire-and-
    // forget; the user can adopt-then-close a workspace before
    // this command reaches the backend. By the time we resolve
    // the record, `close_workspace` may have already cleared
    // `open_tab_id` (or a different tab may have adopted the
    // workspace since). Without this check the enqueue would
    // fire for a closed tab and the executor would spawn an
    // orphan claude process. Treat both "no open tab" and
    // "different tab adopted" as `WorkspaceNotFound` — the
    // user-visible meaning is "the tab you asked about is no
    // longer here."
    if record.open_tab_id.as_deref() != Some(tab_id.as_str()) {
        return Err(AutoLaunchErrorDto::WorkspaceNotFound {
            workspace_id: workspace_id_str.to_string(),
        });
    }
    if !record.profile.auto_launch_claude {
        return Err(AutoLaunchErrorDto::AutoLaunchDisabled {
            workspace_id: workspace_id_str.to_string(),
        });
    }
    // Remote workspaces are eligible now that the executor
    // routes them through SshTransport / DockerOverSshTransport.
    // The Remote-eligibility check is gone; the `RemoteWorkspaceNot
    // Eligible` DTO variant is retained for future per-workspace
    // policy gating but is no longer fired here.
    Ok(PendingLaunch {
        workspace_id: record.workspace_id,
        tab_id,
        workspace_location: record.location.clone(),
        claude_argv: record.profile.claude_argv.clone(),
        enqueued_at_unix_ms,
    })
}

/// Per-workspace transport routing decision used by
/// `RealLaunchExecutor` to pick the right `Transport` impl. Pure
/// function so the routing rules can be unit-tested without a
/// Tauri harness.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TransportRouting {
    /// `LocalTransport` for `WorkspaceLocation::Local`.
    Local,
    /// `SshTransport` for `Remote` with no container.
    Ssh,
    /// `DockerOverSshTransport` for `Remote` with `container: Some(...)`.
    DockerOverSsh,
}

pub fn select_transport_kind_for(location: &WorkspaceLocation) -> TransportRouting {
    match location {
        WorkspaceLocation::Local { .. } => TransportRouting::Local,
        WorkspaceLocation::Remote {
            container: Some(_),
            ..
        } => TransportRouting::DockerOverSsh,
        WorkspaceLocation::Remote { container: None, .. } => TransportRouting::Ssh,
    }
}

/// Pure helper: should the local `DiscoveryCache` be required to
/// be `Ready` before an auto-launch is enqueued? Yes for Local
/// routing (the spec.command is the host-resolved absolute path).
/// No for Remote routing — the executor invokes the bare `claude`
/// command on the remote host and the local discovery path is
/// meaningless there, so blocking on local discovery would deny
/// users with claude installed only on the remote machine.
pub fn should_block_auto_launch_on_local_discovery(routing: TransportRouting) -> bool {
    matches!(routing, TransportRouting::Local)
}

/// Pure helper: should `close_workspace` drop a close-during-
/// launch tombstone? Only when the workspace is actually
/// `Launching` at close time. `scheduler.cancel(id)` returns
/// false for BOTH `Launching` (the target case) AND `NotPresent`
/// (already settled / never enqueued), so using `!was_pending`
/// as the gate over-marks the tombstone for normal closes; the
/// next auto-launch for the same workspace would then consume
/// the stale tombstone and immediately shut down its terminal.
pub fn should_mark_close_during_launch(state: SchedulerState) -> bool {
    matches!(state, SchedulerState::Launching)
}

/// Pure helper: claude discovery must be in the Ready state before a
/// launch can be enqueued. Extracted from the Tauri command so unit
/// tests can exercise the rejection paths without a Tauri harness.
pub fn try_check_discovery_ready(
    snapshot: Option<Result<crate::claude_discovery::ClaudePathRecord, ClaudeDiscoveryError>>,
) -> Result<(), AutoLaunchErrorDto> {
    match snapshot {
        Some(Ok(_)) => Ok(()),
        Some(Err(err)) => Err(AutoLaunchErrorDto::ClaudeDiscoveryNotReady {
            discovery_kind: discovery_error_kind(&err).into(),
            message: err.to_string(),
        }),
        None => Err(AutoLaunchErrorDto::ClaudeDiscoveryNotReady {
            discovery_kind: "not_run".into(),
            message: "claude discovery has not run yet".into(),
        }),
    }
}

fn discovery_error_kind(err: &ClaudeDiscoveryError) -> &'static str {
    match err {
        ClaudeDiscoveryError::ClaudeNotFound { .. } => "claude_not_found",
        ClaudeDiscoveryError::NotExecutable { .. } => "not_executable",
        ClaudeDiscoveryError::VersionProbeFailed { .. } => "version_probe_failed",
        ClaudeDiscoveryError::Io { .. } => "io",
    }
}

#[tauri::command]
pub async fn request_workspace_auto_launch(
    app: AppHandle,
    workspace_id: String,
    tab_id: String,
    registry: State<'_, WorkspaceRegistry>,
    scheduler: State<'_, WorkspaceLaunchScheduler>,
    discovery: State<'_, DiscoveryCache>,
    terminal_registry: State<'_, TerminalMeshRegistry>,
) -> Result<(), AutoLaunchErrorDto> {
    // Resolve the workspace BEFORE the local discovery check so we
    // can skip the check for Remote routing. Remote spawns use the
    // bare `claude` command on the remote host (the local
    // discovery path is meaningless on the remote side), so a user
    // with claude installed only on the remote machine must still
    // be able to auto-launch a Remote workspace even when the
    // local `DiscoveryCache` is not Ready.
    let workspace_uuid = Uuid::parse_str(&workspace_id).ok();
    let record = workspace_uuid.and_then(|id| registry.find_by_id(id));
    let routing_for_discovery_gate = record
        .as_ref()
        .map(|r| select_transport_kind_for(&r.location))
        .unwrap_or(TransportRouting::Local);
    if should_block_auto_launch_on_local_discovery(routing_for_discovery_gate) {
        try_check_discovery_ready(discovery.snapshot())?;
    }
    let launch = try_build_pending_launch(
        &workspace_id,
        tab_id.clone(),
        record.as_ref(),
        now_unix_ms(),
    )?;
    let workspace_uuid = launch.workspace_id;
    scheduler.enqueue(launch);
    // If the workspace landed in Pending (cap saturated), install a
    // tab-id-keyed placeholder snapshot so the rail row surfaces
    // `pending_launch=true` BEFORE the terminal is spawned. The
    // executor's spawn path drains the placeholder via
    // `record() -> take_pending_snapshot_for_tab`.
    if scheduler.state(workspace_uuid) == SchedulerState::Pending {
        let placeholder_kind = record
            .as_ref()
            .map(|r| crate::terminal_mesh::transport_kind_for_location(&r.location))
            .unwrap_or(crate::workspace_lifecycle::TransportKind::Local);
        let mut placeholder = WorkspaceLifecycleSnapshot::fresh_for_workspace_with_kind(
            TabKind::Workspace,
            Some(workspace_uuid.to_string()),
            placeholder_kind,
        );
        placeholder.pending_launch = true;
        terminal_registry.set_pending_for_tab(tab_id.clone(), placeholder.clone());
        // Emit with `terminal_id = None` + `tab_id = Some(...)` so
        // the frontend hook updates the snapshot map only and does
        // NOT mark the tab as resolved in `terminalIdByTabId`. The
        // later real-terminal event from `RealLaunchExecutor`'s
        // post-spawn `on_pending_launch_changed(..., false)` then
        // correctly populates both maps.
        emit_lifecycle_updated_for_placeholder(&app, &tab_id, &placeholder);
    }
    Ok(())
}

fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct CountingExecutor {
        invocations: StdMutex<Vec<Uuid>>,
    }

    impl CountingExecutor {
        fn invocations(&self) -> Vec<Uuid> {
            self.invocations.lock().unwrap().clone()
        }
    }

    impl LaunchExecutor for CountingExecutor {
        fn execute(&self, launch: PendingLaunch) {
            self.invocations.lock().unwrap().push(launch.workspace_id);
        }
    }

    fn launch(id: Uuid) -> PendingLaunch {
        PendingLaunch {
            workspace_id: id,
            tab_id: id.to_string(),
            workspace_location: WorkspaceLocation::Local {
                path: PathBuf::from("/tmp/ws"),
            },
            claude_argv: vec!["--dangerously-skip-permissions".into()],
            enqueued_at_unix_ms: 0,
        }
    }

    #[test]
    fn scheduler_enqueue_within_cap_launches_immediately() {
        let exec = Arc::new(CountingExecutor::default());
        let sched = WorkspaceLaunchScheduler::new(4, exec.clone());
        let id = Uuid::new_v4();
        sched.enqueue(launch(id));
        assert_eq!(exec.invocations(), vec![id]);
        assert_eq!(sched.state(id), SchedulerState::Launching);
    }

    #[test]
    fn scheduler_enqueue_beyond_cap_queues_pending() {
        let exec = Arc::new(CountingExecutor::default());
        let sched = WorkspaceLaunchScheduler::new(2, exec.clone());
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        for id in &ids {
            sched.enqueue(launch(*id));
        }
        // Only the first two fire; the other three are Pending FIFO.
        assert_eq!(exec.invocations(), vec![ids[0], ids[1]]);
        assert_eq!(sched.state(ids[0]), SchedulerState::Launching);
        assert_eq!(sched.state(ids[1]), SchedulerState::Launching);
        assert_eq!(sched.state(ids[2]), SchedulerState::Pending);
        assert_eq!(sched.state(ids[3]), SchedulerState::Pending);
        assert_eq!(sched.state(ids[4]), SchedulerState::Pending);
    }

    #[test]
    fn scheduler_notify_launch_settled_drains_next_pending() {
        let exec = Arc::new(CountingExecutor::default());
        let sched = WorkspaceLaunchScheduler::new(2, exec.clone());
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        for id in &ids {
            sched.enqueue(launch(*id));
        }
        sched.notify_launch_settled(ids[0]);
        // ids[0] cleared from launching; ids[2] (head of pending) promoted.
        assert_eq!(exec.invocations(), vec![ids[0], ids[1], ids[2]]);
        assert_eq!(sched.state(ids[0]), SchedulerState::NotPresent);
        assert_eq!(sched.state(ids[2]), SchedulerState::Launching);
        assert_eq!(sched.state(ids[3]), SchedulerState::Pending);
    }

    #[test]
    fn scheduler_cancel_pending_removes_entry_and_returns_true() {
        let exec = Arc::new(CountingExecutor::default());
        let sched = WorkspaceLaunchScheduler::new(1, exec.clone());
        let ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
        for id in &ids {
            sched.enqueue(launch(*id));
        }
        // ids[0] launching; ids[1], ids[2] pending. Cancel the middle.
        assert!(sched.cancel(ids[1]));
        assert_eq!(sched.state(ids[1]), SchedulerState::NotPresent);
        // Cancellation does NOT consume a slot — ids[2] stays Pending.
        assert_eq!(sched.state(ids[2]), SchedulerState::Pending);
        // After ids[0] settles, ids[2] promotes (skipping the canceled one).
        sched.notify_launch_settled(ids[0]);
        assert_eq!(sched.state(ids[2]), SchedulerState::Launching);
    }

    #[test]
    fn scheduler_cancel_launching_returns_false_and_keeps_state() {
        let exec = Arc::new(CountingExecutor::default());
        let sched = WorkspaceLaunchScheduler::new(2, exec.clone());
        let id = Uuid::new_v4();
        sched.enqueue(launch(id));
        assert!(!sched.cancel(id));
        assert_eq!(sched.state(id), SchedulerState::Launching);
    }

    #[test]
    fn scheduler_cancel_absent_returns_false() {
        let exec = Arc::new(CountingExecutor::default());
        let sched = WorkspaceLaunchScheduler::new(4, exec.clone());
        assert!(!sched.cancel(Uuid::new_v4()));
    }

    #[test]
    fn scheduler_enqueue_duplicate_is_noop() {
        let exec = Arc::new(CountingExecutor::default());
        let sched = WorkspaceLaunchScheduler::new(2, exec.clone());
        let id = Uuid::new_v4();
        sched.enqueue(launch(id));
        sched.enqueue(launch(id));
        assert_eq!(exec.invocations(), vec![id]);
        // Duplicate while pending: enqueue a second workspace first to
        // fill the slot, then re-enqueue the pending one.
        let other = Uuid::new_v4();
        let third = Uuid::new_v4();
        sched.enqueue(launch(other));
        sched.enqueue(launch(third));
        assert_eq!(sched.state(third), SchedulerState::Pending);
        sched.enqueue(launch(third));
        assert_eq!(exec.invocations(), vec![id, other]);
    }

    #[test]
    fn scheduler_fifo_order_preserved_across_multiple_settles() {
        let exec = Arc::new(CountingExecutor::default());
        let sched = WorkspaceLaunchScheduler::new(2, exec.clone());
        let ids: Vec<Uuid> = (0..6).map(|_| Uuid::new_v4()).collect();
        for id in &ids {
            sched.enqueue(launch(*id));
        }
        // Settle the launching pair, then settle the next pair, etc.
        sched.notify_launch_settled(ids[0]);
        sched.notify_launch_settled(ids[1]);
        sched.notify_launch_settled(ids[2]);
        sched.notify_launch_settled(ids[3]);
        assert_eq!(
            exec.invocations(),
            vec![ids[0], ids[1], ids[2], ids[3], ids[4], ids[5]]
        );
    }

    #[test]
    fn scheduler_notify_launch_settled_for_absent_id_is_noop() {
        let exec = Arc::new(CountingExecutor::default());
        let sched = WorkspaceLaunchScheduler::new(2, exec.clone());
        let id = Uuid::new_v4();
        sched.notify_launch_settled(id);
        assert_eq!(exec.invocations(), Vec::<Uuid>::new());
    }

    #[test]
    fn try_build_pending_launch_rejects_when_workspace_missing() {
        let err = try_build_pending_launch("not-a-real-id", "tab-1".into(), None, 0)
            .unwrap_err();
        match err {
            AutoLaunchErrorDto::WorkspaceNotFound { workspace_id } => {
                assert_eq!(workspace_id, "not-a-real-id");
            }
            other => panic!("expected WorkspaceNotFound; got {other:?}"),
        }
    }

    /// Remote workspaces are now eligible for auto-launch;
    /// the scheduler's `RealLaunchExecutor` routes them through
    /// `SshTransport` / `DockerOverSshTransport` per the
    /// `workspace_location` field on the resulting `PendingLaunch`.
    #[test]
    fn try_build_pending_launch_accepts_remote_ssh_workspace() {
        use crate::workspaces::{
            ContainerLocation, SshLocation, WorkspaceLocation, WorkspaceProfile, WorkspaceRecord,
        };
        let id = Uuid::new_v4();
        let record = WorkspaceRecord {
            workspace_id: id,
            name: "remote".into(),
            location: WorkspaceLocation::Remote {
                ssh: SshLocation {
                    user: None,
                    host: "host.example".into(),
                    port: None,
                    canonical_remote_path: "/srv/work".into(),
                },
                container: None as Option<ContainerLocation>,
            },
            profile: WorkspaceProfile::default_local(),
            created_at: "2026-01-01T00:00:00Z".into(),
            last_used_at: "2026-01-01T00:00:00Z".into(),
            open_tab_id: Some("tab-r".into()),
            conversation_rounds_count: 0,
        };
        let launch = try_build_pending_launch(
            &id.to_string(),
            "tab-r".into(),
            Some(&record),
            0,
        )
        .expect("remote workspace must be accepted");
        assert!(matches!(
            launch.workspace_location,
            WorkspaceLocation::Remote { .. }
        ));
    }

    #[test]
    fn select_transport_kind_for_local_returns_local() {
        use crate::workspaces::WorkspaceLocation;
        let loc = WorkspaceLocation::Local {
            path: std::path::PathBuf::from("/tmp/x"),
        };
        assert_eq!(select_transport_kind_for(&loc), TransportRouting::Local);
    }

    #[test]
    fn select_transport_kind_for_remote_without_container_returns_ssh() {
        use crate::workspaces::{SshLocation, WorkspaceLocation};
        let loc = WorkspaceLocation::Remote {
            ssh: SshLocation {
                user: None,
                host: "h".into(),
                port: None,
                canonical_remote_path: "/srv".into(),
            },
            container: None,
        };
        assert_eq!(select_transport_kind_for(&loc), TransportRouting::Ssh);
    }

    #[test]
    fn select_transport_kind_for_remote_with_container_returns_docker_over_ssh() {
        use crate::workspaces::{ContainerLocation, SshLocation, WorkspaceLocation};
        let loc = WorkspaceLocation::Remote {
            ssh: SshLocation {
                user: None,
                host: "h".into(),
                port: None,
                canonical_remote_path: "/srv".into(),
            },
            container: Some(ContainerLocation {
                container_id: "c".into(),
                cwd_in_container: None,
            }),
        };
        assert_eq!(
            select_transport_kind_for(&loc),
            TransportRouting::DockerOverSsh
        );
    }

    #[test]
    fn try_build_pending_launch_rejects_when_auto_launch_disabled() {
        use crate::workspaces::{
            WorkspaceLocation, WorkspaceProfile, WorkspaceRecord,
        };
        let id = Uuid::new_v4();
        let record = WorkspaceRecord {
            workspace_id: id,
            name: "no-auto".into(),
            location: WorkspaceLocation::Local {
                path: PathBuf::from("/tmp/ws"),
            },
            profile: WorkspaceProfile {
                auto_launch_claude: false,
                claude_argv: vec![],
                stashed: false,
                restore_on_startup: false,
            },
            created_at: "2026-01-01T00:00:00Z".into(),
            last_used_at: "2026-01-01T00:00:00Z".into(),
            open_tab_id: Some("tab-x".into()),
            conversation_rounds_count: 0,
        };
        let err = try_build_pending_launch(
            &id.to_string(),
            "tab-x".into(),
            Some(&record),
            0,
        )
        .unwrap_err();
        match err {
            AutoLaunchErrorDto::AutoLaunchDisabled { workspace_id } => {
                assert_eq!(workspace_id, id.to_string());
            }
            other => panic!("expected AutoLaunchDisabled; got {other:?}"),
        }
    }

    /// `close_workspace` may only mark the close-during-launch
    /// tombstone when the scheduler reports `Launching`. The
    /// `Pending` case is already handled by the synchronous
    /// `cancel` call; the `NotPresent` case (already settled or
    /// never enqueued) MUST NOT mark, otherwise the next auto-
    /// launch for the same workspace would consume a stale
    /// tombstone and have its terminal reaped on spawn.
    #[test]
    fn should_mark_close_during_launch_only_for_launching_state() {
        assert!(should_mark_close_during_launch(SchedulerState::Launching));
        assert!(!should_mark_close_during_launch(SchedulerState::Pending));
        assert!(!should_mark_close_during_launch(SchedulerState::NotPresent));
    }

    /// Local routing requires the host's `DiscoveryCache` to be
    /// `Ready` so the executor has an absolute `claude` path to
    /// hand to `LocalTransport`. Remote routings (SSH, Docker-
    /// over-SSH) execute the bare `claude` command on the remote
    /// host; the local discovery path is meaningless there. The
    /// pure-helper gate ensures users with claude installed only
    /// on the remote machine can still auto-launch Remote
    /// workspaces.
    #[test]
    fn should_block_auto_launch_on_local_discovery_routing_matrix() {
        assert!(should_block_auto_launch_on_local_discovery(
            TransportRouting::Local
        ));
        assert!(!should_block_auto_launch_on_local_discovery(
            TransportRouting::Ssh
        ));
        assert!(!should_block_auto_launch_on_local_discovery(
            TransportRouting::DockerOverSsh
        ));
    }

    #[test]
    fn try_check_discovery_ready_passes_for_ok_snapshot() {
        use crate::claude_discovery::ClaudePathRecord;
        let snap = Some(Ok(ClaudePathRecord {
            path: PathBuf::from("/usr/local/bin/claude"),
            version: Some("9.9.9".into()),
            discovered_at: "2026-01-01T00:00:00Z".into(),
        }));
        try_check_discovery_ready(snap).expect("ready state must pass");
    }

    #[test]
    fn try_check_discovery_ready_fails_for_absent_snapshot() {
        let err = try_check_discovery_ready(None).unwrap_err();
        match err {
            AutoLaunchErrorDto::ClaudeDiscoveryNotReady { discovery_kind, message } => {
                assert_eq!(discovery_kind, "not_run");
                assert!(
                    message.contains("not run"),
                    "message should explain not-run state; got: {message}"
                );
            }
            other => panic!("expected ClaudeDiscoveryNotReady; got {other:?}"),
        }
    }

    #[test]
    fn try_check_discovery_ready_fails_for_not_found_error() {
        use crate::claude_discovery::ClaudeDiscoveryError;
        let snap = Some(Err(ClaudeDiscoveryError::ClaudeNotFound {
            probed: vec!["/usr/local/bin/claude".into()],
        }));
        let err = try_check_discovery_ready(snap).unwrap_err();
        match err {
            AutoLaunchErrorDto::ClaudeDiscoveryNotReady { discovery_kind, .. } => {
                assert_eq!(discovery_kind, "claude_not_found");
            }
            other => panic!("expected ClaudeDiscoveryNotReady; got {other:?}"),
        }
    }

    #[test]
    fn try_build_pending_launch_succeeds_for_local_with_auto_launch_enabled() {
        use crate::workspaces::{
            WorkspaceLocation, WorkspaceProfile, WorkspaceRecord,
        };
        let id = Uuid::new_v4();
        let record = WorkspaceRecord {
            workspace_id: id,
            name: "happy".into(),
            location: WorkspaceLocation::Local {
                path: PathBuf::from("/tmp/happy-ws"),
            },
            profile: WorkspaceProfile::default_local(),
            created_at: "2026-01-01T00:00:00Z".into(),
            last_used_at: "2026-01-01T00:00:00Z".into(),
            open_tab_id: Some("tab-h".into()),
            conversation_rounds_count: 0,
        };
        let launch = try_build_pending_launch(
            &id.to_string(),
            "tab-h".into(),
            Some(&record),
            12345,
        )
        .expect("eligible");
        assert_eq!(launch.workspace_id, id);
        assert_eq!(launch.tab_id, "tab-h");
        match &launch.workspace_location {
            crate::workspaces::WorkspaceLocation::Local { path } => {
                assert_eq!(path, &PathBuf::from("/tmp/happy-ws"));
            }
            other => panic!("expected Local location; got {other:?}"),
        }
        assert_eq!(launch.claude_argv, vec!["--dangerously-skip-permissions"]);
        assert_eq!(launch.enqueued_at_unix_ms, 12345);
    }

    /// The frontend fires `requestWorkspaceAutoLaunch`
    /// fire-and-forget; a user can adopt-then-close before the
    /// backend handles the command. By the time we resolve the
    /// record, `open_tab_id` may be cleared. The enqueue must
    /// not fire for a closed tab — surface `WorkspaceNotFound`
    /// so the executor never spawns an orphan claude.
    #[test]
    fn try_build_pending_launch_rejects_when_workspace_is_closed() {
        use crate::workspaces::{WorkspaceLocation, WorkspaceProfile, WorkspaceRecord};
        let id = Uuid::new_v4();
        let record = WorkspaceRecord {
            workspace_id: id,
            name: "closed".into(),
            location: WorkspaceLocation::Local {
                path: PathBuf::from("/tmp/closed"),
            },
            profile: WorkspaceProfile::default_local(),
            created_at: "2026-01-01T00:00:00Z".into(),
            last_used_at: "2026-01-01T00:00:00Z".into(),
            open_tab_id: None,
            conversation_rounds_count: 0,
        };
        let err = try_build_pending_launch(
            &id.to_string(),
            "tab-closed".into(),
            Some(&record),
            0,
        )
        .expect_err("closed workspace must reject the auto-launch");
        match err {
            AutoLaunchErrorDto::WorkspaceNotFound { workspace_id } => {
                assert_eq!(workspace_id, id.to_string());
            }
            other => panic!("expected WorkspaceNotFound; got {other:?}"),
        }
    }

    /// Same workspace, different tab: the workspace is open but
    /// bound to a different tab id than the caller's. Treat as
    /// `WorkspaceNotFound` so the stale request from the closed
    /// tab does not enqueue against the currently-open tab.
    #[test]
    fn try_build_pending_launch_rejects_when_tab_id_does_not_match_open_tab_id() {
        use crate::workspaces::{WorkspaceLocation, WorkspaceProfile, WorkspaceRecord};
        let id = Uuid::new_v4();
        let record = WorkspaceRecord {
            workspace_id: id,
            name: "different-tab".into(),
            location: WorkspaceLocation::Local {
                path: PathBuf::from("/tmp/dt"),
            },
            profile: WorkspaceProfile::default_local(),
            created_at: "2026-01-01T00:00:00Z".into(),
            last_used_at: "2026-01-01T00:00:00Z".into(),
            open_tab_id: Some("tab-current".into()),
            conversation_rounds_count: 0,
        };
        let err = try_build_pending_launch(
            &id.to_string(),
            "tab-stale".into(),
            Some(&record),
            0,
        )
        .expect_err("stale tab_id must reject the auto-launch");
        match err {
            AutoLaunchErrorDto::WorkspaceNotFound { workspace_id } => {
                assert_eq!(workspace_id, id.to_string());
            }
            other => panic!("expected WorkspaceNotFound; got {other:?}"),
        }
    }
}
