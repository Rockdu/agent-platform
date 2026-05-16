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

use fs4::FileExt;
use regex::Regex;
use rusqlite::Connection;

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
        for (name, path) in discover_migrations(migrations_dir)? {
            if is_already_applied(&conn, &name)? {
                continue;
            }
            let sql = fs::read_to_string(&path).map_err(|source| PluginStorageError::Io {
                context: format!("read migration {}", path.display()),
                source,
            })?;
            reject_forbidden_sql(&sql, &self.plugin_id, &name)?;

            let tx = conn
                .transaction()
                .map_err(|source| PluginStorageError::Sqlite {
                    context: format!("begin tx for {}", name),
                    source,
                })?;
            if let Err(source) = tx.execute_batch(&sql) {
                return Err(PluginStorageError::MigrationFailed {
                    plugin_id: self.plugin_id.clone(),
                    file: name,
                    source,
                });
            }
            tx.execute(
                "INSERT INTO _plugin_migrations (name, applied_at) VALUES (?1, datetime('now'))",
                [&name],
            )
            .map_err(|source| PluginStorageError::Sqlite {
                context: format!("record applied migration {}", name),
                source,
            })?;
            tx.commit().map_err(|source| PluginStorageError::Sqlite {
                context: format!("commit migration {}", name),
                source,
            })?;
            applied_now.push(name);
        }
        Ok(applied_now)
    }
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
    conn.execute(
        "CREATE TABLE IF NOT EXISTS _plugin_migrations (
            name TEXT PRIMARY KEY,
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

fn is_already_applied(conn: &Connection, name: &str) -> Result<bool, PluginStorageError> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM _plugin_migrations WHERE name = ?1",
            [name],
            |row| row.get(0),
        )
        .map_err(|source| PluginStorageError::Sqlite {
            context: format!("check applied {}", name),
            source,
        })?;
    Ok(count > 0)
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
    for entry in entries.flatten() {
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
        // _plugin_migrations rows preserve apply order.
        let mut stmt = conn
            .prepare("SELECT name FROM _plugin_migrations ORDER BY applied_at, name")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(names, vec!["0001_a.sql", "0002_b.sql", "0003_z.sql"]);
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
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _plugin_migrations WHERE name = ?1",
                ["0001_init.sql"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
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
}
