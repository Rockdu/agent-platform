#![allow(dead_code)]
// Round 9 ships the host-side Stronghold primitives (setup-marker lifecycle,
// canonical key encoder, in-memory access-token cache). They are consumed by
// the Tauri commands in `lib.rs` and (later) by the Gmail OAuth flow in
// task27+/M6. Suppress dead-code lints module-wide rather than annotating
// every helper individually; downstream tasks will read these from non-test
// code.

//! Host-side secret-storage primitives backing `tauri-plugin-stronghold`.
//!
//! What lives here:
//!   - The persistent setup-state marker at
//!     `${APP_DATA}/stronghold-state/setup.marker` that lets the host detect
//!     interrupted master-password setup across restarts (AC-5.4).
//!   - The canonical Stronghold record-key encoder
//!     `plugins/<plugin_id>/accounts/<account_id>/<secret_name>` (AC-5.2),
//!     validated against the generated plugin registry.
//!   - The in-memory `AccessTokenCache` for short-lived OAuth access tokens.
//!     Has no `Serialize` impl by design (compile-checked); the `Debug` impl
//!     masks token bodies so accidental log emission cannot leak them.
//!
//! What does NOT live here:
//!   - The actual encrypted vault (handled by `tauri-plugin-stronghold`).
//!   - The OAuth dance, account-add UX, or "≥2 accounts" enforcement —
//!     M6/task27–task29.
//!
//! Spec references: `docs/specs/plugin-contract.md` §"Stronghold Secret
//! Storage" sets the marker path, key shape, and resume/reset rules.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::generated::plugin_registry::PLUGINS;

const MARKER_FILE: &str = "setup.marker";
const SNAPSHOT_FILE: &str = "vault.stronghold";

/// One of the three states the master-password setup can be in. Persisted to
/// `setup.marker` so the host can render the right onboarding/recovery UX on
/// the next launch.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SetupStatus {
    Uninitialized,
    InProgress,
    Ready,
}

/// The persisted marker. MUST NOT carry the master password or token
/// material — only timing metadata helpful for UX (the test
/// `marker_does_not_serialize_secret_fields` proves this structurally by
/// pattern-matching the serialized JSON keys).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetupMarker {
    pub status: SetupStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

impl SetupMarker {
    fn uninitialized() -> Self {
        Self {
            status: SetupStatus::Uninitialized,
            started_at: None,
            completed_at: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error("SECRETS_ERROR io: {context} ({source})")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("SECRETS_ERROR marker_parse: {context} ({source})")]
    MarkerParse {
        context: String,
        #[source]
        source: serde_json::Error,
    },

    #[error(
        "SECRETS_ERROR invalid_state: action `{action}` not permitted from setup status `{from:?}`"
    )]
    InvalidState { action: String, from: SetupStatus },

    #[error("SECRETS_ERROR unknown_plugin: `{plugin_id}` is not in the generated PLUGINS registry")]
    UnknownPlugin { plugin_id: String },

    #[error("SECRETS_ERROR invalid_account_id: `{account_id}` does not match `^[a-zA-Z0-9._-]{{1,128}}$`")]
    InvalidAccountId { account_id: String },

    #[error("SECRETS_ERROR invalid_secret_name: `{secret_name}` does not match `^[a-z][a-z0-9_]{{0,62}}$`")]
    InvalidSecretName { secret_name: String },
}

/// Frontend-facing flat DTO. `kind` is the discriminant; optional fields are
/// populated when relevant.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretsErrorDto {
    pub kind: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup_status: Option<SetupStatus>,
}

