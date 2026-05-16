//! Rust IPC dispatcher (task4 / Round 5).
//!
//! Owns the host-side `MountRegistry` (the authoritative source of truth for
//! `PluginCapability`) and the `dispatch_plugin_command` Tauri command that
//! every generated frontend wrapper routes through.
//!
//! Responsibilities:
//!   - Mint and rotate capability handles (`mount_plugin` / `unmount_plugin`).
//!   - Validate handles against the registry, rejecting forged / expired /
//!     wrong-mount nonces with typed errors before the command runs.
//!   - Enforce per-command permission gating against the caller's
//!     plugin-manifest-declared permissions (mandatory, not advisory — per
//!     AC-1.4).
//!   - Emit structured logs carrying correlation IDs (`request_id`,
//!     `plugin_id`, `mount_id`, `command_name`). `tab_id` is reserved for
//!     task17 when terminal-mesh tabs gain stable IDs.
//!
//! Sidecar routing (the actual cross-process call to a plugin sidecar) lands
//! with task11/M2. Until then, a successful validation path returns the typed
//! `NoSidecarWired` error so callers see a predictable shape.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;
use uuid::Uuid;

use crate::generated::plugin_commands::PLUGIN_COMMANDS;
use crate::generated::plugin_registry::PLUGINS;

/// One mounted plugin tab. The handle nonce is the secret that the frontend
/// must present on every dispatched command; the registry stores it alongside
/// the manifest-declared permissions so dispatch decisions never need to walk
/// the static `PLUGINS` table again per request.
#[derive(Debug, Clone)]
pub struct MountEntry {
    pub plugin_id: String,
    pub mount_id: Uuid,
    pub permissions: Vec<String>,
}

/// Host-owned mount registry. Behind a `Mutex` because the Tauri command
/// handlers run on a thread pool and there is no `async` work inside the
/// critical section.
#[derive(Debug, Default)]
pub struct MountRegistry {
    by_handle: Mutex<HashMap<String, MountEntry>>,
}

impl MountRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a freshly-minted mount and return the handle nonce. The nonce is
    /// the only value the frontend ever holds for this mount; the registry
    /// retains the canonical copy for validation.
    fn insert(&self, handle: String, entry: MountEntry) {
        let mut guard = self.by_handle.lock().expect("MountRegistry poisoned");
        guard.insert(handle, entry);
    }

    /// Pure-data lookup used by the dispatcher; clones the entry so the
    /// caller doesn't hold the lock across logging / routing work.
    fn lookup(&self, handle: &str) -> Option<MountEntry> {
        let guard = self.by_handle.lock().expect("MountRegistry poisoned");
        guard.get(handle).cloned()
    }

    /// Remove an entry. Returns `false` if the handle was not present.
    fn remove(&self, handle: &str) -> bool {
        let mut guard = self.by_handle.lock().expect("MountRegistry poisoned");
        guard.remove(handle).is_some()
    }
}

/// Successful response for `mount_plugin` — gives the frontend the opaque
/// nonce (`handle`) plus the human-readable mount identifier (`mount_id`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MountResponse {
    pub handle: String,
    pub mount_id: String,
}

