//! Template matching by normalized cross-correlation: the score of a
//! placement is the correlation of the template with the patch pixels under
//! it, after removing each one's brightness and contrast (−1 … 1), so a
//! lighting change doesn't move the peak.
//!
//! Correlation alone sees only the *shape* of light and dark: a dim, nearly
//! flat patch with a similar gradient scores like a bright white cursor. So
//! the score is also scaled down where the pixels under the template differ
//! a lot in contrast from the template's (more than 2× either way), and, for
//! a painted template, in brightness: those pixels are the subject's own,
//! and a screen recording doesn't relight a cursor ([`photometric`]).
//!
//! How alike is alike enough is the tracker's to say ([`Tolerance`], per
//! tracker in the inspector): the contrast and brightness slack, and
//! whether the colour must agree too (chroma: a white cursor and a yellow
//! marker of the same shape are nearly twins in luma). The defaults are the
//! behaviour from before the options existed.
//!
//! The correlation is *weighted*. By default the weights are a Gaussian over
//! the template (centre-weighting): a box around a subject always holds some
//! background, and the background changes as the subject moves across it.
//! A painted mask replaces them: only the pixels that are the subject count
//! (a cursor over whatever is behind it).

use crate::image::Patch;

/// The centre-weighting's width, as a fraction of the template's half-size.
const SIGMA: f64 = 0.45;

/// A rectangular template of `(2 rx + 1) × (2 ry + 1)` pixels: values with
/// zero weighted mean and unit weighted norm, its weights (summing to 1),
/// and the kernel `weight · value` that correlates them.
#[derive(Clone, Debug)]
pub struct Template {
    pub rx: usize,
    pub ry: usize,
    pub data: Vec<f32>,
    weights: Vec<f32>,
    kernel: Vec<f32>,
    /// The weighted mean and spread of the pixels it was cut from (grey levels).
    pub mean: f32,
    pub sd: f32,
    /// Weighted by a painted mask (its pixels are the subject's own).
    pub masked: bool,
    pub tolerance: Tolerance,
    /// The weighted mean colour (chroma U, V) of the pixels it was cut from,
    /// if the tolerance asks for colour and the patch had it.
    pub colour: Option<[f32; 2]>,
}

/// How alike a placement must be to count as the template (per tracker).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tolerance {
    /// The contrast may differ by this factor either way for free, and
    /// counts less in proportion beyond it.
    pub contrast: f32,
    /// A painted template's brightness may differ by this many of its
    /// spreads for free; the score is gone [`BRIGHTNESS_FADE`] spreads further.
    pub brightness: f32,
    /// Compare the colour too: its mean may differ by this much (chroma
    /// levels, 0–255 scale) for free, and the score is gone at twice it.
    pub colour: Option<f32>,
}

impl Default for Tolerance {
    fn default() -> Self {
        Self { contrast: 1.0 / CONTRAST_SLACK, brightness: BRIGHTNESS_SLACK, colour: None }
    }
}

/// Which pixels of a rectangle are the subject: a `w × h` grid of cells
/// (0–255) laid over it, row-major.
#[derive(Clone, Copy, Debug)]
pub struct Mask<'a> {
    pub cells: &'a [u8],
    pub w: usize,
    pub h: usize,
}

impl Mask<'_> {
    /// The cell under relative position `(u, v)` in `[0, 1)²`, 0–1.
    fn at(&self, u: f64, v: f64) -> f32 {
        let i = ((u * self.w as f64) as usize).min(self.w - 1);
        let j = ((v * self.h as f64) as usize).min(self.h - 1);
        self.cells[j * self.w + i] as f32 / 255.0
    }
}

