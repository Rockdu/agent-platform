//! Minimal ANSI parser state machine + UTF-8 boundary tracker.
//!
//! Spec: `docs/specs/terminal-events.md` §"UTF-8 / ANSI Boundary
//! Safety". A safe boundary is a byte index that is BOTH outside any
//! ANSI/OSC/CSI sequence AND between UTF-8 code points. The scanner
//! must resume across reads (no implicit reset).

const ESC: u8 = 0x1B;
const BEL: u8 = 0x07;
const LBRACKET: u8 = b'[';
const RBRACKET: u8 = b']';
const BACKSLASH: u8 = b'\\';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnsiState {
    Ground,
    Escape,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiFinal,
    OscEntry,
    OscString,
    OscEsc,
}

/// Returns true if `b` is the first byte of a new UTF-8 code point
/// (i.e. it is not a continuation byte). Continuation bytes have the
/// form `10xxxxxx` per spec.
#[inline]
pub fn is_utf8_boundary(b: u8) -> bool {
    b & 0xC0 != 0x80
}

#[derive(Debug, Clone)]
pub struct AnsiScanner {
    state: AnsiState,
    /// Bytes consumed since `new()`. The "logical offset" of the next
    /// byte to be fed; used by the ring buffer to track safe-boundary
    /// indices that survive overflow eviction.
    fed_total: u64,
}

impl Default for AnsiScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl AnsiScanner {
    pub fn new() -> Self {
        Self {
            state: AnsiState::Ground,
            fed_total: 0,
        }
    }

    pub fn state(&self) -> AnsiState {
        self.state
    }

    pub fn fed_total(&self) -> u64 {
        self.fed_total
    }

    /// Feed `bytes` through the state machine and report logical
    /// safe-boundary offsets observed AFTER each byte (i.e. the offset
    /// of the byte that would follow it). Sink the callback with
    /// `|offset| ...`. The scanner's state survives across calls.
    pub fn feed<F: FnMut(u64)>(&mut self, bytes: &[u8], mut on_safe: F) {
        for &b in bytes {
            self.step(b);
            self.fed_total += 1;
            // After this byte, the next byte's logical offset equals
            // self.fed_total. It is a "safe boundary" iff:
            //  - parser is now in Ground, AND
            //  - the next byte (unknown to us yet) starts a new UTF-8
            //    code point. We can only guarantee Ground here; the
            //    UTF-8 check happens against the NEXT byte when it
            //    arrives. So we emit the offset as a Ground-safe
            //    candidate; the ring buffer will reconcile it against
            //    `is_utf8_boundary` of the actual byte at that offset.
            if self.state == AnsiState::Ground {
                on_safe(self.fed_total);
            }
        }
    }

