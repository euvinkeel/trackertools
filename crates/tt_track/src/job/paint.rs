//! Paint trackers (DESIGN §6.2, `Method::Paint`): CoTracker run on many
//! points painted over the subject instead of one pixel, and one motion made
//! from the points that stay on it. Points in one stream cost the model
//! almost nothing more (its cost is the frames it encodes), so a painted
//! area becomes dozens of points.
//!
//! - **Paints** are the tracker's looks: what was brushed on a frame (the
//!   look's rectangle and its mask, any number of patches, gaps between them
//!   allowed). Each paint seeds up to [`POINTS_PER_PAINT`] points spread
//!   evenly over its painted cells, each a CoTracker query on its frame.
//! - **Paints are intersections** (on request: "the individual paints of
//!   each keyframe act like INTERSECTIONS where only the points who make it
//!   from one painting to the next painting are considered actual
//!   trackers"): between two paints, the *cohort* is the points that were
//!   in the cohort at the first (or seeded by it) and land on the second.
//!   Points that don't make it don't count anywhere in that stretch; points
//!   the second paint seeds join from there. After the last paint (in the
//!   job's direction) the cohort is what that paint left: there is nothing
//!   further to check it against.
//! - **The fit:** on every frame, the cohort's points the model sees (score
//!   at least the tracker's `min_score`) give one similarity (move, turn,
//!   scale) from the reference (the job's first frame, view px) to that
//!   frame: least squares, twice more without the points far from the rest.
//!   One point: a move only. None: the last motion holds, and the frame is
//!   lost. The tracked point is the first paint's centre through it; the
//!   output box scales with it.
//! - **So a stretch waits for its end:** who made it is known only when the
//!   next paint's frame is tracked, so the frames up to it come out
//!   together then (catch-up mode shows them when the playhead passes that
//!   paint). After the last paint they come out as they are tracked.
//! - **A stretch nothing makes it through** (the subject was painted
//!   somewhere none of the points went): its frames hold the last motion,
//!   lost, and at the next paint the motion starts again from it (its
//!   centre is the tracked point there).
//! - **Each frame's points** come out too ([`Mark`]): where each cohort
//!   point was, whether the model saw it, and whether it makes it to the
//!   next paint (the app draws them: kept, or faded red).

use super::LookSpec;
use tt_core::time::FrameIndex;
use tt_core::view::SpaceMap;

/// The most points one paint gives.
pub const POINTS_PER_PAINT: usize = 48;
/// A mask cell counts as painted from this value (0–255).
const PAINTED: u8 = 96;

/// A point and where it went.
pub type Pair = ([f64; 2], [f64; 2]);
/// A cohort point on a paint's frame: its query, its place in the reference, where it is (view px).
type Held = (usize, [f64; 2], [f64; 2]);

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

/// A paint's mask side (cells a side): masks are square, `n × n` (32 for a
/// look's, more for a paint over a large area: `tool::paint_look`).
pub fn mask_side(mask: &[u8]) -> Option<usize> {
    let n = (mask.len() as f64).sqrt().round() as usize;
    (n >= 2 && n * n == mask.len()).then_some(n)
}

