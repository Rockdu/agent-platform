//! Rust IPC dispatcher with spec-compliant `PluginCapability` lifecycle.
//!
//! Implements the contract defined in `docs/specs/plugin-contract.md`
//! sections "PluginCapability Lifecycle", "Permission Gating", and the
//! permissioned-command failure modes (`CapabilityMissing`,
//! `CapabilityInvalid`, `CapabilityExpired`, `CapabilityMismatched`,
//! `PermissionDenied`).
//!
//! Authoritative state lives in `MountRegistry`, keyed by
//! `(plugin_id, mount_id)`. Each entry stores:
//!   - a SHA-256 hash of the secret nonce (raw nonce is never persisted);
//!   - the manifest-declared permissions, copied at mount time;
//!   - `cross_tab_read_flag` (always `false` for regular mounts; orchestrator
//!     mount sets this in a later milestone);
//!   - `generation`, a monotonically-increasing counter bumped on each
//!     re-mount of the same `(plugin_id, mount_id)` so an in-flight IPC with
//!     a stale nonce cannot combine with rotated permissions.
//!
//! Opaque handle format presented to the frontend:
//!
//!     cap_v1.<mount_uuid_simple_hex>.<nonce_base64url_nopad>
//!
//! The dispatcher recovers `mount_id` from the handle envelope and validates
//! the nonce by comparing its SHA-256 hash to the registry in constant time
//! (`subtle::ConstantTimeEq`).
//!
//! Sidecar routing lands with task11/M2; the validated happy path here
//! returns the typed `NoSidecarWired` error so callers see a predictable
//! shape until then.

use std::collections::HashMap;
use std::sync::Mutex;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use serde::Serialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::generated::plugin_commands::PLUGIN_COMMANDS;
use crate::generated::plugin_registry::PLUGINS;

/// Identity of a mounted plugin instance. The `(plugin_id, mount_id)` pair is
/// the authoritative key per the plugin contract spec.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MountKey {
    pub plugin_id: String,
    pub mount_id: Uuid,
}

/// Per-mount state. The raw nonce is NEVER stored — only its SHA-256 hash.
#[derive(Debug, Clone)]
pub struct MountEntry {
    pub handle_nonce_hash: [u8; 32],
    pub permissions: Vec<String>,
    /// `false` for every regular mount. Orchestrator tab in task20+/task21
    /// sets this to grant cross-tab read across PTYs (subject to the
    /// `cross_tab_read` permission gate). Verified-false by the
    /// `cross_tab_read_flag_defaults_to_false` test.
    #[allow(dead_code)]
    pub cross_tab_read_flag: bool,
    pub generation: u64,
    /// Captured at mount time for later debug / orchestrator surfaces.
    /// Today the dispatcher emits `tab_id` directly from the IPC parameter
    /// (so per-call values are honored even if a tab is re-bound). Kept on
    /// the entry so a future `inspect_mounts` debug command can attribute
    /// long-lived mounts to their originating tab.
    #[allow(dead_code)]
    pub tab_id: Option<String>,
}

/// Registry state guarded by a single mutex so updates to `entries` and
/// `mount_index` are atomic per the spec's "Registry updates MUST be atomic
/// per (plugin_id, mount_id)" requirement.
#[derive(Debug, Default)]
struct RegistryState {
    entries: HashMap<MountKey, MountEntry>,
    /// Secondary index `mount_id -> plugin_id`. Lets the dispatcher detect
    /// `CapabilityMismatched` (the handle's mount is registered, but for a
    /// different plugin than the command targets) and lets `unmount` find the
    /// owning plugin from just a handle.
    mount_index: HashMap<Uuid, String>,
}

/// task21 / AC-3.3: shared via internal `Arc<Mutex<...>>` so the
/// Tauri-managed handle and the host RPC bridge clone can both hold
/// `MountRegistry` by value while sharing the same underlying state.
#[derive(Debug, Clone, Default)]
pub struct MountRegistry {
    state: std::sync::Arc<Mutex<RegistryState>>,
}

impl MountRegistry {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Successful response for `mount_plugin`. The opaque handle is the only
/// value the frontend ever holds; `mount_id` is exposed for diagnostics/UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MountResponse {
    pub handle: String,
    pub mount_id: String,
}

/// Typed errors carried across the IPC boundary. `kind` is the discriminant;
/// the optional context fields are populated when known.
#[derive(Debug, Clone, Serialize, thiserror::Error)]
#[serde(rename_all = "camelCase")]
pub struct DispatchErrorDto {
    pub kind: String,
    pub plugin_id: Option<String>,
    pub command_name: Option<String>,
    pub permission: Option<String>,
    pub message: String,
}

impl std::fmt::Display for DispatchErrorDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl DispatchErrorDto {
    fn unknown_plugin(plugin_id: &str) -> Self {
        Self {
            kind: "unknown_plugin".into(),
            plugin_id: Some(plugin_id.into()),
            command_name: None,
            permission: None,
            message: format!("plugin `{plugin_id}` is not registered"),
        }
    }

    fn unknown_command(plugin_id: &str, command_name: &str) -> Self {
        Self {
            kind: "unknown_command".into(),
            plugin_id: Some(plugin_id.into()),
            command_name: Some(command_name.into()),
            permission: None,
            message: format!("plugin `{plugin_id}` does not expose command `{command_name}`"),
        }
    }

    fn capability_missing(plugin_id: &str, command_name: &str) -> Self {
        Self {
            kind: "capability_missing".into(),
            plugin_id: Some(plugin_id.into()),
            command_name: Some(command_name.into()),
            permission: None,
            message: format!(
                "command `{plugin_id}.{command_name}` requires a PluginCapability but none was provided"
            ),
        }
    }

    fn capability_invalid() -> Self {
        Self {
            kind: "capability_invalid".into(),
            plugin_id: None,
            command_name: None,
            permission: None,
            message: "capability handle is malformed or its nonce hash does not match the registry".into(),
        }
    }

    fn capability_expired() -> Self {
        Self {
            kind: "capability_expired".into(),
            plugin_id: None,
            command_name: None,
            permission: None,
            message: "capability handle's (plugin_id, mount_id) is not registered (or no longer registered)".into(),
        }
    }

    fn capability_mismatched(
        capability_plugin_id: &str,
        command_plugin_id: &str,
        command_name: &str,
    ) -> Self {
        Self {
            kind: "capability_mismatched".into(),
            plugin_id: Some(command_plugin_id.into()),
            command_name: Some(command_name.into()),
            permission: None,
            message: format!(
                "capability was minted for plugin `{capability_plugin_id}` but the command targets plugin `{command_plugin_id}`"
            ),
        }
    }

