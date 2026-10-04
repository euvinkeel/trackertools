//! The template tracker: follows what its looks look like, frame to frame,
//! inside the region its guide (a sketch: the rough pass) says the subject is.
//!
//! - **Seed:** the first look's centre on its frame (where the user put it);
//!   an older tracker with no looks takes a square around the guide's point.
//! - **Looks:** each is a template (rectangular, masked where painted). On
//!   each frame the best-matching one wins, so a subject that changes shape
//!   (a cursor switching icons) is followed through every shape shown.
//! - **Predict:** the subject keeps its offset from the guide, so the next
//!   position is the guide's point plus the last offset. The hand already
//!   followed the motion; the tracker only corrects what the hand got wrong.
//! - **Search** for the best normalized cross-correlation that also agrees
//!   in contrast (and, painted, in brightness: `ncc::photometric`), with a
//!   gentle preference for the prediction so a look-alike elsewhere doesn't
//!   win a tie: first within `REACH` of the prediction and of where the
//!   tracker itself was going (its last position plus half its last step:
//!   a rough pass is early or late where the subject starts or stops, and a
//!   resting cursor stays put while the guide already moves on), and where
//!   nothing there is good enough, the whole patch (the guide's boxes around
//!   this frame, × `search`), because on a flick the hand lags.
//! - **Pins:** on a frame where the user showed a look, the position is theirs.
//! - **One point:** each look knows its offset from the point the seed look
//!   defines (`LookTemplate::offset`, found by matching the looks against
//!   each other), so whichever look matches, the same point on the subject
//!   is reported: the path doesn't jump when another look takes over.
//! - **Appearance:** the winning look is blended with the last frame's
//!   appearance of it (`adapt`), which follows slow changes without drifting.
//! - **Lost:** below `min_score`, the frame keeps the prediction (the guide
//!   carries the tracker through an occlusion or blur) and is flagged; the
//!   tracker re-locks as soon as a look comes back.
//! - **Refined:** the correlation's peak is refined by Lucas–Kanade
//!   (`ncc::refine`), which follows edges to a fraction of a pixel.
//! - **Both ways:** a job tracks each stretch between two pins from both
//!   ends and keeps, frame by frame, the better of the two ([`fuse`]).

use crate::image::{Grid, Patch};
use crate::ncc::{Mask, Prior, Template, Tolerance, best_match};

/// An older tracker's template half-size, patch pixels (a 21 × 21 template).
pub const TEMPLATE_R: usize = 10;
/// A look's larger half-size in patch pixels (the patch scale is set so).
pub const LOOK_PX: f64 = 12.0;
/// Within this distance of the prediction (patch pixels) the preference for
/// it grows; beyond, it is flat.
pub const REACH: f64 = 40.0;
/// Strength of the preference for the prediction (score units at `REACH` and beyond).
const PRIOR_WEIGHT: f32 = 0.1;
/// How much of a score being outside the guide's box costs (at a box's
/// half-size out and beyond; in proportion closer).
const OFF_WEIGHT: f32 = 0.3;
/// A match at least this good ends the search: other looks aren't tried.
const CONFIDENT: f32 = 0.9;
/// A match at least this good refreshes the remembered appearance.
pub const REFRESH_SCORE: f32 = 0.6;

/// The tracker's tuning (from the `Tracker` component).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub adapt: f32,
    pub min_score: f32,
    /// How alike a placement must be to count (`ncc::Tolerance`).
    pub tolerance: Tolerance,
}

/// One frame's result, in view pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Step {
    pub pos: [f64; 2],
    pub score: f32,
    pub lost: bool,
    /// Which look matched.
    pub look: usize,
}

/// A look ready to match: its template, half-size (patch px) and mask.
#[derive(Clone, Debug)]
pub struct LookTemplate {
    pub template: Template,
    pub r: [usize; 2],
    pub mask: Option<Vec<u8>>,
    /// Its centre minus the tracked point (view px): where it sits on the
    /// subject relative to the point the seed look defines.
    pub offset: [f64; 2],
}

