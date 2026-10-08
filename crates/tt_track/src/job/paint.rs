//! Paint trackers (DESIGN §6.2, `Method::Paint`): CoTracker run on many
//! points painted over the subject instead of one pixel, and one motion made
//! from them. Points in one stream cost the model almost nothing more (its
//! cost is the frames it encodes), so a painted area becomes a few dozen
//! points, and the subject is followed where most of them agree.
//!
//! - **Paints** are the tracker's looks: the area brushed (the look's
//!   rectangle and mask) on their frame. Each paint gives up to
//!   [`POINTS_PER_PAINT`] points spread evenly over its painted cells, each
//!   a CoTracker query on that frame.
//! - **The fit:** on every frame, the points the model sees (score at least
//!   the tracker's `min_score`) give one similarity (move, turn, scale) from
//!   the reference (the job's first frame, view px) to that frame: least
//!   squares, twice more without the points far from the rest (outliers:
//!   points that slid onto the background, or onto something else). One
//!   point: a move only. None: the last motion holds, and the frame is lost.
//! - **The tracked point** is the first paint's centre (its painted cells'
//!   centroid) through that motion; the output box turns with the scale.
//! - **A paint on a later frame** (a reset paint) says what the subject is
//!   there: points that are not on it then are dropped from there on (they
//!   left the subject), and its own points join, placed in the reference
//!   through the motion on its frame. Where none of the earlier points are
//!   on it, the motion starts again from it: its centre is where the first
//!   paint's centre was (the person painted the subject anew). So painting
//!   the same thing on frames spread over the shot keeps the points on it.

use super::LookSpec;
use crate::look::MASK_N;
use tt_core::time::FrameIndex;
use tt_core::view::SpaceMap;

/// The most points one paint gives.
pub const POINTS_PER_PAINT: usize = 48;
/// A mask cell counts as painted from this value (0–255).
const PAINTED: u8 = 96;

/// A point and where it went.
type Pair = ([f64; 2], [f64; 2]);

/// A similarity: `p' = [a·x − b·y + tx, b·x + a·y + ty]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Similarity {
    pub a: f64,
    pub b: f64,
    pub t: [f64; 2],
}

impl Similarity {
    pub const IDENTITY: Self = Self { a: 1.0, b: 0.0, t: [0.0, 0.0] };

    pub fn apply(&self, p: [f64; 2]) -> [f64; 2] {
        [self.a * p[0] - self.b * p[1] + self.t[0], self.b * p[0] + self.a * p[1] + self.t[1]]
    }

    pub fn invert(&self, q: [f64; 2]) -> [f64; 2] {
        let d = (self.a * self.a + self.b * self.b).max(1e-12);
        let (x, y) = (q[0] - self.t[0], q[1] - self.t[1]);
        [(self.a * x + self.b * y) / d, (-self.b * x + self.a * y) / d]
    }

    pub fn scale(&self) -> f64 {
        self.a.hypot(self.b)
    }

    /// Its turn, radians (clockwise on screen: y is down).
    pub fn angle(&self) -> f64 {
        self.b.atan2(self.a)
    }

    /// The least-squares similarity taking `from` to `to` (two points or
    /// more; one: a move only, keeping `keep`'s turn and scale).
    pub fn fit(pairs: &[Pair], keep: &Similarity) -> Option<Self> {
        let n = pairs.len() as f64;
        match pairs.len() {
            0 => None,
            1 => {
                let (p, q) = pairs[0];
                let m = Similarity { t: [0.0, 0.0], ..*keep }.apply(p);
                Some(Similarity { t: [q[0] - m[0], q[1] - m[1]], ..*keep })
            }
            _ => {
                let c = |pick: fn(&Pair) -> [f64; 2]| {
                    let s = pairs.iter().map(pick).fold([0.0, 0.0], |s, p| [s[0] + p[0], s[1] + p[1]]);
                    [s[0] / n, s[1] / n]
                };
                let (cp, cq) = (c(|p| p.0), c(|p| p.1));
                let (mut sa, mut sb, mut ss) = (0.0, 0.0, 0.0);
                for (p, q) in pairs {
                    let (x, y) = (p[0] - cp[0], p[1] - cp[1]);
                    let (u, v) = (q[0] - cq[0], q[1] - cq[1]);
                    sa += x * u + y * v;
                    sb += x * v - y * u;
                    ss += x * x + y * y;
                }
                // (All the points in one place: a move only.)
                if ss < 1e-9 {
                    return Similarity::fit(&[(cp, cq)], keep);
                }
                let (a, b) = (sa / ss, sb / ss);
                Some(Similarity { a, b, t: [cq[0] - (a * cp[0] - b * cp[1]), cq[1] - (b * cp[0] + a * cp[1])] })
            }
        }
    }

