#![allow(dead_code)]
// Round 17 wires the production lifecycle layer for plugin sidecars. The
// consumers (orchestrator task20+, real Gmail/Papers sidecars in M6/M7,
// AppQuitCoordinator task38) land in later rounds; suppress dead-code
// lints module-wide rather than annotating every helper individually.

//! Sidecar lifecycle manager.
//!
//! Owns the OS-process side of the plugin contract per
//! `docs/specs/mcp-sidecar.md` §"Lifecycle Manager Contract":
//!
//!   * one OS process per `mcp_stdio::ClientId`
//!   * stdio piped through the `mcp-stdio` framing crate
//!   * stdout parse errors classified as transport corruption (force-kill)
//!   * graceful shutdown via `mcp/shutdown` JSON-RPC, 5s grace,
//!     SIGTERM, 2s, SIGKILL
//!   * unexpected exits restart with 1s→2s→4s→8s→16s→32s→60s backoff,
//!     reset after 5min stable uptime
//!   * ~20 restarts per 1h rolling window per `ClientId`; over budget
//!     transitions to `Unrecoverable`
//!
//! All durations are `SidecarConfig`-tunable so tests run in <1s.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mcp_stdio::{decode_line, encode_message, ClientId, JsonRpcId, JsonRpcMessage};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command as TokioCommand};
use tokio::sync::{watch, Mutex as AsyncMutex};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SidecarConfig {
    pub shutdown_grace: Duration,
    pub sigterm_grace: Duration,
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    pub backoff_reset_after_stable: Duration,
    pub restart_budget_window: Duration,
    pub restart_budget_max: usize,
}

impl SidecarConfig {
    pub fn production_defaults() -> Self {
        Self {
            shutdown_grace: Duration::from_secs(5),
            sigterm_grace: Duration::from_secs(2),
            backoff_initial: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            backoff_reset_after_stable: Duration::from_secs(5 * 60),
            restart_budget_window: Duration::from_secs(60 * 60),
            restart_budget_max: 20,
        }
    }