/// Typed errors returned across the IPC boundary. Frontend code matches on
/// `kind` to render the right UX. Every variant carries enough structured
/// context to make logs and bug reports actionable without parsing the
/// `message` string.
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

    fn capability_invalid() -> Self {
        Self {
            kind: "capability_invalid".into(),
            plugin_id: None,
            command_name: None,
            permission: None,
            message: "capability handle is malformed or empty".into(),
        }
    }

    fn capability_expired() -> Self {
        Self {
            kind: "capability_expired".into(),
            plugin_id: None,
            command_name: None,
            permission: None,
            message: "capability handle is not (or no longer) registered".into(),
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
                "dispatcher reached the routing layer for `{plugin_id}.{command_name}` but no sidecar is wired yet (lands with task11)"
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

/// Look up the manifest entry for the given plugin id in the static registry.
fn plugin_permissions(plugin_id: &str) -> Option<Vec<String>> {
    PLUGINS
        .iter()
        .find(|p| p.plugin_id == plugin_id)
        .map(|p| p.permissions.iter().map(|s| (*s).to_string()).collect())
}

/// Find the per-command permission requirement for `(plugin_id, command_name)`.
fn command_required_permissions(plugin_id: &str, command_name: &str) -> Option<Vec<String>> {
    PLUGIN_COMMANDS
        .iter()
        .find(|c| c.plugin_id == plugin_id && c.name == command_name)
        .map(|c| c.permissions.iter().map(|s| (*s).to_string()).collect())
}

/// Pure-function dispatch core (no Tauri types) so tests can exercise the full
/// validation pipeline without standing up a Tauri runtime.
pub fn dispatch_inner(
    registry: &MountRegistry,
    command_id: &str,
    _args: &serde_json::Value,
    capability: &str,
) -> Result<serde_json::Value, DispatchErrorDto> {
    let request_id = Uuid::new_v4();

    let (plugin_id, command_name) = parse_command_id(command_id).inspect_err(|err| {
        tracing::warn!(
            request_id = %request_id,
            error = ?err,
            "dispatch rejected: invalid command id"
        );
    })?;

    if capability.is_empty() {
        tracing::warn!(
            request_id = %request_id,
            plugin_id = %plugin_id,
            command_name = %command_name,
            "dispatch rejected: empty capability"
        );
        return Err(DispatchErrorDto::capability_invalid());
    }

    let mount = registry.lookup(capability).ok_or_else(|| {
        tracing::warn!(
            request_id = %request_id,
            plugin_id = %plugin_id,
            command_name = %command_name,
            "dispatch rejected: capability not in registry"
        );
        DispatchErrorDto::capability_expired()
    })?;

    if mount.plugin_id != plugin_id {
        tracing::warn!(
            request_id = %request_id,
            capability_plugin_id = %mount.plugin_id,
            command_plugin_id = %plugin_id,
            command_name = %command_name,
            mount_id = %mount.mount_id,
            "dispatch rejected: capability/command plugin mismatch"
        );
        return Err(DispatchErrorDto::capability_mismatched(
            &mount.plugin_id,
            plugin_id,
            command_name,
        ));
    }

    let required = command_required_permissions(plugin_id, command_name).ok_or_else(|| {
        tracing::warn!(
            request_id = %request_id,
            plugin_id = %plugin_id,
            command_name = %command_name,
            mount_id = %mount.mount_id,
            "dispatch rejected: command not in PLUGIN_COMMANDS"
        );
        DispatchErrorDto::unknown_command(plugin_id, command_name)
    })?;

    for perm in &required {
        if !mount.permissions.iter().any(|p| p == perm) {
            tracing::warn!(
                request_id = %request_id,
                plugin_id = %plugin_id,
                command_name = %command_name,
                permission = %perm,
                mount_id = %mount.mount_id,
                "dispatch rejected: permission_denied"
            );
            return Err(DispatchErrorDto::permission_denied(
                plugin_id,
                command_name,
                perm,
            ));
        }
    }

    // Round 5 stub: validated successfully, but the sidecar that would actually
    // execute the command isn't wired yet. Callers see a predictable typed
    // error rather than a hang or a generic panic. task11 replaces this with
    // the real sidecar lookup + stdio RPC.
    tracing::info!(
        request_id = %request_id,
        plugin_id = %plugin_id,
        command_name = %command_name,
        mount_id = %mount.mount_id,
        "dispatch validated; returning no_sidecar_wired (task11 will route)"
    );
    Err(DispatchErrorDto::no_sidecar_wired(plugin_id, command_name))
}

/// Pure-function mount core for test access.
pub fn mount_inner(
    registry: &MountRegistry,
    plugin_id: &str,
) -> Result<MountResponse, DispatchErrorDto> {
    let permissions =
        plugin_permissions(plugin_id).ok_or_else(|| DispatchErrorDto::unknown_plugin(plugin_id))?;

    let mount_id = Uuid::new_v4();
    let handle = Uuid::new_v4().simple().to_string();

    registry.insert(
        handle.clone(),
        MountEntry {
            plugin_id: plugin_id.to_string(),
            mount_id,
            permissions,
        },
    );

    tracing::info!(
        plugin_id = %plugin_id,
        mount_id = %mount_id,
        "plugin mounted"
    );

    Ok(MountResponse {
        handle,
        mount_id: mount_id.to_string(),
    })
}

/// Pure-function unmount core for test access.
pub fn unmount_inner(registry: &MountRegistry, handle: &str) -> Result<(), DispatchErrorDto> {
    if registry.remove(handle) {
        tracing::info!("plugin unmounted");
        Ok(())
    } else {
        tracing::warn!("unmount rejected: capability not in registry");
        Err(DispatchErrorDto::capability_expired())
    }
}

#[tauri::command]
pub fn mount_plugin(
    plugin_id: String,
    registry: tauri::State<'_, MountRegistry>,
) -> Result<MountResponse, DispatchErrorDto> {
    mount_inner(registry.inner(), &plugin_id)
}

#[tauri::command]
pub fn unmount_plugin(
    handle: String,
    registry: tauri::State<'_, MountRegistry>,
) -> Result<(), DispatchErrorDto> {
    unmount_inner(registry.inner(), &handle)
}

#[tauri::command]
pub fn dispatch_plugin_command(
    command_id: String,
    args: serde_json::Value,
    capability: String,
    registry: tauri::State<'_, MountRegistry>,
) -> Result<serde_json::Value, DispatchErrorDto> {
    dispatch_inner(registry.inner(), &command_id, &args, &capability)
}

#[cfg(test)]
mod tests {
    use super::*;

    // All tests depend on `plugins/example-notes/plugin.toml` providing the
    // ground truth in the generated `PLUGINS` / `PLUGIN_COMMANDS` tables.
    // example-notes declares permissions = ["notify"] and command
    // `create_note` requires "notify"; `list_notes` requires nothing.
    const REAL_PLUGIN: &str = "example-notes";

    fn must_mount(registry: &MountRegistry, plugin_id: &str) -> MountResponse {
        mount_inner(registry, plugin_id).expect("mount should succeed for known plugin")
    }

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

    #[test]
    fn mount_unknown_plugin_fails() {
        let registry = MountRegistry::new();
        let err = mount_inner(&registry, "does-not-exist").unwrap_err();
        assert_eq!(err.kind, "unknown_plugin");
        assert_eq!(err.plugin_id.as_deref(), Some("does-not-exist"));
    }

    #[test]
    fn mount_then_unmount_round_trip() {
        let registry = MountRegistry::new();
        let resp = must_mount(&registry, REAL_PLUGIN);
        // Handle is non-empty + 32 chars (UUIDv4 simple format).
        assert_eq!(resp.handle.len(), 32);
        assert!(registry.lookup(&resp.handle).is_some());
        unmount_inner(&registry, &resp.handle).expect("unmount of known handle succeeds");
        assert!(registry.lookup(&resp.handle).is_none());
    }

    #[test]
    fn unmount_unknown_handle_fails() {
        let registry = MountRegistry::new();
        let err = unmount_inner(&registry, "not-a-real-handle").unwrap_err();
        assert_eq!(err.kind, "capability_expired");
    }

    #[test]
    fn dispatch_empty_capability_fails() {
        let registry = MountRegistry::new();
        let err = dispatch_inner(
            &registry,
            "plugin.example-notes.list_notes",
            &serde_json::json!({}),
            "",
        )
        .unwrap_err();
        assert_eq!(err.kind, "capability_invalid");
    }

    #[test]
    fn dispatch_unregistered_capability_fails() {
        let registry = MountRegistry::new();
        let err = dispatch_inner(
            &registry,
            "plugin.example-notes.list_notes",
            &serde_json::json!({}),
            "never-was-minted",
        )
        .unwrap_err();
        assert_eq!(err.kind, "capability_expired");
    }

    #[test]
    fn dispatch_unknown_command_fails() {
        let registry = MountRegistry::new();
        let resp = must_mount(&registry, REAL_PLUGIN);
        let err = dispatch_inner(
            &registry,
            "plugin.example-notes.no_such_command",
            &serde_json::json!({}),
            &resp.handle,
        )
        .unwrap_err();
        assert_eq!(err.kind, "unknown_command");
        assert_eq!(err.command_name.as_deref(), Some("no_such_command"));
    }

    #[test]
    fn dispatch_invalid_command_id_fails() {
        let registry = MountRegistry::new();
        let resp = must_mount(&registry, REAL_PLUGIN);
        let err = dispatch_inner(
            &registry,
            "not-a-plugin-command",
            &serde_json::json!({}),
            &resp.handle,
        )
        .unwrap_err();
        assert_eq!(err.kind, "invalid_command_id");
    }

    #[test]
    fn dispatch_happy_path_returns_no_sidecar_wired() {
        // example-notes declares permissions=["notify"]; create_note requires
        // "notify". Mount mirrors plugin-declared permissions, so this happy
        // path satisfies permission gating and reaches the routing stub.
        let registry = MountRegistry::new();
        let resp = must_mount(&registry, REAL_PLUGIN);
        let err = dispatch_inner(
            &registry,
            "plugin.example-notes.create_note",
            &serde_json::json!({"body": "hi"}),
            &resp.handle,
        )
        .unwrap_err();
        assert_eq!(err.kind, "no_sidecar_wired");
        assert_eq!(err.plugin_id.as_deref(), Some("example-notes"));
    }

    #[test]
    fn dispatch_missing_permission_fails() {
        // Synthesize a mount that lacks the "notify" permission — this is the
        // permission-gating contract: even with a valid handle, a permission
        // that the manifest never declared MUST be rejected before the command
        // executes. We bypass `mount_inner` to construct a mount whose
        // permissions list is intentionally empty.
        let registry = MountRegistry::new();
        let handle = Uuid::new_v4().simple().to_string();
        registry.insert(
            handle.clone(),
            MountEntry {
                plugin_id: REAL_PLUGIN.into(),
                mount_id: Uuid::new_v4(),
                permissions: Vec::new(),
            },
        );

        let err = dispatch_inner(
            &registry,
            "plugin.example-notes.create_note",
            &serde_json::json!({"body": "x"}),
            &handle,
        )
        .unwrap_err();
        assert_eq!(err.kind, "permission_denied");
        assert_eq!(err.permission.as_deref(), Some("notify"));
        assert_eq!(err.plugin_id.as_deref(), Some(REAL_PLUGIN));
    }

    #[test]
    fn dispatch_capability_mismatched_fails() {
        // Mount example-notes; attempt to dispatch a command targeting a
        // different (synthetic) plugin id. The mount's plugin_id does not
        // match the command's plugin_id, so the dispatcher must reject before
        // looking at permissions or the command table.
        let registry = MountRegistry::new();
        let resp = must_mount(&registry, REAL_PLUGIN);
        let err = dispatch_inner(
            &registry,
            "plugin.someone-else.list_notes",
            &serde_json::json!({}),
            &resp.handle,
        )
        .unwrap_err();
        assert_eq!(err.kind, "capability_mismatched");
        assert_eq!(err.plugin_id.as_deref(), Some("someone-else"));
    }
}