/// Weights over a `(2 rx + 1) × (2 ry + 1)` template, summing to 1: the
/// mask's where given, else a Gaussian. None if the mask is empty.
fn weights(rx: usize, ry: usize, mask: Option<Mask>) -> Option<Vec<f32>> {
    let (sx, sy) = (2 * rx + 1, 2 * ry + 1);
    let (gx, gy) = (2.0 * (SIGMA * rx.max(1) as f64).powi(2), 2.0 * (SIGMA * ry.max(1) as f64).powi(2));
    let mut w: Vec<f64> = (0..sx * sy)
        .map(|k| {
            let (i, j) = (k % sx, k / sx);
            match mask {
                Some(m) => m.at((i as f64 + 0.5) / sx as f64, (j as f64 + 0.5) / sy as f64) as f64,
                None => {
                    let (x, y) = (i as f64 - rx as f64, j as f64 - ry as f64);
                    (-(x * x / gx) - y * y / gy).exp()
                }
            }
        })
        .collect();
    let sum: f64 = w.iter().sum();
    if sum <= 1e-9 {
        return None;
    }
    w.iter_mut().for_each(|v| *v /= sum);
    Some(w.into_iter().map(|v| v as f32).collect())
}

/// Placements scoring below this without colour aren't checked for colour.
const COLOUR_FROM: f32 = 0.4;
/// Windows with more placements than this are searched coarse to fine,
/// refining around this many of the best coarse ones.
const COARSE_ABOVE: usize = 81 * 81;
const COARSE_PICKS: usize = 3;
/// Below this standard deviation (grey levels) a template is flat: there is
/// nothing in it to follow.
const FLAT: f32 = 0.5;
/// Contrast within this ratio of the template's (either way) costs nothing.
const CONTRAST_SLACK: f32 = 0.5;
/// A painted template's brightness may differ by this many of its spreads
/// for free, and the score is gone this many spreads further.
const BRIGHTNESS_SLACK: f32 = 1.0;
pub const BRIGHTNESS_FADE: f32 = 2.0;

/// How much of a placement's correlation counts (0 … 1), from the weighted
/// mean and spread of the pixels under `t`: 1 while they are like the
/// template's, less as the contrast differs by more than 2× either way (in
/// proportion), or a painted template's brightness by more than one of its
/// spreads.
/// (The slack and fade are the template's [`Tolerance`].)
pub fn photometric(t: &Template, mean: f32, sd: f32) -> f32 {
    let ratio = sd.min(t.sd) / sd.max(t.sd).max(1e-6);
    let contrast = (ratio * t.tolerance.contrast.max(1.0)).min(1.0);
    let brightness = if t.masked {
        let d = (mean - t.mean).abs() / t.sd.max(FLAT);
        (1.0 - (d - t.tolerance.brightness.max(0.0)).max(0.0) / BRIGHTNESS_FADE).clamp(0.0, 1.0)
    } else {
        1.0
    };
    contrast * brightness
}

/// How much of a placement's score counts for its colour: 1 while the mean
/// colour under the template is within the tolerance of the template's,
/// down to 0 at twice it.
pub fn chromatic(t: &Template, colour: [f32; 2]) -> f32 {
    match (t.colour, t.tolerance.colour) {
        (Some(c), Some(slack)) => {
            let d = (colour[0] - c[0]).hypot(colour[1] - c[1]);
            let slack = slack.max(0.5);
            (1.0 - (d - slack).max(0.0) / slack).clamp(0.0, 1.0)
        }
        _ => 1.0,
    }
}

impl Template {
    /// A square, centre-weighted template cut from `patch` with its centre
    /// pixel's centre at patch point `c` (bilinear, so `c` may be
    /// fractional). None if the cut is flat.
    pub fn cut(patch: &Patch, c: [f64; 2], r: usize) -> Option<Template> {
        Self::cut_rect(patch, c, [r, r], None)
    }

    /// A `(2 rx + 1) × (2 ry + 1)` template, weighted by `mask` if given.
    pub fn cut_rect(patch: &Patch, c: [f64; 2], r: [usize; 2], mask: Option<Mask>) -> Option<Template> {
        Self::cut_with(patch, c, r, mask, Tolerance::default())
    }

