//! Tauri host library entry point.
//!
//! Holds the `tauri::Builder` configuration. The binary at `src/main.rs` and
//! the mobile entry point both call `run()`.

mod bootstrap;

use bootstrap::{BootstrapError, BootstrapPaths};
use std::sync::OnceLock;

/// Cached result of the first-run bootstrap. Populated by the setup hook;
/// consumed by the `bootstrap_status` Tauri command.
static BOOTSTRAP_RESULT: OnceLock<Result<BootstrapPaths, BootstrapError>> = OnceLock::new();

#[tauri::command]
fn bootstrap_status() -> Result<BootstrapPaths, String> {
    // OnceLock guarantees the setup hook ran before the frontend mounts and
    // calls this command. If somehow it didn't, surface an explicit message.
    match BOOTSTRAP_RESULT.get() {
        Some(Ok(paths)) => Ok(paths.clone()),
        Some(Err(err)) => Err(err.to_string()),
        None => Err("bootstrap has not run".to_string()),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Initialize structured JSON logging early so bootstrap errors are visible
    // in stderr/log streams (the actual file-based per-plugin logs land later).
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    tauri::Builder::default()
        .setup(|_app| {
            let result = bootstrap::ensure_dirs();
            match &result {
                Ok(paths) => tracing::info!(?paths, "bootstrap ok"),
                Err(err) => tracing::error!(%err, "bootstrap failed"),
            }
            BOOTSTRAP_RESULT.set(result).ok();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![bootstrap_status])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
