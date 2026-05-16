# Plugin Contract Specification

## Overview

This document is the canonical contract for the AgentPlatform plugin pipeline.

It is read by:

- plugin authors implementing `plugins/<id>/plugin.toml`, Rust adapters, frontend components, and MCP sidecars
- build-time code generation tasks
- the Rust IPC dispatcher
- the frontend plugin tab/lifecycle layer
- the SQLite and Stronghold storage layers
- the sidecar lifecycle manager
- the privileged orchestrator tab implementation
- downstream implementation tasks that depend on this contract

The plugin contract constrains:

- manifest structure and validation
- plugin directory layout
- generated Rust and TypeScript artifacts
- IPC command declaration and permission metadata
- mandatory permission enforcement
- opaque `PluginCapability` issuance, rotation, and validation
- per-plugin SQLite storage
- Stronghold-backed secret storage
- frontend lifecycle hooks
- host UI sidecars versus per-claude sidecars
- log identity and mount identity
- MVP threat model and non-negotiable authoring rules

Locked architectural decisions:

- Desktop stack is Tauri v2 + Rust + React/TypeScript.
- Plugins are OS-process-isolated MCP server sidecars using stdio transport.
- Each `claude` instance fork-execs its own copy of each plugin sidecar: the N x M model.
- The host has one privileged orchestrator tab, fixed top-left.
- Per-tab workspaces live at `~/AgentPlatform/workspaces/<name>/`.
- Every plugin-mediated write is gated by the confirm-on-write modal.
- SQLite is plaintext.
- OAuth refresh tokens live in Stronghold.
- Access tokens live only in process memory.
- MVP isolation is capability-based inside a single Tauri WebView plus OS process boundaries for sidecars.

## Manifest Schema (`plugin.toml`)

Each plugin MUST provide a manifest at:

```text
plugins/<plugin_id>/plugin.toml
```

`plugin_id` is derived from the directory name `plugins/<plugin_id>`.

`plugin_id` is not user-facing display text. It is a stable machine identifier used for registry keys, database paths, log paths, sidecar identity, generated file names, and capability binding.

Required top-level schema:

```toml
name = "Human Readable Plugin Name"
version = "0.1.0"
type = "mcp-stdio"
command_bin = "target/release/agentplatform-plugin-example"
permissions = ["notify"]
requiredApis = ["mcp.stdio.v1"]
dbNamespace = "example"
migrations_path = "migrations"

[frontend]
entry = "frontend/src/index.tsx"
export = "default"
tab_title = "Example"
```

Required fields:

| Field | Type | Required | Validation |
| --- | --- | --- | --- |
| `name` | string | yes | Non-empty, max 80 Unicode scalar values, no leading/trailing whitespace |
| `version` | string | yes | Valid SemVer: `MAJOR.MINOR.PATCH`, prerelease/build metadata allowed |
| `type` | string enum | yes | MVP value MUST be exactly `mcp-stdio` |
| `command_bin` | string | yes | Non-empty relative path or executable name; absolute paths are forbidden |
| `permissions` | array of strings | yes | Every entry MUST be in the known permission enum; no duplicates |
| `requiredApis` | array of strings | yes | Non-empty; every entry MUST be a supported host API identifier |
| `dbNamespace` | string | yes | Stable identifier; regex `^[a-z][a-z0-9_]{1,62}$` |
| `migrations_path` | string | yes | Relative path under plugin directory; absolute paths and `..` are forbidden |
| `frontend` | table | yes | MUST contain required frontend fields |

Required `[frontend]` fields:

| Field | Type | Required | Validation |
| --- | --- | --- | --- |
| `entry` | string | yes | Relative path under plugin directory; absolute paths and `..` are forbidden |
| `export` | string | yes | `default` or valid JS export identifier |
| `tab_title` | string | yes | Non-empty, max 40 Unicode scalar values |

Optional top-level fields:

| Field | Type | Default | Validation |
| --- | --- | --- | --- |
| `description` | string | `""` | Max 500 Unicode scalar values |
| `authors` | array of strings | `[]` | No empty entries |
| `homepage` | string | unset | Valid `https://` URL |
| `license` | string | unset | SPDX license identifier preferred |
| `minHostVersion` | string | unset | Valid SemVer |
| `sidecar_env` | table string-to-string | `{}` | Keys MUST match `^[A-Z_][A-Z0-9_]*$`; values are literal strings only |
| `confirmWrites` | boolean | `true` | MUST NOT be set to `false` for commands that mutate state |
| `allowOrchestratorCrossTabRead` | boolean | `false` | If true, only orchestrator mounts may receive `cross_tab_read` |

Optional `[frontend]` fields:

| Field | Type | Default | Validation |
| --- | --- | --- | --- |
| `icon` | string | unset | Relative path under plugin directory |
| `route` | string | `/<plugin_id>` | Regex `^/[a-z0-9][a-z0-9/_-]*$` |
| `lazy_chunk_name` | string | `<plugin_id>` | Regex `^[a-z][a-z0-9_-]{1,62}$` |

Supported `type` values:

```text
mcp-stdio
```

Supported `requiredApis` MVP values:

```text
mcp.stdio.v1
plugin.ipc.v1
plugin.capability.v1
plugin.sqlite.v1
plugin.stronghold.v1
plugin.lifecycle.v1
```

