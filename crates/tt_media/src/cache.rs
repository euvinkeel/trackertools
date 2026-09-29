//! Decoded-frame cache (DESIGN §13 step 4). NV12 frames keyed by presented
//! index. Eviction drops the frames *farthest from the playhead* first — for
//! stepping and scrubbing that beats LRU, which would evict the frames just
//! behind the playhead that a backward step needs next.

use std::collections::BTreeMap;
use std::sync::Arc;

pub type FrameData = Arc<[u8]>;

pub struct FrameCache {
    frames: BTreeMap<usize, FrameData>,
    bytes: usize,
    budget: usize,
}

impl FrameCache {
    pub fn new(budget_bytes: usize) -> Self {
        Self { frames: BTreeMap::new(), bytes: 0, budget: budget_bytes }
    }

    pub fn contains(&self, p: usize) -> bool {
        self.frames.contains_key(&p)
    }

    pub fn get(&self, p: usize) -> Option<FrameData> {
        self.frames.get(&p).cloned()
    }

    /// The cached frame nearest to `p` (ties prefer the earlier frame).
    pub fn nearest(&self, p: usize) -> Option<(usize, FrameData)> {
        let below = self.frames.range(..=p).next_back();
        let above = self.frames.range(p..).next();
        match (below, above) {
            (Some(b), Some(a)) => Some(if p - b.0 <= a.0 - p { (*b.0, b.1.clone()) } else { (*a.0, a.1.clone()) }),
            (Some(b), None) => Some((*b.0, b.1.clone())),
            (None, Some(a)) => Some((*a.0, a.1.clone())),
            (None, None) => None,
        }
    }

    /// Insert a frame, then evict the frames farthest from `playhead` until
    /// the cache fits its budget again.
    pub fn insert(&mut self, p: usize, data: FrameData, playhead: usize) {
        self.bytes += data.len();
        if let Some(old) = self.frames.insert(p, data) {
            self.bytes -= old.len();
        }
        while self.bytes > self.budget && self.frames.len() > 1 {
            let first = *self.frames.keys().next().unwrap();
            let last = *self.frames.keys().next_back().unwrap();
            let victim = if playhead.abs_diff(first) >= playhead.abs_diff(last) { first } else { last };
            if let Some(old) = self.frames.remove(&victim) {
                self.bytes -= old.len();
            }
        }
    }

    /// Cached frames as inclusive runs `(first, last)`, for display.
    pub fn ranges(&self) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = Vec::new();
        for &p in self.frames.keys() {
            match out.last_mut() {
                Some((_, last)) if *last + 1 == p => *last = p,
                _ => out.push((p, p)),
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> FrameData {
        Arc::from(vec![0u8; 10])
    }

    #[test]
    fn evicts_farthest_from_playhead() {
        let mut c = FrameCache::new(30);
        c.insert(10, frame(), 12);
        c.insert(11, frame(), 12);
        c.insert(12, frame(), 12);
        c.insert(20, frame(), 12); // over budget: 20 is farthest
        assert!(!c.contains(20) && c.contains(10));
        c.insert(13, frame(), 12); // now 10 is farthest
        assert!(!c.contains(10) && c.contains(13));
        assert_eq!(c.bytes(), 30);
    }

    #[test]
    fn nearest_prefers_earlier_on_ties() {
        let mut c = FrameCache::new(1000);
        c.insert(10, frame(), 0);
        c.insert(14, frame(), 0);
        assert_eq!(c.nearest(12).unwrap().0, 10);
        assert_eq!(c.nearest(13).unwrap().0, 14);
        assert_eq!(c.nearest(99).unwrap().0, 14);
    }
}