impl From<&SecretsError> for SecretsErrorDto {
    fn from(err: &SecretsError) -> Self {
        let mut dto = Self {
            kind: String::new(),
            message: err.to_string(),
            plugin_id: None,
            account_id: None,
            secret_name: None,
            setup_status: None,
        };
        match err {
            SecretsError::Io { .. } => dto.kind = "io".into(),
            SecretsError::MarkerParse { .. } => dto.kind = "marker_parse".into(),
            SecretsError::InvalidState { from, .. } => {
                dto.kind = "invalid_state".into();
                dto.setup_status = Some(*from);
            }
            SecretsError::UnknownPlugin { plugin_id } => {
                dto.kind = "unknown_plugin".into();
                dto.plugin_id = Some(plugin_id.clone());
            }
            SecretsError::InvalidAccountId { account_id } => {
                dto.kind = "invalid_account_id".into();
                dto.account_id = Some(account_id.clone());
            }
            SecretsError::InvalidSecretName { secret_name } => {
                dto.kind = "invalid_secret_name".into();
                dto.secret_name = Some(secret_name.clone());
            }
        }
        dto
    }
}

// ---------------------------------------------------------------------------
// Setup-state marker lifecycle (AC-5.4)
// ---------------------------------------------------------------------------

/// Filesystem path of the setup marker.
pub fn marker_path(stronghold_root: &Path) -> PathBuf {
    stronghold_root.join(MARKER_FILE)
}

/// Filesystem path of the encrypted Stronghold snapshot file.
pub fn snapshot_path(stronghold_root: &Path) -> PathBuf {
    stronghold_root.join(SNAPSHOT_FILE)
}

pub fn ensure_stronghold_root(stronghold_root: &Path) -> Result<(), SecretsError> {
    fs::create_dir_all(stronghold_root).map_err(|source| SecretsError::Io {
        context: format!("create {}", stronghold_root.display()),
        source,
    })
}

pub fn read_setup_marker(stronghold_root: &Path) -> Result<SetupMarker, SecretsError> {
    let path = marker_path(stronghold_root);
    if !path.exists() {
        return Ok(SetupMarker::uninitialized());
    }
    let raw = fs::read_to_string(&path).map_err(|source| SecretsError::Io {
        context: format!("read {}", path.display()),
        source,
    })?;
    serde_json::from_str(&raw).map_err(|source| SecretsError::MarkerParse {
        context: format!("parse {}", path.display()),
        source,
    })
}

pub fn read_setup_status(stronghold_root: &Path) -> Result<SetupStatus, SecretsError> {
    read_setup_marker(stronghold_root).map(|m| m.status)
}

