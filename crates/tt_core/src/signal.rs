//! Signals (DESIGN §5): per-frame channel data — positions, sizes, view
//! transforms, flags — kept outside the ECS archetypes in the [`SignalStore`]
//! resource. Components hold [`SignalId`] handles.
//!
//! A signal is a set of `f32` channels over the media's frame grid, stored in
//! [`CHUNK`]-frame chunks shared by `Arc` and copied on write, so an undo
//! snapshot is a map of chunk pointers (cheap) and workers can read chunks
//! without copying. Every frame has a [`FrameState`]: absent, valid, or stale
//! (an input changed; the old value stays displayable until recomputed).

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::sync::Arc;

use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;
use serde::{Deserialize, Serialize};

use crate::time::FrameIndex;

pub const CHUNK: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Reflect, Serialize, Deserialize)]
pub struct SignalId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum FrameState {
    #[default]
    Absent = 0,
    Valid = 1,
    /// An input changed; the value is kept for display until recomputed.
    Stale = 2,
}

#[derive(Clone)]
struct Chunk {
    data: Box<[f32]>,
    state: Box<[FrameState; CHUNK]>,
}

impl Chunk {
    fn new(channels: usize) -> Self {
        Self { data: vec![f32::NAN; CHUNK * channels].into_boxed_slice(), state: Box::new([FrameState::Absent; CHUNK]) }
    }

    fn is_empty(&self) -> bool {
        self.state.iter().all(|s| *s == FrameState::Absent)
    }
}

/// One signal's data. Cloning is cheap (chunk pointers), which is what undo
/// snapshots store.
#[derive(Clone)]
pub struct Signal {
    channels: usize,
    chunks: BTreeMap<i64, Arc<Chunk>>,
    /// Bumped by every change (for caches and change detection).
    version: u64,
}

fn split(f: FrameIndex) -> (i64, usize) {
    (f.div_euclid(CHUNK as i64), f.rem_euclid(CHUNK as i64) as usize)
}

