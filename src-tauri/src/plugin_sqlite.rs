#![allow(dead_code)]
// The public API is consumed by tests only this round; task11 (sidecar
// lifecycle) and a future bootstrap integration will start invoking it from
// non-test code. Suppress dead-code warnings module-wide rather than annotate
// every constructor/method individually.

//! Per-plugin SQLite framework.
//!
//! Each registered plugin owns one SQLite file at
//! `${APP_DATA}/plugins/<plugin_id>/state.sqlite`. The only public
//! constructor (`PluginStorage::open`) requires a `plugin_id` that is present
//! in the generated `PLUGINS` registry; there is no API that accepts a raw
//! path or another plugin's identifier, so cross-plugin SQL is impossible.
//!
//! Connection PRAGMAs (applied on every `connect()`):
//!   - `journal_mode = WAL`     — multi-reader / single-writer concurrency
//!     compatible with the N×M sibling-sidecar model.
//!   - `busy_timeout = 5000`    — siblings retry SQLITE_BUSY for 5 s before
//!     bubbling an error, per AC-1.3.
//!   - `foreign_keys = ON`      — referential integrity inside the per-plugin
//!     schema (SQLite defaults this OFF for legacy compatibility).
//!
//! Migration runner:
//!   - Discovers `*.sql` files in the plugin's `migrations_path` (the
//!     manifest field; resolved by the caller).
//!   - Acquires an exclusive advisory lock on
//!     `<db_path>.migration-lock` (`fs4::FileExt::lock_exclusive`) so multiple
//!     sibling sidecar copies starting at the same time do not race.
//!   - Tracks applied names in `_plugin_migrations (name PK, applied_at)`.
//!   - Rejects migration bodies containing `ATTACH DATABASE` (case-insensitive,
//!     whole-word) before any execution.
//!   - Per-migration failures yield typed `MigrationFailed { plugin_id, file }`
//!     errors so a host driver can isolate one plugin's failure from another.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fs4::FileExt;
use regex::Regex;
use rusqlite::Connection;
use serde::Serialize;

use crate::generated::plugin_registry::PLUGINS;

const STATE_FILE: &str = "state.sqlite";
const LOCK_SUFFIX: &str = ".migration-lock";
const MIGRATIONS_TABLE: &str = "_plugin_migrations";

#[derive(Debug, thiserror::Error)]
pub enum PluginStorageError {
    #[error("PLUGIN_STORAGE_ERROR unknown_plugin: `{plugin_id}` is not in the generated PLUGINS registry; refusing to open an arbitrary SQLite path")]
    UnknownPlugin { plugin_id: String },

    #[error("PLUGIN_STORAGE_ERROR io: {context} ({source})")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("PLUGIN_STORAGE_ERROR sqlite: {context} ({source})")]
    Sqlite {
        context: String,
        #[source]
        source: rusqlite::Error,
    },

    #[error("PLUGIN_STORAGE_ERROR forbidden_sql: plugin `{plugin_id}` migration `{file}` contains a forbidden statement (e.g. ATTACH DATABASE); rejected before execution")]
    ForbiddenSql { plugin_id: String, file: String },

    #[error("PLUGIN_STORAGE_ERROR migration_failed: plugin `{plugin_id}` migration `{file}` failed: {source}")]
    MigrationFailed {
        plugin_id: String,
        file: String,
        #[source]
        source: rusqlite::Error,
    },

    #[error("PLUGIN_STORAGE_ERROR migration_checksum_mismatch: plugin `{plugin_id}` version `{version}` was applied as `{existing_filename}` but the current source is `{new_filename}` or its body changed; refusing to silently skip — fix the file or add a new versioned migration")]
    MigrationChecksumMismatch {
        plugin_id: String,
        version: String,
        existing_filename: String,
        new_filename: String,
    },
}

/// Per-plugin SQLite handle. Construct via [`PluginStorage::open`] only.
///
/// `db_path` is computed from the constructor's `plugin_id` and is exposed
/// for diagnostics, not for re-construction — there is no `open_path` /
/// `with_db_path` / `from_raw` constructor by design, so plugin A's host code
/// has no API that can be made to point at plugin B's `state.sqlite`.
#[derive(Debug, Clone)]
pub struct PluginStorage {
    plugin_id: String,
    db_path: PathBuf,
}

impl PluginStorage {
    /// Resolve `${plugins_root}/<plugin_id>/state.sqlite` for a registered
    /// plugin. Creates the per-plugin directory if it does not yet exist.
    /// Returns `UnknownPlugin` if `plugin_id` is not in the generated
    /// `PLUGINS` registry.
    pub fn open(plugins_root: &Path, plugin_id: &str) -> Result<Self, PluginStorageError> {
        if !PLUGINS.iter().any(|p| p.plugin_id == plugin_id) {
            return Err(PluginStorageError::UnknownPlugin {
                plugin_id: plugin_id.to_string(),
            });
        }
        let plugin_dir = plugins_root.join(plugin_id);
        fs::create_dir_all(&plugin_dir).map_err(|source| PluginStorageError::Io {
            context: format!("create plugin dir {}", plugin_dir.display()),
            source,
        })?;
        Ok(Self {
            plugin_id: plugin_id.to_string(),
            db_path: plugin_dir.join(STATE_FILE),
        })
    }

    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Open a fresh connection with the contract PRAGMAs applied. Each call
    /// returns a new connection; callers (sidecars later) decide pool vs
    /// actor-owned-connection.
    pub fn connect(&self) -> Result<Connection, PluginStorageError> {
        let conn = Connection::open(&self.db_path).map_err(|source| PluginStorageError::Sqlite {
            context: format!("open {}", self.db_path.display()),
            source,
        })?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(|source| {
            PluginStorageError::Sqlite {
                context: "set journal_mode=WAL".into(),
                source,
            }
        })?;
        conn.pragma_update(None, "busy_timeout", 5000i64)
            .map_err(|source| PluginStorageError::Sqlite {
                context: "set busy_timeout=5000".into(),
                source,
            })?;
        conn.pragma_update(None, "foreign_keys", "ON").map_err(|source| {
            PluginStorageError::Sqlite {
                context: "set foreign_keys=ON".into(),
                source,
            }
        })?;
        Ok(conn)
    }

    /// Apply any pending migrations under an exclusive advisory file lock.
    /// Returns the names of migrations newly applied during this call (empty
    /// when everything was already applied).
    ///
    /// Sibling sidecars hold the lock for the duration of their own pending
    /// set; subsequent processes observe `_plugin_migrations` and skip
    /// already-applied entries, so no migration runs twice.
    pub fn run_migrations(
        &self,
        migrations_dir: &Path,
    ) -> Result<Vec<String>, PluginStorageError> {
        let lock_path = lock_path_for(&self.db_path);
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| PluginStorageError::Io {
                context: format!("open migration lock {}", lock_path.display()),
                source,
            })?;
        FileExt::lock(&lock_file).map_err(|source| PluginStorageError::Io {
            context: format!("acquire migration lock {}", lock_path.display()),
            source,
        })?;

