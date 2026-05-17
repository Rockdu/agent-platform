//! Per-Terminal-Mesh-tab workspace storage (AC-4.3 + AC-4.4 + AC-9.1
//! deepened). Persistent registry of user-visible workspace dirs
//! under `~/AgentPlatform/workspaces/<name>/` plus user-registered
//! existing directories. Canonical-path uniqueness enforced.
//! Atomic write to `${APP_DATA}/workspaces.json` (temp + rename, 0600).
//!
//! Locked-decision invariants:
//! - One workspace ↔ one open tab (the registry stores `open_tab_id`).
//! - Never auto `git init`.
//! - Workspaces survive restart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::State;
use uuid::Uuid;

const WORKSPACES_FILENAME: &str = "workspaces.json";
const NAME_MAX_CHARS: usize = 64;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRecord {
    pub workspace_id: Uuid,
    pub name: String,
    pub path: PathBuf,
    pub created_at: String,
    pub last_used_at: String,
    pub open_tab_id: Option<String>,
    /// Best-effort claude conversation rounds count derived from
    /// scanning `<path>/.claude/` for `*.jsonl` files at command
    /// return time. Computed, not persisted — `StoredWorkspaceRecord`
    /// excludes this field deliberately so the registry never
    /// becomes an authoritative cache. Defaults to 0 when `.claude/`
    /// is missing or unreadable.
    #[serde(default)]
    pub conversation_rounds_count: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("invalid workspace name: {reason}")]
    InvalidName { reason: String },

    #[error("workspace directory `{path}` already exists")]
    WorkspaceAlreadyExists { path: PathBuf },

    #[error(
        "canonical path duplicates existing workspace `{existing_name}` (id={existing_workspace_id})"
    )]
    CanonicalDuplicate {
        existing_workspace_id: Uuid,
        existing_name: String,
    },

    #[error("path `{path}` is not a directory or does not exist")]
    NotADirectory { path: PathBuf },

    #[error("workspace `{workspace_id}` not found")]
    NotFound { workspace_id: String },

    #[error("workspace already open in tab `{existing_tab_id}`")]
    AlreadyOpen { existing_tab_id: String },

    #[error("io error in `{context}`: {message}")]
    Io { context: String, message: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum WorkspaceErrorDto {
    InvalidName { reason: String },
    WorkspaceAlreadyExists { path: String },
    CanonicalDuplicate { existing_workspace_id: String, existing_name: String },
    NotADirectory { path: String },
    NotFound { workspace_id: String },
    AlreadyOpen { existing_tab_id: String },
    Io { context: String, message: String },
}

impl From<&WorkspaceError> for WorkspaceErrorDto {
    fn from(err: &WorkspaceError) -> Self {
        match err {
            WorkspaceError::InvalidName { reason } => Self::InvalidName {
                reason: reason.clone(),
            },
            WorkspaceError::WorkspaceAlreadyExists { path } => Self::WorkspaceAlreadyExists {
                path: path.display().to_string(),
            },
            WorkspaceError::CanonicalDuplicate {
                existing_workspace_id,
                existing_name,
            } => Self::CanonicalDuplicate {
                existing_workspace_id: existing_workspace_id.to_string(),
                existing_name: existing_name.clone(),
            },
            WorkspaceError::NotADirectory { path } => Self::NotADirectory {
                path: path.display().to_string(),
            },
            WorkspaceError::NotFound { workspace_id } => Self::NotFound {
                workspace_id: workspace_id.clone(),
            },
            WorkspaceError::AlreadyOpen { existing_tab_id } => Self::AlreadyOpen {
                existing_tab_id: existing_tab_id.clone(),
            },
            WorkspaceError::Io { context, message } => Self::Io {
                context: context.clone(),
                message: message.clone(),
            },
        }
    }
}

/// Disk-side mirror of `WorkspaceRecord`. Snake_case keys (no
/// `rename_all`) so `workspaces.json` reads like a normal shell-tools
/// JSON file. Separated from the wire `WorkspaceRecord` so the
/// Tauri-IPC camelCase convention and the on-disk snake_case
/// convention can evolve independently.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredWorkspaceRecord {
    workspace_id: Uuid,
    name: String,
    path: PathBuf,
    created_at: String,
    last_used_at: String,
    open_tab_id: Option<String>,
}

impl From<&WorkspaceRecord> for StoredWorkspaceRecord {
    fn from(r: &WorkspaceRecord) -> Self {
        // `conversation_rounds_count` is computed at command return
        // time and intentionally dropped here — the disk shape is
        // not an authoritative cache.
        Self {
            workspace_id: r.workspace_id,
            name: r.name.clone(),
            path: r.path.clone(),
            created_at: r.created_at.clone(),
            last_used_at: r.last_used_at.clone(),
            open_tab_id: r.open_tab_id.clone(),
        }
    }
}

impl From<StoredWorkspaceRecord> for WorkspaceRecord {
    fn from(s: StoredWorkspaceRecord) -> Self {
        Self {
            workspace_id: s.workspace_id,
            name: s.name,
            path: s.path,
            created_at: s.created_at,
            last_used_at: s.last_used_at,
            open_tab_id: s.open_tab_id,
            // Caller is expected to populate via
            // `count_claude_conversation_rounds(&record.path)`
            // before returning to the frontend; default 0 here so
            // an unpopulated record is still serializable.
            conversation_rounds_count: 0,
        }
    }
}