`command_bin` resolution rules:

- If `command_bin` contains `/`, it is resolved relative to the plugin directory.
- If `command_bin` does not contain `/`, it is resolved through the host sidecar executable lookup path.
- `command_bin` MUST NOT be absolute.
- `command_bin` MUST NOT contain `..`.
- `command_bin` MUST NOT be empty.
- The normalized `command_bin` value MUST be unique across all plugins.

`migrations_path` resolution rules:

- Resolved relative to `plugins/<plugin_id>/`.
- MUST point to a directory.
- Directory may be empty for plugins with no migrations.
- Absolute paths are forbidden.
- `..` segments are forbidden.

Uniqueness invariants:

- No duplicate `plugin_id`.
- No duplicate `dbNamespace`.
- No duplicate normalized `command_bin`.

Because `plugin_id` is derived from the directory name, duplicate `plugin_id` detection includes:

- duplicate physical plugin directories after path canonicalization
- case-insensitive collisions on macOS filesystems
- generated artifact name collisions after TypeScript/Rust identifier normalization

Identifier validation:

```text
plugin_id:    ^[a-z][a-z0-9_-]{1,62}$
dbNamespace: ^[a-z][a-z0-9_]{1,62}$
```

`plugin_id` SHOULD use kebab-case.

`dbNamespace` SHOULD use snake_case.

Build-time validation rules:

- Build MUST fail if any manifest is missing.
- Build MUST fail if any required field is missing.
- Build MUST fail if any field has the wrong type.
- Build MUST fail if any enum value is unsupported.
- Build MUST fail if any path escapes the plugin directory.
- Build MUST fail if any permission string is unknown.
- Build MUST fail if permissions contain duplicates.
- Build MUST fail if `requiredApis` contains unsupported values.
- Build MUST fail on duplicate `plugin_id`, `dbNamespace`, or normalized `command_bin`.
- Build MUST fail if generated Rust identifiers or TypeScript identifiers collide.
- Build MUST fail if frontend entry path does not exist.
- Build MUST fail if migrations path does not exist.
- Build MUST fail if `confirmWrites = false` is used to bypass write confirmation.

Build error format:

```text
PLUGIN_CONTRACT_ERROR <code>
plugin: <plugin_id or "<unknown>">
manifest: <path>
field: <field path or "<manifest>">
message: <human-readable explanation>
```

Example:

```text
PLUGIN_CONTRACT_ERROR DUPLICATE_DB_NAMESPACE
plugin: calendar
manifest: plugins/calendar/plugin.toml
field: dbNamespace
message: dbNamespace "notes" is already used by plugin "notes"
```

Required error codes:

```text
MANIFEST_MISSING
MANIFEST_PARSE_FAILED
FIELD_MISSING
FIELD_TYPE_INVALID
FIELD_VALUE_INVALID
PATH_ESCAPE
UNKNOWN_PERMISSION
UNKNOWN_REQUIRED_API
DUPLICATE_PERMISSION
DUPLICATE_PLUGIN_ID
DUPLICATE_DB_NAMESPACE
DUPLICATE_COMMAND_BIN
GENERATED_IDENTIFIER_COLLISION
FRONTEND_ENTRY_MISSING
MIGRATIONS_PATH_MISSING
CONFIRM_WRITES_DISABLED
```

Example manifest:

```toml
name = "Notes"
version = "0.1.0"
type = "mcp-stdio"
command_bin = "target/release/agentplatform-plugin-notes"
permissions = [
  "notify",
  "storage.sqlite.read",
  "storage.sqlite.write",
  "confirm_write.request"
]
requiredApis = [
  "mcp.stdio.v1",
  "plugin.ipc.v1",
  "plugin.capability.v1",
  "plugin.sqlite.v1",
  "plugin.lifecycle.v1"
]
dbNamespace = "notes"
migrations_path = "migrations"
description = "Local notes plugin."
authors = ["AgentPlatform"]
license = "MIT"

[frontend]
entry = "frontend/src/index.tsx"
export = "default"
tab_title = "Notes"
icon = "frontend/assets/icon.svg"
route = "/notes"
lazy_chunk_name = "notes"
```

## Plugin Directory Layout

Canonical layout:

```text
plugins/<plugin_id>/
  plugin.toml
  README.md
  migrations/
    0001_initial.sql
    0002_add_indexes.sql
  adapter/
    Cargo.toml
    src/
      lib.rs
      commands.rs
      mcp.rs
      storage.rs
  frontend/
    package.json
    tsconfig.json
    src/
      index.tsx
      PluginRoot.tsx
      hooks.ts
      components/
      styles/
    assets/
      icon.svg
```

Required paths:

```text
plugins/<plugin_id>/plugin.toml
plugins/<plugin_id>/<migrations_path>/
plugins/<plugin_id>/<frontend.entry>
```

Rust adapter crate conventions:

- The adapter crate SHOULD live at `plugins/<plugin_id>/adapter`.
- The crate name SHOULD be `agentplatform-plugin-<plugin_id>`, with `-` preserved.
- Rust modules SHOULD keep command declarations in `src/commands.rs`.
- The adapter MUST expose MCP stdio server startup through its binary target.
- The adapter MUST use generated command registration metadata rather than duplicating permission metadata manually.