    /// [`Self::fit`] without outliers: fitted again twice without the points
    /// far from where it puts them (over 2.5 × the median distance, and over
    /// `slack` px). Returns it and which pairs it kept.
    pub fn fit_robust(pairs: &[Pair], keep: &Similarity, slack: f64) -> Option<(Self, Vec<bool>)> {
        let mut inlier = vec![true; pairs.len()];
        let mut fit = Similarity::fit(pairs, keep)?;
        for _ in 0..2 {
            let d: Vec<f64> = pairs.iter().map(|(p, q)| dist(fit.apply(*p), *q)).collect();
            let mut sorted = d.clone();
            sorted.sort_by(f64::total_cmp);
            let limit = (2.5 * sorted[sorted.len() / 2]).max(slack);
            inlier = d.iter().map(|d| *d <= limit).collect();
            let kept: Vec<_> = pairs.iter().zip(&inlier).filter(|(_, k)| **k).map(|(p, _)| *p).collect();
            fit = Similarity::fit(&kept, keep).unwrap_or(fit);
        }
        Some((fit, inlier))
    }
}

fn dist(a: [f64; 2], b: [f64; 2]) -> f64 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}

/// A paint's painted cells' centres (source px), all of them.
fn painted_cells(look: &LookSpec) -> Vec<[f64; 2]> {
    let Some(mask) = look.mask.as_ref().filter(|m| m.len() == MASK_N * MASK_N) else {
        return vec![look.center];
    };
    let cell = [2.0 * look.half[0] / MASK_N as f64, 2.0 * look.half[1] / MASK_N as f64];
    let origin = [look.center[0] - look.half[0], look.center[1] - look.half[1]];
    let mut v = Vec::new();
    for (i, m) in mask.iter().enumerate() {
        if *m >= PAINTED {
            let (cx, cy) = ((i % MASK_N) as f64 + 0.5, (i / MASK_N) as f64 + 0.5);
            v.push([origin[0] + cx * cell[0], origin[1] + cy * cell[1]]);
        }
    }
    if v.is_empty() { vec![look.center] } else { v }
}

/// A paint's centre (source px): its painted cells' centroid.
pub fn centre(look: &LookSpec) -> [f64; 2] {
    let cells = painted_cells(look);
    let n = cells.len() as f64;
    let s = cells.iter().fold([0.0, 0.0], |s, p| [s[0] + p[0], s[1] + p[1]]);
    [s[0] / n, s[1] / n]
}

/// Up to `most` points spread evenly over a paint's painted cells (source px).
pub fn sample(look: &LookSpec, most: usize) -> Vec<[f64; 2]> {
    let cells = painted_cells(look);
    if cells.len() <= most {
        return cells;
    }
    // Every k-th cell in reading order, staggered by row so they don't line up in columns.
    let step = cells.len() as f64 / most as f64;
    (0..most).map(|i| cells[((i as f64 + 0.5) * step) as usize]).collect()
}