    fn permission_denied(plugin_id: &str, command_name: &str, permission: &str) -> Self {
        Self {
            kind: "permission_denied".into(),
            plugin_id: Some(plugin_id.into()),
            command_name: Some(command_name.into()),
            permission: Some(permission.into()),
            message: format!(
                "plugin `{plugin_id}` did not declare permission `{permission}` required by command `{command_name}` in its manifest"
            ),
        }
    }

    fn no_sidecar_wired(plugin_id: &str, command_name: &str) -> Self {
        Self {
            kind: "no_sidecar_wired".into(),
            plugin_id: Some(plugin_id.into()),
            command_name: Some(command_name.into()),
            permission: None,
            message: format!(
                "dispatcher reached the routing layer for `{plugin_id}.{command_name}` but no sidecar is wired yet"
            ),
        }
    }

    fn invalid_command_id(command_id: &str) -> Self {
        Self {
            kind: "invalid_command_id".into(),
            plugin_id: None,
            command_name: None,
            permission: None,
            message: format!(
                "command id `{command_id}` does not match the required shape `plugin.<plugin_id>.<command_name>`"
            ),
        }
    }
}

/// `plugin.<plugin_id>.<command_name>` decomposition. Plugin IDs and command
/// names are constrained to `[a-z0-9_-]` by the codegen validator, so the
/// split here is a simple prefix + split-at-next-dot.
fn parse_command_id(command_id: &str) -> Result<(&str, &str), DispatchErrorDto> {
    let rest = command_id
        .strip_prefix("plugin.")
        .ok_or_else(|| DispatchErrorDto::invalid_command_id(command_id))?;
    let dot = rest
        .find('.')
        .ok_or_else(|| DispatchErrorDto::invalid_command_id(command_id))?;
    let plugin_id = &rest[..dot];
    let command_name = &rest[dot + 1..];
    if plugin_id.is_empty() || command_name.is_empty() {
        return Err(DispatchErrorDto::invalid_command_id(command_id));
    }
    Ok((plugin_id, command_name))
}

/// Plugin-level declared permissions. Falls through to the built-in
/// metadata table for host-internal MCP sidecars (`terminal-mesh`)
/// that don't have a frontend plugin manifest. See
/// `crate::builtin_plugins`.
pub(crate) fn plugin_permissions(plugin_id: &str) -> Option<Vec<String>> {
    if let Some(p) = PLUGINS.iter().find(|p| p.plugin_id == plugin_id) {
        return Some(p.permissions.iter().map(|s| (*s).to_string()).collect());
    }
    crate::builtin_plugins::lookup_plugin(plugin_id)
        .map(|p| p.permissions.iter().map(|s| (*s).to_string()).collect())
}

/// Command-level required permissions. Falls through to built-in
/// metadata when the command is not in the generated `PLUGIN_COMMANDS`.
pub(crate) fn command_required_permissions(
    plugin_id: &str,
    command_name: &str,
) -> Option<Vec<String>> {
    if let Some(c) = PLUGIN_COMMANDS
        .iter()
        .find(|c| c.plugin_id == plugin_id && c.name == command_name)
    {
        return Some(c.permissions.iter().map(|s| (*s).to_string()).collect());
    }
    crate::builtin_plugins::lookup_command(plugin_id, command_name)
        .map(|c| c.permissions.iter().map(|s| (*s).to_string()).collect())
}

const HANDLE_PREFIX: &str = "cap_v1.";

pub(crate) fn encode_handle(mount_id: Uuid, nonce: &[u8]) -> String {
    format!(
        "{HANDLE_PREFIX}{}.{}",
        mount_id.simple(),
        URL_SAFE_NO_PAD.encode(nonce),
    )
}

/// Decode an opaque handle. Returns `None` for any malformed input — that is
/// the dispatcher's signal to return `CapabilityInvalid`.
fn parse_handle(handle: &str) -> Option<(Uuid, Vec<u8>)> {
    let body = handle.strip_prefix(HANDLE_PREFIX)?;
    let dot = body.find('.')?;
    let mount_id_str = &body[..dot];
    let nonce_b64 = &body[dot + 1..];
    if mount_id_str.is_empty() || nonce_b64.is_empty() {
        return None;
    }
    let mount_id = Uuid::parse_str(mount_id_str).ok()?;
    let nonce = URL_SAFE_NO_PAD.decode(nonce_b64).ok()?;
    if nonce.is_empty() {
        return None;
    }
    Some((mount_id, nonce))
}

pub(crate) fn hash_nonce(nonce: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(nonce);
    hasher.finalize().into()
}

pub(crate) fn fresh_nonce_bytes() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    // OsRng pulls from the OS CSPRNG (per spec: "nonce MUST be generated by
    // Rust using an OS cryptographic random source").
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes
}

/// Pure-function mount core. The Tauri command shim below forwards to this.
pub fn mount_inner(
    registry: &MountRegistry,
    plugin_id: &str,
    tab_id: Option<&str>,
) -> Result<MountResponse, DispatchErrorDto> {
    let request_id = Uuid::new_v4();
    let tab_id_log = tab_id.unwrap_or("unknown");

    let permissions = match plugin_permissions(plugin_id) {
        Some(p) => p,
        None => {
            tracing::warn!(
                request_id = %request_id,
                tab_id = tab_id_log,
                plugin_id = %plugin_id,
                error_kind = "unknown_plugin",
                "mount rejected"
            );
            return Err(DispatchErrorDto::unknown_plugin(plugin_id));
        }
    };

    let mount_id = Uuid::new_v4();
    let nonce_bytes = fresh_nonce_bytes();
    let handle = encode_handle(mount_id, &nonce_bytes);

    let generation = insert_mount(
        registry,
        plugin_id,
        mount_id,
        hash_nonce(&nonce_bytes),
        permissions,
        tab_id.map(str::to_string),
    );

    tracing::info!(
        request_id = %request_id,
        tab_id = tab_id_log,
        plugin_id = %plugin_id,
        mount_id = %mount_id,
        generation = generation,
        "plugin mounted"
    );

    Ok(MountResponse {
        handle,
        mount_id: mount_id.to_string(),
    })
}