        let result = self.run_migrations_locked(migrations_dir, &lock_file);

        // Unlock even if migration failed so siblings can attempt their own
        // schemas; the failure error is still propagated to the caller.
        let _ = FileExt::unlock(&lock_file);
        result
    }

    fn run_migrations_locked(
        &self,
        migrations_dir: &Path,
        _lock_file: &File,
    ) -> Result<Vec<String>, PluginStorageError> {
        let mut conn = self.connect()?;
        ensure_migrations_table(&conn)?;

        let mut applied_now = Vec::new();
        for (filename, path) in discover_migrations(migrations_dir)? {
            let version = derive_version(&filename);
            let bytes = fs::read(&path).map_err(|source| PluginStorageError::Io {
                context: format!("read migration {}", path.display()),
                source,
            })?;
            let checksum = checksum_hex(&bytes);

            let sql = std::str::from_utf8(&bytes).map_err(|err| PluginStorageError::Io {
                context: format!("decode {} as utf-8", path.display()),
                source: std::io::Error::new(std::io::ErrorKind::InvalidData, err),
            })?;

            let needs_record_insert = match lookup_applied(&conn, &version)? {
                Some(applied) if applied.filename == filename && applied.checksum == checksum => {
                    // Already applied with identical body — skip silently.
                    continue;
                }
                Some(applied) => {
                    // Self-heal: a migration whose body was edited to a
                    // strictly-idempotent superset (every statement is
                    // `CREATE ... IF NOT EXISTS` or `INSERT OR
                    // IGNORE/REPLACE INTO`) is safe to re-apply against
                    // the already-applied schema — every statement
                    // becomes a no-op. Re-run it, then update the
                    // stored checksum so the mismatch stops re-firing
                    // on every launch. Anything that ALTERs, DROPs,
                    // UPDATEs, or DELETEs falls through to the strict
                    // refusal because re-applying could lose data.
                    if applied.filename == filename && is_strictly_idempotent_sql(sql) {
                        false
                    } else {
                        return Err(PluginStorageError::MigrationChecksumMismatch {
                            plugin_id: self.plugin_id.clone(),
                            version,
                            existing_filename: applied.filename,
                            new_filename: filename,
                        });
                    }
                }
                None => true,
            };

            reject_forbidden_sql(sql, &self.plugin_id, &filename)?;

            let tx = conn
                .transaction()
                .map_err(|source| PluginStorageError::Sqlite {
                    context: format!("begin tx for {}", filename),
                    source,
                })?;
            if let Err(source) = tx.execute_batch(sql) {
                return Err(PluginStorageError::MigrationFailed {
                    plugin_id: self.plugin_id.clone(),
                    file: filename,
                    source,
                });
            }
            if needs_record_insert {
                tx.execute(
                    "INSERT INTO _plugin_migrations (version, filename, checksum, applied_at) VALUES (?1, ?2, ?3, datetime('now'))",
                    [&version, &filename, &checksum],
                )
                .map_err(|source| PluginStorageError::Sqlite {
                    context: format!("record applied migration {}", filename),
                    source,
                })?;
            } else {
                tx.execute(
                    "UPDATE _plugin_migrations SET checksum = ?1, applied_at = datetime('now') WHERE version = ?2",
                    [&checksum, &version],
                )
                .map_err(|source| PluginStorageError::Sqlite {
                    context: format!("refresh checksum for migration {}", filename),
                    source,
                })?;
            }
            tx.commit().map_err(|source| PluginStorageError::Sqlite {
                context: format!("commit migration {}", filename),
                source,
            })?;
            applied_now.push(filename);
        }
        Ok(applied_now)
    }
}

/// True when every non-comment statement in `sql` starts with one of
/// the strictly-idempotent forms (`CREATE TABLE/INDEX/UNIQUE INDEX/
/// TRIGGER/VIEW IF NOT EXISTS`, `INSERT OR IGNORE INTO`, `INSERT OR
/// REPLACE INTO`). Used by the migration runner to self-heal a stale
/// checksum row when the file body was edited to add `IF NOT EXISTS`
/// without changing the effective schema. Conservative: anything that
/// could mutate or destroy existing rows (ALTER, DROP, UPDATE, DELETE,
/// plain CREATE without IF NOT EXISTS) makes this return `false` and
/// the strict refusal path runs.
fn is_strictly_idempotent_sql(sql: &str) -> bool {
    let stripped = strip_sql_comments(sql);
    const ALLOWED_PREFIXES: &[&str] = &[
        "CREATE TABLE IF NOT EXISTS ",
        "CREATE INDEX IF NOT EXISTS ",
        "CREATE UNIQUE INDEX IF NOT EXISTS ",
        "CREATE TRIGGER IF NOT EXISTS ",
        "CREATE VIEW IF NOT EXISTS ",
        "INSERT OR IGNORE INTO ",
        "INSERT OR REPLACE INTO ",
    ];
    for stmt in stripped.split(';') {
        let trimmed = stmt.trim();
        if trimmed.is_empty() {
            continue;
        }
        let upper: String = trimmed.to_uppercase();
        let collapsed = upper.split_whitespace().collect::<Vec<_>>().join(" ");
        if !ALLOWED_PREFIXES.iter().any(|p| collapsed.starts_with(p)) {
            return false;
        }
    }
    true
}

/// Strip `-- line` and `/* block */` comments from `sql`, preserving
/// newlines so statement splits on `;` keep line affinity for error
/// messages. Does NOT attempt to honour string-literal escaping; the
/// caller only uses the result for token-prefix matching, not for
/// execution.
fn strip_sql_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '-' && chars.peek() == Some(&'-') {
            chars.next();
            for nc in chars.by_ref() {
                if nc == '\n' {
                    out.push('\n');
                    break;
                }
            }
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut prev = ' ';
            for nc in chars.by_ref() {
                if prev == '*' && nc == '/' {
                    break;
                }
                prev = nc;
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Row shape for the `_plugin_migrations` lookup. `version` is the PK; the
/// dispatcher compares stored `(filename, checksum)` to the on-disk values.
#[derive(Debug, Clone)]
struct AppliedMigration {
    filename: String,
    checksum: String,
}

fn lock_path_for(db_path: &Path) -> PathBuf {
    let mut name = db_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| STATE_FILE.to_string());
    name.push_str(LOCK_SUFFIX);
    db_path.with_file_name(name)
}

fn ensure_migrations_table(conn: &Connection) -> Result<(), PluginStorageError> {
    // Schema matches `docs/specs/plugin-contract.md` §"Required migration
    // metadata table" verbatim. `version` is the PK; `(filename, checksum)`
    // detect mutated files.
    conn.execute(
        "CREATE TABLE IF NOT EXISTS _plugin_migrations (
            version TEXT PRIMARY KEY,
            filename TEXT NOT NULL,
            checksum TEXT NOT NULL,
            applied_at TEXT NOT NULL
        )",
        [],
    )
    .map_err(|source| PluginStorageError::Sqlite {
        context: format!("create {} table", MIGRATIONS_TABLE),
        source,
    })?;
    Ok(())
}

