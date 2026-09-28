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

/// Below this standard deviation (grey levels) a template is flat: there is
/// nothing in it to follow.
const FLAT: f32 = 0.5;
/// Contrast within this ratio of the template's (either way) costs nothing.
const CONTRAST_SLACK: f32 = 0.5;
/// A painted template's brightness may differ by this many of its spreads
/// for free, and the score is gone this many spreads further.
const BRIGHTNESS_SLACK: f32 = 1.0;
const BRIGHTNESS_FADE: f32 = 2.0;

/// How much of a placement's correlation counts (0 … 1), from the weighted
/// mean and spread of the pixels under `t`: 1 while they are like the
/// template's, less as the contrast differs by more than 2× either way (in
/// proportion), or a painted template's brightness by more than one of its
/// spreads.
pub fn photometric(t: &Template, mean: f32, sd: f32) -> f32 {
    let ratio = sd.min(t.sd) / sd.max(t.sd).max(1e-6);
    let contrast = (ratio / CONTRAST_SLACK).min(1.0);
    let brightness = if t.masked {
        let d = (mean - t.mean).abs() / t.sd.max(FLAT);
        (1.0 - (d - BRIGHTNESS_SLACK).max(0.0) / BRIGHTNESS_FADE).clamp(0.0, 1.0)
    } else {
        1.0
    };
    contrast * brightness
}

impl Template {
    /// A square, centre-weighted template cut from `patch` with its centre
    /// pixel's centre at patch point `c` (bilinear, so `c` may be
    /// fractional). None if the cut is flat.
    pub fn cut(patch: &Patch, c: [f64; 2], r: usize) -> Option<Template> {
        Self::cut_rect(patch, c, [r, r], None)
    }

    /// A `(2 rx + 1) × (2 ry + 1)` template, weighted by `mask` if given.
    pub fn cut_rect(patch: &Patch, c: [f64; 2], [rx, ry]: [usize; 2], mask: Option<Mask>) -> Option<Template> {
        let mut data = Vec::with_capacity((2 * rx + 1) * (2 * ry + 1));
        for j in 0..2 * ry + 1 {
            for i in 0..2 * rx + 1 {
                data.push(patch.sample(c[0] + i as f64 - rx as f64, c[1] + j as f64 - ry as f64));
            }
        }
        let masked = mask.is_some();
        Self::normalized(rx, ry, data, weights(rx, ry, mask)?, masked)
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
        Some(Template { rx, ry, data, weights, kernel, mean, sd, masked })
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

    let mut scores = vec![0.0f32; nu * nv];
    for dv in 0..nv {
        for du in 0..nu {
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
                continue; // flat under the template: no evidence either way
            }
            let sd = var.sqrt();
            scores[dv * nu + du] = (cov / sd).min(1.0) * photometric(t, mean + reference, sd);
        }
    }

    let centre = |du: usize, dv: usize| [(u0 + du) as f64 + rx + 0.5, (v0 + dv) as f64 + ry + 0.5];
    let rank = |du: usize, dv: usize| {
        let s = scores[dv * nu + du];
        match prior {
            Some(p) => {
                let c = centre(du, dv);
                let d2 = ((c[0] - p.centre[0]).powi(2) + (c[1] - p.centre[1]).powi(2)) / p.radius.max(1e-6).powi(2);
                s - p.weight * d2.min(1.0) as f32
            }
            None => s,
        }
    };
    let (mut bu, mut bv, mut best) = (0, 0, f32::NEG_INFINITY);
    for dv in 0..nv {
        for du in 0..nu {
            let k = rank(du, dv);
            if k > best {
                (bu, bv, best) = (du, dv, k);
            }
        }
    }
    // Subpixel: a parabola through the peak and its neighbours, per axis.
    let s = |du: usize, dv: usize| scores[dv * nu + du] as f64;
    let vertex = |l: f64, c: f64, r: f64| {
        let den = l - 2.0 * c + r;
        if den < 0.0 { ((l - r) / (2.0 * den)).clamp(-0.5, 0.5) } else { 0.0 }
    };
    let dx = if bu > 0 && bu + 1 < nu { vertex(s(bu - 1, bv), s(bu, bv), s(bu + 1, bv)) } else { 0.0 };
    let dy = if bv > 0 && bv + 1 < nv { vertex(s(bu, bv - 1), s(bu, bv), s(bu, bv + 1)) } else { 0.0 };
    let c = centre(bu, bv);
    Some(Match { pos: [c[0] + dx, c[1] + dy], score: scores[bv * nu + bu] })
}