/// Test-visible insertion path. Public so test helpers can construct entries
/// with a synthetic `mount_id` (which the production path never reuses since
/// `mount_inner` always allocates a fresh UUIDv4). Always inserts with
/// `cross_tab_read_flag = false`; the privileged grant goes through
/// [`insert_orchestrator_mount`] instead.
pub fn insert_mount(
    registry: &MountRegistry,
    plugin_id: &str,
    mount_id: Uuid,
    handle_nonce_hash: [u8; 32],
    permissions: Vec<String>,
    tab_id: Option<String>,
) -> u64 {
    insert_mount_with_flag(
        registry,
        plugin_id,
        mount_id,
        handle_nonce_hash,
        permissions,
        tab_id,
        false,
    )
}

/// task21 / AC-3.3: privileged insertion path used ONLY by the orchestrator
/// bootstrap to mount its `terminal-mesh` capability with
/// `cross_tab_read_flag = true`. Reachable only from inside Rust state —
/// no Tauri command and no IPC surface invokes this. Returns the freshly-
/// minted opaque handle envelope (`cap_v1.<mount_uuid>.<nonce_b64>`); the
/// caller stashes the handle in non-serialized Rust state so the orchestrator
/// terminal-mesh sidecar can present it later.
///
/// The `cross_tab_read_flag` bit lives only in `MountEntry` (Rust state);
/// `MountResponse` does NOT expose it, so a returned handle alone does not
/// disclose the privilege grant to any frontend object.
pub fn insert_orchestrator_mount(
    registry: &MountRegistry,
    plugin_id: &str,
    tab_id: Option<&str>,
) -> Result<MountResponse, DispatchErrorDto> {
    // Resolve manifest (generated) or built-in permission metadata.
    // Per `docs/specs/plugin-contract.md`: the orchestrator's
    // privileged grant requires the target plugin to DECLARE the
    // `cross_tab_read` permission. Silently granting the flag for
    // a plugin whose metadata doesn't claim the permission would
    // sidestep the contract.
    let permissions = plugin_permissions(plugin_id)
        .ok_or_else(|| DispatchErrorDto::unknown_plugin(plugin_id))?;
    if !permissions.iter().any(|p| p == "cross_tab_read") {
        return Err(DispatchErrorDto::permission_denied(
            plugin_id,
            "<orchestrator-mount>",
            "cross_tab_read",
        ));
    }
    let mount_id = Uuid::new_v4();
    let nonce_bytes = fresh_nonce_bytes();
    let handle = encode_handle(mount_id, &nonce_bytes);

    let generation = insert_mount_with_flag(
        registry,
        plugin_id,
        mount_id,
        hash_nonce(&nonce_bytes),
        permissions,
        tab_id.map(str::to_string),
        true,
    );
    tracing::info!(
        plugin_id = %plugin_id,
        mount_id = %mount_id,
        generation = generation,
        cross_tab_read = true,
        "orchestrator mount granted (Rust-only cross_tab_read_flag)"
    );
    Ok(MountResponse {
        handle,
        mount_id: mount_id.to_string(),
    })
}

pub(crate) fn insert_mount_with_flag(
    registry: &MountRegistry,
    plugin_id: &str,
    mount_id: Uuid,
    handle_nonce_hash: [u8; 32],
    permissions: Vec<String>,
    tab_id: Option<String>,
    cross_tab_read_flag: bool,
) -> u64 {
    let mut state = registry.state.lock().expect("MountRegistry poisoned");
    let key = MountKey {
        plugin_id: plugin_id.to_string(),
        mount_id,
    };
    let generation = state.entries.get(&key).map(|e| e.generation + 1).unwrap_or(1);
    state.entries.insert(
        key,
        MountEntry {
            handle_nonce_hash,
            permissions,
            cross_tab_read_flag,
            generation,
            tab_id,
        },
    );
    state.mount_index.insert(mount_id, plugin_id.to_string());
    generation
}

/// task21 / AC-3.3: pure capability-handle validation used by
/// `terminal_mesh_cross_tab_read_scrollback` (and any future privileged
/// endpoint). Re-uses the same parse / lookup / constant-time nonce-compare
/// path as `dispatch_inner`, plus a plugin_id match check, but returns the
/// validated `MountEntry` so the caller can inspect `cross_tab_read_flag`
/// without re-walking the dispatch state machine.
///
/// Failure modes (mirror `dispatch_inner`):
/// - `CapabilityInvalid` — handle envelope malformed OR nonce hash mismatch.
/// - `CapabilityExpired` — `(plugin_id, mount_id)` not in registry.
/// - `CapabilityMismatched` — mount exists for a different plugin.
pub fn authorize_capability_handle(
    registry: &MountRegistry,
    handle: &str,
    expected_plugin_id: &str,
) -> Result<MountEntry, DispatchErrorDto> {
    let (mount_id, nonce_bytes) =
        parse_handle(handle).ok_or_else(DispatchErrorDto::capability_invalid)?;
    let (entry_opt, mount_owner_opt) = {
        let state = registry.state.lock().expect("MountRegistry poisoned");
        let key = MountKey {
            plugin_id: expected_plugin_id.to_string(),
            mount_id,
        };
        let entry = state.entries.get(&key).cloned();
        let owner = state.mount_index.get(&mount_id).cloned();
        (entry, owner)
    };
    let entry = match (entry_opt, mount_owner_opt) {
        (Some(entry), _) => entry,
        (None, Some(owner_plugin_id)) if owner_plugin_id != expected_plugin_id => {
            // The dispatcher's CapabilityMismatched shape carries the
            // intended command name; the cross-tab-read path doesn't have
            // one, so synthesize an "n/a" so the wire kind/message stays
            // consistent with the dispatcher.
            return Err(DispatchErrorDto::capability_mismatched(
                &owner_plugin_id,
                expected_plugin_id,
                "n/a",
            ));
        }
        _ => return Err(DispatchErrorDto::capability_expired()),
    };
    let provided_hash = hash_nonce(&nonce_bytes);
    if !bool::from(entry.handle_nonce_hash.ct_eq(&provided_hash)) {
        return Err(DispatchErrorDto::capability_invalid());
    }
    Ok(entry)
}