    /// [`Template::cut_rect`], matched with `tolerance`: its colour and edges
    /// are cut too if it asks for them (and the patch has them).
    pub fn cut_with(patch: &Patch, c: [f64; 2], [rx, ry]: [usize; 2], mask: Option<Mask>, tolerance: Tolerance) -> Option<Template> {
        let grid = |plane: &dyn Fn(f64, f64) -> f32| {
            let mut v = Vec::with_capacity((2 * rx + 1) * (2 * ry + 1));
            for j in 0..2 * ry + 1 {
                for i in 0..2 * rx + 1 {
                    v.push(plane(c[0] + i as f64 - rx as f64, c[1] + j as f64 - ry as f64));
                }
            }
            v
        };
        let data = grid(&|x, y| patch.sample(x, y));
        let masked = mask.is_some();
        let weights = weights(rx, ry, mask)?;
        let colour = tolerance.colour.and(patch.colour.as_ref()).map(|uv| {
            let [u, v] = [&uv[0], &uv[1]].map(|p| grid(&|x, y| patch.sample_plane(p, x, y)).iter().zip(&weights).map(|(a, w)| a * w).sum::<f32>());
            [u, v]
        });
        let mut t = Self::normalized(rx, ry, data, weights, masked)?;
        (t.tolerance, t.colour) = (tolerance, colour);
        Some(t)
    }

    fn normalized(rx: usize, ry: usize, mut data: Vec<f32>, weights: Vec<f32>, masked: bool) -> Option<Template> {
        let mean: f32 = data.iter().zip(&weights).map(|(v, w)| v * w).sum();
        data.iter_mut().for_each(|v| *v -= mean);
        let sd = data.iter().zip(&weights).map(|(v, w)| w * v * v).sum::<f32>().sqrt();
        if sd < FLAT {
            return None;
        }
        data.iter_mut().for_each(|v| *v /= sd);
        let kernel = data.iter().zip(&weights).map(|(v, w)| v * w).collect();
        Some(Template { rx, ry, data, weights, kernel, mean, sd, masked, tolerance: Tolerance::default(), colour: None })
    }

    /// `(1 − t) · a + t · b`, renormalized: an appearance between the two
    /// (with `a`'s weights; both must be the same size).
    pub fn blend(a: &Template, b: &Template, t: f32) -> Template {
        if (a.rx, a.ry) != (b.rx, b.ry) {
            return a.clone();
        }
        let data = a.data.iter().zip(&b.data).map(|(x, y)| (1.0 - t) * x + t * y).collect();
        // Both are unit-norm; a blend of two flat-free templates is flat only if they cancel.
        match Self::normalized(a.rx, a.ry, data, a.weights.clone(), a.masked) {
            Some(mut m) => {
                // (Normalized data have mean 0 and spread 1: the blend's own are the pixels'.)
                (m.mean, m.sd) = ((1.0 - t) * a.mean + t * b.mean, (1.0 - t) * a.sd + t * b.sd);
                m.tolerance = a.tolerance;
                m.colour = match (a.colour, b.colour) {
                    (Some(x), Some(y)) => Some([(1.0 - t) * x[0] + t * y[0], (1.0 - t) * x[1] + t * y[1]]),
                    (x, _) => x,
                };
                m
            }
            None => a.clone(),
        }
    }

    fn size(&self) -> (usize, usize) {
        (2 * self.rx + 1, 2 * self.ry + 1)
    }
}

/// The best placement of a template.
#[derive(Clone, Copy, Debug)]
pub struct Match {
    /// Patch point of the template's centre (subpixel).
    pub pos: [f64; 2],
    /// Normalized cross-correlation there (−1 … 1), times [`photometric`].
    pub score: f32,
}

