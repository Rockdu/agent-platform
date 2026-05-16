//! Plugin manifest scanner + Rust/TypeScript codegen.
//!
//! Scans `<workspace>/plugins/*/plugin.toml`, validates schema + uniqueness
//! invariants, and emits a stable set of generated artifacts under
//! `<workspace>/src-tauri/src/generated/` (consumed by the host crate) and
//! `<workspace>/src/generated/` (consumed by the React frontend).
//!
//! Two entry points share this library:
//!
//!   - `src-tauri/build.rs` calls [`generate`] before `tauri_build::build()`.
//!   - `cargo run -p plugin-codegen` (invoked by the `npm prebuild` script)
//!     calls [`generate`] so the frontend build path works from a clean
//!     checkout without `cargo build` having run first.
//!
//! All validation failures produce typed errors with `PLUGIN_CONTRACT_ERROR`
//! prefix messages so they are unmistakable in any build log.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use walkdir::WalkDir;

/// Closed allow-list of frontend APIs a plugin manifest may request via
/// `required_apis`. Unknown values are a hard build failure.
pub const ALLOWED_REQUIRED_APIS: &[&str] = &["invoke", "listen"];

/// Output paths the generator writes to. `default(workspace_root)` matches the
/// repository's source layout; tests can override individual paths.
#[derive(Debug, Clone)]
pub struct OutputPaths {
    pub rust_generated_dir: PathBuf,
    pub ts_generated_dir: PathBuf,
}

impl OutputPaths {
    pub fn default(workspace_root: &Path) -> Self {
        Self {
            rust_generated_dir: workspace_root.join("src-tauri").join("src").join("generated"),
            ts_generated_dir: workspace_root.join("src").join("generated"),
        }
    }
}

/// One plugin's parsed manifest plus its on-disk identifier (the directory name).
#[derive(Debug, Clone)]
pub struct PluginManifest {
    pub plugin_id: String,
    pub manifest_path: PathBuf,
    pub file: ManifestFile,
}

/// Verbatim shape of `plugin.toml`. Extending this requires updating the
/// validator and the emitters in lockstep.
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestFile {
    pub name: String,
    pub version: String,
    #[serde(rename = "type")]
    pub plugin_type: String,
    pub command_bin: String,
    pub frontend: String,
    pub permissions: Vec<String>,
    pub required_apis: Vec<String>,
    pub db_namespace: String,
    pub migrations_path: String,
    #[serde(default)]
    pub commands: Vec<CommandDecl>,
}

/// One command exposed by a plugin's sidecar; permissions must be a subset of
/// the plugin's declared `permissions`.
#[derive(Debug, Clone, Deserialize)]
pub struct CommandDecl {
    pub name: String,
    #[serde(default)]
    pub permissions: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CodegenError {
    #[error("PLUGIN_CONTRACT_ERROR io: {context} ({source})")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("PLUGIN_CONTRACT_ERROR manifest_parse: {path} ({source})")]
    ManifestParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error(
        "PLUGIN_CONTRACT_ERROR invalid_plugin_id: directory name `{plugin_id}` (at {manifest}) must be lowercase ascii letters/digits/hyphen/underscore"
    )]
    InvalidPluginId { plugin_id: String, manifest: PathBuf },