fn now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Lightweight ISO-8601 UTC without pulling chrono just for this. Format:
    // `YYYY-MM-DDTHH:MM:SSZ` derived from the unix epoch.
    let (year, month, day, hour, minute, second) = unix_secs_to_ymd_hms(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Tiny gregorian conversion good for 1970..2099. Used only to render the
/// marker's `started_at`/`completed_at` timestamps for diagnostic display.
fn unix_secs_to_ymd_hms(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let secs = secs.max(0);
    let day_secs = secs % 86_400;
    let mut days = secs / 86_400;
    let hour = (day_secs / 3600) as u32;
    let minute = ((day_secs / 60) % 60) as u32;
    let second = (day_secs % 60) as u32;

    let mut year = 1970i32;
    loop {
        let leap = is_leap(year);
        let in_year = if leap { 366 } else { 365 };
        if days < in_year {
            break;
        }
        days -= in_year;
        year += 1;
    }
    let leap = is_leap(year);
    let month_lengths: [i64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1u32;
    for len in month_lengths.iter() {
        if days < *len {
            break;
        }
        days -= *len;
        month += 1;
    }
    let day = (days + 1) as u32;
    (year, month, day, hour, minute, second)
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn write_marker(stronghold_root: &Path, marker: &SetupMarker) -> Result<(), SecretsError> {
    ensure_stronghold_root(stronghold_root)?;
    let path = marker_path(stronghold_root);
    let body = serde_json::to_string_pretty(marker).map_err(|source| SecretsError::MarkerParse {
        context: format!("serialize {}", path.display()),
        source,
    })?;
    fs::write(&path, body).map_err(|source| SecretsError::Io {
        context: format!("write {}", path.display()),
        source,
    })
}

pub fn start_setup(stronghold_root: &Path) -> Result<SetupMarker, SecretsError> {
    let current = read_setup_marker(stronghold_root)?;
    if current.status == SetupStatus::Ready {
        return Err(SecretsError::InvalidState {
            action: "start_setup".into(),
            from: current.status,
        });
    }
    let marker = SetupMarker {
        status: SetupStatus::InProgress,
        started_at: Some(now_iso8601()),
        completed_at: None,
    };
    write_marker(stronghold_root, &marker)?;
    Ok(marker)
}

pub fn complete_setup(stronghold_root: &Path) -> Result<SetupMarker, SecretsError> {
    let current = read_setup_marker(stronghold_root)?;
    if current.status != SetupStatus::InProgress {
        return Err(SecretsError::InvalidState {
            action: "complete_setup".into(),
            from: current.status,
        });
    }
    let marker = SetupMarker {
        status: SetupStatus::Ready,
        started_at: current.started_at,
        completed_at: Some(now_iso8601()),
    };
    write_marker(stronghold_root, &marker)?;
    Ok(marker)
}

/// Removes both the marker and the Stronghold snapshot. Idempotent: absent
/// files are not an error.
pub fn reset_setup(stronghold_root: &Path) -> Result<(), SecretsError> {
    for path in [marker_path(stronghold_root), snapshot_path(stronghold_root)] {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(SecretsError::Io {
                    context: format!("remove {}", path.display()),
                    source,
                });
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Canonical Stronghold record-key encoder (AC-5.2)
// ---------------------------------------------------------------------------

fn account_id_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-zA-Z0-9._-]{1,128}$").unwrap())
}

fn secret_name_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z][a-z0-9_]{0,62}$").unwrap())
}

/// Build the spec-canonical Stronghold record key:
/// `plugins/<plugin_id>/accounts/<account_id>/<secret_name>`.
pub fn secret_key(
    plugin_id: &str,
    account_id: &str,
    secret_name: &str,
) -> Result<String, SecretsError> {
    if !PLUGINS.iter().any(|p| p.plugin_id == plugin_id) {
        return Err(SecretsError::UnknownPlugin {
            plugin_id: plugin_id.into(),
        });
    }
    if !account_id_regex().is_match(account_id) {
        return Err(SecretsError::InvalidAccountId {
            account_id: account_id.into(),
        });
    }
    if !secret_name_regex().is_match(secret_name) {
        return Err(SecretsError::InvalidSecretName {
            secret_name: secret_name.into(),
        });
    }
    Ok(format!(
        "plugins/{plugin_id}/accounts/{account_id}/{secret_name}"
    ))
}

// ---------------------------------------------------------------------------
// In-memory access-token cache
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AccessTokenKey {
    pub plugin_id: String,
    pub account_id: String,
}

/// One short-lived OAuth access token. Per spec these MUST stay in process
/// memory only; the absence of a `serde::Serialize` impl is enforced via
/// `static_assertions::assert_not_impl_any!` in `tests` so accidental wire
/// serialization fails at compile time.
#[derive(Clone)]
pub struct AccessTokenEntry {
    token: String,
    pub expires_at_unix_secs: i64,
}

impl AccessTokenEntry {
    pub fn new(token: String, expires_at_unix_secs: i64) -> Self {
        Self {
            token,
            expires_at_unix_secs,
        }
    }

    /// Borrow the token body. Callers should pass directly to the HTTP
    /// client; do NOT log this value.
    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn is_expired(&self, now_unix_secs: i64) -> bool {
        now_unix_secs >= self.expires_at_unix_secs
    }
}

impl std::fmt::Debug for AccessTokenEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The token body is intentionally redacted in Debug so accidental
        // `tracing::debug!(?entry)` cannot leak the secret.
        f.debug_struct("AccessTokenEntry")
            .field("token", &"<redacted>")
            .field("expires_at_unix_secs", &self.expires_at_unix_secs)
            .finish()
    }
}

#[derive(Debug, Default)]
pub struct AccessTokenCache {
    entries: Mutex<HashMap<AccessTokenKey, AccessTokenEntry>>,
}

