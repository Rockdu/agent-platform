import { Suspense, lazy, useCallback, useEffect, useMemo, useState, type ComponentType, type FormEvent, type LazyExoticComponent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { PLUGIN_TABS, type PluginTabEntry } from "./generated/plugin-tabs";
import type {
  PluginMigrationStatus,
  PluginMigrationStatusMap,
} from "./migration-status";
import { PluginRoot } from "./plugin-lifecycle";
import type { SidecarBinaryDiagnostic } from "./dev-diagnostics";
import {
  classifySidecarState,
  hostUiClientId,
  type SidecarErrorDto,
  type SidecarStatusSnapshot,
} from "./sidecar-status";
import {
  spawnSidecarFromManifest,
  useSidecarStatus,
} from "./use-sidecar-status";
import {
  getClaudeDiscoveryStatus,
  isClaudeDiscoveryErrorDto,
  redoClaudeDiscovery,
  setClaudePathOverride,
  type ClaudeDiscoveryErrorDto,
  type ClaudeDiscoveryStatus,
  type ClaudePathRecord,
} from "./claude-discovery";
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
  const [diagnostics, setDiagnostics] = useState<SidecarBinaryDiagnostic[]>([]);

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

  const refreshDevDiagnostics = useCallback(async () => {
    try {
      const list = await invoke<SidecarBinaryDiagnostic[]>("dev_diagnostics_status");
      setDiagnostics(list);
    } catch {
      // Diagnostics are read-only inspection; an error here just hides
      // the dev card (production won't have the command at all if it's
      // ever feature-gated; today it always exists).
      setDiagnostics([]);
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
        // per-plugin migrations; fetch the resulting status map AND the
        // dev-diagnostics report. Diagnostics is independent of migration
        // status — a plugin can have a healthy migration but a missing
        // sidecar binary, or vice versa.
        return Promise.all([refreshMigrationStatus(), refreshDevDiagnostics()]);
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
  }, [refreshMigrationStatus, refreshDevDiagnostics]);

  const missingSidecars = useMemo(
    () => diagnostics.filter((d) => d.status === "missing"),
    [diagnostics],
  );

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
            diagnostics={diagnostics}
          />
        )}
      </main>

      {missingSidecars.length > 0 && (
        <DevDiagnosticsCard missing={missingSidecars} />
      )}
      <BootstrapDebugCard state={bootstrap} />
    </div>
  );
}

