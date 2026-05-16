//! Tauri host library entry point.
//!
//! Holds the `tauri::Builder` configuration. The binary at `src/main.rs` and
//! the mobile entry point both call `run()`.

mod bootstrap;
mod dispatcher;
mod generated;
mod logging;
mod plugin_sqlite;

use bootstrap::{BootstrapError, BootstrapPaths};
use dispatcher::MountRegistry;
use serde::Serialize;
use std::sync::OnceLock;

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
        .manage(MountRegistry::new())
        .setup(|_app| {
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
            match &result {
                Ok(paths) => tracing::info!(?paths, "bootstrap ok"),
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