/// A paint's painted cells' centres (source px), all of them.
fn painted_cells(look: &LookSpec) -> Vec<[f64; 2]> {
    let Some((mask, n)) = look.mask.as_ref().and_then(|m| Some((m, mask_side(m)?))) else {
        return vec![look.center];
    };
    let cell = [2.0 * look.half[0] / n as f64, 2.0 * look.half[1] / n as f64];
    let origin = [look.center[0] - look.half[0], look.center[1] - look.half[1]];
    let mut v = Vec::new();
    for (i, m) in mask.iter().enumerate() {
        if *m >= PAINTED {
            let (cx, cy) = ((i % n) as f64 + 0.5, (i / n) as f64 + 0.5);
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
    let step = cells.len() as f64 / most as f64;
    (0..most).map(|i| cells[((i as f64 + 0.5) * step) as usize]).collect()
}

/// Whether source point `p` is on a paint (on a painted cell, or next to one).
pub fn on(look: &LookSpec, p: [f64; 2]) -> bool {
    let Some((mask, n)) = look.mask.as_ref().and_then(|m| Some((m, mask_side(m)?))) else {
        return (p[0] - look.center[0]).abs() <= look.half[0] && (p[1] - look.center[1]).abs() <= look.half[1];
    };
    let u = (p[0] - (look.center[0] - look.half[0])) / (2.0 * look.half[0]) * n as f64;
    let v = (p[1] - (look.center[1] - look.half[1])) / (2.0 * look.half[1]) * n as f64;
    let (cx, cy) = (u.floor() as i64, v.floor() as i64);
    (-1..=1).any(|dy| (-1..=1).any(|dx| {
        let (x, y) = (cx + dx, cy + dy);
        (0..n as i64).contains(&x) && (0..n as i64).contains(&y) && mask[y as usize * n + x as usize] >= PAINTED
    }))
}

/// A paint job's state on a paint's frame, after it (its cohort, and the
/// motion there): where a job tracking again from that paint resumes
/// (`PaintFit::resume`), so a changed paint re-tracks from the paint before
/// it, not from the first.
#[derive(Clone, Debug, PartialEq)]
pub struct PaintState {
    pub frame: FrameIndex,
    pub motion: Similarity,
    /// The tracked point in the reference (view px), and the box's half-size at scale 1.
    pub target: [f64; 2],
    pub half: [f64; 2],
    /// The cohort: each point's place in the reference (view px), and where it is on `frame` (source px).
    pub points: Vec<([f64; 2], [f64; 2])>,
}

/// One query of a paint job: the stream frame it starts on (its paint's).
#[derive(Clone, Debug)]
pub struct PaintQuery {
    pub i: usize,
    /// Where it is in the reference (view px of the stream's first frame),
    /// once its paint's frame is fitted.
    pub reference: Option<[f64; 2]>,
}

/// One cohort point on one frame, for the app to draw.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mark {
    /// Which point (its query's number in the job).
    pub id: u32,
    /// Where the model put it (view px).
    pub at: [f64; 2],
    /// The model saw it there (score at least `min_score`).
    pub seen: bool,
    /// It makes it to the next paint (in the cohort); false: it doesn't (faded red).
    pub kept: bool,
}

/// A frame's result: the tracked point (view px), its score, lost, the
/// box's scale, and its cohort's points.
#[derive(Clone, Debug)]
pub struct FrameOut {
    pub i: usize,
    pub f: FrameIndex,
    pub at: [f64; 2],
    pub score: f32,
    pub lost: bool,
    pub scale: f64,
    pub marks: Vec<Mark>,
}

/// The model's points on a frame: per query, view px and score, None where
/// it has none (not started, or not finite).
pub type Points = Vec<Option<([f64; 2], f64)>>;

/// A paint job's state as its results come in (module docs).
#[derive(Clone, Debug)]
pub struct PaintFit {
    pub queries: Vec<PaintQuery>,
    /// The paints on the stream (stream index, the paint), in stream order.
    pub paints: Vec<(usize, LookSpec)>,
    /// The reference → frame motion, as last fitted.
    pub motion: Similarity,
    /// The tracked point in the reference (view px).
    pub target: [f64; 2],
    /// The output box's half-size at scale 1 (patch px).
    pub half: [f64; 2],
    /// Per query: in the cohort at the last paint passed.
    alive: Vec<bool>,
    /// The next paint's stream index (the end of the stretch now), if any.
    next: Option<usize>,
    /// The stretch's frames so far (stream index, frame, points), waiting for its end.
    held: Vec<(usize, FrameIndex, Points)>,
    min_score: f64,
    slack: f64,
    /// The states on the paints' frames passed (for the runner to keep), as
    /// (frame, the cohort's queries): `take_states` makes them whole.
    states: Vec<(FrameIndex, Similarity, Vec<Held>)>,
    /// Resumed: the stream's first frame comes out too (it may be one to redo).
    resumed: bool,
}

impl PaintFit {
    /// A job resuming from `state` (its first frame, stream index 0): its
    /// cohort's points are the first queries (`state.points.len()` of them,
    /// on frame 0, placed already), then the paints after it.
    pub fn resume(state: &PaintState, mut queries: Vec<PaintQuery>, paints: Vec<(usize, LookSpec)>, min_score: f64, slack: f64) -> Self {
        for (q, (r, _)) in queries.iter_mut().zip(&state.points) {
            q.reference = Some(*r);
        }
        let mut fit = Self::new(queries, paints, state.target, state.half, min_score, slack);
        fit.motion = state.motion;
        fit.resumed = true;
        fit
    }

    /// The states on the paints passed since the last call: (frame, motion,
    /// the cohort as (reference, view px)).
    pub fn take_states(&mut self) -> Vec<(FrameIndex, Similarity, Vec<Pair>)> {
        std::mem::take(&mut self.states).into_iter().map(|(f, m, pts)| (f, m, pts.into_iter().map(|(_, r, p)| (r, p)).collect())).collect()
    }
    /// `paints`: (stream index, paint) on the stream; the queries, each with its
    /// paint's stream index; `min_score`; outliers' least distance (view px).
    pub fn new(queries: Vec<PaintQuery>, mut paints: Vec<(usize, LookSpec)>, target: [f64; 2], half: [f64; 2], min_score: f64, slack: f64) -> Self {
        paints.sort_by_key(|(i, _)| *i);
        let alive = vec![false; queries.len()];
        Self { queries, paints, motion: Similarity::IDENTITY, target, half, alive, next: None, held: Vec::new(), min_score, slack, states: Vec::new(), resumed: false }
    }

    fn next_paint_after(&self, i: usize) -> Option<usize> {
        self.paints.iter().map(|(k, _)| *k).find(|k| *k > i)
    }

    /// Stream frame `i` (frame `f`, its view map `map`), the model's points
    /// there. Returns the frames that can come out now (module docs): none
    /// while a stretch waits for its end, the whole stretch at its end, the
    /// frame itself after the last paint.
    pub fn frame(&mut self, i: usize, f: FrameIndex, map: &SpaceMap, points: Points) -> Vec<FrameOut> {
        if i == 0 {
            // The first paint: its points are the cohort, where they are.
            for (k, q) in self.queries.iter_mut().enumerate() {
                if q.i == 0 {
                    // (Resumed: placed already, in the reference of the job they came from.)
                    if q.reference.is_none() {
                        q.reference = points.get(k).copied().flatten().map(|(p, _)| p);
                    }
                    self.alive[k] = q.reference.is_some() && points.get(k).copied().flatten().is_some();
                }
            }
            self.next = self.next_paint_after(0);
        }
        self.held.push((i, f, points));
        match self.next {
            // A stretch's end: who made it, then all its frames.
            Some(end) if end == i => {
                let here: Vec<LookSpec> = self.paints.iter().filter(|(k, _)| *k == i).map(|(_, l)| l.clone()).collect();
                let last = &self.held.last().expect("pushed").2;
                let made: Vec<bool> = (0..self.queries.len())
                    .map(|k| self.alive[k] && self.queries[k].i < i && last.get(k).copied().flatten().is_some_and(|(p, _)| here.iter().any(|l| on(l, map.to_source(p)))))
                    .collect();
                let restart = !made.iter().any(|m| *m);
                let held = std::mem::take(&mut self.held);
                let at_end = held.last().expect("pushed").2.clone();
                let mut out: Vec<FrameOut> = held.into_iter().map(|(j, g, pts)| self.fit(j, g, &pts, &made, false)).collect();
                if restart {
                    // Nothing made it: the motion starts again from this paint (its centre is the tracked point).
                    let c: Vec<[f64; 2]> = here.iter().map(|l| map.from_source(centre(l))).collect();
                    let at = [c.iter().map(|p| p[0]).sum::<f64>() / c.len() as f64, c.iter().map(|p| p[1]).sum::<f64>() / c.len() as f64];
                    let m = Similarity { t: [0.0, 0.0], ..self.motion }.apply(self.target);
                    self.motion.t = [at[0] - m[0], at[1] - m[1]];
                    if let Some(o) = out.last_mut() {
                        (o.at, o.score, o.lost) = (at, 1.0, false);
                    }
                }
                // This paint's points join, placed in the reference through the motion here.
                for (k, q) in self.queries.iter_mut().enumerate() {
                    if q.i == i {
                        q.reference = at_end.get(k).copied().flatten().map(|(p, _)| self.motion.invert(p));
                    }
                }
                self.alive = (0..self.queries.len()).map(|k| made[k] || (self.queries[k].i == i && self.queries[k].reference.is_some())).collect();
                self.next = self.next_paint_after(i);
                // The state here, for a job that tracks again from this paint.
                let cohort: Vec<(usize, [f64; 2], [f64; 2])> =
                    (0..self.queries.len()).filter(|k| self.alive[*k]).filter_map(|k| Some((k, self.queries[k].reference?, at_end.get(k).copied().flatten()?.0))).collect();
                if let Some(o) = out.last() {
                    self.states.push((o.f, self.motion, cohort));
                }
                out
            }
            Some(_) => Vec::new(),
            // After the last paint: as it comes.
            None => {
                let held = std::mem::take(&mut self.held);
                let alive = self.alive.clone();
                held.into_iter().map(|(j, g, pts)| self.fit(j, g, &pts, &alive, true)).collect()
            }
        }
    }

    /// The frames still waiting (the stream ended before the next paint):
    /// out with the cohort as it was (nothing further to check it against).
    pub fn finish(&mut self) -> Vec<FrameOut> {
        let held = std::mem::take(&mut self.held);
        let alive = self.alive.clone();
        held.into_iter().map(|(j, g, pts)| self.fit(j, g, &pts, &alive, true)).collect()
    }

    /// One frame fitted from the cohort `members` (those it marks kept; the
    /// other points alive at the stretch's start are marked not kept).
    fn fit(&mut self, i: usize, f: FrameIndex, pts: &Points, members: &[bool], open: bool) -> FrameOut {
        let seen = |k: usize| pts.get(k).copied().flatten().filter(|(_, s)| *s >= self.min_score).map(|(p, _)| p);
        let pairs: Vec<(usize, Pair)> = (0..self.queries.len()).filter(|k| members[*k]).filter_map(|k| Some((k, (self.queries[k].reference?, seen(k)?)))).collect();
        let only: Vec<Pair> = pairs.iter().map(|(_, p)| *p).collect();
        let fitted = Similarity::fit_robust(&only, &self.motion, self.slack);
        let (score, inliers) = match &fitted {
            Some((m, kept)) => {
                self.motion = *m;
                let s: Vec<f64> = pairs.iter().zip(kept).filter(|(_, k)| **k).map(|((k, _), _)| pts[*k].map_or(0.0, |(_, s)| s)).collect();
                let mean = s.iter().sum::<f64>() / s.len().max(1) as f64;
                (mean * (s.len() as f64 / pairs.len().max(1) as f64).sqrt(), s.len())
            }
            None => (0.0, 0),
        };
        // Its points: the cohort's (kept), and those alive at the stretch's start that don't make it.
        let marks = (0..self.queries.len())
            .filter(|k| self.queries[*k].i <= i && (members[*k] || (!open && self.alive[*k])))
            .filter_map(|k| pts.get(k).copied().flatten().map(|(p, s)| Mark { id: k as u32, at: p, seen: s >= self.min_score, kept: members[k] }))
            .collect();
        let given = i == 0 && !self.resumed;
        FrameOut { i, f, at: self.motion.apply(self.target), score: if given { 1.0 } else { score as f32 }, lost: inliers == 0 && !given, scale: self.motion.scale(), marks }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::look::MASK_N;

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
    /// painted on frames 0, 10 and 20; three of the first paint's points
    /// slide off from frame 5 (they don't reach the paint on 10: they don't
    /// count anywhere in 0–10, and show as not kept); a paint on 30 where
    /// no point went (20–30 lost, starting again on 30); after it, frames
    /// come out as they are tracked.
    #[test]
    fn paints_are_intersections() {
        let map = SpaceMap::identity(&tt_core::view::SourceSize { width: 1920.0, height: 1080.0 });
        let c = [100.0, 100.0];
        let motion = move |f: i64| {
            let ang = 0.01 * f as f64;
            let r = Similarity { a: ang.cos(), b: ang.sin(), t: [0.0, 0.0] }.apply(c);
            Similarity { a: ang.cos(), b: ang.sin(), t: [c[0] + 5.0 * f as f64 - r[0], c[1] - r[1]] }
        };
        let at = |f: i64, p: [f64; 2]| motion(f).apply(p);
        let disc = |f: i64, centre_at: [f64; 2]| {
            let mut l = painted(centre_at, 20.0);
            l.frame = f;
            l
        };
        let far = disc(30, [700.0, 500.0]);
        let paints = vec![(0usize, disc(0, c)), (10, disc(10, at(10, c))), (20, disc(20, at(20, c))), (30, far.clone())];
        // Each paint's points, where they are on its frame; then (in the reference) where they started.
        let mut queries = Vec::new();
        let mut start = Vec::new();
        for (i, l) in &paints {
            for p in sample(l, 20) {
                queries.push(PaintQuery { i: *i, reference: None });
                start.push(if *i == 30 { p } else { motion(*i as i64).invert(p) });
            }
        }
        let mut fit = PaintFit::new(queries.clone(), paints, centre(&disc(0, c)), [10.0, 10.0], 0.6, 1.0);
        let mut got = std::collections::BTreeMap::new();
        for f in 0..40i64 {
            let points: Points = queries
                .iter()
                .zip(&start)
                .enumerate()
                .map(|(k, (q, p))| {
                    if q.i as i64 > f {
                        return None;
                    }
                    let mut pos = if q.i == 30 { [p[0] + (f - 30) as f64 * 2.0, p[1]] } else { at(f, *p) };
                    // Three of the first paint's points slide off from frame 5.
                    if k < 3 && f >= 5 {
                        pos = [pos[0] - 30.0 * (f - 4) as f64, pos[1] + 20.0];
                    }
                    Some((pos, 0.9))
                })
                .collect();
            let out = fit.frame(f as usize, f, &map, points);
            // Before the last paint, a stretch's frames come out at its end; after it, each as it comes.
            match f {
                10 | 20 | 30 => assert!(!out.is_empty(), "frame {f}: the stretch comes out"),
                31.. => assert_eq!(out.len(), 1, "frame {f}: as it comes"),
                _ => assert!(out.is_empty(), "frame {f}: waits for the stretch's end"),
            }
            for o in out {
                got.insert(o.f, o);
            }
        }
        assert_eq!(got.len(), 40);
        for f in 0..=20 {
            let o = &got[&f];
            let want = at(f, c);
            assert!(dist(o.at, want) < 0.5, "frame {f}: {:?} vs {want:?}", o.at);
            assert!(!o.lost, "frame {f}");
        }
        // The points that slid off: shown as not kept in 0–10 (and never in the cohort), gone after.
        assert!(got[&3].marks.iter().filter(|m| m.id < 3).all(|m| !m.kept));
        assert!(got[&3].marks.iter().filter(|m| m.id >= 3 && m.id < 20).all(|m| m.kept));
        assert!(got[&15].marks.iter().all(|m| m.id >= 3), "after their stretch they are gone");
        // 20–30: nothing reaches the far paint: lost, the motion held; then it starts again there.
        assert!((21..30).all(|f| got[&f].lost), "a stretch nothing made it through is lost");
        assert!(dist(got[&30].at, centre(&far)) < 0.5 && !got[&30].lost);
        for f in 31..40 {
            let want = [centre(&far)[0] + (f - 30) as f64 * 2.0, centre(&far)[1]];
            assert!(dist(got[&f].at, want) < 0.5, "frame {f}: {:?} vs {want:?}", got[&f].at);
        }
    }

    /// A stream that ends before its next paint: its waiting frames come out
    /// with the cohort it had.
    #[test]
    fn a_stream_ending_before_the_next_paint_lets_its_frames_out() {
        let map = SpaceMap::identity(&tt_core::view::SourceSize { width: 1920.0, height: 1080.0 });
        let first = painted([100.0, 100.0], 20.0);
        let mut later = painted([300.0, 100.0], 20.0);
        later.frame = 50;
        let pts = sample(&first, 10);
        let queries: Vec<PaintQuery> = pts.iter().map(|_| PaintQuery { i: 0, reference: None }).collect();
        let mut fit = PaintFit::new(queries, vec![(0, first.clone()), (50, later)], centre(&first), [10.0, 10.0], 0.6, 1.0);
        for f in 0..5usize {
            let points: Points = pts.iter().map(|p| Some(([p[0] + f as f64, p[1]], 0.9))).collect();
            assert!(fit.frame(f, f as FrameIndex, &map, points).is_empty());
        }
        let out = fit.finish();
        assert_eq!(out.len(), 5);
        assert!(dist(out[4].at, [centre(&first)[0] + 4.0, centre(&first)[1]]) < 1e-6);
    }
}