function DevDiagnosticsCard({ missing }: { missing: SidecarBinaryDiagnostic[] }) {
  // AC-9.2 positive-test phrase: the literal "缺失 plugin 二进制：" MUST
  // appear so the dev-diagnostics page is identifiable by string search
  // in tests and onboarding screenshots.
  const list = missing.map((d) => d.commandBin).join(", ");
  return (
    <aside
      className="bootstrap-card bootstrap-card--error"
      role="status"
      data-dev-diagnostics="missing-sidecars"
    >
      <h3>开发诊断 · 插件二进制未就绪</h3>
      <p>
        缺失 plugin 二进制：<code>{list}</code>
      </p>
      <p>请在 host 项目根目录执行以下命令构建对应 sidecar 后重启应用：</p>
      <pre className="bootstrap-card__cmd">
        {missing.map((d) => `cargo build --bin ${d.commandBin}`).join("\n")}
      </pre>
      <details>
        <summary>查看每个插件期望的二进制路径</summary>
        <dl>
          {missing.map((d) => (
            <div key={d.pluginId}>
              <dt>
                <code>{d.pluginId}</code>
              </dt>
              <dd>
                <ul>
                  {d.expectedPaths.map((p) => (
                    <li key={p}>
                      <code>{p}</code>
                    </li>
                  ))}
                </ul>
              </dd>
            </div>
          ))}
        </dl>
      </details>
      <p className="bootstrap-card__hint">
        正式构建（bundle）会包含所有 sidecar，该提示仅在 dev checkout 中出现。
      </p>
    </aside>
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
  const [status, setStatus] = useState<ClaudeDiscoveryStatus | null>(null);
  const [error, setError] = useState<ClaudeDiscoveryErrorDto | null>(null);
  const [overrideInput, setOverrideInput] = useState("");
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const s = await getClaudeDiscoveryStatus();
      setStatus(s);
      setError(null);
    } catch (err) {
      if (isClaudeDiscoveryErrorDto(err)) setError(err);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const onRedo = useCallback(async () => {
    setBusy(true);
    try {
      await redoClaudeDiscovery();
      await refresh();
    } catch (err) {
      if (isClaudeDiscoveryErrorDto(err)) setError(err);
    } finally {
      setBusy(false);
    }
  }, [refresh]);

  const onSubmitOverride = useCallback(
    async (event: FormEvent<HTMLFormElement>) => {
      event.preventDefault();
      const trimmed = overrideInput.trim();
      if (!trimmed) return;
      setBusy(true);
      try {
        await setClaudePathOverride(trimmed);
        await refresh();
        setOverrideInput("");
      } catch (err) {
        if (isClaudeDiscoveryErrorDto(err)) setError(err);
      } finally {
        setBusy(false);
      }
    },
    [overrideInput, refresh],
  );

  if (!status) {
    return (
      <section className="placeholder placeholder--loading">
        <h2>Orchestrator</h2>
        <p>正在检查 Claude Code 安装位置…</p>
      </section>
    );
  }

  if (status.kind === "ready") {
    return (
      <section className="placeholder">
        <h2>Orchestrator</h2>
        <p>
          特权单实例 tab。后续会自动启动 <code>claude</code> 并预配所有平台 MCP 服务器。
        </p>
        <p className="placeholder__hint">当前仅渲染占位；PTY + claude 接入留待后续实现。</p>
        <ClaudeReadyFooter record={status.record} onRedo={onRedo} busy={busy} />
      </section>
    );
  }

  // NotFound or NotRun → render the AC-3.2 onboarding card.
  const probed = status.kind === "not_found" ? status.probed : [];
  return (
    <ClaudeOnboardingCard
      probed={probed}
      error={error}
      overrideInput={overrideInput}
      setOverrideInput={setOverrideInput}
      onSubmitOverride={onSubmitOverride}
      onRedo={onRedo}
      busy={busy}
    />
  );
}

function ClaudeReadyFooter({
  record,
  onRedo,
  busy,
}: {
  record: ClaudePathRecord;
  onRedo: () => void;
  busy: boolean;
}) {
  return (
    <aside className="bootstrap-card bootstrap-card--ok" data-claude-discovery="ready">
      <h3>Claude Code 已就绪</h3>
      <dl>
        <dt>路径</dt>
        <dd>
          <code>{record.path}</code>
        </dd>
        <dt>版本</dt>
        <dd>{record.version ?? "未知"}</dd>
        <dt>发现时间</dt>
        <dd>{record.discoveredAt}</dd>
      </dl>
      <button type="button" onClick={onRedo} disabled={busy}>
        重新查找
      </button>
    </aside>
  );
}

function ClaudeOnboardingCard({
  probed,
  error,
  overrideInput,
  setOverrideInput,
  onSubmitOverride,
  onRedo,
  busy,
}: {
  probed: string[];
  error: ClaudeDiscoveryErrorDto | null;
  overrideInput: string;
  setOverrideInput: (s: string) => void;
  onSubmitOverride: (e: FormEvent<HTMLFormElement>) => void;
  onRedo: () => void;
  busy: boolean;
}) {
  return (
    <section
      className="placeholder placeholder--error"
      role="alert"
      data-claude-discovery="not-found"
    >
      <h2>未找到 Claude Code</h2>
      <p>
        应用无法在常见位置找到 <code>claude</code> 命令。由于 macOS
        图形界面应用不会继承终端里的 PATH，需要手动确认 Claude Code 的安装位置。
      </p>
      <p>已检查路径：</p>
      <ul>
        {probed.map((p) => (
          <li key={p}>
            <code>{p}</code>
          </li>
        ))}
      </ul>
      <p>你可以选择 claude 可执行文件，或安装 Claude Code 后重试。</p>
      <form onSubmit={onSubmitOverride} className="claude-discovery__override">
        <label>
          选择 claude 路径
          <input
            type="text"
            placeholder="/usr/local/bin/claude"
            value={overrideInput}
            onChange={(e) => setOverrideInput(e.target.value)}
            spellCheck={false}
          />
        </label>
        <button type="submit" disabled={busy || overrideInput.trim() === ""}>
          提交
        </button>
      </form>
      <button type="button" onClick={onRedo} disabled={busy}>
        重新查找
      </button>
      {error && (
        <p className="bootstrap-card__hint" data-claude-discovery-error={error.kind}>
          错误：<code>{error.kind}</code>
        </p>
      )}
      <p className="bootstrap-card__hint">
        安装提示：<code>brew install claude</code>。
        详见{" "}
        <a
          href="https://docs.anthropic.com/en/docs/claude-code"
          target="_blank"
          rel="noreferrer"
        >
          Anthropic Claude Code 安装文档
        </a>
        。
      </p>
    </section>
  );
}

function PluginBody({
  pluginId,
  registryEntry,
  migrationStatus,
  onRetryMigration,
  diagnostics,
}: {
  pluginId: string;
  registryEntry: PluginTabEntry | undefined;
  migrationStatus: PluginMigrationStatus | undefined;
  onRetryMigration: (pluginId: string) => void;
  diagnostics: SidecarBinaryDiagnostic[];
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
  // Round 19: per-tab sidecar surface. Migration must be Ok (or absent)
  // before we attempt to spawn — same gating contract as plugin mounting.
  return (
    <SidecarAwarePluginBody
      registryEntry={registryEntry}
      diagnostics={diagnostics}
    />
  );
}

function SidecarAwarePluginBody({
  registryEntry,
  diagnostics,
}: {
  registryEntry: PluginTabEntry;
  diagnostics: SidecarBinaryDiagnostic[];
}) {
  const clientId = useMemo(
    () => hostUiClientId(registryEntry.pluginId),
    [registryEntry.pluginId],
  );
  const binaryMissing = useMemo(
    () =>
      diagnostics.some(
        (d) => d.pluginId === registryEntry.pluginId && d.status === "missing",
      ),
    [diagnostics, registryEntry.pluginId],
  );
  const [spawnError, setSpawnError] = useState<SidecarErrorDto | null>(null);
  const { status, error: pollError, retry } = useSidecarStatus(clientId, {
    enabled: !binaryMissing,
  });

  useEffect(() => {
    if (binaryMissing) return;
    let cancelled = false;
    void spawnSidecarFromManifest(clientId).then((err) => {
      if (!cancelled) setSpawnError(err);
    });
    return () => {
      cancelled = true;
    };
  }, [clientId, binaryMissing]);

  const live = status ? classifySidecarState(status.state) : "unknown";
  const showRestartPanel =
    !binaryMissing &&
    status !== null &&
    (live === "backingOff" || live === "exited" || live === "unrecoverable");

  const onRetry = useCallback(() => {
    setSpawnError(null);
    void retry();
  }, [retry]);

  const LazyComponent = lazyForPlugin(registryEntry);
  return (
    <PluginRoot
      pluginId={registryEntry.pluginId}
      label={registryEntry.label}
      tabId={`tab-${registryEntry.pluginId}`}
    >
      {showRestartPanel && (
        <SidecarRestartPanel
          pluginId={registryEntry.pluginId}
          label={registryEntry.label}
          status={status}
          error={pollError ?? spawnError}
          onRetry={onRetry}
        />
      )}
      {!showRestartPanel && (pollError ?? spawnError) && (
        <SidecarErrorBanner error={(pollError ?? spawnError) as SidecarErrorDto} />
      )}
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

function SidecarRestartPanel({
  pluginId,
  label,
  status,
  error,
  onRetry,
}: {
  pluginId: string;
  label: string;
  status: SidecarStatusSnapshot;
  error: SidecarErrorDto | null;
  onRetry: () => void;
}) {
  return (
    <section
      className="placeholder placeholder--error"
      role="alert"
      data-sidecar-status="restart-panel"
      data-plugin-id={pluginId}
    >
      <h2>{label} · 后台进程未就绪</h2>
      <dl>
        <dt>状态</dt>
        <dd>
          <code>{status.state}</code>
        </dd>
        <dt>下一次自动重启</dt>
        <dd>{status.nextBackoffMs} ms</dd>
        <dt>近 1 小时重启次数</dt>
        <dd>{status.recentRestartCount}</dd>
        <dt>generation</dt>
        <dd>{status.generation}</dd>
      </dl>
      {error && (
        <p className="bootstrap-card__hint">
          错误：<code>{error.kind}</code>
        </p>
      )}
      <button type="button" onClick={onRetry}>
        重试启动
      </button>
    </section>
  );
}

function SidecarErrorBanner({ error }: { error: SidecarErrorDto }) {
  return (
    <aside
      className="bootstrap-card bootstrap-card--error"
      role="status"
      data-sidecar-status="error-banner"
    >
      <h3>后台进程错误</h3>
      <p>
        <code>{error.kind}</code>
      </p>
    </aside>
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
