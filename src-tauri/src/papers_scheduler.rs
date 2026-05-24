//! Daily papers digest scheduler.
//!
//! Owns the 10 AM local-time fire, the startup backfill check, the
//! opt-in gate, the in-flight dedup, and the user-facing notification
//! surface. arXiv HTTP itself runs inside the `papers-plugin` sidecar
//! subprocess (spawned via `papers_sidecar_client`) per the immutable
//! plan goal; the host owns timing, context extraction, and reads
//! over the shared SQLite at `${APP_DATA_DIR}/plugins/papers/state.sqlite`.
//!
//! Failure model:
//!   - HTTP / parse errors are recorded into `scheduler_state.last_error`
//!     inside the sidecar's `fetch_papers_gated` call and surfaced to
//!     the host as `FireError::Sidecar`. `last_fired_at` is only
//!     advanced when a scheduled fetch successfully commits rows.
//!   - The opt-in flag is checked on every fire. Disabled → skip
//!     extraction and HTTP entirely (no subprocess spawn beyond the
//!     opt-in check).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use papers_plugin::{FetchPurpose, PaperRecord, PapersStore};

use crate::notification::{NotificationService, PapersTrayEntry};
use crate::papers_context::{extract_workspace_keywords, CONTEXT_BUDGET_BYTES_TOTAL};
use crate::papers_sidecar_client::{fetch_via_sidecar, resolve_binary_path, SidecarClientError};
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

/// Returns true when the next scheduled fire would land on the same
/// local calendar day as `now`, i.e. today's slot is still upcoming.
/// In that case startup backfill must be skipped — otherwise the
/// scheduler would backfill on startup (>20 h gap rule) and then
/// also fire again at the daily slot a few minutes later, producing
/// two arXiv fetches inside the rate-limit window.
///
/// `local_offset_secs` is the same UTC→local offset
/// `duration_until_next_fire_local` uses.
pub fn next_fire_on_same_local_day(
    now_unix_secs: i64,
    wait_until_next_secs: u64,
    local_offset_secs: i64,
) -> bool {
    let now_local = now_unix_secs.saturating_add(local_offset_secs);
    let next_local = now_local.saturating_add(wait_until_next_secs as i64);
    now_local.div_euclid(86400) == next_local.div_euclid(86400)
}

/// Long-lived scheduler state held by the host. Cheap to clone — the
/// store and atomic are wrapped in Arc.
///
/// `binary_root` is the directory tree the resolver walks to find the
/// `papers-plugin` executable (repo root in dev). `app_data_dir` is
/// passed to the sidecar as `APP_DATA_DIR` and is also where the host
/// `PapersStore` is opened — host and sidecar must agree on this so
/// they read/write the same SQLite. `bundle_resource_root` is the
/// optional packaged-app resource directory for `bundle.externalBin`.
#[derive(Clone)]
pub struct PapersScheduler {
    pub store: Arc<PapersStore>,
    pub workspaces: WorkspaceRegistry,
    pub notification: Arc<NotificationService>,
    pub in_flight: Arc<AtomicBool>,
    pub app_handle: Option<tauri::AppHandle>,
    pub binary_root: PathBuf,
    pub bundle_resource_root: Option<PathBuf>,
    pub app_data_dir: PathBuf,
}