/// On-disk shape persisted under `${APP_DATA}/workspaces.json`.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct StoredRegistry {
    #[serde(default)]
    workspaces: Vec<StoredWorkspaceRecord>,
}

/// Filesystem-level directory-identity helper. Two paths refer to
/// the same workspace iff they're byte-equal OR (on Unix) they live
/// on the same device and inode. The dev/ino fallback catches macOS
/// APFS / NTFS / HFS+ case-insensitive aliases that `canonicalize`
/// leaves byte-distinct, plus any residual symlink edge cases the
/// canonical resolver missed. Non-Unix targets keep
/// canonical-equality-only behavior.
fn paths_refer_to_same_workspace(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    same_directory_identity(a, b)
}

#[cfg(unix)]
fn same_directory_identity(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(ma) = std::fs::metadata(a) else { return false };
    let Ok(mb) = std::fs::metadata(b) else { return false };
    ma.dev() == mb.dev() && ma.ino() == mb.ino()
}

#[cfg(not(unix))]
fn same_directory_identity(_a: &Path, _b: &Path) -> bool {
    false
}

/// Tauri-managed registry. Backed by `${storage_root}/workspaces.json`.
/// Internally `Arc<Mutex<...>>`-shared so callers like the host RPC
/// bridge can hold a clone alongside Tauri's `app.manage` handle and
/// observe the same state.
#[derive(Clone)]
pub struct WorkspaceRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}

struct RegistryInner {
    /// Map workspace_id -> record for O(1) lookup.
    records: HashMap<Uuid, WorkspaceRecord>,
    /// Storage root (`${APP_DATA}`); the JSON sits at
    /// `storage_root.join(WORKSPACES_FILENAME)`.
    storage_root: PathBuf,
    /// Optional: workspace root for auto-created workspaces under
    /// `~/AgentPlatform/workspaces/`. None during early-bootstrap tests.
    workspaces_root: Option<PathBuf>,
}