impl LookTemplate {
    /// Cut from `patch` centred at patch point `c`, matched with `tolerance`.
    pub fn cut(patch: &Patch, c: [f64; 2], r: [usize; 2], mask: Option<Vec<u8>>, tolerance: Tolerance) -> Option<Self> {
        let template = Template::cut_with(patch, c, r, mask.as_deref().map(|m| mask_of(m)), tolerance)?;
        Some(Self { template, r, mask, offset: [0.0, 0.0] })
    }

    /// The same look's appearance at patch point `c` of another frame.
    fn again(&self, patch: &Patch, c: [f64; 2]) -> Option<Template> {
        Template::cut_with(patch, c, self.r, self.mask.as_deref().map(|m| mask_of(m)), self.template.tolerance)
    }
}

fn mask_of(cells: &[u8]) -> Mask<'_> {
    let n = (cells.len() as f64).sqrt() as usize;
    Mask { cells, w: n, h: n }
}

#[derive(Clone)]
pub struct TemplateTracker {
    looks: Vec<LookTemplate>,
    /// The last frame's appearance of the look that matched there.
    last: Option<(usize, Template)>,
    /// Position minus the guide's point, view pixels.
    offset: [f64; 2],
    /// The last position it found (view px) and the step before it, if the
    /// frame before was found too.
    own: Option<([f64; 2], [f64; 2])>,
    settings: Settings,
}

impl TemplateTracker {
    /// An older tracker: one square look on the anchor frame's patch at view
    /// point `pos` (the guide's point there).
    pub fn seed(patch: &Patch, grid: Grid, pos: [f64; 2], settings: Settings) -> Option<Self> {
        let look = LookTemplate::cut(patch, grid.from_view(pos), [TEMPLATE_R, TEMPLATE_R], None, settings.tolerance)?;
        Some(Self { looks: vec![look], last: None, offset: [0.0, 0.0], own: Some((pos, [0.0, 0.0])), settings })
    }

    /// Looks the user showed it, starting at `offset` from the guide's point (view px).
    pub fn with_looks(looks: Vec<LookTemplate>, offset: [f64; 2], settings: Settings) -> Option<Self> {
        (!looks.is_empty()).then_some(Self { looks, last: None, offset, own: None, settings })
    }

    /// Continue from a known position on a frame other than the anchor
    /// (resuming a job), where the match scored `score`: the first look's
    /// appearance there becomes the last one if it was a good match (a lost
    /// frame's position is only the guide's prediction: its look may be anything).
    pub fn resume(&mut self, patch: &Patch, grid: Grid, pos: [f64; 2], guide: [f64; 2], score: f32) {
        let o = self.looks[0].offset;
        self.last = if score >= REFRESH_SCORE { self.looks[0].again(patch, grid.from_view([pos[0] + o[0], pos[1] + o[1]])).map(|t| (0, t)) } else { None };
        self.offset = [pos[0] - guide[0], pos[1] - guide[1]];
        self.own = (score >= self.settings.min_score).then_some((pos, [0.0, 0.0]));
    }

    /// Start at `pos` (view px) on the anchor frame.
    pub fn start_at(&mut self, pos: [f64; 2]) {
        self.own = Some((pos, [0.0, 0.0]));
    }

    /// Where it expects the point on the next frame (view px): where it was
    /// going on its own (its last position plus half its last step), else
    /// the guide's point `guide` plus its offset.
    pub fn expected(&self, guide: [f64; 2]) -> [f64; 2] {
        self.own.map_or([guide[0] + self.offset[0], guide[1] + self.offset[1]], |(p, v)| [p[0] + 0.5 * v[0], p[1] + 0.5 * v[1]])
    }

    /// A frame where the user showed the subject at `pos` (a look's frame):
    /// the position is theirs, and tracking goes on from it.
    pub fn pin(&mut self, pos: [f64; 2], guide: [f64; 2]) {
        self.offset = [pos[0] - guide[0], pos[1] - guide[1]];
        self.last = None;
        self.own = Some((pos, [0.0, 0.0]));
    }