    fn step(&mut self, b: u8) {
        use AnsiState::*;
        self.state = match self.state {
            Ground => {
                if b == ESC {
                    Escape
                } else {
                    Ground
                }
            }
            Escape => match b {
                LBRACKET => CsiEntry,
                RBRACKET => OscEntry,
                ESC => Escape,
                // Any other final byte after ESC concludes a 2-byte
                // escape (e.g. ESC c, ESC =). Return to Ground.
                _ => Ground,
            },
            CsiEntry | CsiParam => {
                if (0x30..=0x3F).contains(&b) {
                    CsiParam
                } else if (0x20..=0x2F).contains(&b) {
                    CsiIntermediate
                } else if (0x40..=0x7E).contains(&b) {
                    // CSI final byte; sequence complete.
                    CsiFinal
                } else if b == ESC {
                    // Aborted by another ESC; restart.
                    Escape
                } else {
                    // Garbage inside a CSI — keep parsing.
                    CsiParam
                }
            }
            CsiIntermediate => {
                if (0x20..=0x2F).contains(&b) {
                    CsiIntermediate
                } else if (0x40..=0x7E).contains(&b) {
                    CsiFinal
                } else if b == ESC {
                    Escape
                } else {
                    CsiIntermediate
                }
            }
            CsiFinal => {
                // CsiFinal is a one-byte transient; the previous byte
                // closed the CSI. Reclassify `b` from Ground.
                if b == ESC {
                    Escape
                } else {
                    Ground
                }
            }
            OscEntry | OscString => {
                if b == BEL {
                    Ground
                } else if b == ESC {
                    OscEsc
                } else {
                    OscString
                }
            }
            OscEsc => {
                if b == BACKSLASH {
                    // ESC \ string terminator.
                    Ground
                } else {
                    // Lone ESC inside OSC; treat as restart of escape.
                    Escape
                }
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_collect(bytes: &[u8]) -> (AnsiScanner, Vec<u64>) {
        let mut s = AnsiScanner::new();
        let mut safe = Vec::new();
        s.feed(bytes, |o| safe.push(o));
        (s, safe)
    }

    #[test]
    fn ground_bytes_each_produce_a_safe_offset() {
        let (s, safe) = feed_collect(b"abc");
        assert_eq!(s.state(), AnsiState::Ground);
        assert_eq!(safe, vec![1, 2, 3]);
    }

    #[test]
    fn csi_sequence_suppresses_safe_offsets_until_complete() {
        // ESC [ 3 1 m  → CSI SGR red. Final byte 'm' transitions to
        // CsiFinal, then the next byte (if any) reclassifies to
        // Ground. After only the CSI itself, the final 'm' itself
        // does NOT register as a safe offset because step() leaves
        // state in CsiFinal AFTER the byte.
        let (s, safe) = feed_collect(b"\x1b[31m");
        assert_eq!(s.state(), AnsiState::CsiFinal);
        assert!(safe.is_empty(), "no safe offsets while CSI is active");
        // Now feed one more Ground byte ("X"); the scanner reclassifies
        // CsiFinal → Ground and the X registers as safe.
        let mut s = s;
        let mut safe2 = Vec::new();
        s.feed(b"X", |o| safe2.push(o));
        assert_eq!(s.state(), AnsiState::Ground);
        assert_eq!(safe2, vec![6]);
    }

    #[test]
    fn osc_string_terminated_by_bel_returns_to_ground() {
        // ESC ] 1 3 3 7 ; A g e n t T a s k ; ... BEL
        let mut input: Vec<u8> = b"\x1b]1337;AgentTask;Info;aGVsbG8=".to_vec();
        input.push(0x07);
        let (s, safe) = feed_collect(&input);
        assert_eq!(s.state(), AnsiState::Ground);
        // The BEL itself was the terminator; it took us to Ground,
        // so the BEL byte's "after" offset IS a safe boundary.
        assert_eq!(*safe.last().unwrap(), input.len() as u64);
    }

    #[test]
    fn osc_string_terminated_by_esc_backslash_returns_to_ground() {
        let mut input: Vec<u8> = b"\x1b]1337;AgentTask;Info;aGk=".to_vec();
        input.push(0x1B);
        input.push(b'\\');
        let (s, safe) = feed_collect(&input);
        assert_eq!(s.state(), AnsiState::Ground);
        assert_eq!(*safe.last().unwrap(), input.len() as u64);
    }

    #[test]
    fn parser_state_resumes_across_feeds() {
        let mut s = AnsiScanner::new();
        s.feed(b"\x1b[", |_| {});
        assert_eq!(s.state(), AnsiState::CsiEntry);
        s.feed(b"3", |_| {});
        assert_eq!(s.state(), AnsiState::CsiParam);
        s.feed(b"1m", |_| {});
        assert_eq!(s.state(), AnsiState::CsiFinal);
        s.feed(b"X", |_| {});
        assert_eq!(s.state(), AnsiState::Ground);
    }

    #[test]
    fn is_utf8_boundary_recognizes_continuation_bytes() {
        // "你" = 0xE4 0xBD 0xA0 — lead 0xE4, continuations 0xBD/0xA0.
        assert!(is_utf8_boundary(0xE4));
        assert!(!is_utf8_boundary(0xBD));
        assert!(!is_utf8_boundary(0xA0));
        assert!(is_utf8_boundary(b'a'));
    }
}