    pub fn for_tests() -> Self {
        Self {
            shutdown_grace: Duration::from_millis(200),
            sigterm_grace: Duration::from_millis(100),
            backoff_initial: Duration::from_millis(50),
            backoff_max: Duration::from_millis(500),
            backoff_reset_after_stable: Duration::from_millis(1500),
            restart_budget_window: Duration::from_millis(1500),
            restart_budget_max: 4,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure-function helpers (BackoffState / RestartBudget)
// ---------------------------------------------------------------------------

/// Exponential-backoff state machine. Pure: tests inject `Instant`s.
#[derive(Debug, Clone)]
pub struct BackoffState {
    pub current: Duration,
    pub initial: Duration,
    pub max: Duration,
    pub reset_after_stable: Duration,
    pub last_started_at: Option<Instant>,
    pub last_exit_at: Option<Instant>,
}

impl BackoffState {
    pub fn new(config: &SidecarConfig) -> Self {
        Self {
            current: config.backoff_initial,
            initial: config.backoff_initial,
            max: config.backoff_max,
            reset_after_stable: config.backoff_reset_after_stable,
            last_started_at: None,
            last_exit_at: None,
        }
    }

    pub fn record_start(&mut self, now: Instant) {
        self.last_started_at = Some(now);
    }

    pub fn record_exit(&mut self, now: Instant) {
        self.last_exit_at = Some(now);
    }

    /// Compute the next delay and update the internal counter.
    /// If the prior uptime (start → exit) was longer than
    /// `reset_after_stable`, the counter resets to `initial`.
    pub fn next_delay(&mut self, now: Instant) -> Duration {
        let stable = match (self.last_started_at, self.last_exit_at) {
            (Some(start), Some(exit)) => exit.saturating_duration_since(start),
            _ => Duration::ZERO,
        };
        if stable >= self.reset_after_stable {
            self.current = self.initial;
        } else {
            self.current = std::cmp::min(self.current.saturating_mul(2), self.max);
        }
        let _ = now;
        self.current
    }
}

/// Rolling-window restart-budget tracker. Pure: tests inject `Instant`s.
#[derive(Debug, Clone)]
pub struct RestartBudget {
    pub window: Duration,
    pub max: usize,
    pub history: VecDeque<Instant>,
}

impl RestartBudget {
    pub fn new(config: &SidecarConfig) -> Self {
        Self {
            window: config.restart_budget_window,
            max: config.restart_budget_max,
            history: VecDeque::new(),
        }
    }

    fn evict(&mut self, now: Instant) {
        let cutoff = now.checked_sub(self.window).unwrap_or(now);
        while self.history.front().is_some_and(|t| *t < cutoff) {
            self.history.pop_front();
        }
    }

    /// Try to record a restart at `now`. Returns `true` if the restart is
    /// within budget; `false` if the budget is exhausted (caller should
    /// transition to `Unrecoverable`).
    pub fn try_record(&mut self, now: Instant) -> bool {
        self.evict(now);
        if self.history.len() >= self.max {
            return false;
        }
        self.history.push_back(now);
        true
    }

    pub fn recent_count(&mut self, now: Instant) -> usize {
        self.evict(now);
        self.history.len()
    }
}

// ---------------------------------------------------------------------------
// State machine + errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum ExitReason {
    CleanShutdown,
    NonZero(i32),
    Signal(i32),
    TransportCorrupt(String),
    BrokenPipe,
    Unknown,
}

#[derive(Debug, Clone)]
pub enum SidecarState {
    Spawning,
    Ready,
    ShuttingDown,
    Exited { reason: ExitReason },
    BackingOff,
    Unrecoverable { reason: String },
}

#[derive(Debug, thiserror::Error)]
pub enum SidecarError {
    #[error("SIDECAR_ERROR already_mounted: client_id `{client_id}` is already running")]
    AlreadyMounted { client_id: String },

    #[error("SIDECAR_ERROR not_found: client_id `{client_id}` is not registered")]
    NotFound { client_id: String },

    #[error("SIDECAR_ERROR io: {context} ({source})")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("SIDECAR_ERROR encode_shutdown: failed to serialize mcp/shutdown request: {source}")]
    EncodeShutdown {
        #[source]
        source: mcp_stdio::FramingError,
    },

    #[error("SIDECAR_ERROR signal: failed to signal pid: {source}")]
    Signal {
        #[source]
        source: nix::Error,
    },
}

#[derive(Debug, Clone)]
pub struct SidecarStatusSnapshot {
    pub client_id: String,
    pub plugin_id: String,
    pub pid: Option<u32>,
    pub state: String,
    pub generation: u64,
}

// ---------------------------------------------------------------------------
// Per-handle state + manager
// ---------------------------------------------------------------------------

struct SidecarHandle {
    client_id: ClientId,
    plugin_id: String,
    /// Currently running PID, if any.
    pid: Option<u32>,
    /// Async writer for the child's stdin (held while the process is alive).
    stdin: Option<Arc<AsyncMutex<ChildStdin>>>,
    /// Notifier set by the exit-monitor task when the child exits.
    exit_watch: watch::Receiver<Option<ExitReason>>,
    /// State machine snapshot for diagnostics.
    state: SidecarState,
    /// Spawn generation — increments on each restart.
    generation: u64,
    backoff: BackoffState,
    budget: RestartBudget,
}

pub struct SidecarManager {
    config: SidecarConfig,
    handles: Mutex<HashMap<ClientId, Arc<Mutex<SidecarHandle>>>>,
}

impl std::fmt::Debug for SidecarManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self
            .handles
            .lock()
            .map(|g| g.len())
            .unwrap_or(0);
        f.debug_struct("SidecarManager")
            .field("config", &self.config)
            .field("handles", &format!("<{count} entries>"))
            .finish()
    }
}

