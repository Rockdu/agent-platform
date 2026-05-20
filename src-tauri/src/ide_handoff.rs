//! Per-workspace IDE handoff (AC-4.6): "Open in <IDE>" + Reveal in
//! Finder, with a persisted user-overridable IDE command (default
//! Cursor). Surfaced from both the workspace switcher modal recent
//! rows and the active Terminal Mesh tab's status strip.
//!
//! Wire shape mirrors the existing `workspaces.rs` boundary: wire
//! camelCase via `IdePreference`, on-disk snake_case via internal
//! `StoredIdePreference` + `From` conversions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use tauri::State;

const PREFERENCE_FILENAME: &str = "ide-preference.json";
const DEFAULT_IDE_COMMAND: &str = "cursor";
const DEFAULT_IDE_ARG_PLACEHOLDER: &str = "{path}";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdePreference {
    pub ide_command: String,
    pub ide_args_template: Vec<String>,
}

impl IdePreference {
    pub fn default_cursor() -> Self {
        Self {
            ide_command: DEFAULT_IDE_COMMAND.to_string(),
            ide_args_template: vec![DEFAULT_IDE_ARG_PLACEHOLDER.to_string()],
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredIdePreference {
    ide_command: String,
    ide_args_template: Vec<String>,
}

impl From<&IdePreference> for StoredIdePreference {
    fn from(p: &IdePreference) -> Self {
        Self {
            ide_command: p.ide_command.clone(),
            ide_args_template: p.ide_args_template.clone(),
        }
    }
}

impl From<StoredIdePreference> for IdePreference {
    fn from(s: StoredIdePreference) -> Self {
        Self {
            ide_command: s.ide_command,
            ide_args_template: s.ide_args_template,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IdeHandoffError {
    #[error("IDE command `{command}` is not in PATH")]
    IdeNotInPath { command: String },

    #[error("path `{path}` is not a directory or does not exist")]
    NotADirectory { path: PathBuf },

    #[error("spawning `{command}` failed: {message}")]
    SpawnFailed { command: String, message: String },

    #[error("io error in `{context}`: {message}")]
    Io { context: String, message: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum IdeHandoffErrorDto {
    IdeNotInPath { command: String },
    NotADirectory { path: String },
    SpawnFailed { command: String, message: String },
    Io { context: String, message: String },
}

impl From<&IdeHandoffError> for IdeHandoffErrorDto {
    fn from(err: &IdeHandoffError) -> Self {
        match err {
            IdeHandoffError::IdeNotInPath { command } => Self::IdeNotInPath {
                command: command.clone(),
            },
            IdeHandoffError::NotADirectory { path } => Self::NotADirectory {
                path: path.display().to_string(),
            },
            IdeHandoffError::SpawnFailed { command, message } => Self::SpawnFailed {
                command: command.clone(),
                message: message.clone(),
            },
            IdeHandoffError::Io { context, message } => Self::Io {
                context: context.clone(),
                message: message.clone(),
            },
        }
    }
}

pub struct IdePreferenceStore {
    inner: Mutex<IdePreference>,
    storage_path: PathBuf,
}

impl IdePreferenceStore {
    /// Bootstrap-time loader. Reads `${app_data}/ide-preference.json`
    /// if present; missing or malformed → default Cursor (logged warn
    /// for malformed).
    pub fn load(app_data: PathBuf) -> Self {
        let storage_path = app_data.join(PREFERENCE_FILENAME);
        let pref = match std::fs::read(&storage_path) {
            Ok(bytes) => match serde_json::from_slice::<StoredIdePreference>(&bytes) {
                Ok(stored) => IdePreference::from(stored),
                Err(e) => {
                    tracing::warn!(path = %storage_path.display(), %e, "ide-preference.json malformed; falling back to default");
                    IdePreference::default_cursor()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                IdePreference::default_cursor()
            }
            Err(e) => {
                tracing::warn!(path = %storage_path.display(), %e, "ide-preference.json read failed; falling back to default");
                IdePreference::default_cursor()
            }
        };
        Self {
            inner: Mutex::new(pref),
            storage_path,
        }
    }

    pub fn snapshot(&self) -> IdePreference {
        let guard = self.inner.lock().expect("IdePreferenceStore poisoned");
        guard.clone()
    }

    fn set(&self, new_pref: IdePreference) -> Result<IdePreference, IdeHandoffError> {
        // Validate command resolves on PATH BEFORE persisting; if the
        // user typed `cursor` but it's not installed, we surface a
        // typed error and leave the existing preference intact.
        if which_in_path(&new_pref.ide_command).is_none() {
            return Err(IdeHandoffError::IdeNotInPath {
                command: new_pref.ide_command,
            });
        }
        Self::persist(&self.storage_path, &new_pref)?;
        let mut guard = self.inner.lock().expect("IdePreferenceStore poisoned");
        *guard = new_pref.clone();
        Ok(new_pref)
    }

    fn persist(storage_path: &Path, pref: &IdePreference) -> Result<(), IdeHandoffError> {
        if let Some(parent) = storage_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| IdeHandoffError::Io {
                context: format!("create_dir_all {}", parent.display()),
                message: e.to_string(),
            })?;
        }
        let tmp = storage_path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(&StoredIdePreference::from(pref)).map_err(|e| {
            IdeHandoffError::Io {
                context: "serialize ide-preference.json".into(),
                message: e.to_string(),
            }
        })?;
        {
            use std::io::Write;
            #[cfg(unix)]
            use std::os::unix::fs::OpenOptionsExt;
            #[cfg(unix)]
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| IdeHandoffError::Io {
                    context: format!("open {}", tmp.display()),
                    message: e.to_string(),
                })?;
            #[cfg(not(unix))]
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)
                .map_err(|e| IdeHandoffError::Io {
                    context: format!("open {}", tmp.display()),
                    message: e.to_string(),
                })?;
            f.write_all(&body).map_err(|e| IdeHandoffError::Io {
                context: format!("write {}", tmp.display()),
                message: e.to_string(),
            })?;
            let _ = f.sync_all();
        }
        std::fs::rename(&tmp, storage_path).map_err(|e| IdeHandoffError::Io {
            context: format!("rename {} -> {}", tmp.display(), storage_path.display()),
            message: e.to_string(),
        })
    }
}

/// Walks `$PATH` looking for `command`. On Unix it requires the
/// candidate to be a regular file with any execute bit set; on
/// Windows it accepts any file (Windows uses extension probing
/// `cmd.exe`/`bat`/`.exe` which we don't need for the MVP IDE
/// commands).
///
/// On macOS, Tauri GUI processes inherit a stripped PATH
/// (`/usr/bin:/bin:...`) that omits Homebrew and user-installed
/// CLI tools. To compensate, well-known macOS locations are
/// probed in addition to `$PATH`.
pub fn which_in_path(command: &str) -> Option<PathBuf> {
    if command.is_empty() {
        return None;
    }
    // Probe $PATH first so user overrides win.
    if let Some(raw_path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&raw_path) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let candidate = dir.join(command);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    // macOS fallback: check common locations not included in the
    // stripped PATH that Tauri GUI processes inherit.
    #[cfg(target_os = "macos")]
    {
        // Common Homebrew / user-install locations.
        let extra_dirs = [
            "/usr/local/bin",
            "/opt/homebrew/bin",
            "/opt/homebrew/sbin",
            "/usr/local/sbin",
        ];
        for dir in extra_dirs {
            let candidate = std::path::Path::new(dir).join(command);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
        // macOS app-bundle CLI bins (installed under .app rather than
        // symlinked into PATH). Map well-known IDE command names to the
        // CLI binary inside their bundle.
        let bundle_bins: &[(&str, &str)] = &[
            ("cursor", "/Applications/Cursor.app/Contents/MacOS/cursor"),
            ("code",   "/Applications/Visual Studio Code.app/Contents/MacOS/Electron"),
            ("code",   "/Applications/VSCodium.app/Contents/MacOS/VSCodium"),
            ("zed",    "/Applications/Zed.app/Contents/MacOS/zed"),
        ];
        for (cmd, path) in bundle_bins {
            if *cmd == command {
                let p = std::path::Path::new(path);
                if is_executable(p) {
                    return Some(p.to_path_buf());
                }
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(path) else { return false };
    if !meta.is_file() {
        return false;
    }
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
}

/// Substitute `{key}` placeholders in `template` using `substitutions`.
/// Args without placeholders pass through unchanged. Unknown
/// placeholders remain literal.
pub fn fill_template(template: &[String], substitutions: &HashMap<&str, &str>) -> Vec<String> {
    template
        .iter()
        .map(|arg| {
            let mut out = arg.clone();
            for (k, v) in substitutions {
                let needle = format!("{{{k}}}");
                out = out.replace(&needle, v);
            }
            out
        })
        .collect()
}

fn ensure_dir(path: &Path) -> Result<(), IdeHandoffError> {
    let meta = std::fs::metadata(path).map_err(|_| IdeHandoffError::NotADirectory {
        path: path.to_path_buf(),
    })?;
    if !meta.is_dir() {
        return Err(IdeHandoffError::NotADirectory {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn open_with(command: &str, args: &[String]) -> Result<(), IdeHandoffError> {
    let resolved = which_in_path(command).ok_or_else(|| IdeHandoffError::IdeNotInPath {
        command: command.to_string(),
    })?;
    std::process::Command::new(&resolved)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| IdeHandoffError::SpawnFailed {
            command: resolved.display().to_string(),
            message: e.to_string(),
        })?;
    Ok(())
}

/// Open the workspace in the configured IDE.
pub fn open_workspace_in_ide(
    pref: &IdePreference,
    workspace_path: &Path,
) -> Result<(), IdeHandoffError> {
    ensure_dir(workspace_path)?;
    let path_str = workspace_path.display().to_string();

    // On macOS, `open -a <AppName> <path>` is the most reliable way
    // to open a folder in a GUI editor. It works regardless of whether
    // the editor's CLI tool is installed in $PATH or accessible from
    // the stripped PATH that Tauri GUI processes inherit.
    #[cfg(target_os = "macos")]
    {
        let app_name = match pref.ide_command.as_str() {
            "cursor" => Some("Cursor"),
            "code"   => Some("Visual Studio Code"),
            "zed"    => Some("Zed"),
            _        => None,
        };
        if let Some(app) = app_name {
            let result = std::process::Command::new("/usr/bin/open")
                .args(["-a", app, &path_str])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            if result.is_ok() {
                return Ok(());
            }
            // open -a failed (app not installed); fall through to CLI path.
        }
    }

    // Generic path: use the configured CLI command with template args.
    let mut subs = HashMap::new();
    subs.insert("path", path_str.as_str());
    let args = fill_template(&pref.ide_args_template, &subs);
    open_with(&pref.ide_command, &args)
}

/// Reveal the workspace directory in the platform's file manager.
pub fn reveal_in_finder(workspace_path: &Path) -> Result<(), IdeHandoffError> {
    ensure_dir(workspace_path)?;
    // macOS + many Linux desktops with the `open` shim: use `open`.
    // Otherwise fall back to `xdg-open` (Linux) / `explorer` (Win).
    let (command, args) = file_manager_command(workspace_path);
    open_with(command, &args)
}

fn file_manager_command(workspace_path: &Path) -> (&'static str, Vec<String>) {
    #[cfg(target_os = "macos")]
    {
        return ("open", vec![workspace_path.display().to_string()]);
    }
    #[cfg(target_os = "linux")]
    {
        return ("xdg-open", vec![workspace_path.display().to_string()]);
    }
    #[cfg(target_os = "windows")]
    {
        return ("explorer", vec![workspace_path.display().to_string()]);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        ("open", vec![workspace_path.display().to_string()])
    }
}

// ---------------------------------------------------------------------------
// Tauri command surface
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn ide_get_preference(store: State<'_, IdePreferenceStore>) -> IdePreference {
    store.snapshot()
}

#[tauri::command]
pub fn ide_set_preference(
    preference: IdePreference,
    store: State<'_, IdePreferenceStore>,
) -> Result<IdePreference, IdeHandoffErrorDto> {
    store
        .set(preference)
        .map_err(|e| IdeHandoffErrorDto::from(&e))
}

#[tauri::command]
pub fn ide_open_workspace(
    workspace_path: String,
    store: State<'_, IdePreferenceStore>,
) -> Result<(), IdeHandoffErrorDto> {
    let pref = store.snapshot();
    open_workspace_in_ide(&pref, Path::new(&workspace_path))
        .map_err(|e| IdeHandoffErrorDto::from(&e))
}

/// Open a remote SSH workspace in the configured IDE using the
/// VS Code / Cursor remote URI scheme:
/// `vscode-remote://ssh-remote+[user@]host[:port]/path`
///
/// Cursor (and VS Code with Remote-SSH) recognise the `--folder-uri`
/// flag and connect to the remote host transparently.
#[tauri::command]
pub fn ide_open_remote_workspace(
    ssh_user: Option<String>,
    ssh_host: String,
    ssh_port: Option<u16>,
    remote_path: String,
    store: State<'_, IdePreferenceStore>,
) -> Result<(), IdeHandoffErrorDto> {
    let pref = store.snapshot();
    // Build the authority part: [user@]host[:port]
    let authority = match (ssh_user.as_deref(), ssh_port) {
        (Some(u), Some(p)) => format!("{u}@{ssh_host}:{p}"),
        (Some(u), None) => format!("{u}@{ssh_host}"),
        (None, Some(p)) => format!("{ssh_host}:{p}"),
        (None, None) => ssh_host.clone(),
    };
    // Encode the path so spaces and special chars survive URI parsing.
    // Simple percent-encode: replace space; leave / and alphanumerics.
    let encoded_path: String = remote_path
        .chars()
        .flat_map(|c| {
            if c == ' ' {
                "%20".chars().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect();
    let folder_uri = format!("vscode-remote://ssh-remote+{authority}{encoded_path}");

    // On macOS, use `open -a <AppName> --args --folder-uri <uri>`.
    // The `--args` flag tells open(1) to pass subsequent arguments to
    // the app rather than treating them as file paths.
    #[cfg(target_os = "macos")]
    {
        let app_name = match pref.ide_command.as_str() {
            "cursor" => Some("Cursor"),
            "code"   => Some("Visual Studio Code"),
            _        => None,
        };
        if let Some(app) = app_name {
            let result = std::process::Command::new("/usr/bin/open")
                .args(["-a", app, "--args", "--folder-uri", &folder_uri])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            if result.is_ok() {
                return Ok(());
            }
        }
    }

    let args = vec!["--folder-uri".to_string(), folder_uri];
    open_with(&pref.ide_command, &args).map_err(|e| IdeHandoffErrorDto::from(&e))
}

/// Open a running Docker container in Cursor/VS Code using the Dev Containers
/// extension. For a container on a remote SSH host, set DOCKER_HOST=ssh://host
/// so the local Docker client tunnels to the remote daemon via SSH, then use
/// the `attached-container` URI scheme which the Dev Containers extension
/// handles regardless of where the daemon is.
///
/// URI: vscode-remote://attached-container+HEX_JSON/path_in_container
/// where HEX_JSON = hex-encode({"containerName":"/container_name"})
#[tauri::command]
pub fn ide_open_docker_workspace(
    // SSH host to tunnel Docker through (if container is remote). None for local Docker.
    ssh_user: Option<String>,
    ssh_host: Option<String>,
    ssh_port: Option<u16>,
    container_id: String,
    cwd_in_container: String,
    store: State<'_, IdePreferenceStore>,
) -> Result<(), IdeHandoffErrorDto> {
    let pref = store.snapshot();

    // Build DOCKER_HOST for remote SSH tunneling.
    let docker_host = ssh_host.as_ref().map(|host| {
        match (ssh_user.as_deref(), ssh_port) {
            (Some(u), Some(p)) => format!("ssh://{u}@{host}:{p}"),
            (Some(u), None)    => format!("ssh://{u}@{host}"),
            (None, Some(p))    => format!("ssh://{host}:{p}"),
            (None, None)       => format!("ssh://{host}"),
        }
    });

    // Build the attached-container URI.
    // Hex-encode {"containerName":"/container_id"} — this is the format
    // the Dev Containers extension expects to identify the container.
    let container_json = format!(r#"{{"containerName":"/{container_id}"}}"#);
    let hex_config: String = container_json.bytes()
        .map(|b| format!("{b:02x}"))
        .collect();
    let folder_uri = format!("vscode-remote://attached-container+{hex_config}{cwd_in_container}");

    // Find the IDE binary — prefer the CLI binary over `open -a` so we
    // can pass environment variables (DOCKER_HOST) that the app inherits.
    let ide_bin = which_in_path(&pref.ide_command).or_else(|| {
        // macOS app bundle fallback
        #[cfg(target_os = "macos")]
        {
            let bundles: &[(&str, &str)] = &[
                ("cursor", "/Applications/Cursor.app/Contents/MacOS/cursor"),
                ("code",   "/Applications/Visual Studio Code.app/Contents/MacOS/Electron"),
            ];
            bundles.iter()
                .find(|(cmd, _)| *cmd == pref.ide_command.as_str())
                .and_then(|(_, path)| {
                    let p = std::path::Path::new(path);
                    if is_executable(p) { Some(p.to_path_buf()) } else { None }
                })
        }
        #[cfg(not(target_os = "macos"))]
        { None }
    });

    let Some(bin) = ide_bin else {
        return Err(IdeHandoffErrorDto::from(&IdeHandoffError::IdeNotInPath {
            command: pref.ide_command.clone(),
        }));
    };

    // On macOS, environment variables set on a spawned process are NOT
    // propagated through Electron's internal subprocess chain (the Dev
    // Containers extension spawns `docker` via Node.js, which resets the
    // env from the launchd session). Use `launchctl setenv` to inject
    // DOCKER_HOST into the GUI session environment so all GUI apps and
    // their subprocesses (including Cursor's extension host) can see it.
    #[cfg(target_os = "macos")]
    if let Some(ref dh) = docker_host {
        let _ = std::process::Command::new("launchctl")
            .args(["setenv", "DOCKER_HOST", dh])
            .status();
    }

    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("--folder-uri").arg(&folder_uri)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Also set on the spawned process as a belt-and-suspenders fallback.
    if let Some(dh) = docker_host {
        cmd.env("DOCKER_HOST", dh);
    }
    cmd.spawn().map_err(|e| IdeHandoffErrorDto::from(&IdeHandoffError::SpawnFailed {
        command: bin.display().to_string(),
        message: e.to_string(),
    }))?;
    Ok(())
}

#[tauri::command]
pub fn ide_reveal_in_finder(workspace_path: String) -> Result<(), IdeHandoffErrorDto> {
    reveal_in_finder(Path::new(&workspace_path)).map_err(|e| IdeHandoffErrorDto::from(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_store() -> (tempfile::TempDir, IdePreferenceStore) {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = IdePreferenceStore::load(tmp.path().to_path_buf());
        (tmp, store)
    }

    #[test]
    fn which_in_path_finds_existing_executable() {
        // `/bin/sh` is present on every Unix host the tests run on.
        let resolved = which_in_path("sh").or_else(|| which_in_path("env"));
        assert!(resolved.is_some(), "expected sh or env to resolve via PATH");
    }

    #[test]
    fn which_in_path_returns_none_for_nonexistent_command() {
        assert!(which_in_path("definitely-not-a-real-command-aZqW9").is_none());
    }

    #[test]
    fn which_in_path_rejects_empty_command() {
        assert!(which_in_path("").is_none());
    }

    #[test]
    fn fill_template_substitutes_path_placeholder() {
        let template = vec!["{path}".to_string(), "--reuse-window".to_string()];
        let mut subs = HashMap::new();
        subs.insert("path", "/Users/me/workspace");
        let out = fill_template(&template, &subs);
        assert_eq!(out, vec!["/Users/me/workspace", "--reuse-window"]);
    }

    #[test]
    fn fill_template_preserves_args_without_placeholders() {
        let template = vec!["--new-window".to_string(), "--wait".to_string()];
        let subs = HashMap::new();
        let out = fill_template(&template, &subs);
        assert_eq!(out, vec!["--new-window", "--wait"]);
    }

    #[test]
    fn fill_template_leaves_unknown_placeholder_literal() {
        let template = vec!["{unknown}".to_string()];
        let subs = HashMap::new();
        let out = fill_template(&template, &subs);
        assert_eq!(out, vec!["{unknown}"]);
    }

    #[test]
    fn load_returns_default_preference_when_file_missing() {
        let (_tmp, store) = fresh_store();
        let pref = store.snapshot();
        assert_eq!(pref.ide_command, "cursor");
        assert_eq!(pref.ide_args_template, vec!["{path}"]);
    }

    #[test]
    fn load_returns_default_when_file_malformed() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join(PREFERENCE_FILENAME), b"not json").unwrap();
        let store = IdePreferenceStore::load(tmp.path().to_path_buf());
        let pref = store.snapshot();
        assert_eq!(pref.ide_command, "cursor");
    }

    #[test]
    fn persist_round_trip() {
        let (tmp, store) = fresh_store();
        // Use a command we know exists so set() succeeds.
        let target_cmd = if which_in_path("sh").is_some() { "sh" } else { "env" };
        let next = IdePreference {
            ide_command: target_cmd.into(),
            ide_args_template: vec!["{path}".into(), "--demo".into()],
        };
        store.set(next.clone()).expect("set");
        let reloaded = IdePreferenceStore::load(tmp.path().to_path_buf());
        let pref = reloaded.snapshot();
        assert_eq!(pref.ide_command, target_cmd);
        assert_eq!(pref.ide_args_template, vec!["{path}", "--demo"]);
    }

    #[test]
    fn persist_writes_snake_case_keys_to_disk() {
        let (tmp, store) = fresh_store();
        let target_cmd = if which_in_path("sh").is_some() { "sh" } else { "env" };
        store
            .set(IdePreference {
                ide_command: target_cmd.into(),
                ide_args_template: vec!["{path}".into()],
            })
            .expect("set");
        let body =
            std::fs::read_to_string(tmp.path().join(PREFERENCE_FILENAME)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            v.get("ide_command").is_some(),
            "snake_case `ide_command` required on disk"
        );
        assert!(
            v.get("ide_args_template").is_some(),
            "snake_case `ide_args_template` required on disk"
        );
        // camelCase wire-only keys must not appear on disk:
        assert!(v.get("ideCommand").is_none());
        assert!(v.get("ideArgsTemplate").is_none());
    }

    #[test]
    fn validate_preference_rejects_command_not_in_path() {
        let (_tmp, store) = fresh_store();
        let bogus = IdePreference {
            ide_command: "definitely-not-a-real-command-aZqW9".into(),
            ide_args_template: vec!["{path}".into()],
        };
        let err = store.set(bogus).unwrap_err();
        match err {
            IdeHandoffError::IdeNotInPath { command } => {
                assert!(command.starts_with("definitely-not-a-real-command"));
            }
            other => panic!("expected IdeNotInPath; got {other:?}"),
        }
    }

    #[test]
    fn validate_preference_accepts_well_known_command_in_path() {
        let (_tmp, store) = fresh_store();
        let target_cmd = if which_in_path("sh").is_some() { "sh" } else { "env" };
        let pref = IdePreference {
            ide_command: target_cmd.into(),
            ide_args_template: vec!["{path}".into()],
        };
        let after = store.set(pref).expect("set should accept well-known command");
        assert_eq!(after.ide_command, target_cmd);
    }

    #[test]
    fn ensure_dir_rejects_missing_and_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("nope");
        match ensure_dir(&missing).unwrap_err() {
            IdeHandoffError::NotADirectory { .. } => {}
            other => panic!("expected NotADirectory for missing; got {other:?}"),
        }
        let file = tmp.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        match ensure_dir(&file).unwrap_err() {
            IdeHandoffError::NotADirectory { .. } => {}
            other => panic!("expected NotADirectory for file; got {other:?}"),
        }
    }

    #[test]
    fn error_dto_serializes_with_kind_discriminant() {
        let dto = IdeHandoffErrorDto::from(&IdeHandoffError::IdeNotInPath {
            command: "cursor".into(),
        });
        let v: serde_json::Value = serde_json::to_value(&dto).unwrap();
        assert_eq!(v.get("kind").and_then(|x| x.as_str()), Some("ideNotInPath"));
        assert_eq!(v.get("command").and_then(|x| x.as_str()), Some("cursor"));
    }
}