    /// Track into the next frame. `guide` is the guide's point there (view px).
    pub fn step(&mut self, patch: &Patch, grid: Grid, guide: [f64; 2]) -> Step {
        let far = f64::INFINITY;
        self.step_in(patch, grid, [guide[0], guide[1], -far, -far, far, far])
    }

    /// Track into the next frame. `guide` is the guide's box there
    /// `[x, y, left, top, right, bottom]` (view px): where two placements
    /// match about as well, the one inside it (or less far out) wins, and a
    /// match near the prediction that lies outside it doesn't end the search.
    pub fn step_in(&mut self, patch: &Patch, grid: Grid, guide_box: [f64; 6]) -> Step {
        let guide = [guide_box[0], guide_box[1]];
        // A placement's rank: its score, less for being outside the guide's box.
        let rank = |i: usize, m: &crate::ncc::Match| {
            let (c, o) = (grid.to_view(m.pos), self.looks[i].offset);
            m.score - OFF_WEIGHT * off_box([c[0] - o[0], c[1] - o[1]], &guide_box).min(1.0) as f32
        };
        let predicted = [guide[0] + self.offset[0], guide[1] + self.offset[1]];
        let templates: Vec<Template> = (0..self.looks.len())
            .map(|i| match &self.last {
                Some((j, last)) if *j == i && self.settings.adapt > 0.0 => Template::blend(&self.looks[i].template, last, self.settings.adapt.min(1.0)),
                _ => self.looks[i].template.clone(),
            })
            .collect();
        // Where it was going on its own: its last position plus half its last step.
        let own = self.own.map(|(p, v)| [p[0] + 0.5 * v[0], p[1] + 0.5 * v[1]]);
        // The best look and placement within `REACH` of each of `at` (view
        // px; one window around both when they are close), or anywhere in the patch.
        let search = |at: &[[f64; 2]]| {
            let mut best: Option<(usize, crate::ncc::Match)> = None;
            // The look that matched last first: when it is still clearly
            // there, the others aren't searched (an icon change drops it).
            let first = self.last.as_ref().map_or(0, |(j, _)| *j);
            let order = std::iter::once(first).chain((0..self.looks.len()).filter(|i| *i != first));
            for i in order {
                if best.is_some_and(|(_, b): (usize, crate::ncc::Match)| b.score >= CONFIDENT) {
                    break;
                }
                let (look, template) = (&self.looks[i], &templates[i]);
                // (This look's centre is where the point is predicted, plus its offset.)
                let cs: Vec<[f64; 2]> = at.iter().map(|p| grid.from_view([p[0] + look.offset[0], p[1] + look.offset[1]])).collect();
                let windows: Vec<[[f64; 2]; 2]> = match cs.as_slice() {
                    [] => vec![[[f64::NEG_INFINITY; 2], [f64::INFINITY; 2]]],
                    [a, b] if (a[0] - b[0]).abs().max((a[1] - b[1]).abs()) < REACH => {
                        vec![[[a[0].min(b[0]) - REACH, a[1].min(b[1]) - REACH], [a[0].max(b[0]) + REACH, a[1].max(b[1]) + REACH]]]
                    }
                    cs => cs.iter().map(|c| [[c[0] - REACH, c[1] - REACH], [c[0] + REACH, c[1] + REACH]]).collect(),
                };
                let c = grid.from_view([predicted[0] + look.offset[0], predicted[1] + look.offset[1]]);
                // (The guide's box for this look's centre, in patch points.)
                let (lo, hi) = (grid.from_view([guide_box[2] + look.offset[0], guide_box[3] + look.offset[1]]), grid.from_view([guide_box[4] + look.offset[0], guide_box[5] + look.offset[1]]));
                let g = grid.from_view([guide_box[0] + look.offset[0], guide_box[1] + look.offset[1]]);
                let within = Some(([g[0], g[1], lo[0], lo[1], hi[0], hi[1]], OFF_WEIGHT));
                let prior = Some(Prior { centre: c, radius: REACH, weight: PRIOR_WEIGHT, within });
                for window in windows {
                    if let Some(m) = best_match(patch, template, window, prior)
                        && best.is_none_or(|(j, b)| rank(i, &m) > rank(j, &b))
                    {
                        best = Some((i, m));
                    }
                }
            }
            best
        };
        let near: Vec<[f64; 2]> = std::iter::once(predicted).chain(own).collect();
        let mut best = search(&near);
        // Nothing good near the predictions, or only outside the guide's box: look everywhere.
        if best.is_none_or(|(i, m)| rank(i, &m) < self.settings.min_score || rank(i, &m) < m.score)
            && let Some(wide) = search(&[])
            && best.is_none_or(|(i, m)| rank(wide.0, &wide.1) > rank(i, &m))
        {
            best = Some(wide);
        }
        match best {
            Some((i, mut m)) if m.score >= self.settings.min_score => {
                m.pos = crate::ncc::refine(patch, &templates[i], m.pos);
                let (c, o) = (grid.to_view(m.pos), self.looks[i].offset);
                let pos = [c[0] - o[0], c[1] - o[1]];
                self.offset = [pos[0] - guide[0], pos[1] - guide[1]];
                self.own = Some((pos, self.own.map_or([0.0, 0.0], |(p, _)| [pos[0] - p[0], pos[1] - p[1]])));
                if m.score >= REFRESH_SCORE
                    && let Some(t) = self.looks[i].again(patch, m.pos)
                {
                    self.last = Some((i, t));
                }
                Step { pos, score: m.score, lost: false, look: i }
            }
            other => {
                // Lost: where it last saw the subject stays its own guess, standing still.
                self.own = self.own.map(|(p, _)| (p, [0.0, 0.0]));
                Step { pos: predicted, score: other.map_or(0.0, |(_, m)| m.score.max(0.0)), lost: true, look: other.map_or(0, |(i, _)| i) }
            }
        }
    }
}