    #[error(
        "PLUGIN_CONTRACT_ERROR missing_required_field: plugin `{plugin_id}` ({manifest}) field `{field}` is empty"
    )]
    MissingRequiredField {
        plugin_id: String,
        manifest: PathBuf,
        field: &'static str,
    },

    #[error(
        "PLUGIN_CONTRACT_ERROR unsupported_type: plugin `{plugin_id}` ({manifest}) declares type=`{type_value}`; MVP only supports `tab`"
    )]
    UnsupportedType {
        plugin_id: String,
        manifest: PathBuf,
        type_value: String,
    },

    #[error(
        "PLUGIN_CONTRACT_ERROR unknown_required_api: plugin `{plugin_id}` field `required_apis` value `{value}` (at {manifest}) is not in the allow-list {allowed:?}"
    )]
    UnknownRequiredApi {
        plugin_id: String,
        manifest: PathBuf,
        value: String,
        allowed: &'static [&'static str],
    },

    #[error(
        "PLUGIN_CONTRACT_ERROR missing_required_apis: plugin `{plugin_id}` ({manifest}) declares empty required_apis"
    )]
    EmptyRequiredApis { plugin_id: String, manifest: PathBuf },

    #[error(
        "PLUGIN_CONTRACT_ERROR duplicate_plugin_id: `{plugin_id}` declared at both {first} and {second}"
    )]
    DuplicatePluginId {
        plugin_id: String,
        first: PathBuf,
        second: PathBuf,
    },

    #[error(
        "PLUGIN_CONTRACT_ERROR duplicate_db_namespace: `{namespace}` declared by plugin `{first_id}` ({first_manifest}) and plugin `{second_id}` ({second_manifest})"
    )]
    DuplicateDbNamespace {
        namespace: String,
        first_id: String,
        first_manifest: PathBuf,
        second_id: String,
        second_manifest: PathBuf,
    },

    #[error(
        "PLUGIN_CONTRACT_ERROR duplicate_command_bin: `{command_bin}` declared by plugin `{first_id}` ({first_manifest}) and plugin `{second_id}` ({second_manifest})"
    )]
    DuplicateCommandBin {
        command_bin: String,
        first_id: String,
        first_manifest: PathBuf,
        second_id: String,
        second_manifest: PathBuf,
    },

    #[error(
        "PLUGIN_CONTRACT_ERROR invalid_command_name: plugin `{plugin_id}` command `{command_name}` (at {manifest}) must be non-empty snake_case ascii"
    )]
    InvalidCommandName {
        plugin_id: String,
        manifest: PathBuf,
        command_name: String,
    },

    #[error(
        "PLUGIN_CONTRACT_ERROR duplicate_command_name: plugin `{plugin_id}` declares command `{command_name}` twice (at {manifest})"
    )]
    DuplicateCommandName {
        plugin_id: String,
        manifest: PathBuf,
        command_name: String,
    },

    #[error(
        "PLUGIN_CONTRACT_ERROR command_permission_out_of_scope: plugin `{plugin_id}` command `{command_name}` requests permission `{permission}` not declared in the plugin's own permissions {plugin_permissions:?} (at {manifest})"
    )]
    CommandPermissionOutOfScope {
        plugin_id: String,
        manifest: PathBuf,
        command_name: String,
        permission: String,
        plugin_permissions: Vec<String>,
    },

    #[error(
        "PLUGIN_CONTRACT_ERROR raw_invoke_violation: frontend file `{file}` line {line} contains raw `invoke('plugin.…')` string. Use generated typed wrappers from src/generated/plugins/<id>.ts."
    )]
    RawInvokeUsage { file: PathBuf, line: usize },
}

/// Top-level entry point: scan + validate + emit.
pub fn generate(workspace_root: &Path, out: &OutputPaths) -> Result<(), CodegenError> {
    let plugins_dir = workspace_root.join("plugins");
    let manifests = scan_directory(&plugins_dir)?;
    generate_from_manifests(&manifests, out)?;
    check_no_raw_invoke(&workspace_root.join("src"))?;
    Ok(())
}

/// Pure-input variant for tests: validate the supplied manifests and emit to
/// the given paths. Skips filesystem scan and the raw-invoke check.
pub fn generate_from_manifests(
    manifests: &[PluginManifest],
    out: &OutputPaths,
) -> Result<(), CodegenError> {
    for m in manifests {
        validate_manifest(m)?;
    }
    validate_uniqueness(manifests)?;

    clean_dir(&out.rust_generated_dir)?;
    clean_dir(&out.ts_generated_dir)?;
    fs::create_dir_all(out.ts_generated_dir.join("plugins")).map_err(|source| CodegenError::Io {
        context: format!(
            "create plugins subdir at {}",
            out.ts_generated_dir.join("plugins").display()
        ),
        source,
    })?;

    emit_rust_registry(manifests, &out.rust_generated_dir)?;
    emit_rust_permissions(manifests, &out.rust_generated_dir)?;
    emit_rust_commands(manifests, &out.rust_generated_dir)?;
    emit_rust_mod(&out.rust_generated_dir)?;

    emit_ts_tab_registry(manifests, &out.ts_generated_dir)?;
    emit_ts_command_metadata(manifests, &out.ts_generated_dir)?;
    emit_ts_per_plugin_wrappers(manifests, &out.ts_generated_dir)?;

    Ok(())
}

