// Papers tab frontend.
//
// Renders the daily digest (from the host's papers SQLite — fields
// `starred` and `readAt` hydrated via LEFT JOIN), a manual search
// form whose results are labelled with `source: "manual"`, a star
// toggle per paper persisted to `user_paper_state`, a mark-read
// affordance, and a "refresh now" button gated by the persistent
// rate-limit cooldown returned by `papers_get_cooldown_state` so the
// button reflects the real cooldown window even after restart.

import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { usePluginCapabilityValue } from "../../../src/plugin-lifecycle";
import type { PaperRecord } from "../types";

const FIXTURE_BANNER = "papers-plugin-active";

interface OptInState {
  enabled: boolean | null;
}

interface CooldownState {
  lastFetchedAt: string | null;
  lastError: string | null;
  secondsUntilReady: number;
  rateLimitSeconds: number;
}

export default function PapersPanel() {
  // The capability handle is minted by the host when PluginRoot mounts
  // this component and rotated on every remount. Every papers_*
  // Tauri command requires it; without a valid handle the host
  // rejects the call so other plugins cannot invoke the papers
  // surface and bypass the user-confirmed opt-in flow.
  const capabilityValue = usePluginCapabilityValue();
  const capability = capabilityValue.capability;
  const [digest, setDigest] = useState<PaperRecord[]>([]);
  const [manual, setManual] = useState<PaperRecord[]>([]);
  const [query, setQuery] = useState("");
  const [optIn, setOptIn] = useState<boolean | null>(null);
  const [cooldown, setCooldown] = useState<CooldownState | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [info, setInfo] = useState<string | null>(null);

  const refreshDigest = useCallback(async () => {
    try {
      // Two parallel source-filtered calls so heavy manual searches
      // (more than `limit` recent rows) cannot evict the scheduled
      // digest from the UI. Mixing both sources in one limited query
      // would surface only the newest rows, and manual rows have
      // newer fetched_at than the daily 10am scheduled fire.
      const [scheduled, manualList] = await Promise.all([
        invoke<PaperRecord[]>("papers_list_recent", {
          capability,
          limit: 30,
          source: "scheduled",
        }),
        invoke<PaperRecord[]>("papers_list_recent", {
          capability,
          limit: 30,
          source: "manual",
        }),
      ]);
      setDigest(scheduled);
      setManual(manualList);
      setError(null);
    } catch (err) {
      setError(stringify(err));
    }
  }, [capability]);

  const refreshOptIn = useCallback(async () => {
    try {
      const r = await invoke<OptInState>("papers_get_opt_in", { capability });
      setOptIn(r.enabled);
    } catch (err) {
      setError(stringify(err));
    }
  }, [capability]);

  const refreshCooldown = useCallback(async () => {
    try {
      const r = await invoke<CooldownState>("papers_get_cooldown_state", { capability });
      setCooldown(r);
    } catch (err) {
      setError(stringify(err));
    }
  }, [capability]);

  useEffect(() => {
    void refreshDigest();
    void refreshOptIn();
    void refreshCooldown();
  }, [refreshDigest, refreshOptIn, refreshCooldown]);

  // Poll the cooldown state once per second while there is an active
  // cooldown so the button re-enables when the window expires.
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);
  useEffect(() => {
    if (cooldown && cooldown.secondsUntilReady > 0) {
      if (intervalRef.current === null) {
        intervalRef.current = setInterval(() => {
          void refreshCooldown();
        }, 1000);
      }
    } else if (intervalRef.current !== null) {
      clearInterval(intervalRef.current);
      intervalRef.current = null;
    }
    return () => {
      if (intervalRef.current !== null) {
        clearInterval(intervalRef.current);
        intervalRef.current = null;
      }
    };
  }, [cooldown, refreshCooldown]);

  const setOptInEnabled = useCallback(
    async (enabled: boolean) => {
      try {
        await invoke<void>("papers_set_opt_in", { capability, enabled });
        setOptIn(enabled);
        setInfo(
          enabled
            ? "已开启每日 arXiv 推荐 — 10:00 自动拉取。"
            : "已关闭每日推荐。任何关键词都不会发送到 arXiv。",
        );
      } catch (err) {
        setError(stringify(err));
      }
    },
    [capability],
  );

  const onSearch = useCallback(async () => {
    if (!query.trim() || busy) return;
    setBusy(true);
    setInfo(null);
    try {
      const results = await invoke<PaperRecord[]>("papers_search", { capability, query });
      setManual((prev) => mergeUnique([...results, ...prev]));
      setInfo(`找到 ${results.length} 篇新论文`);
      setError(null);
    } catch (err) {
      setError(stringify(err));
    } finally {
      setBusy(false);
      void refreshCooldown();
    }
  }, [capability, query, busy, refreshCooldown]);

  const onRefreshNow = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    setInfo(null);
    try {
      const count = await invoke<number>("papers_refresh_now", { capability });
      setInfo(`已拉取 ${count} 篇论文`);
      await refreshDigest();
    } catch (err) {
      setError(stringify(err));
    } finally {
      setBusy(false);
      void refreshCooldown();
    }
  }, [capability, busy, refreshDigest, refreshCooldown]);

  const onToggleStar = useCallback(
    async (arxivId: string, currentlyStarred: boolean) => {
      try {
        await invoke<void>("papers_toggle_star", {
          capability,
          arxivId,
          starred: !currentlyStarred,
        });
        await refreshDigest();
      } catch (err) {
        setError(stringify(err));
      }
    },
    [capability, refreshDigest],
  );

  const onMarkRead = useCallback(
    async (arxivId: string) => {
      try {
        await invoke<void>("papers_mark_read", { capability, arxivId });
        await refreshDigest();
      } catch (err) {
        setError(stringify(err));
      }
    },
    [capability, refreshDigest],
  );

  const cooldownDisabled = !!cooldown && cooldown.secondsUntilReady > 0;
  const fetchDisabled = busy || cooldownDisabled || optIn !== true;

  const refreshTitle = useMemo(() => {
    if (optIn !== true) return "需要先开启每日推荐";
    if (cooldownDisabled) {
      return `速率限制：${cooldown!.secondsUntilReady} 秒后可再拉取`;
    }
    return "立即拉取";
  }, [optIn, cooldownDisabled, cooldown]);

  const optInBanner = useMemo(() => {
    if (optIn === null) {
      return (
        <aside className="papers-banner papers-banner--prompt" role="alert">
          <p>
            首次使用需要授权 arXiv 推荐：会扫描 <code>.claude/</code>{" "}
            与最近的 git commit 提取关键词（不发送原文）。
          </p>
          <div className="papers-banner__actions">
            <button type="button" onClick={() => void setOptInEnabled(true)}>
              允许
            </button>
            <button type="button" onClick={() => void setOptInEnabled(false)}>
              拒绝
            </button>
          </div>
        </aside>
      );
    }
    if (optIn === false) {
      return (
        <aside className="papers-banner papers-banner--disabled">
          每日推荐已关闭。
          <button type="button" onClick={() => void setOptInEnabled(true)}>
            重新开启
          </button>
        </aside>
      );
    }
    return null;
  }, [optIn, setOptInEnabled]);

  return (
    <section className="papers" data-fixture={FIXTURE_BANNER}>
      <header className="papers__header">
        <h2>论文</h2>
        <div className="papers__actions">
          <button
            type="button"
            onClick={() => void onRefreshNow()}
            disabled={fetchDisabled}
            title={refreshTitle}
          >
            立即刷新{cooldownDisabled ? ` (${cooldown!.secondsUntilReady}s)` : ""}
          </button>
        </div>
      </header>

      {optInBanner}

      {cooldown?.lastError && (
        <aside className="papers__error" role="alert">
          上次拉取失败：{cooldown.lastError}
        </aside>
      )}
      {cooldown?.lastFetchedAt && (
        <aside className="papers__info" role="status">
          上次拉取：{cooldown.lastFetchedAt}
        </aside>
      )}

      <form
        className="papers__search"
        onSubmit={(e) => {
          e.preventDefault();
          void onSearch();
        }}
      >
        <input
          aria-label="搜索 arXiv"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="关键词，如 transformer attention"
          disabled={optIn !== true}
        />
        <button type="submit" disabled={fetchDisabled || !query.trim()}>
          搜索
        </button>
      </form>

      {error && (
        <aside className="papers__error" role="alert">
          {error}
        </aside>
      )}
      {info && (
        <aside className="papers__info" role="status">
          {info}
        </aside>
      )}

      <section className="papers__section">
        <h3>今日推荐</h3>
        {digest.length === 0 ? (
          <p className="papers__empty">
            还没有推荐。10:00 自动拉取，或点击 “立即刷新”。
          </p>
        ) : (
          <ul className="papers__list">
            {digest.map((p) => (
              <PaperCard
                key={p.arxivId}
                paper={p}
                onToggleStar={onToggleStar}
                onMarkRead={onMarkRead}
              />
            ))}
          </ul>
        )}
      </section>

      {manual.length > 0 && (
        <section className="papers__section">
          <h3>手动搜索结果</h3>
          <ul className="papers__list">
            {manual.map((p) => (
              <PaperCard
                key={p.arxivId}
                paper={p}
                onToggleStar={onToggleStar}
                onMarkRead={onMarkRead}
                manualBadge
              />
            ))}
          </ul>
        </section>
      )}
    </section>
  );
}

