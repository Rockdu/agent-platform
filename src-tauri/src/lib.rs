//! Tauri host library entry point.
//!
//! Holds the `tauri::Builder` configuration. The binary at `src/main.rs` and
//! the mobile entry point both call `run()`.

mod bootstrap;
mod builtin_plugins;
mod claude_discovery;
mod dev_diagnostics;
mod dispatcher;
mod host_rpc;
mod notification;
mod generated;
mod ide_handoff;
mod logging;
mod mcp_config;
mod orchestrator;
mod plugin_sqlite;
mod secrets;
mod sidecar_manager;
mod terminal_mesh;
mod workspace_launch_scheduler;
mod workspace_lifecycle;
mod workspaces;

use bootstrap::{BootstrapError, BootstrapPaths};
use claude_discovery::DiscoveryCache;
use dispatcher::MountRegistry;
use mcp_config::McpConfigRegistry;
use plugin_sqlite::{run_all_plugin_migrations_at_bootstrap, PluginMigrationState};
use secrets::{AccessTokenCache, SecretsErrorDto, SetupMarker, SetupStatus};
use sidecar_manager::{SidecarConfig, SidecarManager};
use ide_handoff::IdePreferenceStore;
use orchestrator::{OrchestratorBootstrap, OrchestratorState};
use terminal_mesh::TerminalMeshRegistry;
use workspace_launch_scheduler::{
    LaunchExecutor, PendingLaunch, WorkspaceLaunchScheduler, DEFAULT_LAUNCH_CAP,
};
use workspace_lifecycle::{on_pending_launch_changed, TabKind};
use workspaces::WorkspaceRegistry;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::OnceLock;
use tauri::Manager;

const STRONGHOLD_DIRNAME: &str = "stronghold-state";

/// Cached result of the first-run bootstrap. Populated by the setup hook;
/// consumed by the `bootstrap_status` Tauri command.
static BOOTSTRAP_RESULT: OnceLock<Result<BootstrapPaths, BootstrapError>> = OnceLock::new();

/// Serializable shape returned to the frontend on bootstrap failure. Preserves
/// the variant tag and offending path (when applicable) so the UI can render
/// them structurally instead of regex-matching a stringified message.
#[derive(Debug, Clone, Serialize)]
pub struct BootstrapErrorDto {
    /// Discriminant: "no_home_dir" | "no_app_data_dir" | "create_dir" | "not_run".
    pub kind: String,
    /// Offending path for filesystem errors; `None` for env-lookup failures.
    pub path: Option<String>,
    /// Human-readable message (from `thiserror`); the frontend translates if needed.
    pub message: String,
}

impl From<&BootstrapError> for BootstrapErrorDto {
    fn from(err: &BootstrapError) -> Self {
        match err {
            BootstrapError::NoHomeDir => Self {
                kind: "no_home_dir".into(),
                path: None,
                message: err.to_string(),
            },
            BootstrapError::NoAppDataDir => Self {
                kind: "no_app_data_dir".into(),
                path: None,
                message: err.to_string(),
            },
            BootstrapError::CreateDir { path, .. } => Self {
                kind: "create_dir".into(),
                path: Some(path.display().to_string()),
                message: err.to_string(),
            },
        }
    }
}

/// Accessor consumed by `plugin_sqlite::retry_plugin_migration` so it can
/// reuse the bootstrap-computed `plugins_root` path without re-deriving it.
/// Returns `None` if bootstrap has not run or failed.
pub(crate) fn bootstrap_status_plugins_root() -> Option<PathBuf> {
    BOOTSTRAP_RESULT
        .get()
        .and_then(|r| r.as_ref().ok())
        .map(|p| p.plugins_root.clone())
}

/// Stronghold state lives at `${APP_DATA}/stronghold-state/` per the plugin
/// contract spec. Derived from the bootstrap-cached app-data root so the
/// secrets module + stronghold Tauri commands share one source of truth.
fn stronghold_root() -> Option<PathBuf> {
    BOOTSTRAP_RESULT
        .get()
        .and_then(|r| r.as_ref().ok())
        .map(|paths| {
            paths
                .plugins_root
                .parent()
                .map(|app_data| app_data.join(STRONGHOLD_DIRNAME))
                .unwrap_or_else(|| PathBuf::from(STRONGHOLD_DIRNAME))
        })
}

// ---------------------------------------------------------------------------
// Stronghold setup Tauri commands (AC-5.4 host)
// ---------------------------------------------------------------------------

fn require_stronghold_root() -> Result<PathBuf, SecretsErrorDto> {
    stronghold_root().ok_or_else(|| SecretsErrorDto {
        kind: "io".into(),
        message: "bootstrap paths not available; stronghold root cannot be resolved".into(),
        plugin_id: None,
        account_id: None,
        secret_name: None,
        setup_status: None,
    })
}

#[tauri::command]
fn stronghold_setup_status() -> Result<SetupStatus, SecretsErrorDto> {
    let root = require_stronghold_root()?;
    secrets::read_setup_status(&root).map_err(|err| SecretsErrorDto::from(&err))
}

#[tauri::command]
fn stronghold_setup_start() -> Result<SetupMarker, SecretsErrorDto> {
    let root = require_stronghold_root()?;
    secrets::start_setup(&root).map_err(|err| SecretsErrorDto::from(&err))
}

#[tauri::command]
fn stronghold_setup_complete() -> Result<SetupMarker, SecretsErrorDto> {
    let root = require_stronghold_root()?;
    secrets::complete_setup(&root).map_err(|err| SecretsErrorDto::from(&err))
}

