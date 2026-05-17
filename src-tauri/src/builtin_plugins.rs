//! Built-in (non-codegen) plugin metadata.
//!
//! The codegen-driven `PLUGINS` slice (`generated::plugin_registry::PLUGINS`)
//! only contains plugins that have a `plugins/<id>/plugin.toml` manifest +
//! frontend entry. A handful of platform-internal MCP sidecars don't have
//! frontend plugin manifests (today: `terminal-mesh`). Their permission
//! metadata still needs to flow through the dispatcher's
//! `plugin_permissions` / `command_required_permissions` lookups so the
//! Round-37/38 cross-tab read authorization gates can read declared
//! permissions, not silently default to empty.
//!
//! `dispatcher::plugin_permissions` / `command_required_permissions` fall
//! through to this table when a plugin id isn't found in `PLUGINS`.

/// A built-in MCP sidecar's manifest-equivalent metadata.
pub struct BuiltinPlugin {
    pub plugin_id: &'static str,
    pub command_bin: &'static str,
    pub permissions: &'static [&'static str],
    pub commands: &'static [BuiltinCommand],
}

pub struct BuiltinCommand {
    pub name: &'static str,
    pub permissions: &'static [&'static str],
}

/// task21 / AC-3.3: `terminal-mesh` is the privileged MCP sidecar the
/// orchestrator's `claude` talks to for cross-tab scrollback reads.
/// The `cross_tab_read` plugin-level permission is REQUIRED for a
/// privileged orchestrator mount (`dispatcher::insert_orchestrator_mount`
/// rejects plugins whose resolved permissions don't include it). The
/// `read_scrollback` command-level permission is REQUIRED at the
/// `cross_tab_read_inner` / host-bridge call site so the auth gate
/// matches `docs/specs/plugin-contract.md` §"Permission gating" — a
/// privileged-looking mount without the declared permission is still
/// denied at read time.
pub const BUILTIN_PLUGINS: &[BuiltinPlugin] = &[BuiltinPlugin {
    plugin_id: "terminal-mesh",
    command_bin: "terminal-mesh-sidecar",
    permissions: &["cross_tab_read", "pty.read_scrollback"],
    commands: &[BuiltinCommand {
        name: "read_scrollback",
        // Both required at the command level — defense in depth.
        // `cross_tab_read_inner` consults this list via
        // `dispatcher::command_required_permissions("terminal-mesh",
        // "read_scrollback")` (Round 39: actual enforcement, not
        // declarative-only). A mount that declares only one of the
        // two is denied at read time.
        permissions: &["cross_tab_read", "pty.read_scrollback"],
    }],
}];

pub fn lookup_plugin(plugin_id: &str) -> Option<&'static BuiltinPlugin> {
    BUILTIN_PLUGINS.iter().find(|p| p.plugin_id == plugin_id)
}

pub fn lookup_command(plugin_id: &str, command_name: &str) -> Option<&'static BuiltinCommand> {
    lookup_plugin(plugin_id)?
        .commands
        .iter()
        .find(|c| c.name == command_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_terminal_mesh_declares_cross_tab_read_permission() {
        let p = lookup_plugin("terminal-mesh").expect("terminal-mesh registered");
        assert!(
            p.permissions.iter().any(|x| *x == "cross_tab_read"),
            "terminal-mesh must declare cross_tab_read at plugin level"
        );
        let c = lookup_command("terminal-mesh", "read_scrollback")
            .expect("read_scrollback command registered");
        assert!(
            c.permissions.iter().any(|x| *x == "cross_tab_read"),
            "read_scrollback must require cross_tab_read at command level"
        );
    }

    #[test]
    fn lookup_unknown_plugin_returns_none() {
        assert!(lookup_plugin("does-not-exist").is_none());
        assert!(lookup_command("terminal-mesh", "nope").is_none());
    }
}