/// Whether source point `p` is on a paint (on a painted cell, or next to one).
pub fn on(look: &LookSpec, p: [f64; 2]) -> bool {
    let Some(mask) = look.mask.as_ref().filter(|m| m.len() == MASK_N * MASK_N) else {
        return (p[0] - look.center[0]).abs() <= look.half[0] && (p[1] - look.center[1]).abs() <= look.half[1];
    };
    let u = (p[0] - (look.center[0] - look.half[0])) / (2.0 * look.half[0]) * MASK_N as f64;
    let v = (p[1] - (look.center[1] - look.half[1])) / (2.0 * look.half[1]) * MASK_N as f64;
    let (cx, cy) = (u.floor() as i64, v.floor() as i64);
    (-1..=1).any(|dy| {
        (-1..=1).any(|dx| {
            let (x, y) = (cx + dx, cy + dy);
            (0..MASK_N as i64).contains(&x) && (0..MASK_N as i64).contains(&y) && mask[y as usize * MASK_N + x as usize] >= PAINTED
        })
    })
}

/// One query of a paint job: its stream frame, and the paint it came from.
#[derive(Clone, Debug)]
pub struct PaintQuery {
    pub i: usize,
    pub paint: usize,
    /// Where it is in the reference (view px of the stream's first frame),
    /// once known.
    pub reference: Option<[f64; 2]>,
    /// Dropped by a later paint it was not on.
    pub dropped: bool,
}

/// A paint job's state as its results come in (module docs).
#[derive(Clone, Debug)]
pub struct PaintFit {
    pub queries: Vec<PaintQuery>,
    /// The paints (the spec's looks with a mask, in its order) and their frames.
    pub paints: Vec<(FrameIndex, LookSpec)>,
    /// The reference → this frame motion, as last fitted.
    pub motion: Similarity,
    /// The tracked point in the reference (view px).
    pub target: [f64; 2],
    /// The output box's half-size at scale 1 (patch px).
    pub half: [f64; 2],
    /// The paints whose frame was taken (their drops and joins done).
    seen: Vec<bool>,
}

/// What a frame's fit gives: the tracked point (view px), its score, lost,
/// and the box's scale.
pub struct FrameFit {
    pub at: [f64; 2],
    pub score: f32,
    pub lost: bool,
    pub scale: f64,
}

impl PaintFit {
    pub fn new(queries: Vec<PaintQuery>, paints: Vec<(FrameIndex, LookSpec)>, target: [f64; 2], half: [f64; 2]) -> Self {
        let seen = vec![false; paints.len()];
        Self { queries, paints, motion: Similarity::IDENTITY, target, half, seen }
    }