/// Walk `<plugins_dir>/*/plugin.toml`. Returns an empty list if `plugins_dir`
/// does not exist (e.g. before any plugin manifests land).
pub fn scan_directory(plugins_dir: &Path) -> Result<Vec<PluginManifest>, CodegenError> {
    let mut out = Vec::new();
    if !plugins_dir.exists() {
        return Ok(out);
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
            .ok_or_else(|| CodegenError::Io {
                context: format!("resolve parent dir name of {}", manifest_path.display()),
                source: std::io::Error::other("no parent dir name"),
            })?;

        let raw = fs::read_to_string(&manifest_path).map_err(|source| CodegenError::Io {
            context: format!("read manifest {}", manifest_path.display()),
            source,
        })?;
        let file: ManifestFile =
            toml::from_str(&raw).map_err(|source| CodegenError::ManifestParse {
                path: manifest_path.clone(),
                source,
            })?;

        out.push(PluginManifest {
            plugin_id,
            manifest_path,
            file,
        });
    }

    out.sort_by(|a, b| a.plugin_id.cmp(&b.plugin_id));
    Ok(out)
}

fn validate_manifest(m: &PluginManifest) -> Result<(), CodegenError> {
    let id_ok = !m.plugin_id.is_empty()
        && m.plugin_id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !id_ok {
        return Err(CodegenError::InvalidPluginId {
            plugin_id: m.plugin_id.clone(),
            manifest: m.manifest_path.clone(),
        });
    }

    for (name, value) in [
        ("name", &m.file.name),
        ("version", &m.file.version),
        ("type", &m.file.plugin_type),
        ("command_bin", &m.file.command_bin),
        ("frontend", &m.file.frontend),
        ("db_namespace", &m.file.db_namespace),
        ("migrations_path", &m.file.migrations_path),
    ] {
        if value.trim().is_empty() {
            return Err(CodegenError::MissingRequiredField {
                plugin_id: m.plugin_id.clone(),
                manifest: m.manifest_path.clone(),
                field: name,
            });
        }
    }

    if m.file.plugin_type != "tab" {
        return Err(CodegenError::UnsupportedType {
            plugin_id: m.plugin_id.clone(),
            manifest: m.manifest_path.clone(),
            type_value: m.file.plugin_type.clone(),
        });
    }

    if m.file.required_apis.is_empty() {
        return Err(CodegenError::EmptyRequiredApis {
            plugin_id: m.plugin_id.clone(),
            manifest: m.manifest_path.clone(),
        });
    }
    for api in &m.file.required_apis {
        if !ALLOWED_REQUIRED_APIS.contains(&api.as_str()) {
            return Err(CodegenError::UnknownRequiredApi {
                plugin_id: m.plugin_id.clone(),
                manifest: m.manifest_path.clone(),
                value: api.clone(),
                allowed: ALLOWED_REQUIRED_APIS,
            });
        }
    }

    let plugin_perm_set: std::collections::HashSet<&str> =
        m.file.permissions.iter().map(String::as_str).collect();
    let mut seen_commands = std::collections::HashSet::new();
    for cmd in &m.file.commands {
        let name_ok = !cmd.name.is_empty()
            && cmd
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !name_ok {
            return Err(CodegenError::InvalidCommandName {
                plugin_id: m.plugin_id.clone(),
                manifest: m.manifest_path.clone(),
                command_name: cmd.name.clone(),
            });
        }
        if !seen_commands.insert(cmd.name.clone()) {
            return Err(CodegenError::DuplicateCommandName {
                plugin_id: m.plugin_id.clone(),
                manifest: m.manifest_path.clone(),
                command_name: cmd.name.clone(),
            });
        }
        for perm in &cmd.permissions {
            if !plugin_perm_set.contains(perm.as_str()) {
                return Err(CodegenError::CommandPermissionOutOfScope {
                    plugin_id: m.plugin_id.clone(),
                    manifest: m.manifest_path.clone(),
                    command_name: cmd.name.clone(),
                    permission: perm.clone(),
                    plugin_permissions: m.file.permissions.clone(),
                });
            }
        }
    }
    Ok(())
}

