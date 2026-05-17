import { useCallback, useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import {
  isTerminalMeshErrorDto,
  readTerminalScrollback,
  resizeTerminal,
  shutdownTerminal,
  spawnTerminal,
  subscribeTerminalEvents,
  writeTerminalStdin,
  type TerminalEventEnvelope,
  type TerminalMeshErrorDto,
} from "./terminal-mesh";
import {
  isIdeHandoffErrorDto,
  openWorkspaceInIde,
  revealWorkspaceInFinder,
  type IdeHandoffErrorDto,
} from "./ide-handoff";
import { IdePreferencePane } from "./IdePreferencePane";

const INITIAL_SCROLLBACK_BYTES = 64 * 1024;

export type TerminalRunStatus =
  | { kind: "running" }
  | { kind: "exited"; code: number | null }
  | { kind: "cancelled" };

export interface TerminalMeshViewProps {
  /// Whether this terminal's panel is the currently visible tab. The
  /// panel stays mounted regardless so the PTY survives tab switches
  /// (AC-4.1 ≥4 PTYs HARD at the UI layer). Visibility is toggled
  /// via CSS; on inactive→active transitions we re-run fit+resize so
  /// xterm.js measures the now-visible container correctly.
  active: boolean;
  /// Working directory for the spawned shell. Round 28 (task17): set
  /// to the active workspace path so each terminal opens at its
  /// bound workspace root rather than $HOME. Falls back to backend
  /// default when undefined.
  cwd?: string;
  /// Display name of the bound workspace. Round 33 (task19
  /// remediation): used as the header label for the per-tab
  /// settings panel.
  workspaceName?: string;
  /// If set, skip spawning a fresh PTY and subscribe directly to an
  /// existing terminal_id whose lifecycle is owned by another host
  /// surface (Round 35: the orchestrator launches its own claude
  /// PTY via `orchestrator_launch_claude` and passes the returned
  /// terminal_id here). When set, the cleanup path also skips
  /// `terminal_shutdown` — the orchestrator owns that lifecycle.
  existingTerminalId?: string;
  /// task21 / AC-3.3 Round 39: workspace tab id, passed through to
  /// `spawnTerminal` so the registry indexes the PTY under this id.
  /// The host RPC bridge needs this for `target_tab_id → terminal_id`
  /// resolution. Ignored when `existingTerminalId` is set (the
  /// orchestrator's path already supplied its own tab id Rust-side).
  tabId?: string;
}

export function TerminalMeshView({
  active,
  cwd,
  workspaceName,
  existingTerminalId,
  tabId,
}: TerminalMeshViewProps) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const terminalIdRef = useRef<string | null>(null);
  const disposedRef = useRef(false);
  const wasActiveRef = useRef(false);
  const [error, setError] = useState<TerminalMeshErrorDto | null>(null);
  const [ready, setReady] = useState(false);
  const [status, setStatus] = useState<TerminalRunStatus>({ kind: "running" });
  const [ideError, setIdeError] = useState<IdeHandoffErrorDto | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);

  const onOpenIde = useCallback(async () => {
    if (!cwd) return;
    setIdeError(null);
    try {
      await openWorkspaceInIde(cwd);
    } catch (err) {
      if (isIdeHandoffErrorDto(err)) setIdeError(err);
    }
  }, [cwd]);

  const onRevealFinder = useCallback(async () => {
    if (!cwd) return;
    setIdeError(null);
    try {
      await revealWorkspaceInFinder(cwd);
    } catch (err) {
      if (isIdeHandoffErrorDto(err)) setIdeError(err);
    }
  }, [cwd]);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    // Reset the shared dispose flag at the start of every setup so
    // React StrictMode's setup → cleanup → setup replay doesn't see
    // the prior cleanup's `true` and immediately bail out of the
    // scrollback/subscribe path. Each setup gets its own per-setup
    // `cancelled` closure flag below; the dispose ref is shared
    // across setups (via useRef) but its meaning is "the CURRENT
    // setup has been torn down".
    disposedRef.current = false;
    // Closure-local cancellation flag — captured per-setup so a
    // stale event from a PRIOR StrictMode setup's listener cannot
    // be revived by the newer setup's `disposedRef = false` reset.
    let cancelled = false;
    let unlisten: (() => void) | null = null;

    const term = new Terminal({
      convertEol: true,
      fontFamily: "ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace",
      fontSize: 13,
      theme: { background: "#1e1e1e", foreground: "#d4d4d4" },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(container);
    try {
      fit.fit();
    } catch {
      // Hidden container at first paint can throw; safe to ignore —
      // the activation-effect below will retry once visible.
    }
    termRef.current = term;
    fitRef.current = fit;

    (async () => {
      try {
        // Round 35 (task20): orchestrator path bypasses spawn — the
        // host already created the PTY and recorded it in the
        // terminal registry. We just subscribe.
        const terminalId: string =
          existingTerminalId ??
          (
            await spawnTerminal({
              cols: term.cols,
              rows: term.rows,
              cwd,
              tabId,
            })
          ).terminalId;
        // Recheck after each await: if the component unmounted while
        // the await was in flight, shut the just-spawned terminal
        // down immediately and bail (but NEVER for an existing
        // terminal — its lifecycle is owned by the caller).
        if (cancelled || disposedRef.current) {
          if (!existingTerminalId) {
            await shutdownTerminal(terminalId).catch(() => {});
          }
          return;
        }
        terminalIdRef.current = terminalId;

        try {
          const scrollback = await readTerminalScrollback(
            terminalId,
            INITIAL_SCROLLBACK_BYTES,
          );
          if (cancelled || disposedRef.current) return;
          if (scrollback) term.write(scrollback);
        } catch {
          // Best-effort restore; if cancellation happened mid-flight
          // bail out before installing a listener — otherwise the
          // post-unmount listener-leak guarantee is lost.
          if (cancelled || disposedRef.current) return;
        }

        const resolvedUnlisten = await subscribeTerminalEvents(
          terminalId,
          (env) => {
            // Dual guard: closure-local `cancelled` (per-setup) AND
            // shared `disposedRef` (current setup). The closure
            // local catches the StrictMode-replay case where a
            // newer setup resets `disposedRef` to false — without
            // this guard, a stale event from a PRIOR setup's
            // listener could write into its disposed `term`.
            if (cancelled || disposedRef.current) return;
            handleEvent(env, term, setStatus);
          },
        );
        if (cancelled || disposedRef.current) {
          // Cleanup already ran; immediately drop the just-installed
          // listener instead of leaking it.
          resolvedUnlisten();
          return;
        }
        unlisten = resolvedUnlisten;
        setReady(true);
      } catch (err) {
        if (cancelled || disposedRef.current) return;
        if (isTerminalMeshErrorDto(err)) setError(err);
        else
          setError({
            kind: "io",
            context: "spawn",
            message: typeof err === "string" ? err : JSON.stringify(err),
          });
      }
    })();

    const onResize = () => {
      const f = fitRef.current;
      const id = terminalIdRef.current;
      if (!f || !id) return;
      try {
        f.fit();
      } catch {
        return;
      }
      void resizeTerminal(id, term.cols, term.rows).catch(() => {});
    };
    window.addEventListener("resize", onResize);
    const inputDisposable = term.onData((data) => {
      const id = terminalIdRef.current;
      if (!id) return;
      void writeTerminalStdin(id, data).catch(() => {});
    });

    return () => {
      cancelled = true;
      disposedRef.current = true;
      window.removeEventListener("resize", onResize);
      inputDisposable.dispose();
      if (unlisten) unlisten();
      const id = terminalIdRef.current;
      // Only shut down terminals we own. The orchestrator (or any
      // future host surface that passes `existingTerminalId`) owns
      // the underlying PTY lifecycle.
      if (id && !existingTerminalId) {
        void shutdownTerminal(id).catch(() => {});
      }
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
    };
  }, []);

  // Inactive→active transitions: re-fit and push the new size back
  // to the actor. xterm doesn't measure correctly while hidden, so
  // we deferred the initial fit; this effect makes sure the first
  // visible paint sees the right cols/rows.
  useEffect(() => {
    if (!active) {
      wasActiveRef.current = false;
      return;
    }
    if (wasActiveRef.current) return;
    wasActiveRef.current = true;
    const f = fitRef.current;
    const term = termRef.current;
    const id = terminalIdRef.current;
    if (!f || !term) return;
    try {
      f.fit();
    } catch {
      return;
    }
    if (id) {
      void resizeTerminal(id, term.cols, term.rows).catch(() => {});
    }
  }, [active]);

  return (
    <div
      className={`terminal-mesh-view ${active ? "terminal-mesh-view--active" : "terminal-mesh-view--hidden"}`}
      data-terminal-active={active ? "true" : "false"}
    >
      <header className="terminal-mesh-view__status-strip" role="status">
        <span className="terminal-mesh-view__status-label">
          {renderStatusLabel(status)}
        </span>
        {cwd && (
          <div className="terminal-mesh-view__status-actions">
            <button
              type="button"
              className="terminal-mesh-view__status-action"
              onClick={() => void onOpenIde()}
            >
              Cursor 中打开
            </button>
            <button
              type="button"
              className="terminal-mesh-view__status-action"
              onClick={() => void onRevealFinder()}
            >
              Finder 中显示
            </button>
            <button
              type="button"
              className="terminal-mesh-view__settings-toggle"
              aria-expanded={settingsOpen}
              aria-label="工作区设置"
              onClick={() => setSettingsOpen((v) => !v)}
            >
              ⚙ 设置
            </button>
          </div>
        )}
      </header>
      {ideError && (
        <p
          className="terminal-mesh-view__ide-error"
          role="alert"
          data-ide-handoff-error={ideError.kind}
        >
          {ideErrorMessage(ideError)}
        </p>
      )}
      {error && (
        <aside
          className="bootstrap-card bootstrap-card--error"
          data-terminal-mesh-error={error.kind}
        >
          <h3>终端启动失败</h3>
          <p>
            <code>{error.kind}</code>
          </p>
        </aside>
      )}
      <div ref={containerRef} className="terminal-mesh-view__xterm" />
      {!ready && !error && (
        <p className="placeholder__hint">正在启动终端…</p>
      )}
      {settingsOpen && cwd && (
        <WorkspaceSettingsPanel
          workspacePath={cwd}
          workspaceName={workspaceName}
          ideError={ideError}
          onOpenIde={onOpenIde}
          onRevealFinder={onRevealFinder}
          onClose={() => setSettingsOpen(false)}
          onIdeError={setIdeError}
        />
      )}
    </div>
  );
}