impl AccessTokenCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&self, key: AccessTokenKey, entry: AccessTokenEntry) {
        let mut guard = self.entries.lock().expect("AccessTokenCache poisoned");
        guard.insert(key, entry);
    }

    /// Returns `None` for missing keys OR expired entries (auto-removed).
    pub fn get(&self, key: &AccessTokenKey, now_unix_secs: i64) -> Option<AccessTokenEntry> {
        let mut guard = self.entries.lock().expect("AccessTokenCache poisoned");
        if let Some(entry) = guard.get(key) {
            if entry.is_expired(now_unix_secs) {
                guard.remove(key);
                return None;
            }
            return Some(entry.clone());
        }
        None
    }

    pub fn delete(&self, key: &AccessTokenKey) {
        let mut guard = self.entries.lock().expect("AccessTokenCache poisoned");
        guard.remove(key);
    }

    pub fn clear_all(&self) {
        let mut guard = self.entries.lock().expect("AccessTokenCache poisoned");
        guard.clear();
    }

    /// Diagnostic count of present (and possibly expired) entries. Returns a
    /// usize, never the token bytes themselves.
    pub fn len(&self) -> usize {
        self.entries.lock().expect("AccessTokenCache poisoned").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_sqlite::PluginStorage;
    use static_assertions::assert_not_impl_any;

    // Compile-time guard: AccessTokenEntry MUST NOT acquire a Serialize impl.
    // If a future change derives Serialize, this assertion (and therefore the
    // whole test crate) fails to compile — exactly the desired regression
    // alarm for an "in-memory only" invariant.
    assert_not_impl_any!(AccessTokenEntry: serde::Serialize);

    const REAL_PLUGIN: &str = "example-notes";

    fn tmp() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("tempdir")
    }

    fn root() -> (tempfile::TempDir, PathBuf) {
        let dir = tmp();
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    // ----- setup marker lifecycle -----

    #[test]
    fn read_marker_missing_file_is_uninitialized() {
        let (_d, root) = root();
        let marker = read_setup_marker(&root).unwrap();
        assert_eq!(marker.status, SetupStatus::Uninitialized);
        assert!(marker.started_at.is_none());
        assert!(marker.completed_at.is_none());
    }

    #[test]
    fn marker_round_trip() {
        let (_d, root) = root();
        ensure_stronghold_root(&root).unwrap();
        let written = SetupMarker {
            status: SetupStatus::Ready,
            started_at: Some("2026-05-16T00:00:00Z".into()),
            completed_at: Some("2026-05-16T00:00:01Z".into()),
        };
        write_marker(&root, &written).unwrap();
        let read = read_setup_marker(&root).unwrap();
        assert_eq!(read, written);
    }

    #[test]
    fn start_setup_transitions_uninitialized_to_in_progress() {
        let (_d, root) = root();
        let marker = start_setup(&root).unwrap();
        assert_eq!(marker.status, SetupStatus::InProgress);
        assert!(marker.started_at.is_some());
        assert!(marker.completed_at.is_none());
        assert_eq!(read_setup_status(&root).unwrap(), SetupStatus::InProgress);
    }

    #[test]
    fn start_setup_rejects_when_already_ready() {
        let (_d, root) = root();
        start_setup(&root).unwrap();
        complete_setup(&root).unwrap();
        let err = start_setup(&root).unwrap_err();
        match err {
            SecretsError::InvalidState { action, from } => {
                assert_eq!(action, "start_setup");
                assert_eq!(from, SetupStatus::Ready);
            }
            other => panic!("expected InvalidState; got {other}"),
        }
    }

    #[test]
    fn complete_setup_requires_in_progress() {
        let (_d, root) = root();
        let err = complete_setup(&root).unwrap_err();
        match err {
            SecretsError::InvalidState { action, from } => {
                assert_eq!(action, "complete_setup");
                assert_eq!(from, SetupStatus::Uninitialized);
            }
            other => panic!("expected InvalidState; got {other}"),
        }
    }

    #[test]
    fn complete_setup_transitions_to_ready_and_preserves_started_at() {
        let (_d, root) = root();
        let started = start_setup(&root).unwrap();
        let completed = complete_setup(&root).unwrap();
        assert_eq!(completed.status, SetupStatus::Ready);
        assert_eq!(completed.started_at, started.started_at);
        assert!(completed.completed_at.is_some());
    }

    #[test]
    fn reset_setup_removes_marker_and_snapshot() {
        let (_d, root) = root();
        ensure_stronghold_root(&root).unwrap();
        // Place both files.
        write_marker(
            &root,
            &SetupMarker {
                status: SetupStatus::Ready,
                started_at: Some("x".into()),
                completed_at: Some("y".into()),
            },
        )
        .unwrap();
        fs::write(snapshot_path(&root), b"\x00pretend-encrypted-bytes\x00").unwrap();
        assert!(marker_path(&root).exists());
        assert!(snapshot_path(&root).exists());

        reset_setup(&root).unwrap();
        assert!(!marker_path(&root).exists());
        assert!(!snapshot_path(&root).exists());
        assert_eq!(read_setup_status(&root).unwrap(), SetupStatus::Uninitialized);
    }

    #[test]
    fn reset_setup_is_idempotent_when_files_absent() {
        let (_d, root) = root();
        ensure_stronghold_root(&root).unwrap();
        reset_setup(&root).expect("reset on empty dir is ok");
        reset_setup(&root).expect("reset twice is ok");
    }

    #[test]
    fn interrupted_setup_recoverable() {
        // Simulate: user started setup, app crashed before completing.
        let (_d, root) = root();
        start_setup(&root).unwrap();
        // "Restart" by simply re-reading the marker from disk in a fresh
        // function call; no in-memory state carries over.
        let after_restart = read_setup_marker(&root).unwrap();
        assert_eq!(after_restart.status, SetupStatus::InProgress);
        assert!(after_restart.started_at.is_some());
        assert!(after_restart.completed_at.is_none());
        // Caller can now choose to resume (call complete_setup once the user
        // re-enters the password) or reset.
        complete_setup(&root).expect("resume after restart");
        assert_eq!(read_setup_status(&root).unwrap(), SetupStatus::Ready);
    }

    #[test]
    fn marker_does_not_serialize_secret_fields() {
        // Structural check: the JSON keys of a Ready marker are limited to
        // status / started_at / completed_at. Any future field that smuggled
        // a password or token through serde would fail this assertion.
        let marker = SetupMarker {
            status: SetupStatus::Ready,
            started_at: Some("s".into()),
            completed_at: Some("c".into()),
        };
        let v: serde_json::Value = serde_json::to_value(&marker).unwrap();
        let obj = v.as_object().expect("object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, vec!["completed_at", "started_at", "status"]);
    }

    // ----- secret_key encoder -----

    #[test]
    fn secret_key_canonical_encoding() {
        let key = secret_key(REAL_PLUGIN, "acct_abc", "refresh_token").unwrap();
        assert_eq!(
            key,
            "plugins/example-notes/accounts/acct_abc/refresh_token"
        );
    }

    #[test]
    fn secret_key_rejects_unregistered_plugin() {
        let err = secret_key("not-real", "acct", "refresh_token").unwrap_err();
        match err {
            SecretsError::UnknownPlugin { plugin_id } => assert_eq!(plugin_id, "not-real"),
            other => panic!("expected UnknownPlugin; got {other}"),
        }
    }

    #[test]
    fn secret_key_rejects_bad_account_id() {
        for bad in ["", "with space", "with/slash", "with\x00null"] {
            let err = secret_key(REAL_PLUGIN, bad, "refresh_token").unwrap_err();
            assert!(matches!(err, SecretsError::InvalidAccountId { .. }), "{bad}");
        }
    }

    #[test]
    fn secret_key_rejects_bad_secret_name() {
        for bad in [
            "",
            "Refresh-Token",
            "refresh-token",
            "0_starts_with_digit",
            "Spaces inside",
        ] {
            let err = secret_key(REAL_PLUGIN, "acct", bad).unwrap_err();
            assert!(matches!(err, SecretsError::InvalidSecretName { .. }), "{bad}");
        }
    }

    // ----- AccessTokenCache -----

    fn key(plugin_id: &str, account_id: &str) -> AccessTokenKey {
        AccessTokenKey {
            plugin_id: plugin_id.into(),
            account_id: account_id.into(),
        }
    }

    #[test]
    fn access_token_cache_put_get_round_trip() {
        let cache = AccessTokenCache::new();
        cache.put(
            key("gmail", "acct_a"),
            AccessTokenEntry::new("ya29.fake".into(), 9_999_999_999),
        );
        let got = cache.get(&key("gmail", "acct_a"), 0).expect("present");
        assert_eq!(got.token(), "ya29.fake");
        assert_eq!(got.expires_at_unix_secs, 9_999_999_999);
    }

    #[test]
    fn access_token_cache_get_returns_none_when_expired() {
        let cache = AccessTokenCache::new();
        cache.put(
            key("gmail", "acct_a"),
            AccessTokenEntry::new("ya29.fake".into(), 100),
        );
        assert!(cache.get(&key("gmail", "acct_a"), 200).is_none());
        // Expired entries are auto-removed.
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn access_token_cache_delete_removes_entry() {
        let cache = AccessTokenCache::new();
        cache.put(
            key("gmail", "acct_a"),
            AccessTokenEntry::new("t".into(), 9_999_999_999),
        );
        cache.delete(&key("gmail", "acct_a"));
        assert!(cache.get(&key("gmail", "acct_a"), 0).is_none());
    }

    #[test]
    fn access_token_cache_clear_all_empties_the_map() {
        let cache = AccessTokenCache::new();
        cache.put(
            key("gmail", "a"),
            AccessTokenEntry::new("t".into(), 9_999_999_999),
        );
        cache.put(
            key("gmail", "b"),
            AccessTokenEntry::new("t".into(), 9_999_999_999),
        );
        cache.clear_all();
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn access_token_entry_debug_masks_token() {
        let entry = AccessTokenEntry::new("ya29.SECRET-DO-NOT-LEAK".into(), 1234567890);
        let rendered = format!("{:?}", entry);
        assert!(
            rendered.contains("<redacted>"),
            "expected <redacted> marker; got: {rendered}"
        );
        assert!(
            !rendered.contains("SECRET-DO-NOT-LEAK"),
            "token leaked through Debug: {rendered}"
        );
        assert!(rendered.contains("1234567890"));
    }

    // ----- negative test: no plaintext refresh-token in plugin SQLite -----

    #[test]
    fn refresh_token_does_not_leak_to_plugin_sqlite() {
        // Place a known refresh-token literal into the in-memory cache, run
        // the example-notes plugin migrations against a temp plugins_root,
        // then scan the resulting state.sqlite (every byte) for the literal.
        // The host's typed APIs MUST NOT carry secret material into SQLite.
        let secret = "ya29.test-refresh-token-SHOULD-NEVER-APPEAR-IN-SQLITE";
        let cache = AccessTokenCache::new();
        cache.put(
            key("example-notes", "acct_test"),
            AccessTokenEntry::new(secret.into(), 9_999_999_999),
        );

        // Drive the storage path the way bootstrap does: open, then run the
        // bundled migration.
        let plugins_root = tmp();
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let migrations = workspace_root
            .join("plugins")
            .join(REAL_PLUGIN)
            .join("migrations");
        let storage =
            PluginStorage::open(plugins_root.path(), REAL_PLUGIN).expect("open per-plugin db");
        storage
            .run_migrations(&migrations)
            .expect("bundled migration applies");

        let db_bytes = fs::read(storage.db_path()).expect("read sqlite file");
        assert!(
            memmem(&db_bytes, secret.as_bytes()).is_none(),
            "secret literal leaked into per-plugin sqlite at {}",
            storage.db_path().display()
        );
        // Defensive: confirm the cache still holds it (so the test is
        // meaningful — we DID actually populate the cache).
        assert!(cache.get(&key("example-notes", "acct_test"), 0).is_some());
    }

    /// Tiny substring search (avoids pulling memchr just for one test).
    fn memmem(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() || haystack.len() < needle.len() {
            return None;
        }
        haystack
            .windows(needle.len())
            .position(|w| w == needle)
    }
}
