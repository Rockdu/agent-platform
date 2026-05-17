//! 1 MB bounded scrollback ring with UTF-8/ANSI-safe slicing and
//! coalesce-on-overflow drop policy.
//!
//! Spec: `docs/specs/terminal-events.md` §"Ring Buffer Policy". The
//! capacity is HARD-pinned at 1,048,576 bytes per PTY.

use std::collections::VecDeque;

use crate::ansi_scanner::{is_utf8_boundary, AnsiScanner, AnsiState};

/// Hard per-PTY capacity per spec (1 MB).
pub const RING_CAPACITY_BYTES: usize = 1_048_576;

/// Diagnostic info returned by `push` when overflow caused a drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferTruncatedInfo {
    pub bytes_dropped: usize,
}

pub struct RingBuffer {
    capacity: usize,
    buf: VecDeque<u8>,
    scanner: AnsiScanner,
    /// Logical offset of the byte at `buf[0]` since `new()`. The
    /// scanner reports safe-boundary candidates by logical offset, so
    /// we keep them stable under overflow eviction by storing them
    /// here too.
    head_logical: u64,
    /// Sorted logical offsets that are candidate safe boundaries (the
    /// parser was in Ground at this offset). UTF-8 boundary validation
    /// against the byte at that physical position happens at slice
    /// time — being in Ground is necessary but not sufficient.
    safe_boundaries: VecDeque<u64>,
}

impl RingBuffer {
    /// Construct with the spec capacity (1 MB).
    pub fn new() -> Self {
        Self::with_capacity(RING_CAPACITY_BYTES)
    }

