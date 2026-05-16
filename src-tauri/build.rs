//! Plugin scanner + codegen pipeline.
//!
//! Discovers plugin manifests at `<workspace>/plugins/*/plugin.toml`, validates
//! schema + uniqueness invariants, and emits generated Rust + TypeScript
//! artifacts into gitignored directories. Then delegates to `tauri_build::build()`.
//!
//! Generated outputs (all gitignored, regenerated clean each build):
//!   - src-tauri/src/generated/plugin_registry.rs    Rust static registry
//!   - src-tauri/src/generated/plugin_permissions.rs Permission enum + helpers
//!   - src-tauri/src/generated/mod.rs                Re-exports
//!   - src/generated/plugin-tabs.ts                  Frontend tab registry
//!   - src/generated/plugin-command-metadata.ts      Command -> permission map
//!   - src/generated/plugins/<plugin_id>.ts          Per-plugin typed wrappers (stubs in Round 1)
//!
//! Validation failures abort the build with `cargo:warning=...` and `panic!()`
//! so the offending plugin id is visible in `cargo build` output.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use walkdir::WalkDir;

#[derive(Debug, Deserialize)]
struct ManifestFile {
    name: String,
    version: String,
    #[serde(rename = "type")]
    plugin_type: String,
    command_bin: String,
    frontend: String,
    permissions: Vec<String>,
    required_apis: Vec<String>,
    db_namespace: String,
    migrations_path: String,
}

#[derive(Debug)]
struct PluginManifest {
    plugin_id: String,
    manifest_path: PathBuf,
    file: ManifestFile,
}

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .expect("src-tauri has a parent (workspace root)")
        .to_path_buf();
    let plugins_dir = workspace_root.join("plugins");

    // Rerun whenever plugins/ tree changes.
    println!("cargo:rerun-if-changed={}", plugins_dir.display());
    println!("cargo:rerun-if-changed=build.rs");

    let manifests = discover_manifests(&plugins_dir);
    validate_uniqueness(&manifests);

    let rust_generated_dir = manifest_dir.join("src").join("generated");
    let ts_generated_dir = workspace_root.join("src").join("generated");

    clean_dir(&rust_generated_dir);
    clean_dir(&ts_generated_dir);

    emit_rust_registry(&manifests, &rust_generated_dir);
    emit_rust_permissions(&manifests, &rust_generated_dir);
    emit_rust_mod(&rust_generated_dir);

    emit_ts_tab_registry(&manifests, &ts_generated_dir);
    emit_ts_command_metadata(&manifests, &ts_generated_dir);
    emit_ts_per_plugin_wrappers(&manifests, &ts_generated_dir);

    tauri_build::build();
}

fn discover_manifests(plugins_dir: &Path) -> Vec<PluginManifest> {
    let mut out = Vec::new();
    if !plugins_dir.exists() {
        return out;
    }

    for entry in WalkDir::new(plugins_dir)
        .min_depth(2)
        .max_depth(2)
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_name() != "plugin.toml" {
            continue;
        }
        let manifest_path = entry.into_path();
        let plugin_id = manifest_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(str::to_string)
            .unwrap_or_else(|| panic!("plugin manifest at {:?} has no parent dir", manifest_path));

        let raw = fs::read_to_string(&manifest_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", manifest_path.display(), e));
        let file: ManifestFile = toml::from_str(&raw).unwrap_or_else(|e| {
            panic!(
                "PLUGIN_CONTRACT_ERROR manifest_parse: {} ({})",
                manifest_path.display(),
                e
            )
        });

        validate_manifest_fields(&plugin_id, &file, &manifest_path);

        out.push(PluginManifest {
            plugin_id,
            manifest_path,
            file,
        });
    }

    // Stable order across builds.
    out.sort_by(|a, b| a.plugin_id.cmp(&b.plugin_id));
    out
}

fn validate_manifest_fields(plugin_id: &str, file: &ManifestFile, manifest_path: &Path) {
    let id_ok = !plugin_id.is_empty()
        && plugin_id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !id_ok {
        panic!(
            "PLUGIN_CONTRACT_ERROR invalid_plugin_id: directory name `{}` (at {}) must be lowercase ascii letters/digits/hyphen/underscore",
            plugin_id,
            manifest_path.display()
        );
    }

    for (name, value) in [
        ("name", &file.name),
        ("version", &file.version),
        ("type", &file.plugin_type),
        ("command_bin", &file.command_bin),
        ("frontend", &file.frontend),
        ("db_namespace", &file.db_namespace),
        ("migrations_path", &file.migrations_path),
    ] {
        if value.trim().is_empty() {
            panic!(
                "PLUGIN_CONTRACT_ERROR missing_required_field: plugin `{}` ({}) field `{}` is empty",
                plugin_id,
                manifest_path.display(),
                name
            );
        }
    }

    if file.plugin_type != "tab" {
        panic!(
            "PLUGIN_CONTRACT_ERROR unsupported_type: plugin `{}` ({}) declares type=`{}`; MVP only supports `tab`",
            plugin_id,
            manifest_path.display(),
            file.plugin_type
        );
    }

    // required_apis values are an open set in Round 1; richer validation lands with task4 dispatcher.
    if file.required_apis.is_empty() {
        panic!(
            "PLUGIN_CONTRACT_ERROR missing_required_apis: plugin `{}` ({}) declares empty required_apis",
            plugin_id,
            manifest_path.display()
        );
    }
}

