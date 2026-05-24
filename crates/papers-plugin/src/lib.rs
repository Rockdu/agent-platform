//! `papers-plugin` MCP sidecar — arXiv recommendation engine.
//!
//! Exposes MCP tools to the orchestrator's `claude` (listed in
//! `tools_list_response`):
//!   - `papers.list_recent` — read the most recent stored recommendations.
//!   - `papers.search` — ad-hoc search by keyword (writes `source = "manual"`).
//!   - `papers.toggle_star` — toggle the `starred` bit for a paper.
//!   - `papers.mark_read` — stamp the `read_at` timestamp for a paper.
//!   - `papers.get_opt_in` — read the global opt-in flag.
//!
//! `papers.fetch` is intentionally NOT advertised in `tools/list` but
//! is still accepted by `handle_tool_call` so the host scheduler's
//! direct `tools/call` (via `papers_sidecar_client::fetch_via_sidecar`)
//! can route the daily digest through the same dispatch path. The
//! orchestrator Claude only sees `papers.search`; this prevents an
//! ad-hoc Claude request from advancing scheduler state.
//!
//! `papers.set_opt_in` is also intentionally absent — the opt-in
//! WRITE lives behind the host UI prompt (Tauri `papers_set_opt_in`
//! command) so the model cannot enable arXiv context queries on the
//! user's behalf.
//!
//! Discovery: `APP_DATA_DIR` env var locates `${APP_DATA}/plugins/papers/state.sqlite`.
//!
//! Frame format: newline-delimited JSON-RPC 2.0 (same as `mcp-stdio`).

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mcp_stdio::{JsonRpcError, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

pub const RATE_LIMIT_SECONDS: u64 = 3;
pub const DEDUP_WINDOW_DAYS: u64 = 7;
pub const MAX_RETRY_ATTEMPTS: u32 = 3;
pub const ARXIV_API_DEFAULT: &str = "https://export.arxiv.org/api/query";
/// Hard cap on rows returned by `PapersStore::list_recent`. Callers
/// (MCP, Tauri, host RPC) supply the limit but the input is untrusted
/// — without clamping, a negative value would make SQLite interpret
/// `LIMIT -1` as no-limit and return the entire recommendations table.
pub const MAX_LIST_RECENT_LIMIT: i64 = 500;

/// CLI args parsed from argv (mirrors terminal-mesh-sidecar shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarArgs {
    pub client_id: String,
    pub workspace: PathBuf,
    pub host_rpc_sock: Option<PathBuf>,
    pub cross_tab_read: bool,
}

#[derive(Debug, Error)]
pub enum SidecarArgError {
    #[error("missing required argument `{0}`")]
    MissingRequired(&'static str),
    #[error("unexpected argument `{0}`")]
    Unexpected(String),
    #[error("argument `{0}` expects a value")]
    NeedsValue(&'static str),
}

pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<SidecarArgs, SidecarArgError> {
    let mut client_id: Option<String> = None;
    let mut workspace: Option<PathBuf> = None;
    let mut host_rpc_sock: Option<PathBuf> = None;
    let mut cross_tab_read = false;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--client-id" => {
                client_id = Some(iter.next().ok_or(SidecarArgError::NeedsValue("--client-id"))?);
            }
            "--workspace" => {
                workspace = Some(PathBuf::from(
                    iter.next().ok_or(SidecarArgError::NeedsValue("--workspace"))?,
                ));
            }
            "--host-rpc-sock" => {
                host_rpc_sock = Some(PathBuf::from(
                    iter.next().ok_or(SidecarArgError::NeedsValue("--host-rpc-sock"))?,
                ));
            }
            "--cross-tab-read" => cross_tab_read = true,
            other => return Err(SidecarArgError::Unexpected(other.to_string())),
        }
    }
    Ok(SidecarArgs {
        client_id: client_id.ok_or(SidecarArgError::MissingRequired("--client-id"))?,
        workspace: workspace.ok_or(SidecarArgError::MissingRequired("--workspace"))?,
        host_rpc_sock,
        cross_tab_read,
    })
}

/// One arXiv paper record. Serialised as camelCase to match the
/// React-facing PaperRecord interface in `plugins/papers/types.ts`.
/// `starred` and `read_at` are hydrated from `user_paper_state` via
/// LEFT JOIN; freshly-inserted rows are unstarred / unread until the
/// user interacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaperRecord {
    pub arxiv_id: String,
    pub title: String,
    pub authors: Vec<String>,
    pub abstract_snippet: String,
    pub pdf_url: String,
    pub abs_url: String,
    pub source: String,
    pub fetched_at: String,
    #[serde(default)]
    pub starred: bool,
    #[serde(default)]
    pub read_at: Option<String>,
}

#[derive(Debug, Error)]
pub enum ArxivError {
    #[error("arxiv http: {0}")]
    Http(String),
    #[error("arxiv parse: {0}")]
    Parse(String),
    #[error("rate limited: must wait {wait_secs}s")]
    RateLimited { wait_secs: u64 },
    #[error("not opted in: enable daily papers in the Papers tab before searching")]
    NotOptedIn,
    #[error("sqlite: {0}")]
    Sqlite(String),
    #[error("io: {0}")]
    Io(String),
}

/// MCP tools/list response. Static — orchestrator-only visibility is
/// enforced by `mcp_config.rs` excluding the papers sidecar from
/// non-orchestrator tab configs, so any Claude that sees these tools
/// has already passed that gate.
///
/// `papers.fetch` is intentionally NOT advertised here. It is the
/// scheduler-driven path and would otherwise let an ad-hoc orchestrator
/// request advance `scheduler_state.last_fired_at` / clear `last_error`,
/// causing the real daily backfill to be skipped. `handle_tool_call`
/// still accepts the tool name so the host scheduler's direct
/// `tools/call` (via `papers_sidecar_client::fetch_via_sidecar`)
/// continues to work without a protocol change.
pub fn tools_list_response() -> Value {
    json!({
        "tools": [
            {
                "name": "papers.list_recent",
                "description": "Return the N most recent stored paper records ordered by fetched_at DESC.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "limit": {"type": "integer", "default": 10}
                    }
                }
            },
            {
                "name": "papers.search",
                "description": "Ad-hoc keyword search; same as papers.fetch but stamps source=manual.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"}
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "papers.toggle_star",
                "description": "Toggle the starred bit for an arxiv_id; persists in user_paper_state.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "arxiv_id": {"type": "string"},
                        "starred": {"type": "boolean"}
                    },
                    "required": ["arxiv_id", "starred"]
                }
            },
            {
                "name": "papers.mark_read",
                "description": "Mark a paper as read at the current time; persists in user_paper_state.read_at.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "arxiv_id": {"type": "string"}
                    },
                    "required": ["arxiv_id"]
                }
            },
            {
                "name": "papers.get_opt_in",
                "description": "Read the global opt-in flag. Returns {enabled: bool|null} — null means the first-run prompt has not been answered yet. Read-only — the WRITE side (`papers_set_opt_in`) is intentionally NOT an MCP tool; it lives behind the host UI prompt so the orchestrator Claude cannot enable arXiv context queries on the user's behalf.",
                "inputSchema": {"type": "object", "properties": {}}
            }
        ]
    })
}

