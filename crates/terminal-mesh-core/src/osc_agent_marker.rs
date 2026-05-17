//! OSC attention parser: recognizes two distinct OSC sequences.
//!
//! - `ESC ] 1337 ; AgentTask ; <severity>;<base64-summary> <BEL or ESC \>`
//!   yields an `AgentMarker` event (general agent activity notification).
//! - `ESC ] 1339 ; am-task-complete ; <severity>;<base64-summary> BEL`
//!   yields a `TaskComplete` event (final task-done marker that drives
//!   the Done queue transition).
//!
//! Spec: `docs/specs/terminal-events.md` §"OSC Agent Marker Sequence"
//! and `docs/specs/transport.md` §8 "TaskComplete OSC marker convention".
//! Stateful — partial sequences resume across feeds. Maximum payload
//! size is 4 KB; oversize → no event + reset. Parsed marker bytes are
//! consumed as control sequences and do not render visibly in the
//! terminal.

use base64::Engine;

use crate::events::AttentionSeverity;

/// 4 KB maximum total OSC payload per spec.
pub const OSC_MAX_PAYLOAD_BYTES: usize = 4096;
const TRUNCATION_MARKER: &str = " … [truncated]";

const ESC: u8 = 0x1B;
const BEL: u8 = 0x07;
const RBRACKET: u8 = b']';
const BACKSLASH: u8 = b'\\';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    AfterEsc,
    InOsc,
    InOscEsc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMarkerEvent {
    pub summary: Option<String>,
    pub severity: AttentionSeverity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OscAttentionEvent {
    AgentMarker(AgentMarkerEvent),
    TaskComplete { summary: String },
}

pub struct OscAttentionParser {
    state: State,
    payload: Vec<u8>,
    max_payload: usize,
}

impl Default for OscAttentionParser {
    fn default() -> Self {
        Self::new()
    }
}

impl OscAttentionParser {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            payload: Vec::with_capacity(256),
            max_payload: OSC_MAX_PAYLOAD_BYTES,
        }
    }

    /// Feed `bytes` and emit any complete attention events found.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<OscAttentionEvent> {
        let mut out = Vec::new();
        for &b in bytes {
            match self.state {
                State::Ground => {
                    if b == ESC {
                        self.state = State::AfterEsc;
                    }
                }
                State::AfterEsc => {
                    if b == RBRACKET {
                        self.state = State::InOsc;
                        self.payload.clear();
                    } else if b == ESC {
                        // Restart escape; stay in AfterEsc.
                    } else {
                        // ESC then non-OSC → drop back to Ground.
                        self.state = State::Ground;
                    }
                }
                State::InOsc => {
                    if b == BEL {
                        if let Some(ev) = self.try_parse_payload() {
                            out.push(ev);
                        }
                        self.reset();
                    } else if b == ESC {
                        self.state = State::InOscEsc;
                    } else if self.payload.len() >= self.max_payload {
                        // Oversize → drop the event silently and reset
                        // state per spec.
                        self.reset();
                    } else {
                        self.payload.push(b);
                    }
                }
                State::InOscEsc => {
                    if b == BACKSLASH {
                        if let Some(ev) = self.try_parse_payload() {
                            out.push(ev);
                        }
                        self.reset();
                    } else if b == ESC {
                        // Spurious ESC ESC inside OSC; stay InOscEsc.
                    } else {
                        // ESC + non-backslash inside OSC abort.
                        self.reset();
                    }
                }
            }
        }
        out
    }

    fn reset(&mut self) {
        self.state = State::Ground;
        self.payload.clear();
    }

    fn try_parse_payload(&self) -> Option<OscAttentionEvent> {
        // Expected payload after `ESC ]` prefix:
        //   1337;AgentTask;<severity>;<base64-summary>
        //   1339;am-task-complete;<severity>;<base64-summary>
        let payload = std::str::from_utf8(&self.payload).ok()?;
        let mut parts = payload.splitn(4, ';');
        let id = parts.next()?;
        let cmd = parts.next()?;
        let sev_str = parts.next()?;
        let b64 = parts.next()?;

        match (id, cmd) {
            ("1337", "AgentTask") => Some(OscAttentionEvent::AgentMarker(parse_agent_marker(
                sev_str, b64,
            )?)),
            ("1339", "am-task-complete") => Some(OscAttentionEvent::TaskComplete {
                summary: parse_task_complete(sev_str, b64)?,
            }),
            _ => None,
        }
    }
}