/// Pure-function unmount core.
pub fn unmount_inner(
    registry: &MountRegistry,
    handle: &str,
    tab_id: Option<&str>,
) -> Result<(), DispatchErrorDto> {
    let request_id = Uuid::new_v4();
    let tab_id_log = tab_id.unwrap_or("unknown");

    let (mount_id, nonce_bytes) = match parse_handle(handle) {
        Some(parsed) => parsed,
        None => {
            tracing::warn!(
                request_id = %request_id,
                tab_id = tab_id_log,
                error_kind = "capability_invalid",
                "unmount rejected: malformed handle"
            );
            return Err(DispatchErrorDto::capability_invalid());
        }
    };

    let mut state = registry.state.lock().expect("MountRegistry poisoned");
    let plugin_id = match state.mount_index.get(&mount_id).cloned() {
        Some(p) => p,
        None => {
            tracing::warn!(
                request_id = %request_id,
                tab_id = tab_id_log,
                mount_id = %mount_id,
                error_kind = "capability_expired",
                "unmount rejected: mount_id not in registry"
            );
            return Err(DispatchErrorDto::capability_expired());
        }
    };

    let key = MountKey {
        plugin_id: plugin_id.clone(),
        mount_id,
    };
    let entry = state.entries.get(&key).cloned().ok_or_else(|| {
        // mount_index says the mount is registered but entries doesn't — this
        // is a registry-corruption state that should never happen. Surface as
        // CapabilityExpired (operationally indistinguishable) and log loudly.
        tracing::error!(
            request_id = %request_id,
            tab_id = tab_id_log,
            plugin_id = %plugin_id,
            mount_id = %mount_id,
            "registry corruption: mount_index has entry but entries does not"
        );
        DispatchErrorDto::capability_expired()
    })?;

    let provided_hash = hash_nonce(&nonce_bytes);
    if !bool::from(entry.handle_nonce_hash.ct_eq(&provided_hash)) {
        tracing::warn!(
            request_id = %request_id,
            tab_id = tab_id_log,
            plugin_id = %plugin_id,
            mount_id = %mount_id,
            error_kind = "capability_invalid",
            "unmount rejected: nonce hash mismatch"
        );
        return Err(DispatchErrorDto::capability_invalid());
    }

    state.entries.remove(&key);
    state.mount_index.remove(&mount_id);

    tracing::info!(
        request_id = %request_id,
        tab_id = tab_id_log,
        plugin_id = %plugin_id,
        mount_id = %mount_id,
        "plugin unmounted"
    );

    Ok(())
}

/// Pure-function dispatch core. The Tauri command shim accepts `Option<String>`
/// for the capability (matching the IPC envelope where `capability` MAY be
/// omitted entirely — that case becomes `CapabilityMissing`).
pub fn dispatch_inner(
    registry: &MountRegistry,
    command_id: &str,
    _args: &serde_json::Value,
    capability: Option<&str>,
    tab_id: Option<&str>,
) -> Result<serde_json::Value, DispatchErrorDto> {
    let request_id = Uuid::new_v4();
    let tab_id_log = tab_id.unwrap_or("unknown");

    let (plugin_id, command_name) = parse_command_id(command_id).inspect_err(|err| {
        tracing::warn!(
            request_id = %request_id,
            tab_id = tab_id_log,
            command_id = %command_id,
            error_kind = %err.kind,
            "dispatch rejected: invalid command id"
        );
    })?;

    // Capability presence is the first contract gate (per spec failure-mode
    // ordering: "No handle for permissioned command: CapabilityMissing").
    let capability_str = match capability {
        Some(c) if !c.is_empty() => c,
        Some(_) => {
            tracing::warn!(
                request_id = %request_id,
                tab_id = tab_id_log,
                plugin_id = %plugin_id,
                command_name = %command_name,
                error_kind = "capability_invalid",
                "dispatch rejected: empty capability"
            );
            return Err(DispatchErrorDto::capability_invalid());
        }
        None => {
            tracing::warn!(
                request_id = %request_id,
                tab_id = tab_id_log,
                plugin_id = %plugin_id,
                command_name = %command_name,
                error_kind = "capability_missing",
                "dispatch rejected: no capability"
            );
            return Err(DispatchErrorDto::capability_missing(plugin_id, command_name));
        }
    };

    let (mount_id, nonce_bytes) = match parse_handle(capability_str) {
        Some(parsed) => parsed,
        None => {
            tracing::warn!(
                request_id = %request_id,
                tab_id = tab_id_log,
                plugin_id = %plugin_id,
                command_name = %command_name,
                error_kind = "capability_invalid",
                "dispatch rejected: malformed handle envelope"
            );
            return Err(DispatchErrorDto::capability_invalid());
        }
    };

    // Atomic snapshot per spec "Race safety: dispatcher validation MUST
    // operate against an atomic snapshot of the MountRegistry."
    let (entry_opt, mount_owner_opt) = {
        let state = registry.state.lock().expect("MountRegistry poisoned");
        let key = MountKey {
            plugin_id: plugin_id.to_string(),
            mount_id,
        };
        let entry = state.entries.get(&key).cloned();
        let owner = state.mount_index.get(&mount_id).cloned();
        (entry, owner)
    };

    let entry = match (entry_opt, mount_owner_opt) {
        (Some(entry), _) => entry,
        (None, Some(owner_plugin_id)) if owner_plugin_id != plugin_id => {
            // The mount exists but belongs to a different plugin — the
            // canonical CapabilityMismatched case.
            tracing::warn!(
                request_id = %request_id,
                tab_id = tab_id_log,
                capability_plugin_id = %owner_plugin_id,
                command_plugin_id = %plugin_id,
                command_name = %command_name,
                mount_id = %mount_id,
                error_kind = "capability_mismatched",
                "dispatch rejected: capability/command plugin mismatch"
            );
            return Err(DispatchErrorDto::capability_mismatched(
                &owner_plugin_id,
                plugin_id,
                command_name,
            ));
        }
        _ => {
            tracing::warn!(
                request_id = %request_id,
                tab_id = tab_id_log,
                plugin_id = %plugin_id,
                command_name = %command_name,
                mount_id = %mount_id,
                error_kind = "capability_expired",
                "dispatch rejected: (plugin_id, mount_id) not in registry"
            );
            return Err(DispatchErrorDto::capability_expired());
        }
    };

    // Constant-time nonce hash comparison: a forged or rotated nonce
    // (well-formed envelope, right mount_id, but wrong secret) yields
    // CapabilityInvalid per spec failure mode "Handle nonce malformed".
    let provided_hash = hash_nonce(&nonce_bytes);
    if !bool::from(entry.handle_nonce_hash.ct_eq(&provided_hash)) {
        tracing::warn!(
            request_id = %request_id,
            tab_id = tab_id_log,
            plugin_id = %plugin_id,
            command_name = %command_name,
            mount_id = %mount_id,
            error_kind = "capability_invalid",
            "dispatch rejected: nonce hash mismatch"
        );
        return Err(DispatchErrorDto::capability_invalid());
    }

    let required = match command_required_permissions(plugin_id, command_name) {
        Some(perms) => perms,
        None => {
            tracing::warn!(
                request_id = %request_id,
                tab_id = tab_id_log,
                plugin_id = %plugin_id,
                command_name = %command_name,
                mount_id = %mount_id,
                error_kind = "unknown_command",
                "dispatch rejected: command not in PLUGIN_COMMANDS"
            );
            return Err(DispatchErrorDto::unknown_command(plugin_id, command_name));
        }
    };

    for perm in &required {
        if !entry.permissions.iter().any(|p| p == perm) {
            tracing::warn!(
                request_id = %request_id,
                tab_id = tab_id_log,
                plugin_id = %plugin_id,
                command_name = %command_name,
                mount_id = %mount_id,
                permission = %perm,
                error_kind = "permission_denied",
                "dispatch rejected"
            );
            return Err(DispatchErrorDto::permission_denied(
                plugin_id,
                command_name,
                perm,
            ));
        }
    }

    // task11/M2 will replace this stub with the actual sidecar lookup +
    // stdio RPC. Until then, validation success yields the typed shape so
    // callers see a predictable error rather than a hang.
    tracing::info!(
        request_id = %request_id,
        tab_id = tab_id_log,
        plugin_id = %plugin_id,
        command_name = %command_name,
        mount_id = %mount_id,
        generation = entry.generation,
        "dispatch validated; returning no_sidecar_wired"
    );
    Err(DispatchErrorDto::no_sidecar_wired(plugin_id, command_name))
}

