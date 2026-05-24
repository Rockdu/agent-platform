//! Daily papers digest scheduler.
//!
//! Owns the 10 AM local-time fire, the startup backfill check, the
//! opt-in gate, and the in-flight dedup. Calls into `papers-plugin`'s
//! library helpers for arXiv HTTP, XML parse, and SQLite write so the
//! sidecar binary is reserved for the orchestrator Claude's MCP tool
//! surface.
//!
//! Failure model:
//!   - HTTP / parse errors update `scheduler_state.last_error` and
//!     do NOT update `last_fired_at`. The next startup or 10 AM tick
//!     will retry.
//!   - The opt-in flag is checked on every fire. Disabled → skip
//!     extraction and HTTP entirely (no network call).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use papers_plugin::{
    fetch_arxiv_with_retry, fetch_papers_gated, make_blocking_arxiv_fetcher,
    resolve_arxiv_api_base, PaperRecord, PapersStore, MAX_RETRY_ATTEMPTS,
};

use crate::notification::{NotificationService, PapersTrayEntry};
use crate::papers_context::{extract_workspace_keywords, CONTEXT_BUDGET_BYTES_TOTAL};
use crate::workspaces::WorkspaceRegistry;

/// The hour-of-day (local time, 0..=23) at which the daily digest
/// fires. Hard-coded today; could become a user preference later.
pub const DAILY_FIRE_HOUR_LOCAL: u32 = 10;

/// How stale `last_fired_at` may be before a startup backfill is
/// triggered (i.e. the daily cycle ran too long ago).
pub const BACKFILL_THRESHOLD_HOURS: u64 = 20;

/// Maximum number of papers the digest stores per fire. Currently
/// enforced inside `fetch_papers_gated` via the arXiv `max_results`
/// query parameter (hard-coded to 10 in the URL builder).
#[allow(dead_code)]
pub const MAX_RESULTS_PER_FIRE: usize = 10;

/// Compute seconds from `now_unix_secs` until the next occurrence of
/// `hour_local` in local time. Pure function; safe to test against
/// mocked `now`s. Uses a fixed UTC offset captured by the caller so
/// the test suite is deterministic across machines.
///
/// `local_offset_secs` is the UTC→local offset in seconds (negative
/// west of UTC). Production callers pass the current system offset.
pub fn duration_until_next_fire_local(
    now_unix_secs: i64,
    hour_local: u32,
    local_offset_secs: i64,
) -> Duration {
    let local_now_secs = now_unix_secs + local_offset_secs;
    let secs_today = local_now_secs.rem_euclid(86400);
    let target_secs = (hour_local as i64) * 3600;
    // Strict `<`: at exactly hour:00:00, sleep a full day rather than
    // returning 0. A zero-delta wake-up inside the loop would race
    // with `fire()` and could re-fire within the same second.
    let delta = if secs_today < target_secs {
        target_secs - secs_today
    } else {
        86400 - (secs_today - target_secs)
    };
    Duration::from_secs(delta.unsigned_abs())
}

/// Returns true if `last_fired_unix_secs` is more than
/// `BACKFILL_THRESHOLD_HOURS` hours ago (or never).
pub fn should_backfill(now_unix_secs: u64, last_fired_unix_secs: Option<u64>) -> bool {
    match last_fired_unix_secs {
        None => true,
        Some(last) => now_unix_secs.saturating_sub(last) > BACKFILL_THRESHOLD_HOURS * 3600,
    }
}

/// Long-lived scheduler state held by the host. Cheap to clone — the
/// store and atomic are wrapped in Arc.
#[derive(Clone)]
pub struct PapersScheduler {
    pub store: Arc<PapersStore>,
    pub workspaces: WorkspaceRegistry,
    pub notification: Arc<NotificationService>,
    pub in_flight: Arc<AtomicBool>,
    pub api_base: String,
    pub app_handle: Option<tauri::AppHandle>,
}