Frontend conventions:

- Frontend code SHOULD live under `plugins/<plugin_id>/frontend`.
- The frontend entry MUST export the React component named by `[frontend].export`.
- The default export SHOULD be a React component called `PluginRoot`.
- Plugin components MUST receive host-provided props only through the generated tab registry and lifecycle wrapper.
- Plugin components MUST obtain `PluginCapability` only through `usePluginCapability`.

Naming conventions:

- Directory `plugin_id`: kebab-case, stable, never display text.
- `dbNamespace`: snake_case, stable, never display text.
- Rust generated module identifiers derive from `plugin_id` by replacing `-` with `_`.
- TypeScript generated file names use `plugin_id` exactly.
- Log path segments use URL-safe escaped `client_id`.

Plugins MUST NOT place generated host artifacts under `plugins/<plugin_id>/`.

Generated host artifacts are owned by the root build pipeline.

## IPC Command/Event Schema (Rust source of truth)

Rust command declarations are the source of truth for IPC command names, permission requirements, generated dispatcher registry entries, generated TypeScript wrappers, and frontend documentation.

Command declaration macro form:

```rust
#[plugin_command(
    name = "notes.create",
    permissions = ["storage.sqlite.write", "confirm_write.request"]
)]
pub async fn create_note(
    ctx: PluginCommandContext,
    capability: PluginCapabilityInput,
    input: CreateNoteInput,
) -> Result<CreateNoteOutput, PluginCommandError> {
    // implementation
}
```

Rules:

- `name` MUST be globally unique.
- `name` MUST use reverse-domain-like plugin prefixing: `<plugin_id>.<verb_or_resource_action>`.
- `permissions` MUST be present.
- `permissions` MAY be an empty array only for explicitly public read-only commands.
- Every permission string MUST be in the known permission enum.
- Input and output types MUST implement `Serialize`, `DeserializeOwned`, and JSON Schema generation where required by codegen.
- Permissioned commands MUST include a `PluginCapabilityInput`.
- Write commands MUST include `confirm_write.request` in permissions.
- Command implementations MUST NOT perform their own permission bypass path.

Event declaration macro form:

```rust
#[plugin_event(name = "notes.changed")]
pub struct NotesChangedEvent {
    pub note_id: String,
}
```

Event rules:

- Event names MUST be globally unique.
- Events MUST be serializable.
- Events MUST NOT contain raw `PluginCapability` values or nonces.
- Events MUST NOT leak Stronghold secret material.
- Events crossing plugin boundaries MUST be routed through host-owned event APIs.

Generated artifact paths:

```text
src-tauri/src/generated/plugin_registry.rs
src-tauri/src/generated/plugin_permissions.rs
src-tauri/src/generated/plugin_commands.rs
src/generated/plugin-tabs.ts
src/generated/plugin-command-metadata.ts
src/generated/plugins/<plugin_id>.ts
```

Required generated Rust registry contents:

- plugin manifest registry
- command registry
- command-to-permissions mapping
- plugin-to-permissions mapping
- frontend tab metadata exported to Tauri
- sidecar launch metadata
- database namespace metadata
- migration path metadata

Required generated TypeScript wrapper contents:

- typed command functions
- typed input and output interfaces
- `PluginCapability` parameter where required
- docstring listing required permissions
- command name as an internal constant, not user-authored raw strings
- normalized typed error mapping

Single-source-of-truth principle:

- Rust annotations define command names and required permissions.
- Build codegen reads Rust annotations and manifests.
- The Rust dispatcher registry is generated from the annotations.
- TypeScript wrappers are generated from the same annotations.
- Wrapper docstrings are generated from the same permission metadata.
- Frontend tab registry is generated from manifest `[frontend]`.
- No manually maintained duplicate permission table is permitted.
- If a command annotation changes, dispatcher gating and TS wrapper docs MUST change in the same build.

Dispatcher invocation shape:

```rust
pub struct PluginInvokeEnvelope<T> {
    pub plugin_id: PluginId,
    pub mount_id: MountId,
    pub capability: Option<PluginCapabilityInput>,
    pub command: String,
    pub payload: T,
}
```

Typed IPC error taxonomy:

```rust
pub enum PluginCommandError {
    PermissionDenied {
        caller_plugin_id: String,
        missing_permission: String,
        requested_command: String,
    },
    CapabilityInvalid {
        caller_plugin_id: String,
        mount_id: String,
        requested_command: String,
    },
    CapabilityExpired {
        caller_plugin_id: String,
        mount_id: String,
        requested_command: String,
    },
    CapabilityMismatched {
        caller_plugin_id: String,
        mount_id: String,
        requested_command: String,
    },
    CapabilityMissing {
        caller_plugin_id: String,
        requested_command: String,
    },
    ManifestInvalid {
        plugin_id: String,
        message: String,
    },
    CommandUnknown {
        requested_command: String,
    },
    PayloadInvalid {
        requested_command: String,
        message: String,
    },
    ConfirmWriteRequired {
        caller_plugin_id: String,
        requested_command: String,
    },
    ConfirmWriteRejected {
        caller_plugin_id: String,
        requested_command: String,
    },
    SidecarUnavailable {
        plugin_id: String,
        client_id: String,
    },
    StorageError {
        plugin_id: String,
        message: String,
    },
    SecretStorageLocked {
        plugin_id: String,
    },
    Internal {
        message: String,
    },
}
```