#[tauri::command]
pub fn mount_plugin(
    plugin_id: String,
    tab_id: Option<String>,
    registry: tauri::State<'_, MountRegistry>,
) -> Result<MountResponse, DispatchErrorDto> {
    mount_inner(registry.inner(), &plugin_id, tab_id.as_deref())
}

#[tauri::command]
pub fn unmount_plugin(
    handle: String,
    tab_id: Option<String>,
    registry: tauri::State<'_, MountRegistry>,
) -> Result<(), DispatchErrorDto> {
    unmount_inner(registry.inner(), &handle, tab_id.as_deref())
}

#[tauri::command]
pub fn dispatch_plugin_command(
    command_id: String,
    args: serde_json::Value,
    capability: Option<String>,
    tab_id: Option<String>,
    registry: tauri::State<'_, MountRegistry>,
) -> Result<serde_json::Value, DispatchErrorDto> {
    dispatch_inner(
        registry.inner(),
        &command_id,
        &args,
        capability.as_deref(),
        tab_id.as_deref(),
    )
}

#[cfg(test)]
mod test_capture {
    //! Per-test tracing capture: installs a `tracing::Subscriber` on the
    //! current thread that writes rendered JSON lines into a shared buffer,
    //! so individual tests can assert on the structured field shape.

    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use tracing::dispatcher::{self, Dispatch};
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    pub struct VecWriter(pub Arc<Mutex<Vec<u8>>>);

    impl Write for VecWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for VecWriter {
        type Writer = VecWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    pub fn with_capture<R>(f: impl FnOnce() -> R) -> (R, String) {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = VecWriter(buf.clone());
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(writer)
            .finish();
        let dispatch = Dispatch::new(subscriber);
        let result = dispatcher::with_default(&dispatch, f);
        let rendered = String::from_utf8(buf.lock().unwrap().clone()).unwrap_or_default();
        (result, rendered)
    }
}

#[cfg(test)]
mod tests {
    use super::test_capture::with_capture;
    use super::*;

    // example-notes is the only bundled plugin and provides ground truth:
    // permissions = ["notify"], create_note requires "notify",
    // list_notes requires nothing.
    const REAL_PLUGIN: &str = "example-notes";

    fn must_mount(registry: &MountRegistry, plugin_id: &str) -> MountResponse {
        mount_inner(registry, plugin_id, Some("tab-test"))
            .expect("mount should succeed for known plugin")
    }

    // ----- command id parsing -----

    #[test]
    fn parse_command_id_accepts_well_formed() {
        let (p, n) = parse_command_id("plugin.example-notes.create_note").unwrap();
        assert_eq!(p, "example-notes");
        assert_eq!(n, "create_note");
    }

    #[test]
    fn parse_command_id_rejects_malformed() {
        assert!(parse_command_id("notplugin.foo.bar").is_err());
        assert!(parse_command_id("plugin.").is_err());
        assert!(parse_command_id("plugin.foo.").is_err());
        assert!(parse_command_id("plugin..bar").is_err());
    }

    // ----- handle envelope round-trip -----

    #[test]
    fn parse_handle_round_trip() {
        let mount_id = Uuid::new_v4();
        let nonce = fresh_nonce_bytes();
        let handle = encode_handle(mount_id, &nonce);
        let (rid, rnonce) = parse_handle(&handle).expect("round-trip parses");
        assert_eq!(rid, mount_id);
        assert_eq!(rnonce.as_slice(), nonce.as_slice());
    }

    #[test]
    fn parse_handle_rejects_malformed() {
        assert!(parse_handle("").is_none(), "empty");
        assert!(parse_handle("no-prefix.abc.def").is_none(), "no prefix");
        assert!(parse_handle("cap_v1.").is_none(), "no body");
        assert!(parse_handle("cap_v1.no-dot-after-prefix").is_none(), "no dot");
        assert!(parse_handle("cap_v1..nonce").is_none(), "empty mount_id");
        assert!(parse_handle("cap_v1.deadbeef.").is_none(), "empty nonce");
        assert!(
            parse_handle("cap_v1.not-a-uuid.aGVsbG8").is_none(),
            "bad uuid"
        );
        assert!(
            parse_handle("cap_v1.00000000000000000000000000000000.!!notb64").is_none(),
            "bad base64"
        );
    }

    // ----- mount / unmount lifecycle -----

    #[test]
    fn mount_unknown_plugin_fails() {
        let registry = MountRegistry::new();
        let err = mount_inner(&registry, "does-not-exist", None).unwrap_err();
        assert_eq!(err.kind, "unknown_plugin");
        assert_eq!(err.plugin_id.as_deref(), Some("does-not-exist"));
    }

    #[test]
    fn mount_then_unmount_round_trip() {
        let registry = MountRegistry::new();
        let resp = must_mount(&registry, REAL_PLUGIN);
        assert!(resp.handle.starts_with("cap_v1."));
        // Inspect registry state directly.
        {
            let state = registry.state.lock().unwrap();
            assert_eq!(state.entries.len(), 1);
            assert_eq!(state.mount_index.len(), 1);
        }
        unmount_inner(&registry, &resp.handle, Some("tab-test")).expect("unmount succeeds");
        let state = registry.state.lock().unwrap();
        assert!(state.entries.is_empty(), "entry removed");
        assert!(state.mount_index.is_empty(), "mount_index entry removed");
    }