fn validate_uniqueness(manifests: &[PluginManifest]) {
    let mut ids = HashSet::new();
    let mut namespaces = HashSet::new();
    let mut bins = HashSet::new();
    for m in manifests {
        if !ids.insert(m.plugin_id.clone()) {
            panic!(
                "PLUGIN_CONTRACT_ERROR duplicate_plugin_id: `{}` (at {})",
                m.plugin_id,
                m.manifest_path.display()
            );
        }
        if !namespaces.insert(m.file.db_namespace.clone()) {
            panic!(
                "PLUGIN_CONTRACT_ERROR duplicate_db_namespace: `{}` declared by plugin `{}` ({})",
                m.file.db_namespace,
                m.plugin_id,
                m.manifest_path.display()
            );
        }
        if !bins.insert(m.file.command_bin.clone()) {
            panic!(
                "PLUGIN_CONTRACT_ERROR duplicate_command_bin: `{}` declared by plugin `{}` ({})",
                m.file.command_bin,
                m.plugin_id,
                m.manifest_path.display()
            );
        }
    }
}

fn clean_dir(dir: &Path) {
    if dir.exists() {
        let _ = fs::remove_dir_all(dir);
    }
    fs::create_dir_all(dir)
        .unwrap_or_else(|e| panic!("failed to create {}: {}", dir.display(), e));
}

fn emit_rust_registry(manifests: &[PluginManifest], out_dir: &Path) {
    let mut entries = String::new();
    for m in manifests {
        let perms = m
            .file
            .permissions
            .iter()
            .map(|p| format!("{:?}", p))
            .collect::<Vec<_>>()
            .join(", ");
        let apis = m
            .file
            .required_apis
            .iter()
            .map(|a| format!("{:?}", a))
            .collect::<Vec<_>>()
            .join(", ");
        entries.push_str(&format!(
            "    PluginManifest {{
        plugin_id: {:?},
        name: {:?},
        version: {:?},
        kind: PluginKind::Tab,
        command_bin: {:?},
        frontend: {:?},
        permissions: &[{}],
        required_apis: &[{}],
        db_namespace: {:?},
        migrations_path: {:?},
    }},
",
            m.plugin_id,
            m.file.name,
            m.file.version,
            m.file.command_bin,
            m.file.frontend,
            perms,
            apis,
            m.file.db_namespace,
            m.file.migrations_path,
        ));
    }

    let body = format!(
        "// @generated by build.rs from plugins/*/plugin.toml. DO NOT EDIT.
#![allow(dead_code)]

#[derive(Debug, Clone, Copy)]
pub enum PluginKind {{
    Tab,
}}

#[derive(Debug)]
pub struct PluginManifest {{
    pub plugin_id: &'static str,
    pub name: &'static str,
    pub version: &'static str,
    pub kind: PluginKind,
    pub command_bin: &'static str,
    pub frontend: &'static str,
    pub permissions: &'static [&'static str],
    pub required_apis: &'static [&'static str],
    pub db_namespace: &'static str,
    pub migrations_path: &'static str,
}}

pub const PLUGINS: &[PluginManifest] = &[
{entries}];
"
    );
    write_if_changed(&out_dir.join("plugin_registry.rs"), &body);
}

fn emit_rust_permissions(manifests: &[PluginManifest], out_dir: &Path) {
    let mut perms: Vec<&str> = manifests
        .iter()
        .flat_map(|m| m.file.permissions.iter().map(String::as_str))
        .collect();
    perms.sort();
    perms.dedup();

    let unique = perms
        .iter()
        .map(|p| format!("    {:?},", p))
        .collect::<Vec<_>>()
        .join("\n");

    let body = format!(
        "// @generated by build.rs from plugins/*/plugin.toml. DO NOT EDIT.
#![allow(dead_code)]

/// Sorted, deduplicated list of every permission declared across all plugin manifests.
///
/// Task4 will turn this into a typed enum + parsing helpers consumed by the IPC dispatcher.
pub const DECLARED_PERMISSIONS: &[&str] = &[
{unique}
];
"
    );
    write_if_changed(&out_dir.join("plugin_permissions.rs"), &body);
}

