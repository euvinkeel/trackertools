//! Sets of frames as sorted, disjoint, half-open ranges — the currency of
//! invalidation (DESIGN §6): "frames 120..380 of this output are dirty".

// Lists of `Range`s are this module's data, not mistyped `(a..b).collect()`s.
#![allow(clippy::single_range_in_vec_init)]

use std::ops::Range;

use crate::time::FrameIndex;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RangeSet {
    /// Sorted, disjoint, non-adjacent, non-empty.
    ranges: Vec<Range<FrameIndex>>,
}

impl RangeSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_range(r: Range<FrameIndex>) -> Self {
        let mut s = Self::new();
        s.insert(r);
        s
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    pub fn ranges(&self) -> &[Range<FrameIndex>] {
        &self.ranges
    }

    pub fn len(&self) -> FrameIndex {
        self.ranges.iter().map(|r| r.end - r.start).sum()
    }

    pub fn contains(&self, f: FrameIndex) -> bool {
        let i = self.ranges.partition_point(|r| r.end <= f);
        self.ranges.get(i).is_some_and(|r| r.start <= f)
    }

    /// Smallest range covering the whole set.
    pub fn hull(&self) -> Option<Range<FrameIndex>> {
        Some(self.ranges.first()?.start..self.ranges.last()?.end)
    }

    /// Add frames, merging overlapping and adjacent ranges.
    pub fn insert(&mut self, r: Range<FrameIndex>) {
        if r.is_empty() {
            return;
        }
        let lo = self.ranges.partition_point(|x| x.end < r.start);
        let hi = self.ranges.partition_point(|x| x.start <= r.end);
        let start = if lo < hi { self.ranges[lo].start.min(r.start) } else { r.start };
        let end = if lo < hi { self.ranges[hi - 1].end.max(r.end) } else { r.end };
        self.ranges.splice(lo..hi, [start..end]);
    }

    pub fn union(&mut self, other: &RangeSet) {
        for r in &other.ranges {
            self.insert(r.clone());
        }
    }

    /// Remove frames.
    pub fn remove(&mut self, r: Range<FrameIndex>) {
        if r.is_empty() {
            return;
        }
        let mut out = Vec::with_capacity(self.ranges.len() + 1);
        for x in self.ranges.drain(..) {
            if x.end <= r.start || x.start >= r.end {
                out.push(x);
                continue;
            }
            if x.start < r.start {
                out.push(x.start..r.start);
            }
            if x.end > r.end {
                out.push(r.end..x.end);
            }
        }
        self.ranges = out;
    }

    /// Frames in both.
    pub fn intersect(&self, r: &Range<FrameIndex>) -> RangeSet {
        let ranges = self
            .ranges
            .iter()
            .filter_map(|x| {
                let (a, b) = (x.start.max(r.start), x.end.min(r.end));
                (a < b).then_some(a..b)
            })
            .collect();
        RangeSet { ranges }
    }

    /// Apply `f` to each range and collect the (clamped, merged) results.
    pub fn map(&self, f: impl Fn(Range<FrameIndex>) -> Range<FrameIndex>) -> RangeSet {
        let mut out = RangeSet::new();
        for r in &self.ranges {
            out.insert(f(r.clone()));
        }
        out
    }

    /// Take up to `budget` frames from the front (evaluation in bounded steps).
    pub fn take_front(&mut self, budget: FrameIndex) -> Option<Range<FrameIndex>> {
        let first = self.ranges.first()?.clone();
        let end = first.end.min(first.start + budget.max(1));
        self.remove(first.start..end);
        Some(first.start..end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_merges_overlaps_and_neighbours() {
        let mut s = RangeSet::new();
        s.insert(10..20);
        s.insert(30..40);
        s.insert(20..25); // adjacent to 10..20
        assert_eq!(s.ranges(), &[10..25, 30..40]);
        s.insert(24..31);
        assert_eq!(s.ranges(), &[10..40]);
        s.insert(0..2);
        assert_eq!(s.ranges(), &[0..2, 10..40]);
        assert_eq!(s.len(), 32);
        assert!(s.contains(39) && !s.contains(40) && !s.contains(5));
    }

    #[test]
    fn remove_splits() {
        let mut s = RangeSet::from_range(0..100);
        s.remove(40..60);
        assert_eq!(s.ranges(), &[0..40, 60..100]);
        s.remove(-5..10);
        assert_eq!(s.ranges(), &[10..40, 60..100]);
    }

    #[test]
    fn intersect_map_take() {
        let mut s = RangeSet::from_range(0..10);
        s.insert(20..30);
        assert_eq!(s.intersect(&(5..25)).ranges(), &[5..10, 20..25]);
        // A ±3 window footprint merges the gap-adjacent ranges.
        let w = s.map(|r| (r.start - 3)..(r.end + 3));
        assert_eq!(w.ranges(), &[-3..13, 17..33]);
        let mut t = RangeSet::from_range(0..10);
        assert_eq!(t.take_front(4), Some(0..4));
        assert_eq!(t.ranges(), &[4..10]);
    }
}
