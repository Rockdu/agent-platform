import { useEffect, useRef, useState } from "react";
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
}

export function TerminalMeshView({ active }: TerminalMeshViewProps) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const terminalIdRef = useRef<string | null>(null);
  const disposedRef = useRef(false);
  const wasActiveRef = useRef(false);
  const [error, setError] = useState<TerminalMeshErrorDto | null>(null);
  const [ready, setReady] = useState(false);
  const [status, setStatus] = useState<TerminalRunStatus>({ kind: "running" });

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    // Closure-local cancellation flag. Mirrored to disposedRef so the
    // async event handler can also bail when the component is gone.
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
        const { terminalId } = await spawnTerminal({
          cols: term.cols,
          rows: term.rows,
        });
        // Recheck after each await: if the component unmounted while
        // the await was in flight, shut the just-spawned terminal
        // down immediately and bail.
        if (cancelled) {
          await shutdownTerminal(terminalId).catch(() => {});
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
          // Best-effort restore; ignore failures.
        }

        const resolvedUnlisten = await subscribeTerminalEvents(
          terminalId,
          (env) => {
            // Guard against disposed terminals (close/switch raced
            // the in-flight listen()).
            if (disposedRef.current) return;
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
      if (id) void shutdownTerminal(id).catch(() => {});
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
      </header>
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
    </div>
  );
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
