//! Prompt-waiting heuristic with 1-second debounce.
//!
//! Spec: `docs/specs/terminal-events.md` §"Prompt-Waiting Heuristic".
//! Trigger only when a tail-regex match has been continuously visible
//! for at least the debounce window WITHOUT new output arriving.

use std::time::{Duration, Instant};

use regex::Regex;

const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(1000);

pub struct PromptDetector {
    regexes: Vec<Regex>,
    debounce: Duration,
    /// `Some(Instant)` when the most recent tail observation matched a
    /// prompt regex and no output has arrived since.
    matching_since: Option<Instant>,
    /// True after [`Self::take_fired`] returns the current pending fire
    /// so the same match doesn't re-fire without a fresh debounce.
    fired_for_current_match: bool,
}

impl PromptDetector {
    /// Default bash/zsh prompt regexes per spec: `\$\s*$` and `%\s*$`.
    pub fn default_bash_zsh() -> Self {
        let regexes = vec![
            Regex::new(r"\$\s*$").expect("static regex"),
            Regex::new(r"%\s*$").expect("static regex"),
        ];
        Self {
            regexes,
            debounce: DEFAULT_DEBOUNCE,
            matching_since: None,
            fired_for_current_match: false,
        }
    }

    pub fn with_regexes(regexes: Vec<Regex>) -> Self {
        Self {
            regexes,
            debounce: DEFAULT_DEBOUNCE,
            matching_since: None,
            fired_for_current_match: false,
        }
    }

    pub fn with_debounce(mut self, debounce: Duration) -> Self {
        self.debounce = debounce;
        self
    }

    /// Observe a fresh ring-buffer tail snapshot. Per spec, EVERY
    /// output arrival counts as activity that resets the 1-second
    /// debounce window — even output that still leaves the tail
    /// matching a prompt. Otherwise prompt-output-prompt cycles could
    /// fire prematurely against the older timestamp.
    pub fn on_output(&mut self, tail: &str, now: Instant) {
        let matches = self.regexes.iter().any(|re| re.is_match(tail));
        if matches {
            self.matching_since = Some(now);
            self.fired_for_current_match = false;
        } else {
            self.matching_since = None;
            self.fired_for_current_match = false;
        }
    }

    /// Tick the detector with no new output. Returns `Some(())` once
    /// per qualifying continuous match.
    pub fn tick(&mut self, now: Instant) -> Option<()> {
        if self.fired_for_current_match {
            return None;
        }
        let since = self.matching_since?;
        if now.duration_since(since) >= self.debounce {
            self.fired_for_current_match = true;
            Some(())
        } else {
            None
        }
    }

    /// Convenience: combined observe + check used by the actor task.
    pub fn poll(&mut self, tail: &str, now: Instant) -> Option<()> {
        self.on_output(tail, now);
        self.tick(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dollar_prompt_after_one_second_of_silence_fires_once() {
        let mut d = PromptDetector::default_bash_zsh();
        let t0 = Instant::now();
        d.on_output("user@host:~$ ", t0);
        assert!(d.tick(t0).is_none(), "must wait for debounce");
        assert!(d.tick(t0 + Duration::from_millis(999)).is_none());
        assert_eq!(d.tick(t0 + Duration::from_millis(1000)), Some(()));
        // Second tick must not re-fire without a fresh match cycle.
        assert!(d.tick(t0 + Duration::from_millis(1500)).is_none());
    }

    #[test]
    fn output_during_debounce_resets_timer() {
        let mut d = PromptDetector::default_bash_zsh();
        let t0 = Instant::now();
        d.on_output("$ ", t0);
        // 500ms in, more output arrives that does NOT end in $:
        d.on_output("running tests", t0 + Duration::from_millis(500));
        // Now a prompt comes back:
        d.on_output("$ ", t0 + Duration::from_millis(800));
        // Tick at 1500ms (700ms since fresh prompt) → still waiting.
        assert!(d.tick(t0 + Duration::from_millis(1500)).is_none());
        // Tick at 1800ms (1000ms since fresh prompt) → fires.
        assert_eq!(d.tick(t0 + Duration::from_millis(1800)), Some(()));
    }

    #[test]
    fn percent_prompt_recognized() {
        let mut d = PromptDetector::default_bash_zsh();
        let t0 = Instant::now();
        d.on_output("user@host %% ", t0);
        assert_eq!(d.tick(t0 + Duration::from_secs(1)), Some(()));
    }

    #[test]
    fn non_terminal_dollar_in_middle_of_line_does_not_fire() {
        let mut d = PromptDetector::default_bash_zsh();
        let t0 = Instant::now();
        d.on_output("the price is $5 today", t0);
        assert!(d.tick(t0 + Duration::from_secs(2)).is_none());
    }

    #[test]
    fn poll_combines_on_output_and_tick() {
        // Per the round-23 spec fix, every matching `on_output` resets
        // the debounce window. So `poll` is best demonstrated as
        // "observe one prompt, then a silent tick after the debounce
        // window fires". Re-polling with another matching output
        // would (correctly, per spec) restart the clock.
        let mut d = PromptDetector::default_bash_zsh();
        let t0 = Instant::now();
        assert!(d.poll("$ ", t0).is_none()); // start debounce
        assert_eq!(d.tick(t0 + Duration::from_secs(1)), Some(()));
    }

    /// Codex round-22 blocker #3 regression: matching output arriving
    /// inside the debounce window must reset the timer to the latest
    /// arrival, not keep the older timestamp. Two prompts at t=0 and
    /// t=500ms must push the fire to t=1500ms, not t=1000ms.
    #[test]
    fn repeated_matching_output_resets_debounce_to_latest_arrival() {
        let mut d = PromptDetector::default_bash_zsh();
        let t0 = Instant::now();
        d.on_output("$ ", t0);
        // A second matching output arrives 500ms later. With the
        // pre-fix code this kept matching_since=t0 and would fire at
        // t0+1000. With the fix, matching_since is reset to t0+500.
        d.on_output("user@host:~$ ", t0 + Duration::from_millis(500));
        assert!(
            d.tick(t0 + Duration::from_millis(999)).is_none(),
            "must NOT fire 999ms after first prompt (only 499ms since the reset)"
        );
        assert_eq!(
            d.tick(t0 + Duration::from_millis(1500)),
            Some(()),
            "must fire 1000ms after the latest matching output"
        );
    }
}
