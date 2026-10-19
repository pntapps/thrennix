//! Byte-range segments shared between the scheduler and the workers, plus the split math.
//!
//! `cursor` (bytes claimed by the worker) and `end` (exclusive, may shrink) are atomics so the
//! scheduler and the progress publisher can read them lock-free. *Mutations* of the pair go
//! through a tiny per-segment lock, which makes "claim bytes" and "shrink end" a single atomic
//! step with respect to each other (the equivalent of a 128-bit compare-exchange): a victim can
//! never read past a split point, and a split never lands behind the victim's cursor.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

/// Split points are aligned down to this boundary.
pub const SPLIT_ALIGN: u64 = 64 * 1024;
/// Speed floor used when ranking victims (a stalled/new connection counts as 64 KiB/s).
pub const SPEED_FLOOR: f64 = 64.0 * 1024.0;

#[derive(Debug)]
pub struct Segment {
    pub start: u64,
    cursor: AtomicU64,
    end: AtomicU64,
    /// Bytes `[start, written)` have been written to the file by the writer thread.
    written: AtomicU64,
    /// The current worker has received response headers for this segment.
    pub connected: AtomicBool,
    lock: Mutex<()>,
}

impl Segment {
    pub fn new(start: u64, cursor: u64, end: u64) -> Self {
        Self {
            start,
            cursor: AtomicU64::new(cursor),
            end: AtomicU64::new(end),
            written: AtomicU64::new(cursor),
            connected: AtomicBool::new(false),
            lock: Mutex::new(()),
        }
    }

    pub fn cursor(&self) -> u64 {
        self.cursor.load(Ordering::Acquire)
    }

    pub fn end(&self) -> u64 {
        self.end.load(Ordering::Acquire)
    }

    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Acquire)
    }

    pub fn remaining(&self) -> u64 {
        self.end().saturating_sub(self.cursor())
    }

    pub fn is_done(&self) -> bool {
        self.cursor() >= self.end()
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        // The lock protects no data of its own; a poisoned lock is still usable.
        self.lock.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Claim up to `n` bytes at the cursor. Returns `(offset, taken)`; `taken < n` means the
    /// end was reached (possibly because it was shrunk by a split).
    pub fn claim(&self, n: u64) -> (u64, u64) {
        let _g = self.guard();
        let c = self.cursor.load(Ordering::Acquire);
        let e = self.end.load(Ordering::Acquire);
        let take = n.min(e.saturating_sub(c));
        self.cursor.store(c + take, Ordering::Release);
        (c, take)
    }

    /// Shrink `end` to `mid` if the cursor has not reached `mid` yet and `mid` lies inside
    /// the remaining range. Returns the old end on success.
    pub fn try_shrink(&self, mid: u64) -> Option<u64> {
        let _g = self.guard();
        let c = self.cursor.load(Ordering::Acquire);
        let e = self.end.load(Ordering::Acquire);
        if mid <= c || mid >= e {
            return None;
        }
        self.end.store(mid, Ordering::Release);
        Some(e)
    }

    /// Called by the writer once `[.., upto)` of this segment is on disk.
    pub fn mark_written(&self, upto: u64) {
        self.written.fetch_max(upto, Ordering::AcqRel);
    }

    /// Restart from `pos` (used by single-stream mode which cannot resume mid-file).
    pub fn reset_to(&self, pos: u64) {
        let _g = self.guard();
        self.cursor.store(pos, Ordering::Release);
        self.written.store(pos, Ordering::Release);
    }

    /// Set the end (single-stream mode with unknown length learns the size at EOF).
    pub fn set_end(&self, end: u64) {
        let _g = self.guard();
        self.end.store(end, Ordering::Release);
    }
}

/// Where to split a range with `cursor..end` remaining, or `None` if it is too small.
/// Requires `remaining >= 2 * min_split`; the split point is `cursor + remaining/2` aligned
/// down to 64 KiB, and both halves must stay non-empty.
pub fn split_point(cursor: u64, end: u64, min_split: u64) -> Option<u64> {
    let remaining = end.checked_sub(cursor)?;
    if remaining < min_split.saturating_mul(2).max(2) {
        return None;
    }
    let raw = cursor + remaining / 2;
    let aligned = raw - raw % SPLIT_ALIGN;
    let mid = if aligned > cursor { aligned } else { raw };
    if mid <= cursor || mid >= end {
        return None;
    }
    Some(mid)
}

/// Rank split candidates: indices of `(remaining, speed_bps)` entries that are big enough to
/// split, ordered so the segment that would finish *last* comes first.
pub fn rank_victims(cands: &[(u64, f64)], min_split: u64) -> Vec<usize> {
    let mut v: Vec<(usize, f64)> = cands
        .iter()
        .enumerate()
        .filter(|(_, (rem, _))| *rem >= min_split.saturating_mul(2).max(2))
        .map(|(i, (rem, speed))| (i, *rem as f64 / speed.max(SPEED_FLOOR)))
        .collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    v.into_iter().map(|(i, _)| i).collect()
}