Frontend error mapping MUST preserve:

- discriminant
- `caller_plugin_id`
- `mount_id`, when present
- `missing_permission`, when present
- `requested_command`
- user-display-safe message

Frontend error mapping MUST NOT expose:

- capability nonce
- Stronghold key material
- raw refresh tokens
- access tokens
- host filesystem paths except user-approved workspace paths

## PluginCapability Lifecycle

`PluginCapability` is an opaque branded handle issued by the host.

It is represented conceptually as:

```ts
declare const PluginCapabilityBrand: unique symbol;

export type PluginCapability = {
  readonly [PluginCapabilityBrand]: never;
};
```

Runtime contents:

- server-issued nonce
- at least 128 bits of cryptographically random entropy
- associated `plugin_id`
- associated `mount_id`
- issuance timestamp
- expiration metadata

The nonce MUST be generated by Rust using an OS cryptographic random source.

The nonce MUST NOT be generated in JavaScript.

The nonce MUST NOT be predictable, sequential, or derived from plugin identity.

Where it lives:

- Held only in the plugin's React component closure.
- Passed only to generated TypeScript wrappers.
- Never stored on `window`.
- Never stored in `globalThis`.
- Never stored in `localStorage`.
- Never stored in `sessionStorage`.
- Never stored in IndexedDB.
- Never stored in cookies.
- Never stored in URL params.
- Never emitted in user-facing serialized JSON.
- Never logged.
- Never sent to an MCP sidecar unless explicitly required by a host-owned protocol, and MVP does not require this.

Authoritative state lives in Rust `MountRegistry`.

Registry key:

```rust
(plugin_id, mount_id)
```

Registry value:

```rust
pub struct MountEntry {
    pub handle_nonce_hash: CapabilityNonceHash,
    pub permissions: BTreeSet<Permission>,
    pub cross_tab_read_flag: bool,
    pub issued_at: SystemTime,
    pub expires_at: Option<SystemTime>,
    pub generation: u64,
}
```

The registry MUST store a hash of the nonce, not the raw nonce, unless an implementation-specific secure comparison strategy requires otherwise.

`cross_tab_read_flag` rules:

- `true` is allowed only for the orchestrator mount.
- regular tab mounts MUST always have `cross_tab_read_flag = false`.
- manifest permission `cross_tab_read` is necessary but not sufficient for cross-tab reads.
- dispatcher MUST also verify orchestrator mount identity.

Issuance flow:

1. Plugin frontend component mounts.
2. Host lifecycle wrapper calls `usePluginCapability(plugin_id, mount_id)`.
3. React hook invokes Tauri IPC command `plugin.capability.issue`.
4. Rust host generates a new nonce.
5. Rust host registers `(plugin_id, mount_id)` in `MountRegistry`.
6. Rust host binds manifest-declared permissions to the mount entry.
7. Rust host sets `cross_tab_read_flag` only if mount is the orchestrator and manifest permits it.
8. Rust host returns an opaque branded handle to React.
9. React stores the handle only in component closure state.
10. Generated TS wrappers require that handle for permissioned IPC.

Rotation rules:

- On unmount, the host MUST invalidate the existing mount entry.
- On remount, the host MUST issue a new nonce.
- The old nonce MUST NOT become valid again.
- `mount_id` MAY be reused for the logical mount, but nonce generation MUST advance.
- `generation` MUST increase on every issuance for the same `(plugin_id, mount_id)`.
- Expired entries MUST be cleaned by the `MountRegistry`.
- Cleanup MAY be opportunistic during validation and lifecycle events.

Unmount flow:

1. React component unmounts.
2. Host lifecycle wrapper calls `plugin.capability.revoke`.
3. Rust host removes or expires `(plugin_id, mount_id)`.
4. Any later IPC using the old handle is rejected.

Validation flow:

1. Dispatcher receives invoke envelope.
2. Dispatcher identifies `requested_command`.
3. Dispatcher loads generated required permissions for command.
4. Dispatcher verifies capability is present if command requires permissions.
5. Dispatcher looks up `(plugin_id, mount_id)` in `MountRegistry`.
6. Dispatcher compares nonce hash in constant time.
7. Dispatcher verifies handle generation is current.
8. Dispatcher verifies every command permission is included in mount permissions.
9. Dispatcher applies special-case checks such as orchestrator-only `cross_tab_read`.
10. Dispatcher either calls the command or returns a typed error.

Failure modes:

- No handle for permissioned command: `CapabilityMissing`.
- Handle nonce malformed: `CapabilityInvalid`.
- Registry entry absent or expired: `CapabilityExpired`.
- Handle plugin or mount does not match envelope: `CapabilityMismatched`.
- Handle valid but lacks required permission: `PermissionDenied`.

Race safety:

- Dispatcher validation MUST operate against an atomic snapshot of the `MountRegistry`.
- In-flight IPC during rotation MUST either:
  - complete against the pre-rotation snapshot, or
  - reject with `CapabilityExpired`.