#[tauri::command]
fn stronghold_setup_reset() -> Result<(), SecretsErrorDto> {
    let root = require_stronghold_root()?;
    secrets::reset_setup(&root).map_err(|err| SecretsErrorDto::from(&err))
}

/// Argon2id-based KDF that derives the Stronghold vault key from a user
/// password. `tauri-plugin-stronghold`'s `Builder::new` requires a function
/// that turns the user's password into 32 raw bytes.
fn hash_password(password: &str) -> Vec<u8> {
    use argon2::{Algorithm, Argon2, Params, Version};
    // Fixed salt is acceptable here because the host has exactly one
    // Stronghold vault per OS user; the salt is not protecting against
    // rainbow tables across multiple users. A future round will switch to a
    // per-install random salt persisted next to setup.marker.
    let salt = b"agentplatform-stronghold-salt-v1";
    let params = Params::new(32 * 1024, 3, 1, Some(32)).expect("argon2 params");
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = vec![0u8; 32];
    argon2
        .hash_password_into(password.as_bytes(), salt, &mut out)
        .expect("argon2 hash_password_into");
    out
}

/// Production executor for `WorkspaceLaunchScheduler`. Holds the
/// Tauri `AppHandle` and the registry handles needed to actually
/// spawn `claude --dangerously-skip-permissions` in the workspace's
/// local cwd. The executor's `execute` invocation runs synchronously
/// (on the scheduler's calling thread) but the spawn work itself
/// happens inside a tokio task so the scheduler returns immediately.
///
/// `scheduler_self` is filled in post-construction via `init_scheduler`
/// because the executor and the scheduler reference each other; the
/// executor needs to call `notify_launch_settled` when its tokio
/// task finishes. `OnceLock` keeps the initialization race-free even
/// though we expect a single bootstrap-time fill.
struct RealLaunchExecutor {
    app_handle: tauri::AppHandle,
    terminal_registry: TerminalMeshRegistry,
    scheduler_self: std::sync::OnceLock<WorkspaceLaunchScheduler>,
}

impl RealLaunchExecutor {
    fn new(app_handle: tauri::AppHandle, terminal_registry: TerminalMeshRegistry) -> Self {
        Self {
            app_handle,
            terminal_registry,
            scheduler_self: std::sync::OnceLock::new(),
        }
    }

    fn init_scheduler(&self, sched: WorkspaceLaunchScheduler) {
        let _ = self.scheduler_self.set(sched);
    }
}

/// Select the `claude` program path that the auto-launched
/// terminal should exec. For Local workspaces this is the
/// absolute path the host's `DiscoveryCache` resolved at
/// bootstrap. For Remote workspaces it's the bare program name
/// so the remote shell's PATH resolves it — the local discovery
/// path means nothing on the remote machine, which typically
/// has `claude` at a different absolute location (or only as a
/// shell function / aliased command).
pub(crate) fn auto_launch_command_for_routing(
    routing: workspace_launch_scheduler::TransportRouting,
    local_claude_path: &std::path::Path,
) -> PathBuf {
    use workspace_launch_scheduler::TransportRouting;
    match routing {
        TransportRouting::Local => local_claude_path.to_path_buf(),
        TransportRouting::Ssh | TransportRouting::DockerOverSsh => PathBuf::from("claude"),
    }
}

/// Surface an auto-launch async failure to the frontend so a
/// queued tab does not stay parked forever in the waiting /
/// pending pane. Drains any pending placeholder for the tab and
/// emits a placeholder lifecycle update with `pending_launch =
/// false` + `status = Done` + `done_reason = Disconnected` so
/// the rail moves the tab to Done and the view shows the error.
/// `tracing::warn!` carries the underlying error so the dev /
/// support log captures the root cause; the user sees the
/// transport-kind in the rail badge and the Done state in the
/// pane.
/// Build the Done lifecycle envelope for an async auto-launch
/// failure AND install it into the pending placeholder map so
/// late subscribers can recover the state via the bootstrap
/// `snapshot_for_tab(tab_id)` fetch. Extracted from
/// `surface_auto_launch_async_failure` so the install side is
/// directly unit-testable without an `AppHandle` mock.
fn install_done_envelope_for_tab(
    registry: &TerminalMeshRegistry,
    tab_id: &str,
    transport_kind: crate::workspace_lifecycle::TransportKind,
) -> crate::workspace_lifecycle::WorkspaceLifecycleSnapshot {
    let mut envelope = crate::workspace_lifecycle::WorkspaceLifecycleSnapshot::fresh_for_workspace_with_kind(
        crate::workspace_lifecycle::TabKind::Workspace,
        None,
        transport_kind,
    );
    envelope.pending_launch = false;
    envelope.status = crate::workspace_lifecycle::TabStatus::Done;
    envelope.done_reason = Some(crate::workspace_lifecycle::DoneReason::Disconnected);
    // Overwrite any prior queued-launch placeholder (idempotent
    // set). The user-close path (`close_workspace`) already calls
    // `clear_pending_for_tab` so the Done entry is dropped when
    // the user dismisses the tab.
    registry.set_pending_for_tab(tab_id.to_string(), envelope.clone());
    envelope
}

