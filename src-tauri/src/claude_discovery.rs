//! `claude` CLI PATH discovery + persistence (AC-2.3 / AC-3.2).
//!
//! See `docs/specs/claude-launch.md` for the contract. The host owns
//! discovery because Finder/Dock-launched macOS apps do not inherit the
//! user's interactive shell PATH; we cannot trust `command -v claude`
//! resolution inside the Tauri process by default.
//!
//! Probe sequence per spec:
//! 1. Cached record in `${APP_DATA}/claude-config.json` (re-validated each boot).
//! 2. Known install paths in order: `/opt/homebrew/bin/claude`,
//!    `/usr/local/bin/claude`, `~/.local/bin/claude`, `~/.npm-global/bin/claude`.
//! 3. `bash -lc 'command -v claude'` (login-shell PATH lookup).
//! 4. `ClaudeNotFound { probed }` if all fail.

use std::path::{Path, PathBuf};

use serde::Serialize;

const CONFIG_FILENAME: &str = "claude-config.json";
const KNOWN_PATHS: &[&str] = &[
    "/opt/homebrew/bin/claude",
    "/usr/local/bin/claude",
    "~/.local/bin/claude",
    "~/.npm-global/bin/claude",
];
const BASH_PROBE: &str = "bash -lc 'command -v claude'";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudePathRecord {
    pub path: PathBuf,
    pub version: Option<String>,
    pub discovered_at: String,
}

/// Persisted on-disk shape per spec. Snake_case to match the spec
/// example literally; the wire type for the frontend is camelCase.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredClaudeConfig {
    claude_path: Option<StoredRecord>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredRecord {
    path: PathBuf,
    version: Option<String>,
    discovered_at: String,
}

impl From<&ClaudePathRecord> for StoredRecord {
    fn from(r: &ClaudePathRecord) -> Self {
        Self {
            path: r.path.clone(),
            version: r.version.clone(),
            discovered_at: r.discovered_at.clone(),
        }
    }
}

