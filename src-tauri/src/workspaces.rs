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
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;

const WORKSPACES_FILENAME: &str = "workspaces.json";
const NAME_MAX_CHARS: usize = 64;

/// Where a workspace lives. `Local` workspaces own a filesystem path
/// the host can canonicalize and inode-check; `Remote` workspaces
/// live behind an SSH endpoint (optionally inside a Docker container
/// on the remote host) and are shell-only in v1.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WorkspaceLocation {
    Local { path: PathBuf },
    Remote {
        ssh: SshLocation,
        #[serde(default)]
        container: Option<ContainerLocation>,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SshLocation {
    /// Defaults to the local user's username when `None`.
    pub user: Option<String>,
    pub host: String,
    /// Canonicalizes to 22 when `None`.
    pub port: Option<u16>,
    /// Remote cwd used as the wrapper's `$AM_REMOTE_CWD`. Required
    /// because `docs/specs/transport.md` §6.1/§6.2 include it in the
    /// remote duplicate-identity tuple — the persisted shape MUST
    /// always carry a remote cwd. The initial value is whatever the
    /// user typed; the SSH transport may canonicalize it post-connect
    /// and rewrite this field in place.
    pub canonical_remote_path: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContainerLocation {
    pub container_id: String,
    pub cwd_in_container: Option<String>,
}

/// Per-workspace claude launch policy. The defaults match the
/// product spec: auto-launch is opt-in but defaults ON, with
/// `--dangerously-skip-permissions` as the argv. The launch
/// scheduler reads these fields to decide whether and how to spawn
/// claude on Local workspace open.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceProfile {
    pub auto_launch_claude: bool,
    pub claude_argv: Vec<String>,
    /// When `true` the workspace is parked in the 暂存区 rail section.
    /// The PTY is kept alive (if open); the tab is excluded from
    /// auto-focus routing. Defaults to `false` for existing records.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stashed: bool,
}

impl WorkspaceProfile {
    pub fn default_local() -> Self {
        Self {
            auto_launch_claude: true,
            claude_argv: vec!["--dangerously-skip-permissions".to_string()],
            stashed: false,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRecord {
    pub workspace_id: Uuid,
    pub name: String,
    /// Where the workspace lives. Replaces the old flat `path` field
    /// so SSH / Docker-over-SSH workspaces can be persisted alongside
    /// Local ones.
    pub location: WorkspaceLocation,
    /// Launch policy (auto-launch claude + argv).
    pub profile: WorkspaceProfile,
    pub created_at: String,
    pub last_used_at: String,
    pub open_tab_id: Option<String>,
    /// Best-effort claude conversation rounds count derived from
    /// scanning `<path>/.claude/` for `*.jsonl` files at command
    /// return time. Computed, not persisted — `StoredWorkspaceRecord`
    /// excludes this field deliberately so the registry never
    /// becomes an authoritative cache. Defaults to 0 when `.claude/`
    /// is missing or unreadable. For Remote workspaces this stays 0
    /// (the host cannot scan a remote `.claude/` without a remote
    /// agent, which is a v2 surface).
    #[serde(default)]
    pub conversation_rounds_count: u32,
}

impl WorkspaceRecord {
    /// Filesystem path for Local workspaces, `None` for Remote.
    /// Callers that previously read the flat `path` field migrate to
    /// this helper.
    pub fn local_path(&self) -> Option<&Path> {
        match &self.location {
            WorkspaceLocation::Local { path } => Some(path.as_path()),
            WorkspaceLocation::Remote { .. } => None,
        }
    }
}

impl WorkspaceLocation {
    /// Convert the registry-side `WorkspaceLocation` (nested
    /// `Remote { ssh, container }` shape per spec §6.1) to the
    /// transport-side core `WorkspaceLocation` (flat
    /// `Remote { user, host, port, canonical_remote_path, container
    /// }` shape that the transport modules consume directly). Used
    /// by the auto-launch executor to thread the workspace location
    /// into `TerminalSpec::workspace_location`.
    pub fn to_core_workspace_location(&self) -> terminal_mesh_core::transport::WorkspaceLocation {
        use terminal_mesh_core::transport::{
            ContainerLocation as CoreContainerLocation,
            WorkspaceLocation as CoreWorkspaceLocation,
        };
        match self {
            WorkspaceLocation::Local { path } => CoreWorkspaceLocation::Local {
                path: Some(path.clone()),
            },
            WorkspaceLocation::Remote { ssh, container } => CoreWorkspaceLocation::Remote {
                user: ssh.user.clone(),
                host: ssh.host.clone(),
                port: ssh.port,
                canonical_remote_path: ssh.canonical_remote_path.clone(),
                container: container.as_ref().map(|c| CoreContainerLocation {
                    container_id: c.container_id.clone(),
                    cwd_in_container: c.cwd_in_container.clone(),
                }),
            },
        }
    }
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

    /// One of the Remote workspace fields (host / canonical remote
    /// path / port / container id) failed validation. `field`
    /// identifies which input the frontend should highlight.
    #[error("remote field `{field}` invalid: {reason}")]
    RemoteFieldInvalid { field: String, reason: String },
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
    RemoteFieldInvalid { field: String, reason: String },
    /// Preflight transport probe failed before the Remote workspace
    /// record was persisted. `phase` distinguishes `ssh` (auth /
    /// connect / host-key / remote-path-invalid) from `docker`
    /// (docker-exec / container-missing / non-TTY); `reason` is the
    /// typed transport error's display message. The registry on
    /// disk is byte-identical to the pre-call state, so a retry
    /// with corrected fields is safe.
    RemoteProbeFailed { phase: String, reason: String },
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
            WorkspaceError::RemoteFieldInvalid { field, reason } => Self::RemoteFieldInvalid {
                field: field.clone(),
                reason: reason.clone(),
            },
        }
    }
}

/// Disk-side mirror of `WorkspaceRecord`. Snake_case keys (no
/// `rename_all`) so `workspaces.json` reads like a normal shell-tools
/// JSON file. Separated from the wire `WorkspaceRecord` so the
/// Tauri-IPC camelCase convention and the on-disk snake_case
/// convention can evolve independently.
///
/// `location` and `profile` are written for every new record. The
/// legacy `path` field is kept `Option<PathBuf>` for backward-
/// compatible reads of legacy `workspaces.json` files (where `path`
/// was the only location indicator). Writes leave `path = None`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredWorkspaceRecord {
    workspace_id: Uuid,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    location: Option<WorkspaceLocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<WorkspaceProfile>,
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
            path: None,
            location: Some(r.location.clone()),
            profile: Some(r.profile.clone()),
            created_at: r.created_at.clone(),
            last_used_at: r.last_used_at.clone(),
            open_tab_id: r.open_tab_id.clone(),
        }
    }
}

/// Errors raised while migrating a single on-disk record into the
/// live registry. Surfaced as warnings + record-drops at load time;
/// the registry tolerates a corrupted entry by skipping it.
#[derive(Debug, thiserror::Error)]
enum StoredRecordMigrationError {
    #[error("stored record `{workspace_id}` has neither `location` nor legacy `path`")]
    MissingLocation { workspace_id: Uuid },
}

impl TryFrom<StoredWorkspaceRecord> for WorkspaceRecord {
    type Error = StoredRecordMigrationError;

