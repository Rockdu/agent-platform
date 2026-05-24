-- Initial schema for the papers plugin.
--
-- Lexically first file in `migrations/` so the per-plugin migration
-- runner applies it first. Also embedded into the `papers-plugin`
-- binary via `include_str!` so `PapersStore::open` can apply the
-- schema unconditionally — packaged installs do not ship the source
-- tree, and a missing/failed migration must NOT leave the store in
-- an opened-but-empty state where every `SELECT` fails with
-- `no such table`.
--
-- All statements are idempotent (`CREATE TABLE IF NOT EXISTS`,
-- `INSERT OR IGNORE`) so applying twice (e.g. on every host launch
-- AND via the plugin migration runner in dev) is safe.

CREATE TABLE IF NOT EXISTS daily_recommendations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    arxiv_id TEXT NOT NULL,
    title TEXT NOT NULL,
    authors TEXT NOT NULL,
    abstract_snippet TEXT NOT NULL,
    pdf_url TEXT NOT NULL,
    abs_url TEXT NOT NULL,
    source_query TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'scheduled',
    fetched_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_daily_recommendations_arxiv_id ON daily_recommendations(arxiv_id);
CREATE INDEX IF NOT EXISTS idx_daily_recommendations_fetched_at ON daily_recommendations(fetched_at);

CREATE TABLE IF NOT EXISTS user_paper_state (
    arxiv_id TEXT PRIMARY KEY,
    starred INTEGER NOT NULL DEFAULT 0,
    read_at TEXT
);

-- Singleton row (id = 1) tracking daily-fetch state. The opt-in flag is
-- NULL until the user has answered the first-run prompt; arXiv calls are
-- blocked until it becomes 1.
CREATE TABLE IF NOT EXISTS scheduler_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    last_fired_at TEXT,
    last_arxiv_call_at TEXT,
    last_error TEXT,
    opt_in_enabled INTEGER
);

INSERT OR IGNORE INTO scheduler_state (id) VALUES (1);