/// A preference for placements near a predicted centre: `weight · (d / radius)²`
/// is subtracted when ranking placements, up to `weight` at `radius` and
/// beyond (the reported score is the raw one).
#[derive(Clone, Copy, Debug)]
pub struct Prior {
    pub centre: [f64; 2],
    pub radius: f64,
    pub weight: f32,
    /// A box `[x, y, left, top, right, bottom]` (patch points, for the
    /// template's centre) placements should be in, and what being outside
    /// it costs (at its half-size out and beyond; in proportion closer).
    pub within: Option<([f64; 6], f32)>,
}

/// Search `patch` for `t`, over template centres inside `window` (patch
/// points `[min, max]`, clipped to where the template fits).
pub fn best_match(patch: &Patch, t: &Template, window: [[f64; 2]; 2], prior: Option<Prior>) -> Option<Match> {
    let ((sx, sy), (rx, ry)) = (t.size(), (t.rx as f64, t.ry as f64));
    if patch.w < sx || patch.h < sy {
        return None;
    }
    // Top-left placement u ↔ centre u + r + 0.5.
    let lo = |c: f64, r: f64| (c - r - 0.5).ceil().max(0.0) as usize;
    let hi = |c: f64, r: f64, n: usize, side: usize| ((c - r - 0.5).floor()).min((n - side) as f64);
    let (u0, v0) = (lo(window[0][0], rx), lo(window[0][1], ry));
    let (u1, v1) = (hi(window[1][0], rx, patch.w, sx), hi(window[1][1], ry, patch.h, sy));
    if u1 < u0 as f64 || v1 < v0 as f64 {
        return None;
    }
    let (u1, v1) = (u1 as usize, v1 as usize);
    let (nu, nv) = (u1 - u0 + 1, v1 - v0 + 1);
    // Pixels relative to their mean, so the variance below doesn't cancel away in f32.
    let reference = patch.data.iter().sum::<f32>() / patch.data.len() as f32;

    let colour = t.colour.and(patch.colour.as_ref()).filter(|_| t.tolerance.colour.is_some());
    // A placement's score (0 where the pixels under it are flat).
    let score_at = |du: usize, dv: usize| {
        let (u, v) = (u0 + du, v0 + dv);
        // Weighted mean and spread of the pixels under the template, and
        // their correlation with it (its weighted mean is zero, so
        // Σ kernel · p = the covariance).
        let (mut mean, mut sq, mut cov) = (0.0f32, 0.0f32, 0.0f32);
        for j in 0..sy {
            let row = &patch.data[(v + j) * patch.w + u..(v + j) * patch.w + u + sx];
            let wrow = &t.weights[j * sx..(j + 1) * sx];
            let krow = &t.kernel[j * sx..(j + 1) * sx];
            for ((p, w), k) in row.iter().zip(wrow).zip(krow) {
                let p = p - reference;
                mean += w * p;
                sq += w * p * p;
                cov += k * p;
            }
        }
        let var = sq - mean * mean;
        if var < FLAT * FLAT {
            return 0.0; // flat under the template: no evidence either way
        }
        let sd = var.sqrt();
        let score = (cov / sd).min(1.0) * photometric(t, mean + reference, sd);
        // The colour under it, against the template's (only where the rest
        // is good enough to matter: it can only lower the score).
        let colour = match colour {
            Some(uv) if score >= COLOUR_FROM => {
                let (mut cu, mut cv) = (0.0f32, 0.0f32);
                for j in 0..sy {
                    let at = (v + j) * patch.w + u;
                    for (i, w) in t.weights[j * sx..(j + 1) * sx].iter().enumerate() {
                        cu += w * uv[0][at + i];
                        cv += w * uv[1][at + i];
                    }
                }
                chromatic(t, [cu, cv])
            }
            _ => 1.0,
        };
        score * colour
    };

    let centre = |du: usize, dv: usize| [(u0 + du) as f64 + rx + 0.5, (v0 + dv) as f64 + ry + 0.5];
    let rank = |s: f32, du: usize, dv: usize| match prior {
        Some(p) => {
            let c = centre(du, dv);
            let d2 = ((c[0] - p.centre[0]).powi(2) + (c[1] - p.centre[1]).powi(2)) / p.radius.max(1e-6).powi(2);
            let off = p.within.map_or(0.0, |(b, w)| w * crate::template::off_box(c, &b).min(1.0) as f32);
            s - p.weight * d2.min(1.0) as f32 - off
        }
        None => s,
    };
    // Scores of the placements looked at (NaN: not looked at). A large
    // window is searched coarse to fine: every other placement each way,
    // then all of them around the best few.
    let mut scores = vec![f32::NAN; nu * nv];
    let coarse = nu * nv > COARSE_ABOVE;
    let step = if coarse { 2 } else { 1 };
    for dv in (0..nv).step_by(step) {
        for du in (0..nu).step_by(step) {
            scores[dv * nu + du] = score_at(du, dv);
        }
    }
    if coarse {
        // The best few coarse placements, apart from each other.
        let mut picks: Vec<(f32, usize, usize)> = Vec::new();
        let mut all: Vec<(f32, usize, usize)> = (0..nv).step_by(2).flat_map(|dv| (0..nu).step_by(2).map(move |du| (du, dv))).map(|(du, dv)| (rank(scores[dv * nu + du], du, dv), du, dv)).collect();
        all.sort_by(|a, b| b.0.total_cmp(&a.0));
        for (k, du, dv) in all {
            if picks.len() == COARSE_PICKS {
                break;
            }
            if picks.iter().all(|(_, pu, pv)| pu.abs_diff(du).max(pv.abs_diff(dv)) > 3) {
                picks.push((k, du, dv));
            }
        }
        for (_, du, dv) in picks {
            for v in dv.saturating_sub(2)..(dv + 3).min(nv) {
                for u in du.saturating_sub(2)..(du + 3).min(nu) {
                    if scores[v * nu + u].is_nan() {
                        scores[v * nu + u] = score_at(u, v);
                    }
                }
            }
        }
    }
    let (mut bu, mut bv, mut best) = (0, 0, f32::NEG_INFINITY);
    for dv in 0..nv {
        for du in 0..nu {
            let s = scores[dv * nu + du];
            if !s.is_nan() && rank(s, du, dv) > best {
                (bu, bv, best) = (du, dv, rank(s, du, dv));
            }
        }
    }
    if best == f32::NEG_INFINITY {
        return None;
    }
    // Subpixel: a parabola through the peak and its neighbours, per axis.
    let s = |du: usize, dv: usize| scores[dv * nu + du] as f64;
    let vertex = |l: f64, c: f64, r: f64| {
        let den = l - 2.0 * c + r;
        if den < 0.0 { ((l - r) / (2.0 * den)).clamp(-0.5, 0.5) } else { 0.0 }
    };
    let (sl, sr, su, sd) = (bu > 0 && !s(bu - 1, bv).is_nan(), bu + 1 < nu && !s((bu + 1).min(nu - 1), bv).is_nan(), bv > 0 && !s(bu, bv.saturating_sub(1)).is_nan(), bv + 1 < nv && !s(bu, (bv + 1).min(nv - 1)).is_nan());
    let dx = if sl && sr { vertex(s(bu - 1, bv), s(bu, bv), s(bu + 1, bv)) } else { 0.0 };
    let dy = if su && sd { vertex(s(bu, bv - 1), s(bu, bv), s(bu, bv + 1)) } else { 0.0 };
    let c = centre(bu, bv);
    Some(Match { pos: [c[0] + dx, c[1] + dy], score: scores[bv * nu + bu] })
}