/// Initial plan: split `[0,total)` into at most `pieces` ranges, none smaller than
/// `min_split` (except when the whole file is smaller), boundaries aligned to 64 KiB.
pub fn initial_plan(total: u64, pieces: usize, min_split: u64) -> Vec<(u64, u64)> {
    if total == 0 {
        return Vec::new();
    }
    let max_by_size = (total / min_split.max(1)).max(1);
    let n = (pieces.max(1) as u64).min(max_by_size);
    let mut out = Vec::with_capacity(n as usize);
    let mut start = 0;
    for i in 1..=n {
        let end = if i == n {
            total
        } else {
            let raw = total / n * i;
            let aligned = raw - raw % SPLIT_ALIGN;
            if aligned > start {
                aligned
            } else {
                raw
            }
        };
        if end > start {
            out.push((start, end));
            start = end;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn split_point_basic() {
        assert_eq!(split_point(0, 8 * MIB, 2 * MIB), Some(4 * MIB));
        // Below 2*min -> no split.
        assert_eq!(split_point(0, 4 * MIB - 1, 2 * MIB), None);
        assert_eq!(split_point(0, 4 * MIB, 2 * MIB), Some(2 * MIB));
        // Alignment rounds down to 64 KiB.
        let mid = split_point(1000, 1000 + 10 * MIB + 12345, 2 * MIB).unwrap();
        assert_eq!(mid % SPLIT_ALIGN, 0);
        assert!(mid > 1000 && mid <= 1000 + 5 * MIB + 6173);
        // Cursor past end.
        assert_eq!(split_point(10, 5, 1), None);
    }

    #[test]
    fn split_point_tiny_min() {
        // When alignment would land at/behind the cursor, use the raw midpoint.
        let mid = split_point(100, 300, 1).unwrap();
        assert_eq!(mid, 200);
    }

    #[test]
    fn victim_is_the_one_finishing_last() {
        // (remaining, speed)
        let c = [
            (10 * MIB, 10.0 * MIB as f64), // 1 s
            (4 * MIB, 0.5 * MIB as f64),   // 8 s  <- finishes last
            (20 * MIB, 5.0 * MIB as f64),  // 4 s
            (3 * MIB, 0.0),                // too small to split
        ];
        let r = rank_victims(&c, 2 * MIB);
        assert_eq!(r, vec![1, 2, 0]);
    }

    #[test]
    fn victim_speed_floor() {
        // Two stalled connections: bigger remaining wins.
        let c = [(5 * MIB, 0.0), (9 * MIB, 1.0)];
        assert_eq!(rank_victims(&c, MIB), vec![1, 0]);
    }

    #[test]
    fn shrink_and_claim() {
        let s = Segment::new(0, 0, 8 * MIB);
        let mid = split_point(s.cursor(), s.end(), 2 * MIB).unwrap();
        assert_eq!(s.try_shrink(mid), Some(8 * MIB));
        assert_eq!(s.end(), mid);
        // The worker can't claim past the new end.
        let (off, took) = s.claim(5 * MIB);
        assert_eq!((off, took), (0, mid));
        assert!(s.is_done());
        assert_eq!(s.claim(10), (mid, 0));
    }

    #[test]
    fn shrink_race_cursor_passed_split_point() {
        let s = Segment::new(0, 0, 8 * MIB);
        let mid = split_point(s.cursor(), s.end(), 2 * MIB).unwrap();
        // Worker races ahead past the planned split point before the scheduler shrinks.
        s.claim(mid + 1);
        assert_eq!(s.try_shrink(mid), None, "must not shrink behind the cursor");
        assert_eq!(s.end(), 8 * MIB);
        // Exactly at the split point is also rejected (victim's half would be empty).
        let s2 = Segment::new(0, 0, 8 * MIB);
        s2.claim(mid);
        assert_eq!(s2.try_shrink(mid), None);
    }

    #[test]
    fn concurrent_claim_and_shrink_never_overlap() {
        use std::sync::Arc;
        for _ in 0..200 {
            let s = Arc::new(Segment::new(0, 0, 1 << 20));
            let w = {
                let s = s.clone();
                std::thread::spawn(move || {
                    let mut got = 0;
                    loop {
                        let (_, t) = s.claim(997);
                        if t == 0 {
                            break got;
                        }
                        got += t;
                    }
                })
            };
            let shrunk = s.try_shrink(1 << 19);
            let got = w.join().unwrap();
            match shrunk {
                Some(_) => assert_eq!(got, 1 << 19),
                None => assert_eq!(got, 1 << 20),
            }
            assert_eq!(s.cursor(), s.end());
        }
    }

    #[test]
    fn plan_pieces() {
        let p = initial_plan(100 * MIB, 8, 2 * MIB);
        assert_eq!(p.len(), 8);
        assert_eq!(p[0].0, 0);
        assert_eq!(p.last().unwrap().1, 100 * MIB);
        for w in p.windows(2) {
            assert_eq!(w[0].1, w[1].0);
        }
        // Small file: limited by min_split.
        assert_eq!(initial_plan(5 * MIB, 8, 2 * MIB).len(), 2);
        assert_eq!(initial_plan(1, 8, 2 * MIB), vec![(0, 1)]);
        assert!(initial_plan(0, 8, 2 * MIB).is_empty());
    }
}