impl From<StoredRecord> for ClaudePathRecord {
    fn from(r: StoredRecord) -> Self {
        Self {
            path: r.path,
            version: r.version,
            discovered_at: r.discovered_at,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClaudeDiscoveryError {
    #[error("claude binary not found; probed: {probed:?}")]
    ClaudeNotFound { probed: Vec<String> },

    #[error("path `{path}` does not exist or is not executable")]
    NotExecutable { path: PathBuf },

    /// Reserved for surfacing `claude --version` execution failures
    /// (non-zero exit, signal, etc.) once we treat them as hard errors
    /// instead of silent `version: None`. Task20 plans to gate on this.
    #[allow(dead_code)]
    #[error("could not probe `claude --version` at `{path}`: {message}")]
    VersionProbeFailed { path: PathBuf, message: String },

    #[error("io error in `{context}`: {message}")]
    Io { context: String, message: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ClaudeDiscoveryErrorDto {
    ClaudeNotFound { probed: Vec<String> },
    NotExecutable { path: String },
    VersionProbeFailed { path: String, message: String },
    Io { context: String, message: String },
}

impl From<&ClaudeDiscoveryError> for ClaudeDiscoveryErrorDto {
    fn from(err: &ClaudeDiscoveryError) -> Self {
        match err {
            ClaudeDiscoveryError::ClaudeNotFound { probed } => Self::ClaudeNotFound {
                probed: probed.clone(),
            },
            ClaudeDiscoveryError::NotExecutable { path } => Self::NotExecutable {
                path: path.display().to_string(),
            },
            ClaudeDiscoveryError::VersionProbeFailed { path, message } => {
                Self::VersionProbeFailed {
                    path: path.display().to_string(),
                    message: message.clone(),
                }
            }
            ClaudeDiscoveryError::Io { context, message } => Self::Io {
                context: context.clone(),
                message: message.clone(),
            },
        }
    }
}

/// Discriminated wire shape consumed by the frontend onboarding card.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClaudeDiscoveryStatus {
    /// Discovery has run and resolved a usable `claude` binary.
    Ready { record: ClaudePathRecord },
    /// Discovery has run but no candidate matched.
    NotFound { probed: Vec<String> },
    /// Bootstrap setup hook has not completed yet. Defensive; should not
    /// be observable in practice because `setup` runs before the
    /// frontend can call `claude_discovery_status`.
    NotRun,
}

fn expand_tilde(raw: &str, home: &Path) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(raw)
    }
}

fn exists_and_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    meta.permissions().mode() & 0o111 != 0
}

fn now_rfc3339() -> String {
    // RFC3339 without an external chrono dep. Format:
    // YYYY-MM-DDTHH:MM:SSZ derived from SystemTime since UNIX_EPOCH.
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() as i64;
    format_rfc3339_utc(secs)
}

fn format_rfc3339_utc(unix_secs: i64) -> String {
    // Civil-from-days algorithm (Howard Hinnant). Sufficient for our
    // logging/persisted timestamp; we don't need sub-second precision.
    let days = unix_secs.div_euclid(86_400);
    let secs_of_day = unix_secs.rem_euclid(86_400) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    let h = secs_of_day / 3600;
    let mi = (secs_of_day / 60) % 60;
    let s = secs_of_day % 60;
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Run the spec probe sequence in order. Persists the resolved record to
/// `${app_data}/claude-config.json` on success.
pub fn discover(home: &Path, app_data: &Path) -> Result<ClaudePathRecord, ClaudeDiscoveryError> {
    if let Some(cached) = read_cache(app_data) {
        if exists_and_executable(&cached.path) {
            tracing::info!(path = %cached.path.display(), "claude_discovery: cached path reused");
            return Ok(cached);
        }
    }

    let mut probed: Vec<String> = Vec::new();
    for raw in KNOWN_PATHS {
        let candidate = expand_tilde(raw, home);
        probed.push(candidate.display().to_string());
        if exists_and_executable(&candidate) {
            return finalize(candidate, app_data);
        }
    }

    // Login-shell PATH lookup. Critical on macOS where Finder/Dock-launched
    // apps don't inherit the user's interactive PATH (`brew shellenv`,
    // nvm/fnm, etc.). Spec: `bash -lc 'command -v claude'`.
    probed.push(BASH_PROBE.to_string());
    if let Some(path) = bash_command_v_claude() {
        if exists_and_executable(&path) {
            return finalize(path, app_data);
        }
    }

    Err(ClaudeDiscoveryError::ClaudeNotFound { probed })
}

/// Bootstrap-time entry point: trust the cache fast-path if its `path`
/// still resolves, else fall back to a full re-probe and overwrite the
/// stored record.
pub fn validate_or_rediscover(
    home: &Path,
    app_data: &Path,
) -> Result<ClaudePathRecord, ClaudeDiscoveryError> {
    discover(home, app_data)
}

/// User-supplied path override. Validates `exists_and_executable`, runs
/// `--version`, and persists. The settings UI surfaces this via a
/// future Tauri command.
pub fn set_override(
    path: PathBuf,
    app_data: &Path,
) -> Result<ClaudePathRecord, ClaudeDiscoveryError> {
    if !exists_and_executable(&path) {
        return Err(ClaudeDiscoveryError::NotExecutable { path });
    }
    finalize(path, app_data)
}

fn finalize(path: PathBuf, app_data: &Path) -> Result<ClaudePathRecord, ClaudeDiscoveryError> {
    let version = probe_version(&path);
    let record = ClaudePathRecord {
        path: path.clone(),
        version,
        discovered_at: now_rfc3339(),
    };
    if let Err(err) = write_cache(app_data, &record) {
        tracing::warn!(%err, "claude_discovery: failed to persist claude-config.json (non-fatal)");
    }
    Ok(record)
}

fn read_cache(app_data: &Path) -> Option<ClaudePathRecord> {
    let path = app_data.join(CONFIG_FILENAME);
    let bytes = std::fs::read(&path).ok()?;
    let stored: StoredClaudeConfig = serde_json::from_slice(&bytes).ok()?;
    stored.claude_path.map(ClaudePathRecord::from)
}

fn write_cache(app_data: &Path, record: &ClaudePathRecord) -> Result<(), ClaudeDiscoveryError> {
    let path = app_data.join(CONFIG_FILENAME);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ClaudeDiscoveryError::Io {
            context: format!("create_dir_all {}", parent.display()),
            message: e.to_string(),
        })?;
    }
    let doc = StoredClaudeConfig {
        claude_path: Some(StoredRecord::from(record)),
    };
    let body = serde_json::to_vec_pretty(&doc).map_err(|e| ClaudeDiscoveryError::Io {
        context: "serialize claude-config.json".to_string(),
        message: e.to_string(),
    })?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body).map_err(|e| ClaudeDiscoveryError::Io {
        context: format!("write {}", tmp.display()),
        message: e.to_string(),
    })?;
    std::fs::rename(&tmp, &path).map_err(|e| ClaudeDiscoveryError::Io {
        context: format!("rename {} -> {}", tmp.display(), path.display()),
        message: e.to_string(),
    })?;
    Ok(())
}

fn probe_version(path: &Path) -> Option<String> {
    let output = std::process::Command::new(path).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_version(&stdout)
}