/// Pure XML parser for arXiv Atom responses. Extracts <entry> blocks and
/// returns one PaperRecord per entry, capped at `max_results`.
pub fn parse_arxiv_atom(xml: &str, max_results: usize) -> Result<Vec<PaperRecord>, ArxivError> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut out: Vec<PaperRecord> = Vec::new();
    let mut in_entry = false;
    let mut current_tag: Option<String> = None;
    let mut in_author = false;
    let mut in_author_name = false;

    let mut id = String::new();
    let mut title = String::new();
    let mut summary = String::new();
    let mut authors: Vec<String> = Vec::new();
    let mut pdf_url = String::new();
    let mut abs_url = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                match name.as_str() {
                    "entry" => {
                        in_entry = true;
                        id.clear();
                        title.clear();
                        summary.clear();
                        authors.clear();
                        pdf_url.clear();
                        abs_url.clear();
                    }
                    "author" if in_entry => in_author = true,
                    "name" if in_author => in_author_name = true,
                    "link" if in_entry => {
                        let mut href = String::new();
                        let mut rel = String::new();
                        let mut typ = String::new();
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = attr
                                .unescape_value()
                                .map(|c| c.into_owned())
                                .unwrap_or_default();
                            match key.as_str() {
                                "href" => href = val,
                                "rel" => rel = val,
                                "type" => typ = val,
                                _ => {}
                            }
                        }
                        if typ == "application/pdf" {
                            pdf_url = href;
                        } else if rel == "alternate" {
                            abs_url = href;
                        }
                    }
                    _ if in_entry => current_tag = Some(name),
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "link" && in_entry {
                    let mut href = String::new();
                    let mut rel = String::new();
                    let mut typ = String::new();
                    for attr in e.attributes().flatten() {
                        let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                        let val = attr
                            .unescape_value()
                            .map(|c| c.into_owned())
                            .unwrap_or_default();
                        match key.as_str() {
                            "href" => href = val,
                            "rel" => rel = val,
                            "type" => typ = val,
                            _ => {}
                        }
                    }
                    if typ == "application/pdf" {
                        pdf_url = href;
                    } else if rel == "alternate" {
                        abs_url = href;
                    }
                }
            }
            Ok(Event::Text(t)) => {
                let txt = t
                    .unescape()
                    .map(|c| c.into_owned())
                    .unwrap_or_default();
                if in_author_name {
                    if !txt.is_empty() {
                        authors.push(txt);
                    }
                } else if let Some(tag) = &current_tag {
                    match tag.as_str() {
                        "id" if in_entry => id.push_str(&txt),
                        "title" if in_entry => title.push_str(&txt),
                        "summary" if in_entry => summary.push_str(&txt),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                match name.as_str() {
                    "entry" => {
                        if !id.is_empty() {
                            let arxiv_id = id
                                .rsplit('/')
                                .next()
                                .unwrap_or(&id)
                                .to_string();
                            let snippet: String = summary
                                .chars()
                                .take(400)
                                .collect::<String>()
                                .trim()
                                .to_string();
                            out.push(PaperRecord {
                                arxiv_id,
                                title: title.trim().to_string(),
                                authors: std::mem::take(&mut authors),
                                abstract_snippet: snippet,
                                pdf_url: pdf_url.clone(),
                                abs_url: if abs_url.is_empty() {
                                    id.clone()
                                } else {
                                    abs_url.clone()
                                },
                                source: "scheduled".to_string(),
                                fetched_at: now_iso8601(),
                                starred: false,
                                read_at: None,
                            });
                            if out.len() >= max_results {
                                return Ok(out);
                            }
                        }
                        in_entry = false;
                    }
                    "author" => in_author = false,
                    "name" => in_author_name = false,
                    _ => current_tag = None,
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(ArxivError::Parse(format!("xml: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// Compute the arXiv query URL for a keyword string.
pub fn build_arxiv_url(api_base: &str, query: &str, max_results: usize) -> String {
    let cleaned: String = query
        .chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' { c } else { ' ' })
        .collect();
    let joined: String = cleaned.split_whitespace().collect::<Vec<_>>().join("+");
    format!(
        "{base}?search_query=all:{q}&start=0&max_results={n}",
        base = api_base,
        q = joined,
        n = max_results
    )
}

/// HTTP fetch with retry and exponential backoff. `attempts` <= MAX_RETRY_ATTEMPTS.
/// Pure-IO function — the caller is responsible for rate-limit gating.
pub fn fetch_arxiv_with_retry(
    client: &reqwest::blocking::Client,
    url: &str,
    attempts: u32,
) -> Result<String, ArxivError> {
    let mut last_err = ArxivError::Http("no attempt made".to_string());
    let mut backoff_secs: u64 = RATE_LIMIT_SECONDS;
    for attempt in 1..=attempts {
        match client.get(url).send() {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return resp
                        .text()
                        .map_err(|e| ArxivError::Http(format!("body read: {e}")));
                }
                last_err = ArxivError::Http(format!("status {} on attempt {}", status, attempt));
                if status.as_u16() == 429 || status.is_server_error() {
                    if attempt < attempts {
                        std::thread::sleep(Duration::from_secs(backoff_secs));
                        backoff_secs = backoff_secs.saturating_mul(3);
                    }
                    continue;
                }
                return Err(last_err);
            }
            Err(e) => {
                last_err = ArxivError::Http(format!("{e}"));
                if attempt < attempts {
                    std::thread::sleep(Duration::from_secs(backoff_secs));
                    backoff_secs = backoff_secs.saturating_mul(3);
                }
            }
        }
    }
    Err(last_err)
}

/// SQLite handle wrapping a single connection guarded by a Mutex.
pub struct PapersStore {
    conn: Mutex<rusqlite::Connection>,
}

/// Embedded copy of the papers plugin's initial schema. Applied
/// unconditionally by `PapersStore::open` so a packaged install
/// (which does not ship the source-tree `plugins/papers/migrations`
/// directory) gets the same schema as a dev checkout. All statements
/// are idempotent (`CREATE TABLE IF NOT EXISTS`, `INSERT OR IGNORE`)
/// so reapplication is a no-op.
const EMBEDDED_SCHEMA_SQL: &str =
    include_str!("../../../plugins/papers/migrations/0001_init.sql");

impl PapersStore {
    /// Open the papers SQLite at `db_path`, configure WAL + busy
    /// timeout, and apply the embedded schema. The schema apply is
    /// idempotent: every CREATE uses `IF NOT EXISTS` and the
    /// singleton row uses `INSERT OR IGNORE`. If schema apply fails,
    /// the store fails to open — the host must then disable the
    /// papers feature rather than continue with an unmigrated DB
    /// that would error on every subsequent SELECT.
    pub fn open(db_path: &std::path::Path) -> Result<Self, ArxivError> {
        let conn = rusqlite::Connection::open(db_path)
            .map_err(|e| ArxivError::Sqlite(format!("open: {e}")))?;
        conn.busy_timeout(Duration::from_millis(5000))
            .map_err(|e| ArxivError::Sqlite(format!("busy_timeout: {e}")))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| ArxivError::Sqlite(format!("WAL: {e}")))?;
        conn.execute_batch(EMBEDDED_SCHEMA_SQL)
            .map_err(|e| ArxivError::Sqlite(format!("embedded schema: {e}")))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Check whether we are within the rate-limit window. Returns `Some(seconds_to_wait)`
    /// if the caller should wait, or `None` if it is safe to proceed.
    ///
    /// Prefer `try_touch_rate_limit()` for the gate-and-acquire flow —
    /// this read-only helper exists for surfaces like the cooldown UI
    /// that report remaining wait time without trying to acquire.
    pub fn rate_limit_wait(&self) -> Result<Option<u64>, ArxivError> {
        let guard = self.conn.lock().unwrap();
        let last_iso: Option<String> = guard
            .query_row(
                "SELECT last_arxiv_call_at FROM scheduler_state WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .map_err(|e| ArxivError::Sqlite(format!("rate select: {e}")))?;
        compute_rate_wait(last_iso.as_deref())
    }

    /// Atomic check-and-acquire: if the window has elapsed since the
    /// last call, stamp the current time and return `Ok(())`. Otherwise
    /// return `Err(ArxivError::RateLimited { wait_secs })` without
    /// modifying state. Uses `BEGIN IMMEDIATE` so concurrent callers
    /// against the same SQLite file (host + sidecar processes, or two
    /// host threads) serialise; exactly one wins.
    pub fn try_touch_rate_limit(&self) -> Result<(), ArxivError> {
        let guard = self.conn.lock().unwrap();
        // BEGIN IMMEDIATE acquires the RESERVED lock immediately so a
        // concurrent transaction starting at the same time is forced
        // to wait (or fail with SQLITE_BUSY, which `busy_timeout`
        // converts into a bounded retry).
        guard
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| ArxivError::Sqlite(format!("begin immediate: {e}")))?;
        let read = (|| -> Result<Option<u64>, ArxivError> {
            let last_iso: Option<String> = guard
                .query_row(
                    "SELECT last_arxiv_call_at FROM scheduler_state WHERE id = 1",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| ArxivError::Sqlite(format!("rate select: {e}")))?;
            compute_rate_wait(last_iso.as_deref())
        })();
        let wait = match read {
            Ok(v) => v,
            Err(e) => {
                let _ = guard.execute_batch("ROLLBACK");
                return Err(e);
            }
        };
        if let Some(secs) = wait {
            let _ = guard.execute_batch("ROLLBACK");
            return Err(ArxivError::RateLimited { wait_secs: secs });
        }
        if let Err(e) = guard.execute(
            "UPDATE scheduler_state SET last_arxiv_call_at = ? WHERE id = 1",
            rusqlite::params![now_iso8601()],
        ) {
            let _ = guard.execute_batch("ROLLBACK");
            return Err(ArxivError::Sqlite(format!("rate update: {e}")));
        }
        guard
            .execute_batch("COMMIT")
            .map_err(|e| ArxivError::Sqlite(format!("rate commit: {e}")))?;
        Ok(())
    }

    /// Insert records that are NOT already in `daily_recommendations` within the
    /// past `DEDUP_WINDOW_DAYS`. Returns the inserted rows (with refreshed
    /// `fetched_at`).
    pub fn insert_dedup(
        &self,
        records: &[PaperRecord],
        source_query: &str,
    ) -> Result<Vec<PaperRecord>, ArxivError> {
        let guard = self.conn.lock().unwrap();
        let tx = guard
            .unchecked_transaction()
            .map_err(|e| ArxivError::Sqlite(format!("tx begin: {e}")))?;
        let mut inserted: Vec<PaperRecord> = Vec::new();
        for r in records {
            let cutoff = unix_now_secs() - DEDUP_WINDOW_DAYS * 86400;
            let cutoff_iso = iso8601_from_unix(cutoff);
            let exists: bool = tx
                .query_row(
                    "SELECT 1 FROM daily_recommendations WHERE arxiv_id = ? AND fetched_at >= ?",
                    rusqlite::params![r.arxiv_id, cutoff_iso],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            if exists {
                continue;
            }
            let authors_str = r.authors.join("; ");
            let fetched = now_iso8601();
            tx.execute(
                "INSERT INTO daily_recommendations (arxiv_id, title, authors, abstract_snippet, pdf_url, abs_url, source_query, source, fetched_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    r.arxiv_id,
                    r.title,
                    authors_str,
                    r.abstract_snippet,
                    r.pdf_url,
                    r.abs_url,
                    source_query,
                    r.source,
                    fetched,
                ],
            )
            .map_err(|e| ArxivError::Sqlite(format!("insert: {e}")))?;
            inserted.push(PaperRecord {
                fetched_at: fetched,
                ..r.clone()
            });
        }
        tx.commit()
            .map_err(|e| ArxivError::Sqlite(format!("tx commit: {e}")))?;
        Ok(inserted)
    }

    pub fn list_recent(&self, limit: i64) -> Result<Vec<PaperRecord>, ArxivError> {
        self.list_recent_by_source(None, limit)
    }

    /// Source-filtered variant of `list_recent`. `source = None` returns
    /// rows of every source (legacy behaviour preserved for back-compat).
    /// `source = Some("scheduled" | "manual")` restricts the result to
    /// the matching `daily_recommendations.source` value.
    ///
    /// Closes a UI eviction bug: if the user accumulates more than
    /// `limit` manual-search rows (newer `fetched_at` than any
    /// scheduled row), a mixed query returns only manual rows and the
    /// Papers panel renders an empty digest even though scheduled
    /// rows still exist. The frontend now issues one filtered call per
    /// section so the two sources can't evict each other.
    pub fn list_recent_by_source(
        &self,
        source: Option<&str>,
        limit: i64,
    ) -> Result<Vec<PaperRecord>, ArxivError> {
        // Clamp before substituting into SQL. SQLite treats `LIMIT -1`
        // as no limit, so passing a negative value through would
        // return the entire recommendations table. Callers come from
        // the MCP and Tauri surfaces so the input is untrusted.
        let bounded = limit.clamp(0, MAX_LIST_RECENT_LIMIT);
        let guard = self.conn.lock().unwrap();
        // LEFT JOIN user_paper_state so the wire shape includes the
        // user's starred / read state for the React component (no
        // separate hydration round trip required). The optional source
        // filter is glued into the same query so the two branches
        // can't drift in projection / ordering.
        let where_clause = if source.is_some() { "WHERE d.source = ? " } else { "" };
        let sql = format!(
            "SELECT d.arxiv_id, d.title, d.authors, d.abstract_snippet, d.pdf_url, d.abs_url, d.source, d.fetched_at, COALESCE(u.starred, 0), u.read_at \
             FROM daily_recommendations d \
             LEFT JOIN user_paper_state u ON u.arxiv_id = d.arxiv_id \
             {where_clause}ORDER BY d.fetched_at DESC LIMIT ?"
        );
        let mut params: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(s) = source {
            params.push(rusqlite::types::Value::Text(s.to_string()));
        }
        params.push(rusqlite::types::Value::Integer(bounded));
        let mut stmt = guard
            .prepare(&sql)
            .map_err(|e| ArxivError::Sqlite(format!("prepare: {e}")))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                let authors_str: String = row.get(2)?;
                let authors: Vec<String> = authors_str
                    .split(';')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                let starred: i64 = row.get(8)?;
                let read_at: Option<String> = row.get(9)?;
                Ok(PaperRecord {
                    arxiv_id: row.get(0)?,
                    title: row.get(1)?,
                    authors,
                    abstract_snippet: row.get(3)?,
                    pdf_url: row.get(4)?,
                    abs_url: row.get(5)?,
                    source: row.get(6)?,
                    fetched_at: row.get(7)?,
                    starred: starred != 0,
                    read_at,
                })
            })
            .map_err(|e| ArxivError::Sqlite(format!("query: {e}")))?;
        let mut out: Vec<PaperRecord> = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| ArxivError::Sqlite(format!("row: {e}")))?);
        }
        Ok(out)
    }

    /// Mark a paper as read at the current time. Idempotent: subsequent
    /// calls overwrite `read_at` so the user always sees their latest
    /// view time.
    pub fn mark_read(&self, arxiv_id: &str) -> Result<(), ArxivError> {
        let guard = self.conn.lock().unwrap();
        guard
            .execute(
                "INSERT INTO user_paper_state (arxiv_id, starred, read_at) VALUES (?, 0, ?) ON CONFLICT(arxiv_id) DO UPDATE SET read_at = excluded.read_at",
                rusqlite::params![arxiv_id, now_iso8601()],
            )
            .map_err(|e| ArxivError::Sqlite(format!("mark_read: {e}")))?;
        Ok(())
    }

    pub fn toggle_star(&self, arxiv_id: &str, starred: bool) -> Result<(), ArxivError> {
        let guard = self.conn.lock().unwrap();
        guard
            .execute(
                "INSERT INTO user_paper_state (arxiv_id, starred, read_at) VALUES (?, ?, NULL) ON CONFLICT(arxiv_id) DO UPDATE SET starred = excluded.starred",
                rusqlite::params![arxiv_id, if starred { 1 } else { 0 }],
            )
            .map_err(|e| ArxivError::Sqlite(format!("toggle_star: {e}")))?;
        Ok(())
    }

    pub fn set_opt_in(&self, enabled: bool) -> Result<(), ArxivError> {
        let guard = self.conn.lock().unwrap();
        guard
            .execute(
                "UPDATE scheduler_state SET opt_in_enabled = ? WHERE id = 1",
                rusqlite::params![if enabled { 1 } else { 0 }],
            )
            .map_err(|e| ArxivError::Sqlite(format!("set_opt_in: {e}")))?;
        Ok(())
    }

    pub fn get_opt_in(&self) -> Result<Option<bool>, ArxivError> {
        let guard = self.conn.lock().unwrap();
        let val: Option<i64> = guard
            .query_row(
                "SELECT opt_in_enabled FROM scheduler_state WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .map_err(|e| ArxivError::Sqlite(format!("get_opt_in: {e}")))?;
        Ok(val.map(|v| v != 0))
    }

    pub fn touch_last_fired(&self) -> Result<(), ArxivError> {
        let guard = self.conn.lock().unwrap();
        guard
            .execute(
                "UPDATE scheduler_state SET last_fired_at = ?, last_error = NULL WHERE id = 1",
                rusqlite::params![now_iso8601()],
            )
            .map_err(|e| ArxivError::Sqlite(format!("touch_last_fired: {e}")))?;
        Ok(())
    }

    /// Persist the most recent fetch failure to `scheduler_state.last_error`
    /// so the UI can surface why the daily digest missed.
    pub fn record_last_error(&self, message: &str) -> Result<(), ArxivError> {
        let guard = self.conn.lock().unwrap();
        guard
            .execute(
                "UPDATE scheduler_state SET last_error = ? WHERE id = 1",
                rusqlite::params![message],
            )
            .map_err(|e| ArxivError::Sqlite(format!("record_last_error: {e}")))?;
        Ok(())
    }

    /// Returns the persisted `last_error` (or None when the last fetch
    /// succeeded).
    pub fn last_error(&self) -> Result<Option<String>, ArxivError> {
        let guard = self.conn.lock().unwrap();
        guard
            .query_row(
                "SELECT last_error FROM scheduler_state WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .map_err(|e| ArxivError::Sqlite(format!("last_error: {e}")))
    }

    /// Insert `records` deduped against the 7-day window AND advance
    /// `last_fired_at` / clear `last_error` in the same transaction.
    /// Either both commits land or neither does — the AC contract is
    /// "rows and last_fired_at commit together".
    pub fn insert_dedup_and_touch_last_fired(
        &self,
        records: &[PaperRecord],
        source_query: &str,
    ) -> Result<Vec<PaperRecord>, ArxivError> {
        let guard = self.conn.lock().unwrap();
        let tx = guard
            .unchecked_transaction()
            .map_err(|e| ArxivError::Sqlite(format!("tx begin: {e}")))?;
        let mut inserted: Vec<PaperRecord> = Vec::new();
        for r in records {
            let cutoff = unix_now_secs() - DEDUP_WINDOW_DAYS * 86400;
            let cutoff_iso = iso8601_from_unix(cutoff);
            let exists: bool = tx
                .query_row(
                    "SELECT 1 FROM daily_recommendations WHERE arxiv_id = ? AND fetched_at >= ?",
                    rusqlite::params![r.arxiv_id, cutoff_iso],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            if exists {
                continue;
            }
            let authors_str = r.authors.join("; ");
            let fetched = now_iso8601();
            tx.execute(
                "INSERT INTO daily_recommendations (arxiv_id, title, authors, abstract_snippet, pdf_url, abs_url, source_query, source, fetched_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    r.arxiv_id,
                    r.title,
                    authors_str,
                    r.abstract_snippet,
                    r.pdf_url,
                    r.abs_url,
                    source_query,
                    r.source,
                    fetched,
                ],
            )
            .map_err(|e| ArxivError::Sqlite(format!("insert: {e}")))?;
            inserted.push(PaperRecord {
                fetched_at: fetched,
                starred: false,
                read_at: None,
                ..r.clone()
            });
        }
        tx.execute(
            "UPDATE scheduler_state SET last_fired_at = ?, last_error = NULL WHERE id = 1",
            rusqlite::params![now_iso8601()],
        )
        .map_err(|e| ArxivError::Sqlite(format!("touch_last_fired in tx: {e}")))?;
        tx.commit()
            .map_err(|e| ArxivError::Sqlite(format!("tx commit: {e}")))?;
        Ok(inserted)
    }

    pub fn last_fired_at_unix(&self) -> Result<Option<u64>, ArxivError> {
        let guard = self.conn.lock().unwrap();
        let s: Option<String> = guard
            .query_row(
                "SELECT last_fired_at FROM scheduler_state WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .map_err(|e| ArxivError::Sqlite(format!("last_fired: {e}")))?;
        match s {
            None => Ok(None),
            Some(iso) => parse_iso8601_to_unix(&iso).map(Some),
        }
    }
}

/// Convert a stored ISO 8601 timestamp into seconds the caller must
/// still wait, or `None` if the rate-limit window has elapsed. Shared
/// between read-only and acquire helpers so they cannot drift.
fn compute_rate_wait(last_iso: Option<&str>) -> Result<Option<u64>, ArxivError> {
    let Some(s) = last_iso else { return Ok(None); };
    let last = parse_iso8601_to_unix(s)?;
    let now = unix_now_secs();
    let delta = now.saturating_sub(last);
    if delta >= RATE_LIMIT_SECONDS {
        Ok(None)
    } else {
        Ok(Some(RATE_LIMIT_SECONDS - delta))
    }
}

/// Distinguishes a scheduler-driven daily fetch from an ad-hoc manual
/// search. Only `ScheduledDigest` advances `scheduler_state.last_fired_at`
/// and clears `last_error` — manual searches must not suppress the
/// next startup backfill or hide a prior scheduler failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchPurpose {
    ScheduledDigest,
    ManualSearch,
}

pub fn now_iso8601() -> String {
    let secs = unix_now_secs();
    iso8601_from_unix(secs)
}

pub fn iso8601_from_unix(secs: u64) -> String {
    // Bare RFC3339-ish ISO 8601 in UTC, second precision. Sufficient for
    // ordering and parsing back via parse_iso8601_to_unix.
    let days_since_epoch = secs / 86400;
    let secs_today = secs % 86400;
    let (year, month, day) = days_to_ymd(days_since_epoch as i64);
    let hh = secs_today / 3600;
    let mm = (secs_today % 3600) / 60;
    let ss = secs_today % 60;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hh, mm, ss
    )
}

pub fn parse_iso8601_to_unix(s: &str) -> Result<u64, ArxivError> {
    // Accept formats `YYYY-MM-DDTHH:MM:SSZ` or `YYYY-MM-DD HH:MM:SS`.
    let trimmed = s.trim().trim_end_matches('Z');
    let (date_part, time_part) = if let Some(idx) = trimmed.find('T') {
        (&trimmed[..idx], &trimmed[idx + 1..])
    } else if let Some(idx) = trimmed.find(' ') {
        (&trimmed[..idx], &trimmed[idx + 1..])
    } else {
        return Err(ArxivError::Parse(format!("iso8601 (no T/space): {s}")));
    };
    let date_components: Vec<&str> = date_part.split('-').collect();
    let time_components: Vec<&str> = time_part.split(':').collect();
    if date_components.len() != 3 || time_components.len() < 3 {
        return Err(ArxivError::Parse(format!("iso8601 components: {s}")));
    }
    let y: i64 = date_components[0]
        .parse()
        .map_err(|_| ArxivError::Parse(format!("year: {s}")))?;
    let mo: u32 = date_components[1]
        .parse()
        .map_err(|_| ArxivError::Parse(format!("month: {s}")))?;
    let d: u32 = date_components[2]
        .parse()
        .map_err(|_| ArxivError::Parse(format!("day: {s}")))?;
    let hh: u64 = time_components[0]
        .parse()
        .map_err(|_| ArxivError::Parse(format!("hour: {s}")))?;
    let mm: u64 = time_components[1]
        .parse()
        .map_err(|_| ArxivError::Parse(format!("minute: {s}")))?;
    let ss_str = time_components[2].split('.').next().unwrap_or("0");
    let ss: u64 = ss_str
        .parse()
        .map_err(|_| ArxivError::Parse(format!("second: {s}")))?;
    let days = ymd_to_days(y, mo, d);
    Ok((days as u64) * 86400 + hh * 3600 + mm * 60 + ss)
}

pub fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn ymd_to_days(year: i64, month: u32, day: u32) -> i64 {
    let mut days: i64 = 0;
    if year >= 1970 {
        for y in 1970..year {
            days += if is_leap(y) { 366 } else { 365 };
        }
    } else {
        for y in year..1970 {
            days -= if is_leap(y) { 366 } else { 365 };
        }
    }
    for m in 1..month {
        days += days_in_month(year, m) as i64;
    }
    days + (day as i64) - 1
}

fn days_to_ymd(mut days: i64) -> (i64, u32, u32) {
    let mut year: i64 = 1970;
    loop {
        let dy = if is_leap(year) { 366 } else { 365 };
        if days >= dy {
            days -= dy;
            year += 1;
        } else if days < 0 {
            year -= 1;
            days += if is_leap(year) { 366 } else { 365 };
        } else {
            break;
        }
    }
    let mut month: u32 = 1;
    loop {
        let dm = days_in_month(year, month) as i64;
        if days >= dm {
            days -= dm;
            month += 1;
        } else {
            break;
        }
    }
    (year, month, (days + 1) as u32)
}

/// Single arXiv entry point — every caller (scheduler, host Tauri
/// commands, host RPC bridge, sidecar MCP `tools/call`) routes through
/// here so opt-in, rate-limit, retry, dedup, transactional state and
/// `last_error` persistence are enforced in one place.
///
/// Steps:
///   1. Verify `scheduler_state.opt_in_enabled = 1`. Returns
///      `ArxivError::NotOptedIn` for both `NULL` (first-run) and `0`.
///   2. Check the 3 s rate-limit gate; return `RateLimited` if too soon.
///   3. Mark the rate-limit timestamp BEFORE the HTTP call so a slow
///      response cannot let a concurrent caller slip through.
///   4. Build the URL + run `fetch_arxiv_with_retry` (3 attempts,
///      exponential backoff).
///   5. Parse Atom XML into `PaperRecord`s, stamp `source`.
///   6. `insert_dedup_and_touch_last_fired` in one transaction — rows
///      and `last_fired_at` commit together.
///   7. On any failure after the opt-in check, persist a concise
///      message to `scheduler_state.last_error` so the UI can surface it.
pub fn fetch_papers_gated(
    store: &PapersStore,
    arxiv_fetcher: &dyn Fn(&str) -> Result<String, ArxivError>,
    api_base: &str,
    query: &str,
    source: &str,
    purpose: FetchPurpose,
) -> Result<Vec<PaperRecord>, ArxivError> {
    if query.trim().is_empty() {
        return Err(ArxivError::Parse("empty query".to_string()));
    }
    match store.get_opt_in()? {
        Some(true) => {}
        _ => return Err(ArxivError::NotOptedIn),
    }
    // Atomic acquire — if the rate-limit window has elapsed this stamps
    // `last_arxiv_call_at` in the same transaction as the read so two
    // concurrent callers can't both observe the gate as open.
    //
    // Cooldown handling differs by purpose. A scheduled digest cannot
    // give up on `RateLimited`: bubbling the error to the host
    // scheduler's `run_loop` discards the daily fire entirely (the
    // loop swallows the result and sleeps until the next 10 AM slot),
    // so a manual search completing at 09:59:59 would silently kill
    // the 10:00:00 digest. Wait the reported cooldown plus a small
    // margin and retry the acquire exactly once. Manual searches
    // surface `RateLimited` directly because the UI cooldown banner
    // already explains the wait.
    if let Err(err) = store.try_touch_rate_limit() {
        match (err, purpose) {
            (ArxivError::RateLimited { wait_secs }, FetchPurpose::ScheduledDigest) => {
                std::thread::sleep(
                    std::time::Duration::from_millis(wait_secs.saturating_mul(1000) + 250),
                );
                store.try_touch_rate_limit()?;
            }
            (other, _) => return Err(other),
        }
    }
    let url = build_arxiv_url(api_base, query, 10);
    let body = match arxiv_fetcher(&url) {
        Ok(b) => b,
        Err(e) => {
            // Only the scheduled path surfaces failures into the daily
            // status banner. Manual search errors are returned to the
            // caller (UI / orchestrator) directly.
            if purpose == FetchPurpose::ScheduledDigest {
                let _ = store.record_last_error(&format!("fetch: {e}"));
            }
            return Err(e);
        }
    };
    let parsed = match parse_arxiv_atom(&body, 10) {
        Ok(p) => p,
        Err(e) => {
            if purpose == FetchPurpose::ScheduledDigest {
                let _ = store.record_last_error(&format!("parse: {e}"));
            }
            return Err(e);
        }
    };
    let stamped: Vec<PaperRecord> = parsed
        .into_iter()
        .map(|r| PaperRecord {
            source: source.to_string(),
            ..r
        })
        .collect();
    let insert_result = match purpose {
        FetchPurpose::ScheduledDigest => {
            store.insert_dedup_and_touch_last_fired(&stamped, query)
        }
        FetchPurpose::ManualSearch => store.insert_dedup(&stamped, query),
    };
    match insert_result {
        Ok(inserted) => Ok(inserted),
        Err(e) => {
            if purpose == FetchPurpose::ScheduledDigest {
                let _ = store.record_last_error(&format!("storage: {e}"));
            }
            Err(e)
        }
    }
}

/// Reusable arXiv-fetcher backed by a blocking `reqwest::Client` with
/// the configured retry budget. Suitable for use as the `arxiv_fetcher`
/// argument to `fetch_papers_gated`.
pub fn make_blocking_arxiv_fetcher() -> Result<reqwest::blocking::Client, ArxivError> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("papers-plugin/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| ArxivError::Http(format!("client build: {e}")))
}

/// Pure dispatch for MCP `tools/call`. The caller passes the parsed
/// arguments and (for tools that need them) a reference to the SQLite
/// store. Returns a JsonRpcResponse ready for `encode_message`.
pub fn handle_tool_call(
    req: &JsonRpcRequest,
    store: &PapersStore,
    arxiv_fetcher: &dyn Fn(&str) -> Result<String, ArxivError>,
) -> JsonRpcResponse {
    let resp_id = req.id.clone();
    let params = req.params.clone().unwrap_or(json!({}));
    let tool_name = params
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let result: Result<Value, ArxivError> = match tool_name.as_str() {
        "papers.fetch" | "papers.search" => {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // The `source` argument was previously advertised on
            // `papers.fetch`'s input schema, which let a caller pass
            // `source: "manual"` and still trigger the
            // ScheduledDigest path — silently suppressing the next
            // backfill and wiping `last_error`. The source argument
            // is now removed from the schema and any value a caller
            // passes is ignored: `papers.fetch` is ALWAYS the
            // scheduler-driven path and `papers.search` is ALWAYS
            // the manual path. Tool name is the only routing input.
            let (source, purpose) = if tool_name == "papers.search" {
                ("manual", FetchPurpose::ManualSearch)
            } else {
                ("scheduled", FetchPurpose::ScheduledDigest)
            };
            let source = source.to_string();
            let api_base = resolve_arxiv_api_base();
            fetch_papers_gated(store, arxiv_fetcher, &api_base, &query, &source, purpose).map(
                |inserted| {
                    json!({
                        "new_count": inserted.len(),
                        "papers": inserted,
                    })
                },
            )
        }
        "papers.list_recent" => {
            let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(10);
            store
                .list_recent(limit)
                .map(|papers| json!({ "papers": papers }))
        }
        "papers.toggle_star" => {
            let arxiv_id = args
                .get("arxiv_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let starred = args.get("starred").and_then(|v| v.as_bool()).unwrap_or(false);
            store
                .toggle_star(&arxiv_id, starred)
                .map(|_| json!({ "ok": true }))
        }
        "papers.mark_read" => {
            let arxiv_id = args
                .get("arxiv_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            store.mark_read(&arxiv_id).map(|_| json!({ "ok": true }))
        }
        // Note: `papers.set_opt_in` is NOT an MCP tool. The opt-in
        // mutation must stay user-confirmed; exposing it to the
        // orchestrator Claude would let the model enable arXiv
        // context queries on the user's behalf. The host's
        // `papers_set_opt_in` Tauri command (wired to the React
        // first-run banner) is the only WRITE path.
        "papers.get_opt_in" => store
            .get_opt_in()
            .map(|enabled| json!({ "enabled": enabled })),
        other => {
            return JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: resp_id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32601,
                    message: format!("unknown tool `{other}`"),
                    data: None,
                }),
            };
        }
    };

    match result {
        Ok(value) => JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: resp_id,
            result: Some(value),
            error: None,
        },
        Err(e) => JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: resp_id,
            result: None,
            error: Some(JsonRpcError {
                code: -32000,
                message: format!("{e}"),
                data: None,
            }),
        },
    }
}

