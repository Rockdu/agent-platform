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

export function TerminalMeshView() {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const terminalIdRef = useRef<string | null>(null);
  const [error, setError] = useState<TerminalMeshErrorDto | null>(null);
  const [ready, setReady] = useState(false);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
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
    fit.fit();
    termRef.current = term;
    fitRef.current = fit;

    (async () => {
      try {
        const { terminalId } = await spawnTerminal({
          cols: term.cols,
          rows: term.rows,
        });
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
          if (scrollback) term.write(scrollback);
        } catch {
          // Best-effort restore; ignore failures.
        }

        unlisten = await subscribeTerminalEvents(terminalId, (env) =>
          handleEvent(env, term),
        );
        setReady(true);
      } catch (err) {
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
      f.fit();
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

  return (
    <div className="terminal-mesh-view">
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

function handleEvent(envelope: TerminalEventEnvelope, term: Terminal): void {
  switch (envelope.event.kind) {
    case "output":
      term.write(Uint8Array.from(envelope.event.bytes));
      break;
    case "exit":
      term.write("\r\n\x1b[2m[终端已退出]\x1b[0m\r\n");
      break;
    case "cancelled":
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