fn bash_command_v_claude() -> Option<PathBuf> {
    let output = std::process::Command::new("bash")
        .arg("-lc")
        .arg("command -v claude")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if line.is_empty() {
        return None;
    }
    Some(PathBuf::from(line))
}

/// Find the first `\d+\.\d+\.\d+` token in `stdout`.
pub fn parse_version(stdout: &str) -> Option<String> {
    static VERSION_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = VERSION_RE.get_or_init(|| regex::Regex::new(r"\d+\.\d+\.\d+").unwrap());
    re.find(stdout).map(|m| m.as_str().to_string())
}

/// Per spec: `>= 2.0.0` supported; older or unparseable allowed with a
/// nudge — this helper is for the nudge gate. Wired in by task20 when
/// the orchestrator launch path actually shells out to `claude`.
#[allow(dead_code)]
pub fn version_is_supported(version: &str) -> bool {
    let parts: Vec<&str> = version.split('.').collect();
    let Some(major_raw) = parts.first() else {
        return false;
    };
    matches!(major_raw.parse::<u32>(), Ok(major) if major >= 2)
}

// ---------------------------------------------------------------------------
// Tauri command surface
// ---------------------------------------------------------------------------

/// Snapshot stored at bootstrap so frontend reads don't race against the
/// initial discovery probe. Populated by `lib.rs::run.setup`.
pub struct DiscoveryCache {
    inner: std::sync::Mutex<Option<Result<ClaudePathRecord, ClaudeDiscoveryError>>>,
}

impl DiscoveryCache {
    pub fn empty() -> Self {
        Self {
            inner: std::sync::Mutex::new(None),
        }
    }

    pub fn store(&self, result: Result<ClaudePathRecord, ClaudeDiscoveryError>) {
        let mut guard = self.inner.lock().expect("DiscoveryCache poisoned");
        *guard = Some(result);
    }

    pub fn snapshot(&self) -> Option<Result<ClaudePathRecord, ClaudeDiscoveryError>> {
        let guard = self.inner.lock().expect("DiscoveryCache poisoned");
        match guard.as_ref()? {
            Ok(rec) => Some(Ok(rec.clone())),
            Err(err) => Some(Err(clone_error(err))),
        }
    }
}

pub(crate) fn clone_error(err: &ClaudeDiscoveryError) -> ClaudeDiscoveryError {
    match err {
        ClaudeDiscoveryError::ClaudeNotFound { probed } => {
            ClaudeDiscoveryError::ClaudeNotFound {
                probed: probed.clone(),
            }
        }
        ClaudeDiscoveryError::NotExecutable { path } => {
            ClaudeDiscoveryError::NotExecutable { path: path.clone() }
        }
        ClaudeDiscoveryError::VersionProbeFailed { path, message } => {
            ClaudeDiscoveryError::VersionProbeFailed {
                path: path.clone(),
                message: message.clone(),
            }
        }
        ClaudeDiscoveryError::Io { context, message } => ClaudeDiscoveryError::Io {
            context: context.clone(),
            message: message.clone(),
        },
    }
}

#[tauri::command]
pub fn claude_discovery_status(
    cache: tauri::State<'_, DiscoveryCache>,
) -> Result<ClaudeDiscoveryStatus, ClaudeDiscoveryErrorDto> {
    match cache.snapshot() {
        Some(Ok(record)) => Ok(ClaudeDiscoveryStatus::Ready { record }),
        Some(Err(ClaudeDiscoveryError::ClaudeNotFound { probed })) => {
            Ok(ClaudeDiscoveryStatus::NotFound { probed })
        }
        Some(Err(other)) => Err(ClaudeDiscoveryErrorDto::from(&other)),
        None => Ok(ClaudeDiscoveryStatus::NotRun),
    }
}

#[tauri::command]
pub async fn claude_redo_discovery(
    cache: tauri::State<'_, DiscoveryCache>,
) -> Result<ClaudePathRecord, ClaudeDiscoveryErrorDto> {
    let (home, app_data) = match resolve_dirs() {
        Some(pair) => pair,
        None => {
            return Err(ClaudeDiscoveryErrorDto::Io {
                context: "resolve home/app_data".into(),
                message: "bootstrap dirs unavailable".into(),
            });
        }
    };
    let result = discover(&home, &app_data);
    cache.store(match &result {
        Ok(r) => Ok(r.clone()),
        Err(e) => Err(clone_error(e)),
    });
    result.map_err(|e| ClaudeDiscoveryErrorDto::from(&e))
}