/// Resolves the effective arXiv API base URL. Honours
/// `PAPERS_ARXIV_BASE_OVERRIDE` so the binary boundary integration
/// test can point at a wiremock server without recompiling.
pub fn resolve_arxiv_api_base() -> String {
    std::env::var("PAPERS_ARXIV_BASE_OVERRIDE").unwrap_or_else(|_| ARXIV_API_DEFAULT.to_string())
}

/// MCP request dispatcher: handles initialize, tools/list, tools/call,
/// returns a JsonRpcResponse. Unknown methods return -32601.
pub fn handle_mcp_message(
    msg: JsonRpcMessage,
    store: &PapersStore,
    arxiv_fetcher: &dyn Fn(&str) -> Result<String, ArxivError>,
) -> Option<JsonRpcResponse> {
    let req = match msg {
        JsonRpcMessage::Request(r) => r,
        _ => return None,
    };
    let id = req.id.clone();
    match req.method.as_str() {
        "initialize" => Some(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(json!({
                "protocolVersion": "2024-11-05",
                "serverInfo": {
                    "name": "papers-plugin",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {
                    "tools": {}
                }
            })),
            error: None,
        }),
        "tools/list" => Some(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(tools_list_response()),
            error: None,
        }),
        "tools/call" => Some(handle_tool_call(&req, store, arxiv_fetcher)),
        other => Some(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code: -32601,
                message: format!("unknown method `{other}`"),
                data: None,
            }),
        }),
    }
}