fn validate_uniqueness(manifests: &[PluginManifest]) -> Result<(), CodegenError> {
    let mut by_id: HashMap<String, PathBuf> = HashMap::new();
    let mut by_namespace: HashMap<String, (String, PathBuf)> = HashMap::new();
    let mut by_bin: HashMap<String, (String, PathBuf)> = HashMap::new();

    for m in manifests {
        if let Some(first_path) = by_id.get(&m.plugin_id) {
            return Err(CodegenError::DuplicatePluginId {
                plugin_id: m.plugin_id.clone(),
                first: first_path.clone(),
                second: m.manifest_path.clone(),
            });
        }
        by_id.insert(m.plugin_id.clone(), m.manifest_path.clone());

        if let Some((first_id, first_path)) = by_namespace.get(&m.file.db_namespace) {
            return Err(CodegenError::DuplicateDbNamespace {
                namespace: m.file.db_namespace.clone(),
                first_id: first_id.clone(),
                first_manifest: first_path.clone(),
                second_id: m.plugin_id.clone(),
                second_manifest: m.manifest_path.clone(),
            });
        }
        by_namespace.insert(
            m.file.db_namespace.clone(),
            (m.plugin_id.clone(), m.manifest_path.clone()),
        );

        if let Some((first_id, first_path)) = by_bin.get(&m.file.command_bin) {
            return Err(CodegenError::DuplicateCommandBin {
                command_bin: m.file.command_bin.clone(),
                first_id: first_id.clone(),
                first_manifest: first_path.clone(),
                second_id: m.plugin_id.clone(),
                second_manifest: m.manifest_path.clone(),
            });
        }
        by_bin.insert(
            m.file.command_bin.clone(),
            (m.plugin_id.clone(), m.manifest_path.clone()),
        );
    }
    Ok(())
}

/// Walk the human-authored frontend tree looking for `invoke('plugin.…')`
/// strings that should have gone through generated wrappers. Excludes
/// `src/generated/` so the generator's own emit can use whatever shape it
/// pleases.
fn check_no_raw_invoke(frontend_src: &Path) -> Result<(), CodegenError> {
    if !frontend_src.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(frontend_src).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.components().any(|c| c.as_os_str() == "generated") {
            continue;
        }
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        if !matches!(ext, "ts" | "tsx" | "js" | "jsx") {
            continue;
        }
        let contents = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for (idx, line) in contents.lines().enumerate() {
            // Match invoke('plugin.<id>.<cmd>' or invoke("plugin.<id>.<cmd>"
            let needle_single = "invoke('plugin.";
            let needle_double = "invoke(\"plugin.";
            if line.contains(needle_single) || line.contains(needle_double) {
                return Err(CodegenError::RawInvokeUsage {
                    file: path.to_path_buf(),
                    line: idx + 1,
                });
            }
        }
    }
    Ok(())
}

fn clean_dir(dir: &Path) -> Result<(), CodegenError> {
    if dir.exists() {
        fs::remove_dir_all(dir).map_err(|source| CodegenError::Io {
            context: format!("remove {}", dir.display()),
            source,
        })?;
    }
    fs::create_dir_all(dir).map_err(|source| CodegenError::Io {
        context: format!("create {}", dir.display()),
        source,
    })?;
    Ok(())
}

fn write_if_changed(path: &Path, contents: &str) -> Result<(), CodegenError> {
    let needs_write = match fs::read_to_string(path) {
        Ok(existing) => existing != contents,
        Err(_) => true,
    };
    if needs_write {
        fs::write(path, contents).map_err(|source| CodegenError::Io {
            context: format!("write {}", path.display()),
            source,
        })?;
    }
    Ok(())
}

fn emit_rust_registry(manifests: &[PluginManifest], out_dir: &Path) -> Result<(), CodegenError> {
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
        "// @generated by plugin-codegen from plugins/*/plugin.toml. DO NOT EDIT.
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
    write_if_changed(&out_dir.join("plugin_registry.rs"), &body)
}

fn emit_rust_permissions(
    manifests: &[PluginManifest],
    out_dir: &Path,
) -> Result<(), CodegenError> {
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
        "// @generated by plugin-codegen from plugins/*/plugin.toml. DO NOT EDIT.
#![allow(dead_code)]

/// Sorted, deduplicated list of every permission declared across all plugin manifests.
pub const DECLARED_PERMISSIONS: &[&str] = &[
{unique}
];
"
    );
    write_if_changed(&out_dir.join("plugin_permissions.rs"), &body)
}

