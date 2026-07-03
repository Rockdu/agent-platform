import { Suspense, lazy, useCallback, useEffect, useMemo, useRef, useState, type ComponentType, type LazyExoticComponent } from "react";
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
import { shutdownTerminal } from "./terminal-mesh";
import type { DoneReason, WorkspaceLifecycleSnapshot } from "./terminal-mesh";
import { resolveAutoLaunchTerminalToShutdown } from "./close-tab-shutdown";
import { synthesizeStuckAutoLaunchError } from "./stuck-auto-launch";
import { removeTabKeyedEntry } from "./remove-tab-keyed-entry";
import { isStaleAutoLaunchRejection } from "./stale-auto-launch-rejection";
import { canOfferResumeFromDone } from "./can-offer-resume";
import {
  DEFAULT_LIFECYCLE_SNAPSHOT,
  useWorkspaceLifecycleStatuses,
} from "./use-workspace-lifecycle-statuses";
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
  deleteWorkspace,
  renameWorkspace,
  stashWorkspace,
  unstashWorkspace,
  isAutoLaunchErrorDto,
  isWorkspaceErrorDto,
  listWorkspaces,
  localPath,
  openWorkspace,
  requestWorkspaceAutoLaunch,
  type AutoLaunchErrorDto,
  type WorkspaceErrorDto,
  type WorkspaceLocation,
  type WorkspaceRecord,
} from "./workspaces";
import { WorkspaceSwitcherModal } from "./WorkspaceSwitcherModal";
import { DependencySetupView } from "./DependencySetup";
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
type HostTabId = "orchestrator" | "setup";
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
        <TabButton
          icon="⚙"
          label="依赖"
          privileged={false}
          isActive={active.kind === "host" && active.id === "setup"}
          onClick={() => setActive({ kind: "host", id: "setup" })}
        />
      </nav>

      <main className="tab-body">
        {/* DependencySetupView stays mounted so install state
            (progress, results) survives switching to other tabs. */}
        <div style={{ display: active.kind === "host" && active.id === "setup" ? "block" : "none" }}>
          <DependencySetupView />
        </div>
        {active.kind === "host" && active.id !== "setup" && (
          <OrchestratorPlaceholder
            agentPlatformPath={
              bootstrap.kind === "ok" ? bootstrap.paths.agent_platform : null
            }
          />
        )}
        {/* MultiTerminalContainer must ALWAYS be mounted (not
            conditional) so its `tabs` state survives switching to
            the orchestrator tab and back. Hide via CSS; unmounting
            resets all workspace tabs. Each non-terminal plugin tab
            is still conditionally rendered (they carry no persisted
            state that needs to survive tab switches). */}
        <div
          style={{
            display:
              active.kind === "plugin" && active.pluginId === "static-terminal"
                ? "contents"
                : "none",
          }}
        >
          <MultiTerminalContainer />
        </div>
        {active.kind === "plugin" && active.pluginId !== "static-terminal" && (
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

// Condense a long claude binary path into a single-line summary that
// fits in the compact footer pill while keeping the file basename
// visible. The full path stays in the title attribute (hover) and in
// the expanded panel's details list, so no information is hidden.
function shortenClaudePath(path: string): string {
  const MAX = 36;
  if (path.length <= MAX) return path;
  const segments = path.split("/").filter((s) => s.length > 0);
  if (segments.length >= 2) {
    const tail = segments.slice(-2).join("/");
    if (tail.length + 2 <= MAX) {
      return `…/${tail}`;
    }
  }
  const basename = segments[segments.length - 1] ?? path;
  return `…/${basename}`;
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
  // Default to the compact ≤24px pill; the chevron expands to reveal
  // the path / version / discovered-at / 重新查找 details. The
  // NotFound and NotRun discovery states are rendered by
  // ClaudeOnboardingCard above, not by this component, so they are
  // unaffected by the pill compression.
  const [expanded, setExpanded] = useState(false);
  const toggle = () => setExpanded((prev) => !prev);
  return (
    <aside
      className={`bootstrap-card bootstrap-card--ok claude-ready-footer claude-ready-footer--pill ${
        expanded ? "claude-ready-footer--expanded" : ""
      }`}
      data-claude-discovery="ready"
      data-expanded={expanded ? "true" : "false"}
    >
      <header className="claude-ready-footer__header">
        <span
          className="claude-ready-footer__title"
          title={record.path}
        >
          {`claude · v${record.version ?? "unknown"} · ${shortenClaudePath(
            record.path,
          )}`}
        </span>
        <button
          type="button"
          className="claude-ready-footer__chevron"
          aria-label={expanded ? "收起 Claude 信息" : "展开 Claude 信息"}
          aria-expanded={expanded ? "true" : "false"}
          onClick={toggle}
        >
          ▸
        </button>
      </header>
      {expanded && (
        <section className="claude-ready-footer__expanded">
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
        </section>
      )}
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
  /// Monotonic per-adoption counter. Workspace tabs reuse the
  /// workspace UUID as `tabId`, so close-then-reopen produces
  /// tabs with the same `tabId` but distinct `openGeneration`
  /// values. The fire-and-forget `requestWorkspaceAutoLaunch`
  /// rejection closure captures this at fire time and bails on
  /// mismatch so a late rejection from a prior incarnation does
  /// NOT clobber the reopened tab's state.
  openGeneration: number;
  workspaceId: string;
  workspaceName: string;
  /// Local filesystem path for Local workspaces; empty string for
  /// Remote workspaces (kept as a stable string so Finder/Cursor
  /// callers stay typed; they MUST check `workspaceLocation.kind`
  /// before treating it as a usable path).
  workspacePath: string;
  /// Full registry-side location so downstream rendering and the
  /// host spawn path can route to the right Transport. Local tabs
  /// retain the old `workspacePath` semantics; Remote tabs carry
  /// the SSH + optional Container shape.
  workspaceLocation: WorkspaceLocation;
  /// `true` when this tab's claude PTY is being scheduled via
  /// `WorkspaceLaunchScheduler`. The tab's `TerminalMeshView` MUST
  /// NOT call `spawnTerminal` while this is true and no live
  /// `existingTerminalId` has been resolved — the scheduler owns
  /// the spawn and the live terminal_id arrives via the lifecycle
  /// subscription.
  awaitingAutoLaunch: boolean;
  /// `true` when the scheduler created the terminal for this tab
  /// (i.e. the workspace had auto-launch enabled). The view side
  /// treats `existingTerminalId` as externally owned and skips
  /// `terminal_shutdown` in its cleanup, so without this flag the
  /// PTY + child process would leak past the tab close. `closeTab`
  /// consults this flag to issue an explicit `shutdownTerminal`
  /// for the resolved id.
  ownsAutoLaunchTerminal: boolean;
}

type HostModal = "workspace-switcher" | null;

type TransportKindHint = "Local" | "Ssh" | "SshDocker" | undefined;

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

function transportKindLabel(kind: TransportKindHint): string {
  switch (kind) {
    case "Ssh":
      return "SSH 远端终端";
    case "SshDocker":
      return "SSH 内 Docker 容器终端";
    case "Local":
    default:
      return "本地终端";
  }
}

interface DoneBadgeInfo {
  label: string;
  modifier: string; // CSS BEM modifier suffix without the leading "--"
  tooltip?: string;
}

function doneReasonBadgeInfo(reason: DoneReason | null): DoneBadgeInfo {
  if (reason === null) {
    return { label: "Done", modifier: "done" };
  }
  switch (reason.kind) {
    case "CleanCompletion":
      return { label: "已完成", modifier: "clean-completion" };
    case "NonZeroExit":
      return { label: `退出 ${reason.code}`, modifier: "non-zero-exit" };
    case "Disconnected":
      return { label: "已断开", modifier: "disconnected" };
    case "TaskComplete":
      return {
        label: "任务完成",
        modifier: "task-complete",
        tooltip: reason.summary,
      };
  }
}

interface WorkspaceRailRowProps {
  tab: OpenTab;
  snapshot: WorkspaceLifecycleSnapshot;
  isActive: boolean;
  onSelect: (tabId: string) => void;
  onClose: (tabId: string) => void;
  onRename: (tabId: string, newName: string) => void;
  doneAffordance?: () => void;
  stashAffordance?: () => void;
  unstashAffordance?: () => void;
}

function WorkspaceRailRow(props: WorkspaceRailRowProps) {
  const { tab, snapshot, isActive, onSelect, onClose, onRename, doneAffordance, stashAffordance, unstashAffordance } = props;
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);

  const startEdit = () => {
    setDraft(tab.workspaceName);
    setEditing(true);
    // Focus after render
    setTimeout(() => inputRef.current?.select(), 0);
  };
  const commitEdit = () => {
    setEditing(false);
    const trimmed = draft.trim();
    if (trimmed && trimmed !== tab.workspaceName) onRename(tab.tabId, trimmed);
  };
  const cancelEdit = () => { setEditing(false); };
  const icon = transportKindIcon(snapshot.transportKind);
  const transportLabel = transportKindLabel(snapshot.transportKind);
  const isDone = snapshot.status === "Done";
  // Pending wins over Running because a workspace can be queued
  // (waiting for a launch slot) even though its snapshot status is
  // still Running by default. Done wins over Pending because a Done
  // tab cannot also be pending — pendingLaunch is cleared whenever
  // the launch settles or the tab is closed.
  const badge: DoneBadgeInfo = isDone
    ? doneReasonBadgeInfo(snapshot.doneReason)
    : snapshot.pendingLaunch
      ? { label: "等待启动", modifier: "pending-launch" }
      : { label: "Running", modifier: "running" };
  const rowTitle = badge.tooltip
    ? `${tab.workspacePath} — ${badge.tooltip}`
    : tab.workspacePath;
  return (
    <div
      className={`terminal-mesh-container__rail-row ${
        isActive ? "terminal-mesh-container__rail-row--active" : ""
      }`}
    >
      <span
        className="rail-row__transport-icon"
        aria-label={transportLabel}
        title={transportLabel}
      >
        {icon}
      </span>
      {editing ? (
        <input
          ref={inputRef}
          className="rail-row__label rail-row__label--editing"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={commitEdit}
          onKeyDown={(e) => {
            if (e.key === "Enter") { e.preventDefault(); commitEdit(); }
            if (e.key === "Escape") { e.preventDefault(); cancelEdit(); }
          }}
          onClick={(e) => e.stopPropagation()}
          autoFocus
        />
      ) : (
        <button
          type="button"
          role="tab"
          aria-selected={isActive}
          className="rail-row__label"
          onClick={() => onSelect(tab.tabId)}
          onDoubleClick={(e) => { e.stopPropagation(); startEdit(); }}
          title={`${rowTitle}（双击重命名）`}
        >
          {tab.workspaceName}
        </button>
      )}
      <span
        className={`rail-row__status-badge rail-row__status-badge--${badge.modifier}`}
        aria-label={`状态：${badge.label}`}
        title={badge.tooltip}
      >
        {badge.label}
      </span>
      {doneAffordance && (
        <button
          type="button"
          className="rail-row__resume"
          onClick={doneAffordance}
          aria-label={`聚焦 ${tab.workspaceName} 输入下一条指令`}
          title="下一条指令"
        >
          下一条指令
        </button>
      )}
      {stashAffordance && (
        <button
          type="button"
          className="rail-row__stash"
          onClick={stashAffordance}
          aria-label={`暂存 ${tab.workspaceName}`}
          title="暂存"
        >
          ⏸
        </button>
      )}
      {unstashAffordance && (
        <button
          type="button"
          className="rail-row__unstash"
          onClick={unstashAffordance}
          aria-label={`恢复 ${tab.workspaceName}`}
          title="恢复到完成区"
        >
          ▶
        </button>
      )}
      <button
        type="button"
        className="terminal-mesh-container__close"
        aria-label={`关闭 ${tab.workspaceName}`}
        onClick={() => onClose(tab.tabId)}
      >
        ×
      </button>
    </div>
  );
}

interface RailSectionsProps {
  tabs: OpenTab[];
  activeId: string | null;
  stashedTabIds: ReadonlySet<string>;
  onSelect: (tabId: string) => void;
  onClose: (tabId: string) => void;
  onStash: (tabId: string) => void;
  onUnstash: (tabId: string) => void;
  onResume: (tabId: string) => void;
  onReorder: (tabId: string, afterTabId: string | null) => void;
  onRename: (tabId: string, newName: string) => void;
  snapshotByTabId: Readonly<Record<string, WorkspaceLifecycleSnapshot>>;
}

function RailSections(props: RailSectionsProps) {
  const { tabs, activeId, stashedTabIds, onSelect, onClose, onStash, onUnstash, onResume, onReorder, onRename, snapshotByTabId } = props;
  // dragTabIdRef: source of truth for drag handlers (ref avoids stale closures).
  // dragTabId state: mirrors the ref, exists only to trigger opacity re-renders.
  const dragTabIdRef = useRef<string | null>(null);
  const [dragTabId, setDragTabId] = useState<string | null>(null);
  // Per-section manual order. null = not yet manually reordered → use FIFO.
  // Once set, manual order takes precedence so drag reorders aren't reverted.
  const [sectionOrder, setSectionOrder] = useState<{
    running: string[] | null;
    done: string[] | null;
    stashed: string[] | null;
  }>({ running: null, done: null, stashed: null });
  // Which section a dragged tab belongs to (so cross-section drops are rejected).
  const dragSectionRef = useRef<"running" | "done" | "stashed" | null>(null);

  const runningRaw: OpenTab[] = [];
  const doneRaw: OpenTab[] = [];
  const stashedRaw: OpenTab[] = [];
  for (const tab of tabs) {
    if (stashedTabIds.has(tab.tabId)) {
      stashedRaw.push(tab);
      continue;
    }
    const snap = snapshotByTabId[tab.tabId] ?? DEFAULT_LIFECYCLE_SNAPSHOT;
    if (snap.tabKind !== "Workspace") continue;
    if (snap.status === "Done" || !snap.agentBusy) {
      doneRaw.push(tab);
    } else {
      runningRaw.push(tab);
    }
  }
  const byActivity = (a: OpenTab, b: OpenTab): number => {
    const sa = snapshotByTabId[a.tabId] ?? DEFAULT_LIFECYCLE_SNAPSHOT;
    const sb = snapshotByTabId[b.tabId] ?? DEFAULT_LIFECYCLE_SNAPSHOT;
    return sa.lastActivityAtUnixMs - sb.lastActivityAtUnixMs;
  };
  // Apply section order: use manual order if set, otherwise FIFO sort.
  const applyOrder = (raw: OpenTab[], order: string[] | null): OpenTab[] => {
    if (!order) return [...raw].sort(byActivity);
    const byId = new Map(raw.map((t) => [t.tabId, t]));
    const ordered = order.flatMap((id) => { const t = byId.get(id); return t ? [t] : []; });
    // Append any tabs not yet in order (newly added).
    const inOrder = new Set(order);
    for (const t of raw) { if (!inOrder.has(t.tabId)) ordered.push(t); }
    return ordered;
  };
  const running = applyOrder(runningRaw, sectionOrder.running);
  const done    = applyOrder(doneRaw,    sectionOrder.done);
  const stashed = applyOrder(stashedRaw, sectionOrder.stashed);

  const sectionOf = (tabId: string): "running" | "done" | "stashed" | null => {
    if (runningRaw.some((t) => t.tabId === tabId)) return "running";
    if (doneRaw.some((t)    => t.tabId === tabId)) return "done";
    if (stashedRaw.some((t) => t.tabId === tabId)) return "stashed";
    return null;
  };

  const renderRow = (tab: OpenTab, isStashed: boolean, section: "running" | "done" | "stashed") => {
    const snap = snapshotByTabId[tab.tabId] ?? DEFAULT_LIFECYCLE_SNAPSHOT;
    const resumeOffered = !isStashed && canOfferResumeFromDone(snap);
    return (

      <div
        key={tab.tabId}
        draggable
        onDragStart={() => {
          dragTabIdRef.current = tab.tabId;
          setDragTabId(tab.tabId);
          dragSectionRef.current = sectionOf(tab.tabId);
        }}
        onDragEnd={() => {
          dragTabIdRef.current = null;
          setDragTabId(null);
          dragSectionRef.current = null;
        }}
        onDragOver={(e) => { e.preventDefault(); }}
        onDrop={() => {
          // Read from ref (not state) to avoid stale closure issues:
          // the drop handler's closure may have been created before the
          // dragStart state update flushed to a new render.
          const from = dragTabIdRef.current;
          const fromSection = dragSectionRef.current;
          dragTabIdRef.current = null;
          setDragTabId(null);
          dragSectionRef.current = null;
          // Reject cross-section drops.
          if (!from || from === tab.tabId || fromSection !== section) return;
          onReorder(from, tab.tabId);
          // Store the new order for this section so FIFO sort doesn't revert it.
          setSectionOrder((prev) => {
            const currentList = section === "running" ? running
              : section === "done" ? done
              : stashed;
            const ids = currentList.map((t) => t.tabId);
            const fromIdx = ids.indexOf(from);
            const toIdx = ids.indexOf(tab.tabId);
            if (fromIdx === -1 || toIdx === -1) return prev;
            const next = [...ids];
            const [moved] = next.splice(fromIdx, 1);
            next.splice(toIdx, 0, moved);
            return { ...prev, [section]: next };
          });
        }}
        style={{ opacity: dragTabId === tab.tabId ? 0.4 : 1 }}
      >
        <WorkspaceRailRow
          tab={tab}
          snapshot={snap}
          isActive={activeId === tab.tabId}
          onSelect={onSelect}
          onClose={onClose}
          onRename={onRename}
          doneAffordance={resumeOffered ? () => onResume(tab.tabId) : undefined}
          stashAffordance={isStashed ? undefined : () => onStash(tab.tabId)}
          unstashAffordance={isStashed ? () => onUnstash(tab.tabId) : undefined}
        />
      </div>
    );
  };

  return (
    <>
      <section className="rail-section rail-section--done">
        <header className="rail-section__header">
          <span className="rail-section__label">完成区</span>
          <span className="rail-section__count">{done.length}</span>
        </header>
        {done.map((t) => renderRow(t, false, "done"))}
      </section>
      <section className="rail-section rail-section--running">
        <header className="rail-section__header">
          <span className="rail-section__label">运行区</span>
          <span className="rail-section__count">{running.length}</span>
        </header>
        {running.map((t) => renderRow(t, false, "running"))}
      </section>
      {stashed.length > 0 && (
        <section className="rail-section rail-section--stashed">
          <header className="rail-section__header">
            <span className="rail-section__label">暂存区</span>
            <span className="rail-section__count">{stashed.length}</span>
          </header>
          {stashed.map((t) => renderRow(t, true, "stashed"))}
        </section>
      )}
    </>
  );
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
  // Set of tab IDs currently in 暂存区. Persisted via WorkspaceProfile.stashed
  // (backend) but mirrored here for O(1) rail-partition lookup.
  const [stashedTabIds, setStashedTabIds] = useState<Set<string>>(new Set());

  // "灵动岛" style completion toast: shown briefly when a workspace
  // tab transitions into 完成区 (TaskComplete or quiescence).
  const [completionToast, setCompletionToast] = useState<{
    workspaceName: string;
    summary?: string;
  } | null>(null);
  const completionToastTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // Monotonic counter stamped onto each new tab at adoption time
  // so the fire-and-forget `requestWorkspaceAutoLaunch` rejection
  // closure can detect close-then-reopen incarnations of the same
  // bare-workspace-UUID `tabId` and silently drop stale rejections.
  const openGenerationCounterRef = useRef<number>(0);
  // Live mirror of `tabs` for synchronous read from async
  // callbacks. The rejection guard for `requestWorkspaceAutoLaunch`
  // needs the LATEST tab generation at the moment the rejection
  // arrives — not the value captured at render time. Synced in an
  // effect below.
  const tabsRef = useRef<OpenTab[]>([]);
  // Tab-set memo + parent-owned lifecycle hook. Hoisted above
  // adoptWorkspaceTab/closeTab so the close path can read
  // `terminalIdByTabId` to shut down scheduler-owned terminals
  // before the tab is dropped from state.
  const tabIds = useMemo(() => tabs.map((t) => t.tabId), [tabs]);
  // Mirror the latest `tabs` into `tabsRef` so async callbacks
  // (e.g. fire-and-forget rejection guards) can read the current
  // value without a stale render-time closure.
  useEffect(() => {
    tabsRef.current = tabs;
  }, [tabs]);
  const { snapshotByTabId, terminalIdByTabId } =
    useWorkspaceLifecycleStatuses(tabIds);
  const [active, setActive] = useState<string>("");
  // When true, auto-focus after Enter is disabled: the view stays on the
  // current terminal instead of jumping to the next idle workspace.
  const [focusLocked, setFocusLocked] = useState(false);
  const [railWidth, setRailWidth] = useState<number>(() => {
    const saved = localStorage.getItem("rail-width");
    return saved ? Math.max(140, Math.min(480, parseInt(saved, 10))) : 200;
  });
  const railResizingRef = useRef(false);
  const [error, setError] = useState<WorkspaceErrorDto | null>(null);
  // Per-tab auto-launch error. Surfaces inside the workspace pane so
  // the user sees why claude failed to start instead of getting a
  // silently-empty terminal.
  const [autoLaunchErrorByTabId, setAutoLaunchErrorByTabId] = useState<
    Record<string, AutoLaunchErrorDto>
  >({});
  const [activeModal, setActiveModal] = useState<HostModal>(null);
  // Per-tab focus nonce: bumped whenever the user clicks the Done-
  // row 下一条指令 affordance. `TerminalMeshView` watches its own
  // entry in this map and calls `term.focus()` when the value
  // changes (and the panel is active). Bare tab-label clicks keep
  // using plain `setActive` so we never steal focus from other
  // panes' inputs on an ordinary tab switch.
  const [focusNonceByTabId, setFocusNonceByTabId] = useState<
    Record<string, number>
  >({});
  const focusTerminal = useCallback((tabId: string) => {
    setActive(tabId);
    setFocusNonceByTabId((prev) => ({
      ...prev,
      [tabId]: (prev[tabId] ?? 0) + 1,
    }));
  }, []);

  const refreshWorkspaces = useCallback(async () => {
    try {
      const list = await listWorkspaces();
      setWorkspaces(list);
      // Re-sync stash state from the persisted profile so restarts
      // preserve which tabs were stashed.
      setStashedTabIds(prev => {
        const next = new Set(prev);
        for (const w of list) {
          const tabId = w.openTabId;
          if (!tabId) continue;
          if (w.profile.stashed) next.add(tabId);
          else next.delete(tabId);
        }
        return next;
      });
    } catch (err) {
      if (isWorkspaceErrorDto(err)) setError(err);
    }
  }, []);

  useEffect(() => {
    void refreshWorkspaces();
  }, [refreshWorkspaces]);

  // Session restoration: on first workspace load, re-open all workspaces
  // whose openTabId is still set (app was quit without closing the tabs).
  // On a clean tab-close the backend clears openTabId, so only workspaces
  // that were open when the app last quit (or crashed) are restored.
  // Auto-launch spawns claude with `--continue` when a prior transcript
  // exists so the conversation history is picked up on restart.
  // Show the real error from failed auto-launches instead of
  // the generic "asyncSpawnFailed → Disconnected".
  useEffect(() => {
    let unlisten: (() => void) | null = null;
    void listen<{ tabId: string; error: string }>(
      "workspace://launch-error",
      (event) => {
        const { tabId, error } = event.payload;
        setAutoLaunchErrorByTabId((prev) => ({
          ...prev,
          [tabId]: {
            kind: "asyncSpawnFailed",
            transportKind: "detail",
            doneReason: error,
          },
        }));
      },
    ).then((fn) => { unlisten = fn; });
    return () => { if (unlisten) unlisten(); };
  }, []);

  // Listen for workspaces opened by Claude via the MCP tool
  // `agent_platform.open_workspace`. The backend emits this event
  // after registering/opening the workspace; the frontend adopts it
  // as a new tab (same flow as the recent-list picker).
  useEffect(() => {
    let unlisten: (() => void) | null = null;
    void listen<WorkspaceRecord>("workspace://agent-opened", (event) => {
      if (adoptWorkspaceTabRef.current) {
        void adoptWorkspaceTabRef.current(event.payload);
      }
    }).then((fn) => { unlisten = fn; });
    return () => { if (unlisten) unlisten(); };
  // adoptWorkspaceTabRef is stable; run once on mount.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const sessionRestoredRef = useRef(false);
  // Stable ref to adoptWorkspaceTab so the restoration effect doesn't
  // depend on the callback's identity (avoids re-running after each render).
  const adoptWorkspaceTabRef = useRef<typeof adoptWorkspaceTab | null>(null);
  useEffect(() => {
    if (sessionRestoredRef.current) return;
    if (workspaces.length === 0) return;
    if (!adoptWorkspaceTabRef.current) return;
    sessionRestoredRef.current = true;
    // Only restore workspaces whose tab was open when the app last quit.
    // profile.restoreOnStartup is a persistent flag: set by open_workspace,
    // cleared by close_workspace (× button). Unlike openTabId it is NOT
    // wiped on backend startup, so it reliably identifies which workspaces
    // were open versus which were explicitly × closed.
    const adopt = adoptWorkspaceTabRef.current;
    for (const w of workspaces) {
      if (w.profile.restoreOnStartup) void adopt(w);
    }
  }, [workspaces]);

  // Stuck-waiting watcher: when an auto-launched tab's snapshot
  // transitions to Done without ever publishing a real terminal
  // id (the back-end's `surface_auto_launch_async_failure` path
  // emits a lifecycle envelope with `terminalId: null`), the
  // pane would otherwise stay parked in `等待 claude 启动…`
  // because the view's waiting branch only exits on a real id or
  // a synchronous request-promise rejection. Synthesize an
  // `AutoLaunchErrorDto.asyncSpawnFailed` entry so the existing
  // banner path renders the error and clear `awaitingAutoLaunch`
  // on the tab so the view re-runs through the attach branch.
  useEffect(() => {
    setTabs((prev) => {
      let mutated = false;
      const next = prev.map((t) => {
        const synthesized = synthesizeStuckAutoLaunchError(
          t,
          snapshotByTabId[t.tabId],
          terminalIdByTabId[t.tabId],
        );
        if (synthesized) {
          mutated = true;
          // Install the synthesized error (idempotent if already
          // installed) so the view's banner renders even if the
          // user did not previously see a synchronous rejection.
          setAutoLaunchErrorByTabId((prevErr) =>
            prevErr[t.tabId]
              ? prevErr
              : { ...prevErr, [t.tabId]: synthesized },
          );
          return { ...t, awaitingAutoLaunch: false };
        }
        return t;
      });
      const committed = mutated ? next : prev;
      tabsRef.current = committed;
      return committed;
    });
  }, [snapshotByTabId, terminalIdByTabId]);

  const reorderTab = useCallback(
    (tabId: string, afterTabId: string | null) => {
      if (!afterTabId) return;
      setTabs((prev) => {
        const from = prev.findIndex((t) => t.tabId === tabId);
        const to = prev.findIndex((t) => t.tabId === afterTabId);
        if (from === -1 || to === -1 || from === to) return prev;
        const next = [...prev];
        const [moved] = next.splice(from, 1);
        next.splice(to, 0, moved);
        tabsRef.current = next;
        return next;
      });
    },
    [],
  );

  const renameTab = useCallback(
    (tabId: string, newName: string) => {
      const target = tabs.find((t) => t.tabId === tabId);
      if (!target) return;
      void renameWorkspace(target.workspaceId, newName).then(() => {
        setTabs((prev) =>
          prev.map((t) =>
            t.tabId === tabId ? { ...t, workspaceName: newName } : t,
          ),
        );
      });
    },
    [tabs],
  );

  const stashTab = useCallback(
    async (tabId: string) => {
      const target = tabs.find((t) => t.tabId === tabId);
      if (!target) return;
      setStashedTabIds((prev) => new Set([...prev, tabId]));
      void stashWorkspace(target.workspaceId).catch(() => {
        setStashedTabIds((prev) => { const n = new Set(prev); n.delete(tabId); return n; });
      });
      // If stashing the active tab, move focus to the next non-stash tab.
      if (active === tabId) {
        const nonStash = tabs.filter(
          (t) => t.tabId !== tabId && !stashedTabIds.has(t.tabId),
        );
        const idx = tabs.findIndex((t) => t.tabId === tabId);
        const next =
          nonStash.find((t) => tabs.indexOf(t) > idx) ??
          nonStash[nonStash.length - 1];
        if (next) setActive(next.tabId);
      }
    },
    [tabs, active, stashedTabIds],
  );

  const unstashTab = useCallback(
    async (tabId: string) => {
      const target = tabs.find((t) => t.tabId === tabId);
      if (!target) return;
      setStashedTabIds((prev) => { const n = new Set(prev); n.delete(tabId); return n; });
      void unstashWorkspace(target.workspaceId).catch(() => {
        setStashedTabIds((prev) => new Set([...prev, tabId]));
      });
    },
    [tabs],
  );

  // "灵动岛" completion toast: watch for TaskComplete transitions
  // and show a brief overlay when any workspace Claude finishes.
  const prevSnapshotRef = useRef<Record<string, WorkspaceLifecycleSnapshot>>({});
  useEffect(() => {
    for (const tab of tabs) {
      const prev = prevSnapshotRef.current[tab.tabId];
      const curr = snapshotByTabId[tab.tabId];
      if (!curr || !prev) continue;

      // Show toast when: TaskComplete fires, OR agent goes from busy→idle
      // (agentBusy true→false means Claude finished a round of work).
      const wasWorking = prev.agentBusy;
      const taskComplete =
        curr.status === "Done" &&
        curr.doneReason?.kind === "TaskComplete" &&
        (prev.status !== "Done" || prev.doneReason?.kind !== "TaskComplete");
      const wentIdle =
        prev.agentBusy && !curr.agentBusy && curr.status !== "Done";

      if (taskComplete || wentIdle) {
        void wasWorking; // suppress unused warning
        const summary =
          curr.doneReason?.kind === "TaskComplete"
            ? curr.doneReason.summary
            : undefined;
        if (completionToastTimerRef.current) {
          clearTimeout(completionToastTimerRef.current);
        }
        setCompletionToast({ workspaceName: tab.workspaceName, summary });
        completionToastTimerRef.current = setTimeout(() => {
          setCompletionToast(null);
          completionToastTimerRef.current = null;
        }, 4000);
      }
    }
    prevSnapshotRef.current = { ...snapshotByTabId };
  }, [snapshotByTabId, tabs]);

  const adoptWorkspaceTab = useCallback(
    async (workspace: WorkspaceRecord) => {
      // The tab id is the bare workspace UUID so the terminal-mesh
      // sidecar's `claude:<tab_id>:terminal-mesh` clientId parser
      // accepts it (the parser requires `tab_id` to be a UUID). The
      // host-side `TerminalMeshRegistry.tab_index` is format-agnostic
      // — it keys by string — so this change is invisible at the
      // backend.
      const tabId = workspace.workspaceId;
      // Stamp a fresh incarnation token. The fire-and-forget
      // rejection closure captures this value and bails on
      // mismatch so close-then-reopen-then-late-reject doesn't
      // corrupt the new tab's state.
      const capturedGeneration = ++openGenerationCounterRef.current;
      try {
        const refreshed = await openWorkspace(workspace.workspaceId, tabId);
        setError(null);
        // Defense in depth: clear any stale per-tab state from a
        // prior life of this workspace's tab id. `closeTab`
        // already clears these, but an abnormal close path (e.g.
        // app crash) could leave entries behind.
        setAutoLaunchErrorByTabId((prev) => removeTabKeyedEntry(prev, tabId));
        setFocusNonceByTabId((prev) => removeTabKeyedEntry(prev, tabId));
        // Any workspace with the auto-launch flag set qualifies —
        // Remote workspaces route through SshTransport /
        // DockerOverSshTransport on the backend (see
        // `select_transport_kind_for` + `RealLaunchExecutor`).
        const willAutoLaunch = refreshed.profile.autoLaunchClaude;
        // Track whether `setTabs` actually inserted a new tab.
        // Adopting an already-open workspace must NOT re-fire the
        // auto-launch enqueue — after the original launch has
        // settled, the scheduler accepts a second enqueue and
        // spawns a duplicate terminal; the registry then rebinds
        // the tab to the new terminal, orphaning the original
        // externally-owned PTY.
        let isNewTab = true;
        setTabs((prev) => {
          if (prev.some((t) => t.workspaceId === refreshed.workspaceId)) {
            isNewTab = false;
            // No mirror write needed — `tabsRef.current` already
            // matches `prev` (every other setTabs updater syncs
            // it inline; the useEffect fallback below also
            // commits the same value).
            return prev;
          }
          const next: OpenTab[] = [
            ...prev,
            {
              tabId,
              openGeneration: capturedGeneration,
              workspaceId: refreshed.workspaceId,
              workspaceName: refreshed.name,
              workspacePath: localPath(refreshed) ?? "",
              workspaceLocation: refreshed.location,
              awaitingAutoLaunch: willAutoLaunch,
              // When the scheduler creates the terminal for us,
              // close-time cleanup is on us — the view's cleanup
              // path treats `existingTerminalId` as externally
              // owned and skips `terminal_shutdown`, so without
              // this flag the PTY + child process would leak.
              ownsAutoLaunchTerminal: willAutoLaunch,
            },
          ];
          // CRITICAL: sync tabsRef synchronously here. The
          // fire-and-forget rejection closure below may run
          // synchronously / microtask-fast (e.g. backend
          // rejects ClaudeDiscoveryNotReady immediately) — BEFORE
          // the `useEffect` that mirrors `tabs` into `tabsRef`
          // commits. Without this inline assignment, the
          // staleness helper would see no matching tab in
          // `tabsRef.current` and silently drop the rejection,
          // leaving the pane stuck waiting forever.
          tabsRef.current = next;
          return next;
        });
        setActive(tabId);
        void refreshWorkspaces();
        // Fire-and-forget: the backend scheduler returns
        // immediately after enqueueing; the eventual terminal_id
        // arrives via the lifecycle subscription. Disabled-auto-
        // launch paths are typed-rejected on the backend, so
        // swallow those without surfacing a user-facing error.
        // `isNewTab` guards against a duplicate enqueue when the
        // user clicks an already-open workspace — without it,
        // the scheduler would spawn a second terminal and the
        // registry would orphan the original.
        if (refreshed.profile.autoLaunchClaude && isNewTab) {
          void requestWorkspaceAutoLaunch(refreshed.workspaceId, tabId).catch(
            (err) => {
              // Stale-incarnation guard: the workspace may have
              // been closed (or closed + reopened) since this
              // request fired; in those cases the current tab's
              // `openGeneration` no longer matches the captured
              // value and applying state would corrupt either an
              // empty tab list (no-op) or a brand-new tab
              // (clobber). `tabsRef.current` is the latest tabs
              // array, synced via the mirror effect above.
              if (
                isStaleAutoLaunchRejection({
                  tabs: tabsRef.current,
                  tabId,
                  capturedGeneration,
                })
              ) {
                console.debug(
                  "auto-launch rejection dropped: tab incarnation changed",
                  { tabId, capturedGeneration },
                );
                return;
              }
              if (isAutoLaunchErrorDto(err)) {
                setAutoLaunchErrorByTabId((prev) => ({
                  ...prev,
                  [tabId]: err,
                }));
              } else {
                console.warn("auto-launch enqueue failed", err);
              }
              // Synchronous enqueue rejection (e.g. local
              // ClaudeDiscoveryNotReady, AutoLaunchDisabled,
              // workspace-not-found) means no scheduler-owned
              // terminal will ever attach for this tab. Clear
              // the waiting flag so the view exits the
              // waiting branch — the typed-error banner (or
              // the empty placeholder for the catch-all)
              // takes over. The setter still gates by
              // generation as defense in depth in case a fast
              // close races with the setter dispatch.
              setTabs((prev) => {
                const next = prev.map((t) =>
                  t.tabId === tabId && t.openGeneration === capturedGeneration
                    ? { ...t, awaitingAutoLaunch: false }
                    : t,
                );
                tabsRef.current = next;
                return next;
              });
            },
          );
        }
      } catch (err) {
        if (isWorkspaceErrorDto(err)) setError(err);
      }
    },
    [refreshWorkspaces],
  );
  // Keep the restoration ref up-to-date so the one-shot session
  // restore effect always calls the latest version of adoptWorkspaceTab.
  adoptWorkspaceTabRef.current = adoptWorkspaceTab;

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
      // Scheduler-owned terminals: the view's cleanup path treats
      // `existingTerminalId` as externally owned and skips
      // `terminal_shutdown`, so the PTY + child process would
      // outlive the tab. Issue the shutdown here, fire-and-forget,
      // BEFORE we drop the tab from state. Ignore errors so a
      // missing-terminal case (already exited naturally) still
      // proceeds with the close.
      const shutdownId = resolveAutoLaunchTerminalToShutdown(target, terminalIdByTabId);
      if (shutdownId) {
        void shutdownTerminal(shutdownId).catch((err) => {
          console.warn("auto-launched terminal shutdown failed", err);
        });
      }
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
        tabsRef.current = remaining;
        return remaining;
      });
      // Clear per-tab maps keyed by tabId. The tabId is the bare
      // workspace UUID, so reopening the same workspace would
      // otherwise inherit stale state (the prior auto-launch
      // failure banner, the prior focus nonce). Defense in
      // depth — `adoptWorkspaceTab` also clears these before
      // installing a new tab.
      setAutoLaunchErrorByTabId((prev) => removeTabKeyedEntry(prev, tabId));
      setFocusNonceByTabId((prev) => removeTabKeyedEntry(prev, tabId));
      void refreshWorkspaces();
    },
    [tabs, active, refreshWorkspaces, terminalIdByTabId],
  );

  const activeId =
    tabs.find((t) => t.tabId === active)?.tabId ??
    tabs[tabs.length - 1]?.tabId ??
    null;

  // Called when the user presses Enter in a terminal. Switches focus
  // to the next tab in 完成区 (idle, waiting for instruction) so the
  // user can dispatch tasks to other waiting agents without manually
  // clicking. Wraps around; skips the current tab.
  const onTerminalSubmit = useCallback(
    (currentTabId: string) => {
      if (focusLocked) return; // stay on current terminal
      const idle = tabs.filter(
        (t) => {
          if (t.tabId === currentTabId) return false;
          if (stashedTabIds.has(t.tabId)) return false; // skip stashed
          const snap = snapshotByTabId[t.tabId] ?? DEFAULT_LIFECYCLE_SNAPSHOT;
          return snap.tabKind === "Workspace" &&
            (snap.status === "Done" || !snap.agentBusy);
        },
      );
      if (idle.length === 0) return;
      const currentIndex = tabs.findIndex((t) => t.tabId === currentTabId);
      const next =
        idle.find((t) => tabs.indexOf(t) > currentIndex) ?? idle[0];
      if (next) setActive(next.tabId);
    },
    [tabs, snapshotByTabId, stashedTabIds, focusLocked],
  );

  const openWorkspaceIds = new Set(tabs.map((t) => t.workspaceId));

  // Lifted lifecycle hook: feeds both the rail badges AND the
  // per-tab `existingTerminalId` resolution that lets auto-launched
  // workspaces attach to the scheduler's PTY instead of spawning a
  // second one. The hook is memoized on `tabIds` so it only re-runs
  // when the tab set actually changes. (The hook is invoked higher
  // up so the close path can read `terminalIdByTabId`; only the
  // explanatory comment remains here for readers tracing the flow.)

  return (
    <section className="terminal-mesh-container">
      {completionToast && (
        <div className="completion-toast" role="status" aria-live="polite">
          <span className="completion-toast__icon">✓</span>
          <div className="completion-toast__body">
            <span className="completion-toast__name">{completionToast.workspaceName}</span>
            {completionToast.summary && (
              <span className="completion-toast__summary">{completionToast.summary}</span>
            )}
          </div>
        </div>
      )}
      <aside
        className="terminal-mesh-container__rail"
        role="tablist"
        aria-orientation="vertical"
        style={{ width: railWidth, flex: `0 0 ${railWidth}px` }}
      >
        <div
          className="terminal-mesh-container__rail-resize"
          onMouseDown={(e) => {
            e.preventDefault();
            railResizingRef.current = true;
            const startX = e.clientX;
            const startW = railWidth;
            const onMove = (ev: MouseEvent) => {
              if (!railResizingRef.current) return;
              const next = Math.max(140, Math.min(480, startW + ev.clientX - startX));
              setRailWidth(next);
              localStorage.setItem("rail-width", String(next));
            };
            const onUp = () => {
              railResizingRef.current = false;
              window.removeEventListener("mousemove", onMove);
              window.removeEventListener("mouseup", onUp);
            };
            window.addEventListener("mousemove", onMove);
            window.addEventListener("mouseup", onUp);
          }}
        />
        <div className="terminal-mesh-container__rail-header">
          <button
            type="button"
            className={`terminal-mesh-container__lock${focusLocked ? " terminal-mesh-container__lock--on" : ""}`}
            onClick={() => setFocusLocked((v) => !v)}
            title={focusLocked ? "锁定中：点击解除，自动跳到下一个终端" : "点击锁定：停留在当前终端"}
            aria-label={focusLocked ? "解除锁定" : "锁定当前终端"}
          >
            {focusLocked ? "🔒" : "🔓"}
          </button>
        </div>
        <RailSections
          tabs={tabs}
          activeId={activeId}
          stashedTabIds={stashedTabIds}
          onSelect={setActive}
          onClose={(id) => void closeTab(id)}
          onStash={(id) => void stashTab(id)}
          onUnstash={(id) => void unstashTab(id)}
          onReorder={reorderTab}
          onResume={focusTerminal}
          onRename={renameTab}
          snapshotByTabId={snapshotByTabId}
        />
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
              workspaceLocation={t.workspaceLocation}
              tabId={t.tabId}
              workspaceId={t.workspaceId}
              focusNonce={focusNonceByTabId[t.tabId]}
              autoLaunchError={autoLaunchErrorByTabId[t.tabId] ?? null}
              awaitingAutoLaunch={t.awaitingAutoLaunch}
              existingTerminalId={
                t.awaitingAutoLaunch ? terminalIdByTabId[t.tabId] : undefined
              }
              onSubmit={() => onTerminalSubmit(t.tabId)}
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
        onDelete={async (w) => {
          try {
            await deleteWorkspace(w.workspaceId);
            void refreshWorkspaces();
          } catch (err) {
            if (isWorkspaceErrorDto(err)) setError(err);
          }
        }}
        onClose={() => setActiveModal(null)}
      />
    </section>
  );
}