/// Decide whether to install the Done placeholder for an async
/// auto-launch failure, consuming the close-during-launch tombstone
/// in the process. Returns `Some(envelope)` if the placeholder was
/// installed (caller should emit), `None` if it was skipped because
/// the tab was closed during launch (caller must NOT emit).
///
/// Workspace tabs reuse the workspace UUID as `tab_id`, so
/// installing a Done placeholder for a closed-during-launch tab
/// would persist into the next reopen of the same workspace: the
/// bootstrap `snapshot_for_tab(tab_id)` fetch would return the
/// stale Done, the React tab would synthesize an auto-launch
/// error, clear `awaitingAutoLaunch`, and then ignore the new
/// launch's real terminal id. Take the tombstone FIRST; if set,
/// skip the install (idempotent — the user-close path already
/// removed any prior placeholder, and a closed tab has no
/// subscriber to drain).
fn try_install_async_failure_placeholder(
    registry: &TerminalMeshRegistry,
    tab_id: &str,
    workspace_id: uuid::Uuid,
    transport_kind: crate::workspace_lifecycle::TransportKind,
) -> Option<crate::workspace_lifecycle::WorkspaceLifecycleSnapshot> {
    if registry.take_workspace_closed_during_launch(workspace_id) {
        tracing::debug!(
            %workspace_id,
            %tab_id,
            "skipping async-failure placeholder install: tab was closed during launch"
        );
        return None;
    }
    Some(install_done_envelope_for_tab(registry, tab_id, transport_kind))
}

fn surface_auto_launch_async_failure(
    app: &tauri::AppHandle,
    registry: &TerminalMeshRegistry,
    tab_id: &str,
    workspace_id: uuid::Uuid,
    transport_kind: crate::workspace_lifecycle::TransportKind,
) {
    if let Some(envelope) =
        try_install_async_failure_placeholder(registry, tab_id, workspace_id, transport_kind)
    {
        crate::workspace_lifecycle::emit_lifecycle_updated_for_placeholder(app, tab_id, &envelope);
    }
}

