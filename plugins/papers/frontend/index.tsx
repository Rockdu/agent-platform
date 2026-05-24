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

  const onOpenUrl = useCallback(
    async (url: string) => {
      try {
        await invoke<void>("papers_open_url", { capability, url });
      } catch (err) {
        setError(stringify(err));
      }
    },
    [capability],
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
      <style>{PAPERS_STYLE}</style>
      <div className="papers__grid-bg" aria-hidden="true" />
      <header className="papers__header">
        <div className="papers__title-wrap">
          <span className="papers__logo" aria-hidden="true">⌬</span>
          <div>
            <h2 className="papers__title">arXiv 智能推荐</h2>
            <p className="papers__subtitle">
              基于近期工作上下文 · 每日 10:00 自动同步
            </p>
          </div>
        </div>
        <div className="papers__actions">
          <button
            type="button"
            className="papers__btn papers__btn--primary"
            onClick={() => void onRefreshNow()}
            disabled={fetchDisabled}
            title={refreshTitle}
          >
            <span className={`papers__btn-dot${busy ? " papers__btn-dot--spin" : ""}`} aria-hidden="true" />
            {busy
              ? "拉取中…"
              : `立即刷新${cooldownDisabled ? ` · ${cooldown!.secondsUntilReady}s` : ""}`}
          </button>
        </div>
      </header>

      {optInBanner}

      {cooldown?.lastError && (
        <aside className="papers__error" role="alert">
          ⚠ 上次拉取失败：{cooldown.lastError}
        </aside>
      )}
      {cooldown?.lastFetchedAt && (
        <aside className="papers__meta" role="status">
          ◷ 上次同步：{cooldown.lastFetchedAt}
        </aside>
      )}

      <form
        className="papers__search"
        onSubmit={(e) => {
          e.preventDefault();
          void onSearch();
        }}
      >
        <span className="papers__search-icon" aria-hidden="true">⌕</span>
        <input
          aria-label="搜索 arXiv"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="输入关键词检索 arXiv，如 transformer attention"
          disabled={optIn !== true}
        />
        <button
          type="submit"
          className="papers__btn papers__btn--ghost"
          disabled={fetchDisabled || !query.trim()}
        >
          搜索
        </button>
      </form>

      {error && (
        <aside className="papers__error" role="alert">
          ⚠ {error}
        </aside>
      )}
      {info && (
        <aside className="papers__info" role="status">
          ✓ {info}
        </aside>
      )}

      <section className="papers__section">
        <h3 className="papers__section-title">
          <span className="papers__section-bar" aria-hidden="true" />
          今日推荐
          <span className="papers__count">{digest.length}</span>
        </h3>
        {digest.length === 0 ? (
          <p className="papers__empty">
            暂无推荐。每日 10:00 自动拉取，或点击右上角「立即刷新」。
          </p>
        ) : (
          <ul className="papers__list">
            {digest.map((p) => (
              <PaperCard
                key={p.arxivId}
                paper={p}
                onToggleStar={onToggleStar}
                onMarkRead={onMarkRead}
                onOpenUrl={onOpenUrl}
              />
            ))}
          </ul>
        )}
      </section>

      {manual.length > 0 && (
        <section className="papers__section">
          <h3 className="papers__section-title">
            <span className="papers__section-bar papers__section-bar--manual" aria-hidden="true" />
            手动搜索结果
            <span className="papers__count">{manual.length}</span>
          </h3>
          <ul className="papers__list">
            {manual.map((p) => (
              <PaperCard
                key={p.arxivId}
                paper={p}
                onToggleStar={onToggleStar}
                onMarkRead={onMarkRead}
                onOpenUrl={onOpenUrl}
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
  onOpenUrl: (url: string) => void;
  manualBadge?: boolean;
}

function PaperCard({ paper, onToggleStar, onMarkRead, onOpenUrl, manualBadge }: PaperCardProps) {
  // Initialise from the record — NOT from local default. After refresh /
  // remount / restart the persisted state in `user_paper_state` is the
  // source of truth.
  const starred = paper.starred;
  const read = paper.readAt !== null;
  return (
    <li className={`papers__card${read ? " papers__card--read" : ""}${starred ? " papers__card--starred" : ""}`}>
      <div className="papers__card-head">
        <code className="papers__arxiv-id">{paper.arxivId}</code>
        {manualBadge && <span className="papers__source-badge">手动</span>}
        {read && <span className="papers__read-badge" title={`已读于 ${paper.readAt}`}>已读</span>}
        <span className="papers__card-head-spacer" />
        <button
          type="button"
          className={`papers__star${starred ? " papers__star--on" : ""}`}
          onClick={() => onToggleStar(paper.arxivId, starred)}
          aria-label={starred ? "取消收藏" : "收藏"}
          title={starred ? "取消收藏" : "收藏"}
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
      <div className="papers__card-foot">
        <button
          type="button"
          className="papers__link"
          onClick={() => onOpenUrl(paper.absUrl)}
          title={paper.absUrl}
        >
          在 arXiv 打开 <span aria-hidden="true">↗</span>
        </button>
      </div>
    </li>
  );
}

const PAPERS_STYLE = `
.papers {
  --pp-bg: #0a0e16;
  --pp-bg2: #0d1320;
  --pp-panel: rgba(20, 28, 44, 0.72);
  --pp-border: rgba(86, 130, 184, 0.22);
  --pp-cyan: #38e1ff;
  --pp-cyan-dim: rgba(56, 225, 255, 0.14);
  --pp-violet: #8b7bff;
  --pp-text: #d6e2f2;
  --pp-muted: #7e8ba6;
  position: relative;
  min-height: 100%;
  padding: 22px 26px 40px;
  color: var(--pp-text);
  background:
    radial-gradient(1200px 480px at 18% -8%, rgba(56,225,255,0.10), transparent 60%),
    radial-gradient(900px 420px at 100% 0%, rgba(139,123,255,0.10), transparent 55%),
    linear-gradient(180deg, var(--pp-bg) 0%, var(--pp-bg2) 100%);
  font-family: -apple-system, "SF Pro Text", "Helvetica Neue", "PingFang SC", system-ui, sans-serif;
  overflow: hidden;
}
.papers__grid-bg {
  position: absolute;
  inset: 0;
  pointer-events: none;
  background-image:
    linear-gradient(rgba(86,130,184,0.06) 1px, transparent 1px),
    linear-gradient(90deg, rgba(86,130,184,0.06) 1px, transparent 1px);
  background-size: 34px 34px;
  mask-image: linear-gradient(180deg, rgba(0,0,0,0.5), transparent 70%);
  z-index: 0;
}
.papers > *:not(.papers__grid-bg) { position: relative; z-index: 1; }

.papers__header {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 16px;
  margin-bottom: 18px;
}
.papers__title-wrap { display: flex; align-items: center; gap: 14px; }
.papers__logo {
  font-size: 30px;
  line-height: 1;
  color: var(--pp-cyan);
  text-shadow: 0 0 14px rgba(56,225,255,0.7);
  animation: pp-pulse 3.4s ease-in-out infinite;
}
@keyframes pp-pulse { 0%,100% { opacity: 0.75; } 50% { opacity: 1; } }
.papers__title {
  margin: 0;
  font-size: 20px;
  font-weight: 700;
  letter-spacing: 0.5px;
  background: linear-gradient(90deg, #eaf6ff, var(--pp-cyan));
  -webkit-background-clip: text;
  background-clip: text;
  -webkit-text-fill-color: transparent;
}
.papers__subtitle { margin: 3px 0 0; font-size: 12px; color: var(--pp-muted); letter-spacing: 0.3px; }

.papers__btn {
  display: inline-flex;
  align-items: center;
  gap: 8px;
  border: 1px solid var(--pp-border);
  border-radius: 10px;
  padding: 9px 16px;
  font-size: 13px;
  font-weight: 600;
  color: var(--pp-text);
  background: rgba(30,42,66,0.6);
  cursor: pointer;
  transition: all 0.16s ease;
  -webkit-backdrop-filter: blur(8px);
  backdrop-filter: blur(8px);
}
.papers__btn:hover:not(:disabled) { border-color: var(--pp-cyan); box-shadow: 0 0 0 1px var(--pp-cyan-dim), 0 6px 18px rgba(0,0,0,0.35); transform: translateY(-1px); }
.papers__btn:disabled { opacity: 0.4; cursor: not-allowed; }
.papers__btn--primary {
  color: #04121a;
  background: linear-gradient(135deg, var(--pp-cyan), #6ad6ff);
  border-color: transparent;
  box-shadow: 0 0 18px rgba(56,225,255,0.35);
}
.papers__btn--primary:hover:not(:disabled) { box-shadow: 0 0 26px rgba(56,225,255,0.55); }
.papers__btn-dot { width: 7px; height: 7px; border-radius: 50%; background: #04121a; opacity: 0.85; }
.papers__btn--ghost .papers__btn-dot, .papers__btn-dot--spin { background: var(--pp-cyan); }
.papers__btn-dot--spin { animation: pp-blink 0.8s steps(2) infinite; }
@keyframes pp-blink { 0% { opacity: 0.2; } 100% { opacity: 1; } }

.papers__search {
  display: flex;
  align-items: center;
  gap: 10px;
  margin: 4px 0 22px;
  padding: 6px 6px 6px 14px;
  border: 1px solid var(--pp-border);
  border-radius: 12px;
  background: var(--pp-panel);
  -webkit-backdrop-filter: blur(10px);
  backdrop-filter: blur(10px);
  transition: border-color 0.16s ease, box-shadow 0.16s ease;
}
.papers__search:focus-within { border-color: var(--pp-cyan); box-shadow: 0 0 0 3px var(--pp-cyan-dim); }
.papers__search-icon { color: var(--pp-cyan); font-size: 16px; }
.papers__search input {
  flex: 1;
  border: none;
  background: transparent;
  color: var(--pp-text);
  font-size: 14px;
  outline: none;
  padding: 8px 0;
}
.papers__search input::placeholder { color: var(--pp-muted); }
.papers__search input:disabled { opacity: 0.5; }

.papers__section { margin-top: 26px; }
.papers__section-title {
  display: flex;
  align-items: center;
  gap: 10px;
  margin: 0 0 14px;
  font-size: 14px;
  font-weight: 700;
  letter-spacing: 1px;
  color: #c8d6ec;
  text-transform: none;
}
.papers__section-bar {
  width: 4px; height: 16px; border-radius: 2px;
  background: linear-gradient(180deg, var(--pp-cyan), #2f8fff);
  box-shadow: 0 0 10px rgba(56,225,255,0.6);
}
.papers__section-bar--manual { background: linear-gradient(180deg, var(--pp-violet), #c06bff); box-shadow: 0 0 10px rgba(139,123,255,0.6); }
.papers__count {
  font-size: 11px;
  font-weight: 700;
  color: var(--pp-cyan);
  background: var(--pp-cyan-dim);
  border: 1px solid var(--pp-border);
  border-radius: 999px;
  padding: 1px 9px;
  font-variant-numeric: tabular-nums;
}

.papers__list { list-style: none; margin: 0; padding: 0; display: grid; gap: 14px; }
.papers__card {
  position: relative;
  border: 1px solid var(--pp-border);
  border-radius: 14px;
  padding: 16px 18px 14px;
  background: var(--pp-panel);
  -webkit-backdrop-filter: blur(10px);
  backdrop-filter: blur(10px);
  overflow: hidden;
  transition: transform 0.16s ease, border-color 0.16s ease, box-shadow 0.16s ease;
}
.papers__card::before {
  content: "";
  position: absolute;
  left: 0; top: 0; bottom: 0;
  width: 3px;
  background: linear-gradient(180deg, var(--pp-cyan), transparent);
  opacity: 0.7;
}
.papers__card--starred::before { background: linear-gradient(180deg, #ffd76a, transparent); opacity: 0.9; }
.papers__card:hover {
  transform: translateY(-2px);
  border-color: rgba(56,225,255,0.5);
  box-shadow: 0 10px 30px rgba(0,0,0,0.4), 0 0 0 1px var(--pp-cyan-dim);
}
.papers__card--read { opacity: 0.62; }
.papers__card-head { display: flex; align-items: center; gap: 8px; margin-bottom: 9px; }
.papers__card-head-spacer { flex: 1; }
.papers__arxiv-id {
  font-family: "SF Mono", "JetBrains Mono", ui-monospace, monospace;
  font-size: 12px;
  color: var(--pp-cyan);
  background: var(--pp-cyan-dim);
  border: 1px solid var(--pp-border);
  border-radius: 6px;
  padding: 2px 8px;
  letter-spacing: 0.3px;
}
.papers__source-badge, .papers__read-badge {
  font-size: 11px;
  font-weight: 600;
  border-radius: 6px;
  padding: 2px 8px;
}
.papers__source-badge { color: var(--pp-violet); background: rgba(139,123,255,0.16); border: 1px solid rgba(139,123,255,0.3); }
.papers__read-badge { color: var(--pp-muted); background: rgba(126,139,166,0.14); border: 1px solid rgba(126,139,166,0.25); }
.papers__star {
  border: none; background: none; cursor: pointer;
  font-size: 18px; line-height: 1; color: var(--pp-muted);
  transition: transform 0.12s ease, color 0.12s ease;
}
.papers__star:hover { transform: scale(1.2); color: #ffd76a; }
.papers__star--on { color: #ffd76a; text-shadow: 0 0 10px rgba(255,215,106,0.7); }
.papers__mark-read {
  border: 1px solid var(--pp-border);
  background: rgba(30,42,66,0.6);
  color: var(--pp-muted);
  font-size: 11px;
  border-radius: 7px;
  padding: 4px 9px;
  cursor: pointer;
  transition: all 0.14s ease;
}
.papers__mark-read:hover { color: var(--pp-cyan); border-color: var(--pp-cyan); }
.papers__title { margin: 0 0 6px; font-size: 15px; font-weight: 650; line-height: 1.45; color: #eaf2ff; }
.papers__authors { margin: 0 0 8px; font-size: 12px; color: var(--pp-muted); }
.papers__abstract { margin: 0 0 12px; font-size: 13px; line-height: 1.6; color: #b3c1da; }
.papers__card-foot { display: flex; }
.papers__link {
  display: inline-flex; align-items: center; gap: 6px;
  border: 1px solid var(--pp-border);
  background: rgba(56,225,255,0.08);
  color: var(--pp-cyan);
  font-size: 12px; font-weight: 600;
  border-radius: 8px; padding: 6px 12px;
  cursor: pointer;
  transition: all 0.14s ease;
}
.papers__link:hover { background: var(--pp-cyan); color: #04121a; box-shadow: 0 0 16px rgba(56,225,255,0.5); }

.papers__empty {
  margin: 0; padding: 28px; text-align: center;
  border: 1px dashed var(--pp-border); border-radius: 12px;
  color: var(--pp-muted); font-size: 13px;
  background: rgba(20,28,44,0.4);
}
.papers__error, .papers__info, .papers__meta {
  margin: 0 0 12px; padding: 10px 14px;
  border-radius: 10px; font-size: 13px;
  border: 1px solid var(--pp-border);
}
.papers__error { color: #ff9b9b; background: rgba(255,90,90,0.10); border-color: rgba(255,90,90,0.3); }
.papers__info { color: #8fffc4; background: rgba(60,255,170,0.08); border-color: rgba(60,255,170,0.28); }
.papers__meta { color: var(--pp-muted); background: rgba(20,28,44,0.5); }

.papers-banner {
  margin: 0 0 16px; padding: 16px 18px;
  border: 1px solid var(--pp-border); border-radius: 12px;
  background: var(--pp-panel); font-size: 13px; line-height: 1.6;
  color: #c8d6ec;
}
.papers-banner--prompt { border-color: rgba(56,225,255,0.4); box-shadow: 0 0 0 1px var(--pp-cyan-dim); }
.papers-banner code { color: var(--pp-cyan); background: var(--pp-cyan-dim); border-radius: 4px; padding: 1px 5px; font-size: 12px; }
.papers-banner__actions { display: flex; gap: 10px; margin-top: 12px; }
.papers-banner__actions button {
  border: 1px solid var(--pp-border); border-radius: 8px;
  padding: 7px 16px; font-size: 13px; font-weight: 600; cursor: pointer;
  background: rgba(30,42,66,0.6); color: var(--pp-text);
  transition: all 0.14s ease;
}
.papers-banner__actions button:first-child { background: linear-gradient(135deg, var(--pp-cyan), #6ad6ff); color: #04121a; border-color: transparent; }
.papers-banner__actions button:hover { transform: translateY(-1px); }
.papers-banner--disabled { display: flex; align-items: center; gap: 12px; }
.papers-banner--disabled button { margin-left: auto; border: 1px solid var(--pp-cyan); color: var(--pp-cyan); background: transparent; border-radius: 8px; padding: 6px 14px; cursor: pointer; }
`;

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