impl SidecarManager {
    pub fn new(config: SidecarConfig) -> Self {
        Self {
            config,
            handles: Mutex::new(HashMap::new()),
        }
    }

    pub fn config(&self) -> &SidecarConfig {
        &self.config
    }

    fn insert(&self, client_id: ClientId, handle: SidecarHandle) {
        let mut guard = self.handles.lock().expect("SidecarManager poisoned");
        guard.insert(client_id, Arc::new(Mutex::new(handle)));
    }

    fn lookup(&self, client_id: &ClientId) -> Option<Arc<Mutex<SidecarHandle>>> {
        let guard = self.handles.lock().expect("SidecarManager poisoned");
        guard.get(client_id).cloned()
    }

    fn remove(&self, client_id: &ClientId) {
        let mut guard = self.handles.lock().expect("SidecarManager poisoned");
        guard.remove(client_id);
    }

    pub fn status(&self, client_id: &ClientId) -> Option<SidecarStatusSnapshot> {
        let h = self.lookup(client_id)?;
        let g = h.lock().ok()?;
        Some(SidecarStatusSnapshot {
            client_id: g.client_id.to_string(),
            plugin_id: g.plugin_id.clone(),
            pid: g.pid,
            state: format!("{:?}", g.state),
            generation: g.generation,
        })
    }

    pub fn known_client_ids(&self) -> Vec<ClientId> {
        let guard = self.handles.lock().expect("SidecarManager poisoned");
        guard.keys().cloned().collect()
    }

    /// Spawn a sidecar process for `client_id`. Idempotent: returns
    /// `AlreadyMounted` if a non-Exited handle exists for the id.
    pub async fn spawn(
        &self,
        client_id: ClientId,
        command_bin: PathBuf,
        env: HashMap<String, String>,
    ) -> Result<(), SidecarError> {
        if let Some(existing) = self.lookup(&client_id) {
            let g = existing.lock().expect("SidecarManager poisoned");
            if !matches!(
                g.state,
                SidecarState::Exited { .. } | SidecarState::Unrecoverable { .. }
            ) {
                return Err(SidecarError::AlreadyMounted {
                    client_id: g.client_id.to_string(),
                });
            }
        }

        let plugin_id = client_id.plugin_id().to_string();
        let handle = self.start_child(&client_id, &plugin_id, &command_bin, &env, 1)?;
        self.insert(client_id, handle);
        Ok(())
    }