impl PapersScheduler {
    pub fn new(
        store: Arc<PapersStore>,
        workspaces: WorkspaceRegistry,
        notification: Arc<NotificationService>,
        app_handle: Option<tauri::AppHandle>,
        binary_root: PathBuf,
        bundle_resource_root: Option<PathBuf>,
        app_data_dir: PathBuf,
    ) -> Self {
        Self {
            store,
            workspaces,
            notification,
            in_flight: Arc::new(AtomicBool::new(false)),
            app_handle,
            binary_root,
            bundle_resource_root,
            app_data_dir,
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
        // Startup backfill: if we missed the last fire by more than
        // the threshold, fire immediately — UNLESS today's scheduled
        // slot is still upcoming. Without the same-day check the
        // following sequence produces two fetches inside the
        // 3-second rate-limit window: yesterday's fire at 10:00,
        // app starts today at 09:50, gap = 23h50m > 20h triggers
        // backfill, loop then sleeps 10 minutes and fires again.
        let now_unix = unix_now_secs();
        let last = self.store.last_fired_at_unix().ok().flatten();
        let offset = local_offset_seconds();
        let wait_until_next = duration_until_next_fire_local(
            now_unix as i64,
            DAILY_FIRE_HOUR_LOCAL,
            offset,
        );
        let same_day = next_fire_on_same_local_day(
            now_unix as i64,
            wait_until_next.as_secs(),
            offset,
        );
        if should_backfill(now_unix, last) && !same_day {
            tracing::info!(
                last_fired_at = ?last,
                "papers-scheduler: triggering startup backfill (>20h since last fire and next slot is tomorrow)"
            );
            let _ = self.fire().await;
        } else if should_backfill(now_unix, last) {
            tracing::info!(
                last_fired_at = ?last,
                wait_secs = wait_until_next.as_secs(),
                "papers-scheduler: skipping startup backfill — today's scheduled slot is still upcoming"
            );
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
            .map_err(|e| FireError::Sidecar(e.to_string()))?;
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
        let binary_root = self.binary_root.clone();
        let bundle_resource_root = self.bundle_resource_root.clone();
        let app_data_dir = self.app_data_dir.clone();
        let inserted: Vec<PaperRecord> = tokio::task::spawn_blocking(move || {
            // Per the immutable goal, arXiv HTTP runs inside the
            // papers-plugin sidecar process. The host owns timing
            // and SQLite reads (list_recent for the UI) but not the
            // network call. The sidecar opens the same SQLite file
            // via APP_DATA_DIR so the host sees the new rows.
            let binary = resolve_binary_path(&binary_root, bundle_resource_root.as_deref())
                .map_err(|e| FireError::Sidecar(e.to_string()))?;
            fetch_via_sidecar(
                &binary,
                &app_data_dir,
                &app_data_dir,
                &query,
                FetchPurpose::ScheduledDigest,
            )
            .map_err(|e: SidecarClientError| FireError::Sidecar(e.to_string()))
        })
        .await
        .map_err(|e| FireError::Join(e.to_string()))?
        .map_err(|e: FireError| e)?;

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
    #[error("papers sidecar: {0}")]
    Sidecar(String),
    #[error("task join: {0}")]
    Join(String),
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Current local-vs-UTC offset in seconds. Uses `chrono::Local`,
/// which calls `localtime_r` on Unix and
/// `GetDynamicTimeZoneInformation` on Windows — the previous
/// `date +%z` shellout silently failed on Windows (the platform's
/// `date` builtin does not understand `+%z`) and made the daily
/// digest fire at 10:00 UTC on Windows hosts instead of 10:00 local.
fn local_offset_seconds() -> i64 {
    chrono::Local::now().offset().local_minus_utc() as i64
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
    fn local_offset_seconds_is_in_valid_earth_range() {
        // Earth's real timezone offsets range from -12:00 (Baker Island)
        // to +14:00 (Kiribati Line Islands). A return value outside
        // [-12h, +14h] means the platform call is broken or the
        // function regressed to the silent-fallback (0) path on a
        // host whose local offset is genuinely non-zero. This test
        // can't distinguish "correct 0" from "broken 0" on UTC CI
        // hosts, but it does pin the range on every other host AND
        // ensures the call itself does not panic on macOS / Linux /
        // Windows.
        let offset = local_offset_seconds();
        assert!(
            (-12 * 3600..=14 * 3600).contains(&offset),
            "local offset {offset}s outside valid earth-timezone range"
        );
    }

    #[test]
    fn next_fire_same_day_when_today_slot_still_upcoming() {
        // 09:50 UTC, target 10:00, +0 offset → wait 600s → same day.
        let now_unix = 86_400_000 + 9 * 3600 + 50 * 60;
        let wait =
            duration_until_next_fire_local(now_unix, DAILY_FIRE_HOUR_LOCAL, 0).as_secs();
        assert!(next_fire_on_same_local_day(now_unix, wait, 0));
    }

    #[test]
    fn next_fire_different_day_when_today_slot_already_past() {
        // 14:00 UTC, target 10:00, +0 offset → wait until tomorrow 10:00.
        let now_unix = 86_400_000 + 14 * 3600;
        let wait =
            duration_until_next_fire_local(now_unix, DAILY_FIRE_HOUR_LOCAL, 0).as_secs();
        assert!(!next_fire_on_same_local_day(now_unix, wait, 0));
    }

    #[test]
    fn next_fire_same_day_check_respects_local_offset() {
        // 02:00 UTC = 10:00 +0800 local. At exact equality
        // `duration_until_next_fire_local` returns 86400 (full day,
        // per the equality-spin guard). So next fire is "tomorrow"
        // 10:00 local — different local day.
        let now_unix = 86_400_000 + 2 * 3600;
        let wait =
            duration_until_next_fire_local(now_unix, DAILY_FIRE_HOUR_LOCAL, 8 * 3600).as_secs();
        assert!(!next_fire_on_same_local_day(now_unix, wait, 8 * 3600));
        // 01:00 UTC = 09:00 +0800 local → wait 3600s → same local day.
        let now_earlier = 86_400_000 + 1 * 3600;
        let wait_earlier =
            duration_until_next_fire_local(now_earlier, DAILY_FIRE_HOUR_LOCAL, 8 * 3600).as_secs();
        assert!(next_fire_on_same_local_day(
            now_earlier,
            wait_earlier,
            8 * 3600
        ));
    }

    #[test]
    fn backfill_skipped_when_same_day_even_if_stale() {
        // 09:50 today, last fire 23h50m ago → would normally backfill,
        // but today's 10:00 slot is upcoming so skip.
        let now = 86_400_000u64 + 9 * 3600 + 50 * 60;
        let last = now - (23 * 3600 + 50 * 60);
        assert!(should_backfill(now, Some(last)));
        let wait =
            duration_until_next_fire_local(now as i64, DAILY_FIRE_HOUR_LOCAL, 0).as_secs();
        assert!(next_fire_on_same_local_day(now as i64, wait, 0));
        // Composed predicate the loop uses: backfill && !same_day.
        assert!(!(should_backfill(now, Some(last))
            && !next_fire_on_same_local_day(now as i64, wait, 0)));
    }

    #[test]
    fn backfill_fires_when_stale_and_next_fire_tomorrow() {
        // 14:00 today, last fire 25h ago → backfill AND next fire is
        // tomorrow → composed predicate fires backfill.
        let now = 86_400_000u64 + 14 * 3600;
        let last = now - 25 * 3600;
        let wait =
            duration_until_next_fire_local(now as i64, DAILY_FIRE_HOUR_LOCAL, 0).as_secs();
        assert!(should_backfill(now, Some(last)));
        assert!(!next_fire_on_same_local_day(now as i64, wait, 0));
        assert!(
            should_backfill(now, Some(last))
                && !next_fire_on_same_local_day(now as i64, wait, 0)
        );
    }
}