/// Lucas–Kanade refinement of a placement: from patch point `pos` (the
/// template's centre, e.g. [`best_match`]'s), Gauss–Newton steps on the
/// weighted squared difference between the pixels under the template and a
/// gain and offset of it (so, like the correlation, it ignores brightness and
/// contrast), moving the centre by subpixel amounts: it follows the image's
/// gradients where the correlation's peak isn't a parabola. Returns `pos`
/// unchanged if it would move by more than a pixel, or if the pixels under
/// the template are flat.
///
/// (Measured: comparing both sides through a matched blur removes the pull
/// toward whole pixels that bilinear resampling gives it, exact on an ideal
/// square between pixels, but it was worse on both encoded fixtures, e.g.
/// the sprite's placed look 0.072 → 0.100–0.109 px median; screen cursors
/// sit on whole pixels. Kept plain.)
pub fn refine(patch: &Patch, t: &Template, pos: [f64; 2]) -> [f64; 2] {
    let (sx, sy) = t.size();
    let at = |d: [f64; 2], i: usize, j: usize| [d[0] + i as f64 - t.rx as f64, d[1] + j as f64 - t.ry as f64];
    let mut d = pos;
    // Gain and offset: start from the pixels' own spread and mean.
    let (mut mean, mut sq) = (0.0f64, 0.0f64);
    for j in 0..sy {
        for i in 0..sx {
            let q = at(d, i, j);
            let (w, p) = (t.weights[j * sx + i] as f64, patch.sample(q[0], q[1]) as f64);
            mean += w * p;
            sq += w * p * p;
        }
    }
    let sd = (sq - mean * mean).max(0.0).sqrt();
    if sd < FLAT as f64 {
        return pos;
    }
    let (mut gain, mut offset) = (sd, mean);
    for _ in 0..6 {
        // Normal equations for (dx, dy, gain, offset).
        let mut a = [[0.0f64; 4]; 4];
        let mut b = [0.0f64; 4];
        for j in 0..sy {
            for i in 0..sx {
                let k = j * sx + i;
                let w = t.weights[k] as f64;
                if w <= 0.0 {
                    continue;
                }
                let q = at(d, i, j);
                let p = patch.sample(q[0], q[1]) as f64;
                let gx = (patch.sample(q[0] + 0.5, q[1]) - patch.sample(q[0] - 0.5, q[1])) as f64;
                let gy = (patch.sample(q[0], q[1] + 0.5) - patch.sample(q[0], q[1] - 0.5)) as f64;
                let tv = t.data[k] as f64;
                let r = p - (gain * tv + offset);
                // r(d + δ) ≈ r + ∇p·δ − tv·Δgain − Δoffset
                let jac = [gx, gy, -tv, -1.0];
                for u in 0..4 {
                    b[u] -= w * jac[u] * r;
                    for v in 0..4 {
                        a[u][v] += w * jac[u] * jac[v];
                    }
                }
            }
        }
        let Some(step) = solve4(a, b) else { return pos };
        d = [d[0] + step[0], d[1] + step[1]];
        gain += step[2];
        offset += step[3];
        if (d[0] - pos[0]).abs() > 1.0 || (d[1] - pos[1]).abs() > 1.0 {
            return pos;
        }
        if step[0].abs() < 1e-3 && step[1].abs() < 1e-3 {
            break;
        }
    }
    d
}

/// Solve a 4 × 4 system by Gaussian elimination with partial pivoting.
fn solve4(mut a: [[f64; 4]; 4], mut b: [f64; 4]) -> Option<[f64; 4]> {
    for c in 0..4 {
        let p = (c..4).max_by(|x, y| a[*x][c].abs().total_cmp(&a[*y][c].abs()))?;
        if a[p][c].abs() < 1e-9 {
            return None;
        }
        a.swap(c, p);
        b.swap(c, p);
        for r in c + 1..4 {
            let f = a[r][c] / a[c][c];
            let pivot = a[c];
            for (x, p) in a[r].iter_mut().zip(pivot).skip(c) {
                *x -= f * p;
            }
            b[r] -= f * b[c];
        }
    }
    let mut x = [0.0; 4];
    for c in (0..4).rev() {
        x[c] = (b[c] - (c + 1..4).map(|k| a[c][k] * x[k]).sum::<f64>()) / a[c][c];
    }
    Some(x)
}