#[tauri::command]
pub async fn claude_set_path_override(
    path: String,
    cache: tauri::State<'_, DiscoveryCache>,
) -> Result<ClaudePathRecord, ClaudeDiscoveryErrorDto> {
    let (_, app_data) = match resolve_dirs() {
        Some(pair) => pair,
        None => {
            return Err(ClaudeDiscoveryErrorDto::Io {
                context: "resolve app_data".into(),
                message: "bootstrap dirs unavailable".into(),
            });
        }
    };
    let result = set_override(PathBuf::from(path), &app_data);
    cache.store(match &result {
        Ok(r) => Ok(r.clone()),
        Err(e) => Err(clone_error(e)),
    });
    result.map_err(|e| ClaudeDiscoveryErrorDto::from(&e))
}

fn resolve_dirs() -> Option<(PathBuf, PathBuf)> {
    let base = directories::BaseDirs::new()?;
    let project = directories::ProjectDirs::from("com", "agentplatform", "app")?;
    Some((
        base.home_dir().to_path_buf(),
        project.data_dir().to_path_buf(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_handles_canonical_output() {
        assert_eq!(parse_version("Claude Code 2.0.1\n"), Some("2.0.1".into()));
    }

    #[test]
    fn parse_version_handles_lowercase_and_extra() {
        assert_eq!(
            parse_version("claude 2.1.128 (linux-x64)\n"),
            Some("2.1.128".into())
        );
    }

    #[test]
    fn parse_version_returns_none_for_garbage() {
        assert_eq!(parse_version("garbage no version"), None);
    }

    #[test]
    fn parse_version_picks_first_triplet_when_multiple_present() {
        assert_eq!(parse_version("Claude Code 2.0.1 / build 9.0.0"), Some("2.0.1".into()));
    }

    #[test]
    fn version_is_supported_recognizes_2_x() {
        assert!(version_is_supported("2.0.0"));
        assert!(version_is_supported("2.1.0"));
        assert!(version_is_supported("3.0.0"));
        assert!(!version_is_supported("1.9.99"));
        assert!(!version_is_supported("garbage"));
    }

    #[test]
    fn expand_tilde_replaces_leading_tilde_slash() {
        let home = std::path::Path::new("/tmp/fakehome");
        assert_eq!(
            expand_tilde("~/.local/bin/claude", home),
            PathBuf::from("/tmp/fakehome/.local/bin/claude")
        );
        assert_eq!(
            expand_tilde("/usr/bin/claude", home),
            PathBuf::from("/usr/bin/claude")
        );
    }

    /// Returns a temp dir where neither the known install paths nor the
    /// login-shell PATH can resolve `claude`. We achieve "no login-shell
    /// claude" by overriding $PATH and HOME for the bash subprocess: the
    /// only reliable cross-environment trick is to point HOME at a dir
    /// where `~/.bashrc` and `~/.profile` are empty. But that doesn't
    /// override the user's compiled-in /usr/local/bin/claude. So this
    /// test is gated to environments where `/opt/homebrew/bin/claude`,
    /// `/usr/local/bin/claude` etc. don't exist; CI/dev containers
    /// satisfy that. We skip the assertion if the host machine has any
    /// of those.
    fn host_has_any_real_known_path() -> bool {
        KNOWN_PATHS
            .iter()
            .filter(|p| !p.starts_with("~"))
            .any(|p| exists_and_executable(std::path::Path::new(p)))
    }

    #[test]
    fn discover_uses_first_known_path_when_present() {
        let tmp_home = tempfile::TempDir::new().unwrap();
        let app_data = tempfile::TempDir::new().unwrap();
        // Place a stub at ~/.local/bin/claude. KNOWN_PATHS[2] is
        // `~/.local/bin/claude`; it sorts after the two absolute paths,
        // so we can only assert that discovery picks it up if the host
        // doesn't already have a real /opt/homebrew or /usr/local
        // claude. We allow either: assert the returned path is
        // executable AND resolved through one of the known candidates.
        let stub_dir = tmp_home.path().join(".local").join("bin");
        std::fs::create_dir_all(&stub_dir).unwrap();
        let stub = stub_dir.join("claude");
        std::fs::write(&stub, "#!/bin/sh\necho 'Claude Code 2.0.0'\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let record = discover(tmp_home.path(), app_data.path()).expect("must resolve");
        assert!(exists_and_executable(&record.path));
        // If the host has a real /opt/homebrew or /usr/local claude, the
        // first known-path probe will resolve to that; otherwise it
        // resolves to our stub. Both are valid spec outcomes.
        if !host_has_any_real_known_path() {
            assert_eq!(record.path, stub, "stub should win when no real install exists");
            assert_eq!(record.version, Some("2.0.0".into()));
        }
    }

    #[test]
    fn discover_persists_record_to_claude_config_json() {
        if host_has_any_real_known_path() {
            // We can't predict the path that wins on hosts with a real
            // claude install; just assert that the file is written.
        }
        let tmp_home = tempfile::TempDir::new().unwrap();
        let app_data = tempfile::TempDir::new().unwrap();
        let stub_dir = tmp_home.path().join(".local").join("bin");
        std::fs::create_dir_all(&stub_dir).unwrap();
        let stub = stub_dir.join("claude");
        std::fs::write(&stub, "#!/bin/sh\necho 'Claude Code 2.0.0'\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let _ = discover(tmp_home.path(), app_data.path()).expect("must resolve");
        let cfg_path = app_data.path().join(CONFIG_FILENAME);
        assert!(cfg_path.is_file(), "claude-config.json must be written");
        let body = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(body.contains("\"claude_path\""), "spec field name required");
        assert!(body.contains("\"path\""));
        assert!(body.contains("\"discovered_at\""));
    }

    #[test]
    fn validate_or_rediscover_clears_stale_cache_when_path_missing() {
        let tmp_home = tempfile::TempDir::new().unwrap();
        let app_data = tempfile::TempDir::new().unwrap();
        // Seed a cache pointing at a non-existent path.
        let stale = StoredClaudeConfig {
            claude_path: Some(StoredRecord {
                path: PathBuf::from("/nonexistent/never-existed/claude"),
                version: Some("1.0.0".into()),
                discovered_at: "2026-01-01T00:00:00Z".into(),
            }),
        };
        std::fs::write(
            app_data.path().join(CONFIG_FILENAME),
            serde_json::to_vec_pretty(&stale).unwrap(),
        )
        .unwrap();

        // Stub at ~/.local/bin/claude so re-discovery resolves on hosts
        // with no real install. On hosts with a real install, the first
        // known path wins; either way the stale cache path must not be
        // the result.
        let stub_dir = tmp_home.path().join(".local").join("bin");
        std::fs::create_dir_all(&stub_dir).unwrap();
        let stub = stub_dir.join("claude");
        std::fs::write(&stub, "#!/bin/sh\necho 'Claude Code 2.0.0'\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let record =
            validate_or_rediscover(tmp_home.path(), app_data.path()).expect("must re-resolve");
        assert_ne!(record.path, PathBuf::from("/nonexistent/never-existed/claude"));
        assert!(exists_and_executable(&record.path));
    }

    #[test]
    fn set_override_rejects_non_executable_path() {
        let app_data = tempfile::TempDir::new().unwrap();
        let bogus = app_data.path().join("not-a-binary");
        std::fs::write(&bogus, "plain text not executable").unwrap();
        let err = set_override(bogus.clone(), app_data.path()).unwrap_err();
        match err {
            ClaudeDiscoveryError::NotExecutable { path } => assert_eq!(path, bogus),
            other => panic!("expected NotExecutable; got {other:?}"),
        }
    }

    #[test]
    fn discovery_error_dto_serializes_with_kind_discriminant() {
        let dto = ClaudeDiscoveryErrorDto::from(&ClaudeDiscoveryError::ClaudeNotFound {
            probed: vec!["/opt/homebrew/bin/claude".into(), BASH_PROBE.into()],
        });
        let v: serde_json::Value = serde_json::to_value(&dto).unwrap();
        assert_eq!(v.get("kind").and_then(|x| x.as_str()), Some("claudeNotFound"));
        assert!(v.get("probed").is_some());
    }

    #[test]
    fn status_serializes_with_snake_case_kind() {
        let s = ClaudeDiscoveryStatus::NotFound {
            probed: vec!["a".into()],
        };
        let v: serde_json::Value = serde_json::to_value(&s).unwrap();
        assert_eq!(v.get("kind").and_then(|x| x.as_str()), Some("not_found"));
    }

    #[test]
    fn format_rfc3339_utc_matches_known_epoch() {
        // 1970-01-01T00:00:00Z = 0
        assert_eq!(format_rfc3339_utc(0), "1970-01-01T00:00:00Z");
        // 2026-05-20T12:00:00Z = 1779278400
        assert_eq!(format_rfc3339_utc(1_779_278_400), "2026-05-20T12:00:00Z");
        // 2000-02-29T00:00:00Z = 951782400 (leap-year boundary)
        assert_eq!(format_rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z");
        // 2026-01-01T00:00:00Z = 1767225600 (start-of-year)
        assert_eq!(format_rfc3339_utc(1_767_225_600), "2026-01-01T00:00:00Z");
    }
}