    fn try_from(s: StoredWorkspaceRecord) -> Result<Self, Self::Error> {
        let location = match (s.location, s.path) {
            (Some(loc), _) => loc,
            (None, Some(p)) => WorkspaceLocation::Local { path: p },
            (None, None) => {
                return Err(StoredRecordMigrationError::MissingLocation {
                    workspace_id: s.workspace_id,
                });
            }
        };
        let profile = s.profile.unwrap_or_else(WorkspaceProfile::default_local);
        Ok(Self {
            workspace_id: s.workspace_id,
            name: s.name,
            location,
            profile,
            created_at: s.created_at,
            last_used_at: s.last_used_at,
            open_tab_id: s.open_tab_id,
            // Caller is expected to populate via
            // `count_claude_conversation_rounds(record.local_path())`
            // before returning to the frontend; default 0 here so
            // an unpopulated record is still serializable.
            conversation_rounds_count: 0,
        })
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
                    .filter_map(|s| {
                        let workspace_id = s.workspace_id;
                        match WorkspaceRecord::try_from(s) {
                            Ok(mut r) => {
                                // `open_tab_id` is RUNTIME state: a
                                // tab id only has meaning within the
                                // lifetime of a single frontend
                                // session. The disk slot is preserved
                                // purely for diagnostics (what was
                                // open at last shutdown); on load we
                                // clear it so a fresh session starts
                                // with zero workspace locks. This
                                // unblocks the open-after-restart
                                // path that would otherwise return
                                // `AlreadyOpen` for a stale tab id
                                // from the previous launch.
                                r.open_tab_id = None;
                                Some((r.workspace_id, r))
                            }
                            Err(e) => {
                                tracing::warn!(
                                    %workspace_id,
                                    %e,
                                    "stored workspace record skipped during migration"
                                );
                                None
                            }
                        }
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
            r.conversation_rounds_count = match r.local_path() {
                Some(p) => count_claude_conversation_rounds(p),
                None => 0,
            };
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

    /// Full-record lookup by `Uuid` for callers (e.g. the auto-launch
    /// scheduler) that need the `WorkspaceLocation` and `WorkspaceProfile`
    /// fields, not just the friendly name. Returns a `clone()` so the
    /// caller does not hold the registry mutex.
    pub fn find_by_id(&self, workspace_id: Uuid) -> Option<WorkspaceRecord> {
        let guard = self.inner.lock().expect("WorkspaceRegistry poisoned");
        guard.records.get(&workspace_id).cloned()
    }

    /// Public projection of `RegistryInner::find_remote_duplicate`
    /// for callers (e.g. the preflight `try_register_remote_*`
    /// helper) that need to fast-reject duplicate identity tuples
    /// BEFORE running a slow network probe. The registry's
    /// `register_remote_workspace` method still enforces the
    /// same check at insert time as defense in depth.
    pub fn find_remote_duplicate(
        &self,
        ssh: &SshLocation,
        container: Option<&ContainerLocation>,
    ) -> Option<WorkspaceRecord> {
        let guard = self.inner.lock().expect("WorkspaceRegistry poisoned");
        guard.find_remote_duplicate(ssh, container).cloned()
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
        record.conversation_rounds_count = match record.local_path() {
            Some(p) => count_claude_conversation_rounds(p),
            None => 0,
        };
        record
    }

    /// Inject the workspaces-root override at runtime so test setups
    /// can point auto-created workspaces at a tempdir HOME.
    #[allow(dead_code)]
    pub fn override_workspaces_root(&self, root: PathBuf) {
        let mut guard = self.inner.lock().expect("WorkspaceRegistry poisoned");
        guard.workspaces_root = Some(root);
    }

    pub fn create_workspace(
        &self,
        name: &str,
        auto_launch_claude: bool,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
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
        let mut profile = WorkspaceProfile::default_local();
        profile.auto_launch_claude = auto_launch_claude;
        let record = WorkspaceRecord {
            workspace_id: Uuid::new_v4(),
            name: name.to_string(),
            location: WorkspaceLocation::Local { path: canonical },
            profile,
            created_at: now.clone(),
            last_used_at: now,
            open_tab_id: None,
            conversation_rounds_count: 0,
        };
        guard.records.insert(record.workspace_id, record.clone());
        Self::persist(&guard)?;
        Ok(self.with_conversation_count(record))
    }

    pub fn register_workspace(
        &self,
        path: &Path,
        auto_launch_claude: bool,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
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
        let mut profile = WorkspaceProfile::default_local();
        profile.auto_launch_claude = auto_launch_claude;
        let record = WorkspaceRecord {
            workspace_id: Uuid::new_v4(),
            name,
            location: WorkspaceLocation::Local { path: canonical },
            profile,
            created_at: now.clone(),
            last_used_at: now,
            open_tab_id: None,
            conversation_rounds_count: 0,
        };
        guard.records.insert(record.workspace_id, record.clone());
        Self::persist(&guard)?;
        Ok(self.with_conversation_count(record))
    }

    /// Persist a `WorkspaceLocation::Remote` record. Validates the
    /// host / canonical_remote_path / port / container_id fields per
    /// the spec §6.1 identity tuple invariants and the per-field
    /// non-empty contract the Remote workspace registration uses.
    pub fn register_remote_workspace(
        &self,
        name: &str,
        ssh: SshLocation,
        container: Option<ContainerLocation>,
        auto_launch_claude: bool,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
        validate_workspace_name(name)?;
        validate_remote_fields(&ssh, container.as_ref())?;
        let mut guard = self.inner.lock().expect("WorkspaceRegistry poisoned");
        // Reject if an existing Remote workspace already points at
        // the same identity tuple (`docs/specs/transport.md` §6.1).
        // Without this check the same SSH target can be registered
        // multiple times and appear as separate workspaces in the
        // rail / switcher. Reuses the existing CanonicalDuplicate
        // variant since the user-facing meaning is the same:
        // "this target is already registered."
        if let Some(dup) = guard.find_remote_duplicate(&ssh, container.as_ref()) {
            return Err(WorkspaceError::CanonicalDuplicate {
                existing_workspace_id: dup.workspace_id,
                existing_name: dup.name.clone(),
            });
        }
        let now = now_rfc3339();
        let mut profile = WorkspaceProfile::default_local();
        profile.auto_launch_claude = auto_launch_claude;
        let record = WorkspaceRecord {
            workspace_id: Uuid::new_v4(),
            name: name.to_string(),
            location: WorkspaceLocation::Remote { ssh, container },
            // Profile per workspace; auto_launch_claude is the
            // caller-supplied flag.
            profile,
            created_at: now.clone(),
            last_used_at: now,
            open_tab_id: None,
            conversation_rounds_count: 0,
        };
        guard.records.insert(record.workspace_id, record.clone());
        Self::persist(&guard)?;
        Ok(record)
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

    /// Permanently remove a workspace record. Fails when the workspace
    /// is still open (has an active tab) to prevent accidental deletion
    /// of a session the user is working in.
    pub fn delete_workspace(&self, workspace_id: Uuid) -> Result<(), WorkspaceError> {
        let mut guard = self.inner.lock().expect("WorkspaceRegistry poisoned");
        let record = guard.records.get(&workspace_id).cloned().ok_or_else(|| {
            WorkspaceError::NotFound {
                workspace_id: workspace_id.to_string(),
            }
        })?;
        if record.open_tab_id.is_some() {
            return Err(WorkspaceError::AlreadyOpen {
                existing_tab_id: record.open_tab_id.unwrap_or_default(),
            });
        }
        guard.records.remove(&workspace_id);
        Self::persist(&guard)
    }

    /// Mutate the workspace's `profile` in-place and persist.
    /// The closure receives a mutable reference to the profile;
    /// whatever it sets is written to disk atomically.
    pub fn update_profile(
        &self,
        workspace_id: Uuid,
        f: impl FnOnce(&mut WorkspaceProfile),
    ) -> Result<(), WorkspaceError> {
        let mut guard = self.inner.lock().expect("WorkspaceRegistry poisoned");
        let mut record = guard.records.get(&workspace_id).cloned().ok_or_else(|| {
            WorkspaceError::NotFound {
                workspace_id: workspace_id.to_string(),
            }
        })?;
        f(&mut record.profile);
        guard.records.insert(workspace_id, record);
        Self::persist(&guard)
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
    ///
    /// Scoped to `WorkspaceLocation::Local` records only — Remote
    /// workspaces don't share the local inode/dev namespace, so a
    /// nominal-path collision between a Local and a Remote record is
    /// not a real duplicate.
    fn find_duplicate(&self, candidate: &Path) -> Option<&WorkspaceRecord> {
        self.records.values().find(|r| match &r.location {
            WorkspaceLocation::Local { path } => {
                paths_refer_to_same_workspace(path, candidate)
            }
            WorkspaceLocation::Remote { .. } => false,
        })
    }

    /// Locate an existing Remote workspace whose identity tuple
    /// matches the candidate. The tuple follows
    /// `docs/specs/transport.md` §6.1: `(user, host, normalized
    /// port, canonical_remote_path, container_id, cwd_in_container)`.
    /// `port = None` normalizes to `22`; `user = None` normalizes
    /// to `None` (it does NOT default to the local username for
    /// dedup purposes — two records with different `user` fields
    /// are different identities even if SSH would resolve them to
    /// the same login). A Remote-with-container and a
    /// Remote-without-container at the same SSH location are NOT
    /// duplicates (legitimately distinct workspaces — one is the
    /// host filesystem, the other is the container).
    fn find_remote_duplicate(
        &self,
        ssh: &SshLocation,
        container: Option<&ContainerLocation>,
    ) -> Option<&WorkspaceRecord> {
        let normalized_port = ssh.port.unwrap_or(22);
        self.records.values().find(|r| match &r.location {
            WorkspaceLocation::Local { .. } => false,
            WorkspaceLocation::Remote {
                ssh: existing_ssh,
                container: existing_container,
            } => {
                existing_ssh.user == ssh.user
                    && existing_ssh.host == ssh.host
                    && existing_ssh.port.unwrap_or(22) == normalized_port
                    && existing_ssh.canonical_remote_path == ssh.canonical_remote_path
                    && existing_container.as_ref() == container
            }
        })
    }
}

/// Per-field validation for the Remote workspace registration
/// surface for the Remote workspace registration. Fields rejected here surface as
/// `WorkspaceError::RemoteFieldInvalid` with a `field` discriminator
/// the frontend uses to highlight the offending input.
/// Validate that a remote workspace's `canonical_remote_path` is
/// a strictly canonical POSIX absolute path. Duplicate detection
/// (`find_remote_duplicate`) compares this string verbatim as the
/// workspace's identity, so the surface MUST be canonical — any
/// non-canonical alias would let the same remote directory be
/// registered multiple times under different textual paths.
///
/// Rules (string-level; no remote filesystem access):
/// - Must not be empty / whitespace-only.
/// - Must be absolute (start with `/`).
/// - Must not contain `.` or `..` as a path segment.
/// - Must not contain empty segments (`//`) or a trailing `/` on
///   a non-root path. The root path `/` is accepted as-is.
///
/// Active canonicalization against the remote shell (e.g.,
/// `cd "$p" && pwd -P`) is a future enhancement — string
/// validation closes the duplicate-detection gap without an extra
/// network round-trip.
pub fn validate_canonical_remote_path(path: &str) -> Result<(), WorkspaceError> {
    if path.trim().is_empty() {
        return Err(WorkspaceError::RemoteFieldInvalid {
            field: "canonicalRemotePath".into(),
            reason: "canonical remote path must not be empty".into(),
        });
    }
    if !path.starts_with('/') {
        return Err(WorkspaceError::RemoteFieldInvalid {
            field: "canonicalRemotePath".into(),
            reason: "canonical remote path must be an absolute POSIX path (start with `/`)".into(),
        });
    }
    if path == "/" {
        return Ok(());
    }
    // Non-root: split on `/`. The leading `/` produces an empty
    // first segment which we skip; every other segment must be
    // non-empty AND must not be `.` or `..`. Because `path != "/"`
    // and `path.starts_with('/')`, at least one further segment
    // exists, so the loop runs at least once.
    let mut iter = path.split('/');
    // First segment from the leading `/` is always empty by
    // construction; consume it.
    let _ = iter.next();
    for segment in iter {
        if segment.is_empty() {
            return Err(WorkspaceError::RemoteFieldInvalid {
                field: "canonicalRemotePath".into(),
                reason: "canonical remote path must not contain `//` or a trailing `/` on a non-root path".into(),
            });
        }
        if segment == "." || segment == ".." {
            return Err(WorkspaceError::RemoteFieldInvalid {
                field: "canonicalRemotePath".into(),
                reason: "canonical remote path must not contain `.` or `..` segments".into(),
            });
        }
    }
    Ok(())
}

pub fn validate_remote_fields(
    ssh: &SshLocation,
    container: Option<&ContainerLocation>,
) -> Result<(), WorkspaceError> {
    if ssh.host.trim().is_empty() {
        return Err(WorkspaceError::RemoteFieldInvalid {
            field: "host".into(),
            reason: "host must not be empty".into(),
        });
    }
    validate_canonical_remote_path(&ssh.canonical_remote_path)?;
    if let Some(port) = ssh.port
        && port == 0
    {
        return Err(WorkspaceError::RemoteFieldInvalid {
            field: "port".into(),
            reason: "port must be 1..=65535".into(),
        });
    }
    if let Some(c) = container
        && c.container_id.trim().is_empty()
    {
        return Err(WorkspaceError::RemoteFieldInvalid {
            field: "containerId".into(),
            reason: "container id must not be empty when container mode is enabled".into(),
        });
    }
    Ok(())
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
    auto_launch_claude: bool,
    registry: State<'_, WorkspaceRegistry>,
) -> Result<WorkspaceRecord, WorkspaceErrorDto> {
    registry
        .create_workspace(&name, auto_launch_claude)
        .map_err(|e| WorkspaceErrorDto::from(&e))
}

#[tauri::command]
pub fn register_workspace(
    path: String,
    auto_launch_claude: bool,
    registry: State<'_, WorkspaceRegistry>,
) -> Result<WorkspaceRecord, WorkspaceErrorDto> {
    registry
        .register_workspace(Path::new(&path), auto_launch_claude)
        .map_err(|e| WorkspaceErrorDto::from(&e))
}

/// Testable seam: run the preflight probe against `transport` and
/// — only on `Ok(())` — persist the record into `registry`. The
/// Tauri command builds the transport from app data and forwards
/// to this helper. Stub-transport tests bypass the Tauri layer
/// entirely and assert that a `probe` failure leaves the registry
/// on disk byte-identical to the pre-call state.
pub fn try_register_remote_workspace_with_probe(
    name: &str,
    ssh: SshLocation,
    container: Option<ContainerLocation>,
    auto_launch_claude: bool,
    registry: &WorkspaceRegistry,
    transport: &dyn terminal_mesh_core::transport::Transport,
    phase_label: &str,
) -> Result<WorkspaceRecord, WorkspaceErrorDto> {
    validate_workspace_name(name).map_err(|e| WorkspaceErrorDto::from(&e))?;
    validate_remote_fields(&ssh, container.as_ref())
        .map_err(|e| WorkspaceErrorDto::from(&e))?;
    // Fast-reject identity-tuple duplicates BEFORE running the
    // network probe. Without this ordering, registering an
    // already-known workspace whose endpoint is unreachable or
    // slow would surface as a 5-10s `RemoteProbeFailed` instead
    // of an immediate `CanonicalDuplicate` — and would waste a
    // network round-trip for a record that cannot be added
    // anyway. The downstream `register_remote_workspace` keeps
    // the same check at insert time for defense in depth.
    if let Some(dup) = registry.find_remote_duplicate(&ssh, container.as_ref()) {
        return Err(WorkspaceErrorDto::CanonicalDuplicate {
            existing_workspace_id: dup.workspace_id.to_string(),
            existing_name: dup.name.clone(),
        });
    }
    let probe_location = WorkspaceLocation::Remote {
        ssh: ssh.clone(),
        container: container.clone(),
    };
    let core_location = probe_location.to_core_workspace_location();
    transport
        .probe(core_location)
        .map_err(|e| WorkspaceErrorDto::RemoteProbeFailed {
            phase: phase_label.into(),
            reason: e.to_string(),
        })?;
    registry
        .register_remote_workspace(name, ssh, container, auto_launch_claude)
        .map_err(|e| WorkspaceErrorDto::from(&e))
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn register_remote_workspace(
    app: AppHandle,
    name: String,
    host: String,
    user: Option<String>,
    port: Option<u16>,
    canonical_remote_path: String,
    container_id: Option<String>,
    cwd_in_container: Option<String>,
    auto_launch_claude: bool,
    registry: State<'_, WorkspaceRegistry>,
) -> Result<WorkspaceRecord, WorkspaceErrorDto> {
    let ssh = SshLocation {
        user,
        host,
        port,
        canonical_remote_path,
    };
    let container = container_id.map(|container_id| ContainerLocation {
        container_id,
        cwd_in_container,
    });
    // Construct the transport implied by the form fields. The
    // preflight probe runs on a blocking thread pool because the
    // SSH spawn path is sync underneath; the registry insertion
    // stays on the current async context.
    let probe_location = WorkspaceLocation::Remote {
        ssh: ssh.clone(),
        container: container.clone(),
    };
    let app_data_root = app
        .try_state::<crate::orchestrator::OrchestratorBootstrap>()
        .map(|b| b.app_data_root.clone())
        .ok_or_else(|| WorkspaceErrorDto::Io {
            context: "register_remote_workspace".into(),
            message: "OrchestratorBootstrap app_data_root unavailable".into(),
        })?;
    let routing = crate::workspace_launch_scheduler::select_transport_kind_for(
        &probe_location,
    );
    let (transport, phase_label): (
        std::sync::Arc<dyn terminal_mesh_core::transport::Transport>,
        &'static str,
    ) = {
        use crate::workspace_launch_scheduler::TransportRouting;
        match routing {
            TransportRouting::Ssh => (
                std::sync::Arc::new(
                    terminal_mesh_core::SshTransport::from_app_data(
                        std::path::PathBuf::from("/usr/bin/ssh"),
                        &app_data_root,
                    )
                    .map_err(|e| WorkspaceErrorDto::Io {
                        context: "SshTransport::from_app_data".into(),
                        message: e.to_string(),
                    })?,
                ),
                "ssh",
            ),
            TransportRouting::DockerOverSsh => (
                std::sync::Arc::new(
                    terminal_mesh_core::DockerOverSshTransport::from_app_data(
                        std::path::PathBuf::from("/usr/bin/ssh"),
                        &app_data_root,
                    )
                    .map_err(|e| WorkspaceErrorDto::Io {
                        context: "DockerOverSshTransport::from_app_data".into(),
                        message: e.to_string(),
                    })?,
                ),
                "docker",
            ),
            TransportRouting::Local => {
                return Err(WorkspaceErrorDto::Io {
                    context: "register_remote_workspace".into(),
                    message: "Remote workspace registration routed to Local transport".into(),
                });
            }
        }
    };

    let registry_for_blocking: WorkspaceRegistry = (*registry).clone();
    tokio::task::spawn_blocking(move || {
        try_register_remote_workspace_with_probe(
            &name,
            ssh,
            container,
            auto_launch_claude,
            &registry_for_blocking,
            &*transport,
            phase_label,
        )
    })
    .await
    .map_err(|join_err| WorkspaceErrorDto::Io {
        context: "register_remote_workspace::probe".into(),
        message: format!("probe join error: {join_err}"),
    })?
}

#[tauri::command]
pub fn open_workspace(
    workspace_id: String,
    tab_id: String,
    registry: State<'_, WorkspaceRegistry>,
    terminal_registry: State<'_, crate::terminal_mesh::TerminalMeshRegistry>,
) -> Result<WorkspaceRecord, WorkspaceErrorDto> {
    let id = parse_workspace_id(&workspace_id)?;
    let record = registry
        .open_workspace(id, &tab_id)
        .map_err(|e| WorkspaceErrorDto::from(&e))?;
    // Clear any stale close-during-launch tombstone the previous
    // close may have left behind. If the user closes a workspace
    // whose auto-launch is `Launching` and reopens the same
    // workspace before that original launch settles, the
    // executor's `Ok` branch would otherwise consume the
    // tombstone and immediately shut down the newly-spawned
    // terminal — leaving the reopened tab parked forever. The
    // reopen is the user-visible signal that any prior
    // close-during-launch is no longer relevant; discard the
    // tombstone here so the executor's post-spawn check is a
    // no-op for this id.
    let _ = terminal_registry.take_workspace_closed_during_launch(id);
    Ok(record)
}

#[tauri::command]
pub async fn close_workspace(
    workspace_id: String,
    registry: State<'_, WorkspaceRegistry>,
    scheduler: State<'_, crate::workspace_launch_scheduler::WorkspaceLaunchScheduler>,
    terminal_registry: State<'_, crate::terminal_mesh::TerminalMeshRegistry>,
) -> Result<(), WorkspaceErrorDto> {
    let id = parse_workspace_id(&workspace_id)?;
    // Capture the scheduler state BEFORE the cancel attempt so we
    // can distinguish "in flight, cannot interrupt → need a
    // tombstone for the executor's post-spawn reap" from "settled
    // or never enqueued → no executor will spawn, no tombstone
    // needed." `cancel` returns false for both `Launching` AND
    // `NotPresent`; using `!was_pending` as the gate over-marks
    // the tombstone for normal closes of long-running tabs,
    // which the next auto-launch for the same workspace would
    // then consume and immediately shut down. Reading `state`
    // first lets us mark only when the workspace is actually
    // `Launching`.
    let pre_cancel_state = scheduler.state(id);
    let _was_pending = scheduler.cancel(id);
    if crate::workspace_launch_scheduler::should_mark_close_during_launch(pre_cancel_state) {
        terminal_registry.mark_workspace_closed_during_launch(id);
    }
    // Pull the workspace's tab_id (if any) and reap the scheduler-
    // owned terminal + clear the pending-launch placeholder.
    // Returns the terminal id (if any) that was reaped + had its
    // actor command tx claimed — the caller fires the actor
    // `Shutdown` outside the helper so the helper itself stays
    // synchronous and unit-testable without a tokio runtime.
    if let Some(rec) = registry.find_by_id(id) {
        if let Some(t) = rec.open_tab_id.as_deref() {
            if let Some((terminal_id, tx)) =
                reap_scheduler_owned_terminal_for_tab(&terminal_registry, t)
            {
                if let Some(tx) = tx {
                    tokio::spawn(async move {
                        let _ = tx
                            .send(terminal_mesh_core::ActorCommand::Shutdown)
                            .await;
                    });
                }
                tracing::debug!(
                    %id,
                    tab_id = %t,
                    %terminal_id,
                    "close_workspace: reaped scheduler-owned terminal by tab id"
                );
            }
        }
    }
    // Always reset stash flag on close so the workspace can be
    // re-opened as a normal tab if the user opens it again later.
    let _ = registry.update_profile(id, |p| p.stashed = false);
    registry
        .close_workspace(id)
        .map_err(|e| WorkspaceErrorDto::from(&e))
}

/// Remove a workspace from the registry permanently. Fails when the
/// workspace is still open (has an active tab) — the caller must
/// close it first.
#[tauri::command]
pub async fn delete_workspace(
    workspace_id: String,
    registry: State<'_, WorkspaceRegistry>,
) -> Result<(), WorkspaceErrorDto> {
    let id = parse_workspace_id(&workspace_id)?;
    registry.delete_workspace(id)
        .map_err(|e| WorkspaceErrorDto::from(&e))
}

#[tauri::command]
pub async fn stash_workspace(
    workspace_id: String,
    registry: State<'_, WorkspaceRegistry>,
) -> Result<(), WorkspaceErrorDto> {
    let id = parse_workspace_id(&workspace_id)?;
    registry.update_profile(id, |p| p.stashed = true)
        .map_err(|e| WorkspaceErrorDto::from(&e))
}

#[tauri::command]
pub async fn unstash_workspace(
    workspace_id: String,
    registry: State<'_, WorkspaceRegistry>,
) -> Result<(), WorkspaceErrorDto> {
    let id = parse_workspace_id(&workspace_id)?;
    registry.update_profile(id, |p| p.stashed = false)
        .map_err(|e| WorkspaceErrorDto::from(&e))
}

/// Synchronous reap of the scheduler-owned terminal bound to a
/// tab id, called from `close_workspace`. Clears any pending-launch
/// placeholder; if a real terminal is registered under the tab id,
/// claims its actor command tx (so the caller can fire `Shutdown`
/// outside the helper) and drops it from the registry so the
/// retained snapshot does not outlive the tab.
///
/// Returns `Some((terminal_id, command_tx))` when a terminal was
/// found; `None` when only a placeholder existed (still cleared) or
/// no entry existed at all. The placeholder-only case avoids the
/// race window where the user closes a workspace whose auto-launch
/// has settled the spawn AND registered the terminal but whose
/// real-id event hasn't reached the frontend yet — the frontend's
/// `closeTab` would otherwise lookup `terminalIdByTabId[tabId]`,
/// get null, and skip the shutdown.
pub fn reap_scheduler_owned_terminal_for_tab(
    terminal_registry: &crate::terminal_mesh::TerminalMeshRegistry,
    tab_id: &str,
) -> Option<(
    Uuid,
    Option<tokio::sync::mpsc::Sender<terminal_mesh_core::ActorCommand>>,
)> {
    let _ = terminal_registry.clear_pending_for_tab(tab_id);
    let terminal_id = terminal_registry.lookup_terminal_by_tab(tab_id)?;
    let tx = terminal_registry.lookup_command_tx(terminal_id);
    terminal_registry.forget(terminal_id);
    Some((terminal_id, tx))
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
            location: WorkspaceLocation::Local {
                path: std::path::PathBuf::from("/tmp/test-workspace"),
            },
            profile: WorkspaceProfile::default_local(),
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

    fn assert_canon_remote_path_invalid(path: &str, hint: &str) {
        match validate_canonical_remote_path(path) {
            Err(WorkspaceError::RemoteFieldInvalid { field, reason }) => {
                assert_eq!(field, "canonicalRemotePath", "field tag for `{path}`");
                assert!(
                    reason.contains(hint),
                    "reason `{reason}` for `{path}` should mention `{hint}`"
                );
            }
            other => panic!("expected RemoteFieldInvalid for `{path}`; got {other:?}"),
        }
    }

    #[test]
    fn validate_canonical_remote_path_accepts_simple_absolute_paths() {
        validate_canonical_remote_path("/").expect("root accepted");
        validate_canonical_remote_path("/home/user/repo").expect("nested absolute");
        validate_canonical_remote_path("/srv/work").expect("simple absolute");
        validate_canonical_remote_path("/tmp").expect("short absolute");
        validate_canonical_remote_path("/a")
            .expect("single-char-segment accepted");
        validate_canonical_remote_path("/with spaces/ok")
            .expect("spaces inside segments are not canonicalization concerns");
    }

    #[test]
    fn validate_canonical_remote_path_rejects_empty_or_whitespace() {
        assert_canon_remote_path_invalid("", "empty");
        assert_canon_remote_path_invalid("   ", "empty");
    }

    #[test]
    fn validate_canonical_remote_path_rejects_relative_paths() {
        assert_canon_remote_path_invalid("repo", "absolute");
        assert_canon_remote_path_invalid("./repo", "absolute");
        assert_canon_remote_path_invalid("../parent", "absolute");
        assert_canon_remote_path_invalid("~/home", "absolute");
        assert_canon_remote_path_invalid("home/user", "absolute");
    }

    #[test]
    fn validate_canonical_remote_path_rejects_dot_segments() {
        assert_canon_remote_path_invalid("/foo/./bar", "`.` or `..`");
        assert_canon_remote_path_invalid("/foo/../bar", "`.` or `..`");
        assert_canon_remote_path_invalid("/./bar", "`.` or `..`");
        assert_canon_remote_path_invalid("/foo/..", "`.` or `..`");
        assert_canon_remote_path_invalid("/foo/.", "`.` or `..`");
    }

    #[test]
    fn validate_canonical_remote_path_rejects_empty_segments_and_trailing_slash() {
        assert_canon_remote_path_invalid("//foo", "`//`");
        assert_canon_remote_path_invalid("/foo//bar", "`//`");
        assert_canon_remote_path_invalid("/foo/bar//", "`//`");
        assert_canon_remote_path_invalid("/foo/", "`//`");
        assert_canon_remote_path_invalid("/foo/bar/", "`//`");
    }

    /// `validate_remote_fields` delegates to the canonical-path
    /// validator so a relative path on the SSH location is
    /// rejected pre-persist with the same typed error.
    #[test]
    fn validate_remote_fields_rejects_relative_canonical_remote_path() {
        let ssh = SshLocation {
            user: None,
            host: "example.com".into(),
            port: None,
            canonical_remote_path: "repo".into(),
        };
        let err = validate_remote_fields(&ssh, None).unwrap_err();
        match err {
            WorkspaceError::RemoteFieldInvalid { field, .. } => {
                assert_eq!(field, "canonicalRemotePath");
            }
            other => panic!("expected RemoteFieldInvalid; got {other:?}"),
        }
    }

    #[test]
    fn create_workspace_creates_dir_under_workspaces_root() {
        let (_storage, home, reg) = fresh_registry();
        let rec = reg.create_workspace("demo", true).expect("create");
        let target = home.path().join("AgentPlatform").join("workspaces").join("demo");
        assert!(target.is_dir());
        let canonical = std::fs::canonicalize(&target).unwrap();
        assert_eq!(rec.local_path(), Some(canonical.as_path()));
        assert_eq!(rec.name, "demo");
        assert_eq!(rec.open_tab_id, None);
        assert!(!target.join(".git").exists(), "no auto-git-init per spec");
        assert_eq!(rec.profile, WorkspaceProfile::default_local());
    }

    #[test]
    fn create_workspace_rejects_duplicate_name_via_already_exists() {
        let (_storage, _home, reg) = fresh_registry();
        reg.create_workspace("twin", true).expect("first");
        let err = reg.create_workspace("twin", true).unwrap_err();
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
        let rec = reg.register_workspace(&dir, true).expect("register");
        assert_eq!(
            rec.local_path(),
            Some(std::fs::canonicalize(&dir).unwrap().as_path())
        );
        // Verify the file is byte-identical (no copy/modification).
        let body = std::fs::read(&canary).unwrap();
        assert_eq!(body, b"untouched");
    }

    #[test]
    fn register_workspace_rejects_non_directory_or_missing_path() {
        let (_storage, home, reg) = fresh_registry();
        let missing = home.path().join("does-not-exist");
        match reg.register_workspace(&missing, true).unwrap_err() {
            WorkspaceError::NotADirectory { .. } => {}
            other => panic!("expected NotADirectory for missing; got {other:?}"),
        }
        let file = home.path().join("a-file.txt");
        std::fs::write(&file, b"x").unwrap();
        match reg.register_workspace(&file, true).unwrap_err() {
            WorkspaceError::NotADirectory { .. } => {}
            other => panic!("expected NotADirectory for file; got {other:?}"),
        }
    }

    #[test]
    fn canonical_duplicate_rejected_for_register_after_create() {
        let (_storage, _home, reg) = fresh_registry();
        let created = reg.create_workspace("alpha", true).expect("create");
        // Try to register the same canonical path again.
        let created_path = created
            .local_path()
            .expect("local workspace has path")
            .to_path_buf();
        let err = reg.register_workspace(&created_path, true).unwrap_err();
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
        reg.create_workspace("one", true).unwrap();
        reg.create_workspace("two", true).unwrap();
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
        let rec = reg.create_workspace("ws", true).unwrap();
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
        let rec = reg.create_workspace("ws", true).unwrap();
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
        let rec = reg.create_workspace("ws", true).unwrap();
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
        let rec = reg.create_workspace("ws", true).unwrap();
        assert!(reg.resolve_for_tab("tab-1").is_none());
        reg.open_workspace(rec.workspace_id, "tab-1").unwrap();
        let r = reg.resolve_for_tab("tab-1").expect("resolved");
        assert_eq!(r.workspace_id, rec.workspace_id);
    }

    /// Reap a settled scheduler-owned terminal by tab id. This
    /// closes the close-during-id-resolution race: when the
    /// executor's Ok branch has registered the terminal under the
    /// tab id but the frontend lifecycle hook hasn't populated
    /// `terminalIdByTabId[tabId]` yet, the frontend's `closeTab`
    /// path can't issue the shutdown — the backend must take over.
    #[test]
    fn reap_scheduler_owned_terminal_drops_registry_entry_and_claims_tx() {
        use crate::terminal_mesh::TerminalMeshRegistry;
        let term_registry = TerminalMeshRegistry::new();
        let terminal_id = Uuid::new_v4();
        let tab_id = Uuid::new_v4().to_string();
        let (tx, _rx) =
            tokio::sync::mpsc::channel::<terminal_mesh_core::ActorCommand>(8);
        let buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        term_registry.record(
            terminal_id,
            tx,
            buf,
            Some(tab_id.clone()),
            crate::workspace_lifecycle::TabKind::Workspace,
            None,
            crate::workspace_lifecycle::TransportKind::Local,
        );
        // Sanity: terminal is registered under the tab id.
        assert_eq!(
            term_registry.lookup_terminal_by_tab(&tab_id),
            Some(terminal_id)
        );
        let result = reap_scheduler_owned_terminal_for_tab(&term_registry, &tab_id);
        let (reaped_id, reaped_tx) = result.expect("terminal found and reaped");
        assert_eq!(reaped_id, terminal_id);
        assert!(reaped_tx.is_some(), "actor command tx returned for shutdown");
        // After reap: registry no longer resolves the tab to a
        // terminal — defense in depth so the next operation sees
        // a clean slate.
        assert_eq!(
            term_registry.lookup_terminal_by_tab(&tab_id),
            None,
            "terminal forgotten after reap"
        );
    }

    /// When no terminal is registered under the tab id (e.g. the
    /// user closes a workspace that never spawned anything, or
    /// the only state is a pending placeholder), the helper
    /// returns None — the placeholder is still cleared as a side
    /// effect.
    #[test]
    fn reap_scheduler_owned_terminal_returns_none_when_no_terminal_registered() {
        use crate::terminal_mesh::TerminalMeshRegistry;
        let term_registry = TerminalMeshRegistry::new();
        let tab_id = Uuid::new_v4().to_string();
        let result = reap_scheduler_owned_terminal_for_tab(&term_registry, &tab_id);
        assert!(result.is_none());
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
        let created = reg.register_workspace(&real, true).expect("register real");

        let alias = home.path().join("alias-link");
        symlink(&real, &alias).unwrap();
        let err = reg.register_workspace(&alias, true).unwrap_err();
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

        let created = reg.register_workspace(&mixed, true).expect("register mixed");

        if case_insensitive {
            let err = reg.register_workspace(&lower, true).unwrap_err();
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
        let created = reg.create_workspace("restart-demo", true).expect("create");
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
        let _ = reg.create_workspace("snake", true).expect("create");
        let body = std::fs::read_to_string(storage.path().join("workspaces.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let workspaces = v
            .get("workspaces")
            .and_then(|x| x.as_array())
            .expect("workspaces array");
        let entry = workspaces.first().expect("at least one record");
        // snake_case keys required on disk for the current shape:
        for k in [
            "workspace_id",
            "name",
            "location",
            "profile",
            "created_at",
            "last_used_at",
            "open_tab_id",
        ] {
            assert!(
                entry.get(k).is_some(),
                "on-disk key `{k}` missing from {entry:?}"
            );
        }
        // Legacy flat `path` field must not be written by current code:
        assert!(
            entry.get("path").is_none(),
            "legacy flat `path` key leaked to disk in {entry:?}"
        );
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
        // Location is tagged + nested correctly:
        let loc = entry.get("location").expect("location present");
        assert_eq!(
            loc.get("kind").and_then(|x| x.as_str()),
            Some("local"),
            "default create yields Local location; got {loc:?}"
        );
        assert!(
            loc.get("path").is_some(),
            "Local location must carry `path`; got {loc:?}"
        );
        // Profile uses camelCase field names per #[serde(rename_all)]:
        let prof = entry.get("profile").expect("profile present");
        assert_eq!(
            prof.get("autoLaunchClaude").and_then(|x| x.as_bool()),
            Some(true),
            "default profile auto-launch=true; got {prof:?}"
        );
        let argv = prof
            .get("claudeArgv")
            .and_then(|x| x.as_array())
            .expect("claudeArgv array");
        assert_eq!(argv.len(), 1);
        assert_eq!(argv[0].as_str(), Some("--dangerously-skip-permissions"));
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
        let created = reg.create_workspace("counted", true).expect("create");
        // Seed two .jsonl files under the auto-created workspace.
        let claude = created
            .local_path()
            .expect("local workspace has path")
            .join(".claude/projects/counted");
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
        let a = reg.create_workspace("a", true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let b = reg.create_workspace("b", true).unwrap();
        // `b` was created later → should be first when sorted desc by
        // last_used_at. The RFC3339 helper has second resolution, so
        // we slept >1s to guarantee a different timestamp.
        let listed = reg.list();
        assert_eq!(listed[0].workspace_id, b.workspace_id);
        assert_eq!(listed[1].workspace_id, a.workspace_id);
    }

    #[test]
    fn workspace_profile_default_local_matches_spec() {
        let p = WorkspaceProfile::default_local();
        assert!(p.auto_launch_claude);
        assert_eq!(p.claude_argv, vec!["--dangerously-skip-permissions"]);
    }

    /// Legacy on-disk records (only the flat `path` field) must
    /// deserialize as `Local + default profile`. Verified by handing a
    /// raw JSON blob to the registry loader and round-tripping through
    /// `list()`.
    #[test]
    fn legacy_stored_record_without_location_migrates_to_local_with_default_profile() {
        let storage = tempfile::TempDir::new().unwrap();
        let workspaces_root = tempfile::TempDir::new().unwrap();
        let id = Uuid::new_v4();
        let blob = serde_json::json!({
            "workspaces": [{
                "workspace_id": id.to_string(),
                "name": "legacy-ws",
                "path": "/tmp/some-legacy-path",
                "created_at": "2025-01-01T00:00:00Z",
                "last_used_at": "2025-01-01T00:00:00Z",
                "open_tab_id": null
            }]
        });
        std::fs::write(
            storage.path().join("workspaces.json"),
            serde_json::to_vec_pretty(&blob).unwrap(),
        )
        .unwrap();

        let reg = WorkspaceRegistry::load(
            storage.path().to_path_buf(),
            Some(workspaces_root.path().to_path_buf()),
        );
        let listed = reg.list();
        assert_eq!(listed.len(), 1);
        let rec = &listed[0];
        assert_eq!(rec.workspace_id, id);
        assert_eq!(rec.name, "legacy-ws");
        assert_eq!(
            rec.local_path(),
            Some(std::path::Path::new("/tmp/some-legacy-path"))
        );
        assert_eq!(rec.profile, WorkspaceProfile::default_local());
    }

    /// A record carrying `location` + `profile` on disk should round-
    /// trip through Stored serialize → deserialize without loss.
    #[test]
    fn stored_record_with_location_round_trips() {
        let original = WorkspaceRecord {
            workspace_id: Uuid::new_v4(),
            name: "remote-demo".into(),
            location: WorkspaceLocation::Remote {
                ssh: SshLocation {
                    user: Some("alice".into()),
                    host: "host.example".into(),
                    port: Some(2222),
                    canonical_remote_path: "/home/alice/work".into(),
                },
                container: Some(ContainerLocation {
                    container_id: "ctr-7".into(),
                    cwd_in_container: Some("/app".into()),
                }),
            },
            profile: WorkspaceProfile {
                auto_launch_claude: false,
                claude_argv: vec!["--print".into(), "hello".into()],
            },
            created_at: "2026-01-01T00:00:00Z".into(),
            last_used_at: "2026-01-02T00:00:00Z".into(),
            open_tab_id: None,
            conversation_rounds_count: 0,
        };
        let stored: StoredWorkspaceRecord = (&original).into();
        let json = serde_json::to_string(&stored).unwrap();
        let parsed: StoredWorkspaceRecord = serde_json::from_str(&json).unwrap();
        let restored: WorkspaceRecord = parsed.try_into().expect("migrate ok");
        assert_eq!(restored.workspace_id, original.workspace_id);
        assert_eq!(restored.name, original.name);
        assert_eq!(restored.location, original.location);
        assert_eq!(restored.profile, original.profile);
        assert_eq!(restored.local_path(), None, "Remote has no local_path");
    }

    /// When both `location` and legacy `path` are present in a stored
    /// blob, `location` wins. (Forward-only migration: new writes
    /// drop `path`, so this only matters if a hand-edited file ends
    /// up with both.)
    #[test]
    fn stored_record_location_takes_precedence_over_legacy_path() {
        let id = Uuid::new_v4();
        let blob = serde_json::json!({
            "workspace_id": id.to_string(),
            "name": "both",
            "path": "/tmp/legacy",
            "location": {"kind": "local", "path": "/tmp/new"},
            "created_at": "2025-01-01T00:00:00Z",
            "last_used_at": "2025-01-01T00:00:00Z",
            "open_tab_id": null
        });
        let stored: StoredWorkspaceRecord = serde_json::from_value(blob).unwrap();
        let rec: WorkspaceRecord = stored.try_into().expect("migrate ok");
        assert_eq!(rec.local_path(), Some(std::path::Path::new("/tmp/new")));
    }

    /// A corrupted record with neither `location` nor legacy `path`
    /// must fail migration; the loader logs + drops it rather than
    /// inserting a malformed record.
    #[test]
    fn stored_record_without_location_or_path_fails_migration() {
        let id = Uuid::new_v4();
        let blob = serde_json::json!({
            "workspace_id": id.to_string(),
            "name": "corrupt",
            "created_at": "2025-01-01T00:00:00Z",
            "last_used_at": "2025-01-01T00:00:00Z",
            "open_tab_id": null
        });
        let stored: StoredWorkspaceRecord = serde_json::from_value(blob).unwrap();
        let err = WorkspaceRecord::try_from(stored).unwrap_err();
        assert!(
            matches!(
                err,
                StoredRecordMigrationError::MissingLocation { workspace_id } if workspace_id == id
            ),
            "expected MissingLocation; got {err:?}"
        );
    }

    /// Pins the spec invariant that every persisted Remote record
    /// carries a `canonical_remote_path` — `docs/specs/transport.md`
    /// §6.1 lists it as required and §6.2 uses it in the remote
    /// duplicate-identity tuple. A stored Remote record missing the
    /// field must fail to deserialize so a corrupt or hand-edited
    /// file does not produce an identity-less Remote record.
    #[test]
    fn stored_record_remote_missing_canonical_path_fails_deserialization() {
        let blob = serde_json::json!({
            "workspace_id": Uuid::new_v4().to_string(),
            "name": "remote-no-path",
            "location": {
                "kind": "remote",
                "ssh": {
                    "user": "alice",
                    "host": "host.example",
                    "port": 22
                },
                "container": null
            },
            "profile": {
                "autoLaunchClaude": false,
                "claudeArgv": []
            },
            "created_at": "2026-01-01T00:00:00Z",
            "last_used_at": "2026-01-01T00:00:00Z",
            "open_tab_id": null
        });
        let err = serde_json::from_value::<StoredWorkspaceRecord>(blob)
            .expect_err("Remote record without canonical_remote_path must fail to deserialize");
        let msg = err.to_string();
        assert!(
            msg.contains("canonicalRemotePath")
                || msg.contains("canonical_remote_path"),
            "error should mention the missing field; got: {msg}"
        );
    }

    /// `create_workspace(name, false)` must persist
    /// `WorkspaceProfile.auto_launch_claude = false` so a reload sees
    /// the opt-out state. Pins the create-form checkbox plumbing.
    #[test]
    fn create_workspace_persists_auto_launch_false_when_opted_out() {
        let (storage, home, reg) = fresh_registry();
        let workspaces_root = home.path().join("AgentPlatform").join("workspaces");
        let rec = reg
            .create_workspace("opt-out", false)
            .expect("create with opt-out");
        assert!(!rec.profile.auto_launch_claude, "in-memory record");
        drop(reg);
        let reloaded = WorkspaceRegistry::load(
            storage.path().to_path_buf(),
            Some(workspaces_root),
        );
        let after = reloaded.find_by_id(rec.workspace_id).expect("present");
        assert!(
            !after.profile.auto_launch_claude,
            "auto_launch_claude=false must survive a registry reload"
        );
    }

    /// Symmetric: `register_workspace(path, true)` preserves the
    /// checked-by-default behavior.
    #[test]
    fn register_workspace_persists_auto_launch_true_by_default() {
        let (storage, home, reg) = fresh_registry();
        let dir = home.path().join("default-on");
        std::fs::create_dir_all(&dir).unwrap();
        let rec = reg
            .register_workspace(&dir, true)
            .expect("register opted in");
        assert!(rec.profile.auto_launch_claude);
        drop(reg);
        let reloaded = WorkspaceRegistry::load(
            storage.path().to_path_buf(),
            Some(home.path().join("AgentPlatform").join("workspaces")),
        );
        let after = reloaded.find_by_id(rec.workspace_id).expect("present");
        assert!(after.profile.auto_launch_claude);
    }

    /// `register_remote_workspace` persists a Remote record with the
    /// supplied SSH + container fields and the auto-launch flag,
    /// then a reload sees the same record.
    #[test]
    fn register_remote_workspace_persists_remote_record() {
        let (storage, home, reg) = fresh_registry();
        let workspaces_root = home.path().join("AgentPlatform").join("workspaces");
        let ssh = SshLocation {
            user: Some("alice".into()),
            host: "h.example".into(),
            port: Some(2222),
            canonical_remote_path: "/srv/work".into(),
        };
        let container = Some(ContainerLocation {
            container_id: "ctr-7".into(),
            cwd_in_container: Some("/app".into()),
        });
        let rec = reg
            .register_remote_workspace("remote-ws", ssh.clone(), container.clone(), false)
            .expect("register remote");
        assert!(matches!(rec.location, WorkspaceLocation::Remote { .. }));
        assert!(!rec.profile.auto_launch_claude);
        drop(reg);
        let reloaded = WorkspaceRegistry::load(
            storage.path().to_path_buf(),
            Some(workspaces_root),
        );
        let after = reloaded.find_by_id(rec.workspace_id).expect("present");
        match &after.location {
            WorkspaceLocation::Remote { ssh, container } => {
                assert_eq!(ssh.user.as_deref(), Some("alice"));
                assert_eq!(ssh.host, "h.example");
                assert_eq!(ssh.port, Some(2222));
                assert_eq!(ssh.canonical_remote_path, "/srv/work");
                let c = container.as_ref().expect("container present");
                assert_eq!(c.container_id, "ctr-7");
                assert_eq!(c.cwd_in_container.as_deref(), Some("/app"));
            }
            other => panic!("expected Remote location; got {other:?}"),
        }
    }

    #[test]
    fn register_remote_workspace_rejects_blank_host() {
        let (_storage, _home, reg) = fresh_registry();
        let err = reg
            .register_remote_workspace(
                "x",
                SshLocation {
                    user: None,
                    host: "   ".into(),
                    port: None,
                    canonical_remote_path: "/srv".into(),
                },
                None,
                true,
            )
            .unwrap_err();
        match err {
            WorkspaceError::RemoteFieldInvalid { field, .. } => assert_eq!(field, "host"),
            other => panic!("expected RemoteFieldInvalid(host); got {other:?}"),
        }
    }

    #[test]
    fn register_remote_workspace_rejects_blank_canonical_path() {
        let (_storage, _home, reg) = fresh_registry();
        let err = reg
            .register_remote_workspace(
                "x",
                SshLocation {
                    user: None,
                    host: "h".into(),
                    port: None,
                    canonical_remote_path: "".into(),
                },
                None,
                true,
            )
            .unwrap_err();
        match err {
            WorkspaceError::RemoteFieldInvalid { field, .. } => {
                assert_eq!(field, "canonicalRemotePath")
            }
            other => panic!("expected RemoteFieldInvalid(canonicalRemotePath); got {other:?}"),
        }
    }

    #[test]
    fn register_remote_workspace_rejects_port_zero() {
        let (_storage, _home, reg) = fresh_registry();
        let err = reg
            .register_remote_workspace(
                "x",
                SshLocation {
                    user: None,
                    host: "h".into(),
                    port: Some(0),
                    canonical_remote_path: "/srv".into(),
                },
                None,
                true,
            )
            .unwrap_err();
        match err {
            WorkspaceError::RemoteFieldInvalid { field, .. } => assert_eq!(field, "port"),
            other => panic!("expected RemoteFieldInvalid(port); got {other:?}"),
        }
    }

    #[test]
    fn register_remote_workspace_rejects_blank_container_id_when_container_set() {
        let (_storage, _home, reg) = fresh_registry();
        let err = reg
            .register_remote_workspace(
                "x",
                SshLocation {
                    user: None,
                    host: "h".into(),
                    port: None,
                    canonical_remote_path: "/srv".into(),
                },
                Some(ContainerLocation {
                    container_id: "  ".into(),
                    cwd_in_container: None,
                }),
                true,
            )
            .unwrap_err();
        match err {
            WorkspaceError::RemoteFieldInvalid { field, .. } => {
                assert_eq!(field, "containerId")
            }
            other => panic!("expected RemoteFieldInvalid(containerId); got {other:?}"),
        }
    }

    /// Registering the same Remote SSH identity tuple twice must
    /// be rejected with `CanonicalDuplicate` and the registry on
    /// disk must end with exactly one Remote record. Covers the
    /// spec §6.1 duplicate detection for Remote workspaces.
    #[test]
    fn register_remote_workspace_rejects_duplicate_remote_identity_tuple() {
        let (storage, _home, reg) = fresh_registry();
        let ssh = SshLocation {
            user: Some("alice".into()),
            host: "h.example".into(),
            port: Some(22),
            canonical_remote_path: "/srv".into(),
        };
        let first = reg
            .register_remote_workspace("first", ssh.clone(), None, true)
            .expect("first registration");
        let err = reg
            .register_remote_workspace("second", ssh, None, true)
            .expect_err("second registration must be rejected as duplicate");
        match err {
            WorkspaceError::CanonicalDuplicate {
                existing_workspace_id,
                existing_name,
            } => {
                assert_eq!(existing_workspace_id, first.workspace_id);
                assert_eq!(existing_name, "first");
            }
            other => panic!("expected CanonicalDuplicate; got {other:?}"),
        }
        drop(reg);
        let reloaded = WorkspaceRegistry::load(storage.path().to_path_buf(), None);
        let remote_count = reloaded
            .list()
            .iter()
            .filter(|r| matches!(r.location, WorkspaceLocation::Remote { .. }))
            .count();
        assert_eq!(
            remote_count, 1,
            "duplicate rejection must leave exactly one Remote record on disk"
        );
    }

    /// `port = None` and `port = Some(22)` MUST normalize to the
    /// same identity (SSH's default port). Mirrors the
    /// `normalized_port` step in the spec's identity tuple.
    #[test]
    fn register_remote_workspace_treats_implicit_port_22_as_duplicate_of_explicit_22() {
        let (_storage, _home, reg) = fresh_registry();
        reg.register_remote_workspace(
            "implicit-port",
            SshLocation {
                user: Some("alice".into()),
                host: "h.example".into(),
                port: None,
                canonical_remote_path: "/srv".into(),
            },
            None,
            true,
        )
        .expect("implicit-port registration");
        let err = reg
            .register_remote_workspace(
                "explicit-port",
                SshLocation {
                    user: Some("alice".into()),
                    host: "h.example".into(),
                    port: Some(22),
                    canonical_remote_path: "/srv".into(),
                },
                None,
                true,
            )
            .expect_err("port=None vs port=Some(22) must collide");
        assert!(matches!(err, WorkspaceError::CanonicalDuplicate { .. }));
    }

    /// Remote-with-container and Remote-without-container at the
    /// same SSH location are NOT duplicates — the container record
    /// names a distinct filesystem (the container's), the
    /// host-side record names the host's. Both are legitimate
    /// workspaces a user may want side by side.
    #[test]
    fn register_remote_workspace_allows_container_and_host_side_by_side() {
        let (_storage, _home, reg) = fresh_registry();
        let ssh = SshLocation {
            user: Some("alice".into()),
            host: "h.example".into(),
            port: Some(22),
            canonical_remote_path: "/srv".into(),
        };
        reg.register_remote_workspace("host-side", ssh.clone(), None, true)
            .expect("host-side registration");
        let with_container = reg
            .register_remote_workspace(
                "container-side",
                ssh,
                Some(ContainerLocation {
                    container_id: "ctr-1".into(),
                    cwd_in_container: None,
                }),
                true,
            )
            .expect("container-side must NOT collide with host-side");
        assert!(matches!(
            with_container.location,
            WorkspaceLocation::Remote {
                container: Some(_), ..
            }
        ));
    }

    /// Canonical-duplicate detection must skip `Remote` records so a
    /// Remote SSH workspace whose nominal path happens to match a
    /// Local path does not falsely block creation.
    #[test]
    fn canonical_duplicate_detection_skips_remote_records() {
        let (_storage, home, reg) = fresh_registry();
        // Seed a Remote record whose remote path string equals a real
        // local path we'll then register.
        let local_dir = home.path().join("shared-name");
        std::fs::create_dir_all(&local_dir).unwrap();
        let remote_id = Uuid::new_v4();
        let remote_record = WorkspaceRecord {
            workspace_id: remote_id,
            name: "remote-shared-name".into(),
            location: WorkspaceLocation::Remote {
                ssh: SshLocation {
                    user: None,
                    host: "host.example".into(),
                    port: None,
                    canonical_remote_path: local_dir.to_string_lossy().into_owned(),
                },
                container: None,
            },
            profile: WorkspaceProfile::default_local(),
            created_at: "2026-01-01T00:00:00Z".into(),
            last_used_at: "2026-01-01T00:00:00Z".into(),
            open_tab_id: None,
            conversation_rounds_count: 0,
        };
        reg.insert_record_for_tests(remote_record);

        // Registering a Local workspace at the same nominal path must
        // succeed (the Remote record is not a duplicate of a Local).
        let local_rec = reg.register_workspace(&local_dir, true).expect("register local");
        assert_ne!(local_rec.workspace_id, remote_id);
        assert!(matches!(
            local_rec.location,
            WorkspaceLocation::Local { .. }
        ));
    }

    /// Stub transport whose `probe` always returns the supplied
    /// typed error. Lets the registry preflight test prove the
    /// "Err → no record persisted" contract without touching a
    /// real SSH endpoint.
    struct StubProbeTransport {
        probe_outcome: std::sync::Mutex<
            Option<Result<(), terminal_mesh_core::transport::TransportError>>,
        >,
    }

    impl StubProbeTransport {
        fn rejecting(err: terminal_mesh_core::transport::TransportError) -> Self {
            Self {
                probe_outcome: std::sync::Mutex::new(Some(Err(err))),
            }
        }
        fn accepting() -> Self {
            Self {
                probe_outcome: std::sync::Mutex::new(Some(Ok(()))),
            }
        }
    }

    impl terminal_mesh_core::transport::Transport for StubProbeTransport {
        fn spawn(
            &self,
            _request: terminal_mesh_core::transport::TransportSpawnRequest,
        ) -> Result<
            Box<dyn terminal_mesh_core::transport::TransportSession>,
            terminal_mesh_core::transport::TransportError,
        > {
            Err(terminal_mesh_core::transport::TransportError::Protocol {
                message: "stub spawn not used in this test".into(),
            })
        }
        fn probe(
            &self,
            _workspace: terminal_mesh_core::transport::WorkspaceLocation,
        ) -> Result<(), terminal_mesh_core::transport::TransportError> {
            self.probe_outcome
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| {
                    Err(terminal_mesh_core::transport::TransportError::Protocol {
                        message: "stub probe consumed twice".into(),
                    })
                })
        }
    }

    /// A failed probe MUST leave the registry on disk byte-
    /// identical to the pre-call state. Uses
    /// `try_register_remote_workspace_with_probe` directly with a
    /// stub transport so the contract is provable without a live
    /// SSH endpoint, and reloads the registry from disk to assert
    /// zero Remote records were persisted.
    #[test]
    fn try_register_remote_with_failed_probe_does_not_persist_record() {
        let (storage, _home, reg) = fresh_registry();
        let storage_path = storage.path().to_path_buf();

        // Snapshot: storage must start empty (no `workspaces.json`).
        assert!(
            !storage_path.join(WORKSPACES_FILENAME).exists(),
            "fresh registry must not have a workspaces.json on disk"
        );

        let stub = StubProbeTransport::rejecting(
            terminal_mesh_core::transport::TransportError::SshAuth {
                user: "alice".into(),
                host: "h.example".into(),
                port: 22,
            },
        );
        let ssh = SshLocation {
            user: Some("alice".into()),
            host: "h.example".into(),
            port: Some(22),
            canonical_remote_path: "/srv".into(),
        };
        let err = try_register_remote_workspace_with_probe(
            "rejected-remote",
            ssh,
            None,
            true,
            &reg,
            &stub,
            "ssh",
        )
        .expect_err("probe rejection must surface as DTO");
        match err {
            WorkspaceErrorDto::RemoteProbeFailed { phase, reason } => {
                assert_eq!(phase, "ssh");
                assert!(reason.contains("ssh auth"), "got {reason}");
                assert!(reason.contains("alice@h.example"), "got {reason}");
            }
            other => panic!("expected RemoteProbeFailed; got {other:?}"),
        }

        // Reload the registry from disk and assert zero Remote
        // records are present. The negative contract: a failed
        // Remote creation must NOT leave a broken workspace record
        // behind for the user to discover on next app launch.
        let reloaded = WorkspaceRegistry::load(storage_path.clone(), None);
        let remote_count = reloaded
            .list()
            .iter()
            .filter(|r| matches!(r.location, WorkspaceLocation::Remote { .. }))
            .count();
        assert_eq!(remote_count, 0, "no Remote record may be persisted on probe failure");
    }

    /// Conversely: a successful probe MUST persist the record so
    /// the same helper covers the happy path too.
    #[test]
    fn try_register_remote_with_successful_probe_persists_record() {
        let (storage, _home, reg) = fresh_registry();
        let stub = StubProbeTransport::accepting();
        let ssh = SshLocation {
            user: Some("alice".into()),
            host: "h.example".into(),
            port: Some(22),
            canonical_remote_path: "/srv".into(),
        };
        let rec = try_register_remote_workspace_with_probe(
            "accepted-remote",
            ssh,
            None,
            true,
            &reg,
            &stub,
            "ssh",
        )
        .expect("happy path");
        assert_eq!(rec.name, "accepted-remote");
        assert!(matches!(rec.location, WorkspaceLocation::Remote { .. }));

        // Reload from disk and assert the record is present.
        let reloaded = WorkspaceRegistry::load(storage.path().to_path_buf(), None);
        let remote_count = reloaded
            .list()
            .iter()
            .filter(|r| matches!(r.location, WorkspaceLocation::Remote { .. }))
            .count();
        assert_eq!(remote_count, 1, "successful probe must persist exactly one Remote record");
    }

    /// Stub transport whose `probe` PANICS if called. Lets the
    /// dedup-before-probe test prove the duplicate check fires
    /// BEFORE the network round-trip — if the helper called
    /// probe even once, the test panics with the explanatory
    /// message.
    struct PanicOnProbeTransport;
    impl terminal_mesh_core::transport::Transport for PanicOnProbeTransport {
        fn spawn(
            &self,
            _request: terminal_mesh_core::transport::TransportSpawnRequest,
        ) -> Result<
            Box<dyn terminal_mesh_core::transport::TransportSession>,
            terminal_mesh_core::transport::TransportError,
        > {
            panic!("spawn must not be called in the dedup-before-probe test");
        }
        fn probe(
            &self,
            _workspace: terminal_mesh_core::transport::WorkspaceLocation,
        ) -> Result<(), terminal_mesh_core::transport::TransportError> {
            panic!(
                "probe must not be called for a duplicate Remote identity tuple — dedup gate failed"
            );
        }
    }

    /// Registering a Remote workspace whose identity tuple is
    /// already known MUST return `CanonicalDuplicate` without
    /// running the SSH probe. Otherwise users with the same
    /// workspace already registered would wait for a slow
    /// transport probe failure (5-10s SSH timeout) before
    /// seeing the duplicate error — and would pay an
    /// unnecessary network round-trip for a record that cannot
    /// be added.
    #[test]
    fn try_register_remote_with_duplicate_returns_canonical_duplicate_without_probing() {
        let (_storage, _home, reg) = fresh_registry();
        let ssh = SshLocation {
            user: Some("alice".into()),
            host: "h.example".into(),
            port: Some(22),
            canonical_remote_path: "/srv".into(),
        };
        // Seed the existing duplicate.
        let first = reg
            .register_remote_workspace("first", ssh.clone(), None, true)
            .expect("seed");
        // Attempt the second registration with the same tuple,
        // using the panic-on-probe stub. If the helper called
        // probe (i.e. ordering bug), the panic surfaces here.
        let stub = PanicOnProbeTransport;
        let err = try_register_remote_workspace_with_probe(
            "second",
            ssh,
            None,
            true,
            &reg,
            &stub,
            "ssh",
        )
        .expect_err("duplicate must be rejected before probe runs");
        match err {
            WorkspaceErrorDto::CanonicalDuplicate {
                existing_workspace_id,
                existing_name,
            } => {
                assert_eq!(existing_workspace_id, first.workspace_id.to_string());
                assert_eq!(existing_name, "first");
            }
            other => panic!("expected CanonicalDuplicate; got {other:?}"),
        }
    }
}