impl PapersScheduler {
    pub fn new(
        store: Arc<PapersStore>,
        workspaces: WorkspaceRegistry,
        notification: Arc<NotificationService>,
        app_handle: Option<tauri::AppHandle>,
    ) -> Self {
        Self {
            store,
            workspaces,
            notification,
            in_flight: Arc::new(AtomicBool::new(false)),
            api_base: resolve_arxiv_api_base(),
            app_handle,
        }
    }

    /// Spawn the long-running daily loop on the current tokio runtime.
    /// The future returned by `start()` resolves immediately; the loop
    /// runs as a `tokio::spawn` task.
    pub fn start(self) {
        tokio::spawn(async move {
            self.run_loop().await;
        });
    }

    async fn run_loop(self) {
        // Startup backfill: if we missed the last fire by more than the
        // threshold, fire immediately. Then enter the daily-cycle loop.
        let now_unix = unix_now_secs();
        let last = self.store.last_fired_at_unix().ok().flatten();
        if should_backfill(now_unix, last) {
            tracing::info!(
                last_fired_at = ?last,
                "papers-scheduler: triggering startup backfill (>20h since last fire)"
            );
            let _ = self.fire().await;
        }
        loop {
            let now = unix_now_secs() as i64;
            let offset = local_offset_seconds();
            let wait = duration_until_next_fire_local(now, DAILY_FIRE_HOUR_LOCAL, offset);
            tracing::info!(
                wait_secs = wait.as_secs(),
                "papers-scheduler: sleeping until next daily fire"
            );
            tokio::time::sleep(wait).await;
            let _ = self.fire().await;
        }
    }