impl LaunchExecutor for RealLaunchExecutor {
    fn execute(&self, launch: PendingLaunch) {
        let app = self.app_handle.clone();
        let registry = self.terminal_registry.clone();
        let scheduler = self.scheduler_self.get().cloned();
        tokio::spawn(async move {
            // Resolve the discovered `claude` binary from the
            // bootstrap-time cache. If discovery is not Ready, log
            // and settle without spawning so the scheduler slot
            // drains cleanly.
            let claude_path: Option<PathBuf> = app
                .try_state::<DiscoveryCache>()
                .and_then(|c| c.snapshot())
                .and_then(|r| r.ok().map(|rec| rec.path));
            let workspace_id = launch.workspace_id;
            let tab_id = launch.tab_id.clone();
            let location = launch.workspace_location.clone();
            let routing = workspace_launch_scheduler::select_transport_kind_for(&location);

            // For Remote workspaces we need the SSH binary path +
            // an app_data dir for the ControlMaster socket. The SSH
            // binary defaults to `/usr/bin/ssh`; the app_data dir is
            // the same one bootstrap uses.
            let app_data_root_for_transport: Option<PathBuf> = app
                .try_state::<orchestrator::OrchestratorBootstrap>()
                .map(|b| b.app_data_root.clone());

            // Local routing needs the host-resolved absolute
            // `claude` path; Remote routing uses the bare `claude`
            // command on the remote host (`auto_launch_command_for_routing`
            // ignores the path for Remote). Only abort the launch
            // when discovery is needed but absent — Remote
            // workspaces must still be allowed to proceed when
            // local discovery is not Ready (e.g. claude is
            // installed only on the remote machine).
            let path = if let Some(p) = claude_path {
                p
            } else if workspace_launch_scheduler::should_block_auto_launch_on_local_discovery(
                routing,
            ) {
                tracing::warn!(
                    %workspace_id,
                    %tab_id,
                    "auto-launch skipped: claude discovery not ready (Local routing)"
                );
                surface_auto_launch_async_failure(
                    &app,
                    &registry,
                    &tab_id,
                    workspace_id,
                    crate::workspace_lifecycle::TransportKind::Local,
                );
                if let Some(sched) = scheduler {
                    sched.notify_launch_settled(workspace_id);
                }
                return;
            } else {
                // Remote routing — the executor will set
                // `spec.command = "claude"` via the bare-name
                // helper, so any PathBuf works as the unused
                // value passed in. Empty PathBuf is the clearest
                // sentinel.
                PathBuf::new()
            };
            let core_location = location.to_core_workspace_location();
            let cwd_for_spec = match &core_location {
                terminal_mesh_core::transport::WorkspaceLocation::Local { path } => path.clone(),
                terminal_mesh_core::transport::WorkspaceLocation::Remote { .. } => None,
            };
            let spec = terminal_mesh_core::TerminalSpec {
                terminal_id: uuid::Uuid::new_v4(),
                command: auto_launch_command_for_routing(routing, &path),
                args: launch.claude_argv.clone(),
                cwd: cwd_for_spec,
                env: Vec::new(),
                cols: 80,
                rows: 24,
                workspace_location: Some(core_location),
            };
            // Pick the transport matching the workspace location.
            // Local stays on the default LocalTransport path; Remote
            // routes through SSH or DockerOverSsh per the workspace location.
            let spawn_result = match routing {
                workspace_launch_scheduler::TransportRouting::Local => {
                    terminal_mesh::spawn_into_registry(
                        spec,
                        &app,
                        &registry,
                        Some(tab_id.clone()),
                        TabKind::Workspace,
                        Some(workspace_id.to_string()),
                    )
                }
                workspace_launch_scheduler::TransportRouting::Ssh => {
                    let Some(app_data) = app_data_root_for_transport.as_ref() else {
                        tracing::error!(
                            %workspace_id,
                            %tab_id,
                            "auto-launch SSH skipped: app_data_root not available"
                        );
                        surface_auto_launch_async_failure(
                            &app,
                            &registry,
                            &tab_id,
                            workspace_id,
                            crate::workspace_lifecycle::TransportKind::Ssh,
                        );
                        if let Some(sched) = scheduler {
                            sched.notify_launch_settled(workspace_id);
                        }
                        return;
                    };
                    match terminal_mesh_core::SshTransport::from_app_data(
                        PathBuf::from("/usr/bin/ssh"),
                        app_data,
                    ) {
                        Ok(t) => {
                            let transport: std::sync::Arc<
                                dyn terminal_mesh_core::transport::Transport,
                            > = std::sync::Arc::new(t);
                            terminal_mesh::spawn_into_registry_with_transport(
                                spec,
                                transport,
                                crate::workspace_lifecycle::TransportKind::Ssh,
                                &app,
                                &registry,
                                Some(tab_id.clone()),
                                TabKind::Workspace,
                                Some(workspace_id.to_string()),
                            )
                        }
                        Err(err) => {
                            tracing::error!(
                                %workspace_id,
                                %tab_id,
                                %err,
                                "SshTransport::from_app_data failed"
                            );
                            surface_auto_launch_async_failure(
                                &app,
                                &registry,
                                &tab_id,
                                workspace_id,
                                crate::workspace_lifecycle::TransportKind::Ssh,
                            );
                            if let Some(sched) = scheduler {
                                sched.notify_launch_settled(workspace_id);
                            }
                            return;
                        }
                    }
                }
                workspace_launch_scheduler::TransportRouting::DockerOverSsh => {
                    let Some(app_data) = app_data_root_for_transport.as_ref() else {
                        tracing::error!(
                            %workspace_id,
                            %tab_id,
                            "auto-launch Docker skipped: app_data_root not available"
                        );
                        surface_auto_launch_async_failure(
                            &app,
                            &registry,
                            &tab_id,
                            workspace_id,
                            crate::workspace_lifecycle::TransportKind::SshDocker,
                        );
                        if let Some(sched) = scheduler {
                            sched.notify_launch_settled(workspace_id);
                        }
                        return;
                    };
                    match terminal_mesh_core::DockerOverSshTransport::from_app_data(
                        PathBuf::from("/usr/bin/ssh"),
                        app_data,
                    ) {
                        Ok(t) => {
                            let transport: std::sync::Arc<
                                dyn terminal_mesh_core::transport::Transport,
                            > = std::sync::Arc::new(t);
                            terminal_mesh::spawn_into_registry_with_transport(
                                spec,
                                transport,
                                crate::workspace_lifecycle::TransportKind::SshDocker,
                                &app,
                                &registry,
                                Some(tab_id.clone()),
                                TabKind::Workspace,
                                Some(workspace_id.to_string()),
                            )
                        }
                        Err(err) => {
                            tracing::error!(
                                %workspace_id,
                                %tab_id,
                                %err,
                                "DockerOverSshTransport::from_app_data failed"
                            );
                            surface_auto_launch_async_failure(
                                &app,
                                &registry,
                                &tab_id,
                                workspace_id,
                                crate::workspace_lifecycle::TransportKind::SshDocker,
                            );
                            if let Some(sched) = scheduler {
                                sched.notify_launch_settled(workspace_id);
                            }
                            return;
                        }
                    }
                }
            };
            match spawn_result {
                Ok(terminal_id) => {
                    on_pending_launch_changed(&registry, &app, terminal_id, false);
                    // If the user closed this workspace's tab
                    // while the launch was past the cancel
                    // window, the tombstone tells us to shut the
                    // just-spawned terminal down + drop the
                    // retained snapshot so the child process and
                    // snapshot don't outlive the tab. Done BEFORE
                    // the unconditional real-id emit so the
                    // frontend (already without this tab) does
                    // not briefly see a Running snapshot for a
                    // gone tab.
                    if registry.take_workspace_closed_during_launch(workspace_id) {
                        if let Some(tx) = registry.lookup_command_tx(terminal_id) {
                            let app_for_reap = app.clone();
                            tokio::spawn(async move {
                                let _ = tx
                                    .send(terminal_mesh_core::ActorCommand::Shutdown)
                                    .await;
                                // Drop the live + retained
                                // bookkeeping; the actor's exit
                                // would normally call
                                // `forget_live`, but we want
                                // both sides cleared since no
                                // tab is bound to this terminal.
                                let _ = app_for_reap;
                            });
                        }
                        registry.forget(terminal_id);
                    } else {
                        // Real-id lifecycle emission for the
                        // common (immediate) case where no
                        // pending placeholder existed: the
                        // `on_pending_launch_changed(_, false)`
                        // call above is a no-op (snapshot was
                        // already false), so the frontend hook
                        // would never see a real-id envelope
                        // and `terminalIdByTabId` would stay
                        // unresolved past the hook's retry
                        // window. Emit explicitly using the
                        // live retained snapshot so both
                        // immediate and queued auto-launches
                        // attach reliably. Idempotent: queued
                        // launches already emitted via the
                        // pending-launch transition; replaying
                        // the same real-id event is a no-op in
                        // the frontend hook.
                        if let Some(snap) = registry.snapshot_for_terminal(terminal_id) {
                            crate::workspace_lifecycle::emit_lifecycle_updated(
                                &app,
                                terminal_id,
                                &snap,
                            );
                        }
                    }
                }
                Err(err) => {
                    tracing::error!(
                        %workspace_id,
                        %tab_id,
                        %err,
                        "auto-launch spawn failed"
                    );
                    // The spawn itself failed (transport-level or
                    // PTY-allocation error). Mirror the surface
                    // behavior so the React tab stops waiting and
                    // moves to Done. Local routing also benefits
                    // because `spawn_into_registry` can fail (rare,
                    // but possible for /bin/zsh missing etc.).
                    let kind = match routing {
                        workspace_launch_scheduler::TransportRouting::Local =>
                            crate::workspace_lifecycle::TransportKind::Local,
                        workspace_launch_scheduler::TransportRouting::Ssh =>
                            crate::workspace_lifecycle::TransportKind::Ssh,
                        workspace_launch_scheduler::TransportRouting::DockerOverSsh =>
                            crate::workspace_lifecycle::TransportKind::SshDocker,
                    };
                    surface_auto_launch_async_failure(&app, &registry, &tab_id, workspace_id, kind);
                }
            }
            if let Some(sched) = scheduler {
                sched.notify_launch_settled(workspace_id);
            }
        });
    }
}

