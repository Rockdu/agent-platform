//! OSC agent marker parser: `ESC ] 1337 ; AgentTask ; <severity>;<base64-summary> <BEL or ESC \>`.
//!
//! Spec: `docs/specs/terminal-events.md` §"OSC Agent Marker Sequence".
//! Recognizes ONLY OSC 1337 sequences whose subcommand is exactly
//! `AgentTask`. Stateful — partial sequences resume across feeds.
//! Maximum payload size is 4 KB; oversize → no event + reset.

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

pub struct OscAgentMarkerParser {
    state: State,
    payload: Vec<u8>,
    max_payload: usize,
}

impl Default for OscAgentMarkerParser {
    fn default() -> Self {
        Self::new()
    }
}

impl OscAgentMarkerParser {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            payload: Vec::with_capacity(256),
            max_payload: OSC_MAX_PAYLOAD_BYTES,
        }
    }

    /// Feed `bytes` and emit any complete `AgentMarkerEvent`s found.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<AgentMarkerEvent> {
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
                    } else {
                        if self.payload.len() >= self.max_payload {
                            // Oversize → drop the event silently and
                            // reset state per spec.
                            self.reset();
                        } else {
                            self.payload.push(b);
                        }
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

    fn try_parse_payload(&self) -> Option<AgentMarkerEvent> {
        // Expected payload after `ESC ]` prefix:
        //   1337;AgentTask;<severity>;<base64-summary>
        let payload = std::str::from_utf8(&self.payload).ok()?;
        let mut parts = payload.splitn(4, ';');
        let id = parts.next()?;
        let cmd = parts.next()?;
        let sev_str = parts.next()?;
        let b64 = parts.next()?;
        if id != "1337" || cmd != "AgentTask" {
            return None;
        }
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

    fn build_marker(severity: &str, summary: &str, terminator: u8) -> Vec<u8> {
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

    #[test]
    fn complete_marker_in_single_feed_emits_event() {
        let mut p = OscAgentMarkerParser::new();
        let bytes = build_marker("Info", "hello", BEL);
        let evs = p.feed(&bytes);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].severity, AttentionSeverity::Info);
        assert_eq!(evs[0].summary.as_deref(), Some("hello"));
    }

    #[test]
    fn marker_split_across_two_feeds_emits_event_after_terminator() {
        let mut p = OscAgentMarkerParser::new();
        let bytes = build_marker("NeedsConfirm", "中文摘要", BEL);
        let (a, b) = bytes.split_at(10);
        assert!(p.feed(a).is_empty(), "no event before terminator");
        let evs = p.feed(b);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].severity, AttentionSeverity::NeedsConfirm);
        assert_eq!(evs[0].summary.as_deref(), Some("中文摘要"));
    }

    #[test]
    fn malformed_severity_emits_no_event() {
        let mut p = OscAgentMarkerParser::new();
        let bytes = build_marker("Critical", "hello", BEL);
        let evs = p.feed(&bytes);
        assert!(evs.is_empty());
    }

    #[test]
    fn malformed_base64_emits_no_event() {
        let mut p = OscAgentMarkerParser::new();
        let mut bytes: Vec<u8> = b"\x1b]1337;AgentTask;Info;not-valid-b64!!".to_vec();
        bytes.push(BEL);
        let evs = p.feed(&bytes);
        assert!(evs.is_empty());
    }

    #[test]
    fn oversized_payload_emits_no_event_and_returns_to_ground() {
        let mut p = OscAgentMarkerParser::new();
        // Build a payload bigger than 4 KB inside the OSC string.
        let mut bytes: Vec<u8> = b"\x1b]1337;AgentTask;Info;".to_vec();
        bytes.extend(std::iter::repeat_n(b'A', OSC_MAX_PAYLOAD_BYTES + 50));
        bytes.push(BEL);
        // Feed everything; expect no event AND the parser back at
        // Ground so a fresh marker right after would parse.
        let evs = p.feed(&bytes);
        assert!(evs.is_empty());
        let fresh = build_marker("Error", "after-overflow", BEL);
        let evs2 = p.feed(&fresh);
        assert_eq!(evs2.len(), 1, "parser must recover after oversize");
        assert_eq!(evs2[0].severity, AttentionSeverity::Error);
    }

    #[test]
    fn bel_terminator_works() {
        let mut p = OscAgentMarkerParser::new();
        let evs = p.feed(&build_marker("Info", "x", BEL));
        assert_eq!(evs.len(), 1);
    }

    #[test]
    fn esc_backslash_terminator_works() {
        let mut p = OscAgentMarkerParser::new();
        let evs = p.feed(&build_marker("Info", "x", 0x1B));
        assert_eq!(evs.len(), 1);
    }

    #[test]
    fn non_agent_task_subcommand_passes_through_without_marker() {
        let mut p = OscAgentMarkerParser::new();
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
}