fn emit_rust_mod(out_dir: &Path) {
    let body = "// @generated by build.rs. DO NOT EDIT.
pub mod plugin_registry;
pub mod plugin_permissions;
";
    write_if_changed(&out_dir.join("mod.rs"), body);
}

fn emit_ts_tab_registry(manifests: &[PluginManifest], out_dir: &Path) {
    let mut entries = String::new();
    for m in manifests {
        // Lazy import path: src/generated/plugins/<id>.ts re-exports a default
        // component shim until task7 + plugin frontends land.
        entries.push_str(&format!(
            "  {{
    pluginId: {id:?},
    label: {name:?},
    version: {version:?},
    permissions: [{perms}],
    requiredApis: [{apis}],
    // Lazy-imported via dynamic import; consumer must call `loadComponent()`.
    loadComponent: () => import(\"./plugins/{id}.ts\").then((m) => m.default),
  }},\n",
            id = m.plugin_id,
            name = m.file.name,
            version = m.file.version,
            perms = m
                .file
                .permissions
                .iter()
                .map(|p| format!("{:?}", p))
                .collect::<Vec<_>>()
                .join(", "),
            apis = m
                .file
                .required_apis
                .iter()
                .map(|a| format!("{:?}", a))
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }

    let body = format!(
        "// @generated by build.rs from plugins/*/plugin.toml. DO NOT EDIT.

import type {{ ComponentType }} from \"react\";

export interface PluginTabEntry {{
  pluginId: string;
  label: string;
  version: string;
  permissions: readonly string[];
  requiredApis: readonly string[];
  loadComponent: () => Promise<ComponentType>;
}}

export const PLUGIN_TABS: readonly PluginTabEntry[] = [
{entries}];
"
    );
    write_if_changed(&out_dir.join("plugin-tabs.ts"), &body);
}

fn emit_ts_command_metadata(manifests: &[PluginManifest], out_dir: &Path) {
    let mut entries = String::new();
    for m in manifests {
        entries.push_str(&format!(
            "  {id:?}: {{ permissions: [{perms}] }},\n",
            id = m.plugin_id,
            perms = m
                .file
                .permissions
                .iter()
                .map(|p| format!("{:?}", p))
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }

    let body = format!(
        "// @generated by build.rs from plugins/*/plugin.toml. DO NOT EDIT.
//
// Permission metadata per plugin. Task4 will extend this with per-command
// permission requirements (drawn from #[plugin_command(permissions=[...])]
// annotations) so the dispatcher and the TS wrappers share a single source.

export interface PluginPermissionMetadata {{
  permissions: readonly string[];
}}

export const PLUGIN_PERMISSIONS: Readonly<Record<string, PluginPermissionMetadata>> = {{
{entries}}};
"
    );
    write_if_changed(&out_dir.join("plugin-command-metadata.ts"), &body);
}

fn emit_ts_per_plugin_wrappers(manifests: &[PluginManifest], out_dir: &Path) {
    let plugins_subdir = out_dir.join("plugins");
    fs::create_dir_all(&plugins_subdir).unwrap_or_else(|e| {
        panic!("failed to create {}: {}", plugins_subdir.display(), e)
    });

    for m in manifests {
        let body = format!(
            "// @generated by build.rs from plugins/{id}/plugin.toml. DO NOT EDIT.
//
// Round 1 stub: exports a placeholder component that renders the plugin's
// manifest label. Real wrappers (typed commands generated from Rust SoT via
// ts-rs or specta) land with task4.

export const pluginMeta = {{
  pluginId: {id:?},
  name: {name:?},
  version: {version:?},
  permissions: [{perms}] as const,
}};

function PluginStub() {{
  return null;
}}
PluginStub.displayName = {label:?};

export default PluginStub;
",
            id = m.plugin_id,
            name = m.file.name,
            version = m.file.version,
            perms = m
                .file
                .permissions
                .iter()
                .map(|p| format!("{:?}", p))
                .collect::<Vec<_>>()
                .join(", "),
            label = format!("Plugin({})", m.plugin_id),
        );
        write_if_changed(&plugins_subdir.join(format!("{}.ts", m.plugin_id)), &body);
    }
}

fn write_if_changed(path: &Path, contents: &str) {
    let needs_write = match fs::read_to_string(path) {
        Ok(existing) => existing != contents,
        Err(_) => true,
    };
    if needs_write {
        fs::write(path, contents)
            .unwrap_or_else(|e| panic!("failed to write {}: {}", path.display(), e));
    }
}