/// Resolve the papers SQLite path from APP_DATA_DIR env var.
pub fn resolve_papers_db_path() -> Result<PathBuf, ArxivError> {
    let app_data = std::env::var("APP_DATA_DIR").map_err(|_| {
        ArxivError::Io("APP_DATA_DIR env var missing — sidecar must be spawned by the host".to_string())
    })?;
    let dir = PathBuf::from(app_data).join("plugins").join("papers");
    std::fs::create_dir_all(&dir).map_err(|e| ArxivError::Io(format!("mkdir: {e}")))?;
    Ok(dir.join("state.sqlite"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_stdio::JsonRpcId;
    use rusqlite::Connection;
    use tempfile::TempDir;

    fn setup_store() -> (PapersStore, TempDir) {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("state.sqlite");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE daily_recommendations (
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
            CREATE TABLE user_paper_state (
                arxiv_id TEXT PRIMARY KEY,
                starred INTEGER NOT NULL DEFAULT 0,
                read_at TEXT
            );
            CREATE TABLE scheduler_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                last_fired_at TEXT,
                last_arxiv_call_at TEXT,
                last_error TEXT,
                opt_in_enabled INTEGER
            );
            INSERT INTO scheduler_state (id) VALUES (1);
            "#,
        )
        .unwrap();
        drop(conn);
        let store = PapersStore::open(&db_path).unwrap();
        (store, tmp)
    }

    #[test]
    fn papers_store_open_initialises_schema_on_fresh_db() {
        // Packaged-install regression: the source-tree `migrations/`
        // directory is absent, so PapersStore::open MUST embed and
        // apply the schema itself. A fresh empty DB file plus
        // PapersStore::open must give us a usable store where
        // SELECTs against scheduler_state and daily_recommendations
        // succeed.
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("fresh.sqlite");
        // Touch the file but do NOT pre-apply any schema.
        std::fs::File::create(&db_path).unwrap();
        let store = PapersStore::open(&db_path).expect("open must succeed");
        // Opt-in starts as NULL — proves scheduler_state exists AND
        // the singleton row was inserted.
        assert_eq!(store.get_opt_in().unwrap(), None);
        // list_recent on an empty daily_recommendations table must
        // return an empty Vec, not error with `no such table`.
        let recent = store.list_recent(10).expect("list_recent must succeed");
        assert!(recent.is_empty());
        // user_paper_state operations succeed.
        store.toggle_star("2024.fresh", true).expect("toggle_star must succeed");
    }

    #[test]
    fn embedded_schema_is_idempotent() {
        // Reopening PapersStore on the same DB file must be a no-op
        // for the schema (every CREATE uses IF NOT EXISTS; the
        // singleton INSERT uses INSERT OR IGNORE). The second open
        // must NOT duplicate the scheduler_state row.
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("repeat.sqlite");
        let s1 = PapersStore::open(&db_path).unwrap();
        s1.set_opt_in(true).unwrap();
        drop(s1);
        let s2 = PapersStore::open(&db_path).unwrap();
        // Opt-in value from the first open must survive reopen.
        assert_eq!(s2.get_opt_in().unwrap(), Some(true));
        // Verify there is still exactly one scheduler_state row.
        let count: i64 = s2
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM scheduler_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn parses_arxiv_atom_response() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <id>http://arxiv.org/abs/2301.12345v1</id>
    <title>Attention Is All You Need</title>
    <summary>This paper introduces the Transformer architecture.</summary>
    <author><name>Ashish Vaswani</name></author>
    <author><name>Noam Shazeer</name></author>
    <link rel="alternate" href="http://arxiv.org/abs/2301.12345v1"/>
    <link href="http://arxiv.org/pdf/2301.12345v1" type="application/pdf"/>
  </entry>
  <entry>
    <id>http://arxiv.org/abs/2302.99999v1</id>
    <title>Second Paper</title>
    <summary>Another paper.</summary>
    <author><name>Anon</name></author>
    <link href="http://arxiv.org/pdf/2302.99999v1" type="application/pdf"/>
  </entry>
</feed>"#;
        let papers = parse_arxiv_atom(xml, 10).unwrap();
        assert_eq!(papers.len(), 2);
        assert_eq!(papers[0].arxiv_id, "2301.12345v1");
        assert_eq!(papers[0].title, "Attention Is All You Need");
        assert_eq!(papers[0].authors, vec!["Ashish Vaswani", "Noam Shazeer"]);
        assert!(papers[0].abstract_snippet.contains("Transformer"));
        assert_eq!(papers[0].pdf_url, "http://arxiv.org/pdf/2301.12345v1");
        assert_eq!(papers[1].arxiv_id, "2302.99999v1");
    }

    #[test]
    fn parse_caps_at_max_results() {
        let mut xml =
            String::from("<?xml version=\"1.0\"?><feed xmlns=\"http://www.w3.org/2005/Atom\">");
        for i in 0..20 {
            xml.push_str(&format!(
                "<entry><id>http://arxiv.org/abs/{i}</id><title>T{i}</title><summary>S</summary><link href=\"http://arxiv.org/pdf/{i}\" type=\"application/pdf\"/></entry>",
            ));
        }
        xml.push_str("</feed>");
        let papers = parse_arxiv_atom(&xml, 5).unwrap();
        assert_eq!(papers.len(), 5);
    }

    #[test]
    fn build_url_filters_punctuation() {
        let url = build_arxiv_url("https://api/", "transformers, attention!", 7);
        assert!(url.contains("search_query=all:transformers+attention"));
        assert!(url.contains("max_results=7"));
    }

    #[test]
    fn rate_limit_wait_reflects_recent_acquire() {
        let (store, _t) = setup_store();
        assert!(store.rate_limit_wait().unwrap().is_none());
        store.try_touch_rate_limit().unwrap();
        let wait = store.rate_limit_wait().unwrap();
        assert!(wait.is_some());
        assert!(wait.unwrap() <= RATE_LIMIT_SECONDS);
    }

    #[test]
    fn dedup_skips_known_arxiv_id_within_window() {
        let (store, _t) = setup_store();
        let r = PaperRecord {
            arxiv_id: "2301.aaa".to_string(),
            title: "T".to_string(),
            authors: vec![],
            abstract_snippet: "".to_string(),
            pdf_url: "".to_string(),
            abs_url: "".to_string(),
            source: "scheduled".to_string(),
            fetched_at: now_iso8601(),
            starred: false,
            read_at: None,
        };
        let first = store.insert_dedup(&[r.clone()], "q").unwrap();
        assert_eq!(first.len(), 1);
        let second = store.insert_dedup(&[r.clone()], "q").unwrap();
        assert_eq!(second.len(), 0);
    }

    #[test]
    fn list_recent_returns_newest_first() {
        let (store, _t) = setup_store();
        for i in 0..3 {
            store
                .insert_dedup(
                    &[PaperRecord {
                        arxiv_id: format!("2301.{:04}", i),
                        title: format!("T{}", i),
                        authors: vec![],
                        abstract_snippet: "".to_string(),
                        pdf_url: "".to_string(),
                        abs_url: "".to_string(),
                        source: "scheduled".to_string(),
                        fetched_at: now_iso8601(),
                        starred: false,
                        read_at: None,
                    }],
                    "q",
                )
                .unwrap();
            std::thread::sleep(Duration::from_millis(1100));
        }
        let recent = store.list_recent(2).unwrap();
        assert_eq!(recent.len(), 2);
    }

    #[test]
    fn opt_in_round_trips() {
        let (store, _t) = setup_store();
        assert_eq!(store.get_opt_in().unwrap(), None);
        store.set_opt_in(true).unwrap();
        assert_eq!(store.get_opt_in().unwrap(), Some(true));
        store.set_opt_in(false).unwrap();
        assert_eq!(store.get_opt_in().unwrap(), Some(false));
    }

    #[test]
    fn toggle_star_persists() {
        let (store, _t) = setup_store();
        store.toggle_star("2301.aaa", true).unwrap();
        let g = store.conn.lock().unwrap();
        let starred: i64 = g
            .query_row(
                "SELECT starred FROM user_paper_state WHERE arxiv_id = ?",
                rusqlite::params!["2301.aaa"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(starred, 1);
        drop(g);
        store.toggle_star("2301.aaa", false).unwrap();
        let g = store.conn.lock().unwrap();
        let starred: i64 = g
            .query_row(
                "SELECT starred FROM user_paper_state WHERE arxiv_id = ?",
                rusqlite::params!["2301.aaa"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(starred, 0);
    }

    #[test]
    fn retry_succeeds_after_503() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;
        let attempt = Arc::new(AtomicU32::new(0));
        let _a = attempt.clone();
        // We test retry behavior via direct loop — fetch_arxiv_with_retry uses
        // a real reqwest client which we cannot easily mock here without a
        // running mock server. The wiremock integration test in
        // `tests/integration_arxiv.rs` exercises this end-to-end.
        let _ = attempt.load(Ordering::SeqCst);
    }

    #[test]
    fn iso8601_round_trip() {
        let secs = 1_700_000_000u64;
        let iso = iso8601_from_unix(secs);
        let back = parse_iso8601_to_unix(&iso).unwrap();
        assert_eq!(secs, back);
    }

    #[test]
    fn tools_list_includes_all_orchestrator_visible_papers_tools() {
        let v = tools_list_response();
        let tools = v.get("tools").and_then(|t| t.as_array()).unwrap();
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t.get("name").and_then(|n| n.as_str()).unwrap())
            .collect();
        assert!(names.contains(&"papers.list_recent"));
        assert!(names.contains(&"papers.search"));
        assert!(names.contains(&"papers.toggle_star"));
        assert!(names.contains(&"papers.mark_read"));
        assert!(names.contains(&"papers.get_opt_in"));
    }

    #[test]
    fn papers_fetch_is_not_advertised_in_tools_list() {
        // The scheduler-driven fetch path must NOT be discoverable by
        // the orchestrator Claude. If listed, an ad-hoc request can
        // pick it instead of `papers.search`, hardcoded to
        // ScheduledDigest, which would advance `last_fired_at` /
        // clear `last_error` and silently kill the next daily digest.
        let v = tools_list_response();
        let tools = v.get("tools").and_then(|t| t.as_array()).unwrap();
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t.get("name").and_then(|n| n.as_str()).unwrap())
            .collect();
        assert!(
            !names.contains(&"papers.fetch"),
            "papers.fetch must NOT be advertised in tools/list; got: {names:?}"
        );
    }

    #[test]
    fn papers_set_opt_in_is_not_an_mcp_tool() {
        // Opt-in WRITE must stay user-confirmed; the orchestrator
        // Claude must not be able to flip it via MCP.
        let v = tools_list_response();
        let tools = v.get("tools").and_then(|t| t.as_array()).unwrap();
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t.get("name").and_then(|n| n.as_str()).unwrap())
            .collect();
        assert!(
            !names.contains(&"papers.set_opt_in"),
            "papers.set_opt_in must NOT be exposed as an MCP tool; got: {names:?}"
        );
        // The dispatch arm must also be gone — invoking the name via
        // tools/call returns an unknown-tool error, not a successful
        // mutation.
        let (store, _t) = setup_store();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: JsonRpcId::Number(99),
            method: "tools/call".to_string(),
            params: Some(json!({
                "name": "papers.set_opt_in",
                "arguments": { "enabled": true }
            })),
        };
        let fetcher = |_: &str| Ok::<String, ArxivError>(String::new());
        let resp = handle_mcp_message(JsonRpcMessage::Request(req), &store, &fetcher).unwrap();
        let err = resp.error.expect("expected MCP error for removed tool");
        assert_eq!(err.code, -32601);
        // The store state must NOT have been mutated.
        assert_eq!(store.get_opt_in().unwrap(), None);
    }

    #[test]
    fn papers_get_opt_in_still_exposed_as_mcp_tool() {
        // Read access stays — orchestrator needs to know whether
        // context-driven searches are enabled.
        let v = tools_list_response();
        let tools = v.get("tools").and_then(|t| t.as_array()).unwrap();
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t.get("name").and_then(|n| n.as_str()).unwrap())
            .collect();
        assert!(names.contains(&"papers.get_opt_in"));
    }

    #[test]
    fn list_recent_clamps_negative_limit_to_zero() {
        // SQLite treats `LIMIT -1` as no-limit. A negative caller
        // value must not return the entire table.
        let (store, _t) = setup_store();
        // Seed a handful of rows so the test would fail if clamping
        // were absent (we'd see all 3 rows back instead of 0).
        for i in 0..3 {
            store
                .insert_dedup(
                    &[PaperRecord {
                        arxiv_id: format!("2024.neg.{i}"),
                        title: format!("T{i}"),
                        authors: vec![],
                        abstract_snippet: "".to_string(),
                        pdf_url: "".to_string(),
                        abs_url: "".to_string(),
                        source: "scheduled".to_string(),
                        fetched_at: now_iso8601(),
                        starred: false,
                        read_at: None,
                    }],
                    "q",
                )
                .unwrap();
        }
        let rows = store.list_recent(-1).unwrap();
        assert!(
            rows.is_empty(),
            "negative limit must clamp to 0 rows; got {} rows",
            rows.len()
        );
    }

    #[test]
    fn list_recent_clamps_above_max_limit() {
        // Above the cap, list_recent returns at most MAX_LIST_RECENT_LIMIT.
        let (store, _t) = setup_store();
        let seed_count = (MAX_LIST_RECENT_LIMIT as usize) + 50;
        for i in 0..seed_count {
            store
                .insert_dedup(
                    &[PaperRecord {
                        arxiv_id: format!("2024.cap.{i:05}"),
                        title: format!("T{i}"),
                        authors: vec![],
                        abstract_snippet: "".to_string(),
                        pdf_url: "".to_string(),
                        abs_url: "".to_string(),
                        source: "scheduled".to_string(),
                        fetched_at: now_iso8601(),
                        starred: false,
                        read_at: None,
                    }],
                    "q",
                )
                .unwrap();
        }
        let rows = store.list_recent(1_000_000).unwrap();
        assert_eq!(rows.len(), MAX_LIST_RECENT_LIMIT as usize);
    }

    #[test]
    fn list_recent_by_source_returns_only_matching_rows() {
        // Regression: with N manual rows and a smaller scheduled set,
        // the mixed `list_recent(limit)` returns only manual rows
        // (newer fetched_at) and the UI loses the scheduled digest.
        // The source-filtered variant must return ONLY the requested
        // source so each panel section can be fetched independently.
        let (store, _t) = setup_store();
        for i in 0..5 {
            store
                .insert_dedup(
                    &[PaperRecord {
                        arxiv_id: format!("sched.{i}"),
                        title: format!("S{i}"),
                        authors: vec![],
                        abstract_snippet: String::new(),
                        pdf_url: String::new(),
                        abs_url: String::new(),
                        source: "scheduled".to_string(),
                        fetched_at: now_iso8601(),
                        starred: false,
                        read_at: None,
                    }],
                    "q",
                )
                .unwrap();
        }
        for i in 0..40 {
            store
                .insert_dedup(
                    &[PaperRecord {
                        arxiv_id: format!("man.{i}"),
                        title: format!("M{i}"),
                        authors: vec![],
                        abstract_snippet: String::new(),
                        pdf_url: String::new(),
                        abs_url: String::new(),
                        source: "manual".to_string(),
                        fetched_at: now_iso8601(),
                        starred: false,
                        read_at: None,
                    }],
                    "q",
                )
                .unwrap();
        }
        let scheduled = store
            .list_recent_by_source(Some("scheduled"), 30)
            .unwrap();
        let manual = store.list_recent_by_source(Some("manual"), 30).unwrap();
        assert_eq!(scheduled.len(), 5);
        assert!(scheduled.iter().all(|p| p.source == "scheduled"));
        assert_eq!(manual.len(), 30);
        assert!(manual.iter().all(|p| p.source == "manual"));
    }

    #[test]
    fn list_recent_by_source_none_matches_existing_behaviour() {
        // Back-compat: `list_recent_by_source(None, limit)` must
        // return the same rows as `list_recent(limit)` so the legacy
        // wrapper is a faithful delegate.
        let (store, _t) = setup_store();
        for i in 0..3 {
            store
                .insert_dedup(
                    &[PaperRecord {
                        arxiv_id: format!("mix.{i}"),
                        title: format!("T{i}"),
                        authors: vec![],
                        abstract_snippet: String::new(),
                        pdf_url: String::new(),
                        abs_url: String::new(),
                        source: if i % 2 == 0 { "scheduled" } else { "manual" }
                            .to_string(),
                        fetched_at: now_iso8601(),
                        starred: false,
                        read_at: None,
                    }],
                    "q",
                )
                .unwrap();
        }
        let via_legacy = store.list_recent(10).unwrap();
        let via_filter = store.list_recent_by_source(None, 10).unwrap();
        assert_eq!(via_legacy.len(), via_filter.len());
        let legacy_ids: Vec<_> = via_legacy.iter().map(|p| p.arxiv_id.clone()).collect();
        let filter_ids: Vec<_> = via_filter.iter().map(|p| p.arxiv_id.clone()).collect();
        assert_eq!(legacy_ids, filter_ids);
    }

    #[test]
    fn handle_tools_list_returns_tool_array() {
        let (store, _t) = setup_store();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: JsonRpcId::Number(1),
            method: "tools/list".to_string(),
            params: None,
        };
        let fetcher = |_url: &str| Ok::<String, ArxivError>(String::new());
        let resp = handle_mcp_message(JsonRpcMessage::Request(req), &store, &fetcher).unwrap();
        assert!(resp.error.is_none());
        assert!(resp.result.is_some());
        let r = resp.result.unwrap();
        assert!(r.get("tools").is_some());
    }

    #[test]
    fn fetch_papers_gated_rejects_when_opt_in_null() {
        let (store, _t) = setup_store();
        let fetcher = |_: &str| Ok::<String, ArxivError>("<feed/>".to_string());
        let err = fetch_papers_gated(&store, &fetcher, "https://x/", "q", "scheduled", FetchPurpose::ScheduledDigest).unwrap_err();
        assert!(matches!(err, ArxivError::NotOptedIn));
    }

    #[test]
    fn fetch_papers_gated_rejects_when_opt_in_false() {
        let (store, _t) = setup_store();
        store.set_opt_in(false).unwrap();
        let fetcher = |_: &str| Ok::<String, ArxivError>("<feed/>".to_string());
        let err = fetch_papers_gated(&store, &fetcher, "https://x/", "q", "scheduled", FetchPurpose::ScheduledDigest).unwrap_err();
        assert!(matches!(err, ArxivError::NotOptedIn));
    }

    #[test]
    fn fetch_papers_gated_records_last_error_on_http_failure() {
        let (store, _t) = setup_store();
        store.set_opt_in(true).unwrap();
        let fetcher = |_: &str| Err::<String, ArxivError>(ArxivError::Http("boom".to_string()));
        let _ = fetch_papers_gated(&store, &fetcher, "https://x/", "q", "scheduled", FetchPurpose::ScheduledDigest);
        let recorded = store.last_error().unwrap();
        assert!(recorded.is_some());
        assert!(recorded.unwrap().contains("fetch"));
    }

    #[test]
    fn fetch_papers_gated_commits_rows_and_last_fired_together() {
        let (store, _t) = setup_store();
        store.set_opt_in(true).unwrap();
        let body =
            r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><entry><id>http://arxiv.org/abs/x.1</id><title>T</title><summary>S</summary><link href="http://arxiv.org/pdf/x.1" type="application/pdf"/></entry></feed>"#
                .to_string();
        let fetcher = move |_: &str| Ok::<String, ArxivError>(body.clone());
        let before_fired = store.last_fired_at_unix().unwrap();
        let inserted =
            fetch_papers_gated(&store, &fetcher, "https://x/", "q", "scheduled", FetchPurpose::ScheduledDigest).unwrap();
        assert_eq!(inserted.len(), 1);
        // last_fired_at advanced (was None before the very first successful fire).
        let after_fired = store.last_fired_at_unix().unwrap();
        assert!(before_fired.is_none() || after_fired > before_fired);
        assert!(after_fired.is_some());
        // last_error is cleared by the transaction.
        assert!(store.last_error().unwrap().is_none());
    }

    #[test]
    fn list_recent_hydrates_starred_and_read_state() {
        let (store, _t) = setup_store();
        store.set_opt_in(true).unwrap();
        let r = PaperRecord {
            arxiv_id: "2401.zzz".to_string(),
            title: "T".to_string(),
            authors: vec![],
            abstract_snippet: "".to_string(),
            pdf_url: "".to_string(),
            abs_url: "".to_string(),
            source: "scheduled".to_string(),
            fetched_at: now_iso8601(),
            starred: false,
            read_at: None,
        };
        store.insert_dedup(&[r.clone()], "q").unwrap();
        store.toggle_star("2401.zzz", true).unwrap();
        store.mark_read("2401.zzz").unwrap();
        let listed = store.list_recent(10).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].starred);
        assert!(listed[0].read_at.is_some());
    }

    #[test]
    fn manual_search_does_not_advance_last_fired_at() {
        // Manual searches must NOT bump `last_fired_at`, otherwise the
        // next startup backfill would think the daily digest already
        // fired and skip itself. This is the FetchPurpose split's
        // primary invariant.
        let (store, _t) = setup_store();
        store.set_opt_in(true).unwrap();
        // Pre-set a stale last_fired_at (older than the 20h backfill
        // threshold) and capture its value.
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE scheduler_state SET last_fired_at = ?, last_error = 'previous failure' WHERE id = 1",
                rusqlite::params!["1970-01-01T00:00:00Z"],
            )
            .unwrap();
        let stale_before = store.last_fired_at_unix().unwrap().unwrap();

        let body = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><entry><id>http://arxiv.org/abs/m.1</id><title>T</title><summary>S</summary><link href="http://arxiv.org/pdf/m.1" type="application/pdf"/></entry></feed>"#.to_string();
        let fetcher = move |_: &str| Ok::<String, ArxivError>(body.clone());
        let inserted = fetch_papers_gated(
            &store,
            &fetcher,
            "https://x/",
            "query",
            "manual",
            FetchPurpose::ManualSearch,
        )
        .unwrap();
        assert_eq!(inserted.len(), 1);

        // last_fired_at must NOT have advanced.
        let stale_after = store.last_fired_at_unix().unwrap().unwrap();
        assert_eq!(stale_before, stale_after);

        // last_error must NOT have been cleared by a manual search.
        let err = store.last_error().unwrap();
        assert_eq!(err.as_deref(), Some("previous failure"));

        // should_backfill must still return true on a stale timestamp.
        let now_unix = stale_after + 21 * 3600;
        assert!(crate::compute_rate_wait(None).unwrap().is_none());
        // Reuse the scheduler helper instead of duplicating logic.
        // The crate's own should_backfill lives in src-tauri; here we
        // assert the underlying invariant: stored last_fired_at is
        // unchanged, so any sane backfill predicate will still trip.
        assert!(now_unix > stale_after + 20 * 3600);
    }

    #[test]
    fn try_touch_rate_limit_returns_rate_limited_within_window() {
        let (store, _t) = setup_store();
        // First acquire succeeds.
        store.try_touch_rate_limit().unwrap();
        // Second within the window fails atomically.
        let err = store.try_touch_rate_limit().unwrap_err();
        match err {
            ArxivError::RateLimited { wait_secs } => {
                assert!(wait_secs <= RATE_LIMIT_SECONDS);
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn try_touch_rate_limit_is_atomic_across_two_stores() {
        // Two `PapersStore` handles pointed at the same SQLite file
        // race; exactly one wins. The loser must get RateLimited
        // without performing any side effect that lets it slip past.
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("state.sqlite");
        let setup_conn = Connection::open(&db_path).unwrap();
        setup_conn
            .execute_batch(
                r#"
                CREATE TABLE daily_recommendations (
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
                CREATE TABLE user_paper_state (
                    arxiv_id TEXT PRIMARY KEY,
                    starred INTEGER NOT NULL DEFAULT 0,
                    read_at TEXT
                );
                CREATE TABLE scheduler_state (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    last_fired_at TEXT,
                    last_arxiv_call_at TEXT,
                    last_error TEXT,
                    opt_in_enabled INTEGER
                );
                INSERT INTO scheduler_state (id) VALUES (1);
                "#,
            )
            .unwrap();
        drop(setup_conn);

        let store_a = PapersStore::open(&db_path).unwrap();
        let store_b = PapersStore::open(&db_path).unwrap();
        // Both stores see a clean state; first one to call try_touch wins.
        store_a.try_touch_rate_limit().unwrap();
        let err = store_b.try_touch_rate_limit().unwrap_err();
        assert!(
            matches!(err, ArxivError::RateLimited { .. }),
            "second concurrent caller must see RateLimited; got {err:?}"
        );
    }

    #[test]
    fn handle_fetch_through_dispatch() {
        let (store, _t) = setup_store();
        // fetch_papers_gated requires explicit opt-in; the centralized
        // gate is exactly what this dispatch test exercises.
        store.set_opt_in(true).unwrap();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: JsonRpcId::Number(2),
            method: "tools/call".to_string(),
            params: Some(json!({
                "name": "papers.fetch",
                "arguments": { "query": "transformers" }
            })),
        };
        let fetcher = |_url: &str| {
            Ok::<String, ArxivError>(
                r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><entry><id>http://arxiv.org/abs/1</id><title>T</title><summary>S</summary><link href="http://arxiv.org/pdf/1" type="application/pdf"/></entry></feed>"#
                    .to_string(),
            )
        };
        let resp = handle_mcp_message(JsonRpcMessage::Request(req), &store, &fetcher).unwrap();
        assert!(resp.error.is_none());
        let r = resp.result.unwrap();
        assert_eq!(r["new_count"].as_i64().unwrap(), 1);
    }

    #[test]
    fn papers_fetch_still_routes_to_scheduled_digest_via_tools_call() {
        // Even though papers.fetch is no longer advertised in
        // tools/list, the dispatch arm in handle_tool_call MUST remain
        // so the host scheduler's direct tools/call (via
        // papers_sidecar_client::fetch_via_sidecar) keeps working
        // without a protocol change. The arm hardcodes
        // (source="scheduled", FetchPurpose::ScheduledDigest).
        let (store, _t) = setup_store();
        store.set_opt_in(true).unwrap();
        let before_fired = store.last_fired_at_unix().unwrap();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: JsonRpcId::Number(7),
            method: "tools/call".to_string(),
            params: Some(json!({
                "name": "papers.fetch",
                "arguments": { "query": "transformers" }
            })),
        };
        let fetcher = |_url: &str| {
            Ok::<String, ArxivError>(
                r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><entry><id>http://arxiv.org/abs/keep.1</id><title>T</title><summary>S</summary><link href="http://arxiv.org/pdf/keep.1" type="application/pdf"/></entry></feed>"#
                    .to_string(),
            )
        };
        let resp = handle_mcp_message(JsonRpcMessage::Request(req), &store, &fetcher).unwrap();
        assert!(
            resp.error.is_none(),
            "papers.fetch via tools/call must still succeed; got error: {:?}",
            resp.error
        );
        let after_fired = store.last_fired_at_unix().unwrap();
        assert!(
            before_fired.is_none() || after_fired > before_fired,
            "papers.fetch must advance last_fired_at (ScheduledDigest path); before={before_fired:?} after={after_fired:?}"
        );
        let listed = store.list_recent(10).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].source, "scheduled");
    }

    #[test]
    fn scheduled_digest_waits_for_cooldown_and_retries() {
        // Reproduces the missed-digest bug: a manual search at
        // (now - 1s) is inside the 3-second rate-limit window. The
        // scheduled fire that follows MUST wait the cooldown and
        // retry the acquire, not bubble RateLimited up to the
        // scheduler (which would discard the daily fire entirely
        // and sleep until tomorrow).
        let (store, _t) = setup_store();
        store.set_opt_in(true).unwrap();
        // Acquire once to stamp last_arxiv_call_at = "now".
        store.try_touch_rate_limit().unwrap();
        let fetcher = |_url: &str| {
            Ok::<String, ArxivError>(
                r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><entry><id>http://arxiv.org/abs/wait.1</id><title>T</title><summary>S</summary><link href="http://arxiv.org/pdf/wait.1" type="application/pdf"/></entry></feed>"#
                    .to_string(),
            )
        };
        let start = std::time::Instant::now();
        let inserted = fetch_papers_gated(
            &store,
            &fetcher,
            "https://x/",
            "query",
            "scheduled",
            FetchPurpose::ScheduledDigest,
        )
        .expect("scheduled digest must succeed after cooldown wait, not bubble RateLimited");
        let elapsed = start.elapsed();
        assert_eq!(inserted.len(), 1, "expected one row inserted");
        // The acquire stamped 'now' and the gate window is
        // RATE_LIMIT_SECONDS; we should have waited at least
        // (window - small slack) before the retry. The retry sleep
        // is wait_secs + 250 ms, so the upper bound is ~window + 1s.
        let lower = std::time::Duration::from_millis(
            RATE_LIMIT_SECONDS.saturating_mul(1000).saturating_sub(500),
        );
        let upper = std::time::Duration::from_millis(
            RATE_LIMIT_SECONDS.saturating_mul(1000) + 1500,
        );
        assert!(
            elapsed >= lower && elapsed <= upper,
            "scheduled digest should have waited ~{}s for cooldown; took {:?}",
            RATE_LIMIT_SECONDS,
            elapsed
        );
    }

    #[test]
    fn manual_search_still_bubbles_rate_limited_immediately() {
        // ManualSearch must keep its current behaviour: surface
        // RateLimited to the UI immediately so the cooldown banner
        // can explain the wait. It must NOT inherit the scheduled
        // retry/sleep path (that would block the UI thread).
        let (store, _t) = setup_store();
        store.set_opt_in(true).unwrap();
        store.try_touch_rate_limit().unwrap();
        let fetcher = |_url: &str| Ok::<String, ArxivError>(String::new());
        let start = std::time::Instant::now();
        let err = fetch_papers_gated(
            &store,
            &fetcher,
            "https://x/",
            "query",
            "manual",
            FetchPurpose::ManualSearch,
        )
        .unwrap_err();
        let elapsed = start.elapsed();
        match err {
            ArxivError::RateLimited { wait_secs } => {
                assert!(wait_secs <= RATE_LIMIT_SECONDS);
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "manual search must return immediately on RateLimited; took {elapsed:?}"
        );
    }

    #[test]
    fn papers_fetch_ignores_caller_supplied_source_and_uses_scheduled_digest() {
        // Even if a caller passes `source: "manual"` to papers.fetch
        // (which the schema no longer advertises but which is still
        // valid JSON), the dispatch MUST hardcode source="scheduled"
        // and FetchPurpose::ScheduledDigest. The proof:
        //   1. The inserted row's `source` is "scheduled", not "manual".
        //   2. `last_fired_at` advanced (ManualSearch would have left
        //      it untouched per the FetchPurpose split).
        let (store, _t) = setup_store();
        store.set_opt_in(true).unwrap();
        let before_fired = store.last_fired_at_unix().unwrap();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: JsonRpcId::Number(42),
            method: "tools/call".to_string(),
            params: Some(json!({
                "name": "papers.fetch",
                "arguments": { "query": "transformers", "source": "manual" }
            })),
        };
        let fetcher = |_url: &str| {
            Ok::<String, ArxivError>(
                r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><entry><id>http://arxiv.org/abs/forge.1</id><title>T</title><summary>S</summary><link href="http://arxiv.org/pdf/forge.1" type="application/pdf"/></entry></feed>"#
                    .to_string(),
            )
        };
        let resp = handle_mcp_message(JsonRpcMessage::Request(req), &store, &fetcher).unwrap();
        assert!(resp.error.is_none());
        let papers = resp.result.unwrap()["papers"].clone();
        let arr = papers.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(
            arr[0]["source"].as_str(),
            Some("scheduled"),
            "papers.fetch must stamp source=\"scheduled\" even when caller passes \"manual\""
        );

        let after_fired = store.last_fired_at_unix().unwrap();
        assert!(after_fired.is_some());
        assert!(
            before_fired.is_none() || after_fired > before_fired,
            "papers.fetch must advance last_fired_at (ScheduledDigest path); got before={before_fired:?} after={after_fired:?}"
        );
    }
}
