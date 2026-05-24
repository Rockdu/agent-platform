// TrayBottomCenter tray window root.
//
// Renders the most-recent NeedsAttention events surfaced by the
// host notification service, plus a separate section for papers
// digest entries. Subscribes to `tray://updated` so the list
// refreshes whenever a new event fires (or a dedup-suppressed
// event bumps an existing entry's suppressed_count).

import { useCallback, useEffect, useState } from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import {
  notificationClearPapersTrayEntries,
  notificationClearTrayEntries,
  notificationListRecentPapersTrayEntries,
  notificationListRecentTrayEntries,
  type PapersTrayEntryDto,
  type TrayEntryDto,
} from "./notification";

export function TrayApp() {
  const [entries, setEntries] = useState<TrayEntryDto[]>([]);
  const [papers, setPapers] = useState<PapersTrayEntryDto[]>([]);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      const [list, paperList] = await Promise.all([
        notificationListRecentTrayEntries(),
        notificationListRecentPapersTrayEntries(),
      ]);
      setEntries(list);
      setPapers(paperList);
      setError(null);
    } catch (err) {
      setError(typeof err === "string" ? err : JSON.stringify(err));
    }
  }, []);

  useEffect(() => {
    void refresh();
    let unlisten: UnlistenFn | null = null;
    let cancelled = false;
    void (async () => {
      const u = await listen<unknown>("tray://updated", () => {
        void refresh();
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
  }, [refresh]);

  const onClear = useCallback(async () => {
    try {
      // Clear both rings together — the papers section is rendered in
      // the same tray window as the terminal entries, so the global
      // "清空" button must wipe both. Run in parallel since they hit
      // independent ring buffers on the host.
      await Promise.all([
        notificationClearTrayEntries(),
        notificationClearPapersTrayEntries(),
      ]);
      await refresh();
    } catch (err) {
      setError(typeof err === "string" ? err : JSON.stringify(err));
    }
  }, [refresh]);

  return (
    <div className="tray-app" role="region" aria-label="通知托盘">
      <header className="tray-app__header">
        <h1>最近通知</h1>
        <button type="button" onClick={() => void onClear()} className="tray-app__clear">
          清空
        </button>
      </header>
      {error && (
        <aside className="tray-app__error" role="alert">
          {error}
        </aside>
      )}
      {entries.length === 0 ? (
        <p className="tray-app__empty">还没有任务事件。</p>
      ) : (
        <ul className="tray-app__list">
          {entries.map((e) => (
            <li
              key={e.id}
              className={`tray-app__item tray-app__item--${e.severity}${
                e.isOrchestrator ? " tray-app__item--orchestrator" : ""
              }`}
            >
              <div className="tray-app__item-head">
                {e.isOrchestrator && (
                  <span className="tray-app__orch-badge" title="orchestrator (claude)">
                    🤖 claude
                  </span>
                )}
                <code className="tray-app__source">
                  {e.pluginId} · {e.terminalId.slice(0, 8)}…
                </code>
                <span className="tray-app__kind">{e.kindName}</span>
                {e.suppressedCount > 0 && (
                  <span className="tray-app__suppressed" title="dedup window 内被抑制的事件数">
                    +{e.suppressedCount}
                  </span>
                )}
              </div>
              <p className="tray-app__summary">{e.summary}</p>
              <time className="tray-app__time" dateTime={new Date(e.firedAtUnixMs).toISOString()}>
                {new Date(e.firedAtUnixMs).toLocaleTimeString()}
              </time>
            </li>
          ))}
        </ul>
      )}
      {papers.length > 0 && (
        <section className="tray-app__papers" aria-label="每日论文推荐">
          <header className="tray-app__papers-header">
            <h2>每日论文推荐</h2>
          </header>
          <ul className="tray-app__list tray-app__list--papers">
            {papers.map((p) => (
              <li key={p.arxivId} className="tray-app__item tray-app__item--papers">
                <div className="tray-app__item-head">
                  <code className="tray-app__source">{p.arxivId}</code>
                </div>
                <p className="tray-app__summary">
                  <strong>{p.title}</strong>
                </p>
                <p className="tray-app__summary tray-app__summary--abstract">
                  {p.abstractSnippet}
                </p>
                <a
                  className="tray-app__paper-link"
                  href={p.absUrl}
                  target="_blank"
                  rel="noreferrer"
                >
                  arXiv ↗
                </a>
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}
