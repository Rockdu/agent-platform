//! Tauri host library entry point.
//!
//! Holds the `tauri::Builder` configuration. The binary at `src/main.rs` and
//! the mobile entry point both call `run()`.

mod bootstrap;
mod claude_discovery;
mod dev_diagnostics;
mod dispatcher;
mod generated;
mod logging;
mod mcp_config;
mod plugin_sqlite;
mod secrets;
mod sidecar_manager;
mod terminal_mesh;
mod workspaces;

use bootstrap::{BootstrapError, BootstrapPaths};
use claude_discovery::DiscoveryCache;
use dispatcher::MountRegistry;
use mcp_config::McpConfigRegistry;
use plugin_sqlite::{run_all_plugin_migrations_at_bootstrap, PluginMigrationState};
use secrets::{AccessTokenCache, SecretsErrorDto, SetupMarker, SetupStatus};
use sidecar_manager::{SidecarConfig, SidecarManager};
use terminal_mesh::TerminalMeshRegistry;
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
                    let registry = WorkspaceRegistry::load(
                        paths.plugins_root.parent().unwrap_or(&paths.plugins_root).to_path_buf(),
                        Some(paths.workspaces.clone()),
                    );
                    tracing::info!(
                        workspaces = registry.list().len(),
                        "workspaces registry loaded"
                    );
                    app.manage(registry);
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
            workspaces::list_workspaces,
            workspaces::create_workspace,
            workspaces::register_workspace,
            workspaces::open_workspace,
            workspaces::close_workspace,
            workspaces::resolve_workspace_for_tab,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
