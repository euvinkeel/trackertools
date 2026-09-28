//! The template tracker: follows the patch it was seeded on, frame to frame,
//! inside the region its guide (a sketch: the rough pass) says the subject is.
//!
//! - **Seed:** at the anchor frame, the template is cut around the guide's
//!   point. The tracker's point is *defined* by that appearance, so the
//!   output there is the guide's point exactly.
//! - **Predict:** the subject keeps its offset from the guide, so the next
//!   position is the guide's point plus the last offset. The hand already
//!   followed the motion; the tracker only corrects what the hand got wrong.
//! - **Search** around the prediction (a patch the guide's box sized) for the
//!   best normalized cross-correlation, with a gentle preference for the
//!   prediction so a look-alike elsewhere in the box doesn't win a tie.
//! - **Appearance:** matches use a blend of the anchor's look and the last
//!   frame's (`adapt`), which follows slow changes without drifting off.
//! - **Lost:** below `min_score`, the frame keeps the prediction (the guide
//!   carries the tracker through an occlusion or blur) and is marked low
//!   confidence; the tracker re-locks as soon as the look comes back.

use crate::image::{Grid, Patch};
use crate::ncc::{Prior, Template, best_match};

/// Template half-size, patch pixels (a 21 × 21 template).
pub const TEMPLATE_R: usize = 10;
/// How far from the prediction to search, patch pixels.
pub const REACH: f64 = 40.0;
/// Strength of the preference for the prediction (score units at `REACH`).
const PRIOR_WEIGHT: f32 = 0.1;
/// A match at least this good refreshes the remembered appearance.
const REFRESH_SCORE: f32 = 0.6;

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
}

pub struct TemplateTracker {
    anchor: Template,
    last: Option<Template>,
    /// Position minus the guide's point, view pixels.
    offset: [f64; 2],
    settings: Settings,
}

impl TemplateTracker {
    /// Seed on the anchor frame's patch at view point `pos` (the guide's point there).
    pub fn seed(patch: &Patch, grid: Grid, pos: [f64; 2], settings: Settings) -> Option<Self> {
        let anchor = Template::cut(patch, grid.from_view(pos), TEMPLATE_R)?;
        Some(Self { anchor, last: None, offset: [0.0, 0.0], settings })
    }

    /// Continue from a known position on a frame other than the anchor
    /// (resuming a job): that frame's look becomes the last appearance.
    pub fn resume(&mut self, patch: &Patch, grid: Grid, pos: [f64; 2], guide: [f64; 2]) {
        self.last = Template::cut(patch, grid.from_view(pos), TEMPLATE_R);
        self.offset = [pos[0] - guide[0], pos[1] - guide[1]];
    }

    /// Track into the next frame. `guide` is the guide's point there (view px).
    pub fn step(&mut self, patch: &Patch, grid: Grid, guide: [f64; 2]) -> Step {
        let predicted = [guide[0] + self.offset[0], guide[1] + self.offset[1]];
        let p = grid.from_view(predicted);
        let template = match &self.last {
            Some(last) if self.settings.adapt > 0.0 => Template::blend(&self.anchor, last, self.settings.adapt.min(1.0)),
            _ => self.anchor.clone(),
        };
        let window = [[p[0] - REACH, p[1] - REACH], [p[0] + REACH, p[1] + REACH]];
        let found = best_match(patch, &template, window, Some(Prior { centre: p, radius: REACH, weight: PRIOR_WEIGHT }));
        match found {
            Some(m) if m.score >= self.settings.min_score => {
                let pos = grid.to_view(m.pos);
                self.offset = [pos[0] - guide[0], pos[1] - guide[1]];
                if m.score >= REFRESH_SCORE
                    && let Some(t) = Template::cut(patch, m.pos, TEMPLATE_R)
                {
                    self.last = Some(t);
                }
                Step { pos, score: m.score, lost: false }
            }
            other => Step { pos: predicted, score: other.map_or(0.0, |m| m.score.max(0.0)), lost: true },
        }
    }
}