fn parse_agent_marker(sev_str: &str, b64: &str) -> Option<AgentMarkerEvent> {
    let severity = match sev_str {
        "Info" => AttentionSeverity::Info,
        "NeedsConfirm" => AttentionSeverity::NeedsConfirm,
        "Error" => AttentionSeverity::Error,
        _ => return None,
    };
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .ok()?;
    let summary = String::from_utf8(decoded).ok()?;
    Some(AgentMarkerEvent {
        summary: Some(summary),
        severity,
    })
}

fn parse_task_complete(sev_str: &str, b64: &str) -> Option<String> {
    // Validate severity per spec even though the AttentionKind variant
    // does not carry it today; unknown severities are ignored.
    match sev_str {
        "info" | "success" | "warning" | "error" => {}
        _ => return None,
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .ok()?;
    String::from_utf8(decoded).ok()
}

/// Helper: truncate `s` to roughly `max_chars` characters; append the
/// canonical truncation marker if truncation occurred.
pub fn truncate_summary(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let take: String = s.chars().take(max_chars).collect();
    format!("{take}{TRUNCATION_MARKER}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD as B64;

    fn build_agent_marker(severity: &str, summary: &str, terminator: u8) -> Vec<u8> {
        let b64 = B64.encode(summary);
        let mut v = Vec::new();
        v.extend_from_slice(b"\x1b]");
        v.extend_from_slice(b"1337;AgentTask;");
        v.extend_from_slice(severity.as_bytes());
        v.push(b';');
        v.extend_from_slice(b64.as_bytes());
        v.push(terminator);
        if terminator == 0x1B {
            v.push(b'\\');
        }
        v
    }

    fn build_task_complete(severity: &str, summary: &str) -> Vec<u8> {
        let b64 = B64.encode(summary);
        let mut v = Vec::new();
        v.extend_from_slice(b"\x1b]1339;am-task-complete;");
        v.extend_from_slice(severity.as_bytes());
        v.push(b';');
        v.extend_from_slice(b64.as_bytes());
        v.push(BEL);
        v
    }

    fn agent_marker(events: &[OscAttentionEvent]) -> Option<&AgentMarkerEvent> {
        events.iter().find_map(|e| match e {
            OscAttentionEvent::AgentMarker(m) => Some(m),
            _ => None,
        })
    }

    fn task_complete_summary(events: &[OscAttentionEvent]) -> Option<&str> {
        events.iter().find_map(|e| match e {
            OscAttentionEvent::TaskComplete { summary } => Some(summary.as_str()),
            _ => None,
        })
    }

    #[test]
    fn complete_marker_in_single_feed_emits_event() {
        let mut p = OscAttentionParser::new();
        let bytes = build_agent_marker("Info", "hello", BEL);
        let evs = p.feed(&bytes);
        assert_eq!(evs.len(), 1);
        let m = agent_marker(&evs).expect("agent marker");
        assert_eq!(m.severity, AttentionSeverity::Info);
        assert_eq!(m.summary.as_deref(), Some("hello"));
    }

    #[test]
    fn marker_split_across_two_feeds_emits_event_after_terminator() {
        let mut p = OscAttentionParser::new();
        let bytes = build_agent_marker("NeedsConfirm", "中文摘要", BEL);
        let (a, b) = bytes.split_at(10);
        assert!(p.feed(a).is_empty(), "no event before terminator");
        let evs = p.feed(b);
        assert_eq!(evs.len(), 1);
        let m = agent_marker(&evs).expect("agent marker");
        assert_eq!(m.severity, AttentionSeverity::NeedsConfirm);
        assert_eq!(m.summary.as_deref(), Some("中文摘要"));
    }

    #[test]
    fn malformed_severity_emits_no_event() {
        let mut p = OscAttentionParser::new();
        let bytes = build_agent_marker("Critical", "hello", BEL);
        let evs = p.feed(&bytes);
        assert!(evs.is_empty());
    }

    #[test]
    fn malformed_base64_emits_no_event() {
        let mut p = OscAttentionParser::new();
        let mut bytes: Vec<u8> = b"\x1b]1337;AgentTask;Info;not-valid-b64!!".to_vec();
        bytes.push(BEL);
        let evs = p.feed(&bytes);
        assert!(evs.is_empty());
    }

    #[test]
    fn oversized_payload_emits_no_event_and_returns_to_ground() {
        let mut p = OscAttentionParser::new();
        let mut bytes: Vec<u8> = b"\x1b]1337;AgentTask;Info;".to_vec();
        bytes.extend(std::iter::repeat_n(b'A', OSC_MAX_PAYLOAD_BYTES + 50));
        bytes.push(BEL);
        let evs = p.feed(&bytes);
        assert!(evs.is_empty());
        let fresh = build_agent_marker("Error", "after-overflow", BEL);
        let evs2 = p.feed(&fresh);
        assert_eq!(evs2.len(), 1, "parser must recover after oversize");
        let m = agent_marker(&evs2).expect("agent marker");
        assert_eq!(m.severity, AttentionSeverity::Error);
    }

    #[test]
    fn bel_terminator_works() {
        let mut p = OscAttentionParser::new();
        let evs = p.feed(&build_agent_marker("Info", "x", BEL));
        assert_eq!(evs.len(), 1);
    }

    #[test]
    fn esc_backslash_terminator_works() {
        let mut p = OscAttentionParser::new();
        let evs = p.feed(&build_agent_marker("Info", "x", 0x1B));
        assert_eq!(evs.len(), 1);
    }

    #[test]
    fn non_agent_task_subcommand_passes_through_without_marker() {
        let mut p = OscAttentionParser::new();
        // OSC 1337 ; some other subcommand (e.g., iTerm2 file).
        let mut bytes: Vec<u8> = b"\x1b]1337;File=name=hi.png:".to_vec();
        bytes.push(BEL);
        let evs = p.feed(&bytes);
        assert!(evs.is_empty());
    }

    #[test]
    fn truncate_summary_appends_marker_when_truncating() {
        let s: String = "A".repeat(20);
        let t = truncate_summary(&s, 5);
        assert!(t.starts_with("AAAAA"));
        assert!(t.contains("[truncated]"));
    }

    #[test]
    fn truncate_summary_returns_input_when_short_enough() {
        assert_eq!(truncate_summary("short", 100), "short");
    }

    #[test]
    fn task_complete_osc_1339_emits_task_complete_event_for_each_severity() {
        for severity in ["info", "success", "warning", "error"] {
            let mut p = OscAttentionParser::new();
            let evs = p.feed(&build_task_complete(severity, "shipped"));
            assert_eq!(evs.len(), 1, "severity={severity}");
            assert_eq!(task_complete_summary(&evs), Some("shipped"));
        }
    }

    #[test]
    fn task_complete_osc_1339_with_unknown_severity_emits_no_event() {
        let mut p = OscAttentionParser::new();
        let evs = p.feed(&build_task_complete("critical", "shipped"));
        assert!(evs.is_empty());
    }

    #[test]
    fn task_complete_osc_1339_with_malformed_base64_emits_no_event() {
        let mut p = OscAttentionParser::new();
        let mut bytes: Vec<u8> = b"\x1b]1339;am-task-complete;success;not-valid-b64!!".to_vec();
        bytes.push(BEL);
        let evs = p.feed(&bytes);
        assert!(evs.is_empty());
    }

    #[test]
    fn task_complete_osc_1339_with_empty_summary_emits_event_with_empty_string() {
        let mut p = OscAttentionParser::new();
        // Empty base64 → empty UTF-8 decode → empty summary; spec
        // explicitly permits empty summaries.
        let mut bytes: Vec<u8> = b"\x1b]1339;am-task-complete;info;".to_vec();
        bytes.push(BEL);
        let evs = p.feed(&bytes);
        assert_eq!(evs.len(), 1);
        assert_eq!(task_complete_summary(&evs), Some(""));
    }

    #[test]
    fn task_complete_osc_1339_split_across_chunks_emits_after_terminator() {
        let mut p = OscAttentionParser::new();
        let bytes = build_task_complete("success", "shipped");
        let (a, b) = bytes.split_at(8);
        assert!(p.feed(a).is_empty());
        let evs = p.feed(b);
        assert_eq!(evs.len(), 1);
        assert_eq!(task_complete_summary(&evs), Some("shipped"));
    }

    #[test]
    fn osc_1337_and_1339_can_be_interleaved_in_single_feed() {
        let mut p = OscAttentionParser::new();
        let mut combined = build_agent_marker("Info", "agent-msg", BEL);
        combined.extend_from_slice(&build_task_complete("success", "done"));
        let evs = p.feed(&combined);
        assert_eq!(evs.len(), 2, "both events expected");
        let am = agent_marker(&evs).expect("agent marker");
        assert_eq!(am.summary.as_deref(), Some("agent-msg"));
        assert_eq!(task_complete_summary(&evs), Some("done"));
    }
}
