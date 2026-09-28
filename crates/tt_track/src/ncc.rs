//! Template matching by normalized cross-correlation: the score of a
//! placement is the correlation of the template with the patch pixels under
//! it, after removing each one's brightness and contrast (−1 … 1), so a
//! lighting change doesn't move the peak.
//!
//! The correlation is *centre-weighted* (a Gaussian over the template): a
//! box around a subject always holds some background, and the background
//! changes as the subject moves across it. Weighting keeps the subject in
//! charge of the score.

use crate::image::Patch;

/// The weighting's width, as a fraction of the template's half-size.
const SIGMA: f64 = 0.45;

/// A square template of side `2 r + 1`: values with zero weighted mean and
/// unit weighted norm, and the kernel `weight · value` that correlates them.
#[derive(Clone, Debug)]
pub struct Template {
    pub r: usize,
    pub data: Vec<f32>,
    kernel: Vec<f32>,
}

/// Gaussian weights over a `(2 r + 1)²` template, summing to 1.
fn weights(r: usize) -> Vec<f32> {
    let side = 2 * r + 1;
    let s2 = 2.0 * (SIGMA * r.max(1) as f64).powi(2);
    let mut w: Vec<f64> = (0..side * side)
        .map(|k| {
            let (i, j) = ((k % side) as f64 - r as f64, (k / side) as f64 - r as f64);
            (-(i * i + j * j) / s2).exp()
        })
        .collect();
    let sum: f64 = w.iter().sum();
    w.iter_mut().for_each(|v| *v /= sum);
    w.into_iter().map(|v| v as f32).collect()
}

/// Below this standard deviation (grey levels) a template is flat: there is
/// nothing in it to follow.
const FLAT: f32 = 0.5;

impl Template {
    /// Cut from `patch` with its centre pixel's centre at patch point `c`
    /// (bilinear, so `c` may be fractional). None if the cut is flat.
    pub fn cut(patch: &Patch, c: [f64; 2], r: usize) -> Option<Template> {
        let side = 2 * r + 1;
        let mut data = Vec::with_capacity(side * side);
        for j in 0..side {
            for i in 0..side {
                data.push(patch.sample(c[0] + i as f64 - r as f64, c[1] + j as f64 - r as f64));
            }
        }
        Self::normalized(r, data)
    }

    fn normalized(r: usize, mut data: Vec<f32>) -> Option<Template> {
        let w = weights(r);
        let mean: f32 = data.iter().zip(&w).map(|(v, w)| v * w).sum();
        data.iter_mut().for_each(|v| *v -= mean);
        let sd = data.iter().zip(&w).map(|(v, w)| w * v * v).sum::<f32>().sqrt();
        if sd < FLAT {
            return None;
        }
        data.iter_mut().for_each(|v| *v /= sd);
        let kernel = data.iter().zip(&w).map(|(v, w)| v * w).collect();
        Some(Template { r, data, kernel })
    }

    /// `(1 − t) · a + t · b`, renormalized: an appearance between the two.
    pub fn blend(a: &Template, b: &Template, t: f32) -> Template {
        let data = a.data.iter().zip(&b.data).map(|(x, y)| (1.0 - t) * x + t * y).collect();
        // Both are unit-norm; a blend of two flat-free templates is flat only if they cancel.
        Self::normalized(a.r, data).unwrap_or_else(|| a.clone())
    }

    fn side(&self) -> usize {
        2 * self.r + 1
    }
}

/// The best placement of a template.
#[derive(Clone, Copy, Debug)]
pub struct Match {
    /// Patch point of the template's centre (subpixel).
    pub pos: [f64; 2],
    /// Normalized cross-correlation there (−1 … 1).
    pub score: f32,
}

/// A preference for placements near a predicted centre: `weight · (d / radius)²`
/// is subtracted when ranking placements (the reported score is the raw one).
#[derive(Clone, Copy, Debug)]
pub struct Prior {
    pub centre: [f64; 2],
    pub radius: f64,
    pub weight: f32,
}

/// Search `patch` for `t`, over template centres inside `window` (patch
/// points `[min, max]`, clipped to where the template fits).
pub fn best_match(patch: &Patch, t: &Template, window: [[f64; 2]; 2], prior: Option<Prior>) -> Option<Match> {
    let (side, r) = (t.side(), t.r as f64);
    if patch.w < side || patch.h < side {
        return None;
    }
    // Top-left placement u ↔ centre u + r + 0.5.
    let lo = |c: f64| (c - r - 0.5).ceil().max(0.0) as usize;
    let hi = |c: f64, n: usize| ((c - r - 0.5).floor()).min((n - side) as f64);
    let (u0, v0) = (lo(window[0][0]), lo(window[0][1]));
    let (u1, v1) = (hi(window[1][0], patch.w), hi(window[1][1], patch.h));
    if u1 < u0 as f64 || v1 < v0 as f64 {
        return None;
    }
    let (u1, v1) = (u1 as usize, v1 as usize);
    let (nu, nv) = (u1 - u0 + 1, v1 - v0 + 1);
    let w = weights(t.r);
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
            for j in 0..side {
                let row = &patch.data[(v + j) * patch.w + u..(v + j) * patch.w + u + side];
                let wrow = &w[j * side..(j + 1) * side];
                let krow = &t.kernel[j * side..(j + 1) * side];
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
            scores[dv * nu + du] = (cov / var.sqrt()).min(1.0);
        }
    }

    let centre = |du: usize, dv: usize| [(u0 + du) as f64 + r + 0.5, (v0 + dv) as f64 + r + 0.5];
    let rank = |du: usize, dv: usize| {
        let s = scores[dv * nu + du];
        match prior {
            Some(p) => {
                let c = centre(du, dv);
                let d2 = ((c[0] - p.centre[0]).powi(2) + (c[1] - p.centre[1]).powi(2)) / p.radius.max(1e-6).powi(2);
                s - p.weight * d2 as f32
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