    /// Stream frame `i` (frame `f`, its view map `map`): the model's points
    /// there (view px and score, per query, None where not tracked), the
    /// score below which a point is not seen. Its paints on `f` drop and add
    /// points; the motion is fitted again (module docs).
    pub fn frame(&mut self, i: usize, f: FrameIndex, map: &SpaceMap, points: &[Option<([f64; 2], f64)>], min_score: f64, slack: f64) -> FrameFit {
        let seen = |q: &PaintQuery, k: usize| -> Option<[f64; 2]> { points.get(k).copied().flatten().filter(|(_, s)| *s >= min_score && q.i <= i).map(|(p, _)| p) };
        // A paint on this frame: earlier points not on it are dropped (on it, kept).
        let here: Vec<usize> = (0..self.paints.len()).filter(|p| self.paints[*p].0 == f && !self.seen[*p]).collect();
        let mut restart = false;
        if !here.is_empty() && i > 0 {
            let mut kept = 0;
            for (k, q) in self.queries.iter_mut().enumerate() {
                if q.dropped || q.reference.is_none() || q.i >= i {
                    continue;
                }
                let on_paint = points.get(k).copied().flatten().is_some_and(|(p, _)| here.iter().any(|h| on(&self.paints[*h].1, map.to_source(p))));
                if on_paint {
                    kept += 1;
                } else {
                    q.dropped = true;
                }
            }
            restart = kept == 0;
        }
        // The motion from what is seen.
        let pairs: Vec<([f64; 2], [f64; 2])> = self.queries.iter().enumerate().filter(|(_, q)| !q.dropped).filter_map(|(k, q)| Some((q.reference?, seen(q, k)?))).collect();
        let fitted = Similarity::fit_robust(&pairs, &self.motion, slack);
        let (inliers, total) = match &fitted {
            Some((m, kept)) => {
                self.motion = *m;
                (kept.iter().filter(|k| **k).count(), pairs.len())
            }
            None => (0, 0),
        };
        if restart {
            // No earlier point is on the new paint: it is where the first paint's centre was.
            let c: Vec<[f64; 2]> = here.iter().map(|h| map.from_source(centre(&self.paints[*h].1))).collect();
            let at = [c.iter().map(|p| p[0]).sum::<f64>() / c.len() as f64, c.iter().map(|p| p[1]).sum::<f64>() / c.len() as f64];
            let m = Similarity { t: [0.0, 0.0], ..self.motion }.apply(self.target);
            self.motion.t = [at[0] - m[0], at[1] - m[1]];
        }
        // Points starting on this frame join, placed in the reference through the motion here.
        for (k, q) in self.queries.iter_mut().enumerate() {
            if q.i == i && q.reference.is_none() {
                q.reference = points.get(k).copied().flatten().map(|(p, _)| self.motion.invert(p));
            }
        }
        for h in here {
            self.seen[h] = true;
        }
        let mean = |v: Vec<f64>| if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 };
        let scores: Vec<f64> = match &fitted {
            Some((_, kept)) => self
                .queries
                .iter()
                .enumerate()
                .filter(|(_, q)| !q.dropped && q.reference.is_some())
                .filter_map(|(k, q)| seen(q, k).map(|_| points[k].map_or(0.0, |(_, s)| s)))
                .zip(kept.iter())
                .filter(|(_, k)| **k)
                .map(|(s, _)| s)
                .collect(),
            None => Vec::new(),
        };
        let agree = if total == 0 { 0.0 } else { inliers as f64 / total as f64 };
        let score = (mean(scores) * agree.sqrt()) as f32;
        // (The first frame is where the paint is: nothing to fit there.)
        let given = restart || i == 0;
        FrameFit { at: self.motion.apply(self.target), score: if given { 1.0 } else { score }, lost: inliers == 0 && !given, scale: self.motion.scale() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn painted(center: [f64; 2], half: f64) -> LookSpec {
        // A disc painted inside its square.
        let mut mask = vec![0u8; MASK_N * MASK_N];
        for y in 0..MASK_N {
            for x in 0..MASK_N {
                let (u, v) = (x as f64 + 0.5 - MASK_N as f64 / 2.0, y as f64 + 0.5 - MASK_N as f64 / 2.0);
                if u.hypot(v) <= MASK_N as f64 / 2.0 {
                    mask[y * MASK_N + x] = 255;
                }
            }
        }
        LookSpec { frame: 0, center, half: [half, half], mask: Some(mask) }
    }

    #[test]
    fn a_similarity_is_found_from_its_points_and_outliers_are_left_out() {
        let truth = Similarity { a: 1.1 * 0.3f64.cos(), b: 1.1 * 0.3f64.sin(), t: [40.0, -12.0] };
        let mut pairs: Vec<([f64; 2], [f64; 2])> = (0..30).map(|k| [(k % 6) as f64 * 9.0, (k / 6) as f64 * 7.0]).map(|p| (p, truth.apply(p))).collect();
        // Five points slid off onto the background.
        for (_, q) in pairs.iter_mut().take(5) {
            *q = [q[0] + 60.0, q[1] - 35.0];
        }
        let (fit, kept) = Similarity::fit_robust(&pairs, &Similarity::IDENTITY, 1.0).expect("a fit");
        assert!((fit.a - truth.a).abs() < 1e-9 && (fit.b - truth.b).abs() < 1e-9, "{fit:?}");
        assert!(dist(fit.t, truth.t) < 1e-6);
        assert_eq!(kept.iter().filter(|k| !**k).count(), 5, "the five outliers left out");
        let p = [3.0, 4.0];
        let back = fit.invert(fit.apply(p));
        assert!(dist(back, p) < 1e-9);
    }

    #[test]
    fn a_paint_gives_points_on_it_and_its_centre() {
        let look = painted([100.0, 50.0], 20.0);
        let pts = sample(&look, POINTS_PER_PAINT);
        assert_eq!(pts.len(), POINTS_PER_PAINT);
        assert!(pts.iter().all(|p| on(&look, *p)), "every point is on the paint");
        assert!(dist(centre(&look), [100.0, 50.0]) < 0.5);
        // Spread over it: some on each side of its centre.
        assert!(pts.iter().any(|p| p[0] < 92.0) && pts.iter().any(|p| p[0] > 108.0) && pts.iter().any(|p| p[1] < 42.0) && pts.iter().any(|p| p[1] > 58.0));
        assert!(!on(&look, [140.0, 50.0]) && !on(&look, [100.0, 90.0]));
    }

    /// The whole loop on made-up model output: a subject moving and turning,
    /// a few points sliding off, a reset paint that drops them, and one that
    /// starts again where no earlier point is on it.
    #[test]
    fn a_paint_fit_follows_the_subject_and_its_reset_paints() {
        let map = SpaceMap::identity(&tt_core::view::SourceSize { width: 1920.0, height: 1080.0 });
        let first = painted([100.0, 100.0], 20.0);
        let mut later = painted([150.0, 100.0], 20.0);
        later.frame = 10;
        let mut far = painted([400.0, 300.0], 20.0);
        far.frame = 20;
        let paints = vec![(0, first.clone()), (10, later.clone()), (20, far.clone())];
        let mut queries = Vec::new();
        let mut reference_pts = Vec::new();
        for (paint, (f, look)) in paints.iter().enumerate() {
            for p in sample(look, 20) {
                queries.push(PaintQuery { i: *f as usize, paint, reference: None, dropped: false });
                reference_pts.push(p);
            }
        }
        let mut fit = PaintFit::new(queries.clone(), paints, centre(&first), [10.0, 10.0]);
        // The subject moves 5 px right a frame and turns a little; until frame 20, when it is far away.
        let c = centre(&first);
        let motion = move |f: i64| {
            // Turning about its own centre as it moves.
            let ang = 0.01 * f as f64;
            let r = Similarity { a: ang.cos(), b: ang.sin(), t: [0.0, 0.0] }.apply(c);
            Similarity { a: ang.cos(), b: ang.sin(), t: [c[0] + 5.0 * f as f64 - r[0], c[1] - r[1]] }
        };
        for f in 0..25i64 {
            let points: Vec<Option<([f64; 2], f64)>> = queries
                .iter()
                .zip(&reference_pts)
                .enumerate()
                .map(|(k, (q, p))| {
                    if (q.i as i64) > f {
                        return None;
                    }
                    // Its point at its own frame where it was painted; the subject's motion since.
                    let at_q = if q.paint == 0 { *p } else { motion(q.i as i64).apply(motion(q.i as i64).invert(*p)) };
                    let mut pos = if q.paint == 2 { [p[0] + (f - 20) as f64 * 2.0, p[1]] } else { motion(f).apply(motion(q.i as i64).invert(at_q)) };
                    // Three of the first paint's points slide off from frame 5.
                    if q.paint == 0 && k < 3 && f >= 5 {
                        pos = [pos[0] - 30.0 * (f - 4) as f64, pos[1] + 20.0];
                    }
                    Some((pos, 0.9))
                })
                .collect();
            let r = fit.frame(f as usize, f, &map, &points, 0.6, 1.0);
            let want = if f < 20 { motion(f).apply(centre(&first)) } else { [centre(&far)[0] + (f - 20) as f64 * 2.0, centre(&far)[1]] };
            assert!(dist(r.at, want) < 0.5, "frame {f}: {:?} vs {want:?}", r.at);
            assert!(!r.lost);
            if f == 10 {
                assert!(fit.queries[..3].iter().all(|q| q.dropped), "the points that slid off are dropped by the reset paint");
                assert!(fit.queries[3..20].iter().all(|q| !q.dropped), "the others are kept");
            }
        }
    }
}