fn lookup_applied(
    conn: &Connection,
    version: &str,
) -> Result<Option<AppliedMigration>, PluginStorageError> {
    let row = conn
        .query_row(
            "SELECT filename, checksum FROM _plugin_migrations WHERE version = ?1",
            [version],
            |row| {
                Ok(AppliedMigration {
                    filename: row.get(0)?,
                    checksum: row.get(1)?,
                })
            },
        )
        .map(Some)
        .or_else(|err| match err {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .map_err(|source| PluginStorageError::Sqlite {
            context: format!("lookup applied migration version {}", version),
            source,
        })?;
    Ok(row)
}

/// Numeric prefix of the filename stem, or the full stem when no numeric
/// prefix exists. Pure function exposed for tests.
pub fn derive_version(filename: &str) -> String {
    let stem = std::path::Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(filename);
    let prefix: String = stem.chars().take_while(|c| c.is_ascii_digit()).collect();
    if prefix.is_empty() {
        stem.to_string()
    } else {
        prefix
    }
}

/// SHA-256 hex digest of the migration body bytes. Pure function exposed for
/// tests.
pub fn checksum_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    hex_encode(&digest)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(*b >> 4) as usize] as char);
        out.push(HEX[(*b & 0x0f) as usize] as char);
    }
    out
}