impl Signal {
    pub fn new(channels: usize) -> Self {
        assert!(channels > 0);
        Self { channels, chunks: BTreeMap::new(), version: 0 }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn state(&self, f: FrameIndex) -> FrameState {
        let (c, i) = split(f);
        self.chunks.get(&c).map_or(FrameState::Absent, |ch| ch.state[i])
    }

    /// Values on frame `f` if present (valid or stale).
    pub fn get(&self, f: FrameIndex) -> Option<&[f32]> {
        let (c, i) = split(f);
        let ch = self.chunks.get(&c)?;
        (ch.state[i] != FrameState::Absent).then(|| &ch.data[i * self.channels..(i + 1) * self.channels])
    }

    /// Values on frame `f` only if valid.
    pub fn get_valid(&self, f: FrameIndex) -> Option<&[f32]> {
        (self.state(f) == FrameState::Valid).then(|| self.get(f)).flatten()
    }

    fn chunk_mut(&mut self, c: i64) -> &mut Chunk {
        let channels = self.channels;
        Arc::make_mut(self.chunks.entry(c).or_insert_with(|| Arc::new(Chunk::new(channels))))
    }

    /// Set frame `f` to `values` (valid).
    pub fn set(&mut self, f: FrameIndex, values: &[f32]) {
        assert_eq!(values.len(), self.channels, "channel count");
        let (c, i) = split(f);
        let n = self.channels;
        let ch = self.chunk_mut(c);
        ch.data[i * n..(i + 1) * n].copy_from_slice(values);
        ch.state[i] = FrameState::Valid;
        self.version += 1;
    }

    /// Write consecutive frames from `start`: `values.len()` must be a multiple of the channel count.
    pub fn write(&mut self, start: FrameIndex, values: &[f32]) {
        let n = self.channels;
        assert_eq!(values.len() % n, 0, "channel count");
        for (k, frame) in values.chunks_exact(n).enumerate() {
            let (c, i) = split(start + k as i64);
            let ch = self.chunk_mut(c);
            ch.data[i * n..(i + 1) * n].copy_from_slice(frame);
            ch.state[i] = FrameState::Valid;
        }
        self.version += 1;
    }

    /// Remove frames in `range`; chunks that become empty are dropped.
    pub fn clear(&mut self, range: Range<FrameIndex>) {
        self.for_each_present(range, |ch, i, _| ch.state[i] = FrameState::Absent);
        self.chunks.retain(|_, ch| !ch.is_empty());
        self.version += 1;
    }

    /// Mark present frames in `range` stale (inputs changed; recompute pending).
    pub fn mark_stale(&mut self, range: Range<FrameIndex>) {
        self.for_each_present(range, |ch, i, _| {
            if ch.state[i] == FrameState::Valid {
                ch.state[i] = FrameState::Stale;
            }
        });
        self.version += 1;
    }

    /// Visit present frames in `range` mutably (touching only chunks that exist).
    fn for_each_present(&mut self, range: Range<FrameIndex>, mut f: impl FnMut(&mut Chunk, usize, FrameIndex)) {
        if range.is_empty() {
            return;
        }
        let (c0, _) = split(range.start);
        let (c1, _) = split(range.end - 1);
        let keys: Vec<i64> = self.chunks.range(c0..=c1).map(|(k, _)| *k).collect();
        for c in keys {
            let base = c * CHUNK as i64;
            let lo = (range.start - base).max(0) as usize;
            let hi = ((range.end - base).min(CHUNK as i64)) as usize;
            let needs = {
                let ch = &self.chunks[&c];
                ch.state[lo..hi].iter().any(|s| *s != FrameState::Absent)
            };
            if !needs {
                continue; // don't clone a shared chunk we won't change
            }
            let ch = self.chunk_mut(c);
            for i in lo..hi {
                if ch.state[i] != FrameState::Absent {
                    f(ch, i, base + i as i64);
                }
            }
        }
    }

    /// The first and last present frames (O(chunk size): chunks are ordered and never empty).
    pub fn present_hull(&self) -> Option<(FrameIndex, FrameIndex)> {
        let (first_c, first) = self.chunks.iter().next()?;
        let (last_c, last) = self.chunks.iter().next_back()?;
        let lo = first.state.iter().position(|s| *s != FrameState::Absent)?;
        let hi = last.state.iter().rposition(|s| *s != FrameState::Absent)?;
        Some((first_c * CHUNK as i64 + lo as i64, last_c * CHUNK as i64 + hi as i64))
    }

    /// Present frames as maximal runs `(start..end, state)` within `range`.
    pub fn runs(&self, range: Range<FrameIndex>) -> Vec<(Range<FrameIndex>, FrameState)> {
        let mut out: Vec<(Range<FrameIndex>, FrameState)> = Vec::new();
        if range.is_empty() {
            return out;
        }
        let (c0, _) = split(range.start);
        let (c1, _) = split(range.end - 1);
        for (&c, ch) in self.chunks.range(c0..=c1) {
            let base = c * CHUNK as i64;
            for i in 0..CHUNK {
                let f = base + i as i64;
                if f < range.start || f >= range.end || ch.state[i] == FrameState::Absent {
                    continue;
                }
                match out.last_mut() {
                    Some((r, s)) if r.end == f && *s == ch.state[i] => r.end = f + 1,
                    _ => out.push((f..f + 1, ch.state[i])),
                }
            }
        }
        out
    }

    /// Whether two signals share every chunk (a snapshot taken with no writes since).
    pub fn same_as(&self, other: &Signal) -> bool {
        self.channels == other.channels
            && self.chunks.len() == other.chunks.len()
            && self.chunks.iter().zip(&other.chunks).all(|((a, x), (b, y))| a == b && Arc::ptr_eq(x, y))
    }

    /// Frames of the chunks that differ from `other` (by identity): what an
    /// undo swap must invalidate. Conservative at chunk granularity.
    pub fn differing_chunks(&self, other: &Signal) -> crate::ranges::RangeSet {
        let mut out = crate::ranges::RangeSet::new();
        let keys: std::collections::BTreeSet<i64> = self.chunks.keys().chain(other.chunks.keys()).copied().collect();
        for k in keys {
            let same = matches!((self.chunks.get(&k), other.chunks.get(&k)), (Some(a), Some(b)) if Arc::ptr_eq(a, b));
            if !same {
                out.insert(k * CHUNK as i64..(k + 1) * CHUNK as i64);
            }
        }
        out
    }

    /// Serialized chunks `(index, bytes)`: CHUNK state bytes, then the f32
    /// values little-endian. Used by the project file (content-addressed).
    pub fn chunk_bytes(&self) -> Vec<(i64, Vec<u8>)> {
        self.chunks
            .iter()
            .map(|(k, ch)| {
                let mut b = Vec::with_capacity(CHUNK + ch.data.len() * 4);
                b.extend(ch.state.iter().map(|s| *s as u8));
                for v in ch.data.iter() {
                    b.extend_from_slice(&v.to_le_bytes());
                }
                (*k, b)
            })
            .collect()
    }

    /// Rebuild a signal from [`Signal::chunk_bytes`] output.
    pub fn from_chunk_bytes<'a>(channels: usize, chunks: impl IntoIterator<Item = (i64, &'a [u8])>) -> anyhow::Result<Signal> {
        let mut s = Signal::new(channels);
        let expected = CHUNK + CHUNK * channels * 4;
        for (k, b) in chunks {
            anyhow::ensure!(b.len() == expected, "chunk {k}: {} bytes, expected {expected}", b.len());
            let mut ch = Chunk::new(channels);
            for (i, st) in b[..CHUNK].iter().enumerate() {
                ch.state[i] = match st {
                    1 => FrameState::Valid,
                    2 => FrameState::Stale,
                    _ => FrameState::Absent,
                };
            }
            for (i, v) in b[CHUNK..].chunks_exact(4).enumerate() {
                ch.data[i] = f32::from_le_bytes([v[0], v[1], v[2], v[3]]);
            }
            s.chunks.insert(k, Arc::new(ch));
        }
        Ok(s)
    }

    /// Number of chunk allocations shared with `other` (for tests and memory stats).
    pub fn shared_chunks_with(&self, other: &Signal) -> usize {
        self.chunks.iter().filter(|(k, v)| other.chunks.get(k).is_some_and(|w| Arc::ptr_eq(v, w))).count()
    }
}