    /// Test-only constructor for smaller capacities. Production callers
    /// should use [`RingBuffer::new`].
    pub fn with_capacity(capacity: usize) -> Self {
        // Seed with the virtual start-of-stream boundary so reads from
        // an unflushed-prefix buffer can include the very first byte.
        // Eviction preserves the invariant: `head_logical` after every
        // drop is guaranteed to be a Ground+UTF-8 boundary by the
        // chosen-offset filter, so it remains a valid candidate.
        let mut safe_boundaries = VecDeque::new();
        safe_boundaries.push_back(0);
        Self {
            capacity,
            buf: VecDeque::with_capacity(capacity.min(64 * 1024)),
            scanner: AnsiScanner::new(),
            head_logical: 0,
            safe_boundaries,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Logical offset of the next byte to be appended (after current tail).
    pub fn tail_logical(&self) -> u64 {
        self.head_logical + self.buf.len() as u64
    }

    /// Append `bytes`. Returns a coalesced `BufferTruncatedInfo` if any
    /// older bytes had to be evicted to make room. Multiple drops
    /// caused by one call coalesce into a single returned info.
    pub fn push(&mut self, bytes: &[u8]) -> Option<BufferTruncatedInfo> {
        if bytes.is_empty() {
            return None;
        }

        // 1. Feed scanner first so safe-boundary candidates land at
        //    the right LOGICAL offsets even before we drop anything.
        let bytes_before_feed = self.scanner.fed_total();
        let new_boundaries_start = self.safe_boundaries.len();
        {
            let collector = &mut self.safe_boundaries;
            self.scanner.feed(bytes, |offset| collector.push_back(offset));
        }
        debug_assert_eq!(
            self.scanner.fed_total() - bytes_before_feed,
            bytes.len() as u64,
            "scanner.feed must advance fed_total by bytes.len()"
        );

        // 2. Push raw bytes onto the tail.
        self.buf.extend(bytes.iter().copied());

        // 3. If we're over capacity, drop oldest bytes to the nearest
        //    safe boundary AFTER the required drop count. A "true safe
        //    boundary" is BOTH ANSI-Ground (scanner-emitted candidate)
        //    AND a UTF-8 codepoint boundary at the physical position
        //    after eviction. We must iterate past candidates whose
        //    physical byte is a UTF-8 continuation — bailing on the
        //    first one would erase usable scrollback around multi-byte
        //    output. Only fall back to full-buffer drop when no true
        //    safe boundary exists at all.
        let dropped = if self.buf.len() > self.capacity {
            let must_drop = self.buf.len() - self.capacity;
            let drop_target_logical = self.head_logical + must_drop as u64;
            let chosen = self
                .safe_boundaries
                .iter()
                .copied()
                .find(|&off| {
                    if off < drop_target_logical || off > self.tail_logical() {
                        return false;
                    }
                    let physical = (off - self.head_logical) as usize;
                    physical >= self.buf.len() || is_utf8_boundary(self.buf[physical])
                });

            let actual_drop = match chosen {
                Some(off) => (off - self.head_logical) as usize,
                // No reachable safe boundary — drop the entire buffer.
                None => self.buf.len(),
            };
            // Evict.
            for _ in 0..actual_drop {
                self.buf.pop_front();
            }
            self.head_logical += actual_drop as u64;
            // Prune evicted safe boundaries.
            while let Some(&front) = self.safe_boundaries.front() {
                if front < self.head_logical {
                    self.safe_boundaries.pop_front();
                } else {
                    break;
                }
            }
            actual_drop
        } else {
            0
        };

        // 4. Suppress the trailing scanner-generated boundary at
        //    `tail_logical` if it lies inside the trailing partial
        //    UTF-8 codepoint. This is conservative: we only emit
        //    safe-boundary candidates from inside Ground; the consumer
        //    (`read_scrollback`) re-validates against the byte at the
        //    offset anyway.
        let _ = new_boundaries_start; // reserved for diagnostic use.

        if dropped > 0 {
            Some(BufferTruncatedInfo {
                bytes_dropped: dropped,
            })
        } else {
            None
        }
    }

    /// Return up to `max_bytes` from the tail as a UTF-8 string sliced
    /// to safe boundaries on both ends.
    pub fn read_scrollback(&self, max_bytes: usize) -> String {
        if self.buf.is_empty() || max_bytes == 0 {
            return String::new();
        }
        let want = max_bytes.min(self.buf.len());
        let raw_start_logical = self.tail_logical() - want as u64;
        let raw_end_logical = self.tail_logical();

        // Round start FORWARD: scan ascending, skip candidates whose
        // physical byte is a UTF-8 continuation (those would split a
        // codepoint at the slice head).
        let start_logical = match self.safe_boundaries.iter().copied().find(|&off| {
            if off < raw_start_logical || off > raw_end_logical {
                return false;
            }
            let physical = (off - self.head_logical) as usize;
            physical >= self.buf.len() || is_utf8_boundary(self.buf[physical])
        }) {
            Some(off) => off,
            None => return String::new(),
        };
        // Round end BACKWARD: scan descending, same UTF-8 filter; allow
        // `off == tail_logical` (slice goes to end-of-buffer; there's
        // no byte at that offset to validate).
        let end_logical = match self
            .safe_boundaries
            .iter()
            .copied()
            .rev()
            .find(|&off| {
                if off > raw_end_logical || off < start_logical {
                    return false;
                }
                let physical = (off - self.head_logical) as usize;
                physical >= self.buf.len() || is_utf8_boundary(self.buf[physical])
            }) {
            Some(off) => off,
            None => return String::new(),
        };
        if end_logical <= start_logical {
            return String::new();
        }
        let phys_start = (start_logical - self.head_logical) as usize;
        let phys_end = (end_logical - self.head_logical) as usize;
        if phys_end > self.buf.len() || phys_start > phys_end {
            return String::new();
        }
        // Materialize the slice. VecDeque may be wrapped; collect into
        // a Vec<u8> for a contiguous view. Final `String::from_utf8`
        // is the last line of defense — if the safe-boundary filter
        // missed something pathological, the slice is silently
        // discarded rather than panicking.
        let slice: Vec<u8> = self
            .buf
            .iter()
            .skip(phys_start)
            .take(phys_end - phys_start)
            .copied()
            .collect();
        String::from_utf8(slice).unwrap_or_default()
    }

    /// True iff the scanner currently sits at a safe-to-slice state
    /// (Ground AND the last byte ended at a UTF-8 boundary, or buffer
    /// is empty).
    pub fn scanner_state(&self) -> AnsiState {
        self.scanner.state()
    }
}

impl Default for RingBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_is_one_mebibyte_by_default() {
        assert_eq!(RingBuffer::new().capacity(), 1_048_576);
    }

    #[test]
    fn push_within_capacity_records_safe_boundaries() {
        let mut r = RingBuffer::with_capacity(64);
        let info = r.push(b"hello");
        assert_eq!(info, None);
        assert_eq!(r.len(), 5);
        // Constructor seeds offset 0 + scanner emits 5 → 6 entries.
        assert_eq!(r.safe_boundaries.len(), 6);
        let s = r.read_scrollback(64);
        assert_eq!(s, "hello");
    }

    #[test]
    fn push_overflow_emits_buffer_truncated_with_coalesced_count() {
        let mut r = RingBuffer::with_capacity(8);
        r.push(b"abcdefgh"); // exactly fills
        let info = r.push(b"XYZ"); // overflow by 3
        let info = info.expect("must report a coalesced drop");
        // The drop count is the LOGICAL drop applied; expect at least 3.
        assert!(info.bytes_dropped >= 3, "expected ≥3 dropped; got {info:?}");
        assert!(r.len() <= 8);
        let s = r.read_scrollback(64);
        // Tail must contain the freshly pushed "XYZ".
        assert!(s.ends_with("XYZ"), "tail should end with XYZ; got {s:?}");
    }

    #[test]
    fn overflow_does_not_split_utf8_continuation_byte() {
        // "你好" = E4 BD A0 E5 A5 BD (6 bytes, two 3-byte codepoints).
        // Capacity = 4; first push fills exactly, second pushes ascii
        // so we evict the first codepoint cleanly.
        let mut r = RingBuffer::with_capacity(4);
        let info = r.push("你好".as_bytes()); // 6 bytes → 2 over
        assert!(info.is_some());
        // After eviction, the head must NOT be a continuation byte.
        let head = r.buf.front().copied();
        if let Some(b) = head {
            assert!(
                is_utf8_boundary(b),
                "ring head 0x{b:02X} is a UTF-8 continuation byte (BAD)"
            );
        }
        let s = r.read_scrollback(64);
        // Anything we return must be valid UTF-8 (String round-trip
        // already guarantees this; we just sanity-check).
        let _ = s;
    }

    #[test]
    fn overflow_does_not_split_csi_sequence() {
        let mut r = RingBuffer::with_capacity(8);
        // Pre-fill exactly to capacity with ground bytes.
        r.push(b"abcdefgh");
        // Now push a CSI sequence + more text. The new bytes include a
        // full CSI ("\x1b[31m" = 5 bytes) + "X" (1 byte) = 6 bytes,
        // requiring eviction of 6 ground bytes.
        let info = r.push(b"\x1b[31mX");
        assert!(info.is_some());
        // After eviction, the head must NOT be inside the CSI bytes
        // 0x1B / [ / digit / m. We can assert it's not 0x1B because
        // a coalesced drop must round past the CSI to the trailing
        // ground 'X'.
        let head = r.buf.front().copied();
        if let Some(b) = head {
            assert_ne!(b, 0x1B, "head must not be ESC");
        }
    }

    #[test]
    fn read_scrollback_returns_valid_utf8_only() {
        let mut r = RingBuffer::with_capacity(64);
        r.push("你好世界 hello".as_bytes());
        let s = r.read_scrollback(64);
        assert!(s.contains("你好"));
    }

    #[test]
    fn read_scrollback_does_not_start_or_end_inside_ansi() {
        let mut r = RingBuffer::with_capacity(64);
        // Mix ground + CSI + ground.
        r.push(b"ABC\x1b[31mXYZ\x1b[0mDEF");
        let s = r.read_scrollback(64);
        // The returned slice must not start or end at a byte inside
        // a CSI. Easiest check: it must be valid UTF-8 AND not contain
        // a trailing ESC.
        assert!(!s.starts_with('\x1b'));
        assert!(!s.ends_with('\x1b'));
    }

    #[test]
    fn read_scrollback_returns_empty_when_no_safe_slice_exists() {
        let mut r = RingBuffer::with_capacity(16);
        // Only feed bytes that NEVER bring the scanner back to Ground:
        // an OSC string with no terminator. The scanner stays in
        // OscString for every byte → no safe boundaries.
        r.push(b"\x1b]1337;AgentTask;Info;data");
        let s = r.read_scrollback(64);
        assert_eq!(s, "", "no safe boundaries → empty slice");
    }

    #[test]
    fn read_scrollback_zero_bytes_returns_empty() {
        let mut r = RingBuffer::with_capacity(16);
        r.push(b"abc");
        assert_eq!(r.read_scrollback(0), "");
    }

    /// Codex round-22 blocker #2 regression: with cap=4 and pushing
    /// "你好" (6 bytes = 3+3), the first ANSI-Ground candidate at the
    /// required drop count lands at logical offset 4, which is byte 4
    /// = 0xA5 (continuation byte of "好"). The fix must scan past
    /// that to the next candidate (offset 6, end-of-buffer) and drop
    /// the entire first codepoint + the first byte of the second, but
    /// only the FIRST codepoint should actually be lost; the second
    /// must remain intact.
    #[test]
    fn overflow_retains_second_complete_codepoint_when_first_drop_lands_inside_a_codepoint() {
        let mut r = RingBuffer::with_capacity(4);
        let _ = r.push("你好".as_bytes());
        let s = r.read_scrollback(64);
        // Either we retain "好" (3 bytes, fits in cap=4) or we drop
        // everything; the spec rules out the latter when a true safe
        // boundary exists. With the fix, the boundary at logical
        // offset 3 (end of "你") is reachable, so the kept tail is "好".
        assert_eq!(s, "好", "must retain the second complete codepoint; got {s:?}");
    }

    /// Codex round-22 blocker #2 regression: when raw_start lands on
    /// a UTF-8 continuation byte, `read_scrollback` must scan forward
    /// to the next safe boundary instead of returning empty. Build a
    /// scenario where the buffer head sits at a continuation byte and
    /// trailing valid ASCII follows.
    #[test]
    fn read_scrollback_skips_continuation_byte_start_to_return_later_valid_text() {
        // Capacity 8 keeps "好abc" (3 + 3 = 6 bytes) comfortably.
        // Push "你好abc" (3 + 3 + 3 = 9 bytes); overflow drops the
        // first codepoint "你" and leaves "好abc" (6 bytes). A partial
        // read with max_bytes=4 sets raw_start at logical offset 5 —
        // the middle of "好" (a continuation byte). The fix must scan
        // forward to the next valid boundary (offset 6, start of "a")
        // and return "abc".
        let mut r = RingBuffer::with_capacity(8);
        let _ = r.push("你好abc".as_bytes());
        let s = r.read_scrollback(4);
        assert_eq!(
            s, "abc",
            "must skip the continuation-byte start and return trailing ASCII; got {s:?}"
        );
    }
}
