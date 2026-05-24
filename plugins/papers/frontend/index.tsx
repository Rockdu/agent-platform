// Papers tab frontend.
//
// Renders the daily digest (from the host's papers SQLite), a manual
// search form whose results are labelled with `source: "manual"`, a
// star toggle per paper persisted to `user_paper_state`, and a
// "refresh now" button gated by the same rate-limit window the
// scheduler uses (3s between arXiv calls).

import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useMemo, useState } from "react";
import { usePluginCapabilityValue } from "../../../src/plugin-lifecycle";
import type { PaperRecord } from "../types";

const FIXTURE_BANNER = "papers-plugin-active";

interface OptInState {
  enabled: boolean | null;
}

export default function PapersPanel() {
  // Establish plugin capability subscription (also unmount-safe).
  usePluginCapabilityValue();
  const [digest, setDigest] = useState<PaperRecord[]>([]);
  const [manual, setManual] = useState<PaperRecord[]>([]);
  const [query, setQuery] = useState("");
  const [optIn, setOptIn] = useState<boolean | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [info, setInfo] = useState<string | null>(null);

  const refreshDigest = useCallback(async () => {
    try {
      const list = await invoke<PaperRecord[]>("papers_list_recent", { limit: 30 });
      const scheduled = list.filter((p) => p.source !== "manual");
      const manualList = list.filter((p) => p.source === "manual");
      setDigest(scheduled);
      setManual(manualList);
      setError(null);
    } catch (err) {
      setError(stringify(err));
    }
  }, []);

  const refreshOptIn = useCallback(async () => {
    try {
      const r = await invoke<OptInState>("papers_get_opt_in");
      setOptIn(r.enabled);
    } catch (err) {
      setError(stringify(err));
    }
  }, []);

  useEffect(() => {
    void refreshDigest();
    void refreshOptIn();
  }, [refreshDigest, refreshOptIn]);

  const setOptInEnabled = useCallback(
    async (enabled: boolean) => {
      try {
        await invoke<void>("papers_set_opt_in", { enabled });
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
    [],
  );

  const onSearch = useCallback(async () => {
    if (!query.trim() || busy) return;
    setBusy(true);
    setInfo(null);
    try {
      const results = await invoke<PaperRecord[]>("papers_search", { query });
      setManual((prev) => mergeUnique([...results, ...prev]));
      setInfo(`找到 ${results.length} 篇新论文`);
      setError(null);
    } catch (err) {
      setError(stringify(err));
    } finally {
      setBusy(false);
    }
  }, [query, busy]);

  const onRefreshNow = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    setInfo(null);
    try {
      const count = await invoke<number>("papers_refresh_now");
      setInfo(`已拉取 ${count} 篇论文`);
      await refreshDigest();
    } catch (err) {
      setError(stringify(err));
    } finally {
      setBusy(false);
    }
  }, [busy, refreshDigest]);

  const onToggleStar = useCallback(
    async (arxivId: string, currentlyStarred: boolean) => {
      try {
        await invoke<void>("papers_toggle_star", {
          arxivId,
          starred: !currentlyStarred,
        });
        await refreshDigest();
      } catch (err) {
        setError(stringify(err));
      }
    },
    [refreshDigest],
  );

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
            disabled={busy || optIn !== true}
            title={optIn !== true ? "需要先开启每日推荐" : "立即拉取（受 3 秒速率限制）"}
          >
            立即刷新
          </button>
        </div>
      </header>

      {optInBanner}

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
        />
        <button type="submit" disabled={busy || !query.trim()}>
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
  manualBadge?: boolean;
}

function PaperCard({ paper, onToggleStar, manualBadge }: PaperCardProps) {
  const [starred, setStarred] = useState(false);
  return (
    <li className="papers__card">
      <div className="papers__card-head">
        <code className="papers__arxiv-id">{paper.arxivId}</code>
        {manualBadge && <span className="papers__source-badge">手动</span>}
        <button
          type="button"
          className={`papers__star${starred ? " papers__star--on" : ""}`}
          onClick={() => {
            const next = !starred;
            setStarred(next);
            onToggleStar(paper.arxivId, starred);
          }}
          aria-label={starred ? "取消收藏" : "收藏"}
        >
          {starred ? "★" : "☆"}
        </button>
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