- No IPC may observe a half-updated mount entry.
- No IPC may combine old nonce with new permissions.
- No IPC may combine new nonce with old permissions.
- Registry updates MUST be atomic per `(plugin_id, mount_id)`.

## Permission Gating (Mandatory)

Permission gating is mandatory at the Rust IPC dispatcher boundary.

There is no warning-only mode.

There is no frontend-only permission enforcement.

There is no plugin-author opt-out.

Every generated wrapper is convenience only; the Rust dispatcher is the enforcement lower bound.

MVP permission enum:

```text
notify
user.email
user.profile.read

workspace.read
workspace.write
workspace.list

storage.sqlite.read
storage.sqlite.write

secret.read
secret.write
secret.delete

oauth.start
oauth.refresh
oauth.revoke

cross_tab_read

pty.spawn
pty.read_scrollback
pty.write_stdin
pty.kill

sidecar.spawn
sidecar.stop
sidecar.restart

mcp.call_tool
mcp.read_resource
mcp.subscribe

clipboard.read
clipboard.write

external.open_url
external.fetch

confirm_write.request
```

Permission meanings:

| Permission | Meaning |
| --- | --- |
| `notify` | Show host-mediated user notifications |
| `user.email` | Read user email identity from approved account metadata |
| `user.profile.read` | Read approved user profile metadata |
| `workspace.read` | Read files in the plugin's current workspace scope |
| `workspace.write` | Write files in the plugin's current workspace scope after confirmation |
| `workspace.list` | List workspace files |
| `storage.sqlite.read` | Read own plugin SQLite database |
| `storage.sqlite.write` | Write own plugin SQLite database |
| `secret.read` | Read own plugin Stronghold refresh-token material through host API |
| `secret.write` | Store own plugin Stronghold refresh-token material through host API |
| `secret.delete` | Delete own plugin Stronghold material |
| `oauth.start` | Start OAuth authorization |
| `oauth.refresh` | Refresh access token using Stronghold refresh token |
| `oauth.revoke` | Revoke OAuth token |
| `cross_tab_read` | Read cross-tab state, orchestrator mount only |
| `pty.spawn` | Spawn PTY process in approved workspace |
| `pty.read_scrollback` | Read PTY scrollback |
| `pty.write_stdin` | Write stdin to PTY |
| `pty.kill` | Terminate PTY process |
| `sidecar.spawn` | Start own plugin sidecar |
| `sidecar.stop` | Stop own plugin sidecar |
| `sidecar.restart` | Restart own plugin sidecar |
| `mcp.call_tool` | Call own sidecar MCP tool |
| `mcp.read_resource` | Read own sidecar MCP resource |
| `mcp.subscribe` | Subscribe to own sidecar MCP events/resources |
| `clipboard.read` | Read clipboard after host policy permits |
| `clipboard.write` | Write clipboard |
| `external.open_url` | Open URL through host-controlled external opener |
| `external.fetch` | Perform host-mediated network fetch if enabled |
| `confirm_write.request` | Request confirm-on-write modal for mutating operations |

Rules:

- Manifest declares the maximum permissions a plugin may receive.
- Command annotations declare the permissions required by each command.
- The dispatcher enforces both:
  - requested command permissions MUST be present in generated command metadata
  - requested command permissions MUST be included in the mount entry permissions
- Mount entry permissions MUST be derived from the manifest.
- A command MUST NOT acquire permission not declared by its manifest.
- A command MUST NOT bypass dispatcher permission validation.
- A write command MUST require `confirm_write.request`.
- A plugin-mediated write MUST display and pass the confirm-on-write modal before mutation.
- Denied permissions MUST return structured typed errors.

Structured permission error:

```rust
PluginCommandError::PermissionDenied {
    caller_plugin_id,
    missing_permission,
    requested_command,
}
```

Permission metadata generation:

```rust
#[plugin_command(
    name = "workspace.write_file",
    permissions = ["workspace.write", "confirm_write.request"]
)]
```

The same metadata MUST feed:

- Rust dispatcher gating
- TypeScript wrapper docstring
- TypeScript wrapper static metadata
- command registry debug output
- test fixtures for permission coverage

Generated TypeScript docstring example:

```ts
/**
 * Command: workspace.write_file
 * Required permissions:
 * - workspace.write
 * - confirm_write.request
 */
export async function writeFile(
  capability: PluginCapability,
  input: WriteFileInput
): Promise<WriteFileOutput>;
```

## Per-Plugin SQLite Database

Each plugin receives one plaintext SQLite database file:

```text
${APP_DATA}/plugins/<plugin_id>/state.sqlite
```

Rules:

- A plugin may open only its own database through host-provided APIs.
- Host provides no API to open another plugin's SQLite file.
- Cross-plugin SQL is forbidden.
- SQLite files are plaintext.
- Secrets MUST NOT be stored in SQLite.
- OAuth refresh tokens MUST live in Stronghold.
- OAuth access tokens MUST live only in process memory.
- Non-secret account metadata MAY live in SQLite.

Required connection settings:

```sql
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;
PRAGMA foreign_keys = ON;
```

WAL rationale:

- The N x M model may create multiple sibling sidecar processes for the same plugin.
- Sibling sidecars may need concurrent read access.
- WAL improves read/write concurrency for this process shape.

`busy_timeout=5000` rationale:

- Sibling sidecar processes may briefly contend on SQLite locks.
- A five-second timeout avoids immediate failure during normal contention.
- Longer blocking should surface as storage errors.

Migration path:

```text
plugins/<plugin_id>/<migrations_path>/
```

Migration lock file:

```text
${APP_DATA}/plugins/<plugin_id>/state.sqlite.migration-lock
```

Migration rules:

- Migrations MUST be ordered lexicographically.
- Migration files SHOULD use numeric prefixes: `0001_initial.sql`.
- Only one sibling sidecar copy may run migrations at a time.
- Migration execution MUST be guarded by advisory file lock.
- Other sibling sidecars MUST wait for the migration lock.
- Waiting sidecars MUST resume against the migrated schema.
- Migration state MUST be recorded in the SQLite database.
- Failed migration MUST leave a clear storage error and MUST NOT silently continue.

Migration algorithm:

1. Open or create plugin data directory.
2. Acquire advisory lock on `state.sqlite.migration-lock`.
3. Open SQLite connection.
4. Apply required pragmas.
5. Ensure migration metadata table exists.
6. Apply unapplied migrations inside transactions.
7. Release advisory lock.
8. Allow waiting sidecars to open normal connections.

Required migration metadata table:

```sql
CREATE TABLE IF NOT EXISTS _plugin_migrations (
  version TEXT PRIMARY KEY,
  filename TEXT NOT NULL,
  checksum TEXT NOT NULL,
  applied_at TEXT NOT NULL
);
```

Migrations MUST NOT access filesystem paths outside the plugin database.

Migrations MUST NOT attach other SQLite databases.

Use of `ATTACH DATABASE` is forbidden in plugin migrations.

## Stronghold Secret Storage

Stronghold stores OAuth refresh tokens and other long-lived plugin secrets.

Refresh token key shape:

```text
(plugin_id, account_id)
```

Canonical logical key:

```text
plugins/<plugin_id>/accounts/<account_id>/refresh_token
```

Rules:

- Refresh tokens MUST be persisted only in Stronghold.
- Refresh tokens MUST NOT be stored in SQLite.
- Refresh tokens MUST NOT be logged.
- Refresh tokens MUST NOT be sent to the frontend.
- Access tokens MUST stay in process memory.
- Access tokens MUST NOT be persisted.
- Access tokens SHOULD be refreshed on demand.
- Token APIs MUST be permission-gated with `secret.read`, `secret.write`, `secret.delete`, and OAuth permissions as appropriate.

Master-password setup state marker:

```text
${APP_DATA}/stronghold-state/setup.marker
```

Setup marker rules:

- Marker indicates that Stronghold setup has been initialized.
- Marker MUST NOT contain the master password.
- Marker MUST NOT contain token material.
- Marker MAY contain non-secret setup metadata needed for resume/reset UX.
- Missing marker means setup has not completed or was reset.

Resume/reset behavior:

- On resume, host checks `setup.marker`.
- If marker exists, host prompts for unlock path.
- If marker is missing, host prompts for setup path.
- Reset MUST remove Stronghold secret material only after explicit user confirmation.
- Reset MUST also trigger plugin account metadata cleanup where appropriate.

Account removal event:

1. User removes account.
2. Host deletes Stronghold refresh token for `(plugin_id, account_id)`.
3. Host clears in-memory access token.
4. Host emits account-removal event to plugin.
5. Plugin removes SQLite metadata for that account.
6. Plugin UI transitions to empty or disconnected state.

SQLite metadata cleanup:

- Plugin MUST remove account rows associated with the removed account.
- Plugin MUST NOT assume token deletion failed merely because metadata still exists.
- Host token deletion is authoritative for secret removal.

## Lifecycle Hooks (Frontend)

Each plugin frontend is mounted by the host lifecycle wrapper.

React component contract:

```ts
export interface PluginRootProps {
  pluginId: string;
  mountId: string;
  clientId: string;
}
```

Required lifecycle behavior:

- `onMount`: issue capability.
- `onUnmount`: revoke or rotate capability.
- `onError`: host error boundary catches error and renders plugin-owned error state.

Canonical hook flow:

```ts
function PluginRoot(props: PluginRootProps) {
  const capability = usePluginCapability(props.pluginId, props.mountId);

  if (capability.status === "loading") {
    return <LoadingState />;
  }

  if (capability.status === "error") {
    return <ErrorState error={capability.error} />;
  }

  return <PluginApp capability={capability.value} />;
}
```

`usePluginCapability` responsibilities:

- call host IPC to issue capability on mount
- hold returned handle in React closure state
- revoke or expire handle on unmount
- never expose handle globally
- never serialize handle
- return typed loading, ready, and error states
- retry only through host-approved lifecycle policy

Required UI states:

- empty-state
- loading-state
- error-state

Empty-state requirements:

- Render when the plugin has no account, no data, or no selected resource.
- Provide plugin-owned action controls where appropriate.
- MUST NOT require host to invent plugin-specific empty UI.

Loading-state requirements:

- Render while capability issuance, sidecar startup, or initial data load is pending.
- MUST avoid layout shift where practical.
- MUST not display stale secret data.

Error-state requirements:

