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
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::State;
use uuid::Uuid;

use crate::workspaces::{WorkspaceLocation, WorkspaceRegistry};

pub const DEFAULT_LAUNCH_CAP: usize = 4;

#[derive(Debug, Clone)]
pub struct PendingLaunch {
    pub workspace_id: Uuid,
    pub tab_id: String,
    pub local_path: PathBuf,
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
    RemoteWorkspaceNotEligible { workspace_id: String },
    /// Workspace id does not parse or no record matches.
    WorkspaceNotFound { workspace_id: String },
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
    let local_path = match &record.location {
        WorkspaceLocation::Local { path } => path.clone(),
        WorkspaceLocation::Remote { .. } => {
            return Err(AutoLaunchErrorDto::RemoteWorkspaceNotEligible {
                workspace_id: workspace_id_str.to_string(),
            });
        }
    };
    if !record.profile.auto_launch_claude {
        return Err(AutoLaunchErrorDto::AutoLaunchDisabled {
            workspace_id: workspace_id_str.to_string(),
        });
    }
    Ok(PendingLaunch {
        workspace_id: record.workspace_id,
        tab_id,
        local_path,
        claude_argv: record.profile.claude_argv.clone(),
        enqueued_at_unix_ms,
    })
}

#[tauri::command]
pub fn request_workspace_auto_launch(
    workspace_id: String,
    tab_id: String,
    registry: State<'_, WorkspaceRegistry>,
    scheduler: State<'_, WorkspaceLaunchScheduler>,
) -> Result<(), AutoLaunchErrorDto> {
    let record = match Uuid::parse_str(&workspace_id).ok() {
        Some(id) => registry.find_by_id(id),
        None => None,
    };
    let launch = try_build_pending_launch(
        &workspace_id,
        tab_id,
        record.as_ref(),
        now_unix_ms(),
    )?;
    scheduler.enqueue(launch);
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
            local_path: PathBuf::from("/tmp/ws"),
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

    #[test]
    fn try_build_pending_launch_rejects_remote_workspace() {
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
            open_tab_id: None,
            conversation_rounds_count: 0,
        };
        let err = try_build_pending_launch(
            &id.to_string(),
            "tab-r".into(),
            Some(&record),
            0,
        )
        .unwrap_err();
        match err {
            AutoLaunchErrorDto::RemoteWorkspaceNotEligible { workspace_id } => {
                assert_eq!(workspace_id, id.to_string());
            }
            other => panic!("expected RemoteWorkspaceNotEligible; got {other:?}"),
        }
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
            },
            created_at: "2026-01-01T00:00:00Z".into(),
            last_used_at: "2026-01-01T00:00:00Z".into(),
            open_tab_id: None,
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
            open_tab_id: None,
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
        assert_eq!(launch.local_path, PathBuf::from("/tmp/happy-ws"));
        assert_eq!(launch.claude_argv, vec!["--dangerously-skip-permissions"]);
        assert_eq!(launch.enqueued_at_unix_ms, 12345);
    }
}