interface PaperCardProps {
  paper: PaperRecord;
  onToggleStar: (arxivId: string, currentlyStarred: boolean) => void;
  onMarkRead: (arxivId: string) => void;
  manualBadge?: boolean;
}

function PaperCard({ paper, onToggleStar, onMarkRead, manualBadge }: PaperCardProps) {
  // Initialise from the record — NOT from local default. After refresh /
  // remount / restart the persisted state in `user_paper_state` is the
  // source of truth.
  const starred = paper.starred;
  const read = paper.readAt !== null;
  return (
    <li className={`papers__card${read ? " papers__card--read" : ""}`}>
      <div className="papers__card-head">
        <code className="papers__arxiv-id">{paper.arxivId}</code>
        {manualBadge && <span className="papers__source-badge">手动</span>}
        {read && <span className="papers__read-badge" title={`已读于 ${paper.readAt}`}>已读</span>}
        <button
          type="button"
          className={`papers__star${starred ? " papers__star--on" : ""}`}
          onClick={() => onToggleStar(paper.arxivId, starred)}
          aria-label={starred ? "取消收藏" : "收藏"}
        >
          {starred ? "★" : "☆"}
        </button>
        {!read && (
          <button
            type="button"
            className="papers__mark-read"
            onClick={() => onMarkRead(paper.arxivId)}
            aria-label="标记已读"
          >
            标记已读
          </button>
        )}
      </div>
      <p className="papers__title">{paper.title}</p>
      <p className="papers__authors">{(paper.authors ?? []).join(", ")}</p>
      <p className="papers__abstract">{paper.abstractSnippet}</p>
      <a
        className="papers__link"
        href={paper.absUrl}
        target="_blank"
        rel="noreferrer"
      >
        arXiv ↗
      </a>
    </li>
  );
}

function mergeUnique(papers: PaperRecord[]): PaperRecord[] {
  const seen = new Set<string>();
  const out: PaperRecord[] = [];
  for (const p of papers) {
    if (seen.has(p.arxivId)) continue;
    seen.add(p.arxivId);
    out.push(p);
  }
  return out;
}

function stringify(err: unknown): string {
  if (typeof err === "string") return err;
  if (err instanceof Error) return err.message;
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}