impl WorkspaceRegistry {
    /// Construct an empty registry pointed at `storage_root` (typically
    /// `${APP_DATA}`) without touching disk. Useful for tests.
    #[allow(dead_code)]
    pub fn empty(storage_root: PathBuf, workspaces_root: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RegistryInner {
                records: HashMap::new(),
                storage_root,
                workspaces_root,
            })),
        }
    }

    /// Bootstrap-time loader. Reads `${storage_root}/workspaces.json` if
    /// present. Malformed JSON is logged + tolerated (registry starts
    /// empty).
    pub fn load(storage_root: PathBuf, workspaces_root: Option<PathBuf>) -> Self {
        let path = storage_root.join(WORKSPACES_FILENAME);
        let records: HashMap<Uuid, WorkspaceRecord> = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<StoredRegistry>(&bytes) {
                Ok(stored) => stored
                    .workspaces
                    .into_iter()
                    .map(|s| {
                        let mut r: WorkspaceRecord = s.into();
                        // `open_tab_id` is RUNTIME state: a tab id
                        // only has meaning within the lifetime of a
                        // single frontend session. The disk slot is
                        // preserved purely for diagnostics (what was
                        // open at last shutdown); on load we clear
                        // it so a fresh session starts with zero
                        // workspace locks. This unblocks the
                        // open-after-restart path that would
                        // otherwise return `AlreadyOpen` for a stale
                        // tab id from the previous launch.
                        r.open_tab_id = None;
                        (r.workspace_id, r)
                    })
                    .collect(),
                Err(e) => {
                    tracing::warn!(path = %path.display(), %e, "workspaces.json malformed; starting empty");
                    HashMap::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                tracing::warn!(path = %path.display(), %e, "workspaces.json read failed; starting empty");
                HashMap::new()
            }
        };
        Self {
            inner: Arc::new(Mutex::new(RegistryInner {
                records,
                storage_root,
                workspaces_root,
            })),
        }
    }

    fn persist(inner: &RegistryInner) -> Result<(), WorkspaceError> {
        std::fs::create_dir_all(&inner.storage_root).map_err(|e| WorkspaceError::Io {
            context: format!("create_dir_all {}", inner.storage_root.display()),
            message: e.to_string(),
        })?;
        let path = inner.storage_root.join(WORKSPACES_FILENAME);
        let tmp = path.with_extension("json.tmp");
        let stored = StoredRegistry {
            workspaces: {
                let mut v: Vec<StoredWorkspaceRecord> = inner
                    .records
                    .values()
                    .map(StoredWorkspaceRecord::from)
                    .collect();
                v.sort_by(|a, b| a.created_at.cmp(&b.created_at));
                v
            },
        };
        let body = serde_json::to_vec_pretty(&stored).map_err(|e| WorkspaceError::Io {
            context: "serialize workspaces.json".into(),
            message: e.to_string(),
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
                .map_err(|e| WorkspaceError::Io {
                    context: format!("open {}", tmp.display()),
                    message: e.to_string(),
                })?;
            #[cfg(not(unix))]
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)
                .map_err(|e| WorkspaceError::Io {
                    context: format!("open {}", tmp.display()),
                    message: e.to_string(),
                })?;
            f.write_all(&body).map_err(|e| WorkspaceError::Io {
                context: format!("write {}", tmp.display()),
                message: e.to_string(),
            })?;
            let _ = f.sync_all();
        }
        std::fs::rename(&tmp, &path).map_err(|e| WorkspaceError::Io {
            context: format!("rename {} -> {}", tmp.display(), path.display()),
            message: e.to_string(),
        })
    }

    pub fn list(&self) -> Vec<WorkspaceRecord> {
        let guard = self.inner.lock().expect("WorkspaceRegistry poisoned");
        let mut out: Vec<WorkspaceRecord> = guard.records.values().cloned().collect();
        drop(guard);
        // Sort descending by `last_used_at` (string-comparable RFC3339).
        out.sort_by(|a, b| b.last_used_at.cmp(&a.last_used_at));
        for r in out.iter_mut() {
            r.conversation_rounds_count = count_claude_conversation_rounds(&r.path);
        }
        out
    }

    /// Friendly-name lookup for the host RPC `terminal_mesh.list_tabs`
    /// projection enrichment. Returns `None` for malformed UUIDs or
    /// missing records so the bridge can safely call it on every
    /// snapshot entry without per-call validation.
    pub fn lookup_name(&self, workspace_id_str: &str) -> Option<String> {
        let uuid = Uuid::parse_str(workspace_id_str).ok()?;
        let guard = self.inner.lock().expect("WorkspaceRegistry poisoned");
        guard.records.get(&uuid).map(|r| r.name.clone())
    }

    /// Test-only helper used by sibling-crate tests (e.g. the
    /// host_rpc bridge tests) to seed a workspace record without
    /// touching disk or going through the validation-heavy create
    /// path.
    #[cfg(test)]
    pub(crate) fn insert_record_for_tests(&self, record: WorkspaceRecord) {
        let mut guard = self.inner.lock().expect("WorkspaceRegistry poisoned");
        guard.records.insert(record.workspace_id, record);
    }

    /// Populate a returned `WorkspaceRecord` with a fresh
    /// `conversation_rounds_count` derived from its path. Used by
    /// every Tauri-command return path to keep the wire shape
    /// up-to-date without persisting the count.
    fn with_conversation_count(&self, mut record: WorkspaceRecord) -> WorkspaceRecord {
        record.conversation_rounds_count = count_claude_conversation_rounds(&record.path);
        record
    }

    /// Inject the workspaces-root override at runtime so test setups
    /// can point auto-created workspaces at a tempdir HOME.
    #[allow(dead_code)]
    pub fn override_workspaces_root(&self, root: PathBuf) {
        let mut guard = self.inner.lock().expect("WorkspaceRegistry poisoned");
        guard.workspaces_root = Some(root);
    }

    pub fn create_workspace(&self, name: &str) -> Result<WorkspaceRecord, WorkspaceError> {
        validate_workspace_name(name)?;
        let mut guard = self.inner.lock().expect("WorkspaceRegistry poisoned");
        let workspaces_root = guard
            .workspaces_root
            .clone()
            .or_else(default_workspaces_root)
            .ok_or_else(|| WorkspaceError::Io {
                context: "resolve workspaces_root".into(),
                message: "no HOME directory available".into(),
            })?;
        let target = workspaces_root.join(name);
        if target.exists() {
            return Err(WorkspaceError::WorkspaceAlreadyExists { path: target });
        }
        std::fs::create_dir_all(&target).map_err(|e| WorkspaceError::Io {
            context: format!("create_dir_all {}", target.display()),
            message: e.to_string(),
        })?;
        let canonical = std::fs::canonicalize(&target).map_err(|e| WorkspaceError::Io {
            context: format!("canonicalize {}", target.display()),
            message: e.to_string(),
        })?;
        if let Some(dup) = guard.find_duplicate(&canonical) {
            return Err(WorkspaceError::CanonicalDuplicate {
                existing_workspace_id: dup.workspace_id,
                existing_name: dup.name.clone(),
            });
        }
        let now = now_rfc3339();
        let record = WorkspaceRecord {
            workspace_id: Uuid::new_v4(),
            name: name.to_string(),
            path: canonical,
            created_at: now.clone(),
            last_used_at: now,
            open_tab_id: None,
            conversation_rounds_count: 0,
        };
        guard.records.insert(record.workspace_id, record.clone());
        Self::persist(&guard)?;
        Ok(self.with_conversation_count(record))
    }

    pub fn register_workspace(&self, path: &Path) -> Result<WorkspaceRecord, WorkspaceError> {
        let meta = std::fs::metadata(path).map_err(|_| WorkspaceError::NotADirectory {
            path: path.to_path_buf(),
        })?;
        if !meta.is_dir() {
            return Err(WorkspaceError::NotADirectory {
                path: path.to_path_buf(),
            });
        }
        let canonical = std::fs::canonicalize(path).map_err(|e| WorkspaceError::Io {
            context: format!("canonicalize {}", path.display()),
            message: e.to_string(),
        })?;
        let mut guard = self.inner.lock().expect("WorkspaceRegistry poisoned");
        if let Some(dup) = guard.find_duplicate(&canonical) {
            return Err(WorkspaceError::CanonicalDuplicate {
                existing_workspace_id: dup.workspace_id,
                existing_name: dup.name.clone(),
            });
        }
        let name = canonical
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| canonical.display().to_string());
        let now = now_rfc3339();
        let record = WorkspaceRecord {
            workspace_id: Uuid::new_v4(),
            name,
            path: canonical,
            created_at: now.clone(),
            last_used_at: now,
            open_tab_id: None,
            conversation_rounds_count: 0,
        };
        guard.records.insert(record.workspace_id, record.clone());
        Self::persist(&guard)?;
        Ok(self.with_conversation_count(record))
    }

    pub fn open_workspace(
        &self,
        workspace_id: Uuid,
        tab_id: &str,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
        let mut guard = self.inner.lock().expect("WorkspaceRegistry poisoned");
        let record = guard.records.get(&workspace_id).cloned().ok_or_else(|| {
            WorkspaceError::NotFound {
                workspace_id: workspace_id.to_string(),
            }
        })?;
        if let Some(existing) = record.open_tab_id.as_deref() {
            if existing != tab_id {
                return Err(WorkspaceError::AlreadyOpen {
                    existing_tab_id: existing.to_string(),
                });
            }
        }
        let now = now_rfc3339();
        let updated = WorkspaceRecord {
            open_tab_id: Some(tab_id.to_string()),
            last_used_at: now,
            ..record
        };
        guard.records.insert(workspace_id, updated.clone());
        Self::persist(&guard)?;
        Ok(self.with_conversation_count(updated))
    }

    pub fn close_workspace(&self, workspace_id: Uuid) -> Result<(), WorkspaceError> {
        let mut guard = self.inner.lock().expect("WorkspaceRegistry poisoned");
        let mut record = guard.records.get(&workspace_id).cloned().ok_or_else(|| {
            WorkspaceError::NotFound {
                workspace_id: workspace_id.to_string(),
            }
        })?;
        record.open_tab_id = None;
        guard.records.insert(workspace_id, record);
        Self::persist(&guard)?;
        Ok(())
    }

    pub fn resolve_for_tab(&self, tab_id: &str) -> Option<WorkspaceRecord> {
        let guard = self.inner.lock().expect("WorkspaceRegistry poisoned");
        let found = guard
            .records
            .values()
            .find(|r| r.open_tab_id.as_deref() == Some(tab_id))
            .cloned();
        drop(guard);
        found.map(|r| self.with_conversation_count(r))
    }
}