/// How far `p` is outside box `b` (`[x, y, left, top, right, bottom]`), in
/// the box's half-sizes on that side (0 inside).
pub fn off_box(p: [f64; 2], b: &[f64; 6]) -> f64 {
    let side = |v: f64, c: f64, lo: f64, hi: f64| {
        if v < lo {
            (lo - v) / (c - lo).max(1.0)
        } else if v > hi {
            (v - hi) / (hi - c).max(1.0)
        } else {
            0.0
        }
    };
    side(p[0], b[0], b[2], b[4]).max(side(p[1], b[1], b[3], b[5]))
}

/// One frame's estimate, as fused: its position (view px), score, whether
/// it was lost, and how far outside the guide's box it is ([`off_box`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Estimate {
    pub pos: [f64; 2],
    pub score: f32,
    pub lost: bool,
    pub off: f64,
}

/// Two estimates this close (view px) agree: the fused one is their mean.
const AGREE: f64 = 1.0;
/// What switching between the two passes costs, in the units of [`cost`]:
/// the fused path takes one pass for a run of frames, not a frame here and
/// there. Where they agree, switching is free.
const SWITCH: f64 = 0.6;

/// How bad an estimate looks on its own: a low score, lost, outside the
/// guide's box (the more, the further out: the rough pass is the authority
/// on where the subject roughly is).
fn cost(e: &Estimate) -> f64 {
    (1.0 - e.score.clamp(0.0, 1.0) as f64) + if e.lost { 1.0 } else { 0.0 } + 0.5 * e.off.min(2.0)
}

