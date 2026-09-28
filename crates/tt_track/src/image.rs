//! Pixels for trackers: a decoded frame's luma, and float patches resampled
//! from it through a view (DESIGN §6.1: a frame source reads a rendition
//! through any view, at the size its consumer wants).
//!
//! Coordinates are continuous pixels everywhere (DESIGN §4): (0, 0) is the
//! top-left corner of the top-left pixel and pixel centres sit at +0.5, in
//! the frame, in views and in patches alike.

use tt_core::view::SpaceMap;

/// A decoded frame's luma: the Y plane of NV12, `width × height` bytes.
#[derive(Clone, Copy)]
pub struct Luma<'a> {
    pub data: &'a [u8],
    pub width: usize,
    pub height: usize,
}

impl Luma<'_> {
    /// Bilinear sample at a continuous point; the edge pixels extend outwards.
    pub fn sample(&self, x: f64, y: f64) -> f32 {
        let fx = (x - 0.5).clamp(0.0, (self.width - 1) as f64);
        let fy = (y - 0.5).clamp(0.0, (self.height - 1) as f64);
        let (x0, y0) = (fx as usize, fy as usize);
        let (x1, y1) = ((x0 + 1).min(self.width - 1), (y0 + 1).min(self.height - 1));
        let (tx, ty) = ((fx - x0 as f64) as f32, (fy - y0 as f64) as f32);
        let p = |x: usize, y: usize| self.data[y * self.width + x] as f32;
        let top = p(x0, y0) + (p(x1, y0) - p(x0, y0)) * tx;
        let bottom = p(x0, y1) + (p(x1, y1) - p(x0, y1)) * tx;
        top + (bottom - top) * ty
    }
}

/// Where a patch lies in a view: patch point `p` is view point
/// `origin + p / scale`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Grid {
    pub origin: [f64; 2],
    /// Patch pixels per view pixel.
    pub scale: f64,
}

impl Grid {
    pub fn to_view(&self, p: [f64; 2]) -> [f64; 2] {
        [self.origin[0] + p[0] / self.scale, self.origin[1] + p[1] / self.scale]
    }

    pub fn from_view(&self, v: [f64; 2]) -> [f64; 2] {
        [(v[0] - self.origin[0]) * self.scale, (v[1] - self.origin[1]) * self.scale]
    }
}

/// A float image, row-major.
#[derive(Clone, Debug)]
pub struct Patch {
    pub w: usize,
    pub h: usize,
    pub data: Vec<f32>,
}

impl Patch {
    pub fn at(&self, x: usize, y: usize) -> f32 {
        self.data[y * self.w + x]
    }

    /// Bilinear sample at a continuous patch point; the edge pixels extend outwards.
    pub fn sample(&self, x: f64, y: f64) -> f32 {
        let fx = (x - 0.5).clamp(0.0, (self.w - 1) as f64);
        let fy = (y - 0.5).clamp(0.0, (self.h - 1) as f64);
        let (x0, y0) = (fx as usize, fy as usize);
        let (x1, y1) = ((x0 + 1).min(self.w - 1), (y0 + 1).min(self.h - 1));
        let (tx, ty) = ((fx - x0 as f64) as f32, (fy - y0 as f64) as f32);
        let top = self.at(x0, y0) + (self.at(x1, y0) - self.at(x0, y0)) * tx;
        let bottom = self.at(x0, y1) + (self.at(x1, y1) - self.at(x0, y1)) * tx;
        top + (bottom - top) * ty
    }
}

/// Resample a `w × h` patch on `grid` through `map` (view → source) from a
/// frame of a rendition `k`× the source's size ([`resample_xy`] with the
/// same scale on both axes).
pub fn resample(luma: &Luma, k: f64, map: &SpaceMap, grid: Grid, w: usize, h: usize) -> Patch {
    resample_xy(luma, [k, k], map, grid, w, h)
}

/// Resample a `w × h` patch on `grid` through `map` (view → source) from a
/// frame of a rendition `k = [kx, ky]` × the source's size (per axis: a
/// proxy's width is rounded to even, so it isn't always scaled uniformly).
/// Patch pixels larger than the rendition's are averaged over their
/// footprint, so nothing aliases.
pub fn resample_xy(luma: &Luma, k: [f64; 2], map: &SpaceMap, grid: Grid, w: usize, h: usize) -> Patch {
    // Rendition pixels per patch pixel, per axis: 1 tap per rendition pixel, at most 8.
    let step = |k: f64| map.a * k / grid.scale;
    let taps = |k: f64| {
        let n = (step(k).ceil() as usize).clamp(1, 8);
        (0..n).map(|s| (s as f64 + 0.5) / n as f64).collect::<Vec<f64>>()
    };
    let (tx, ty) = (taps(k[0]), taps(k[1]));
    let norm = 1.0 / (tx.len() * ty.len()) as f32;
    // Patch point → rendition point is affine per axis: r = ((origin + p / scale) · a + b) · k.
    let (ax, ay) = (step(k[0]), step(k[1]));
    let bx = (grid.origin[0] * map.a + map.b[0]) * k[0];
    let by = (grid.origin[1] * map.a + map.b[1]) * k[1];
    let mut data = Vec::with_capacity(w * h);
    for j in 0..h {
        for i in 0..w {
            let mut acc = 0.0;
            for &sy in &ty {
                for &sx in &tx {
                    acc += luma.sample(bx + (i as f64 + sx) * ax, by + (j as f64 + sy) * ay);
                }
            }
            data.push(acc * norm);
        }
    }
    Patch { w, h, data }
}