    #[test]
    fn unmount_unknown_handle_fails_with_invalid() {
        let registry = MountRegistry::new();
        let err = unmount_inner(&registry, "not-a-real-handle", None).unwrap_err();
        // Malformed envelope → CapabilityInvalid (per spec).
        assert_eq!(err.kind, "capability_invalid");
    }

    #[test]
    fn unmount_unregistered_well_formed_handle_fails_with_expired() {
        let registry = MountRegistry::new();
        let synthetic_handle = encode_handle(Uuid::new_v4(), &fresh_nonce_bytes());
        let err = unmount_inner(&registry, &synthetic_handle, None).unwrap_err();
        assert_eq!(err.kind, "capability_expired");
    }

    #[test]
    fn unmount_rejects_nonce_mismatch() {
        // Mount, then craft a handle with the same mount_id but a different
        // nonce: unmount must reject without removing the entry.
        let registry = MountRegistry::new();
        let resp = must_mount(&registry, REAL_PLUGIN);
        let mount_id = Uuid::parse_str(&resp.mount_id).unwrap();
        let bad_handle = encode_handle(mount_id, &fresh_nonce_bytes());
        let err = unmount_inner(&registry, &bad_handle, None).unwrap_err();
        assert_eq!(err.kind, "capability_invalid");
        // Original entry still present.
        let state = registry.state.lock().unwrap();
        assert_eq!(state.entries.len(), 1);
    }

    #[test]
    fn cross_tab_read_flag_defaults_to_false() {
        let registry = MountRegistry::new();
        let _ = must_mount(&registry, REAL_PLUGIN);
        let state = registry.state.lock().unwrap();
        for entry in state.entries.values() {
            assert!(
                !entry.cross_tab_read_flag,
                "regular mounts MUST have cross_tab_read_flag = false"
            );
        }
    }

    // ----- task21 / AC-3.3: orchestrator privileged mount -----

    #[test]
    fn insert_orchestrator_mount_sets_cross_tab_read_flag_true() {
        let registry = MountRegistry::new();
        let resp = insert_orchestrator_mount(&registry, "terminal-mesh", Some("tab-orch")).expect("terminal-mesh built-in metadata declares cross_tab_read");
        assert!(resp.handle.starts_with("cap_v1."));

        let state = registry.state.lock().unwrap();
        let entry = state
            .entries
            .values()
            .next()
            .expect("one entry after orchestrator mount");
        assert!(
            entry.cross_tab_read_flag,
            "orchestrator mount MUST set cross_tab_read_flag = true"
        );
        // Sanity: a parallel regular mount on a manifested plugin keeps the
        // flag false, so the privileged grant is per-mount, not global.
        drop(state);
        let regular = mount_inner(&registry, REAL_PLUGIN, Some("tab-regular")).unwrap();
        let state = registry.state.lock().unwrap();
        let regular_mount_id = Uuid::parse_str(&regular.mount_id).unwrap();
        let regular_entry = state
            .entries
            .iter()
            .find(|(k, _)| k.mount_id == regular_mount_id)
            .map(|(_, e)| e)
            .expect("regular mount in registry");
        assert!(
            !regular_entry.cross_tab_read_flag,
            "regular mount MUST NOT inherit the orchestrator's flag"
        );
    }

    #[test]
    fn mount_response_serialization_does_not_leak_cross_tab_read_field() {
        // task21 / AC-3.3 negative serialization probe: the wire
        // shape of MountResponse MUST NOT carry any cross_tab_read /
        // crossTabRead field, even after the orchestrator privileged
        // mint. The flag lives only in Rust-side MountEntry.
        let registry = MountRegistry::new();
        let resp = insert_orchestrator_mount(&registry, "terminal-mesh", Some("tab-orch")).expect("terminal-mesh built-in metadata declares cross_tab_read");
        let v: serde_json::Value = serde_json::to_value(&resp).unwrap();
        assert!(v.get("crossTabRead").is_none());
        assert!(v.get("cross_tab_read").is_none());
        assert!(v.get("crossTabReadFlag").is_none());
        // Only the documented fields are present.
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
        assert_eq!(
            keys.iter().copied().collect::<std::collections::BTreeSet<_>>(),
            ["handle", "mountId"].into_iter().collect()
        );
    }

    #[test]
    fn authorize_capability_handle_returns_entry_on_valid_handle() {
        let registry = MountRegistry::new();
        let resp = insert_orchestrator_mount(&registry, "terminal-mesh", Some("tab-orch")).expect("terminal-mesh built-in metadata declares cross_tab_read");
        let entry = authorize_capability_handle(&registry, &resp.handle, "terminal-mesh")
            .expect("valid orchestrator handle authorizes");
        assert!(entry.cross_tab_read_flag, "flag preserved through auth");
        assert_eq!(entry.tab_id.as_deref(), Some("tab-orch"));
    }

    #[test]
    fn authorize_capability_handle_rejects_invalid_handle_envelope() {
        let registry = MountRegistry::new();
        let err =
            authorize_capability_handle(&registry, "not-a-real-handle", "terminal-mesh").unwrap_err();
        assert_eq!(err.kind, "capability_invalid");
    }

    #[test]
    fn authorize_capability_handle_rejects_mismatched_plugin() {
        // Mount example-notes (regular), then attempt to authorize that
        // handle against "terminal-mesh" → CapabilityMismatched.
        let registry = MountRegistry::new();
        let resp = must_mount(&registry, REAL_PLUGIN);
        let err = authorize_capability_handle(&registry, &resp.handle, "terminal-mesh").unwrap_err();
        assert_eq!(err.kind, "capability_mismatched");
    }

    #[test]
    fn authorize_capability_handle_rejects_nonce_mismatch() {
        // Mount the orchestrator handle, then forge a handle with the
        // same mount_id but a fresh nonce: nonce-hash mismatch →
        // CapabilityInvalid.
        let registry = MountRegistry::new();
        let resp = insert_orchestrator_mount(&registry, "terminal-mesh", None).expect("terminal-mesh built-in metadata declares cross_tab_read");
        let mount_id = Uuid::parse_str(&resp.mount_id).unwrap();
        let forged = encode_handle(mount_id, &fresh_nonce_bytes());
        let err = authorize_capability_handle(&registry, &forged, "terminal-mesh").unwrap_err();
        assert_eq!(err.kind, "capability_invalid");
    }