/// A stretch tracked from both ends: `a` from its near end, `b` from its far
/// end (both in frame order). Where they agree, the mean; where they don't,
/// the path through them that costs least (each frame's [`cost`], plus
/// [`SWITCH`] per change of pass): a pass that slipped onto a look-alike, or
/// lost the subject, gives way to the one that didn't, for as long as it did.
pub fn fuse(a: &[Estimate], b: &[Estimate]) -> Vec<Estimate> {
    let n = a.len().min(b.len());
    if n == 0 {
        return a.to_vec();
    }
    // Viterbi over two states (0: a, 1: b).
    let mut acc = [cost(&a[0]), cost(&b[0]) + SWITCH];
    let mut from: Vec<[u8; 2]> = Vec::with_capacity(n);
    from.push([0, 1]);
    let agree = |i: usize| !a[i].lost && !b[i].lost && (a[i].pos[0] - b[i].pos[0]).hypot(a[i].pos[1] - b[i].pos[1]) <= AGREE;
    for i in 1..n {
        let c = [cost(&a[i]), cost(&b[i])];
        let mut next = [0.0; 2];
        let mut back = [0u8; 2];
        let toll = if agree(i - 1) || agree(i) { 0.0 } else { SWITCH };
        for s in 0..2 {
            let (stay, switch) = (acc[s], acc[1 - s] + toll);
            (next[s], back[s]) = if stay <= switch { (stay + c[s], s as u8) } else { (switch + c[s], (1 - s) as u8) };
        }
        acc = next;
        from.push(back);
    }
    let mut state = if acc[0] <= acc[1] { 0 } else { 1 };
    let mut pick = vec![0u8; n];
    for i in (0..n).rev() {
        pick[i] = state as u8;
        state = from[i][state] as usize;
    }
    (0..n)
        .map(|i| {
            let (x, y) = (a[i], b[i]);
            if agree(i) {
                Estimate { pos: [(x.pos[0] + y.pos[0]) / 2.0, (x.pos[1] + y.pos[1]) / 2.0], score: x.score.max(y.score), ..x }
            } else if pick[i] == 0 {
                x
            } else {
                y
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(x: f64, score: f32, lost: bool, outside: bool) -> Estimate {
        Estimate { pos: [x, 0.0], score, lost, off: if outside { 1.0 } else { 0.0 } }
    }

    #[test]
    fn the_pass_that_slipped_gives_way_for_as_long_as_it_slipped() {
        // Forward slips onto a look-alike (x = 50, outside the guide) on frames 3..7; backward stays on the subject.
        let a: Vec<Estimate> = (0..10).map(|i| if (3..7).contains(&i) { e(50.0, 0.95, false, true) } else { e(i as f64, 0.95, false, false) }).collect();
        let b: Vec<Estimate> = (0..10).map(|i| e(i as f64 + 0.2, 0.93, false, false)).collect();
        let f = fuse(&a, &b);
        for (i, v) in f.iter().enumerate() {
            let want = if (3..7).contains(&i) { i as f64 + 0.2 } else { i as f64 + 0.1 };
            assert!((v.pos[0] - want).abs() < 1e-9, "frame {i}: {:?}", v.pos);
        }
        // Passes that disagree all along: one frame where the other is a
        // little better doesn't flip it (switching costs more than it saves)…
        let a2: Vec<Estimate> = (0..10).map(|i| e(i as f64, 0.9, false, false)).collect();
        let mut b2: Vec<Estimate> = (0..10).map(|i| e(i as f64 + 10.0, 0.85, false, false)).collect();
        b2[5] = e(15.0, 1.0, false, false);
        assert!(fuse(&a2, &b2).iter().zip(&a2).all(|(x, y)| x.pos == y.pos));
        // … but where they agree, switching is free: a better frame between agreeing ones is taken.
        let mut b3 = a2.clone();
        b3[5] = e(15.0, 1.0, false, false);
        let a3: Vec<Estimate> = (0..10).map(|i| if i == 5 { e(5.0, 0.7, false, true) } else { a2[i] }).collect();
        assert_eq!(fuse(&a3, &b3)[5].pos, [15.0, 0.0]);
        // Lost frames give way too.
        let lost: Vec<Estimate> = (0..10).map(|i| e(i as f64 + 20.0, 0.3, true, false)).collect();
        assert!(fuse(&lost, &b).iter().zip(&b).all(|(x, y)| x.pos == y.pos));
    }
}
