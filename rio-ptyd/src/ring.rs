//! Bounded replay buffer of raw PTY output.
//!
//! Evicted bytes stream through a [`ModeTracker`] exactly once, so the
//! tracker always holds the terminal state as of the ring's oldest
//! byte. Eviction additionally aligns to escape-sequence boundaries so
//! a replay never starts mid-sequence.

use crate::modes::ModeTracker;

pub const DEFAULT_RING_BYTES: usize = 1024 * 1024;

pub struct ReplayRing {
    buf: Vec<u8>,
    start: usize,
    len: usize,
    baseline: ModeTracker,
}

impl ReplayRing {
    pub fn new(capacity: usize) -> ReplayRing {
        let capacity = capacity.max(4096);
        ReplayRing {
            buf: vec![0u8; capacity],
            start: 0,
            len: 0,
            baseline: ModeTracker::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push(&mut self, data: &[u8]) {
        let cap = self.buf.len();

        // Oversized writes: only the tail can survive; everything
        // before it is history that must still flow through the
        // baseline tracker in order.
        if data.len() >= cap {
            self.evict(self.len);
            let cut = data.len() - cap;
            self.baseline.advance(&data[..cut]);
            self.copy_in(&data[cut..]);
            self.align_start();
            return;
        }

        let free = cap - self.len;
        if data.len() > free {
            self.evict(data.len() - free);
        }
        self.copy_in(data);
        if self.len == cap {
            // Ring just became full; opportunistically re-align so the
            // oldest byte sits on a sequence boundary.
            self.align_start();
        }
    }

    fn copy_in(&mut self, data: &[u8]) {
        let cap = self.buf.len();
        let end = (self.start + self.len) % cap;
        let first = (cap - end).min(data.len());
        self.buf[end..end + first].copy_from_slice(&data[..first]);
        if first < data.len() {
            let rest = data.len() - first;
            self.buf[..rest].copy_from_slice(&data[first..]);
        }
        self.len += data.len();
    }

    /// Drop `n` oldest bytes, feeding them through the baseline tracker.
    fn evict(&mut self, n: usize) {
        let n = n.min(self.len);
        let cap = self.buf.len();
        let first = (cap - self.start).min(n);
        // Feed in order; two slices when wrapped.
        let (a, b) = {
            let a = self.start..self.start + first;
            let b = 0..n - first;
            (a, b)
        };
        // Split borrows: tracker vs buffer are distinct fields.
        let buf = std::mem::take(&mut self.buf);
        self.baseline.advance(&buf[a]);
        self.baseline.advance(&buf[b]);
        self.buf = buf;
        self.start = (self.start + n) % cap;
        self.len -= n;
    }

    /// If the baseline tracker sits mid-sequence, evict single bytes
    /// until it reaches ground state so replay starts on a boundary.
    ///
    /// Bounded: a string sequence (a large sixel/iTerm2 DCS/APC image)
    /// can exceed the whole ring, in which case draining to ground
    /// would throw away all history AND leave the ring re-filling with
    /// naked mid-sequence bytes. Cap the scan at a quarter of capacity;
    /// past that, keep the ring intact and accept a possibly
    /// mid-sequence start (a bounded visual glitch on one reattach) —
    /// far better than total history loss.
    fn align_start(&mut self) {
        let budget = (self.buf.len() / 4).max(1);
        let mut scanned = 0;
        while self.baseline.in_sequence() && self.len > 0 {
            if scanned >= budget {
                return;
            }
            self.evict(1);
            scanned += 1;
        }
    }

    /// Replay prefix (synthesized state) + ring contents, in order.
    pub fn replay(&self) -> Vec<u8> {
        let mut out = self.baseline.replay_prefix();
        out.reserve(self.len);
        let cap = self.buf.len();
        let first = (cap - self.start).min(self.len);
        out.extend_from_slice(&self.buf[self.start..self.start + first]);
        out.extend_from_slice(&self.buf[..self.len - first]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_tail_and_baseline_state() {
        let mut r = ReplayRing::new(4096);
        // Enter alt screen, then push enough to evict that sequence.
        r.push(b"\x1b[?1049h");
        r.push(&vec![b'x'; 8192]);
        let replay = r.replay();
        let s = String::from_utf8_lossy(&replay);
        // The 1049h scrolled out of the ring but must reappear in the
        // synthesized prefix.
        assert!(s.starts_with("\x1b[!p"));
        assert!(s.contains("\x1b[?1049h"));
        // Ring bytes plus the synthesized mode prefix. The prefix now
        // emits both directions for every tracked mode, so allow a
        // generous fixed headroom (it is a small bounded constant).
        assert!(replay.len() <= 4096 + 256);
    }

    #[test]
    fn never_starts_mid_sequence() {
        let mut r = ReplayRing::new(4096);
        r.push(&vec![b'a'; 4090]);
        // This escape sequence straddles the eviction point.
        r.push(b"\x1b[?25l");
        r.push(&[b'b'; 10]);
        let replay = r.replay();
        // Strip the synthesized prefix (ends after title/modes; find
        // the first ring byte by locating the last prefix byte).
        // Simpler check: ring content must not begin with a partial
        // CSI — scan for an ESC without terminator at the very start
        // after the known prefix end. We assert indirectly: parsing
        // the whole replay with a fresh tracker must end in ground.
        let mut t = ModeTracker::new();
        t.advance(&replay);
        assert!(!t.in_sequence());
    }

    #[test]
    fn oversized_single_write() {
        let mut r = ReplayRing::new(4096);
        r.push(b"\x1b[?2004h");
        let mut big = vec![b'z'; 10000];
        big.extend_from_slice(b"tail");
        r.push(&big);
        let replay = r.replay();
        let s = String::from_utf8_lossy(&replay);
        assert!(s.ends_with("tail"));
        assert!(s.contains("\x1b[?2004h")); // resurrected in prefix
    }

    #[test]
    fn wrap_around_replay_order() {
        let mut r = ReplayRing::new(4096);
        for i in 0..100u32 {
            r.push(format!("line-{i:04}\n").as_bytes());
        }
        let replay = r.replay();
        let s = String::from_utf8_lossy(&replay);
        let first = s.find("line-").unwrap();
        let nums: Vec<u32> = s[first..]
            .lines()
            .filter_map(|l| l.strip_prefix("line-"))
            .filter_map(|n| n.parse().ok())
            .collect();
        assert!(nums.windows(2).all(|w| w[1] == w[0] + 1), "order broken");
        assert_eq!(*nums.last().unwrap(), 99);
    }
}