- Render when plugin initialization or command invocation fails.
- MUST accept typed host errors.
- MUST NOT display raw capability nonce.
- MUST NOT display raw refresh tokens or access tokens.
- SHOULD provide retry or reconnect controls when meaningful.

Host error boundary:

- catches render errors from plugin component
- records plugin id and mount id
- renders the plugin's own error state when possible
- never logs capability nonce
- never grants replacement capability without normal issuance flow

## Sidecar Identity (host-UI vs per-claude)

There are two sidecar client identity classes.

Host UI client id:

```text
host_ui:<plugin_id>
```

Per-claude client id:

```text
claude:<tab_id>:<plugin_id>
```

Rules:

- The host UI MAY start one sidecar per plugin for host-owned UI interactions.
- Each `claude` tab MUST fork-exec its own sidecar copy per plugin it uses.
- Sidecars are OS processes.
- Sidecars communicate with the host using MCP stdio.
- Sidecars MUST NOT share stdio pipes.
- Sidecar process identity MUST include `client_id`.
- Sidecar lifecycle manager MUST key process supervision by `(plugin_id, client_id)`.

Distinct log file path per `client_id`:

```text
${APP_DATA}/logs/<plugin_id>/<client_id>.log
```

Because `client_id` contains `:`, filesystem storage MUST use a URL-safe or otherwise reversible escaped filename.

Example escaped paths:

```text
${APP_DATA}/logs/notes/host_ui%3Anotes.log
${APP_DATA}/logs/notes/claude%3Atab-123%3Anotes.log
```

Mount identity rules:

- Host UI mount and claude tab mounts are distinct.
- Orchestrator mount is distinct from regular tab mounts.
- Each mount receives its own `PluginCapability`.
- Each mount has its own `MountRegistry` entry.
- Orchestrator mount may have `cross_tab_read_flag = true`.
- Regular tab mounts MUST never have `cross_tab_read_flag = true`.

Orchestrator rules:

- The orchestrator tab is single and privileged.
- The orchestrator tab is fixed top-left in the UI.
- Only the orchestrator may receive cross-tab read authority.
- Orchestrator cross-tab read still requires:
  - manifest permission `cross_tab_read`
  - command permission `cross_tab_read`
  - mount flag `cross_tab_read_flag = true`
  - valid capability bound to orchestrator mount

Per-claude rules:

- Each claude tab has its own workspace at `~/AgentPlatform/workspaces/<name>/`.
- A claude sidecar must operate in the scope of its tab workspace.
- A claude sidecar must not read another tab workspace unless host policy explicitly allows it through orchestrator-only APIs.
- A claude sidecar must not inherit host UI capability.

## Code-gen Pipeline (build.rs Output Contract)

The root build pipeline scans:

```text
plugins/*/plugin.toml
```

The build pipeline emits generated artifacts to gitignored paths.

Required outputs:

```text
src-tauri/src/generated/plugin_registry.rs
src-tauri/src/generated/plugin_permissions.rs
src-tauri/src/generated/plugin_commands.rs
src/generated/plugin-tabs.ts
src/generated/plugin-command-metadata.ts
src/generated/plugins/<plugin_id>.ts
```

Minimum required artifact contract:

`src-tauri/src/generated/plugin_registry.rs` MUST include:

- static plugin registry
- plugin manifest metadata
- normalized `plugin_id`
- display `name`
- `version`
- `type`
- resolved `command_bin`
- declared permissions
- required APIs
- database namespace
- migrations path
- frontend metadata needed by Tauri commands
- sidecar launch metadata

`src-tauri/src/generated/plugin_permissions.rs` MUST include:

- permission enum
- permission parsing
- permission display names
- manifest permission validation helpers

`src-tauri/src/generated/plugin_commands.rs` MUST include:

- command registry
- command-to-permissions mapping
- command dispatch metadata
- typed command descriptors where available

`src/generated/plugin-tabs.ts` MUST include:

- frontend tab registry
- lazy imports for plugin frontend entries
- tab title
- plugin id
- route
- icon path, if present
- orchestrator visibility metadata where applicable

`src/generated/plugin-command-metadata.ts` MUST include:

- command names
- required permissions
- plugin ownership
- frontend-safe metadata for documentation and debugging

`src/generated/plugins/<plugin_id>.ts` MUST include:

- typed wrappers for that plugin's commands
- typed input/output interfaces
- required `PluginCapability` arguments
- generated docstrings with permissions
- normalized typed error exports

Clean-on-build semantics:

- Generated directories MUST be cleaned before emission.
- Stale generated files MUST NOT survive a build.
- Build MUST write all generated files atomically where practical.
- A failed build MUST NOT leave partially generated files that appear valid.
- Generated files MUST be gitignored.
- Source manifests and Rust annotations are the authoritative inputs.

Validation order:

1. Discover plugin directories.
2. Parse manifests.
3. Validate manifest schema.
4. Validate path containment.
5. Validate uniqueness invariants.
6. Scan Rust command annotations.
7. Validate command names and permissions.
8. Validate command ownership by plugin prefix.
9. Build in-memory registry.
10. Clean generated output directories.
11. Emit Rust artifacts.
12. Emit TypeScript artifacts.

Build failure requirements:

- Build fails on manifest schema errors.
- Build fails on uniqueness errors.
- Build fails on command annotation errors.
- Build fails on unknown permissions.
- Build fails on generated identifier collisions.
- Build error MUST name the offending plugin when known.
- Build error MUST use `PLUGIN_CONTRACT_ERROR <code>` format for manifest/contract failures.

No stale behavior:

- Removing a plugin directory MUST remove its generated wrapper on next build.
- Renaming a plugin id MUST remove the old generated wrapper on next build.
- Removing a command annotation MUST remove that command from generated metadata on next build.
- Permission changes MUST update dispatcher metadata and TS docstrings in the same build.

## Threat Model

MVP WebView model:

- MVP uses a single Tauri WebView.
- Plugin frontend isolation inside that WebView relies on `PluginCapability` not being globally exposed.
- Rust-side dispatcher binding by `(plugin_id, mount_id)` is mandatory.
- Frontend TypeScript wrappers are not a security boundary.
- Rust dispatcher validation is the security boundary for IPC commands.

MVP process model:

- Plugin sidecars are OS processes.
- OS process boundary is the only MVP sidecar sandbox.
- No macOS App Sandbox is required for MVP.
- No `sandbox-exec` profile is required for MVP.
- Sidecars are supervised by the host lifecycle manager.
- Sidecars communicate over MCP stdio.

Capability risks:

- If a plugin leaks its capability to global JavaScript state, other code in the WebView may attempt to use it.
- This is why plugins MUST keep capability handles in React closure state only.
- Dispatcher still validates `(plugin_id, mount_id)` and nonce on every permissioned IPC.
- Capability rotation limits lifetime after unmount/remount.

Malicious content risk:

- Plugin-vs-malicious-content is plugin-author responsibility in MVP.
- Examples include HTML email bodies, remote Markdown, issue descriptions, calendar invite HTML, and untrusted web content.
- Plugins rendering untrusted content MUST use sandboxed sub-frames or equivalent containment.
- Plugins MUST NOT inject untrusted HTML directly into privileged React DOM.
- Plugins MUST sanitize untrusted rich text before display.

Storage risks:

- SQLite is plaintext.
- SQLite MUST NOT contain refresh tokens or long-lived secrets.
- Stronghold protects refresh tokens.
- Access tokens are memory-only and may be lost on process restart.

Write risks:

- Every plugin-mediated write MUST go through confirm-on-write.
- The dispatcher MUST enforce `confirm_write.request`.
- Plugins MUST NOT implement hidden write paths.

Phase-2 hardening:

- Per-plugin WebViews are Phase-2 hardening.
- Tighter OS sandboxing is Phase-2 hardening.
- Stronger content isolation for plugin-rendered untrusted content may be added in Phase 2.
- This MVP contract MUST remain compatible with later per-plugin WebViews.

## Normative DOs and DON'Ts

Plugin authors MUST:

- provide `plugins/<plugin_id>/plugin.toml`
- keep `plugin_id` stable after release
- declare all required permissions in the manifest
- declare all required host APIs in `requiredApis`
- keep `dbNamespace` unique and stable
- implement frontend `empty-state`, `loading-state`, and `error-state`
- obtain `PluginCapability` using `usePluginCapability`
- pass `PluginCapability` only to generated TypeScript wrappers
- render untrusted remote content in a sandboxed sub-frame or sanitize it first
- store refresh tokens only through Stronghold APIs
- store non-secret local state only in the plugin's own SQLite database
- include `confirm_write.request` for write commands
- use host confirm-on-write flow for plugin-mediated writes
- keep sidecar logs free of secrets and capability nonces
- scope file operations to the approved workspace
- treat generated Rust and TypeScript artifacts as read-only build outputs

Plugin authors MAY:

- store non-secret account metadata in SQLite
- define multiple frontend components under the plugin frontend directory
- use migrations for schema evolution
- use plugin-owned MCP tools and resources through the host sidecar manager
- show user notifications when `notify` is granted
- request OAuth flows when OAuth permissions are granted
- implement richer plugin-specific retry UX in error states
- use `cross_tab_read` only when building orchestrator-approved behavior

Plugin authors MUST NOT:

- write raw `invoke("plugin...")` command strings by hand
- bypass generated TypeScript wrappers for permissioned commands
- expose `PluginCapability` on `window`
- expose `PluginCapability` on `globalThis`
- store `PluginCapability` in `localStorage`
- store `PluginCapability` in `sessionStorage`
- store `PluginCapability` in IndexedDB
- store `PluginCapability` in cookies
- serialize `PluginCapability` into user-facing JSON
- log capability nonces
- copy capabilities between mounts
- reuse capabilities after unmount
- send capabilities to untrusted content
- store refresh tokens in SQLite
- store access tokens on disk
- log refresh tokens or access tokens
- open another plugin's SQLite database
- use `ATTACH DATABASE` to reach another plugin database
- perform plugin-mediated writes without confirm-on-write
- rely on frontend checks as the only permission enforcement
- assume a regular tab can read cross-tab state
- assume host UI sidecar identity is interchangeable with per-claude sidecar identity
- share stdio pipes between sidecar processes
- mutate generated files manually
- depend on stale generated files surviving a build
- use absolute paths in manifest-controlled plugin paths
- use `..` path traversal in manifest-controlled plugin paths