impl RegistryInner {
    /// Locate an existing workspace whose canonical path refers to
    /// the same physical directory as `candidate`. Uses
    /// [`paths_refer_to_same_workspace`] so macOS APFS case-
    /// insensitive aliases and any residual symlink edge cases are
    /// rejected, not just byte-distinct canonical paths.
    fn find_duplicate(&self, candidate: &Path) -> Option<&WorkspaceRecord> {
        self.records
            .values()
            .find(|r| paths_refer_to_same_workspace(&r.path, candidate))
    }
}

pub fn validate_workspace_name(name: &str) -> Result<(), WorkspaceError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(WorkspaceError::InvalidName {
            reason: "name must not be empty or whitespace-only".into(),
        });
    }
    let char_count = trimmed.chars().count();
    if char_count > NAME_MAX_CHARS {
        return Err(WorkspaceError::InvalidName {
            reason: format!(
                "name length {} exceeds maximum {} characters",
                char_count, NAME_MAX_CHARS
            ),
        });
    }
    if trimmed == "." || trimmed == ".." {
        return Err(WorkspaceError::InvalidName {
            reason: "name `.` or `..` is reserved".into(),
        });
    }
    if trimmed.starts_with('.') {
        return Err(WorkspaceError::InvalidName {
            reason: "name must not start with `.`".into(),
        });
    }
    for c in trimmed.chars() {
        if c == '/' || c == '\\' {
            return Err(WorkspaceError::InvalidName {
                reason: format!("name must not contain path separator `{c}`"),
            });
        }
        if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
            return Err(WorkspaceError::InvalidName {
                reason: format!("name must not contain forbidden character `{c}`"),
            });
        }
        if c.is_control() {
            return Err(WorkspaceError::InvalidName {
                reason: "name must not contain control characters".into(),
            });
        }
    }
    Ok(())
}

/// Best-effort count of `.jsonl` files under `<workspace>/.claude/`.
/// Used to populate `WorkspaceRecord::conversation_rounds_count` at
/// command return time. Bounded by a directory-visit budget so a
/// pathological workspace (e.g. one that user-pointed at `/`) can't
/// stall the registry. Returns 0 when `.claude/` is missing or
/// unreadable.
const CLAUDE_SCAN_DIR_BUDGET: usize = 256;

pub fn count_claude_conversation_rounds(workspace_path: &Path) -> u32 {
    let root = workspace_path.join(".claude");
    if !root.is_dir() {
        return 0;
    }
    let mut stack: Vec<PathBuf> = vec![root];
    let mut count: u32 = 0;
    let mut visited: usize = 0;
    while let Some(dir) = stack.pop() {
        if visited >= CLAUDE_SCAN_DIR_BUDGET {
            break;
        }
        visited += 1;
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else { continue };
            let p = entry.path();
            if file_type.is_dir() {
                stack.push(p);
            } else if file_type.is_file()
                && p.extension().and_then(|s| s.to_str()) == Some("jsonl")
            {
                count = count.saturating_add(1);
            }
        }
    }
    count
}

fn default_workspaces_root() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| {
        b.home_dir()
            .join("AgentPlatform")
            .join("workspaces")
    })
}

fn now_rfc3339() -> String {
    // Reuse the same chrono-free helper shape as claude_discovery.
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() as i64;
    format_rfc3339_utc(secs)
}

