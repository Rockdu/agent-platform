import { Suspense, lazy, useCallback, useEffect, useMemo, useState, type ComponentType, type LazyExoticComponent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
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
import { TerminalMeshView } from "./TerminalMeshView";
import {
  getOrchestratorStatus,
  isOrchestratorErrorDto,
  launchOrchestratorClaude,
  shutdownOrchestrator,
  type OrchestratorErrorDto,
  type OrchestratorStatus,
} from "./orchestrator";
import {
  closeWorkspace,
  isWorkspaceErrorDto,
  listWorkspaces,
  openWorkspace,
  type WorkspaceErrorDto,
  type WorkspaceRecord,
} from "./workspaces";
import { WorkspaceSwitcherModal } from "./WorkspaceSwitcherModal";
import {
  notificationGetPermissionState,
  type PermissionStateDto,
} from "./notification";
import "@xterm/xterm/css/xterm.css";
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
  // task22 / AC-8.4: polled once at mount + after notification activity
  // (the tray bridge emits `tray://updated` which the host already
  // re-fires perm state at the user via this banner). The denial
  // banner explains the fallback in Chinese.
  const [notifPermission, setNotifPermission] = useState<PermissionStateDto>("unknown");

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

  // task22 / AC-8.4: poll notification permission state on mount and
  // whenever the tray refreshes (so the banner flips off if the user
  // grants permission after first denial). The host caches the
  // resolved state internally; this just mirrors it for the UI.
  useEffect(() => {
    let cancelled = false;
    const refreshPerm = async () => {
      try {
        const p = await notificationGetPermissionState();
        if (!cancelled) setNotifPermission(p);
      } catch {
        // Notification service may not be ready at very first paint;
        // safe to ignore — the next tray:// event triggers a retry.
      }
    };
    void refreshPerm();
    let unlisten: (() => void) | null = null;
    void (async () => {
      const u = await listen("tray://updated", () => {
        void refreshPerm();
      });
      if (cancelled) {
        u();
        return;
      }
      unlisten = u;
    })();
    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
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
      {notifPermission === "denied" && (
        <aside
          className="notif-permission-banner"
          role="alert"
          data-notif-permission="denied"
        >
          系统通知已被拒绝。请在系统设置中开启通知权限，否则只能通过菜单栏托盘窗口查看任务提醒。
        </aside>
      )}
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
        {active.kind === "host" && (
          <OrchestratorPlaceholder
            agentPlatformPath={
              bootstrap.kind === "ok" ? bootstrap.paths.agent_platform : null
            }
          />
        )}
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

function OrchestratorPlaceholder({
  agentPlatformPath,
}: {
  agentPlatformPath: string | null;
}) {
  const [status, setStatus] = useState<ClaudeDiscoveryStatus | null>(null);
  const [error, setError] = useState<ClaudeDiscoveryErrorDto | null>(null);
  const [busy, setBusy] = useState(false);
  const [orchStatus, setOrchStatus] = useState<OrchestratorStatus | null>(null);
  const [orchError, setOrchError] =
    useState<OrchestratorErrorDto | null>(null);
  // Bumped on `关闭` to force the launch effect to re-run after the
  // backend has cleared its session. Combined with `key={terminalId}`
  // on TerminalMeshView this guarantees a full xterm remount on
  // relaunch instead of a stale re-subscribe.
  const [orchVersion, setOrchVersion] = useState(0);
  const [orchBusy, setOrchBusy] = useState(false);

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

  // Round 35 (task20): once claude discovery resolves to `ready`,
  // ask the orchestrator backend for its current session, then
  // idempotently `launch_claude` if no session exists yet. The
  // backend returns the same session on repeated calls, so this is
  // safe to re-run on every claude-discovery change.
  useEffect(() => {
    if (!status || status.kind !== "ready") {
      setOrchStatus(null);
      return;
    }
    let cancelled = false;
    void (async () => {
      try {
        const current = await getOrchestratorStatus();
        if (cancelled) return;
        if (current.kind === "ready") {
          setOrchStatus(current);
          return;
        }
        const launched = await launchOrchestratorClaude();
        if (cancelled) return;
        setOrchStatus(launched);
        setOrchError(null);
      } catch (err) {
        if (cancelled) return;
        if (isOrchestratorErrorDto(err)) setOrchError(err);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [status, orchVersion]);

  const onCloseOrchestrator = useCallback(async () => {
    setOrchBusy(true);
    try {
      await shutdownOrchestrator();
      setOrchStatus(null);
      setOrchError(null);
      // Bump the version so the launch effect re-runs against the
      // now-cleared backend state, producing a fresh session with
      // rotated tab_id / terminal_id / mcp_config_path.
      setOrchVersion((v) => v + 1);
    } catch (err) {
      if (isOrchestratorErrorDto(err)) setOrchError(err);
    } finally {
      setOrchBusy(false);
    }
  }, []);

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

  const onPickClaude = useCallback(async () => {
    setBusy(true);
    try {
      const picked = await openFileDialog({
        multiple: false,
        directory: false,
        title: "选择 claude 路径",
      });
      // Cancelled picker returns null; leave status unchanged and skip
      // the backend call so we don't ping claude_set_path_override
      // with an empty path.
      if (picked === null) return;
      const path = Array.isArray(picked) ? picked[0] : picked;
      if (typeof path !== "string" || path === "") return;
      setError(null);
      await setClaudePathOverride(path);
      await refresh();
    } catch (err) {
      if (isClaudeDiscoveryErrorDto(err)) setError(err);
    } finally {
      setBusy(false);
    }
  }, [refresh]);

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
      <section className="orchestrator-pane">
        <header className="orchestrator-pane__header">
          <div>
            <h2>Orchestrator</h2>
            <p className="placeholder__hint">
              特权单实例 tab — 自动启动 <code>claude</code> 并预配所有 MVP 平台 MCP 服务器。
            </p>
          </div>
          {orchStatus?.kind === "ready" && (
            <button
              type="button"
              className="orchestrator-pane__close"
              onClick={() => void onCloseOrchestrator()}
              disabled={orchBusy}
              aria-label="关闭 orchestrator session"
            >
              {orchBusy ? "关闭中…" : "关闭"}
            </button>
          )}
        </header>
        {orchError && (
          <aside
            className="bootstrap-card bootstrap-card--error"
            role="alert"
            data-orchestrator-error={orchError.kind}
          >
            <h3>Orchestrator 启动失败</h3>
            <p>
              <code>{orchError.kind}</code>
              {"message" in orchError ? `: ${orchError.message}` : null}
            </p>
          </aside>
        )}
        <div className="orchestrator-pane__terminal">
          {orchStatus?.kind === "ready" ? (
            // `cwd` MUST be the AgentPlatform root (where workspaces
            // live), NOT the discovered `claude` binary path — the
            // per-tab settings panel surfaces this as the working
            // directory and threads it to Cursor/Finder shortcuts.
            // The claude binary path stays in the ClaudeReadyFooter
            // below. `key={terminalId}` forces a fresh xterm mount
            // when the orchestrator is relaunched with a new session.
            <TerminalMeshView
              key={orchStatus.session.terminalId}
              active
              cwd={agentPlatformPath ?? status.record.path}
              workspaceName="Orchestrator"
              existingTerminalId={orchStatus.session.terminalId}
            />
          ) : (
            <p className="placeholder__hint">
              {orchStatus?.kind === "notLaunched"
                ? "正在启动 claude…"
                : "正在初始化 orchestrator…"}
            </p>
          )}
        </div>
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
      onPickClaude={onPickClaude}
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
  onPickClaude,
  onRedo,
  busy,
}: {
  probed: string[];
  error: ClaudeDiscoveryErrorDto | null;
  onPickClaude: () => void;
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
      <div className="claude-discovery__actions">
        <button type="button" onClick={onPickClaude} disabled={busy}>
          选择 claude 路径
        </button>
        <button type="button" onClick={onRedo} disabled={busy}>
          重新查找
        </button>
      </div>
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
    if (pluginId === "static-terminal") {
      return <MultiTerminalContainer />;
    }
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

interface OpenTab {
  tabId: string;
  workspaceId: string;
  workspaceName: string;
  workspacePath: string;
}

type HostModal = "workspace-switcher" | null;

type TransportKindHint = "Local" | "Ssh" | "SshDocker" | undefined;
type TabStatusHint = "Running" | "Done" | undefined;

function transportKindIcon(kind: TransportKindHint): string {
  switch (kind) {
    case "Ssh":
      return "🌐";
    case "SshDocker":
      return "🐳";
    case "Local":
    default:
      return "🖥";
  }
}

function statusBadgeLabel(status: TabStatusHint): string {
  return status === "Done" ? "Done" : "Running";
}

function MultiTerminalContainer() {
  // task17: every open terminal tab is bound to exactly one
  // persisted workspace. The host registry enforces
  // one-workspace-one-tab via `open_workspace`/`close_workspace`,
  // and we spawn each PTY at that workspace's canonical path.
  // task18: workspace creation and recent-list adoption live in a
  // host-coordinated modal (WorkspaceSwitcherModal); the container
  // only owns the tab array + open/close/focus calls.
  const [workspaces, setWorkspaces] = useState<WorkspaceRecord[]>([]);
  const [tabs, setTabs] = useState<OpenTab[]>([]);
  const [active, setActive] = useState<string>("");
  const [error, setError] = useState<WorkspaceErrorDto | null>(null);
  const [activeModal, setActiveModal] = useState<HostModal>(null);

  const refreshWorkspaces = useCallback(async () => {
    try {
      const list = await listWorkspaces();
      setWorkspaces(list);
    } catch (err) {
      if (isWorkspaceErrorDto(err)) setError(err);
    }
  }, []);

  useEffect(() => {
    void refreshWorkspaces();
  }, [refreshWorkspaces]);

  const adoptWorkspaceTab = useCallback(
    async (workspace: WorkspaceRecord) => {
      const tabId = `tab-${workspace.workspaceId}`;
      try {
        const refreshed = await openWorkspace(workspace.workspaceId, tabId);
        setError(null);
        setTabs((prev) => {
          if (prev.some((t) => t.workspaceId === refreshed.workspaceId)) {
            return prev;
          }
          return [
            ...prev,
            {
              tabId,
              workspaceId: refreshed.workspaceId,
              workspaceName: refreshed.name,
              workspacePath: refreshed.path,
            },
          ];
        });
        setActive(tabId);
        void refreshWorkspaces();
      } catch (err) {
        if (isWorkspaceErrorDto(err)) setError(err);
      }
    },
    [refreshWorkspaces],
  );

  const onPickExistingFromModal = useCallback(
    async (workspace: WorkspaceRecord) => {
      // Recent-list click: focus if already mounted locally, else
      // adopt as a fresh tab.
      const existing = tabs.find((t) => t.workspaceId === workspace.workspaceId);
      if (existing) {
        setActive(existing.tabId);
        return;
      }
      await adoptWorkspaceTab(workspace);
    },
    [tabs, adoptWorkspaceTab],
  );

  const closeTab = useCallback(
    async (tabId: string) => {
      const target = tabs.find((t) => t.tabId === tabId);
      if (!target) return;
      try {
        await closeWorkspace(target.workspaceId);
      } catch (err) {
        // Treat backend failures as non-fatal for the close path;
        // the user still expects the tab to disappear locally.
        if (isWorkspaceErrorDto(err)) setError(err);
      }
      setTabs((prev) => {
        const remaining = prev.filter((t) => t.tabId !== tabId);
        if (active === tabId && remaining.length > 0) {
          setActive(remaining[remaining.length - 1].tabId);
        }
        return remaining;
      });
      void refreshWorkspaces();
    },
    [tabs, active, refreshWorkspaces],
  );

  const activeId =
    tabs.find((t) => t.tabId === active)?.tabId ??
    tabs[tabs.length - 1]?.tabId ??
    null;

  const openWorkspaceIds = new Set(tabs.map((t) => t.workspaceId));

  return (
    <section className="terminal-mesh-container">
      <aside
        className="terminal-mesh-container__rail"
        role="tablist"
        aria-orientation="vertical"
      >
        {tabs.map((t) => {
          // The transport-kind icon + status badge are placeholders
          // today (LocalTransport is the only live transport and the
          // lifecycle snapshot hook lands in a follow-up). The
          // helpers fall through to Local + Running when no data is
          // wired in.
          const transportIcon = transportKindIcon(undefined);
          const status = statusBadgeLabel(undefined);
          const isActive = activeId === t.tabId;
          return (
            <div
              key={t.tabId}
              className={`terminal-mesh-container__rail-row ${
                isActive ? "terminal-mesh-container__rail-row--active" : ""
              }`}
            >
              <span
                className="rail-row__transport-icon"
                aria-label="Local"
                title="Local transport"
              >
                {transportIcon}
              </span>
              <button
                type="button"
                role="tab"
                aria-selected={isActive}
                className="rail-row__label"
                onClick={() => setActive(t.tabId)}
                title={t.workspacePath}
              >
                {t.workspaceName}
              </button>
              <span
                className={`rail-row__status-badge rail-row__status-badge--${status.toLowerCase()}`}
                aria-label={`状态：${status}`}
              >
                {status}
              </span>
              <button
                type="button"
                className="terminal-mesh-container__close"
                aria-label={`关闭 ${t.workspaceName}`}
                onClick={() => void closeTab(t.tabId)}
              >
                ×
              </button>
            </div>
          );
        })}
        <button
          type="button"
          className="terminal-mesh-container__add"
          onClick={() => setActiveModal("workspace-switcher")}
          aria-label="打开工作区"
        >
          +
        </button>
      </aside>
      <div className="terminal-mesh-container__body">
        {error && (
          <aside
            className="bootstrap-card bootstrap-card--error"
            role="alert"
            data-workspaces-error={error.kind}
          >
            <h3>工作区操作失败</h3>
            <p>
              <code>{error.kind}</code>
            </p>
          </aside>
        )}
        {tabs.length === 0 && (
          <section className="placeholder">
            <p>还没有打开任何工作区。</p>
            <button
              type="button"
              onClick={() => setActiveModal("workspace-switcher")}
            >
              打开工作区
            </button>
          </section>
        )}
        <div className="terminal-mesh-container__panes">
          {tabs.map((t) => (
            <TerminalMeshView
              key={t.tabId}
              active={activeId === t.tabId}
              cwd={t.workspacePath}
              workspaceName={t.workspaceName}
              tabId={t.tabId}
            />
          ))}
        </div>
      </div>
      <WorkspaceSwitcherModal
        open={activeModal === "workspace-switcher"}
        workspaces={workspaces}
        openWorkspaceIds={openWorkspaceIds}
        onPickExisting={onPickExistingFromModal}
        onAdopt={adoptWorkspaceTab}
        onClose={() => setActiveModal(null)}
      />
    </section>
  );
}