#[tauri::command]
fn bootstrap_status() -> Result<BootstrapPaths, BootstrapErrorDto> {
    // OnceLock guarantees the setup hook ran before the frontend mounts and
    // calls this command. If somehow it didn't, surface an explicit DTO with
    // a "not_run" kind so the UI still gets a structured shape.
    match BOOTSTRAP_RESULT.get() {
        Some(Ok(paths)) => Ok(paths.clone()),
        Some(Err(err)) => Err(BootstrapErrorDto::from(err)),
        None => Err(BootstrapErrorDto {
            kind: "not_run".into(),
            path: None,
            message: "bootstrap has not run".into(),
        }),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Structured JSON logging with sensitive-value redaction (AC-9.5). The
    // RedactingMakeWriter masks `refresh_token`, `access_token`, `password`,
    // `bearer`, and `email_body` field values before they reach stderr.
    logging::init_subscriber();

    tauri::Builder::default()
        .plugin(tauri_plugin_stronghold::Builder::new(hash_password).build())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_positioner::init())
        .manage(MountRegistry::new())
        .manage(PluginMigrationState::new())
        .manage(AccessTokenCache::new())
        .manage(DiscoveryCache::empty())
        .manage(McpConfigRegistry::new())
        .manage(TerminalMeshRegistry::new())
        .manage({
            let mgr = std::sync::Arc::new(SidecarManager::new(SidecarConfig::production_defaults()));
            SidecarManager::install_self_arc(&mgr);
            mgr
        })
        .setup(|app| {
            let result = bootstrap::ensure_dirs().and_then(|paths| {
                // Now that the generated plugin registry is available, create
                // ${APP_DATA}/plugins/<id>/ for each bundled plugin. Reporting
                // an error here surfaces via the same BootstrapErrorDto path
                // as the top-level dirs (preserves AC-9.1 negative-test
                // structural inspection).
                let plugin_ids = generated::plugin_registry::PLUGINS
                    .iter()
                    .map(|p| p.plugin_id);
                let created = bootstrap::ensure_plugin_dirs(&paths.plugins_root, plugin_ids)?;
                tracing::info!(per_plugin = created.len(), "per-plugin dirs ready");
                Ok(paths)
            });
            // Per-plugin migrations at startup (AC-9.3): a failing plugin's
            // migration is recorded in PluginMigrationState; bootstrap itself
            // never aborts on a per-plugin migration failure. The frontend
            // gates plugin component mounting on the recorded status.
            match &result {
                Ok(paths) => {
                    tracing::info!(?paths, "bootstrap ok");
                    let state = app.state::<PluginMigrationState>();
                    run_all_plugin_migrations_at_bootstrap(&paths.plugins_root, &state);

                    // Stronghold state directory (AC-5.4 host): ensure
                    // `${APP_DATA}/stronghold-state/` exists so the marker +
                    // snapshot have a place to land. The actual Stronghold
                    // plugin handles snapshot creation when the frontend
                    // calls its `initialize` / `load` commands.
                    if let Some(app_data) = paths.plugins_root.parent() {
                        let sg_root = app_data.join(STRONGHOLD_DIRNAME);
                        match secrets::ensure_stronghold_root(&sg_root) {
                            Ok(()) => tracing::info!(stronghold_root = %sg_root.display(), "stronghold-state dir ready"),
                            Err(err) => tracing::error!(%err, "stronghold-state dir create failed"),
                        }
                        // Round 20 (task13): clean any stale per-tab MCP
                        // config files left behind by the previous app
                        // run BEFORE any new tab generates a config. The
                        // conservative GC policy (delete all *.json
                        // inside `${APP_DATA}/claude-mcp-configs/`) is
                        // acceptable per spec when no `host-runtime.json`
                        // PID metadata is available; task38 will replace
                        // this with PID-correlated GC.
                        mcp_config::startup_gc(app_data);

                        // Round 20 (task13): run `claude` PATH discovery
                        // once at bootstrap and cache the result so the
                        // orchestrator placeholder can render the
                        // onboarding card without polling. Failure does
                        // NOT abort bootstrap; the cache stores the
                        // typed error so the frontend can recover.
                        if let Some(home) = directories::BaseDirs::new()
                            .map(|b| b.home_dir().to_path_buf())
                        {
                            let cache = app.state::<DiscoveryCache>();
                            let outcome = claude_discovery::validate_or_rediscover(&home, app_data);
                            cache.store(match &outcome {
                                Ok(r) => Ok(r.clone()),
                                Err(e) => Err(claude_discovery::clone_error(e)),
                            });
                            match &outcome {
                                Ok(rec) => tracing::info!(path = %rec.path.display(), "claude discovery ready"),
                                Err(err) => tracing::warn!(%err, "claude discovery not found"),
                            }
                        }
                    }

                    // Round 28 (task17): load the persisted workspace
                    // registry from `${APP_DATA}/workspaces.json` and
                    // attach it to the Tauri app state. The workspaces
                    // root is `paths.workspaces` (the same dir
                    // bootstrap::ensure_dirs already created).
                    let app_data_root = paths
                        .plugins_root
                        .parent()
                        .unwrap_or(&paths.plugins_root)
                        .to_path_buf();
                    let registry = WorkspaceRegistry::load(
                        app_data_root.clone(),
                        Some(paths.workspaces.clone()),
                    );
                    tracing::info!(
                        workspaces = registry.list().len(),
                        "workspaces registry loaded"
                    );
                    app.manage(registry);

                    // Round 32 (task19): load persisted IDE handoff
                    // preference (default Cursor). Surfaced via
                    // `ide_get_preference` / `ide_set_preference` and
                    // consumed by `ide_open_workspace` /
                    // `ide_reveal_in_finder`.
                    let ide_store = IdePreferenceStore::load(app_data_root.clone());
                    tracing::info!(
                        ide_command = %ide_store.snapshot().ide_command,
                        "ide preference loaded"
                    );
                    app.manage(ide_store);

                    // Round 35 (task20): manage orchestrator bootstrap
                    // context + empty session state. The orchestrator
                    // tab's launch path consumes these via
                    // `orchestrator_launch_claude` / `_status`.
                    let orchestrator = OrchestratorState::new();
                    app.manage(orchestrator.clone());

                    // Auto-launch scheduler: gates how many claude
                    // processes can spawn at once on Local workspace
                    // open (default cap = 4 per spec). The executor
                    // and scheduler reference each other so the
                    // settle callback can drain the next pending
                    // entry; OnceLock breaks the chicken-and-egg.
                    let scheduler_terminal_registry =
                        app.state::<TerminalMeshRegistry>().inner().clone();
                    let real_executor = std::sync::Arc::new(RealLaunchExecutor::new(
                        app.handle().clone(),
                        scheduler_terminal_registry,
                    ));
                    let scheduler = WorkspaceLaunchScheduler::new(
                        DEFAULT_LAUNCH_CAP,
                        real_executor.clone(),
                    );
                    real_executor.init_scheduler(scheduler.clone());
                    app.manage(scheduler);

                    // Round 38 (task21 remediation): spawn the host
                    // RPC bridge for sidecar back-channel calls. The
                    // bridge clones the same internally-Arc'd state
                    // handles the Tauri commands hold, so updates
                    // are observed in both places. The socket path
                    // is recorded in `OrchestratorBootstrap` so the
                    // orchestrator MCP config generation can pass
                    // `--host-rpc-sock <path>` to every sidecar.
                    let mount_registry_handle = app.state::<dispatcher::MountRegistry>().inner().clone();
                    let terminal_registry_handle = app.state::<TerminalMeshRegistry>().inner().clone();
                    let workspaces_handle = app.state::<WorkspaceRegistry>().inner().clone();
                    let host_rpc_sock_path = match host_rpc::prepare_socket_path(&app_data_root) {
                        Ok(p) => {
                            host_rpc::spawn_bridge(
                                p.clone(),
                                host_rpc::HostRpcState {
                                    orchestrator: orchestrator.clone(),
                                    mount_registry: mount_registry_handle,
                                    terminal_registry: terminal_registry_handle,
                                    workspaces: workspaces_handle,
                                },
                            );
                            tracing::info!(host_rpc_sock = %p.display(), "host_rpc bridge spawned");
                            Some(p)
                        }
                        Err(err) => {
                            tracing::error!(%err, "host_rpc bridge socket prep failed; cross-tab read disabled");
                            None
                        }
                    };
                    app.manage(OrchestratorBootstrap {
                        agent_platform_root: paths.agent_platform.clone(),
                        app_data_root,
                        host_rpc_sock: host_rpc_sock_path,
                    });

                    // Round 40 (task22): notification surface — wires
                    // tauri-plugin-notification + tauri-plugin-positioner
                    // through a host-level dedup arbiter, lazy
                    // permission flow, and tray-entry ring. The
                    // `RealNotifySink` holds a clone of the
                    // AppHandle so it can call into the notification
                    // plugin on every fire.
                    let real_sink = std::sync::Arc::new(
                        notification::RealNotifySink::new(app.handle().clone()),
                    );
                    let notification_service =
                        notification::NotificationService::with_sink(real_sink);
                    app.manage(notification_service);

                    // Round 41 (task22 remediation): menubar tray icon
                    // — left-click toggles the tray window via
                    // `Position::TrayBottomCenter` (per AC-8.1). The
                    // tray-icon API needs the positioner crate's
                    // `tray-icon` feature enabled in Cargo.toml.
                    let tray_icon_result = tauri::tray::TrayIconBuilder::with_id("notifications")
                        .icon(app.default_window_icon().expect("default icon").clone())
                        .icon_as_template(true)
                        .show_menu_on_left_click(false)
                        .on_tray_icon_event(|tray, event| {
                            let app = tray.app_handle().clone();
                            // First let the positioner record the tray
                            // rect for its subsequent move_window call.
                            tauri_plugin_positioner::on_tray_event(&app, &event);
                            // Only react to the left-click-release; the
                            // Click event fires twice (Down + Up) and
                            // we only want a single toggle per click.
                            let is_left_click_up = matches!(
                                event,
                                tauri::tray::TrayIconEvent::Click {
                                    button: tauri::tray::MouseButton::Left,
                                    button_state: tauri::tray::MouseButtonState::Up,
                                    ..
                                }
                            );
                            if !is_left_click_up {
                                return;
                            }
                            use tauri::Manager;
                            let Some(window) = app.get_webview_window("tray") else {
                                tracing::warn!("tray click: tray webview window missing");
                                return;
                            };
                            // Reposition first so the window appears
                            // at the right spot when shown.
                            use tauri_plugin_positioner::{Position, WindowExt};
                            if let Err(err) =
                                window.move_window(Position::TrayBottomCenter)
                            {
                                tracing::warn!(%err, "tray click: move_window failed");
                            }
                            let visible = window.is_visible().unwrap_or(false);
                            match notification::compute_tray_toggle_action(visible) {
                                notification::TrayToggleAction::Hide => {
                                    if let Err(err) = window.hide() {
                                        tracing::warn!(%err, "tray window hide failed");
                                    }
                                }
                                notification::TrayToggleAction::Show => {
                                    if let Err(err) = window.show() {
                                        tracing::warn!(%err, "tray window show failed");
                                    }
                                    if let Err(err) = window.set_focus() {
                                        tracing::warn!(%err, "tray window set_focus failed");
                                    }
                                }
                            }
                        })
                        .build(app);
                    match tray_icon_result {
                        Ok(_tray) => tracing::info!("tray icon registered"),
                        Err(err) => tracing::error!(%err, "tray icon registration failed"),
                    }
                }
                Err(err) => tracing::error!(%err, "bootstrap failed"),
            }
            BOOTSTRAP_RESULT.set(result).ok();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            bootstrap_status,
            dispatcher::mount_plugin,
            dispatcher::unmount_plugin,
            dispatcher::dispatch_plugin_command,
            plugin_sqlite::plugin_migration_status,
            plugin_sqlite::retry_plugin_migration,
            stronghold_setup_status,
            stronghold_setup_start,
            stronghold_setup_complete,
            stronghold_setup_reset,
            dev_diagnostics::dev_diagnostics_status,
            sidecar_manager::sidecar_status,
            sidecar_manager::retry_sidecar,
            sidecar_manager::shutdown_sidecar,
            sidecar_manager::spawn_sidecar_from_manifest,
            claude_discovery::claude_discovery_status,
            claude_discovery::claude_redo_discovery,
            claude_discovery::claude_set_path_override,
            mcp_config::generate_mcp_config,
            mcp_config::delete_mcp_config,
            terminal_mesh::terminal_spawn,
            terminal_mesh::terminal_write_stdin,
            terminal_mesh::terminal_resize,
            terminal_mesh::terminal_shutdown,
            terminal_mesh::terminal_scrollback,
            terminal_mesh::terminal_mesh_cross_tab_read_scrollback,
            terminal_mesh::workspace_lifecycle_snapshot,
            workspaces::list_workspaces,
            workspaces::create_workspace,
            workspaces::register_workspace,
            workspaces::register_remote_workspace,
            workspaces::open_workspace,
            workspaces::close_workspace,
            workspaces::resolve_workspace_for_tab,
            workspace_launch_scheduler::request_workspace_auto_launch,
            ide_handoff::ide_get_preference,
            ide_handoff::ide_set_preference,
            ide_handoff::ide_open_workspace,
            ide_handoff::ide_open_remote_workspace,
            ide_handoff::ide_reveal_in_finder,
            orchestrator::orchestrator_status,
            orchestrator::orchestrator_launch_claude,
            orchestrator::orchestrator_shutdown,
            notification::notification_list_recent_tray_entries,
            notification::notification_clear_tray_entries,
            notification::notification_get_permission_state,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Local routing reuses the host's resolved absolute Claude
    /// path. Remote routing (SSH or Docker-over-SSH) switches to
    /// the bare program name so the remote shell's PATH resolves
    /// it — the local absolute path means nothing on the remote
    /// host. Regression for the case where the remote wrapper
    /// would silently fail because it tried to exec a local-only
    /// binary path.
    #[test]
    fn auto_launch_command_for_routing_picks_local_or_bare_remote() {
        use workspace_launch_scheduler::TransportRouting;
        let local_path = std::path::PathBuf::from("/opt/homebrew/bin/claude");

        assert_eq!(
            auto_launch_command_for_routing(TransportRouting::Local, &local_path),
            local_path,
            "Local routing reuses the host-resolved absolute path"
        );
        assert_eq!(
            auto_launch_command_for_routing(TransportRouting::Ssh, &local_path),
            std::path::PathBuf::from("claude"),
            "Remote SSH routing uses bare `claude` so remote PATH resolves"
        );
        assert_eq!(
            auto_launch_command_for_routing(TransportRouting::DockerOverSsh, &local_path),
            std::path::PathBuf::from("claude"),
            "Remote Docker routing uses bare `claude` so remote PATH resolves"
        );
    }

    /// An async auto-launch failure MUST leave a retained Done
    /// snapshot in the pending placeholder map so a React
    /// lifecycle hook that subscribes after the event fired can
    /// still see the failure on its bootstrap fetch. Without
    /// this, a subscribe-after-event race would leave the tab
    /// stuck in `awaitingAutoLaunch: true` forever — the
    /// stuck-waiting watcher only flips the flag when it sees a
    /// Done snapshot, and there would be no snapshot to see.
    #[test]
    fn install_done_envelope_for_tab_makes_late_snapshot_lookup_return_done() {
        let r = TerminalMeshRegistry::new();
        let tab_id = "tab-late-subscribe";
        // Pre-state: no snapshot for the tab.
        assert!(
            r.snapshot_for_tab(tab_id).is_none(),
            "tab has no live snapshot before install"
        );
        // Install the Done envelope.
        let envelope = install_done_envelope_for_tab(
            &r,
            tab_id,
            crate::workspace_lifecycle::TransportKind::Ssh,
        );
        assert!(
            matches!(envelope.status, crate::workspace_lifecycle::TabStatus::Done),
            "installed envelope must be Done"
        );
        assert!(
            matches!(
                envelope.done_reason,
                Some(crate::workspace_lifecycle::DoneReason::Disconnected)
            ),
            "installed envelope must carry the Disconnected done_reason"
        );
        assert_eq!(envelope.transport_kind, crate::workspace_lifecycle::TransportKind::Ssh);
        // A late subscriber querying `snapshot_for_tab` (the
        // path the React hook's bootstrap fetch uses) must now
        // see the Done snapshot.
        let late_lookup = r
            .snapshot_for_tab(tab_id)
            .expect("late snapshot lookup must return the installed Done envelope");
        assert!(matches!(
            late_lookup.status,
            crate::workspace_lifecycle::TabStatus::Done
        ));
        assert!(matches!(
            late_lookup.done_reason,
            Some(crate::workspace_lifecycle::DoneReason::Disconnected)
        ));
        assert_eq!(
            late_lookup.transport_kind,
            crate::workspace_lifecycle::TransportKind::Ssh
        );
    }

    /// Idempotent install: a second call replaces the first
    /// snapshot (latest failure wins). Mirrors the
    /// `set_pending_for_tab` overwrite semantics.
    #[test]
    fn install_done_envelope_for_tab_overwrites_prior_install() {
        let r = TerminalMeshRegistry::new();
        let tab_id = "tab-double-fail";
        install_done_envelope_for_tab(
            &r,
            tab_id,
            crate::workspace_lifecycle::TransportKind::Ssh,
        );
        install_done_envelope_for_tab(
            &r,
            tab_id,
            crate::workspace_lifecycle::TransportKind::SshDocker,
        );
        let latest = r
            .snapshot_for_tab(tab_id)
            .expect("snapshot retained after double install");
        assert_eq!(
            latest.transport_kind,
            crate::workspace_lifecycle::TransportKind::SshDocker,
            "second install overwrites the first"
        );
    }

    /// If the user closed a workspace tab while its auto-launch
    /// was still in `Launching` state and the async path then
    /// failed, the close-during-launch tombstone is present.
    /// `try_install_async_failure_placeholder` MUST consume the
    /// tombstone and MUST NOT install a Done placeholder for the
    /// now-closed tab. Otherwise — because workspace tabs reuse
    /// the workspace UUID as `tab_id` — reopening the same
    /// workspace later would bootstrap from the stale Done
    /// placeholder, synthesize an auto-launch error, clear
    /// `awaitingAutoLaunch`, and then ignore the next launch's
    /// real terminal id.
    #[test]
    fn try_install_async_failure_placeholder_skips_install_when_tombstone_present() {
        let r = TerminalMeshRegistry::new();
        let workspace_id = uuid::Uuid::new_v4();
        let tab_id = workspace_id.to_string();
        // Simulate: user closed the tab mid-launch.
        r.mark_workspace_closed_during_launch(workspace_id);
        // Async launch then fails — should NOT install the Done
        // envelope for this closed tab.
        let result = try_install_async_failure_placeholder(
            &r,
            &tab_id,
            workspace_id,
            crate::workspace_lifecycle::TransportKind::Ssh,
        );
        assert!(
            result.is_none(),
            "tombstone present must skip the placeholder install"
        );
        assert!(
            r.snapshot_for_tab(&tab_id).is_none(),
            "snapshot_for_tab must return None — no stale Done placeholder for the closed tab"
        );
        // The tombstone must have been CONSUMED (not just
        // observed), otherwise a future successful launch's
        // `take_workspace_closed_during_launch` would reap the
        // wrong terminal.
        assert!(
            !r.take_workspace_closed_during_launch(workspace_id),
            "tombstone must be consumed by the failure path"
        );
    }

    /// No tombstone → the failure path installs the Done envelope
    /// as before. This is the common path (transport-level error
    /// without a close-during-launch race).
    #[test]
    fn try_install_async_failure_placeholder_installs_when_no_tombstone() {
        let r = TerminalMeshRegistry::new();
        let workspace_id = uuid::Uuid::new_v4();
        let tab_id = workspace_id.to_string();
        let result = try_install_async_failure_placeholder(
            &r,
            &tab_id,
            workspace_id,
            crate::workspace_lifecycle::TransportKind::SshDocker,
        );
        let envelope = result.expect("no tombstone must install the placeholder");
        assert!(matches!(
            envelope.status,
            crate::workspace_lifecycle::TabStatus::Done
        ));
        assert_eq!(
            envelope.transport_kind,
            crate::workspace_lifecycle::TransportKind::SshDocker
        );
        // The Done envelope is now visible to late subscribers.
        let late = r
            .snapshot_for_tab(&tab_id)
            .expect("late snapshot lookup returns the installed Done envelope");
        assert!(matches!(
            late.status,
            crate::workspace_lifecycle::TabStatus::Done
        ));
    }
}