fn format_rfc3339_utc(unix_secs: i64) -> String {
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

// ---------------------------------------------------------------------------
// Tauri command surface
// ---------------------------------------------------------------------------

fn parse_workspace_id(raw: &str) -> Result<Uuid, WorkspaceErrorDto> {
    Uuid::parse_str(raw).map_err(|_| {
        WorkspaceErrorDto::from(&WorkspaceError::NotFound {
            workspace_id: raw.to_string(),
        })
    })
}

#[tauri::command]
pub fn list_workspaces(
    registry: State<'_, WorkspaceRegistry>,
) -> Result<Vec<WorkspaceRecord>, WorkspaceErrorDto> {
    Ok(registry.list())
}

#[tauri::command]
pub fn create_workspace(
    name: String,
    registry: State<'_, WorkspaceRegistry>,
) -> Result<WorkspaceRecord, WorkspaceErrorDto> {
    registry
        .create_workspace(&name)
        .map_err(|e| WorkspaceErrorDto::from(&e))
}

#[tauri::command]
pub fn register_workspace(
    path: String,
    registry: State<'_, WorkspaceRegistry>,
) -> Result<WorkspaceRecord, WorkspaceErrorDto> {
    registry
        .register_workspace(Path::new(&path))
        .map_err(|e| WorkspaceErrorDto::from(&e))
}

#[tauri::command]
pub fn open_workspace(
    workspace_id: String,
    tab_id: String,
    registry: State<'_, WorkspaceRegistry>,
) -> Result<WorkspaceRecord, WorkspaceErrorDto> {
    let id = parse_workspace_id(&workspace_id)?;
    registry
        .open_workspace(id, &tab_id)
        .map_err(|e| WorkspaceErrorDto::from(&e))
}

#[tauri::command]
pub fn close_workspace(
    workspace_id: String,
    registry: State<'_, WorkspaceRegistry>,
) -> Result<(), WorkspaceErrorDto> {
    let id = parse_workspace_id(&workspace_id)?;
    registry
        .close_workspace(id)
        .map_err(|e| WorkspaceErrorDto::from(&e))
}

#[tauri::command]
pub fn resolve_workspace_for_tab(
    tab_id: String,
    registry: State<'_, WorkspaceRegistry>,
) -> Result<Option<WorkspaceRecord>, WorkspaceErrorDto> {
    Ok(registry.resolve_for_tab(&tab_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_registry() -> (tempfile::TempDir, tempfile::TempDir, WorkspaceRegistry) {
        let storage = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        let workspaces_root = home.path().join("AgentPlatform").join("workspaces");
        std::fs::create_dir_all(&workspaces_root).unwrap();
        let reg = WorkspaceRegistry::empty(
            storage.path().to_path_buf(),
            Some(workspaces_root.clone()),
        );
        (storage, home, reg)
    }

    fn registry_with_one_workspace(name: &str) -> (WorkspaceRegistry, Uuid) {
        let registry = WorkspaceRegistry::empty(
            std::path::PathBuf::from("/tmp/workspaces-test"),
            None,
        );
        let id = Uuid::new_v4();
        let record = WorkspaceRecord {
            workspace_id: id,
            name: name.into(),
            path: std::path::PathBuf::from("/tmp/test-workspace"),
            created_at: "2025-01-01T00:00:00Z".into(),
            last_used_at: "2025-01-01T00:00:00Z".into(),
            open_tab_id: None,
            conversation_rounds_count: 0,
        };
        let mut guard = registry.inner.lock().expect("registry mutex");
        guard.records.insert(id, record);
        drop(guard);
        (registry, id)
    }

    #[test]
    fn lookup_name_returns_name_for_known_workspace() {
        let (registry, id) = registry_with_one_workspace("Alice's Project");
        let name = registry.lookup_name(&id.to_string());
        assert_eq!(name.as_deref(), Some("Alice's Project"));
    }

    #[test]
    fn lookup_name_returns_none_for_unknown_id() {
        let (registry, _) = registry_with_one_workspace("ignored");
        let other = Uuid::new_v4();
        assert!(registry.lookup_name(&other.to_string()).is_none());
    }

    #[test]
    fn lookup_name_returns_none_for_malformed_id() {
        let (registry, _) = registry_with_one_workspace("ignored");
        assert!(registry.lookup_name("not-a-uuid").is_none());
        assert!(registry.lookup_name("").is_none());
    }

    #[test]
    fn validate_name_accepts_simple_ascii_and_cjk() {
        assert!(validate_workspace_name("alpha").is_ok());
        assert!(validate_workspace_name("alpha-beta_v2.1").is_ok());
        assert!(validate_workspace_name("项目-中文").is_ok());
        assert!(validate_workspace_name("プロジェクト").is_ok());
    }

    #[test]
    fn validate_name_rejects_empty_and_whitespace_only() {
        assert!(matches!(
            validate_workspace_name(""),
            Err(WorkspaceError::InvalidName { .. })
        ));
        assert!(matches!(
            validate_workspace_name("   \t  "),
            Err(WorkspaceError::InvalidName { .. })
        ));
    }

    #[test]
    fn validate_name_rejects_path_traversal_components() {
        for n in [".", "..", "../foo", "foo/bar", "foo\\bar", ".hidden"] {
            assert!(
                matches!(
                    validate_workspace_name(n),
                    Err(WorkspaceError::InvalidName { .. })
                ),
                "name {n:?} should be rejected"
            );
        }
    }

    #[test]
    fn validate_name_rejects_platform_forbidden_chars() {
        for n in [
            "foo<bar", "foo>bar", "foo:bar", "foo\"bar", "foo|bar", "foo?bar",
            "foo*bar", "foo\u{0007}bar",
        ] {
            assert!(
                matches!(
                    validate_workspace_name(n),
                    Err(WorkspaceError::InvalidName { .. })
                ),
                "name {n:?} should be rejected"
            );
        }
    }

    #[test]
    fn validate_name_rejects_over_64_chars() {
        let long: String = "a".repeat(65);
        assert!(matches!(
            validate_workspace_name(&long),
            Err(WorkspaceError::InvalidName { .. })
        ));
    }

    #[test]
    fn create_workspace_creates_dir_under_workspaces_root() {
        let (_storage, home, reg) = fresh_registry();
        let rec = reg.create_workspace("demo").expect("create");
        let target = home.path().join("AgentPlatform").join("workspaces").join("demo");
        assert!(target.is_dir());
        let canonical = std::fs::canonicalize(&target).unwrap();
        assert_eq!(rec.path, canonical);
        assert_eq!(rec.name, "demo");
        assert_eq!(rec.open_tab_id, None);
        assert!(!target.join(".git").exists(), "no auto-git-init per spec");
    }

    #[test]
    fn create_workspace_rejects_duplicate_name_via_already_exists() {
        let (_storage, _home, reg) = fresh_registry();
        reg.create_workspace("twin").expect("first");
        let err = reg.create_workspace("twin").unwrap_err();
        match err {
            WorkspaceError::WorkspaceAlreadyExists { .. } => {}
            other => panic!("expected WorkspaceAlreadyExists; got {other:?}"),
        }
    }

    #[test]
    fn register_workspace_accepts_existing_dir_without_modifying_contents() {
        let (_storage, home, reg) = fresh_registry();
        let dir = home.path().join("external-project");
        std::fs::create_dir_all(&dir).unwrap();
        let canary = dir.join("README.md");
        std::fs::write(&canary, b"untouched").unwrap();
        let rec = reg.register_workspace(&dir).expect("register");
        assert_eq!(rec.path, std::fs::canonicalize(&dir).unwrap());
        // Verify the file is byte-identical (no copy/modification).
        let body = std::fs::read(&canary).unwrap();
        assert_eq!(body, b"untouched");
    }

    #[test]
    fn register_workspace_rejects_non_directory_or_missing_path() {
        let (_storage, home, reg) = fresh_registry();
        let missing = home.path().join("does-not-exist");
        match reg.register_workspace(&missing).unwrap_err() {
            WorkspaceError::NotADirectory { .. } => {}
            other => panic!("expected NotADirectory for missing; got {other:?}"),
        }
        let file = home.path().join("a-file.txt");
        std::fs::write(&file, b"x").unwrap();
        match reg.register_workspace(&file).unwrap_err() {
            WorkspaceError::NotADirectory { .. } => {}
            other => panic!("expected NotADirectory for file; got {other:?}"),
        }
    }

    #[test]
    fn canonical_duplicate_rejected_for_register_after_create() {
        let (_storage, _home, reg) = fresh_registry();
        let created = reg.create_workspace("alpha").expect("create");
        // Try to register the same canonical path again.
        let err = reg.register_workspace(&created.path).unwrap_err();
        match err {
            WorkspaceError::CanonicalDuplicate {
                existing_workspace_id,
                existing_name,
            } => {
                assert_eq!(existing_workspace_id, created.workspace_id);
                assert_eq!(existing_name, created.name);
            }
            other => panic!("expected CanonicalDuplicate; got {other:?}"),
        }
    }

    #[test]
    fn persistence_round_trip() {
        let (storage, home, reg) = fresh_registry();
        let workspaces_root = home.path().join("AgentPlatform").join("workspaces");
        reg.create_workspace("one").unwrap();
        reg.create_workspace("two").unwrap();
        // Drop and reload from the same storage_root.
        drop(reg);
        let reg2 = WorkspaceRegistry::load(
            storage.path().to_path_buf(),
            Some(workspaces_root.clone()),
        );
        let listed = reg2.list();
        assert_eq!(listed.len(), 2);
        let names: std::collections::HashSet<_> = listed.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains("one"));
        assert!(names.contains("two"));
    }

    #[test]
    fn open_then_close_clears_open_tab_id() {
        let (_storage, _home, reg) = fresh_registry();
        let rec = reg.create_workspace("ws").unwrap();
        let after_open = reg.open_workspace(rec.workspace_id, "tab-1").unwrap();
        assert_eq!(after_open.open_tab_id.as_deref(), Some("tab-1"));
        reg.close_workspace(rec.workspace_id).unwrap();
        let listed = reg.list();
        let same = listed.iter().find(|r| r.workspace_id == rec.workspace_id).unwrap();
        assert_eq!(same.open_tab_id, None);
    }

    #[test]
    fn open_when_already_open_by_different_tab_returns_already_open() {
        let (_storage, _home, reg) = fresh_registry();
        let rec = reg.create_workspace("ws").unwrap();
        reg.open_workspace(rec.workspace_id, "tab-a").unwrap();
        match reg.open_workspace(rec.workspace_id, "tab-b").unwrap_err() {
            WorkspaceError::AlreadyOpen { existing_tab_id } => {
                assert_eq!(existing_tab_id, "tab-a");
            }
            other => panic!("expected AlreadyOpen; got {other:?}"),
        }
        // Re-opening with the same tab_id is idempotent (no error).
        let again = reg.open_workspace(rec.workspace_id, "tab-a").unwrap();
        assert_eq!(again.open_tab_id.as_deref(), Some("tab-a"));
    }

    #[test]
    fn close_when_not_open_is_idempotent() {
        let (_storage, _home, reg) = fresh_registry();
        let rec = reg.create_workspace("ws").unwrap();
        // Close without prior open — should succeed and remain closed.
        reg.close_workspace(rec.workspace_id).unwrap();
        reg.close_workspace(rec.workspace_id).unwrap();
        let listed = reg.list();
        assert_eq!(listed[0].open_tab_id, None);
    }

    #[test]
    fn close_unknown_workspace_returns_not_found() {
        let (_storage, _home, reg) = fresh_registry();
        let bogus = Uuid::new_v4();
        match reg.close_workspace(bogus).unwrap_err() {
            WorkspaceError::NotFound { workspace_id } => {
                assert_eq!(workspace_id, bogus.to_string());
            }
            other => panic!("expected NotFound; got {other:?}"),
        }
    }

    #[test]
    fn resolve_for_tab_returns_open_record() {
        let (_storage, _home, reg) = fresh_registry();
        let rec = reg.create_workspace("ws").unwrap();
        assert!(reg.resolve_for_tab("tab-1").is_none());
        reg.open_workspace(rec.workspace_id, "tab-1").unwrap();
        let r = reg.resolve_for_tab("tab-1").expect("resolved");
        assert_eq!(r.workspace_id, rec.workspace_id);
    }

    #[test]
    fn error_dto_serializes_with_kind_discriminant() {
        let dto = WorkspaceErrorDto::from(&WorkspaceError::InvalidName {
            reason: "bad".into(),
        });
        let v: serde_json::Value = serde_json::to_value(&dto).unwrap();
        assert_eq!(v.get("kind").and_then(|x| x.as_str()), Some("invalidName"));
        assert_eq!(v.get("reason").and_then(|x| x.as_str()), Some("bad"));
    }

    /// Codex round-28 blocker #1 regression: directory-identity check
    /// rejects a symlink alias even though `canonicalize` already
    /// collapses symlinks in most cases. This test pins the contract
    /// against future refactors that might short-circuit identity
    /// checks for byte-distinct candidates.
    #[cfg(unix)]
    #[test]
    fn register_workspace_rejects_symlink_alias_of_existing_workspace() {
        use std::os::unix::fs::symlink;
        let (_storage, home, reg) = fresh_registry();
        let real = home.path().join("real-workspace");
        std::fs::create_dir_all(&real).unwrap();
        let created = reg.register_workspace(&real).expect("register real");

        let alias = home.path().join("alias-link");
        symlink(&real, &alias).unwrap();
        let err = reg.register_workspace(&alias).unwrap_err();
        match err {
            WorkspaceError::CanonicalDuplicate {
                existing_workspace_id,
                existing_name,
            } => {
                assert_eq!(existing_workspace_id, created.workspace_id);
                assert_eq!(existing_name, created.name);
            }
            other => panic!("expected CanonicalDuplicate; got {other:?}"),
        }
    }

    /// Codex round-28 blocker #1 regression: the case-insensitive
    /// alias path. On macOS APFS / NTFS / HFS+, `canonicalize`
    /// preserves the case the user typed, so byte-wise `PathBuf ==
    /// PathBuf` misses true identity. The dev/inode fallback catches
    /// it. On case-sensitive filesystems (Linux ext4) the alternate
    /// path doesn't exist, so we skip the assertion path; the test
    /// still passes structurally.
    #[cfg(unix)]
    #[test]
    fn register_workspace_rejects_case_insensitive_alias_on_case_insensitive_filesystem() {
        let (_storage, home, reg) = fresh_registry();
        let mixed = home.path().join("MixedCase");
        std::fs::create_dir_all(&mixed).unwrap();
        let lower = home.path().join("mixedcase");

        // Detect filesystem case-sensitivity at runtime: if statting
        // the lowercased name succeeds AND points to the same inode,
        // the filesystem is case-insensitive (macOS APFS default,
        // NTFS, HFS+). Otherwise (Linux ext4) skip the assertion —
        // the lowercased path is genuinely a different directory.
        let case_insensitive = match (std::fs::metadata(&mixed), std::fs::metadata(&lower)) {
            (Ok(a), Ok(b)) => {
                use std::os::unix::fs::MetadataExt;
                a.dev() == b.dev() && a.ino() == b.ino()
            }
            _ => false,
        };

        let created = reg.register_workspace(&mixed).expect("register mixed");

        if case_insensitive {
            let err = reg.register_workspace(&lower).unwrap_err();
            match err {
                WorkspaceError::CanonicalDuplicate {
                    existing_workspace_id,
                    existing_name,
                } => {
                    assert_eq!(existing_workspace_id, created.workspace_id);
                    assert_eq!(existing_name, created.name);
                }
                other => panic!(
                    "expected CanonicalDuplicate on case-insensitive FS; got {other:?}"
                ),
            }
        } else {
            eprintln!(
                "case-sensitive filesystem detected; skipping case-insensitive alias assertion"
            );
        }
    }

    /// Codex round-28 blocker #2 regression: the persisted JSON shape
    /// uses snake_case keys, decoupled from the camelCase wire shape
    /// the Tauri IPC layer returns to the frontend.
    /// Codex round-29 blocker regression: persisted `open_tab_id`
    /// must not survive a restart. After load, the in-memory record
    /// must show no lock so the user can reopen the workspace with
    /// a fresh tab id without hitting `AlreadyOpen`.
    #[test]
    fn open_persist_reload_reopen_with_fresh_tab_id_succeeds() {
        let (storage, home, reg) = fresh_registry();
        let workspaces_root = home.path().join("AgentPlatform").join("workspaces");
        let created = reg.create_workspace("restart-demo").expect("create");
        let opened = reg
            .open_workspace(created.workspace_id, "tab-old")
            .expect("open original");
        assert_eq!(opened.open_tab_id.as_deref(), Some("tab-old"));
        // Drop the registry instance; the disk file still records
        // `open_tab_id = "tab-old"` for diagnostic purposes.
        drop(reg);

        // Reload from the same storage root, mimicking app restart.
        let reg2 = WorkspaceRegistry::load(
            storage.path().to_path_buf(),
            Some(workspaces_root),
        );
        let listed = reg2.list();
        let found = listed
            .iter()
            .find(|r| r.workspace_id == created.workspace_id)
            .expect("reloaded record present");
        assert_eq!(
            found.open_tab_id, None,
            "load must clear runtime-only open_tab_id; got {:?}",
            found.open_tab_id
        );

        // Reopening with a DIFFERENT tab id must now succeed.
        let reopened = reg2
            .open_workspace(created.workspace_id, "tab-new")
            .expect("reopen post-restart with fresh tab id");
        assert_eq!(reopened.open_tab_id.as_deref(), Some("tab-new"));
    }

    #[test]
    fn persisted_json_uses_snake_case_keys() {
        let (storage, _home, reg) = fresh_registry();
        let _ = reg.create_workspace("snake").expect("create");
        let body = std::fs::read_to_string(storage.path().join("workspaces.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let workspaces = v
            .get("workspaces")
            .and_then(|x| x.as_array())
            .expect("workspaces array");
        let entry = workspaces.first().expect("at least one record");
        // snake_case keys required on disk:
        for k in ["workspace_id", "name", "path", "created_at", "last_used_at", "open_tab_id"] {
            assert!(
                entry.get(k).is_some(),
                "on-disk key `{k}` missing from {entry:?}"
            );
        }
        // camelCase wire-only keys must not appear on disk:
        for k in ["workspaceId", "createdAt", "lastUsedAt", "openTabId"] {
            assert!(
                entry.get(k).is_none(),
                "wire-only camelCase key `{k}` leaked to disk in {entry:?}"
            );
        }
        // conversation_rounds_count is computed at command return
        // time only — never persisted, regardless of case.
        for k in ["conversation_rounds_count", "conversationRoundsCount"] {
            assert!(
                entry.get(k).is_none(),
                "computed-only key `{k}` leaked to disk in {entry:?}"
            );
        }
    }

    /// Codex round-30 task18 contract: helper returns 0 for a fresh
    /// workspace dir with no `.claude/`.
    #[test]
    fn count_claude_conversation_rounds_returns_zero_when_no_dot_claude() {
        let (_storage, home, _reg) = fresh_registry();
        let bare = home.path().join("bare-workspace");
        std::fs::create_dir_all(&bare).unwrap();
        assert_eq!(count_claude_conversation_rounds(&bare), 0);
    }

    /// Codex round-30 task18 contract: helper recursively counts
    /// `*.jsonl` files under `.claude/`, skipping non-jsonl siblings.
    #[test]
    fn count_claude_conversation_rounds_counts_nested_jsonl_files() {
        let (_storage, home, _reg) = fresh_registry();
        let ws = home.path().join("claude-ws");
        std::fs::create_dir_all(ws.join(".claude/projects/foo")).unwrap();
        std::fs::create_dir_all(ws.join(".claude/other")).unwrap();
        std::fs::create_dir_all(ws.join(".claude/extra")).unwrap();
        std::fs::write(ws.join(".claude/projects/foo/a.jsonl"), b"{}").unwrap();
        std::fs::write(ws.join(".claude/projects/foo/b.jsonl"), b"{}").unwrap();
        std::fs::write(ws.join(".claude/other/c.jsonl"), b"{}").unwrap();
        std::fs::write(ws.join(".claude/extra/d.txt"), b"not jsonl").unwrap();
        assert_eq!(count_claude_conversation_rounds(&ws), 3);
    }

    /// `list_workspaces` populates `conversation_rounds_count` at
    /// return time from the workspace's `.claude/` directory.
    #[test]
    fn list_workspaces_populates_conversation_rounds_count() {
        let (_storage, _home, reg) = fresh_registry();
        let created = reg.create_workspace("counted").expect("create");
        // Seed two .jsonl files under the auto-created workspace.
        let claude = created.path.join(".claude/projects/counted");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(claude.join("a.jsonl"), b"{}").unwrap();
        std::fs::write(claude.join("b.jsonl"), b"{}").unwrap();
        let listed = reg.list();
        let found = listed
            .iter()
            .find(|r| r.workspace_id == created.workspace_id)
            .expect("present");
        assert_eq!(found.conversation_rounds_count, 2);
    }

    #[test]
    fn list_sorted_descending_by_last_used_at() {
        let (_storage, _home, reg) = fresh_registry();
        let a = reg.create_workspace("a").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let b = reg.create_workspace("b").unwrap();
        // `b` was created later → should be first when sorted desc by
        // last_used_at. The RFC3339 helper has second resolution, so
        // we slept >1s to guarantee a different timestamp.
        let listed = reg.list();
        assert_eq!(listed[0].workspace_id, b.workspace_id);
        assert_eq!(listed[1].workspace_id, a.workspace_id);
    }
}