/// All signals of the world.
#[derive(Resource, Default, Clone)]
pub struct SignalStore {
    signals: HashMap<SignalId, Signal>,
    next: u64,
}

impl SignalStore {
    pub fn create(&mut self, channels: usize) -> SignalId {
        self.next += 1;
        let id = SignalId(self.next);
        self.signals.insert(id, Signal::new(channels));
        id
    }

    pub fn get(&self, id: SignalId) -> Option<&Signal> {
        self.signals.get(&id)
    }

    pub fn get_mut(&mut self, id: SignalId) -> Option<&mut Signal> {
        self.signals.get_mut(&id)
    }

    pub fn remove(&mut self, id: SignalId) -> Option<Signal> {
        self.signals.remove(&id)
    }

    /// Put a signal back (undo of a removal, or restore from a snapshot).
    pub fn insert(&mut self, id: SignalId, signal: Signal) {
        self.next = self.next.max(id.0);
        self.signals.insert(id, signal);
    }

    pub fn ids(&self) -> impl Iterator<Item = SignalId> + '_ {
        self.signals.keys().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_and_state() {
        let mut s = Signal::new(2);
        assert_eq!(s.state(5), FrameState::Absent);
        s.set(5, &[1.0, 2.0]);
        assert_eq!(s.get(5), Some(&[1.0, 2.0][..]));
        assert_eq!(s.state(5), FrameState::Valid);
        s.set(-3, &[4.0, 5.0]); // negative frames (e.g. before the first frame) work too
        assert_eq!(s.get(-3), Some(&[4.0, 5.0][..]));
    }

    #[test]
    fn stale_keeps_values_but_not_validity() {
        let mut s = Signal::new(1);
        s.write(0, &[1.0, 2.0, 3.0, 4.0]);
        s.mark_stale(1..3);
        assert_eq!(s.state(0), FrameState::Valid);
        assert_eq!(s.state(1), FrameState::Stale);
        assert_eq!(s.get(1), Some(&[2.0][..]));
        assert_eq!(s.get_valid(1), None);
        assert_eq!(s.runs(0..10), vec![(0..1, FrameState::Valid), (1..3, FrameState::Stale), (3..4, FrameState::Valid)]);
    }

    #[test]
    fn clear_drops_empty_chunks() {
        let mut s = Signal::new(1);
        s.write(250, &[0.0; 20]); // spans two chunks
        s.clear(250..256);
        assert_eq!(s.runs(0..1000), vec![(256..270, FrameState::Valid)]);
        assert_eq!(s.chunks.len(), 1);
    }

    #[test]
    fn snapshots_share_untouched_chunks() {
        let mut s = Signal::new(1);
        s.write(0, &vec![1.0; CHUNK * 4]);
        let snap = s.clone();
        assert!(s.same_as(&snap));
        s.set(CHUNK as i64 + 3, &[9.0]); // copy-on-write of chunk 1 only
        assert!(!s.same_as(&snap));
        assert_eq!(s.shared_chunks_with(&snap), 3);
        assert_eq!(snap.get(CHUNK as i64 + 3), Some(&[1.0][..]));
        // Marking stale where nothing is present must not copy shared chunks.
        let snap2 = s.clone();
        s.mark_stale(10 * CHUNK as i64..11 * CHUNK as i64);
        assert_eq!(s.shared_chunks_with(&snap2), 4);
    }

    #[test]
    fn store_ids_are_unique_after_reinsert() {
        let mut st = SignalStore::default();
        let a = st.create(1);
        let sig = st.remove(a).unwrap();
        st.insert(a, sig);
        let b = st.create(1);
        assert_ne!(a, b);
    }
}
