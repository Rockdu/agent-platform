import { Suspense, lazy, useCallback, useEffect, useMemo, useState, type ComponentType, type LazyExoticComponent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { PLUGIN_TABS, type PluginTabEntry } from "./generated/plugin-tabs";
import type {
  PluginMigrationStatus,
  PluginMigrationStatusMap,
} from "./migration-status";
import { PluginRoot } from "./plugin-lifecycle";
import "./App.css";

// Mirrors src-tauri/src/bootstrap.rs::BootstrapPaths
interface BootstrapPaths {
  agent_platform: string;
  workspaces: string;
  plugins_root: string;
  logs: string;
  claude_mcp_configs: string;
}

// Mirrors src-tauri/src/lib.rs::BootstrapErrorDto
interface BootstrapErrorDto {
  kind: string;
  path: string | null;
  message: string;
}

type BootstrapState =
  | { kind: "loading" }
  | { kind: "ok"; paths: BootstrapPaths }
  | { kind: "error"; error: BootstrapErrorDto };

// Host-owned tab identifiers. The orchestrator is host-owned and fixed
// leftmost (not a plugin per the contract). Regular plugin tabs come from
// PLUGIN_TABS (generated from plugins/*/plugin.toml).
type HostTabId = "orchestrator";
type ActiveTab = { kind: "host"; id: HostTabId } | { kind: "plugin"; pluginId: string };

const ORCHESTRATOR: { id: HostTabId; label: string; icon: string } = {
  id: "orchestrator",
  label: "Orchestrator",
  icon: "✦",
};

// MVP plugins not yet packaged as plugin.toml entries. These vanish when the
// matching real manifest lands under plugins/.
type StaticPlaceholder = { id: string; label: string; icon: string };
const STATIC_PLACEHOLDERS: StaticPlaceholder[] = [
  { id: "static-terminal", label: "终端", icon: "▤" },
  { id: "static-gmail", label: "邮件", icon: "✉" },
  { id: "static-papers", label: "论文", icon: "❡" },
];

// Cache React.lazy components per pluginId so toggling between tabs doesn't
// retrigger module evaluation.
const lazyComponentCache = new Map<string, LazyExoticComponent<ComponentType>>();
function lazyForPlugin(entry: PluginTabEntry): LazyExoticComponent<ComponentType> {
  let cached = lazyComponentCache.get(entry.pluginId);
  if (!cached) {
    cached = lazy(entry.loadComponent);
    lazyComponentCache.set(entry.pluginId, cached);
  }
  return cached;
}

export default function App() {
  const [active, setActive] = useState<ActiveTab>({ kind: "host", id: "orchestrator" });
  const [bootstrap, setBootstrap] = useState<BootstrapState>({ kind: "loading" });
  const [migrationStatus, setMigrationStatus] = useState<PluginMigrationStatusMap>({});

  const refreshMigrationStatus = useCallback(async () => {
    try {
      const map = await invoke<PluginMigrationStatusMap>("plugin_migration_status");
      setMigrationStatus(map);
    } catch {
      // First-launch race: bootstrap may not have set the state yet; the
      // map stays empty and plugin tabs render normally.
      setMigrationStatus({});
    }
  }, []);

  const retryMigration = useCallback(
    async (pluginId: string) => {
      try {
        const status = await invoke<PluginMigrationStatus>("retry_plugin_migration", {
          pluginId,
        });
        setMigrationStatus((prev) => ({ ...prev, [pluginId]: status }));
      } catch (err) {
        // Tauri-typed `Err` arms cross the boundary as throws; the payload
        // is the same `PluginMigrationStatus` shape so the panel updates
        // uniformly with the latest failure.
        if (isMigrationStatus(err)) {
          setMigrationStatus((prev) => ({ ...prev, [pluginId]: err }));
        }
      }
    },
    [],
  );

  // Hide a placeholder if a real plugin label contains the placeholder's
  // keyword (suppress duplicates when MVP plugins start shipping as
  // manifests). Round-3 expedient; cleaner deletion once MVP plugins land.
  const visiblePlaceholders = useMemo(() => {
    const lower = PLUGIN_TABS.map((p) => p.label.toLowerCase());
    return STATIC_PLACEHOLDERS.filter((p) => {
      const key = p.label.toLowerCase();
      return !lower.some((l) => l.includes(key));
    });
  }, []);

  useEffect(() => {
    invoke<BootstrapPaths>("bootstrap_status")
      .then((paths) => {
        setBootstrap({ kind: "ok", paths });
        // Bootstrap success means the setup hook has already kicked off
        // per-plugin migrations; fetch the resulting status map.
        return refreshMigrationStatus();
      })
      .catch((err: unknown) => {
        if (isBootstrapErrorDto(err)) {
          setBootstrap({ kind: "error", error: err });
        } else {
          setBootstrap({
            kind: "error",
            error: {
              kind: "unknown",
              path: null,
              message: typeof err === "string" ? err : JSON.stringify(err),
            },
          });
        }
      });
  }, [refreshMigrationStatus]);

  return (
    <div className="app">
      <nav className="tab-strip" role="tablist">
        <TabButton
          icon={ORCHESTRATOR.icon}
          label={ORCHESTRATOR.label}
          privileged
          isActive={active.kind === "host" && active.id === "orchestrator"}
          onClick={() => setActive({ kind: "host", id: "orchestrator" })}
        />
        {visiblePlaceholders.map((p) => (
          <TabButton
            key={p.id}
            icon={p.icon}
            label={p.label}
            privileged={false}
            isActive={active.kind === "plugin" && active.pluginId === p.id}
            onClick={() => setActive({ kind: "plugin", pluginId: p.id })}
          />
        ))}
        {PLUGIN_TABS.map((tab) => (
          <TabButton
            key={tab.pluginId}
            icon="❖"
            label={tab.label}
            privileged={false}
            isActive={active.kind === "plugin" && active.pluginId === tab.pluginId}
            onClick={() => setActive({ kind: "plugin", pluginId: tab.pluginId })}
          />
        ))}
      </nav>

      <main className="tab-body">
        {active.kind === "host" && <OrchestratorPlaceholder />}
        {active.kind === "plugin" && (
          <PluginBody
            pluginId={active.pluginId}
            registryEntry={lookupPlugin(active.pluginId)}
            migrationStatus={migrationStatus[active.pluginId]}
            onRetryMigration={retryMigration}
          />
        )}
      </main>

      <BootstrapDebugCard state={bootstrap} />
    </div>
  );
}

function isBootstrapErrorDto(value: unknown): value is BootstrapErrorDto {
  return (
    typeof value === "object" &&
    value !== null &&
    "kind" in value &&
    "message" in value &&
    typeof (value as { kind: unknown }).kind === "string"
  );
}

function isMigrationStatus(value: unknown): value is PluginMigrationStatus {
  if (typeof value !== "object" || value === null) return false;
  const tag = (value as { kind?: unknown }).kind;
  if (tag === "ok") return Array.isArray((value as { applied?: unknown }).applied);
  if (tag === "error") {
    return (
      typeof (value as { errorKind?: unknown }).errorKind === "string" &&
      typeof (value as { message?: unknown }).message === "string"
    );
  }
  return false;
}

function lookupPlugin(pluginId: string): PluginTabEntry | undefined {
  return PLUGIN_TABS.find((p) => p.pluginId === pluginId);
}

function TabButton(props: {
  icon: string;
  label: string;
  privileged: boolean;
  isActive: boolean;
  onClick: () => void;
}) {
  return (
    <button
      role="tab"
      aria-selected={props.isActive}
      className={`tab ${props.privileged ? "tab--privileged" : ""} ${
        props.isActive ? "tab--active" : ""
      }`}
      onClick={props.onClick}
    >
      <span className="tab__icon" aria-hidden="true">
        {props.icon}
      </span>
      <span className="tab__label">{props.label}</span>
    </button>
  );
}

function OrchestratorPlaceholder() {
  return (
    <section className="placeholder">
      <h2>Orchestrator</h2>
      <p>
        特权单实例 tab。后续会自动启动 <code>claude</code> 并预配所有平台 MCP 服务器。
      </p>
      <p className="placeholder__hint">当前仅渲染占位；PTY + claude 接入留待后续实现。</p>
    </section>
  );
}

function PluginBody({
  pluginId,
  registryEntry,
  migrationStatus,
  onRetryMigration,
}: {
  pluginId: string;
  registryEntry: PluginTabEntry | undefined;
  migrationStatus: PluginMigrationStatus | undefined;
  onRetryMigration: (pluginId: string) => void;
}) {
  if (!registryEntry) {
    // Static placeholder (MVP plugin not yet packaged as a manifest).
    const label = STATIC_PLACEHOLDERS.find((p) => p.id === pluginId)?.label ?? pluginId;
    return (
      <section className="placeholder">
        <h2>{label}</h2>
        <p>插件占位。等该插件以 <code>plugin.toml</code> 形式落地后此面板将由生成的 wrapper 替换。</p>
      </section>
    );
  }
  // AC-9.3: failed migration blocks plugin component mounting; show the
  // typed error + retry button instead.
  if (migrationStatus && migrationStatus.kind === "error") {
    return (
      <MigrationFailurePanel
        pluginId={registryEntry.pluginId}
        label={registryEntry.label}
        status={migrationStatus}
        onRetry={onRetryMigration}
      />
    );
  }
  const LazyComponent = lazyForPlugin(registryEntry);
  // Round 10 (task7): wrap the plugin component in PluginRoot so it
  // mounts a capability on render and unmounts it when the tab closes /
  // remounts on tab change. tabId defaults to `tab-${pluginId}` — one
  // singleton tab per plugin in the MVP shell; task17 (workspace storage)
  // will replace this with real workspace tab IDs.
  return (
    <PluginRoot
      pluginId={registryEntry.pluginId}
      label={registryEntry.label}
      tabId={`tab-${registryEntry.pluginId}`}
    >
      <Suspense
        fallback={
          <section className="placeholder placeholder--loading">
            <h2>{registryEntry.label}</h2>
            <p>加载插件组件中…</p>
          </section>
        }
      >
        <LazyComponent />
      </Suspense>
    </PluginRoot>
  );
}

function MigrationFailurePanel({
  pluginId,
  label,
  status,
  onRetry,
}: {
  pluginId: string;
  label: string;
  status: Extract<PluginMigrationStatus, { kind: "error" }>;
  onRetry: (pluginId: string) => void;
}) {
  return (
    <section
      className="placeholder placeholder--error"
      role="alert"
      data-plugin-id={pluginId}
    >
      <h2>{label} · 迁移失败</h2>
      <dl>
        <dt>错误类型</dt>
        <dd>
          <code>{status.errorKind}</code>
        </dd>
        {status.file && (
          <>
            <dt>文件</dt>
            <dd>
              <code>{status.file}</code>
            </dd>
          </>
        )}
        <dt>消息</dt>
        <dd>{status.message}</dd>
      </dl>
      <p>
        该插件的 SQLite 迁移未成功，组件已被阻止挂载以避免对未就绪 schema 进行读写。
      </p>
      <button type="button" onClick={() => onRetry(pluginId)}>
        重试迁移
      </button>
    </section>
  );
}

function BootstrapDebugCard({ state }: { state: BootstrapState }) {
  if (state.kind === "loading") {
    return (
      <aside className="bootstrap-card bootstrap-card--loading" aria-live="polite">
        <h3>启动中…</h3>
        <p>正在调用 <code>bootstrap_status</code> 校验首启目录。</p>
      </aside>
    );
  }
  if (state.kind === "error") {
    const { kind, path, message } = state.error;
    return (
      <aside className="bootstrap-card bootstrap-card--error" aria-live="polite">
        <h3>首启失败</h3>
        <dl>
          <dt>类型</dt>
          <dd>
            <code>{kind}</code>
          </dd>
          {path && (
            <>
              <dt>路径</dt>
              <dd>
                <code>{path}</code>
              </dd>
            </>
          )}
          <dt>消息</dt>
          <dd>{message}</dd>
        </dl>
        <p className="bootstrap-card__hint">
          请检查路径是否可写；典型原因是上级目录权限或磁盘空间不足。
        </p>
      </aside>
    );
  }
  return (
    <aside className="bootstrap-card bootstrap-card--ok" aria-live="polite">
      <h3>首启完成</h3>
      <dl>
        <dt>~/AgentPlatform</dt>
        <dd>{state.paths.agent_platform}</dd>
        <dt>workspaces</dt>
        <dd>{state.paths.workspaces}</dd>
        <dt>plugins root</dt>
        <dd>{state.paths.plugins_root}</dd>
        <dt>logs</dt>
        <dd>{state.paths.logs}</dd>
        <dt>claude-mcp-configs</dt>
        <dd>{state.paths.claude_mcp_configs}</dd>
      </dl>
      <p className="bootstrap-card__hint">调试卡，后续由插件健康面板替换。</p>
    </aside>
  );
}