fn emit_rust_commands(manifests: &[PluginManifest], out_dir: &Path) -> Result<(), CodegenError> {
    let mut entries = String::new();
    for m in manifests {
        for cmd in &m.file.commands {
            let perms = cmd
                .permissions
                .iter()
                .map(|p| format!("{:?}", p))
                .collect::<Vec<_>>()
                .join(", ");
            entries.push_str(&format!(
                "    PluginCommandSpec {{
        plugin_id: {:?},
        name: {:?},
        permissions: &[{}],
    }},
",
                m.plugin_id, cmd.name, perms,
            ));
        }
    }

    let body = format!(
        "// @generated by plugin-codegen from plugins/*/plugin.toml. DO NOT EDIT.
#![allow(dead_code)]

#[derive(Debug)]
pub struct PluginCommandSpec {{
    pub plugin_id: &'static str,
    pub name: &'static str,
    pub permissions: &'static [&'static str],
}}

pub const PLUGIN_COMMANDS: &[PluginCommandSpec] = &[
{entries}];
"
    );
    write_if_changed(&out_dir.join("plugin_commands.rs"), &body)
}

fn emit_rust_mod(out_dir: &Path) -> Result<(), CodegenError> {
    let body = "// @generated by plugin-codegen. DO NOT EDIT.
pub mod plugin_registry;
pub mod plugin_permissions;
pub mod plugin_commands;
";
    write_if_changed(&out_dir.join("mod.rs"), body)
}