    #[test]
    fn authorize_capability_handle_rejects_unknown_mount() {
        let registry = MountRegistry::new();
        let stale = encode_handle(Uuid::new_v4(), &fresh_nonce_bytes());
        let err = authorize_capability_handle(&registry, &stale, "terminal-mesh").unwrap_err();
        assert_eq!(err.kind, "capability_expired");
    }

    // ----- task21 Round 38 remediation: manifest permission required -----

    #[test]
    fn insert_orchestrator_mount_fails_for_unknown_plugin() {
        let registry = MountRegistry::new();
        let err = insert_orchestrator_mount(&registry, "no-such-plugin", None).unwrap_err();
        assert_eq!(err.kind, "unknown_plugin");
    }

    #[test]
    fn insert_orchestrator_mount_fails_for_plugin_without_cross_tab_read_permission() {
        // example-notes is a real manifest-registered plugin whose
        // permissions are `["notify"]` (no `cross_tab_read`). Even
        // though the plugin exists in PLUGINS, the orchestrator
        // privileged mount must reject it — the spec requires the
        // target's declared permissions to include `cross_tab_read`.
        let registry = MountRegistry::new();
        let err = insert_orchestrator_mount(&registry, "example-notes", Some("tab-x")).unwrap_err();
        assert_eq!(err.kind, "permission_denied");
        assert_eq!(err.permission.as_deref(), Some("cross_tab_read"));
    }

    #[test]
    fn insert_orchestrator_mount_succeeds_for_terminal_mesh_via_builtin_metadata() {
        // The plugin id `terminal-mesh` is NOT in the generated PLUGINS
        // but IS in BUILTIN_PLUGINS with `cross_tab_read` declared.
        // The mount must succeed through the built-in fallback path.
        let registry = MountRegistry::new();
        let resp = insert_orchestrator_mount(&registry, "terminal-mesh", Some("tab-orch"))
            .expect("terminal-mesh builtin metadata declares cross_tab_read");
        let state = registry.state.lock().unwrap();
        let entry = state.entries.values().next().expect("entry");
        assert!(entry.cross_tab_read_flag);
        assert!(entry.permissions.iter().any(|p| p == "cross_tab_read"));
        drop(state);
        drop(resp);
    }

    #[test]
    fn mount_twice_for_same_key_bumps_generation() {
        // Production `mount_inner` always allocates a fresh mount_id, so this
        // exercises the generation counter via `insert_mount` directly with a
        // stable mount_id (the path the React lifecycle will eventually use
        // for re-mount with the same logical `(plugin_id, mount_id)`).
        let registry = MountRegistry::new();
        let mount_id = Uuid::new_v4();
        let g1 = insert_mount(
            &registry,
            REAL_PLUGIN,
            mount_id,
            hash_nonce(&fresh_nonce_bytes()),
            vec!["notify".into()],
            Some("tab-test".into()),
        );
        let g2 = insert_mount(
            &registry,
            REAL_PLUGIN,
            mount_id,
            hash_nonce(&fresh_nonce_bytes()),
            vec!["notify".into()],
            Some("tab-test".into()),
        );
        assert_eq!(g1, 1);
        assert_eq!(g2, 2);
    }

    // ----- dispatch capability validation -----

    #[test]
    fn capability_missing_returns_typed_error() {
        let registry = MountRegistry::new();
        let err = dispatch_inner(
            &registry,
            "plugin.example-notes.list_notes",
            &serde_json::json!({}),
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err.kind, "capability_missing");
        assert_eq!(err.plugin_id.as_deref(), Some("example-notes"));
        assert_eq!(err.command_name.as_deref(), Some("list_notes"));
    }

    #[test]
    fn capability_empty_string_returns_capability_invalid() {
        let registry = MountRegistry::new();
        let err = dispatch_inner(
            &registry,
            "plugin.example-notes.list_notes",
            &serde_json::json!({}),
            Some(""),
            None,
        )
        .unwrap_err();
        assert_eq!(err.kind, "capability_invalid");
    }

    #[test]
    fn capability_malformed_returns_capability_invalid() {
        let registry = MountRegistry::new();
        let err = dispatch_inner(
            &registry,
            "plugin.example-notes.list_notes",
            &serde_json::json!({}),
            Some("not-a-cap-handle"),
            None,
        )
        .unwrap_err();
        assert_eq!(err.kind, "capability_invalid");
    }

    #[test]
    fn capability_expired_when_mount_id_unknown() {
        let registry = MountRegistry::new();
        let stale = encode_handle(Uuid::new_v4(), &fresh_nonce_bytes());
        let err = dispatch_inner(
            &registry,
            "plugin.example-notes.list_notes",
            &serde_json::json!({}),
            Some(&stale),
            None,
        )
        .unwrap_err();
        assert_eq!(err.kind, "capability_expired");
    }

    #[test]
    fn capability_mismatched_via_mount_index() {
        // Mount example-notes; then use that same handle to call a different
        // plugin. The mount_index has the mount_id keyed to example-notes,
        // but the command's plugin_id is different → mismatched.
        let registry = MountRegistry::new();
        let resp = must_mount(&registry, REAL_PLUGIN);
        let err = dispatch_inner(
            &registry,
            "plugin.someone-else.list_notes",
            &serde_json::json!({}),
            Some(&resp.handle),
            None,
        )
        .unwrap_err();
        assert_eq!(err.kind, "capability_mismatched");
        assert_eq!(err.plugin_id.as_deref(), Some("someone-else"));
    }

    #[test]
    fn capability_nonce_hash_mismatch_returns_capability_invalid() {
        // Mount, then craft a handle with the same mount_id but a different
        // nonce — well-formed envelope, registered mount, wrong secret.
        let registry = MountRegistry::new();
        let resp = must_mount(&registry, REAL_PLUGIN);
        let mount_id = Uuid::parse_str(&resp.mount_id).unwrap();
        let bad_handle = encode_handle(mount_id, &fresh_nonce_bytes());
        let err = dispatch_inner(
            &registry,
            "plugin.example-notes.list_notes",
            &serde_json::json!({}),
            Some(&bad_handle),
            None,
        )
        .unwrap_err();
        assert_eq!(err.kind, "capability_invalid");
    }

    // ----- dispatch permission gating + happy path -----

    #[test]
    fn dispatch_unknown_command_fails() {
        let registry = MountRegistry::new();
        let resp = must_mount(&registry, REAL_PLUGIN);
        let err = dispatch_inner(
            &registry,
            "plugin.example-notes.no_such_command",
            &serde_json::json!({}),
            Some(&resp.handle),
            None,
        )
        .unwrap_err();
        assert_eq!(err.kind, "unknown_command");
    }

