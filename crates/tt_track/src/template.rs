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
//! - **Search** around the prediction (a patch the guide's box sized) for the
//!   best normalized cross-correlation, with a gentle preference for the
//!   prediction so a look-alike elsewhere in the box doesn't win a tie.
//! - **Appearance:** the winning look is blended with the last frame's
//!   appearance of it (`adapt`), which follows slow changes without drifting.
//! - **Lost:** below `min_score`, the frame keeps the prediction (the guide
//!   carries the tracker through an occlusion or blur) and is flagged; the
//!   tracker re-locks as soon as a look comes back.

use crate::image::{Grid, Patch};
use crate::ncc::{Mask, Prior, Template, best_match};

/// An older tracker's template half-size, patch pixels (a 21 × 21 template).
pub const TEMPLATE_R: usize = 10;
/// A look's larger half-size in patch pixels (the patch scale is set so).
pub const LOOK_PX: f64 = 12.0;
/// How far from the prediction to search, patch pixels.
pub const REACH: f64 = 40.0;
/// Strength of the preference for the prediction (score units at `REACH`).
const PRIOR_WEIGHT: f32 = 0.1;
/// A match at least this good refreshes the remembered appearance.
pub const REFRESH_SCORE: f32 = 0.6;

/// The tracker's tuning (from the `Tracker` component).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub adapt: f32,
    pub min_score: f32,
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
}

impl LookTemplate {
    /// Cut from `patch` centred at patch point `c`.
    pub fn cut(patch: &Patch, c: [f64; 2], r: [usize; 2], mask: Option<Vec<u8>>) -> Option<Self> {
        let template = Template::cut_rect(patch, c, r, mask.as_deref().map(|m| mask_of(m)))?;
        Some(Self { template, r, mask })
    }

    /// The same look's appearance at patch point `c` of another frame.
    fn again(&self, patch: &Patch, c: [f64; 2]) -> Option<Template> {
        Template::cut_rect(patch, c, self.r, self.mask.as_deref().map(|m| mask_of(m)))
    }
}

fn mask_of(cells: &[u8]) -> Mask<'_> {
    let n = (cells.len() as f64).sqrt() as usize;
    Mask { cells, w: n, h: n }
}

pub struct TemplateTracker {
    looks: Vec<LookTemplate>,
    /// The last frame's appearance of the look that matched there.
    last: Option<(usize, Template)>,
    /// Position minus the guide's point, view pixels.
    offset: [f64; 2],
    settings: Settings,
}

impl TemplateTracker {
    /// An older tracker: one square look on the anchor frame's patch at view
    /// point `pos` (the guide's point there).
    pub fn seed(patch: &Patch, grid: Grid, pos: [f64; 2], settings: Settings) -> Option<Self> {
        let look = LookTemplate::cut(patch, grid.from_view(pos), [TEMPLATE_R, TEMPLATE_R], None)?;
        Some(Self { looks: vec![look], last: None, offset: [0.0, 0.0], settings })
    }

    /// Looks the user showed it, starting at `offset` from the guide's point (view px).
    pub fn with_looks(looks: Vec<LookTemplate>, offset: [f64; 2], settings: Settings) -> Option<Self> {
        (!looks.is_empty()).then_some(Self { looks, last: None, offset, settings })
    }

    /// Continue from a known position on a frame other than the anchor
    /// (resuming a job), where the match scored `score`: the first look's
    /// appearance there becomes the last one if it was a good match (a lost
    /// frame's position is only the guide's prediction: its look may be anything).
    pub fn resume(&mut self, patch: &Patch, grid: Grid, pos: [f64; 2], guide: [f64; 2], score: f32) {
        self.last = if score >= REFRESH_SCORE { self.looks[0].again(patch, grid.from_view(pos)).map(|t| (0, t)) } else { None };
        self.offset = [pos[0] - guide[0], pos[1] - guide[1]];
    }

    /// Track into the next frame. `guide` is the guide's point there (view px).
    pub fn step(&mut self, patch: &Patch, grid: Grid, guide: [f64; 2]) -> Step {
        let predicted = [guide[0] + self.offset[0], guide[1] + self.offset[1]];
        let p = grid.from_view(predicted);
        let window = [[p[0] - REACH, p[1] - REACH], [p[0] + REACH, p[1] + REACH]];
        let prior = Some(Prior { centre: p, radius: REACH, weight: PRIOR_WEIGHT });
        let mut best: Option<(usize, crate::ncc::Match)> = None;
        for (i, look) in self.looks.iter().enumerate() {
            let template = match &self.last {
                Some((j, last)) if *j == i && self.settings.adapt > 0.0 => Template::blend(&look.template, last, self.settings.adapt.min(1.0)),
                _ => look.template.clone(),
            };
            if let Some(m) = best_match(patch, &template, window, prior)
                && best.is_none_or(|(_, b)| m.score > b.score)
            {
                best = Some((i, m));
            }
        }
        match best {
            Some((i, m)) if m.score >= self.settings.min_score => {
                let pos = grid.to_view(m.pos);
                self.offset = [pos[0] - guide[0], pos[1] - guide[1]];
                if m.score >= REFRESH_SCORE
                    && let Some(t) = self.looks[i].again(patch, m.pos)
                {
                    self.last = Some((i, t));
                }
                Step { pos, score: m.score, lost: false, look: i }
            }
            other => Step { pos: predicted, score: other.map_or(0.0, |(_, m)| m.score.max(0.0)), lost: true, look: other.map_or(0, |(i, _)| i) },
        }
    }
}