fn emit_ts_tab_registry(manifests: &[PluginManifest], out_dir: &Path) -> Result<(), CodegenError> {
    let mut entries = String::new();
    for m in manifests {
        entries.push_str(&format!(
            "  {{
    pluginId: {id:?},
    label: {name:?},
    version: {version:?},
    permissions: [{perms}],
    requiredApis: [{apis}],
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
        "// @generated by plugin-codegen from plugins/*/plugin.toml. DO NOT EDIT.

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
    write_if_changed(&out_dir.join("plugin-tabs.ts"), &body)
}

fn emit_ts_command_metadata(
    manifests: &[PluginManifest],
    out_dir: &Path,
) -> Result<(), CodegenError> {
    let mut plugins = String::new();
    for m in manifests {
        let mut commands = String::new();
        for cmd in &m.file.commands {
            let perms = cmd
                .permissions
                .iter()
                .map(|p| format!("{:?}", p))
                .collect::<Vec<_>>()
                .join(", ");
            commands.push_str(&format!(
                "    {name:?}: {{ permissions: [{perms}] }},\n",
                name = cmd.name,
                perms = perms,
            ));
        }
        plugins.push_str(&format!(
            "  {id:?}: {{\n{commands}  }},\n",
            id = m.plugin_id,
            commands = commands,
        ));
    }

    let body = format!(
        "// @generated by plugin-codegen from plugins/*/plugin.toml. DO NOT EDIT.
//
// Per-command permission metadata. The dispatcher (Rust) and TS wrappers
// share this single source so a permission change in plugin.toml flows
// through both sides on the next build.

export interface PluginCommandPermissionMetadata {{
  permissions: readonly string[];
}}

export const PLUGIN_COMMANDS: Readonly<
  Record<string, Readonly<Record<string, PluginCommandPermissionMetadata>>>
> = {{
{plugins}}};
"
    );
    write_if_changed(&out_dir.join("plugin-command-metadata.ts"), &body)
}

fn emit_ts_per_plugin_wrappers(
    manifests: &[PluginManifest],
    out_dir: &Path,
) -> Result<(), CodegenError> {
    let plugins_subdir = out_dir.join("plugins");
    fs::create_dir_all(&plugins_subdir).map_err(|source| CodegenError::Io {
        context: format!("create {}", plugins_subdir.display()),
        source,
    })?;

    for m in manifests {
        let mut commands_block = String::new();
        for cmd in &m.file.commands {
            let perms_doc = if cmd.permissions.is_empty() {
                String::from("// permissions: (none)")
            } else {
                format!("// permissions: {}", cmd.permissions.join(", "))
            };
            commands_block.push_str(&format!(
                "  {perms_doc}\n  async {name}(args: unknown, _capability: PluginCapability): Promise<unknown> {{\n    void args;\n    void _capability;\n    throw new Error(\"plugin command not yet wired: {plugin_id}.{name}\");\n  }},\n",
                perms_doc = perms_doc,
                name = cmd.name,
                plugin_id = m.plugin_id,
            ));
        }
        if commands_block.is_empty() {
            commands_block.push_str("  // (no commands declared in plugin.toml)\n");
        }

        let body = format!(
            "// @generated by plugin-codegen from plugins/{id}/plugin.toml. DO NOT EDIT.
//
// Round-2 typed-wrapper stub: real dispatch wiring lands with task4 (Rust IPC
// dispatcher). For now the wrappers throw so any accidental caller surfaces
// loudly during integration testing.

import type {{ ComponentType }} from \"react\";

export interface PluginCapability {{
  readonly _branded: unique symbol;
}}

export const pluginMeta = {{
  pluginId: {id:?},
  name: {name:?},
  version: {version:?},
  permissions: [{perms}] as const,
}};

export const commands = {{
{commands_block}}};

const PluginStub: ComponentType = function PluginStub() {{
  return null;
}};
(PluginStub as {{ displayName?: string }}).displayName = {display_name:?};

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
            commands_block = commands_block,
            display_name = format!("Plugin({})", m.plugin_id),
        );
        write_if_changed(&plugins_subdir.join(format!("{}.ts", m.plugin_id)), &body)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_manifest(plugin_id: &str, file: ManifestFile) -> PluginManifest {
        PluginManifest {
            plugin_id: plugin_id.to_string(),
            manifest_path: PathBuf::from(format!("plugins/{plugin_id}/plugin.toml")),
            file,
        }
    }

    fn mk_file(
        name: &str,
        bin: &str,
        ns: &str,
        permissions: &[&str],
        apis: &[&str],
        commands: Vec<CommandDecl>,
    ) -> ManifestFile {
        ManifestFile {
            name: name.into(),
            version: "0.0.1".into(),
            plugin_type: "tab".into(),
            command_bin: bin.into(),
            frontend: "frontend/index.tsx".into(),
            permissions: permissions.iter().map(|s| (*s).to_string()).collect(),
            required_apis: apis.iter().map(|s| (*s).to_string()).collect(),
            db_namespace: ns.into(),
            migrations_path: "migrations/".into(),
            commands,
        }
    }

    fn tmp_out() -> (tempfile::TempDir, OutputPaths) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let out = OutputPaths {
            rust_generated_dir: dir.path().join("rust"),
            ts_generated_dir: dir.path().join("ts"),
        };
        (dir, out)
    }

    #[test]
    fn happy_path_two_plugins() {
        let (_tmp, out) = tmp_out();
        let manifests = vec![
            mk_manifest(
                "alpha",
                mk_file("Alpha", "alpha-bin", "alpha", &["notify"], &["invoke"], vec![
                    CommandDecl { name: "ping".into(), permissions: vec!["notify".into()] },
                ]),
            ),
            mk_manifest(
                "beta",
                mk_file("Beta", "beta-bin", "beta", &["notify"], &["invoke", "listen"], vec![]),
            ),
        ];
        generate_from_manifests(&manifests, &out).expect("happy path generates");
        let rust_reg = fs::read_to_string(out.rust_generated_dir.join("plugin_registry.rs")).unwrap();
        assert!(rust_reg.contains("plugin_id: \"alpha\""));
        assert!(rust_reg.contains("plugin_id: \"beta\""));
        let ts_tabs = fs::read_to_string(out.ts_generated_dir.join("plugin-tabs.ts")).unwrap();
        assert!(ts_tabs.contains("pluginId: \"alpha\""));
        let rust_cmds = fs::read_to_string(out.rust_generated_dir.join("plugin_commands.rs")).unwrap();
        assert!(rust_cmds.contains("plugin_id: \"alpha\""));
        assert!(rust_cmds.contains("name: \"ping\""));
    }

    #[test]
    fn missing_required_field_fails() {
        let (_tmp, out) = tmp_out();
        let mut file = mk_file("Alpha", "alpha-bin", "alpha", &[], &["invoke"], vec![]);
        file.name = String::new();
        let manifests = vec![mk_manifest("alpha", file)];
        match generate_from_manifests(&manifests, &out).unwrap_err() {
            CodegenError::MissingRequiredField { field, .. } => assert_eq!(field, "name"),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn unknown_required_api_fails() {
        let (_tmp, out) = tmp_out();
        let manifests = vec![mk_manifest(
            "alpha",
            mk_file(
                "Alpha",
                "alpha-bin",
                "alpha",
                &["notify"],
                &["definitely-not-a-real-api"],
                vec![],
            ),
        )];
        match generate_from_manifests(&manifests, &out).unwrap_err() {
            CodegenError::UnknownRequiredApi { value, .. } => {
                assert_eq!(value, "definitely-not-a-real-api");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn duplicate_plugin_id_fails_dual_named() {
        let (_tmp, out) = tmp_out();
        let manifests = vec![
            mk_manifest("alpha", mk_file("A1", "bin-a", "ns-a", &[], &["invoke"], vec![])),
            mk_manifest("alpha", mk_file("A2", "bin-b", "ns-b", &[], &["invoke"], vec![])),
        ];
        match generate_from_manifests(&manifests, &out).unwrap_err() {
            CodegenError::DuplicatePluginId { first, second, .. } => {
                assert!(first.display().to_string().contains("alpha"));
                assert!(second.display().to_string().contains("alpha"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn duplicate_db_namespace_fails_dual_named() {
        let (_tmp, out) = tmp_out();
        let manifests = vec![
            mk_manifest(
                "alpha",
                mk_file("A", "bin-a", "shared-ns", &[], &["invoke"], vec![]),
            ),
            mk_manifest(
                "beta",
                mk_file("B", "bin-b", "shared-ns", &[], &["invoke"], vec![]),
            ),
        ];
        match generate_from_manifests(&manifests, &out).unwrap_err() {
            CodegenError::DuplicateDbNamespace {
                first_id, second_id, namespace, ..
            } => {
                assert_eq!(namespace, "shared-ns");
                assert_eq!(first_id, "alpha");
                assert_eq!(second_id, "beta");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn duplicate_command_bin_fails_dual_named() {
        let (_tmp, out) = tmp_out();
        let manifests = vec![
            mk_manifest(
                "alpha",
                mk_file("A", "shared-bin", "ns-a", &[], &["invoke"], vec![]),
            ),
            mk_manifest(
                "beta",
                mk_file("B", "shared-bin", "ns-b", &[], &["invoke"], vec![]),
            ),
        ];
        match generate_from_manifests(&manifests, &out).unwrap_err() {
            CodegenError::DuplicateCommandBin {
                first_id, second_id, ..
            } => {
                assert_eq!(first_id, "alpha");
                assert_eq!(second_id, "beta");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn command_permission_outside_plugin_set_fails() {
        let (_tmp, out) = tmp_out();
        let manifests = vec![mk_manifest(
            "alpha",
            mk_file(
                "Alpha",
                "alpha-bin",
                "alpha",
                &["notify"],
                &["invoke"],
                vec![CommandDecl {
                    name: "do_stuff".into(),
                    permissions: vec!["user.email".into()],
                }],
            ),
        )];
        match generate_from_manifests(&manifests, &out).unwrap_err() {
            CodegenError::CommandPermissionOutOfScope {
                command_name, permission, ..
            } => {
                assert_eq!(command_name, "do_stuff");
                assert_eq!(permission, "user.email");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn empty_commands_array_is_ok() {
        let (_tmp, out) = tmp_out();
        let manifests = vec![mk_manifest(
            "alpha",
            mk_file("Alpha", "alpha-bin", "alpha", &[], &["invoke"], vec![]),
        )];
        generate_from_manifests(&manifests, &out).expect("zero-commands plugin OK");
    }

    #[test]
    fn raw_invoke_detection_in_frontend_tree() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let src_root = dir.path().join("src");
        fs::create_dir_all(src_root.join("plugins")).unwrap();
        // Generated dir is ignored.
        fs::create_dir_all(src_root.join("generated")).unwrap();
        fs::write(
            src_root.join("generated").join("plugin-tabs.ts"),
            "// safe to mention invoke('plugin.foo.bar') inside generated\n",
        )
        .unwrap();
        // A normal source file with a raw invoke must be flagged.
        fs::write(
            src_root.join("plugins").join("Bad.tsx"),
            "import { invoke } from \"@tauri-apps/api/core\";\nawait invoke('plugin.example-notes.list_notes');\n",
        )
        .unwrap();
        match check_no_raw_invoke(&src_root).unwrap_err() {
            CodegenError::RawInvokeUsage { line, .. } => assert_eq!(line, 2),
            other => panic!("unexpected error: {other}"),
        }
    }
}