    /// Run one fetch cycle. Returns Ok(count) on success or Err on
    /// failure. Errors update `last_error` but do not break the loop.
    pub async fn fire(&self) -> Result<usize, FireError> {
        if self
            .in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(FireError::AlreadyInFlight);
        }
        let result = self.fire_inner().await;
        self.in_flight.store(false, Ordering::SeqCst);
        result
    }

    async fn fire_inner(&self) -> Result<usize, FireError> {
        // The opt-in gate is enforced once and authoritatively inside
        // `fetch_papers_gated`. Repeating it here would only widen the
        // window where the user could disable opt-in between this
        // check and the actual HTTP call. Skip the early bail-out and
        // let the centralised gate decide.
        //
        // Context extraction still respects opt-in implicitly: it
        // never runs unless we are about to call `fetch_papers_gated`.
        let opt_in = self
            .store
            .get_opt_in()
            .map_err(|e| FireError::Storage(e.to_string()))?;
        if !matches!(opt_in, Some(true)) {
            tracing::info!(
                opt_in = ?opt_in,
                "papers-scheduler: skipping fire — opt-in not enabled"
            );
            return Err(FireError::NotOptedIn);
        }

        // Extract context off the tokio reactor (file IO + git subprocess).
        let workspaces = self.workspaces.clone();
        let keywords = tokio::task::spawn_blocking(move || {
            let snapshot = workspaces.list();
            let local_paths: Vec<PathBuf> = snapshot
                .iter()
                .filter_map(|r| r.local_path().map(|p| p.to_path_buf()))
                .collect();
            extract_workspace_keywords(&local_paths, CONTEXT_BUDGET_BYTES_TOTAL)
        })
        .await
        .map_err(|e| FireError::ContextExtraction(e.to_string()))?;

        if keywords.is_empty() {
            tracing::info!("papers-scheduler: empty context — skipping arXiv call");
            return Err(FireError::EmptyContext);
        }

        let query = keywords.join(" ");
        let api_base = self.api_base.clone();
        let store = self.store.clone();
        let inserted: Vec<PaperRecord> = tokio::task::spawn_blocking(move || {
            let client = make_blocking_arxiv_fetcher()?;
            let fetcher = |url: &str| fetch_arxiv_with_retry(&client, url, MAX_RETRY_ATTEMPTS);
            fetch_papers_gated(&store, &fetcher, &api_base, &query, "scheduled")
        })
        .await
        .map_err(|e| FireError::Join(e.to_string()))?
        .map_err(|e| FireError::Storage(e.to_string()))?;

        let count = inserted.len();
        // last_fired_at was already advanced inside the transaction
        // owned by `fetch_papers_gated::insert_dedup_and_touch_last_fired`.
        if count > 0 {
            let entries: Vec<PapersTrayEntry> = inserted
                .iter()
                .map(|p| PapersTrayEntry {
                    arxiv_id: p.arxiv_id.clone(),
                    title: p.title.clone(),
                    abstract_snippet: p.abstract_snippet.clone(),
                    abs_url: p.abs_url.clone(),
                    fetched_at: p.fetched_at.clone(),
                })
                .collect();
            self.notification.push_papers_digest(
                "每日论文推荐".to_string(),
                format!("{count} papers found based on your recent work"),
                entries,
                self.app_handle.clone(),
            );
        }
        Ok(count)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FireError {
    #[error("scheduler is already mid-fire")]
    AlreadyInFlight,
    #[error("global opt-in not enabled")]
    NotOptedIn,
    #[error("context extraction returned no keywords")]
    EmptyContext,
    #[error("context extraction task: {0}")]
    ContextExtraction(String),
    #[error("storage / arxiv: {0}")]
    Storage(String),
    #[error("task join: {0}")]
    Join(String),
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Best-effort current local-vs-UTC offset in seconds. Reads
/// `chrono` if available; otherwise asks `date +%z`. On failure
/// returns 0 (treat as UTC).
fn local_offset_seconds() -> i64 {
    let output = std::process::Command::new("date").arg("+%z").output();
    match output {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            parse_date_offset(&s).unwrap_or(0)
        }
        _ => 0,
    }
}

fn parse_date_offset(s: &str) -> Option<i64> {
    // Expect format ±HHMM, e.g. "-0500" or "+0900".
    if s.len() < 5 {
        return None;
    }
    let sign: i64 = if s.starts_with('+') {
        1
    } else if s.starts_with('-') {
        -1
    } else {
        return None;
    };
    let hh: i64 = s[1..3].parse().ok()?;
    let mm: i64 = s[3..5].parse().ok()?;
    Some(sign * (hh * 3600 + mm * 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fire_time_today_returns_today_offset() {
        // 09:00 UTC, +0 offset, target 10am local → 3600s
        let now_unix = 86400 * 1000 + 9 * 3600;
        let wait = duration_until_next_fire_local(now_unix, 10, 0);
        assert_eq!(wait.as_secs(), 3600);
    }

    #[test]
    fn fire_time_tomorrow_when_past_target() {
        // 10:01 UTC, +0 offset, target 10am → 23h 59m
        let now_unix = 86400 * 1000 + 10 * 3600 + 60;
        let wait = duration_until_next_fire_local(now_unix, 10, 0);
        assert_eq!(wait.as_secs(), 86400 - 60);
    }

    #[test]
    fn fire_time_at_exact_target_returns_full_day_to_prevent_spin() {
        // 02:00 UTC = 10:00 +0800 local. At exact equality the loop
        // MUST treat the next fire as a full day away — returning 0
        // would race the in-flight atomic and re-fire within the same
        // second. Startup backfill handles the "should fire now" case.
        let now_unix = 86400 * 1000 + 2 * 3600;
        let wait = duration_until_next_fire_local(now_unix, 10, 8 * 3600);
        assert_eq!(wait.as_secs(), 86400);
    }

    #[test]
    fn backfill_when_never_fired() {
        assert!(should_backfill(1_000_000, None));
    }

    #[test]
    fn backfill_when_stale_beyond_threshold() {
        let now = 1_000_000u64;
        let stale = now - (BACKFILL_THRESHOLD_HOURS + 1) * 3600;
        assert!(should_backfill(now, Some(stale)));
    }

    #[test]
    fn no_backfill_within_threshold() {
        let now = 1_000_000u64;
        let recent = now - 5 * 3600;
        assert!(!should_backfill(now, Some(recent)));
    }

    #[test]
    fn parse_date_offset_handles_both_signs() {
        assert_eq!(parse_date_offset("+0800"), Some(28800));
        assert_eq!(parse_date_offset("-0500"), Some(-18000));
        assert_eq!(parse_date_offset("+0000"), Some(0));
        assert_eq!(parse_date_offset("UTC"), None);
    }
}