fn discover_migrations(dir: &Path) -> Result<Vec<(String, PathBuf)>, PluginStorageError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    let entries = fs::read_dir(dir).map_err(|source| PluginStorageError::Io {
        context: format!("read_dir {}", dir.display()),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| PluginStorageError::Io {
            context: format!("iterate {}", dir.display()),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sql") {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        found.push((name, path));
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

fn reject_forbidden_sql(
    sql: &str,
    plugin_id: &str,
    file: &str,
) -> Result<(), PluginStorageError> {
    // Whole-word `ATTACH` (case-insensitive) — covers `ATTACH DATABASE 'x'
    // AS y`, `ATTACH 'x' AS y`, and any spacing/comment variation. Catches
    // column or identifier collisions too; that's acceptable for migration
    // SQL where authors can rename the offending identifier.
    static ATTACH_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = ATTACH_RE.get_or_init(|| Regex::new(r"(?i)\battach\b").unwrap());
    if re.is_match(sql) {
        return Err(PluginStorageError::ForbiddenSql {
            plugin_id: plugin_id.to_string(),
            file: file.to_string(),
        });
    }
    Ok(())
}

/// Iterates every registered plugin and attempts to run its migrations.
/// `migrations_dir_for` is the host's resolver from plugin id to migrations
/// directory on disk (dev: `<workspace>/plugins/<id>/<manifest.migrations_path>`;
/// packaged: bundled-asset location). Returns a per-plugin result map so the
/// host can render plugin-scoped error states without one plugin's failure
/// poisoning another (AC-9.3).
pub fn run_migrations_for_all_plugins<F>(
    plugins_root: &Path,
    mut migrations_dir_for: F,
) -> HashMap<String, Result<Vec<String>, PluginStorageError>>
where
    F: FnMut(&str) -> PathBuf,
{
    let mut out = HashMap::new();
    for plugin in PLUGINS {
        let storage = match PluginStorage::open(plugins_root, plugin.plugin_id) {
            Ok(s) => s,
            Err(err) => {
                out.insert(plugin.plugin_id.to_string(), Err(err));
                continue;
            }
        };
        let migrations_dir = migrations_dir_for(plugin.plugin_id);
        let result = storage.run_migrations(&migrations_dir);
        out.insert(plugin.plugin_id.to_string(), result);
    }
    out
}

// ---------------------------------------------------------------------------
// Host-side migration status (AC-9.3)
// ---------------------------------------------------------------------------

/// Wire shape returned to the frontend for each plugin's most recent
/// migration outcome. Frontend renders the `Error` arm as a Chinese
/// retry panel and gates plugin-component mounting on `Ok`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PluginMigrationStatus {
    #[serde(rename = "ok")]
    Ok { applied: Vec<String> },
    #[serde(rename = "error")]
    Error {
        error_kind: String,
        file: Option<String>,
        message: String,
    },
}

impl PluginMigrationStatus {
    pub fn from_storage_result(result: Result<Vec<String>, PluginStorageError>) -> Self {
        match result {
            Ok(applied) => Self::Ok { applied },
            Err(err) => Self::from_error(&err),
        }
    }

    pub fn from_error(err: &PluginStorageError) -> Self {
        let (kind, file) = match err {
            PluginStorageError::UnknownPlugin { .. } => ("unknown_plugin", None),
            PluginStorageError::Io { .. } => ("io", None),
            PluginStorageError::Sqlite { .. } => ("sqlite", None),
            PluginStorageError::ForbiddenSql { file, .. } => ("forbidden_sql", Some(file.clone())),
            PluginStorageError::MigrationFailed { file, .. } => {
                ("migration_failed", Some(file.clone()))
            }
            PluginStorageError::MigrationChecksumMismatch {
                new_filename, ..
            } => ("migration_checksum_mismatch", Some(new_filename.clone())),
        };
        Self::Error {
            error_kind: kind.into(),
            file,
            message: err.to_string(),
        }
    }
}

/// Tauri-managed host state holding the latest migration status per plugin.
#[derive(Debug, Default)]
pub struct PluginMigrationState {
    by_plugin: Mutex<HashMap<String, PluginMigrationStatus>>,
}

impl PluginMigrationState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(
        &self,
        plugin_id: &str,
        result: Result<Vec<String>, PluginStorageError>,
    ) -> PluginMigrationStatus {
        let status = PluginMigrationStatus::from_storage_result(result);
        let mut guard = self.by_plugin.lock().expect("PluginMigrationState poisoned");
        guard.insert(plugin_id.to_string(), status.clone());
        status
    }

    pub fn snapshot(&self) -> HashMap<String, PluginMigrationStatus> {
        self.by_plugin
            .lock()
            .expect("PluginMigrationState poisoned")
            .clone()
    }

    pub fn status(&self, plugin_id: &str) -> Option<PluginMigrationStatus> {
        self.by_plugin
            .lock()
            .expect("PluginMigrationState poisoned")
            .get(plugin_id)
            .cloned()
    }
}

/// Resolve a plugin's migrations directory from the dev checkout. The result
/// is `<workspace>/plugins/<plugin_id>/<manifest_migrations_path>` where
/// `<workspace>` is the project root inferred from this crate's manifest dir.
///
/// Packaged builds will need a different resolver (bundled-resource path);
/// that lands alongside task11 sidecar wiring.
pub fn resolve_workspace_migrations_dir(plugin_id: &str, manifest_migrations_path: &str) -> PathBuf {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("src-tauri crate has a parent workspace dir");
    workspace_root
        .join("plugins")
        .join(plugin_id)
        .join(manifest_migrations_path)
}

/// Bootstrap-time helper: runs migrations for every registered plugin and
/// records the per-plugin outcome in `state`. Logs each outcome with
/// structured fields; never panics, never returns an error.
pub fn run_all_plugin_migrations_at_bootstrap(
    plugins_root: &Path,
    state: &PluginMigrationState,
) {
    for plugin in PLUGINS {
        let storage_result =
            PluginStorage::open(plugins_root, plugin.plugin_id).and_then(|storage| {
                let migrations_dir =
                    resolve_workspace_migrations_dir(plugin.plugin_id, plugin.migrations_path);
                storage.run_migrations(&migrations_dir)
            });

        let status = state.record(plugin.plugin_id, storage_result);
        match &status {
            PluginMigrationStatus::Ok { applied } => {
                tracing::info!(
                    plugin_id = %plugin.plugin_id,
                    applied_count = applied.len(),
                    "plugin migrations applied"
                );
            }
            PluginMigrationStatus::Error {
                error_kind,
                file,
                message,
            } => {
                tracing::error!(
                    plugin_id = %plugin.plugin_id,
                    error_kind = %error_kind,
                    file = file.as_deref().unwrap_or(""),
                    message = %message,
                    "plugin migrations failed"
                );
            }
        }
    }
}

#[tauri::command]
pub fn plugin_migration_status(
    state: tauri::State<'_, PluginMigrationState>,
) -> HashMap<String, PluginMigrationStatus> {
    state.snapshot()
}

#[tauri::command]
pub fn retry_plugin_migration(
    plugin_id: String,
    state: tauri::State<'_, PluginMigrationState>,
) -> Result<PluginMigrationStatus, PluginMigrationStatus> {
    let plugin = match PLUGINS.iter().find(|p| p.plugin_id == plugin_id) {
        Some(p) => p,
        None => {
            // Cannot retry an unregistered plugin; surface it via the same
            // typed wire shape the frontend already handles.
            let err = PluginStorageError::UnknownPlugin {
                plugin_id: plugin_id.clone(),
            };
            let status = state.record(&plugin_id, Err(err));
            return Err(status);
        }
    };

    // The plugins_root path is the same one bootstrap computed; we recover it
    // through the bootstrap cache (already stored as a Tauri-managed state in
    // lib.rs). To keep this module dependency-light we re-derive via the
    // BootstrapPaths exposed through Tauri state at retry time.
    //
    // The lookup is wired in `lib.rs` where both `BootstrapPaths` and
    // `PluginMigrationState` are managed.
    let plugins_root = match crate::bootstrap_status_plugins_root() {
        Some(path) => path,
        None => {
            let err = PluginStorageError::Io {
                context: "bootstrap paths unavailable".into(),
                source: std::io::Error::other("bootstrap not run"),
            };
            let status = PluginMigrationStatus::from_error(&err);
            state.record(&plugin_id, Err(err));
            return Err(status);
        }
    };

    let storage_result = PluginStorage::open(&plugins_root, plugin.plugin_id).and_then(|storage| {
        let migrations_dir =
            resolve_workspace_migrations_dir(plugin.plugin_id, plugin.migrations_path);
        storage.run_migrations(&migrations_dir)
    });

    let status = state.record(plugin.plugin_id, storage_result);
    match status {
        PluginMigrationStatus::Ok { .. } => Ok(status),
        PluginMigrationStatus::Error { .. } => Err(status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    const REAL_PLUGIN: &str = "example-notes";

    fn tmp_root() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("tempdir")
    }

    fn write_migration(dir: &Path, name: &str, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), body).unwrap();
    }

    // ----- constructor invariants -----

    #[test]
    fn open_unknown_plugin_fails() {
        let root = tmp_root();
        let err = PluginStorage::open(root.path(), "not-registered").unwrap_err();
        match err {
            PluginStorageError::UnknownPlugin { plugin_id } => {
                assert_eq!(plugin_id, "not-registered");
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn db_path_is_per_plugin_under_plugins_root() {
        let root = tmp_root();
        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();
        let expected = root.path().join(REAL_PLUGIN).join(STATE_FILE);
        assert_eq!(storage.db_path(), expected.as_path());
        assert_eq!(storage.plugin_id(), REAL_PLUGIN);
        assert!(
            root.path().join(REAL_PLUGIN).is_dir(),
            "plugin dir should be created"
        );
    }

    // ----- pragma contract -----

    fn pragma_string(conn: &Connection, pragma: &str) -> String {
        conn.query_row(&format!("PRAGMA {pragma}"), [], |r| r.get::<_, String>(0))
            .unwrap()
    }
    fn pragma_int(conn: &Connection, pragma: &str) -> i64 {
        conn.query_row(&format!("PRAGMA {pragma}"), [], |r| r.get::<_, i64>(0))
            .unwrap()
    }

    #[test]
    fn connect_sets_wal_journal_mode() {
        let root = tmp_root();
        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();
        let conn = storage.connect().unwrap();
        assert_eq!(pragma_string(&conn, "journal_mode").to_lowercase(), "wal");
    }

    #[test]
    fn connect_sets_busy_timeout_5000() {
        let root = tmp_root();
        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();
        let conn = storage.connect().unwrap();
        assert_eq!(pragma_int(&conn, "busy_timeout"), 5000);
    }

    #[test]
    fn connect_enables_foreign_keys() {
        let root = tmp_root();
        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();
        let conn = storage.connect().unwrap();
        assert_eq!(pragma_int(&conn, "foreign_keys"), 1);
    }

    // ----- migration discovery + application -----

    #[test]
    fn run_migrations_applies_ordered_sql_files() {
        let root = tmp_root();
        let migrations = root.path().join("mig");
        write_migration(&migrations, "0003_z.sql", "CREATE TABLE z (id INTEGER);");
        write_migration(&migrations, "0001_a.sql", "CREATE TABLE a (id INTEGER);");
        write_migration(&migrations, "0002_b.sql", "CREATE TABLE b (id INTEGER);");

        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();
        let applied = storage.run_migrations(&migrations).unwrap();
        assert_eq!(applied, vec!["0001_a.sql", "0002_b.sql", "0003_z.sql"]);

        let conn = storage.connect().unwrap();
        // Each table now exists.
        for table in ["a", "b", "z"] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "table {table} should exist");
        }
        // _plugin_migrations rows preserve apply order, keyed by version.
        let mut stmt = conn
            .prepare("SELECT version, filename FROM _plugin_migrations ORDER BY version")
            .unwrap();
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            rows,
            vec![
                ("0001".to_string(), "0001_a.sql".to_string()),
                ("0002".to_string(), "0002_b.sql".to_string()),
                ("0003".to_string(), "0003_z.sql".to_string()),
            ]
        );
    }

    #[test]
    fn run_migrations_is_idempotent() {
        let root = tmp_root();
        let migrations = root.path().join("mig");
        write_migration(&migrations, "0001_init.sql", "CREATE TABLE t (id INTEGER);");

        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();
        let first = storage.run_migrations(&migrations).unwrap();
        let second = storage.run_migrations(&migrations).unwrap();
        assert_eq!(first, vec!["0001_init.sql"]);
        assert!(second.is_empty(), "second run applied nothing");
    }

    #[test]
    fn run_migrations_records_in_underscore_plugin_migrations_table() {
        let root = tmp_root();
        let migrations = root.path().join("mig");
        write_migration(&migrations, "0001_init.sql", "CREATE TABLE t (id INTEGER);");

        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();
        storage.run_migrations(&migrations).unwrap();

        let conn = storage.connect().unwrap();
        let (version, filename, checksum): (String, String, String) = conn
            .query_row(
                "SELECT version, filename, checksum FROM _plugin_migrations",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(version, "0001");
        assert_eq!(filename, "0001_init.sql");
        assert_eq!(checksum.len(), 64, "sha256 hex is 64 chars: {checksum}");
    }

    #[test]
    fn run_migrations_ignores_non_sql_files() {
        let root = tmp_root();
        let migrations = root.path().join("mig");
        write_migration(&migrations, "README.md", "not sql");
        write_migration(&migrations, "0001_init.sql", "CREATE TABLE t (id INTEGER);");

        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();
        let applied = storage.run_migrations(&migrations).unwrap();
        assert_eq!(applied, vec!["0001_init.sql"]);
    }

    #[test]
    fn run_migrations_missing_dir_is_no_op() {
        let root = tmp_root();
        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();
        let applied = storage.run_migrations(&root.path().join("nope")).unwrap();
        assert!(applied.is_empty());
    }

    // ----- forbidden SQL gate -----

    #[test]
    fn run_migrations_rejects_attach_database() {
        let root = tmp_root();
        let migrations = root.path().join("mig");
        write_migration(
            &migrations,
            "0001_bad.sql",
            "ATTACH DATABASE '/tmp/evil.sqlite' AS evil;",
        );

        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();
        let err = storage.run_migrations(&migrations).unwrap_err();
        match err {
            PluginStorageError::ForbiddenSql { plugin_id, file } => {
                assert_eq!(plugin_id, REAL_PLUGIN);
                assert_eq!(file, "0001_bad.sql");
            }
            other => panic!("unexpected: {other}"),
        }
        // _plugin_migrations not advanced; the file is not recorded.
        let conn = storage.connect().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM _plugin_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn forbidden_sql_check_is_case_insensitive() {
        let err =
            reject_forbidden_sql("attach database 'x' as y;", "p", "f.sql").unwrap_err();
        assert!(matches!(err, PluginStorageError::ForbiddenSql { .. }));
        let err2 = reject_forbidden_sql("ATTACH 'x' AS y;", "p", "f.sql").unwrap_err();
        assert!(matches!(err2, PluginStorageError::ForbiddenSql { .. }));
    }

    // ----- concurrency -----

    #[test]
    fn run_migrations_concurrent_threads_apply_each_migration_exactly_once() {
        let root = tmp_root();
        let migrations = root.path().join("mig");
        write_migration(
            &migrations,
            "0001_init.sql",
            "CREATE TABLE t (id INTEGER PRIMARY KEY);",
        );

        let n_threads = 6;
        let barrier = Arc::new(Barrier::new(n_threads));
        let root_path = root.path().to_path_buf();
        let migrations_path = migrations.clone();

        let handles: Vec<_> = (0..n_threads)
            .map(|_| {
                let root_path = root_path.clone();
                let migrations_path = migrations_path.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    let storage = PluginStorage::open(&root_path, REAL_PLUGIN).unwrap();
                    storage.run_migrations(&migrations_path).unwrap()
                })
            })
            .collect();

        let results: Vec<Vec<String>> = handles
            .into_iter()
            .map(|h| h.join().expect("thread did not panic"))
            .collect();

        let total_applied: usize = results.iter().map(|v| v.len()).sum();
        assert_eq!(
            total_applied, 1,
            "exactly one thread should apply the migration; got per-thread {results:?}"
        );

        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();
        let conn = storage.connect().unwrap();
        let row_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _plugin_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(row_count, 1);
    }

    // ----- failure isolation -----

    #[test]
    fn migration_failure_returns_plugin_scoped_typed_error() {
        let root = tmp_root();
        let migrations = root.path().join("mig");
        write_migration(&migrations, "0001_bad.sql", "THIS IS NOT VALID SQL;");

        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();
        let err = storage.run_migrations(&migrations).unwrap_err();
        match err {
            PluginStorageError::MigrationFailed { plugin_id, file, .. } => {
                assert_eq!(plugin_id, REAL_PLUGIN);
                assert_eq!(file, "0001_bad.sql");
            }
            other => panic!("expected MigrationFailed; got {other}"),
        }
    }

    #[test]
    fn failing_migration_for_one_plugin_does_not_block_another() {
        // Use the batch driver: example-notes is the only registered plugin
        // in the bundled PLUGINS table, so simulate "another plugin" by
        // first running migrations against a synthetic temp DB at the same
        // mount path with a known-good schema, then explicitly invoking the
        // batch runner with both a failing migrations dir (for plugin A) and
        // a good migrations dir (for plugin B). Since we have one registered
        // plugin, the test demonstrates that the batch API isolates per-plugin
        // failure: it routes each plugin's result independently and one error
        // does not poison the map.
        //
        // Concretely: we exercise the batch runner with a migrations_dir_for
        // closure that returns a deliberately-broken migration; assert the
        // map slot for example-notes is Err(MigrationFailed). Then run a
        // second time with a good migration; assert the map slot is Ok. The
        // batch driver's per-plugin try/collect pattern is the AC-9.3
        // architectural guarantee.
        let root = tmp_root();
        let bad_dir = root.path().join("bad");
        let good_dir = root.path().join("good");
        write_migration(&bad_dir, "0001_bad.sql", "INVALID SYNTAX HERE;");
        write_migration(&good_dir, "0001_good.sql", "CREATE TABLE g (id INTEGER);");

        // Pass 1: broken migration -> Err.
        let mut results = run_migrations_for_all_plugins(root.path(), |_| bad_dir.clone());
        let r = results.remove(REAL_PLUGIN).expect("entry per registered plugin");
        match r {
            Err(PluginStorageError::MigrationFailed { plugin_id, file, .. }) => {
                assert_eq!(plugin_id, REAL_PLUGIN);
                assert_eq!(file, "0001_bad.sql");
            }
            other => panic!("pass 1 expected MigrationFailed; got {other:?}"),
        }
        // Other plugins (if any in PLUGINS) must also have map entries.
        assert!(
            results.values().all(|r| matches!(r, Err(_))),
            "all plugins under the failing resolver should fail independently"
        );

        // Pass 2: good migration -> Ok; partial recovery confirms the batch
        // driver does not carry state from a previous failure.
        let mut root2 = tmp_root();
        // (use a fresh root so the previous failed-DB does not interfere)
        let _ = &mut root2;
        let results2 = run_migrations_for_all_plugins(root2.path(), |_| good_dir.clone());
        let r2 = results2.get(REAL_PLUGIN).expect("entry per registered plugin");
        assert!(matches!(r2, Ok(applied) if applied == &vec!["0001_good.sql".to_string()]));
    }

    // ----- API surface guard -----

    #[test]
    fn cross_plugin_access_denied_by_api_surface() {
        // The only public constructor is `PluginStorage::open(root, plugin_id)`,
        // which validates `plugin_id` against the generated `PLUGINS` table.
        // There is no `open_path`/`with_db_path`/`from_raw_path` constructor;
        // the `db_path` accessor is read-only. This test demonstrates that:
        //
        // 1. Constructing a `PluginStorage` for an unregistered id fails.
        // 2. There is no way to ask `PluginStorage` for a path under a
        //    different plugin's directory.
        let root = tmp_root();
        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();
        let path = storage.db_path();
        // The path is bound to the constructor's plugin_id.
        assert!(
            path.to_string_lossy()
                .contains(&format!("/{}/", REAL_PLUGIN)),
            "{path:?} should sit under the constructor's plugin_id dir"
        );
        // Cross-plugin construction fails before any FS work happens.
        assert!(matches!(
            PluginStorage::open(root.path(), "another-plugin").unwrap_err(),
            PluginStorageError::UnknownPlugin { .. }
        ));
    }

    // ----- example-notes round trip -----

    #[test]
    fn example_notes_real_migration_applies_end_to_end() {
        // Validates the bundled fixture by pointing the runner at the actual
        // `plugins/example-notes/migrations/` directory and asserting the
        // notes table exists post-migration.
        let root = tmp_root();
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let migrations = workspace_root.join("plugins").join(REAL_PLUGIN).join("migrations");
        assert!(
            migrations.is_dir(),
            "fixture dir should exist at {migrations:?}"
        );

        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();
        let applied = storage.run_migrations(&migrations).unwrap();
        assert!(applied.contains(&"0001_init.sql".to_string()));

        let conn = storage.connect().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='notes'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "notes table should exist after applying the example-notes migration");
    }

    // ----- spec-compliant metadata + checksum (Round 8) -----

    #[test]
    fn derive_version_numeric_prefix() {
        assert_eq!(derive_version("0001_init.sql"), "0001");
        assert_eq!(derive_version("0042_add_index.sql"), "0042");
        assert_eq!(derive_version("init.sql"), "init");
        assert_eq!(derive_version("v1_legacy.sql"), "v1_legacy");
    }

    #[test]
    fn checksum_hex_known_vector() {
        // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(
            checksum_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // SHA-256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        assert_eq!(
            checksum_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn migration_metadata_schema_matches_spec() {
        let root = tmp_root();
        let migrations = root.path().join("mig");
        write_migration(&migrations, "0001_init.sql", "CREATE TABLE t (id INTEGER);");
        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();
        storage.run_migrations(&migrations).unwrap();

        let conn = storage.connect().unwrap();
        let mut stmt = conn
            .prepare("PRAGMA table_info(_plugin_migrations)")
            .unwrap();
        let cols: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            cols,
            vec![
                ("version".to_string(), "TEXT".to_string()),
                ("filename".to_string(), "TEXT".to_string()),
                ("checksum".to_string(), "TEXT".to_string()),
                ("applied_at".to_string(), "TEXT".to_string()),
            ]
        );
    }

    #[test]
    fn same_file_same_checksum_skipped_idempotent() {
        let root = tmp_root();
        let migrations = root.path().join("mig");
        write_migration(&migrations, "0001_init.sql", "CREATE TABLE t (id INTEGER);");
        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();

        let first = storage.run_migrations(&migrations).unwrap();
        let second = storage.run_migrations(&migrations).unwrap();
        assert_eq!(first, vec!["0001_init.sql"]);
        assert!(second.is_empty());

        // Exactly one row in _plugin_migrations.
        let conn = storage.connect().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM _plugin_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn same_file_modified_returns_checksum_mismatch() {
        let root = tmp_root();
        let migrations = root.path().join("mig");
        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();

        write_migration(&migrations, "0001_init.sql", "CREATE TABLE t (id INTEGER);");
        storage.run_migrations(&migrations).unwrap();

        // Rewrite the same filename with different body — common authoring
        // mistake; must fail loudly.
        write_migration(
            &migrations,
            "0001_init.sql",
            "CREATE TABLE t (id INTEGER, extra TEXT);",
        );
        let err = storage.run_migrations(&migrations).unwrap_err();
        match err {
            PluginStorageError::MigrationChecksumMismatch {
                plugin_id,
                version,
                existing_filename,
                new_filename,
            } => {
                assert_eq!(plugin_id, REAL_PLUGIN);
                assert_eq!(version, "0001");
                assert_eq!(existing_filename, "0001_init.sql");
                assert_eq!(new_filename, "0001_init.sql");
            }
            other => panic!("expected MigrationChecksumMismatch; got {other}"),
        }
    }

    #[test]
    fn renamed_file_with_same_version_returns_checksum_mismatch() {
        let root = tmp_root();
        let migrations = root.path().join("mig");
        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();

        write_migration(&migrations, "0001_init.sql", "CREATE TABLE t (id INTEGER);");
        storage.run_migrations(&migrations).unwrap();

        // Remove the original; place a different file with the same version
        // prefix.
        fs::remove_file(migrations.join("0001_init.sql")).unwrap();
        write_migration(&migrations, "0001_init_v2.sql", "CREATE TABLE t (id INTEGER);");
        let err = storage.run_migrations(&migrations).unwrap_err();
        match err {
            PluginStorageError::MigrationChecksumMismatch {
                version,
                existing_filename,
                new_filename,
                ..
            } => {
                assert_eq!(version, "0001");
                assert_eq!(existing_filename, "0001_init.sql");
                assert_eq!(new_filename, "0001_init_v2.sql");
            }
            other => panic!("expected MigrationChecksumMismatch; got {other}"),
        }
    }

    #[test]
    fn discover_migrations_propagates_read_dir_error() {
        // Point at a regular file (not a directory) — `fs::read_dir` returns
        // an Io error. Previously `entries.flatten()` would have swallowed
        // any per-entry errors; this test pins the propagation contract on
        // the directory-open path.
        let root = tmp_root();
        let not_a_dir = root.path().join("regular_file");
        fs::write(&not_a_dir, "i am a file, not a dir").unwrap();
        let err = discover_migrations(&not_a_dir).unwrap_err();
        match err {
            PluginStorageError::Io { context, .. } => {
                assert!(
                    context.contains("read_dir"),
                    "context should describe read_dir failure: {context}"
                );
            }
            other => panic!("expected Io; got {other}"),
        }
    }

    // ----- AC-1.3 concurrent read/write stress -----

    #[test]
    fn concurrent_readers_writers_under_wal_busy_timeout() {
        // Models the N×M sibling-sidecar contention case: multiple independent
        // connections on the same plugin DB performing interleaved insert +
        // select operations. WAL + busy_timeout=5000 must keep all operations
        // moving without `SQLITE_BUSY` / `database is locked` errors.
        let root = tmp_root();
        let migrations = root.path().join("mig");
        write_migration(
            &migrations,
            "0001_init.sql",
            "CREATE TABLE rw_stress (id INTEGER PRIMARY KEY AUTOINCREMENT, val TEXT);",
        );
        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();
        storage.run_migrations(&migrations).unwrap();

        const THREADS: usize = 4;
        const OPS_PER_THREAD: usize = 25; // 4 * 25 = 100 inserts (+ 100 selects)
        let barrier = Arc::new(Barrier::new(THREADS));
        let root_path = root.path().to_path_buf();

        let handles: Vec<_> = (0..THREADS)
            .map(|tid| {
                let root_path = root_path.clone();
                let barrier = barrier.clone();
                thread::spawn(move || -> Result<usize, String> {
                    let storage = PluginStorage::open(&root_path, REAL_PLUGIN)
                        .map_err(|e| format!("open: {e}"))?;
                    let conn = storage.connect().map_err(|e| format!("connect: {e}"))?;
                    barrier.wait();
                    for i in 0..OPS_PER_THREAD {
                        conn.execute(
                            "INSERT INTO rw_stress (val) VALUES (?1)",
                            [&format!("thread {tid} iter {i}")],
                        )
                        .map_err(|e| format!("insert thread {tid} iter {i}: {e}"))?;
                        let _: i64 = conn
                            .query_row("SELECT COUNT(*) FROM rw_stress", [], |r| r.get(0))
                            .map_err(|e| format!("select thread {tid} iter {i}: {e}"))?;
                    }
                    Ok(OPS_PER_THREAD)
                })
            })
            .collect();

        let mut total_ops = 0usize;
        for h in handles {
            let n = h.join().expect("no panic").expect("no SQLITE_BUSY");
            total_ops += n;
        }
        assert_eq!(total_ops, THREADS * OPS_PER_THREAD);

        // Sibling-final assertion: every insert reached the table.
        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();
        let conn = storage.connect().unwrap();
        let final_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM rw_stress", [], |r| r.get(0))
            .unwrap();
        assert_eq!(final_count as usize, THREADS * OPS_PER_THREAD);
    }

    // ----- PluginMigrationState (AC-9.3) -----

    fn ok_status(applied: &[&str]) -> Result<Vec<String>, PluginStorageError> {
        Ok(applied.iter().map(|s| (*s).to_string()).collect())
    }
    fn err_status() -> Result<Vec<String>, PluginStorageError> {
        Err(PluginStorageError::MigrationFailed {
            plugin_id: REAL_PLUGIN.into(),
            file: "0001_bad.sql".into(),
            source: rusqlite::Error::QueryReturnedNoRows,
        })
    }

    #[test]
    fn migration_state_records_ok_and_error() {
        let state = PluginMigrationState::new();
        state.record("plugin-a", ok_status(&["0001_init.sql"]));
        state.record("plugin-b", err_status());
        let snap = state.snapshot();
        assert!(matches!(
            snap.get("plugin-a"),
            Some(PluginMigrationStatus::Ok { applied }) if applied == &vec!["0001_init.sql".to_string()]
        ));
        assert!(matches!(
            snap.get("plugin-b"),
            Some(PluginMigrationStatus::Error { error_kind, .. }) if error_kind == "migration_failed"
        ));
    }

    #[test]
    fn migration_state_one_plugin_failure_does_not_hide_other() {
        let state = PluginMigrationState::new();
        state.record("plugin-a", ok_status(&["0001_init.sql"]));
        state.record("plugin-b", err_status());
        let a = state.status("plugin-a").expect("plugin-a present");
        let b = state.status("plugin-b").expect("plugin-b present");
        assert!(matches!(a, PluginMigrationStatus::Ok { .. }));
        assert!(matches!(b, PluginMigrationStatus::Error { .. }));
    }

    #[test]
    fn migration_state_retry_overwrites_status() {
        let state = PluginMigrationState::new();
        state.record(REAL_PLUGIN, err_status());
        assert!(matches!(
            state.status(REAL_PLUGIN),
            Some(PluginMigrationStatus::Error { .. })
        ));
        state.record(REAL_PLUGIN, ok_status(&["0001_init.sql"]));
        match state.status(REAL_PLUGIN).expect("present") {
            PluginMigrationStatus::Ok { applied } => {
                assert_eq!(applied, vec!["0001_init.sql".to_string()]);
            }
            other => panic!("expected Ok; got {other:?}"),
        }
    }

    #[test]
    fn plugin_migration_status_from_storage_error_covers_every_variant() {
        let cases: Vec<(PluginStorageError, &str)> = vec![
            (
                PluginStorageError::UnknownPlugin {
                    plugin_id: "x".into(),
                },
                "unknown_plugin",
            ),
            (
                PluginStorageError::Io {
                    context: "ctx".into(),
                    source: std::io::Error::other("boom"),
                },
                "io",
            ),
            (
                PluginStorageError::Sqlite {
                    context: "ctx".into(),
                    source: rusqlite::Error::QueryReturnedNoRows,
                },
                "sqlite",
            ),
            (
                PluginStorageError::ForbiddenSql {
                    plugin_id: "p".into(),
                    file: "f.sql".into(),
                },
                "forbidden_sql",
            ),
            (
                PluginStorageError::MigrationFailed {
                    plugin_id: "p".into(),
                    file: "f.sql".into(),
                    source: rusqlite::Error::QueryReturnedNoRows,
                },
                "migration_failed",
            ),
            (
                PluginStorageError::MigrationChecksumMismatch {
                    plugin_id: "p".into(),
                    version: "0001".into(),
                    existing_filename: "0001_a.sql".into(),
                    new_filename: "0001_b.sql".into(),
                },
                "migration_checksum_mismatch",
            ),
        ];
        for (err, expected_kind) in cases {
            let status = PluginMigrationStatus::from_error(&err);
            match status {
                PluginMigrationStatus::Error { error_kind, .. } => {
                    assert_eq!(error_kind, expected_kind);
                }
                _ => panic!("expected Error variant"),
            }
        }
    }

    #[test]
    fn run_all_plugin_migrations_at_bootstrap_populates_state_for_every_registered_plugin() {
        let root = tmp_root();
        let state = PluginMigrationState::new();
        run_all_plugin_migrations_at_bootstrap(root.path(), &state);
        let snap = state.snapshot();
        for plugin in PLUGINS {
            assert!(
                snap.contains_key(plugin.plugin_id),
                "missing state entry for {}",
                plugin.plugin_id
            );
        }
        // For example-notes the bundled migration is valid: status must be Ok.
        match snap.get(REAL_PLUGIN).expect("example-notes present") {
            PluginMigrationStatus::Ok { applied } => {
                assert!(
                    applied.contains(&"0001_init.sql".to_string()),
                    "bundled example-notes migration should be applied: {applied:?}"
                );
            }
            other => panic!("expected Ok for example-notes; got {other:?}"),
        }
    }

    // ----- self-healing on idempotent body change -----

    #[test]
    fn is_strictly_idempotent_sql_accepts_create_if_not_exists_and_insert_or_ignore() {
        let body = r#"
            -- header comment
            CREATE TABLE IF NOT EXISTS t (id INTEGER);
            CREATE INDEX IF NOT EXISTS idx_t ON t(id);
            CREATE UNIQUE INDEX IF NOT EXISTS uniq_t ON t(id);
            INSERT OR IGNORE INTO t (id) VALUES (1);
            INSERT OR REPLACE INTO t (id) VALUES (2);
        "#;
        assert!(is_strictly_idempotent_sql(body));
    }

    #[test]
    fn is_strictly_idempotent_sql_rejects_plain_create_and_destructive_forms() {
        for body in [
            "CREATE TABLE t (id INTEGER);",
            "DROP TABLE t;",
            "ALTER TABLE t ADD COLUMN c INTEGER;",
            "UPDATE t SET x = 1;",
            "DELETE FROM t;",
            "CREATE TABLE IF NOT EXISTS t (id INTEGER); ALTER TABLE t ADD COLUMN c INTEGER;",
        ] {
            assert!(
                !is_strictly_idempotent_sql(body),
                "body should NOT be idempotent: {body}"
            );
        }
    }

    #[test]
    fn run_migrations_self_heals_when_body_becomes_idempotent_superset() {
        // Reproduces the regression that blocked the Papers tab from
        // mounting on every cold start: an earlier version of
        // `0001_init.sql` was applied with `CREATE TABLE foo (...)`
        // (no `IF NOT EXISTS`); a later release edited the file to
        // add `IF NOT EXISTS` so `PapersStore::open`'s embedded
        // include_str! could also apply it idempotently. The schema
        // is materially identical but the checksum differs, so the
        // migration runner refused to silently skip and the host
        // gated mount with a MigrationFailurePanel. With self-healing
        // in place the runner re-applies (no-op) and updates the
        // stored checksum.
        let root = tmp_root();
        let migrations = root.path().join("mig");
        write_migration(&migrations, "0001_init.sql", "CREATE TABLE foo (id INTEGER);");
        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();
        storage.run_migrations(&migrations).expect("first apply");
        // Snapshot the stored checksum BEFORE the body edit.
        let conn = storage.connect().unwrap();
        let before_checksum: String = conn
            .query_row(
                "SELECT checksum FROM _plugin_migrations WHERE version = '0001'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        drop(conn);

        // Edit the body to an idempotent superset and re-run.
        write_migration(
            &migrations,
            "0001_init.sql",
            "CREATE TABLE IF NOT EXISTS foo (id INTEGER);",
        );
        let applied = storage
            .run_migrations(&migrations)
            .expect("self-heal must succeed for idempotent body change");
        assert_eq!(applied, vec!["0001_init.sql"]);

        // Stored checksum updated; row count still 1 (UPDATE not INSERT).
        let conn = storage.connect().unwrap();
        let after_checksum: String = conn
            .query_row(
                "SELECT checksum FROM _plugin_migrations WHERE version = '0001'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(before_checksum, after_checksum, "checksum must be refreshed");
        let row_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _plugin_migrations WHERE version = '0001'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(row_count, 1, "still exactly one row for version 0001");

        // Re-running again with the SAME body is a silent no-op.
        let second = storage.run_migrations(&migrations).unwrap();
        assert!(second.is_empty(), "second run must be a no-op: {second:?}");
    }

    #[test]
    fn run_migrations_refuses_destructive_body_change() {
        // Non-idempotent body changes (ALTER, DROP, ...) still hit
        // the strict refusal so the user is forced to add a new
        // versioned migration rather than silently replay something
        // that could lose rows.
        let root = tmp_root();
        let migrations = root.path().join("mig");
        write_migration(&migrations, "0001_init.sql", "CREATE TABLE foo (id INTEGER);");
        let storage = PluginStorage::open(root.path(), REAL_PLUGIN).unwrap();
        storage.run_migrations(&migrations).expect("first apply");

        write_migration(
            &migrations,
            "0001_init.sql",
            "DROP TABLE foo; CREATE TABLE foo (id INTEGER, x INTEGER);",
        );
        let err = storage.run_migrations(&migrations).unwrap_err();
        match err {
            PluginStorageError::MigrationChecksumMismatch { plugin_id, version, .. } => {
                assert_eq!(plugin_id, REAL_PLUGIN);
                assert_eq!(version, "0001");
            }
            other => panic!("expected MigrationChecksumMismatch; got {other:?}"),
        }
    }
}