function WorkspaceSettingsPanel(props: {
  workspacePath: string;
  workspaceName?: string;
  /// Most recent typed IDE handoff error from the surrounding view.
  /// The panel overlays the strip-level `.terminal-mesh-view__ide-
  /// error` line, so we render the same error inside the overlay so
  /// it stays visible while the user is in the panel.
  ideError: IdeHandoffErrorDto | null;
  onOpenIde: () => Promise<void> | void;
  onRevealFinder: () => Promise<void> | void;
  onClose: () => void;
  onIdeError: (e: IdeHandoffErrorDto | null) => void;
}) {
  const {
    workspacePath,
    workspaceName,
    ideError,
    onOpenIde,
    onRevealFinder,
    onClose,
    onIdeError,
  } = props;
  const headerLabel =
    workspaceName ??
    workspacePath.split("/").filter(Boolean).pop() ??
    workspacePath;
  return (
    <section
      className="terminal-mesh-view__settings-panel"
      role="dialog"
      aria-label="工作区设置"
    >
      <header className="terminal-mesh-view__settings-header">
        <h3>工作区设置 · {headerLabel}</h3>
        <button
          type="button"
          className="terminal-mesh-view__settings-close"
          onClick={onClose}
          aria-label="关闭设置"
        >
          ×
        </button>
      </header>
      <div className="terminal-mesh-view__settings-body">
        <dl className="terminal-mesh-view__settings-meta">
          <dt>路径</dt>
          <dd>
            <code>{workspacePath}</code>
          </dd>
        </dl>
        <div className="terminal-mesh-view__settings-actions">
          <button type="button" onClick={() => void onOpenIde()}>
            Cursor 中打开
          </button>
          <button type="button" onClick={() => void onRevealFinder()}>
            Finder 中显示
          </button>
        </div>
        {ideError && (
          <aside
            className="terminal-mesh-view__settings-error"
            role="alert"
            data-ide-handoff-error={ideError.kind}
          >
            <code>{ideError.kind}</code>: {ideErrorMessage(ideError)}
          </aside>
        )}
        <hr className="terminal-mesh-view__settings-divider" />
        <h4 className="terminal-mesh-view__settings-subhead">IDE 偏好</h4>
        <IdePreferencePane onError={onIdeError} />
      </div>
    </section>
  );
}