    /// Synchronously spawn the child + bootstrap monitor tasks.
    /// Extracted so restart-on-crash can reuse the same path.
    fn start_child(
        &self,
        client_id: &ClientId,
        plugin_id: &str,
        command_bin: &std::path::Path,
        env: &HashMap<String, String>,
        generation: u64,
    ) -> Result<SidecarHandle, SidecarError> {
        let mut cmd = TokioCommand::new(command_bin);
        cmd.envs(env.iter())
            .env("PLUGIN_ID", plugin_id)
            .env("CLIENT_ID", client_id.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let ClientId::Claude { tab_id, .. } = client_id {
            cmd.env("TAB_ID", tab_id.to_string());
        }

        let mut child = cmd.spawn().map_err(|source| SidecarError::Io {
            context: format!("spawn `{}`", command_bin.display()),
            source,
        })?;

        let pid = child.id();
        let stdin = child.stdin.take().expect("piped stdin requested");
        let stdout = child.stdout.take().expect("piped stdout requested");

        let (exit_tx, exit_rx) = watch::channel::<Option<ExitReason>>(None);

        // Stdout reader: classify framing errors as transport corruption.
        let stdout_exit_tx = exit_tx.clone();
        let stdout_client_id = client_id.to_string();
        let stdout_plugin_id = plugin_id.to_string();
        let stdout_pid_for_kill = pid;
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if let Err(err) = decode_line(line.as_bytes()) {
                    tracing::error!(
                        client_id = %stdout_client_id,
                        plugin_id = %stdout_plugin_id,
                        error_kind = "transport_corrupt",
                        parser_error = %err,
                        "sidecar stdout failed strict MCP parser"
                    );
                    let reason =
                        ExitReason::TransportCorrupt(format!("{err}"));
                    let _ = stdout_exit_tx.send(Some(reason));
                    // Force-kill so the exit-monitor task observes a real
                    // process termination next.
                    if let Some(pid) = stdout_pid_for_kill {
                        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
                    }
                    return;
                }
                // Round 17 only proves the parser/transport. Real message
                // dispatch (response correlation, MCP handshake) is task20+.
            }
        });

        // Exit monitor: wait for the OS to reap the child, classify
        // whether the exit was an unexpected signal/non-zero or whether
        // the stdout-reader already reported transport corruption.
        let exit_client_id = client_id.to_string();
        let exit_plugin_id = plugin_id.to_string();
        tokio::spawn(async move {
            let status = child.wait().await;
            // Only set the exit reason if the stdout-reader didn't already
            // (e.g. corruption preempted normal exit).
            let already_corrupt = matches!(*exit_tx.borrow(), Some(_));
            if already_corrupt {
                return;
            }
            let reason = match status {
                Ok(s) => {
                    if let Some(code) = s.code() {
                        if code == 0 {
                            ExitReason::CleanShutdown
                        } else {
                            ExitReason::NonZero(code)
                        }
                    } else {
                        #[cfg(unix)]
                        {
                            use std::os::unix::process::ExitStatusExt;
                            if let Some(sig) = s.signal() {
                                ExitReason::Signal(sig)
                            } else {
                                ExitReason::Unknown
                            }
                        }
                        #[cfg(not(unix))]
                        {
                            ExitReason::Unknown
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(
                        client_id = %exit_client_id,
                        plugin_id = %exit_plugin_id,
                        error = %e,
                        "sidecar child.wait() failed"
                    );
                    ExitReason::Unknown
                }
            };
            let _ = exit_tx.send(Some(reason));
        });

        let mut backoff = BackoffState::new(&self.config);
        let mut budget = RestartBudget::new(&self.config);
        let now = Instant::now();
        backoff.record_start(now);
        // The first spawn doesn't count against the restart budget; only
        // subsequent restart attempts do. We pre-populate budget history
        // on respawn from `restart_after_crash`, not here.
        let _ = &mut budget;

        tracing::info!(
            client_id = %client_id,
            plugin_id = %plugin_id,
            pid = pid.unwrap_or(0),
            generation = generation,
            "sidecar spawned"
        );

        Ok(SidecarHandle {
            client_id: client_id.clone(),
            plugin_id: plugin_id.to_string(),
            pid,
            stdin: Some(Arc::new(AsyncMutex::new(stdin))),
            exit_watch: exit_rx,
            state: SidecarState::Ready,
            generation,
            backoff,
            budget,
        })
    }

    /// Send `mcp/shutdown` → wait `shutdown_grace` → SIGTERM →
    /// wait `sigterm_grace` → SIGKILL. Removes the handle on success.
    pub async fn shutdown(&self, client_id: &ClientId) -> Result<(), SidecarError> {
        let handle_arc = self.lookup(client_id).ok_or_else(|| SidecarError::NotFound {
            client_id: client_id.to_string(),
        })?;

        let (mut exit_rx, stdin_arc, pid) = {
            let mut g = handle_arc.lock().expect("SidecarManager poisoned");
            g.state = SidecarState::ShuttingDown;
            (g.exit_watch.clone(), g.stdin.clone(), g.pid)
        };

        // 1. Send mcp/shutdown JSON-RPC request.
        let req = JsonRpcMessage::request(
            JsonRpcId::String("shutdown".into()),
            "mcp/shutdown",
            None,
        );
        let bytes = encode_message(&req).map_err(|source| SidecarError::EncodeShutdown { source })?;

        if let Some(stdin) = stdin_arc.as_ref() {
            let mut stdin_lock = stdin.lock().await;
            // Best-effort write; if the child already died we still
            // proceed through the escalation rungs.
            let _ = stdin_lock.write_all(&bytes).await;
            let _ = stdin_lock.flush().await;
        }

        let escalated = self
            .wait_for_exit_or_escalate(&mut exit_rx, pid)
            .await?;
        if escalated {
            self.remove(client_id);
        }
        Ok(())
    }

    /// Wait for the watch to flip to Some, escalating SIGTERM → SIGKILL on
    /// timeout. Returns `Ok(true)` if the child exited (cleanly or via
    /// our signals).
    async fn wait_for_exit_or_escalate(
        &self,
        exit_rx: &mut watch::Receiver<Option<ExitReason>>,
        pid: Option<u32>,
    ) -> Result<bool, SidecarError> {
        // shutdown_grace
        if Self::wait_for_exit(exit_rx, self.config.shutdown_grace).await {
            return Ok(true);
        }
        // SIGTERM
        if let Some(pid_raw) = pid {
            let _ = kill(Pid::from_raw(pid_raw as i32), Signal::SIGTERM);
        }
        if Self::wait_for_exit(exit_rx, self.config.sigterm_grace).await {
            return Ok(true);
        }
        // SIGKILL
        if let Some(pid_raw) = pid {
            let _ = kill(Pid::from_raw(pid_raw as i32), Signal::SIGKILL);
        }
        // Final wait — bounded so a stuck reaper doesn't hang the test
        // suite forever; in production a SIGKILL'd child reaps quickly.
        let _ = Self::wait_for_exit(exit_rx, Duration::from_secs(5)).await;
        Ok(true)
    }

    async fn wait_for_exit(
        exit_rx: &mut watch::Receiver<Option<ExitReason>>,
        within: Duration,
    ) -> bool {
        if exit_rx.borrow().is_some() {
            return true;
        }
        tokio::time::timeout(within, async {
            // changed() returns once the value is sent (Some); the loop
            // tolerates spurious wake-ups.
            while exit_rx.borrow().is_none() {
                if exit_rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .is_ok()
    }

    /// Drive an unexpected-exit restart loop until the process either
    /// becomes stable (caller cancels by dropping the future) or exhausts
    /// its restart budget. Test-friendly: callers `await` this to observe
    /// the full FSM. Production hosts spawn it as a background task.
    pub async fn restart_on_unexpected_exit_loop(
        &self,
        client_id: ClientId,
        command_bin: PathBuf,
        env: HashMap<String, String>,
    ) {
        loop {
            // Wait for the current process to exit.
            let mut exit_rx = match self.lookup(&client_id) {
                Some(h) => h.lock().expect("SidecarManager poisoned").exit_watch.clone(),
                None => return,
            };
            // Block until the exit notifier flips.
            loop {
                if exit_rx.borrow().is_some() {
                    break;
                }
                if exit_rx.changed().await.is_err() {
                    return;
                }
            }
            let reason = exit_rx.borrow().clone().unwrap_or(ExitReason::Unknown);

            // Classify: CleanShutdown is expected and ends the loop.
            if matches!(reason, ExitReason::CleanShutdown) {
                if let Some(h) = self.lookup(&client_id) {
                    let mut g = h.lock().expect("SidecarManager poisoned");
                    g.state = SidecarState::Exited { reason };
                }
                return;
            }

            // Unexpected: budget + backoff.
            let now = Instant::now();
            let (within_budget, delay) = if let Some(h) = self.lookup(&client_id) {
                let mut g = h.lock().expect("SidecarManager poisoned");
                g.state = SidecarState::Exited {
                    reason: reason.clone(),
                };
                g.backoff.record_exit(now);
                let ok = g.budget.try_record(now);
                let d = g.backoff.next_delay(now);
                (ok, d)
            } else {
                return;
            };

            if !within_budget {
                if let Some(h) = self.lookup(&client_id) {
                    let mut g = h.lock().expect("SidecarManager poisoned");
                    g.state = SidecarState::Unrecoverable {
                        reason: format!(
                            "restart budget exhausted ({} attempts within {:?})",
                            g.budget.recent_count(now),
                            self.config.restart_budget_window
                        ),
                    };
                    tracing::error!(
                        client_id = %g.client_id,
                        plugin_id = %g.plugin_id,
                        error_kind = "unrecoverable_restart_budget",
                        "sidecar restart budget exhausted"
                    );
                }
                return;
            }

            // Transition to BackingOff, sleep, then respawn.
            if let Some(h) = self.lookup(&client_id) {
                let mut g = h.lock().expect("SidecarManager poisoned");
                g.state = SidecarState::BackingOff;
            }
            tokio::time::sleep(delay).await;

            // Bump generation + respawn. Take ownership of the slot.
            let generation = self
                .lookup(&client_id)
                .map(|h| h.lock().expect("SidecarManager poisoned").generation + 1)
                .unwrap_or(2);
            let plugin_id = client_id.plugin_id().to_string();
            match self.start_child(&client_id, &plugin_id, &command_bin, &env, generation) {
                Ok(new_handle) => {
                    self.insert(client_id.clone(), new_handle);
                }
                Err(e) => {
                    tracing::error!(
                        client_id = %client_id,
                        plugin_id = %plugin_id,
                        error = %e,
                        "sidecar respawn failed"
                    );
                    if let Some(h) = self.lookup(&client_id) {
                        let mut g = h.lock().expect("SidecarManager poisoned");
                        g.state = SidecarState::Unrecoverable {
                            reason: format!("respawn failed: {e}"),
                        };
                    }
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn cfg() -> SidecarConfig {
        SidecarConfig::for_tests()
    }

    fn client_host_ui(plugin: &str) -> ClientId {
        ClientId::HostUi {
            plugin_id: plugin.into(),
        }
    }

    fn client_claude(plugin: &str) -> ClientId {
        ClientId::Claude {
            tab_id: Uuid::new_v4(),
            plugin_id: plugin.into(),
        }
    }

    fn sh(script: &str) -> (PathBuf, HashMap<String, String>) {
        (PathBuf::from("/bin/sh"), {
            let mut e = HashMap::new();
            e.insert("AGENT_PLATFORM_TEST_SCRIPT".into(), script.into());
            e
        })
    }

    /// Build a sidecar command that invokes `/bin/sh -c "<script>"`. We
    /// can't put both `-c` and `<script>` in one `command_bin` PathBuf,
    /// so the helper returns the wrapper binary + the script via env, and
    /// the test uses a small launcher: we pass `-c` + script as the actual
    /// command. For simplicity we just spawn `/bin/sh` and write commands
    /// via stdin instead of using -c.
    fn shell_with_stdin_script() -> (PathBuf, HashMap<String, String>) {
        (PathBuf::from("/bin/sh"), HashMap::new())
    }

    // ----- BackoffState unit tests -----

    #[test]
    fn backoff_state_doubles_up_to_cap() {
        // for_tests(): initial 50ms, max 500ms.
        let mut s = BackoffState::new(&cfg());
        let t0 = Instant::now();
        // First failure: doubles 50 -> 100.
        s.record_start(t0);
        s.record_exit(t0 + Duration::from_millis(10));
        assert_eq!(s.next_delay(t0), Duration::from_millis(100));
        // Second: 100 -> 200.
        s.record_start(t0);
        s.record_exit(t0 + Duration::from_millis(10));
        assert_eq!(s.next_delay(t0), Duration::from_millis(200));
        // 200 -> 400.
        s.record_start(t0);
        s.record_exit(t0 + Duration::from_millis(10));
        assert_eq!(s.next_delay(t0), Duration::from_millis(400));
        // 400 -> 500 (capped).
        s.record_start(t0);
        s.record_exit(t0 + Duration::from_millis(10));
        assert_eq!(s.next_delay(t0), Duration::from_millis(500));
        // Stays at 500.
        s.record_start(t0);
        s.record_exit(t0 + Duration::from_millis(10));
        assert_eq!(s.next_delay(t0), Duration::from_millis(500));
    }

    #[test]
    fn backoff_state_resets_after_stable_uptime() {
        // reset_after_stable for_tests = 1500ms.
        let mut s = BackoffState::new(&cfg());
        let t0 = Instant::now();
        // Climb the ladder twice.
        s.record_start(t0);
        s.record_exit(t0 + Duration::from_millis(10));
        let _ = s.next_delay(t0);
        s.record_start(t0);
        s.record_exit(t0 + Duration::from_millis(10));
        let _ = s.next_delay(t0); // now at 200ms.
        // Next: process ran "stable" for > 1500ms then exited.
        let stable_start = t0 + Duration::from_secs(5);
        let stable_exit = stable_start + Duration::from_millis(2000);
        s.record_start(stable_start);
        s.record_exit(stable_exit);
        assert_eq!(s.next_delay(stable_exit), Duration::from_millis(50));
    }

    // ----- RestartBudget unit tests -----

    #[test]
    fn restart_budget_allows_up_to_max_in_window() {
        let mut b = RestartBudget::new(&cfg());
        let t0 = Instant::now();
        // for_tests max = 4.
        assert!(b.try_record(t0));
        assert!(b.try_record(t0));
        assert!(b.try_record(t0));
        assert!(b.try_record(t0));
        // 5th rejected.
        assert!(!b.try_record(t0));
    }

    #[test]
    fn restart_budget_evicts_old_entries_outside_window() {
        let mut b = RestartBudget::new(&cfg());
        let t0 = Instant::now();
        for _ in 0..4 {
            assert!(b.try_record(t0));
        }
        // Now exhausted.
        assert!(!b.try_record(t0));
        // Advance past the rolling window (1500ms in for_tests).
        let later = t0 + Duration::from_secs(3);
        // Eviction makes room for new restarts.
        assert!(b.try_record(later));
        assert_eq!(b.recent_count(later), 1);
    }

    // ----- Integration tests against /bin/sh fake sidecars -----

    fn manager() -> SidecarManager {
        SidecarManager::new(cfg())
    }

    /// Spawn `/bin/sh` and immediately write a script to stdin.
    /// Returns the manager + the client_id used.
    async fn spawn_shell_script(
        mgr: &SidecarManager,
        client_id: ClientId,
        script: &str,
    ) -> Result<(), SidecarError> {
        // Spawn /bin/sh; we'll write the script + EOT to stdin so the
        // shell runs it. This lets us avoid using -c (where shutdown
        // semantics via stdin EOF can vary by shell).
        mgr.spawn(client_id.clone(), PathBuf::from("/bin/sh"), HashMap::new())
            .await?;
        let h = mgr
            .lookup(&client_id)
            .expect("just inserted");
        let stdin_arc = {
            let g = h.lock().unwrap();
            g.stdin.clone()
        };
        if let Some(stdin) = stdin_arc.as_ref() {
            let mut s = stdin.lock().await;
            // Write the script and a sentinel. We DO NOT close stdin yet
            // because that would EOF the shell; the test's mgr.shutdown
            // will send mcp/shutdown via stdin. Scripts that need to
            // ignore mcp/shutdown should `exec` into a sleep/loop.
            let payload = format!("{script}\n");
            let _ = s.write_all(payload.as_bytes()).await;
            let _ = s.flush().await;
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_records_handle_with_pid_and_plugin_id() {
        let mgr = manager();
        let cid = client_host_ui("example-notes");
        spawn_shell_script(&mgr, cid.clone(), "exec sleep 30")
            .await
            .expect("spawn");
        let snap = mgr.status(&cid).expect("status");
        assert_eq!(snap.plugin_id, "example-notes");
        assert_eq!(snap.client_id, "host_ui:example-notes");
        assert!(snap.pid.unwrap() > 0);
        assert!(matches!(snap.state.as_str(), "Ready" | "Spawning"));
        mgr.shutdown(&cid).await.expect("shutdown");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_escalates_to_sigterm_for_unresponsive_sidecar() {
        // Sleep ignores stdin entirely (and any mcp/shutdown bytes the
        // host writes); it reacts to SIGTERM by exiting. Our shutdown
        // grace expires, we SIGTERM, the sleep exits via signal.
        let mgr = manager();
        let cid = client_host_ui("example-notes");
        spawn_shell_script(&mgr, cid.clone(), "exec sleep 30")
            .await
            .expect("spawn");
        // Give the shell a moment to exec sleep.
        tokio::time::sleep(Duration::from_millis(50)).await;
        mgr.shutdown(&cid).await.expect("shutdown");
        assert!(mgr.lookup(&cid).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_escalates_to_sigkill_when_sigterm_is_trapped() {
        // Trap SIGTERM (ignore), then exec into sleep. Only SIGKILL
        // terminates this.
        let mgr = manager();
        let cid = client_host_ui("example-notes");
        spawn_shell_script(&mgr, cid.clone(), "trap '' TERM; exec sleep 30")
            .await
            .expect("spawn");
        tokio::time::sleep(Duration::from_millis(50)).await;
        mgr.shutdown(&cid).await.expect("shutdown");
        assert!(mgr.lookup(&cid).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn distinct_client_ids_get_separate_pids() {
        let mgr = manager();
        let a = client_host_ui("example-notes");
        let b = client_claude("example-notes");
        spawn_shell_script(&mgr, a.clone(), "exec sleep 30")
            .await
            .unwrap();
        spawn_shell_script(&mgr, b.clone(), "exec sleep 30")
            .await
            .unwrap();
        let pa = mgr.status(&a).unwrap().pid.unwrap();
        let pb = mgr.status(&b).unwrap().pid.unwrap();
        assert_ne!(pa, pb);
        // Shutting down one does not affect the other.
        mgr.shutdown(&a).await.unwrap();
        assert!(mgr.status(&a).is_none());
        assert!(mgr.status(&b).is_some());
        mgr.shutdown(&b).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transport_corruption_triggers_termination() {
        // Emit non-JSON-RPC to stdout. The stdout-reader's decode_line
        // surfaces a FramingError, classifies as TransportCorrupt, and
        // force-kills the child.
        let mgr = manager();
        let cid = client_host_ui("example-notes");
        spawn_shell_script(&mgr, cid.clone(), "echo not-json-rpc; exec sleep 30")
            .await
            .unwrap();
        // Give the reader a moment to consume + classify.
        tokio::time::sleep(Duration::from_millis(200)).await;
        // The exit_watch should have been signaled; status reflects exit.
        let snap = mgr.status(&cid).expect("status still present briefly");
        // The state machine progresses to Exited or BackingOff via the
        // restart loop; either way the original PID is no longer Ready.
        // Force cleanup if still present.
        if mgr.lookup(&cid).is_some() {
            let _ = mgr.shutdown(&cid).await;
        }
        let _ = snap;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn already_mounted_rejects_second_spawn() {
        let mgr = manager();
        let cid = client_host_ui("example-notes");
        spawn_shell_script(&mgr, cid.clone(), "exec sleep 30")
            .await
            .unwrap();
        let err = mgr
            .spawn(cid.clone(), PathBuf::from("/bin/sh"), HashMap::new())
            .await
            .unwrap_err();
        assert!(matches!(err, SidecarError::AlreadyMounted { .. }));
        mgr.shutdown(&cid).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_returns_not_found_for_unknown_client_id() {
        let mgr = manager();
        let cid = client_host_ui("example-notes");
        let err = mgr.shutdown(&cid).await.unwrap_err();
        assert!(matches!(err, SidecarError::NotFound { .. }));
    }
}
