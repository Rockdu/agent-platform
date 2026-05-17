//! NeedsAttention dedup arbiter.
//!
//! Spec: `docs/specs/terminal-events.md` §"Dedup Arbiter Behavior".
//! Production window is exactly 2000 ms per `(plugin_id,
//! terminal_id, kind_name)` — tests must use 2000 ms.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use uuid::Uuid;

/// Spec-required production window. The constructor `for_production()`
/// pins this exactly; tests construct with the same value per spec.
pub const PRODUCTION_WINDOW: Duration = Duration::from_millis(2000);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArbiterDecision {
    Fire,
    Suppress { suppressed_count: usize },
}

#[derive(Debug, Clone)]
struct WindowState {
    fired_at: Instant,
    suppressed_count: usize,
    last_suppressed_event_id: Option<Uuid>,
    last_suppressed_timestamp: Option<Instant>,
}

#[derive(Debug)]
pub struct DedupArbiter {
    window: Duration,
    state: HashMap<String, WindowState>,
}

impl DedupArbiter {
    pub fn for_production() -> Self {
        Self::new(PRODUCTION_WINDOW)
    }

    pub fn new(window: Duration) -> Self {
        Self {
            window,
            state: HashMap::new(),
        }
    }

    /// Decide whether `key` should fire a notification at `now`.
    /// First call for a key in a window returns `Fire`; subsequent
    /// calls inside the same window return `Suppress`.
    pub fn try_record(&mut self, key: &str, event_id: Uuid, now: Instant) -> ArbiterDecision {
        let prior = self.state.get(key).cloned();
        match prior {
            Some(state) if now.duration_since(state.fired_at) < self.window => {
                // Inside an active window — suppress.
                let entry = self.state.get_mut(key).expect("checked above");
                entry.suppressed_count += 1;
                entry.last_suppressed_event_id = Some(event_id);
                entry.last_suppressed_timestamp = Some(now);
                ArbiterDecision::Suppress {
                    suppressed_count: entry.suppressed_count,
                }
            }
            _ => {
                // No active window — Fire and open a fresh one.
                self.state.insert(
                    key.to_string(),
                    WindowState {
                        fired_at: now,
                        suppressed_count: 0,
                        last_suppressed_event_id: None,
                        last_suppressed_timestamp: None,
                    },
                );
                ArbiterDecision::Fire
            }
        }
    }

    /// Diagnostic accessor used by debug views (not user-visible).
    pub fn suppressed_count(&self, key: &str) -> usize {
        self.state.get(key).map(|s| s.suppressed_count).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid() -> Uuid {
        Uuid::new_v4()
    }

    #[test]
    fn same_key_within_2000ms_suppressed() {
        let mut arb = DedupArbiter::for_production();
        let t0 = Instant::now();
        let k = "terminal_mesh:a:Completion";
        assert_eq!(arb.try_record(k, uuid(), t0), ArbiterDecision::Fire);
        let later = t0 + Duration::from_millis(1999);
        match arb.try_record(k, uuid(), later) {
            ArbiterDecision::Suppress { suppressed_count } => assert_eq!(suppressed_count, 1),
            other => panic!("expected Suppress; got {other:?}"),
        }
    }

    #[test]
    fn same_key_at_2000ms_boundary_fires_new() {
        let mut arb = DedupArbiter::for_production();
        let t0 = Instant::now();
        let k = "terminal_mesh:a:Completion";
        assert_eq!(arb.try_record(k, uuid(), t0), ArbiterDecision::Fire);
        let at_boundary = t0 + Duration::from_millis(2000);
        assert_eq!(
            arb.try_record(k, uuid(), at_boundary),
            ArbiterDecision::Fire,
            "elapsed >= 2000ms must fire a new notification"
        );
    }

    #[test]
    fn different_terminal_id_does_not_dedup() {
        let mut arb = DedupArbiter::for_production();
        let t0 = Instant::now();
        assert_eq!(arb.try_record("terminal_mesh:a:Completion", uuid(), t0), ArbiterDecision::Fire);
        // Same plugin + kind, different terminal → independent window.
        assert_eq!(
            arb.try_record("terminal_mesh:b:Completion", uuid(), t0),
            ArbiterDecision::Fire,
        );
    }

    #[test]
    fn different_kind_does_not_dedup() {
        let mut arb = DedupArbiter::for_production();
        let t0 = Instant::now();
        assert_eq!(arb.try_record("terminal_mesh:a:Completion", uuid(), t0), ArbiterDecision::Fire);
        assert_eq!(
            arb.try_record("terminal_mesh:a:NonZeroExit", uuid(), t0),
            ArbiterDecision::Fire,
        );
    }

    #[test]
    fn suppressed_count_accumulates_then_resets_on_new_window() {
        let mut arb = DedupArbiter::for_production();
        let t0 = Instant::now();
        let k = "terminal_mesh:a:Completion";
        arb.try_record(k, uuid(), t0);
        arb.try_record(k, uuid(), t0 + Duration::from_millis(100));
        arb.try_record(k, uuid(), t0 + Duration::from_millis(200));
        assert_eq!(arb.suppressed_count(k), 2);
        // Boundary crossing: next record opens a fresh window.
        arb.try_record(k, uuid(), t0 + Duration::from_millis(2000));
        assert_eq!(arb.suppressed_count(k), 0);
    }

    #[test]
    fn production_window_is_exactly_2000ms() {
        assert_eq!(PRODUCTION_WINDOW, Duration::from_millis(2000));
    }
}