function ideErrorMessage(e: IdeHandoffErrorDto): string {
  switch (e.kind) {
    case "ideNotInPath":
      return `IDE 命令 ${e.command} 不在 PATH 中,请在 “工作区 → 设置” 修改。`;
    case "notADirectory":
      return `不是有效目录: ${e.path}`;
    case "spawnFailed":
      return `启动 ${e.command} 失败: ${e.message}`;
    case "io":
      return `IO 错误 (${e.context}): ${e.message}`;
  }
}

export function renderStatusLabel(status: TerminalRunStatus): string {
  switch (status.kind) {
    case "running":
      return "运行中";
    case "exited":
      return status.code === null
        ? "已退出"
        : `已退出 (exit ${status.code})`;
    case "cancelled":
      return "已取消";
  }
}

function handleEvent(
  envelope: TerminalEventEnvelope,
  term: Terminal,
  setStatus: (s: TerminalRunStatus) => void,
): void {
  switch (envelope.event.kind) {
    case "output":
      term.write(Uint8Array.from(envelope.event.bytes));
      break;
    case "exit": {
      const code = envelope.event.code;
      setStatus({ kind: "exited", code });
      const codeLabel = code === null ? "exit ?" : `exit ${code}`;
      term.write(`\r\n\x1b[2m[终端已退出 · ${codeLabel}]\x1b[0m\r\n`);
      break;
    }
    case "cancelled":
      setStatus({ kind: "cancelled" });
      term.write("\r\n\x1b[2m[终端已关闭]\x1b[0m\r\n");
      break;
    case "resize":
    case "needsAttention":
      // Resize is observability-only at the UI; NeedsAttention is
      // consumed by the notification surface (task22) rather than the
      // terminal renderer.
      break;
  }
}