    #[test]
    fn dispatch_invalid_command_id_fails() {
        let registry = MountRegistry::new();
        let resp = must_mount(&registry, REAL_PLUGIN);
        let err = dispatch_inner(
            &registry,
            "not-a-plugin-command",
            &serde_json::json!({}),
            Some(&resp.handle),
            None,
        )
        .unwrap_err();
        assert_eq!(err.kind, "invalid_command_id");
    }

    #[test]
    fn dispatch_happy_path_returns_no_sidecar_wired() {
        let registry = MountRegistry::new();
        let resp = must_mount(&registry, REAL_PLUGIN);
        let err = dispatch_inner(
            &registry,
            "plugin.example-notes.create_note",
            &serde_json::json!({"body": "hi"}),
            Some(&resp.handle),
            None,
        )
        .unwrap_err();
        assert_eq!(err.kind, "no_sidecar_wired");
    }

    #[test]
    fn dispatch_missing_permission_fails() {
        // Construct a mount whose manifest-declared permissions are empty so
        // the "notify" requirement of create_note fails.
        let registry = MountRegistry::new();
        let mount_id = Uuid::new_v4();
        let nonce = fresh_nonce_bytes();
        insert_mount(
            &registry,
            REAL_PLUGIN,
            mount_id,
            hash_nonce(&nonce),
            Vec::new(),
            Some("tab-test".into()),
        );
        let handle = encode_handle(mount_id, &nonce);
        let err = dispatch_inner(
            &registry,
            "plugin.example-notes.create_note",
            &serde_json::json!({"body": "x"}),
            Some(&handle),
            None,
        )
        .unwrap_err();
        assert_eq!(err.kind, "permission_denied");
        assert_eq!(err.permission.as_deref(), Some("notify"));
    }

    // ----- correlation logging (AC-9.5) -----

    #[test]
    fn mount_emits_correlation_fields() {
        let registry = MountRegistry::new();
        let (_resp, logs) = with_capture(|| {
            mount_inner(&registry, REAL_PLUGIN, Some("tab-test")).expect("mount")
        });
        assert!(logs.contains("\"request_id\""), "missing request_id: {logs}");
        assert!(
            logs.contains("\"tab_id\":\"tab-test\""),
            "missing tab_id: {logs}"
        );
        assert!(
            logs.contains("\"plugin_id\":\"example-notes\""),
            "missing plugin_id: {logs}"
        );
        assert!(logs.contains("\"mount_id\""), "missing mount_id: {logs}");
        assert!(
            logs.contains("\"generation\":1"),
            "missing generation: {logs}"
        );
    }

    #[test]
    fn unmount_emits_correlation_fields() {
        let registry = MountRegistry::new();
        let resp = must_mount(&registry, REAL_PLUGIN);
        let (_r, logs) = with_capture(|| {
            unmount_inner(&registry, &resp.handle, Some("tab-test")).expect("unmount")
        });
        assert!(logs.contains("\"request_id\""), "missing request_id: {logs}");
        assert!(
            logs.contains("\"tab_id\":\"tab-test\""),
            "missing tab_id: {logs}"
        );
        assert!(
            logs.contains("\"plugin_id\":\"example-notes\""),
            "missing plugin_id: {logs}"
        );
        assert!(logs.contains("\"mount_id\""), "missing mount_id: {logs}");
    }

    #[test]
    fn dispatch_permission_denied_emits_correlation_fields() {
        let registry = MountRegistry::new();
        let mount_id = Uuid::new_v4();
        let nonce = fresh_nonce_bytes();
        insert_mount(
            &registry,
            REAL_PLUGIN,
            mount_id,
            hash_nonce(&nonce),
            Vec::new(),
            Some("tab-test".into()),
        );
        let handle = encode_handle(mount_id, &nonce);
        let (_err, logs) = with_capture(|| {
            dispatch_inner(
                &registry,
                "plugin.example-notes.create_note",
                &serde_json::json!({"body": "x"}),
                Some(&handle),
                Some("tab-test"),
            )
        });
        assert!(logs.contains("\"request_id\""), "missing request_id: {logs}");
        assert!(
            logs.contains("\"tab_id\":\"tab-test\""),
            "missing tab_id: {logs}"
        );
        assert!(
            logs.contains("\"plugin_id\":\"example-notes\""),
            "missing plugin_id: {logs}"
        );
        assert!(
            logs.contains("\"command_name\":\"create_note\""),
            "missing command_name: {logs}"
        );
        assert!(
            logs.contains("\"permission\":\"notify\""),
            "missing permission: {logs}"
        );
    }

    #[test]
    fn dispatch_no_sidecar_wired_emits_correlation_fields() {
        let registry = MountRegistry::new();
        let resp = must_mount(&registry, REAL_PLUGIN);
        let (_err, logs) = with_capture(|| {
            dispatch_inner(
                &registry,
                "plugin.example-notes.create_note",
                &serde_json::json!({"body": "hi"}),
                Some(&resp.handle),
                Some("tab-test"),
            )
        });
        assert!(logs.contains("\"request_id\""), "missing request_id: {logs}");
        assert!(
            logs.contains("\"tab_id\":\"tab-test\""),
            "missing tab_id: {logs}"
        );
        assert!(
            logs.contains("\"plugin_id\":\"example-notes\""),
            "missing plugin_id: {logs}"
        );
        assert!(logs.contains("\"mount_id\""), "missing mount_id: {logs}");
        assert!(
            logs.contains("\"command_name\":\"create_note\""),
            "missing command_name: {logs}"
        );
    }

    #[test]
    fn dispatch_capability_missing_emits_correlation_fields() {
        let registry = MountRegistry::new();
        let (_err, logs) = with_capture(|| {
            dispatch_inner(
                &registry,
                "plugin.example-notes.list_notes",
                &serde_json::json!({}),
                None,
                Some("tab-test"),
            )
        });
        assert!(logs.contains("\"request_id\""), "missing request_id: {logs}");
        assert!(
            logs.contains("\"tab_id\":\"tab-test\""),
            "missing tab_id: {logs}"
        );
        assert!(
            logs.contains("\"plugin_id\":\"example-notes\""),
            "missing plugin_id: {logs}"
        );
        assert!(
            logs.contains("\"error_kind\":\"capability_missing\""),
            "missing error_kind: {logs}"
        );
    }
}
