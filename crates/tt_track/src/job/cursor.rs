//! Cursor trackers (`Method::Cursor`): paint loosely over the mouse cursor on
//! a few frames, and the tracker learns what the cursor looks like and finds
//! it on every frame (on request: "detect multiple distinct, shared
//! patterns/edges [of] valid mouse icons from many different images with
//! different backgrounds … not have the user manually paint … the user can
//! paint more precisely if a mouse icon is only visible for one frame").
//!
//! - **Learning** ([`learn`]). A cursor is the one thing under every paint
//!   that looks the same each time; what is around it changes. Each paint's
//!   pixels (a *shot*) are lined up with every other's where their edges
//!   agree best (not where the screen itself is the same: a cursor that
//!   didn't move says nothing). Paints that line up make one *shape*; a
//!   paint that lines up with none (a hand, an I-beam) starts another.
//!   Frames a little before and after each paint are shots too: the cursor
//!   moved over them, so a single paint already has backgrounds to compare.
//!   Pixels the shots agree on are the cursor (opaque), the rest is
//!   background (transparent). A shape learned from one paint alone, with
//!   nothing to compare, is the paint itself: paint it tightly.
//! - **Learning more** ([`Model::grow`]). The shapes are then looked for on
//!   frames across the video; where one is found for sure, on a new
//!   background, that is another shot, and the shape is learned again. A few
//!   loose paints end up as clean as many.
//! - **Finding** ([`Finder`]). Each frame is searched as a whole for every
//!   shape (masked normalized cross-correlation: only the cursor's own
//!   pixels count, weighted by how sure it is of them; brightness and
//!   contrast don't matter), coarse first, then exactly around the best
//!   places. A cursor that jumps is found all the same, and a frame where
//!   no shape matches well is *not visible* (flagged lost, held where it
//!   was). Of matches about as good: one that moves wins over one that
//!   stays put (a look-alike in the scenery), then one near where it was
//!   ([`Chooser`]).
//! - Its point is the shape's *tip*: the middle of its top edge (an arrow's
//!   point, a hand's fingertip).
//!
//! Everything is in the decoded rendition's pixels; the job converts.

use tt_core::time::FrameIndex;

use super::LookSpec;

/// A grey image, row-major.
#[derive(Clone, Debug, PartialEq)]
pub struct Img {
    pub w: usize,
    pub h: usize,
    pub px: Vec<f32>,
}

impl Img {
    /// A whole luma plane.
    pub fn whole(luma: &[u8], width: usize, height: usize) -> Img {
        Img { w: width, h: height, px: luma[..width * height].iter().map(|v| *v as f32).collect() }
    }

    /// `w × h` pixels of `src` from (x0, y0); beyond its edges, the nearest edge pixel.
    pub fn crop(src: &Img, x0: i64, y0: i64, w: usize, h: usize) -> Img {
        let mut px = Vec::with_capacity(w * h);
        for y in 0..h as i64 {
            let sy = (y0 + y).clamp(0, src.h as i64 - 1) as usize;
            for x in 0..w as i64 {
                let sx = (x0 + x).clamp(0, src.w as i64 - 1) as usize;
                px.push(src.px[sy * src.w + sx]);
            }
        }
        Img { w, h, px }
    }

    /// Bilinear between pixels (`x`, `y` in pixels: 0 is the first's centre); None off the image.
    pub fn sample(&self, x: f64, y: f64) -> Option<f32> {
        if x < 0.0 || y < 0.0 || x > (self.w - 1) as f64 || y > (self.h - 1) as f64 {
            return None;
        }
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.w - 1), (y0 + 1).min(self.h - 1));
        let (tx, ty) = ((x - x0 as f64) as f32, (y - y0 as f64) as f32);
        let p = |x: usize, y: usize| self.px[y * self.w + x];
        let top = p(x0, y0) + (p(x1, y0) - p(x0, y0)) * tx;
        let bottom = p(x0, y1) + (p(x1, y1) - p(x0, y1)) * tx;
        Some(top + (bottom - top) * ty)
    }

    pub fn at(&self, x: i64, y: i64) -> Option<f32> {
        ((0..self.w as i64).contains(&x) && (0..self.h as i64).contains(&y)).then(|| self.px[y as usize * self.w + x as usize])
    }

    /// Half the size each way (rounded down, at least 1), each pixel the mean of its 2 × 2.
    pub fn half(&self) -> Img {
        let (w, h) = ((self.w / 2).max(1), (self.h / 2).max(1));
        let mut px = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                let p = |dx: usize, dy: usize| self.px[(2 * y + dy).min(self.h - 1) * self.w + (2 * x + dx).min(self.w - 1)];
                px.push((p(0, 0) + p(1, 0) + p(0, 1) + p(1, 1)) / 4.0);
            }
        }
        Img { w, h, px }
    }

    /// Central-difference gradients (0 on the border).
    fn grad(&self) -> (Vec<f32>, Vec<f32>) {
        let (mut gx, mut gy) = (vec![0.0; self.px.len()], vec![0.0; self.px.len()]);
        for y in 1..self.h.saturating_sub(1) {
            for x in 1..self.w.saturating_sub(1) {
                let i = y * self.w + x;
                gx[i] = (self.px[i + 1] - self.px[i - 1]) / 2.0;
                gy[i] = (self.px[i + self.w] - self.px[i - self.w]) / 2.0;
            }
        }
        (gx, gy)
    }
}

/// A decoded frame in colour: its luma, and its chroma (U and V) at half
/// size each way, as NV12 has them.
#[derive(Clone, Debug)]
pub struct Frame {
    pub y: Img,
    pub u: Img,
    pub v: Img,
}

impl Frame {
    /// A decoded NV12 frame `width × height`.
    pub fn from_nv12(frame: &[u8], width: usize, height: usize) -> Frame {
        let [u, v] = crate::image::chroma_planes(frame, width, height);
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        let plane = |p: Vec<u8>| Img { w: cw, h: ch, px: p.into_iter().map(f32::from).collect() };
        Frame { y: Img::whole(frame, width, height), u: plane(u), v: plane(v) }
    }

    /// No colour: grey (tests).
    pub fn grey(y: Img) -> Frame {
        let (cw, ch) = (y.w.div_ceil(2), y.h.div_ceil(2));
        let g = Img { w: cw, h: ch, px: vec![128.0; cw * ch] };
        Frame { y, u: g.clone(), v: g }
    }

    /// The colour at full-size pixel (x, y) (the nearest chroma sample; clamped to the frame).
    pub fn chroma(&self, x: i64, y: i64) -> (f32, f32) {
        let (cx, cy) = (x.div_euclid(2).clamp(0, self.u.w as i64 - 1) as usize, y.div_euclid(2).clamp(0, self.u.h as i64 - 1) as usize);
        (self.u.px[cy * self.u.w + cx], self.v.px[cy * self.v.w + cx])
    }

    /// `w × h` full-size pixels from (x0, y0): luma, and U and V per pixel.
    fn crop(&self, x0: i64, y0: i64, w: usize, h: usize) -> (Img, Img, Img) {
        let (mut u, mut v) = (Vec::with_capacity(w * h), Vec::with_capacity(w * h));
        for y in 0..h as i64 {
            for x in 0..w as i64 {
                let (a, b) = self.chroma(x0 + x, y0 + y);
                u.push(a);
                v.push(b);
            }
        }
        (Img::crop(&self.y, x0, y0, w, h), Img { w, h, px: u }, Img { w, h, px: v })
    }
}

/// Half `mask` (w × h) the way [`Img::half`] halves pixels: a cell is on if any of its 2 × 2 is.
fn half_mask(mask: &[bool], w: usize, h: usize) -> Vec<bool> {
    let (hw, hh) = ((w / 2).max(1), (h / 2).max(1));
    let mut out = Vec::with_capacity(hw * hh);
    for y in 0..hh {
        for x in 0..hw {
            let p = |dx: usize, dy: usize| mask[(2 * y + dy).min(h - 1) * w + (2 * x + dx).min(w - 1)];
            out.push(p(0, 0) || p(1, 0) || p(0, 1) || p(1, 1));
        }
    }
    out
}

/// Pixels of one frame that hold the cursor somewhere: a paint's (or, around
/// it, a frame near a paint's, or where a shape was found for sure).
#[derive(Clone, Debug)]
pub struct Shot {
    pub frame: FrameIndex,
    /// Its top-left pixel in the frame.
    pub origin: [i64; 2],
    pub img: Img,
    /// Its colour (chroma U and V), per pixel.
    pub u: Img,
    pub v: Img,
    /// Which pixels may hold the cursor (painted).
    pub painted: Vec<bool>,
    /// Which of them changed in the frames near it (the cursor moved
    /// there), when that says something ([`Shot::mark_moving`]): lining up
    /// looks only at those, not at a background that repeats (a studded
    /// floor lines up with itself every few pixels; the cursor moves over it).
    pub moving: Option<Vec<bool>>,
    /// A paint's: where its painted pixels centre (its pixels), where the
    /// cursor is, give or take ([`Window`]).
    pub centre: Option<[f64; 2]>,
}

/// A pixel changed by more than this (levels) in a frame near: something moved there …
const MOVED: f32 = 16.0;
/// … or its colour did, by this (chroma levels).
const MOVED_COLOUR: f32 = 12.0;
/// Colours this close (chroma levels, each of U and V) count as the same.
const SAME_COLOUR: f32 = 12.0;

/// Paints larger than this (rendition px, each way) are cut to it around their centre.
pub const MAX_SHOT: usize = 320;

impl Shot {
    /// The paint `look`'s pixels in `frame` (rendition `k` × source px), and
    /// which of them are painted. None: off the frame.
    pub fn of_paint(look: &LookSpec, k: [f64; 2], frame: &Frame) -> Option<Shot> {
        let (x0, y0, w, h) = rect(look.center, look.half, k, 2.0, &frame.y)?;
        let painted: Vec<bool> = (0..w * h)
            .map(|i| {
                let p = [(x0 as f64 + (i % w) as f64 + 0.5) / k[0], (y0 as f64 + (i / w) as f64 + 0.5) / k[1]];
                super::paint::on(look, p)
            })
            .collect();
        let on: Vec<usize> = (0..w * h).filter(|i| painted[*i]).collect();
        let centre = (!on.is_empty()).then(|| {
            let n = on.len() as f64;
            [on.iter().map(|i| (i % w) as f64).sum::<f64>() / n, on.iter().map(|i| (i / w) as f64).sum::<f64>() / n]
        });
        let (img, u, v) = frame.crop(x0, y0, w, h);
        Some(Shot { frame: look.frame, origin: [x0, y0], img, u, v, painted, moving: None, centre })
    }

    /// Around a paint, on a frame near it (`f`): the paint's rectangle grown
    /// by half each way (the cursor moved), all of it may hold the cursor.
    pub fn near_paint(look: &LookSpec, k: [f64; 2], f: FrameIndex, frame: &Frame) -> Option<Shot> {
        let (x0, y0, w, h) = rect(look.center, [look.half[0] * 1.5, look.half[1] * 1.5], k, 2.0, &frame.y)?;
        let (img, u, v) = frame.crop(x0, y0, w, h);
        Some(Shot { frame: f, origin: [x0, y0], img, u, v, painted: vec![true; w * h], moving: None, centre: None })
    }

    /// Where a shape `w × h` was found on frame `f` (its top-left at `at`):
    /// its pixels and a few around, and where its top-left lies in them.
    pub fn found(f: FrameIndex, frame: &Frame, at: [f64; 2], w: usize, h: usize) -> (Shot, [f64; 2]) {
        let origin = [at[0].floor() as i64 - 3, at[1].floor() as i64 - 3];
        let (w, h) = (w + 7, h + 7);
        let (img, u, v) = frame.crop(origin[0], origin[1], w, h);
        (Shot { frame: f, origin, img, u, v, painted: vec![true; w * h], moving: None, centre: None }, [at[0] - origin[0] as f64, at[1] - origin[1] as f64])
    }

    /// Mark where it moved ([`Shot::moving`]): painted pixels that differ
    /// from the same spot of the screen in every frame `near` that shows it
    /// (as cut, not lined up): where the cursor is now, not where it was or
    /// will be (that differs in one only). Only
    /// if that is a part of the paint, not nothing (it didn't move) nor most
    /// of it (the whole picture moved).
    pub fn mark_moving(&mut self, near: &[Shot]) {
        let (w, h) = (self.img.w, self.img.h);
        let mut m = vec![false; w * h];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                if !self.painted[i] {
                    continue;
                }
                let (p, pu, pv) = (self.img.px[i], self.u.px[i], self.v.px[i]);
                let there: Vec<(f32, f32, f32)> = near
                    .iter()
                    .filter_map(|n| {
                        let (nx, ny) = (self.origin[0] + x as i64 - n.origin[0], self.origin[1] + y as i64 - n.origin[1]);
                        Some((n.img.at(nx, ny)?, n.u.at(nx, ny)?, n.v.at(nx, ny)?))
                    })
                    .collect();
                m[i] = !there.is_empty() && there.iter().all(|(q, qu, qv)| (p - q).abs() > MOVED || (pu - qu).abs() > MOVED_COLOUR || (pv - qv).abs() > MOVED_COLOUR);
            }
        }
        let (on, painted) = (m.iter().zip(&self.painted).filter(|(a, b)| **a && **b).count(), self.painted.iter().filter(|b| **b).count());
        self.moving = (on >= 8 && on * 2 <= painted).then_some(m);
    }

    /// Its pixels in RGB (BT.709 for HD, else BT.601; video range), to show.
    pub fn rgb(&self) -> Vec<u8> {
        let hd = self.img.h >= 160 || self.img.w >= 160;
        let (rv, gu, gv, bu) = if hd { (1.793, 0.213, 0.533, 2.112) } else { (1.596, 0.392, 0.813, 2.017) };
        let mut out = Vec::with_capacity(self.img.px.len() * 3);
        for i in 0..self.img.px.len() {
            let (y, u, v) = (1.164 * (self.img.px[i] - 16.0), self.u.px[i] - 128.0, self.v.px[i] - 128.0);
            out.extend([y + rv * v, y - gu * u - gv * v, y + bu * u].map(|c| c.round().clamp(0.0, 255.0) as u8));
        }
        out
    }

    /// Whether the pixel nearest point (`x`, `y`) is painted.
    fn painted_at(&self, x: f64, y: f64) -> bool {
        let (x, y) = (x.round() as i64, y.round() as i64);
        (0..self.img.w as i64).contains(&x) && (0..self.img.h as i64).contains(&y) && self.painted[y as usize * self.img.w + x as usize]
    }
}

/// A rectangle (source px centre and half-size, `pad` rendition px more each
/// way) in rendition px, at most [`MAX_SHOT`] a side: `(x0, y0, w, h)`.
fn rect(center: [f64; 2], half: [f64; 2], k: [f64; 2], pad: f64, frame: &Img) -> Option<(i64, i64, usize, usize)> {
    let c = [center[0] * k[0], center[1] * k[1]];
    let h = [(half[0] * k[0] + pad).min(MAX_SHOT as f64 / 2.0), (half[1] * k[1] + pad).min(MAX_SHOT as f64 / 2.0)];
    let (x0, y0) = ((c[0] - h[0]).floor() as i64, (c[1] - h[1]).floor() as i64);
    let (x1, y1) = ((c[0] + h[0]).ceil() as i64, (c[1] + h[1]).ceil() as i64);
    let (x0, y0, x1, y1) = (x0.max(0), y0.max(0), x1.min(frame.w as i64), y1.min(frame.h as i64));
    (x1 - x0 >= 4 && y1 - y0 >= 4).then(|| (x0, y0, (x1 - x0) as usize, (y1 - y0) as usize))
}

/// A shot at one resolution, ready to line up: its pixels, gradients, and
/// its edge pixels (painted, with a gradient).
struct Level {
    img: Img,
    gx: Vec<f32>,
    gy: Vec<f32>,
    painted: Vec<bool>,
    /// `(x, y, gx, gy, |g|, value)`.
    edges: Vec<(i64, i64, f32, f32, f32, f32)>,
}

/// Gradients below this (levels per pixel) aren't edges.
const EDGE: f32 = 4.0;

impl Level {
    /// `img`, which pixels are `painted`, and its edges where `edgy` (painted, moving).
    fn new(img: Img, painted: Vec<bool>, edgy: &[bool]) -> Level {
        let (gx, gy) = img.grad();
        let mut edges = Vec::new();
        for y in 0..img.h {
            for x in 0..img.w {
                let i = y * img.w + x;
                let g = gx[i].hypot(gy[i]);
                if painted[i] && edgy[i] && g >= EDGE {
                    edges.push((x as i64, y as i64, gx[i], gy[i], g, img.px[i]));
                }
            }
        }
        Level { img, gx, gy, painted, edges }
    }

    /// `shot` at 1 / 2^`n` of its size.
    fn of(shot: &Shot, n: u32) -> Level {
        let (mut img, mut painted) = (shot.img.clone(), shot.painted.clone());
        // (Edges near what moved: an outline on the same colour as what is behind it didn't change.)
        let mut edgy = shot.moving.as_ref().map_or_else(|| vec![true; painted.len()], |m| within(m, img.w, img.h, 2));
        for _ in 0..n {
            painted = half_mask(&painted, img.w, img.h);
            edgy = half_mask(&edgy, img.w, img.h);
            img = img.half();
        }
        Level::new(img, painted, &edgy)
    }
}

/// Cells agreement is summed in (pixels of the level a shot is lined up at).
const CELL: i64 = 6;

/// How well `a`'s edges agree with `b`'s with `b` moved by `d` (`a`'s pixel
/// `x` on `b`'s `x + d`), where they agree most: each pair of edges alike
/// (about as bright, and about as steep the same way) counts by how strong
/// the weaker is, summed over a cursor-sized window (3 × 3 cells), the best
/// window's. Strict and local, so busy backgrounds that happen to line up a
/// little here and there add up to little: a cursor is all in one place.
fn agreement(a: &Level, b: &Level, d: [i64; 2]) -> f32 {
    let (cw, ch) = ((a.img.w as i64 / CELL + 1) as usize, (a.img.h as i64 / CELL + 1) as usize);
    let mut cells = vec![0.0f32; cw * ch];
    for &(x, y, gx, gy, g, v) in &a.edges {
        let (bx, by) = (x + d[0], y + d[1]);
        if bx < 0 || by < 0 || bx >= b.img.w as i64 || by >= b.img.h as i64 {
            continue;
        }
        let j = by as usize * b.img.w + bx as usize;
        if !b.painted[j] {
            continue;
        }
        let bright = 1.0 - ((v - b.img.px[j]) / SAME).powi(2);
        if bright <= 0.0 {
            continue;
        }
        let (hx, hy) = (b.gx[j], b.gy[j]);
        let gb = hx.hypot(hy);
        let slack = 0.3 * g.max(gb) + 3.0;
        let steep = 1.0 - ((gx - hx).powi(2) + (gy - hy).powi(2)) / (slack * slack);
        if steep > 0.0 {
            cells[(y / CELL) as usize * cw + (x / CELL) as usize] += g.min(gb) * bright * steep;
        }
    }
    let mut best = 0.0f32;
    for cy in 0..ch {
        for cx in 0..cw {
            let mut s = 0.0;
            for yy in cy.saturating_sub(1)..(cy + 2).min(ch) {
                for xx in cx.saturating_sub(1)..(cx + 2).min(cw) {
                    s += cells[yy * cw + xx];
                }
            }
            best = best.max(s);
        }
    }
    best
}

/// Where `b` lines up with `a`: `b`'s point `x + d` is `a`'s `x` there (to a
/// fraction of a pixel: a cursor drawn between pixels, or a scaled
/// recording, never lines up on whole ones).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aligned {
    pub d: [f64; 2],
    /// The agreement there (full size).
    pub score: f32,
    /// The best elsewhere over the best (coarse): near 1 is no clear answer.
    pub ratio: f32,
}

/// Where two paints can line up: the cursor is about in the middle of each
/// paint (on request: "we will always assume that the person will generally
/// keep the cursor centered, and thus we have an allowance error … to shift
/// and match the cursor … configurable"), so `b` lines up with `a` within
/// `reach` px of `d` (`b`'s centre minus `a`'s). Inside it, the best place
/// is the answer: nothing elsewhere (a repeating floor, a menu, a spinner)
/// can pull it away.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Window {
    pub d: [f64; 2],
    pub reach: f64,
}

impl Window {
    /// Between two paints, each about centred on the cursor (within `reach`); None: one isn't a paint.
    pub fn between(a: &Shot, b: &Shot, reach: f64) -> Option<Window> {
        let (ca, cb) = (a.centre?, b.centre?);
        Some(Window { d: [cb[0] - ca[0], cb[1] - ca[1]], reach })
    }

    fn holds(&self, d: [f64; 2]) -> bool {
        (d[0] - self.d[0]).abs() <= self.reach && (d[1] - self.d[1]).abs() <= self.reach
    }
}

/// Less agreement than this (coarse) lines nothing up.
const MIN_AGREEMENT: f32 = 60.0;
/// A second-best place this close to the best is no clear answer.
const MAX_RATIO: f32 = 0.8;

/// Line `b` up with `a` (see [`Aligned`]) where their edges agree best,
/// coarse to fine. Not where the screen itself is the same, if it mostly
/// is there (a cursor that didn't move between the two). None: no clear
/// answer.
pub fn align(a: &Shot, b: &Shot, window: Option<Window>) -> Option<Aligned> {
    // Coarse enough that a loose paint is ~40 px across (a cursor a few px).
    let size = a.img.w.max(a.img.h).max(b.img.w.max(b.img.h));
    let n = ((size as f64 / 48.0).log2().floor().max(0.0) as u32).min(3);
    let (la, lb) = (Level::of(a, n), Level::of(b, n));
    // Where the screen is the same (in coarse px), if most of it agrees there.
    let still = [a.origin[0] - b.origin[0], a.origin[1] - b.origin[1]];
    let still_here = same_screen(a, b, still);
    let s = 1i64 << n;
    // (Within a window, the screen standing still is no reason to skip: the cursor didn't move between them either.)
    let skip = |d: [i64; 2]| match window {
        Some(w) => !w.holds([(d[0] * s) as f64, (d[1] * s) as f64]) && !w.holds([((d[0] + 1) * s) as f64, ((d[1] + 1) * s) as f64]) && !w.holds([((d[0] - 1) * s) as f64, ((d[1] - 1) * s) as f64]),
        None => still_here && (d[0] * s - still[0]).abs() <= 2 * s && (d[1] * s - still[1]).abs() <= 2 * s,
    };
    let mut scores: Vec<([i64; 2], f32)> = Vec::new();
    for dy in -(la.img.h as i64)..lb.img.h as i64 {
        for dx in -(la.img.w as i64)..lb.img.w as i64 {
            if !skip([dx, dy]) {
                scores.push(([dx, dy], agreement(&la, &lb, [dx, dy])));
            }
        }
    }
    let (best, top) = scores.iter().copied().max_by(|p, q| p.1.total_cmp(&q.1))?;
    if top < MIN_AGREEMENT {
        return None;
    }
    let second = scores.iter().filter(|(d, _)| (d[0] - best[0]).abs() > 2 || (d[1] - best[1]).abs() > 2).map(|(_, v)| *v).fold(0.0, f32::max);
    let ratio = second / top;
    if debug() {
        eprintln!("    align {}->{}: best {best:?} {top:.0}, second {second:.0}, still {still_here}", a.frame, b.frame);
    }
    if ratio > MAX_RATIO && window.is_none() {
        return None;
    }
    // Finer and finer, within a coarse pixel either way.
    let mut d = best;
    let mut score = top;
    let mut full = la;
    let mut fb = lb;
    for m in (0..n).rev() {
        let (la, lb) = (Level::of(a, m), Level::of(b, m));
        let c = [d[0] * 2, d[1] * 2];
        let mut here = (c, f32::MIN);
        for dy in -2..=2 {
            for dx in -2..=2 {
                let t = [c[0] + dx, c[1] + dy];
                let v = agreement(&la, &lb, t);
                if v > here.1 {
                    here = (t, v);
                }
            }
        }
        (d, score) = here;
        (full, fb) = (la, lb);
    }
    // To a fraction of a pixel: a parabola through the neighbours, each way.
    let sub = |e: [i64; 2]| {
        let (l, r) = (agreement(&full, &fb, [d[0] - e[0], d[1] - e[1]]), agreement(&full, &fb, [d[0] + e[0], d[1] + e[1]]));
        let den = l - 2.0 * score + r;
        if den < 0.0 { (0.5 * (l - r) / den).clamp(-0.5, 0.5) as f64 } else { 0.0 }
    };
    Some(Aligned { d: [d[0] as f64 + sub([1, 0]), d[1] as f64 + sub([0, 1])], score, ratio })
}

/// A cut-out cursor lines up where it matches this well ([`align_moving`]) …
const CUT_OUT_MATCH: f32 = 0.45;
/// … and this much better than anywhere else.
const CUT_OUT_CLEAR: f32 = 0.15;

/// Line `b` up with `a` (see [`Aligned`]) by what moved in `a` ([`Shot::moving`]):
/// those pixels, the cursor cut out, matched over `b` (masked normalized
/// cross-correlation: only the cursor's pixels count, not what is behind
/// it). None: `a` has none, or nothing in `b` matches well and clearly.
pub fn align_moving(a: &Shot, b: &Shot, window: Option<Window>) -> Option<Aligned> {
    let moved = a.moving.as_ref()?;
    let (w, h) = (a.img.w, a.img.h);
    // What moved, and what it walls in: on a page the cursor's own colour,
    // only its outline changes; its body is inside it.
    let outside = reachable_from_border(moved, w, h);
    let on: Vec<usize> = (0..w * h).filter(|i| (moved[*i] || !outside[*i]) && a.painted[*i]).collect();
    let (x0, y0) = (on.iter().map(|i| i % w).min()?, on.iter().map(|i| i / w).min()?);
    let (x1, y1) = (on.iter().map(|i| i % w).max()? + 1, on.iter().map(|i| i / w).max()? + 1);
    let (tw, th) = (x1 - x0, y1 - y0);
    let mut value = vec![0.0; tw * th];
    let mut alpha = vec![0.0; tw * th];
    for i in &on {
        let j = (i / w - y0) * tw + (i % w - x0);
        (value[j], alpha[j]) = (a.img.px[*i], 1.0);
    }
    let t = Taps::new(&value, &alpha, None, tw, th)?;
    if b.img.w < tw || b.img.h < th {
        return None;
    }
    let map = scan(&t, &b.img, [0, 0, b.img.w - tw + 1, b.img.h - th + 1]);
    let mw = b.img.w - tw + 1;
    // (Its top-left at `k` in `b` is a shift `d` of k − (x0, y0).)
    let inside = |k: usize| window.is_none_or(|w| w.holds([(k % mw) as f64 - x0 as f64, (k / mw) as f64 - y0 as f64]));
    let (best, top) = map.iter().copied().enumerate().filter(|(k, _)| inside(*k)).max_by(|p, q| p.1.total_cmp(&q.1))?;
    let (bx, by) = ((best % mw) as i64, (best / mw) as i64);
    let second = map.iter().enumerate().filter(|(k, _)| inside(*k)).filter(|(k, _)| ((*k % mw) as i64 - bx).abs() > 2 || ((*k / mw) as i64 - by).abs() > 2).map(|(_, v)| *v).fold(f32::MIN, f32::max);
    if debug() {
        eprintln!("    cut-out {}->{}: best ({bx}, {by}) {top:.2}, second {second:.2}", a.frame, b.frame);
    }
    // (Within a window the best is the answer; anywhere, it must stand out.)
    if top < CUT_OUT_MATCH || (window.is_none() && second > top - CUT_OUT_CLEAR) {
        return None;
    }
    // To a fraction of a pixel: a parabola through the neighbours, each way.
    let at = |x: i64, y: i64| ((0..mw as i64).contains(&x) && (0..(b.img.h - th + 1) as i64).contains(&y)).then(|| map[y as usize * mw + x as usize]);
    let sub = |l: Option<f32>, r: Option<f32>| match (l, r) {
        (Some(l), Some(r)) => {
            let den = l - 2.0 * top + r;
            if den < 0.0 { (0.5 * (l - r) / den).clamp(-0.5, 0.5) as f64 } else { 0.0 }
        }
        _ => 0.0,
    };
    let fx = bx as f64 + sub(at(bx - 1, by), at(bx + 1, by));
    let fy = by as f64 + sub(at(bx, by - 1), at(bx, by + 1));
    Some(Aligned { d: [fx - x0 as f64, fy - y0 as f64], score: top * 1000.0, ratio: second / top })
}

/// Line `b` up with `a`: by what moved in either ([`align_moving`]: the
/// clearer, as what moves in one may be more than the cursor, a spinner
/// beside it that the other hasn't), else by their edges ([`align`]).
pub fn line_up(a: &Shot, b: &Shot, window: Option<Window>) -> Option<Aligned> {
    if a.moving.is_none() && b.moving.is_none() {
        return align(a, b, window);
    }
    let there = align_moving(a, b, window);
    let back = align_moving(b, a, window.map(|w| Window { d: [-w.d[0], -w.d[1]], ..w })).map(|x| Aligned { d: [-x.d[0], -x.d[1]], ..x });
    // (Clearer: the best further above the next best.)
    let clear = |x: &Aligned| x.score * (1.0 - x.ratio);
    match (there, back) {
        (Some(x), Some(y)) => Some(if clear(&y) > clear(&x) { y } else { x }),
        (x, y) => x.or(y),
    }
}

/// Whether `a` and `b` mostly show the same screen with `b` moved by `d` (a
/// cursor that didn't move between them says nothing).
fn same_screen(a: &Shot, b: &Shot, d: [i64; 2]) -> bool {
    if a.frame == b.frame {
        return true;
    }
    let (mut n, mut same) = (0, 0);
    for y in 0..a.img.h as i64 {
        for x in 0..a.img.w as i64 {
            if let (Some(u), Some(v)) = (a.img.at(x, y), b.img.at(x + d[0], y + d[1])) {
                n += 1;
                same += usize::from((u - v).abs() < 6.0);
            }
        }
    }
    n > 0 && same * 2 > n
}

/// A learned shape: what each pixel looks like and how sure it is that the
/// pixel is the cursor (`alpha`, 0–1), and its tip.
#[derive(Clone, Debug, PartialEq)]
pub struct Shape {
    pub w: usize,
    pub h: usize,
    pub value: Vec<f32>,
    /// Its colour (chroma U and V), per pixel.
    pub u: Vec<f32>,
    pub v: Vec<f32>,
    pub alpha: Vec<f32>,
    /// Its point (pixels from its top-left corner): the middle of its top edge.
    pub tip: [f64; 2],
    /// How many shots it was learned from (paints, frames near them, finds).
    pub shots: usize,
    /// How many of the user's paints.
    pub paints: usize,
    /// Which pattern it is (the user's: Shift+brush starts another).
    pub pattern: u32,
    /// Its pattern's paints that didn't line up with the others, or didn't fit (left out).
    pub left_out: usize,
}

/// Pixels this close (levels) count as the same.
const SAME: f32 = 20.0;
/// An edge the shots agree on: this steep (levels per pixel) on average …
const AGREED_EDGE: f32 = 10.0;
/// … and pointing this much the same way in each (the length of their mean
/// over their mean length: 1 is all alike, a texture that differs is low).
const COHERENT: f32 = 0.75;

/// A member of a shape: a shot, lined up (its point `x + d` is the first
/// one's `x`), and for a frame near a paint, which member is that paint.
type Member<'a> = (&'a Shot, [f64; 2], Option<usize>);

/// A [`Member`], owned.
type Owned = (Shot, [f64; 2], Option<usize>);

/// The shape `members` agree on, `paints` of them the user's, and where its
/// top-left lies in the first. None: they agree on too little. `strict`: a
/// pixel is sure only where every paint agrees (members known to fit: each
/// paint with the frames near it is one say, as they mostly show the same
/// background); else where a clear bunch of shots does (to see which
/// members don't fit).
///
/// A pixel is the cursor where the shots agree on its value, and it is on
/// or inside edges they agree on (each steep, pointing the same way in
/// all). Pixels alike only because the backgrounds happen to be alike there
/// (a dark scene behind every paint) have no such edges.
fn shape_of(members: &[Member<'_>], votes: &[usize], paints: usize, strict: bool) -> Option<(Shape, [i64; 2])> {
    let (first, _, _) = members.first()?;
    let (w, h) = (first.img.w, first.img.h);
    let n = members.len();
    let mut value = vec![0.0; w * h];
    let (mut cu, mut cv) = (vec![128.0; w * h], vec![128.0; w * h]);
    let mut alpha = vec![0.0; w * h];
    let mut edge = vec![false; w * h];
    let mut v: Vec<f32> = Vec::with_capacity(n);
    let mut c: Vec<(f32, f32)> = Vec::with_capacity(n);
    // Which say each value is: its paint's (a frame near a paint is its paint's; a find is its own).
    let mut says: Vec<usize> = Vec::with_capacity(n);
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if n == 1 {
                // One shot: the paint itself.
                if first.painted[i] {
                    (value[i], cu[i], cv[i], alpha[i]) = (first.img.px[i], first.u.px[i], first.v.px[i], 1.0);
                }
                continue;
            }
            v.clear();
            c.clear();
            says.clear();
            let (mut gsum, mut glen) = ([0.0f32; 2], 0.0f32);
            for (k, (s, d, parent)) in members.iter().enumerate() {
                let (sx, sy) = (x as f64 + d[0], y as f64 + d[1]);
                if !s.painted_at(sx, sy) {
                    continue;
                }
                let (Some(p), Some(l), Some(r), Some(u), Some(b)) = (s.img.sample(sx, sy), s.img.sample(sx - 1.0, sy), s.img.sample(sx + 1.0, sy), s.img.sample(sx, sy - 1.0), s.img.sample(sx, sy + 1.0)) else {
                    continue;
                };
                v.push(p);
                c.push((s.u.sample(sx, sy).unwrap_or(128.0), s.v.sample(sx, sy).unwrap_or(128.0)));
                says.push(votes.get(parent.unwrap_or(k)).copied().unwrap_or(k));
                let g = [(r - l) / 2.0, (b - u) / 2.0];
                gsum = [gsum[0] + g[0], gsum[1] + g[1]];
                glen += g[0].hypot(g[1]);
            }
            if v.len() < 2 || v.len() * 2 < n {
                continue;
            }
            // The largest bunch of values alike: its share, and its mean. (On a
            // steep edge, a fraction of a pixel changes a value a lot: more slack there.)
            let slack = SAME + (0.5 * glen / v.len() as f32).min(SAME);
            // (Colour bleeds at edges too: video keeps it at half size.)
            let cslack = SAME_COLOUR + (0.5 * glen / v.len() as f32).min(SAME_COLOUR);
            let alike = |a: usize, b: usize| (v[a] - v[b]).abs() <= slack && (c[a].0 - c[b].0).abs() <= cslack && (c[a].1 - c[b].1).abs() <= cslack;
            let (mut best, mut mean, mut mean_c) = (0, 0.0, (128.0, 128.0));
            for a in 0..v.len() {
                let (mut k, mut sum, mut su, mut sv) = (0, 0.0, 0.0, 0.0);
                for b in 0..v.len() {
                    if alike(a, b) {
                        k += 1;
                        sum += v[b];
                        su += c[b].0;
                        sv += c[b].1;
                    }
                }
                if k > best {
                    (best, mean, mean_c) = (k, sum / k as f32, (su / k as f32, sv / k as f32));
                }
            }
            let share = best as f32 / v.len() as f32;
            value[i] = mean;
            (cu[i], cv[i]) = mean_c;
            let (lo, hi) = if strict { (0.55, 0.8) } else { (0.35, 0.6) };
            alpha[i] = if best >= 2 { smoothstep(lo, hi, share) * (v.len() as f32 / n as f32).min(1.0) } else { 0.0 };
            // Strict: the cursor is the same picture on every frame, so a
            // pixel any paint shows otherwise (most of its shots) is behind
            // it: a white page behind it on most paints but not on one, or a
            // spinner beside it on some. (With six or more paints, one in six
            // may disagree: a paint lined up a little off.)
            if strict {
                let mut seen: Vec<(usize, u32, u32)> = Vec::new();
                for ((p, q), g) in v.iter().zip(&c).zip(&says) {
                    let agrees = u32::from((p - mean).abs() <= slack && (q.0 - mean_c.0).abs() <= cslack && (q.1 - mean_c.1).abs() <= cslack);
                    match seen.iter_mut().find(|(s, _, _)| s == g) {
                        Some((_, a, t)) => (*a, *t) = (*a + agrees, *t + 1),
                        None => seen.push((*g, agrees, 1)),
                    }
                }
                let against = seen.iter().filter(|(_, a, t)| a * 2 < *t).count();
                if against > seen.len() / 6 {
                    alpha[i] = 0.0;
                }
            }
            let m = v.len() as f32;
            let steep = gsum[0].hypot(gsum[1]) / m;
            edge[i] = steep >= AGREED_EDGE && glen > 0.0 && gsum[0].hypot(gsum[1]) / glen >= COHERENT;
        }
    }
    if n > 1 {
        background_where_it_moved(members, &value, &mut alpha, w, h);
        // On or inside the agreed edges: near one (2 px), or walled in by them.
        let band = within(&edge, w, h, 2);
        let wall = within(&edge, w, h, 1);
        let outside = reachable_from_border(&wall, w, h);
        for i in 0..w * h {
            if !band[i] && outside[i] {
                alpha[i] = 0.0;
            }
        }
    }
    if std::env::var_os("TT_CURSOR_DEBUG").is_some() {
        eprintln!("  shape_of: {n} members, ds {:?}", members.iter().map(|(_, d, _)| [(d[0] * 10.0).round() / 10.0, (d[1] * 10.0).round() / 10.0]).collect::<Vec<_>>());
        for y in 0..h {
            eprintln!("  |{}|", (0..w).map(|x| { let a = alpha[y * w + x]; if a > 0.5 { if value[y * w + x] < 100.0 { '#' } else { 'o' } } else if edge[y * w + x] { '+' } else if a > 0.05 { '.' } else { ' ' } }).collect::<String>());
        }
    }
    // The cursor is one piece: the sure pixels' largest piece by its edges,
    // and the pixels next to it.
    let keep = largest_piece(&value, &alpha, w, h)?;
    let near = within(&keep, w, h, 1);
    for i in 0..w * h {
        if !near[i] {
            alpha[i] = 0.0;
        }
    }
    crop_shape(value, cu, cv, alpha, w, h, n, paints)
}

/// Frames near a paint show the same screen with the cursor moved: where a
/// pixel of the shape (`alpha` and `value`, in the first member's grid)
/// was, a frame near it shows what is behind it, unless the cursor moved
/// onto that spot there too (and looks like that there). A pixel that looks
/// the same as what is behind it on every paint that saw behind it is that
/// background, not the cursor (or the cursor where it can't be seen
/// anyway): its alpha goes.
fn background_where_it_moved(members: &[Member<'_>], value: &[f32], alpha: &mut [f32], w: usize, h: usize) {
    let cover = alpha.to_vec();
    // What the shape shows at a point, if it is (maybe) the cursor there.
    let shows = |x: f64, y: f64| {
        let (x, y) = (x.round() as i64, y.round() as i64);
        if !(0..w as i64).contains(&x) || !(0..h as i64).contains(&y) {
            return None;
        }
        let i = y as usize * w + x as usize;
        (cover[i] >= 0.3).then(|| value[i])
    };
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if alpha[i] <= 0.0 {
                continue;
            }
            // Per paint: how many of its frames near saw behind the pixel and found it the same, or not.
            let mut views: Vec<(usize, u32, u32)> = Vec::new();
            for (near, dn, parent) in members {
                let Some(p) = parent else { continue };
                let (paint, dp, _) = &members[*p];
                // This pixel on the screen, in the paint's frame, and the same spot in the near one's.
                let screen = [paint.origin[0] as f64 + x as f64 + dp[0], paint.origin[1] as f64 + y as f64 + dp[1]];
                let there = [screen[0] - near.origin[0] as f64, screen[1] - near.origin[1] as f64];
                let (px, py) = (screen[0] - paint.origin[0] as f64, screen[1] - paint.origin[1] as f64);
                let (Some(was), Some(now)) = (paint.img.sample(px, py), near.img.sample(there[0], there[1])) else { continue };
                let colour_same = match (paint.u.sample(px, py), paint.v.sample(px, py), near.u.sample(there[0], there[1]), near.v.sample(there[0], there[1])) {
                    (Some(a), Some(b), Some(c), Some(d)) => (a - c).abs() <= SAME_COLOUR && (b - d).abs() <= SAME_COLOUR,
                    _ => true,
                };
                // (There, the cursor covers the shape's pixel `there − dn`: if what it shows explains it, no view behind.)
                if shows(there[0] - dn[0], there[1] - dn[1]).is_some_and(|c| (c - now).abs() <= SAME) {
                    continue;
                }
                let same = u32::from((was - now).abs() <= SAME && colour_same);
                match views.iter_mut().find(|(q, _, _)| q == p) {
                    Some((_, b, d)) => (*b, *d) = (*b + same, *d + 1 - same),
                    None => views.push((*p, same, 1 - same)),
                }
            }
            // The same as behind it on a paint only means it can't be seen
            // there (the cursor's white on a white page); where any paint
            // shows it differs from what is behind, it is the cursor. Where
            // none does, it is that background.
            if !views.is_empty() && !views.iter().any(|(_, b, d)| d > b) {
                alpha[i] = 0.0;
            }
        }
    }
}

/// The pixels within `r` (a square) of one that is on.
fn within(on: &[bool], w: usize, h: usize, r: usize) -> Vec<bool> {
    let mut out = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            if on[y * w + x] {
                for yy in y.saturating_sub(r)..(y + r + 1).min(h) {
                    for xx in x.saturating_sub(r)..(x + r + 1).min(w) {
                        out[yy * w + xx] = true;
                    }
                }
            }
        }
    }
    out
}

/// The pixels off `wall` that can be reached from the border without crossing it (4-connected).
fn reachable_from_border(wall: &[bool], w: usize, h: usize) -> Vec<bool> {
    let mut seen = vec![false; w * h];
    let mut stack: Vec<usize> = (0..w * h).filter(|i| (i % w == 0 || i % w == w - 1 || i / w == 0 || i / w == h - 1) && !wall[*i]).collect();
    for i in &stack {
        seen[*i] = true;
    }
    while let Some(i) = stack.pop() {
        let (x, y) = (i % w, i / w);
        let mut push = |j: usize| {
            if !seen[j] && !wall[j] {
                seen[j] = true;
                stack.push(j);
            }
        };
        if x > 0 {
            push(i - 1);
        }
        if x + 1 < w {
            push(i + 1);
        }
        if y > 0 {
            push(i - w);
        }
        if y + 1 < h {
            push(i + w);
        }
    }
    seen
}

/// How well a member matches `shape` (cut from the first member's grid at
/// `at`): the correlation over the shape's sure pixels.
fn fits(shape: &Shape, at: [i64; 2], member: &Member<'_>) -> f32 {
    let (s, d, _) = member;
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for y in 0..shape.h {
        for x in 0..shape.w {
            let i = y * shape.w + x;
            if shape.alpha[i] < 0.5 {
                continue;
            }
            let (sx, sy) = ((at[0] + x as i64) as f64 + d[0], (at[1] + y as i64) as f64 + d[1]);
            if let Some(p) = s.img.sample(sx, sy) {
                a.push(shape.value[i]);
                b.push(p);
            }
        }
    }
    if a.len() < 6 {
        return 0.0;
    }
    let (ma, mb) = (a.iter().sum::<f32>() / a.len() as f32, b.iter().sum::<f32>() / b.len() as f32);
    let (mut c, mut va, mut vb) = (0.0, 0.0, 0.0);
    for (p, q) in a.iter().zip(&b) {
        c += (p - ma) * (q - mb);
        va += (p - ma).powi(2);
        vb += (q - mb).powi(2);
    }
    if va <= 0.0 || vb <= 0.0 { 0.0 } else { c / (va * vb).sqrt() }
}

/// How far a shot (the shape's top-left at `top` in it) is from `shape` on the
/// shape's sure pixels, weighted by how sure: the mean difference in luma, and
/// in colour (the mean of U and V). None: it shows none of them.
fn looks_like(shape: &Shape, s: &Shot, top: [f64; 2]) -> Option<(f32, f32)> {
    let (mut luma, mut colour, mut total) = (0.0, 0.0, 0.0);
    for y in 0..shape.h {
        for x in 0..shape.w {
            let i = y * shape.w + x;
            let a = shape.alpha[i];
            if a < 0.5 {
                continue;
            }
            let (sx, sy) = (top[0] + x as f64, top[1] + y as f64);
            let (Some(p), Some(u), Some(v)) = (s.img.sample(sx, sy), s.u.sample(sx, sy), s.v.sample(sx, sy)) else { continue };
            luma += a * (p - shape.value[i]).abs();
            colour += a * ((u - shape.u[i]).abs() + (v - shape.v[i]).abs()) / 2.0;
            total += a;
        }
    }
    (total > 0.0).then(|| (luma / total, colour / total))
}

/// How far `member` is off `shape` (cut from the first member's grid at
/// `at`): the shift (within 2 px, to a fraction) that matches its sure
/// pixels best (normalized cross-correlation). None: too little to match.
fn register(shape: &Shape, at: [i64; 2], member: &Member<'_>) -> Option<[f64; 2]> {
    let (s, d, _) = member;
    let on: Vec<usize> = (0..shape.w * shape.h).filter(|i| shape.alpha[*i] > 0.5).collect();
    if on.len() < 8 {
        return None;
    }
    let score = |dx: f64, dy: f64| -> Option<f32> {
        let (mut a, mut b) = (Vec::with_capacity(on.len()), Vec::with_capacity(on.len()));
        for i in &on {
            let (x, y) = (i % shape.w, i / shape.w);
            let p = s.img.sample((at[0] + x as i64) as f64 + d[0] + dx, (at[1] + y as i64) as f64 + d[1] + dy)?;
            a.push(shape.value[*i]);
            b.push(p);
        }
        let (ma, mb) = (a.iter().sum::<f32>() / a.len() as f32, b.iter().sum::<f32>() / b.len() as f32);
        let (mut c, mut va, mut vb) = (0.0, 0.0, 0.0);
        for (p, q) in a.iter().zip(&b) {
            c += (p - ma) * (q - mb);
            va += (p - ma).powi(2);
            vb += (q - mb).powi(2);
        }
        (va > 0.0 && vb > 0.0).then(|| c / (va * vb).sqrt())
    };
    let mut best: Option<(i64, i64, f32)> = None;
    for dy in -2..=2i64 {
        for dx in -2..=2i64 {
            if let Some(v) = score(dx as f64, dy as f64)
                && best.is_none_or(|b| v > b.2)
            {
                best = Some((dx, dy, v));
            }
        }
    }
    let (bx, by, top) = best?;
    let sub = |l: Option<f32>, r: Option<f32>| match (l, r) {
        (Some(l), Some(r)) => {
            let den = l - 2.0 * top + r;
            if den < 0.0 { (0.5 * (l - r) / den).clamp(-0.5, 0.5) as f64 } else { 0.0 }
        }
        _ => 0.0,
    };
    let (fx, fy) = (bx as f64, by as f64);
    Some([fx + sub(score(fx - 1.0, fy), score(fx + 1.0, fy)), fy + sub(score(fx, fy - 1.0), score(fx, fy + 1.0))])
}

/// The 8-connected piece of pixels with alpha ≥ ½ whose edges are strongest
/// (a cursor has an outline; a patch of the same plain background doesn't).
/// None: no piece of at least 6 pixels.
fn largest_piece(value: &[f32], alpha: &[f32], w: usize, h: usize) -> Option<Vec<bool>> {
    let img = Img { w, h, px: value.to_vec() };
    let (gx, gy) = img.grad();
    let mut label = vec![usize::MAX; w * h];
    let mut best: Option<(f32, usize)> = None;
    let mut pieces = 0;
    for start in 0..w * h {
        if alpha[start] < 0.5 || label[start] != usize::MAX {
            continue;
        }
        let (mut stack, mut size, mut strength) = (vec![start], 0, 0.0);
        label[start] = pieces;
        while let Some(i) = stack.pop() {
            size += 1;
            strength += gx[i].hypot(gy[i]) * alpha[i];
            let (x, y) = ((i % w) as i64, (i / w) as i64);
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let (nx, ny) = (x + dx, y + dy);
                    if nx < 0 || ny < 0 || nx >= w as i64 || ny >= h as i64 {
                        continue;
                    }
                    let j = ny as usize * w + nx as usize;
                    if alpha[j] >= 0.5 && label[j] == usize::MAX {
                        label[j] = pieces;
                        stack.push(j);
                    }
                }
            }
        }
        if size >= 6 && best.is_none_or(|(s, _)| strength > s) {
            best = Some((strength, pieces));
        }
        pieces += 1;
    }
    let (_, piece) = best?;
    Some(label.iter().map(|l| *l == piece).collect())
}

/// The shape cut to its pixels (alpha > 0.05) and a pixel around them, and where that starts.
#[allow(clippy::too_many_arguments)]
fn crop_shape(value: Vec<f32>, cu: Vec<f32>, cv: Vec<f32>, alpha: Vec<f32>, w: usize, h: usize, shots: usize, paints: usize) -> Option<(Shape, [i64; 2])> {
    let on: Vec<usize> = (0..w * h).filter(|i| alpha[*i] > 0.05).collect();
    if on.len() < 6 {
        return None;
    }
    let x0 = on.iter().map(|i| i % w).min()?.saturating_sub(1);
    let x1 = (on.iter().map(|i| i % w).max()? + 2).min(w);
    let y0 = on.iter().map(|i| i / w).min()?.saturating_sub(1);
    let y1 = (on.iter().map(|i| i / w).max()? + 2).min(h);
    let (cw, ch) = (x1 - x0, y1 - y0);
    let (mut v, mut u2, mut v2, mut a) = (Vec::with_capacity(cw * ch), Vec::with_capacity(cw * ch), Vec::with_capacity(cw * ch), Vec::with_capacity(cw * ch));
    for y in y0..y1 {
        for x in x0..x1 {
            v.push(value[y * w + x]);
            u2.push(cu[y * w + x]);
            v2.push(cv[y * w + x]);
            a.push(alpha[y * w + x]);
        }
    }
    let tip = tip_of(&a, cw, ch);
    Some((Shape { w: cw, h: ch, value: v, u: u2, v: v2, alpha: a, tip, shots, paints, pattern: 0, left_out: 0 }, [x0 as i64, y0 as i64]))
}

/// The middle of the top edge of the sure pixels (alpha ≥ ½): its first
/// row's middle, at that row's top.
fn tip_of(alpha: &[f32], w: usize, h: usize) -> [f64; 2] {
    for y in 0..h {
        let xs: Vec<usize> = (0..w).filter(|x| alpha[y * w + x] >= 0.5).collect();
        if let (Some(a), Some(b)) = (xs.first(), xs.last()) {
            return [(*a + *b + 1) as f64 / 2.0, y as f64];
        }
    }
    [w as f64 / 2.0, h as f64 / 2.0]
}

fn smoothstep(lo: f32, hi: f32, v: f32) -> f32 {
    let t = ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A paint's shots: its own, and those of frames near it, and the pattern it teaches.
#[derive(Clone, Debug)]
pub struct PaintShots {
    pub paint: Shot,
    pub pattern: u32,
    /// Frames near it, each lined up with the paint (`d` as in [`Aligned`]).
    pub near: Vec<(Shot, [f64; 2])>,
}

impl PaintShots {
    /// The paint and the frames near it that line up with it (the cursor moved there).
    pub fn new(paint: Shot, pattern: u32, near: Vec<Shot>) -> PaintShots {
        let near = near.into_iter().filter_map(|s| line_up(&paint, &s, None).map(|a| (s, a.d))).collect();
        PaintShots { paint, pattern, near }
    }
}

/// What a cursor tracker learned: its shapes, and for each the shots it
/// was learned from (each lined up with the first).
#[derive(Clone, Debug, Default)]
pub struct Model {
    pub shapes: Vec<Shape>,
    /// Patterns painted that it couldn't learn (nothing in their paints lined up).
    pub unlearned: Vec<u32>,
    /// Per shape: its shots, where they line up (the first is its
    /// reference), and for a frame near a paint, which is that paint.
    members: Vec<Vec<Owned>>,
    /// Per shape: where its shape's pixels lie in its reference shot.
    at: Vec<[i64; 2]>,
    /// Per shape, per member: whose say it is (paints of the same still screen share one).
    votes: Vec<Vec<usize>>,
    /// Each paint learned from, and what became of it.
    pub paints: Vec<PaintSeen>,
}

/// What became of a paint when its pattern was learned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PaintUse {
    /// One of the paints the pattern was learned from.
    #[default]
    Used,
    /// Used, as one say with the paint on this frame: the same still screen.
    SameAs(FrameIndex),
    /// Nothing near its centre lines up with the other paints (no cursor there?).
    NotLinedUp,
    /// It lines up, but doesn't look like what the others make.
    LeftOut,
}

/// A paint, as the app is shown it: its frame and pattern, and what became of it.
#[derive(Clone, Debug, PartialEq)]
pub struct PaintSeen {
    pub frame: FrameIndex,
    pub pattern: u32,
    pub used: PaintUse,
}

/// What the app is shown of what a cursor tracker learned.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Learned {
    pub shapes: Vec<Shape>,
    pub unlearned: Vec<u32>,
    pub paints: Vec<PaintSeen>,
}

impl Model {
    pub fn learned(&self) -> Learned {
        Learned { shapes: self.shapes.clone(), unlearned: self.unlearned.clone(), paints: self.paints.clone() }
    }
}

/// The first of `i`'s set (union-find without the rank: a few paints).
fn root(set: &[usize], mut i: usize) -> usize {
    while set[i] != i {
        i = set[i];
    }
    i
}

/// A paint whose pixels match its pattern's shape less than this is left out of it.
const FITS: f32 = 0.5;
/// A find joins a shape's shots ([`Model::grow`]) only if, on the shape's sure
/// pixels, it is this close (levels, on average) in brightness …
const GROW_LUMA: f32 = 40.0;
/// … and this close in colour (chroma levels, the mean of U and V).
const GROW_COLOUR: f32 = 15.0;

fn debug() -> bool {
    std::env::var_os("TT_CURSOR_DEBUG").is_some()
}

/// A paint is about centred on the cursor within this many px (rendition), by default.
pub const CENTRED: f64 = 10.0;

/// Learn the cursor's shapes from its paints (see the module docs): each
/// pattern's from its own paints, each paint about centred on the cursor
/// (within `centred` px).
pub fn learn(paints: Vec<PaintShots>, centred: f64) -> Model {
    let mut patterns: Vec<u32> = paints.iter().map(|p| p.pattern).collect();
    patterns.sort_unstable();
    patterns.dedup();
    let mut model = Model::default();
    for p in patterns {
        let mine: Vec<&PaintShots> = paints.iter().filter(|s| s.pattern == p).collect();
        model.learn_pattern(p, &mine, centred);
    }
    model
}

impl Model {
    /// Pattern `pattern`'s shape, from its `paints`: each lined up with the
    /// one that lines up with the most (or through another already lined
    /// up), and what they agree on; a paint that doesn't fit what the
    /// others make is left out of it.
    fn learn_pattern(&mut self, pattern: u32, paints: &[&PaintShots], centred: f64) {
        let n = paints.len();
        let mut pair: Vec<Vec<Option<Aligned>>> = vec![vec![None; n]; n];
        // Paints of the same still screen (the cursor parked, nothing moving):
        // what one shows, the other does too; together, one say.
        let mut same: Vec<usize> = (0..n).collect();
        for i in 0..n {
            for j in i + 1..n {
                let (a, b) = (&paints[i].paint, &paints[j].paint);
                // (Each paint is about centred on the cursor: within `centred` px each, so twice that apart.)
                if let Some(x) = line_up(a, b, Window::between(a, b, 2.0 * centred)) {
                    pair[i][j] = Some(x);
                    pair[j][i] = Some(Aligned { d: [-x.d[0], -x.d[1]], ..x });
                    let still = [a.origin[0] - b.origin[0], a.origin[1] - b.origin[1]];
                    if (x.d[0] - still[0] as f64).abs() <= 1.5 && (x.d[1] - still[1] as f64).abs() <= 1.5 && same_screen(a, b, still) {
                        let (ri, rj) = (root(&same, i), root(&same, j));
                        same[ri.max(rj)] = ri.min(rj);
                    }
                }
            }
        }
        if debug() {
            for i in 0..n {
                let links: Vec<_> = (0..n).map(|j| pair[i][j].map(|a| ([(a.d[0] * 10.0).round() / 10.0, (a.d[1] * 10.0).round() / 10.0], a.score as i64))).collect();
                eprintln!("pattern {pattern} paint {i} (frame {}, {}x{}, {} near): {links:?}", paints[i].paint.frame, paints[i].paint.img.w, paints[i].paint.img.h, paints[i].near.len());
            }
        }
        // The reference: lined up with the most (the first painted, on a tie).
        let r = (0..n).max_by_key(|i| ((0..n).filter(|j| pair[*i][*j].is_some()).count(), std::cmp::Reverse(*i))).unwrap_or(0);
        // Everything lined up with it, directly or through another (strongest links first).
        let mut d: Vec<Option<[f64; 2]>> = vec![None; n];
        d[r] = Some([0.0, 0.0]);
        loop {
            let next = (0..n)
                .filter(|u| d[*u].is_some())
                .flat_map(|u| (0..n).filter(|v| d[*v].is_none()).filter_map(|v| pair[u][v].map(|a| (u, v, a))).collect::<Vec<_>>())
                .max_by(|a, b| a.2.score.total_cmp(&b.2.score));
            let Some((u, v, a)) = next else { break };
            let du = d[u].expect("lined up");
            d[v] = Some([du[0] + a.d[0], du[1] + a.d[1]]);
        }
        let mut group: Vec<usize> = (0..n).filter(|i| d[*i].is_some()).collect();
        let mut used = vec![PaintUse::NotLinedUp; n];
        // A group's members (lined up with its first paint), whose paint each is, and whose say.
        let build = |group: &[usize], d: &[Option<[f64; 2]>]| {
            let mut members: Vec<Owned> = Vec::new();
            let mut whose: Vec<Option<usize>> = Vec::new();
            let mut votes: Vec<usize> = Vec::new();
            // (Which member each paint is: a paint of the same still screen as another votes as that one.)
            let mut member_of: Vec<Option<usize>> = vec![None; n];
            let base = group.first().and_then(|j| d[*j]).unwrap_or([0.0, 0.0]);
            for j in group {
                let dj = d[*j].expect("lined up");
                let dj = [dj[0] - base[0], dj[1] - base[1]];
                let parent = members.len();
                member_of[*j] = Some(parent);
                let vote = member_of[root(&same, *j)].unwrap_or(parent);
                members.push((paints[*j].paint.clone(), dj, None));
                whose.push(Some(*j));
                votes.push(vote);
                for (s, e) in &paints[*j].near {
                    members.push((s.clone(), [dj[0] + e[0], dj[1] + e[1]], Some(parent)));
                    whose.push(None);
                    votes.push(vote);
                }
            }
            (members, whose, votes)
        };
        let sure = |made: &Option<(Shape, [i64; 2])>| made.as_ref().map_or(0, |(s, _)| s.alpha.iter().filter(|a| **a > 0.5).count());
        // Lined up again with what they make, this many times at most.
        let mut sharpen = 2;
        loop {
            let (members, whose, votes) = build(&group, &d);
            let refs: Vec<Member<'_>> = members.iter().map(|(s, d, p)| (s, *d, *p)).collect();
            // What every paint shows (on request: "if one example disproves
            // multiple others, those multiple others should be discarded"):
            // a pixel any paint shows otherwise isn't the cursor. A paint is
            // left out only if it doesn't show even that (no cursor in it,
            // or another shape): the worst first, one at a time.
            let made = shape_of(&refs, &votes, group.len(), true);
            // Lined up with each other to about a pixel, a thin outline
            // falls on the body in one paint and on the background in
            // another: each paint lined up again with what they make (its
            // frames near move with it), and learned again.
            if sharpen > 0
                && let Some((shape, at)) = &made
            {
                sharpen -= 1;
                let mut moved = false;
                for (m, w) in refs.iter().zip(&whose) {
                    let Some(j) = w else { continue };
                    if let Some(step) = register(shape, *at, m) {
                        let dj = d[*j].expect("lined up");
                        d[*j] = Some([dj[0] + step[0], dj[1] + step[1]]);
                        moved |= step[0].abs() > 0.2 || step[1].abs() > 0.2;
                    }
                }
                if debug() {
                    eprintln!("pattern {pattern}: lined up again ({})", if moved { "moved" } else { "as they were" });
                }
                if moved {
                    continue;
                }
            }
            let worst = match &made {
                Some((shape, at)) => refs.iter().zip(&whose).filter_map(|(m, w)| w.map(|j| (j, fits(shape, *at, m)))).filter(|(_, f)| *f < FITS).min_by(|a, b| a.1.total_cmp(&b.1)),
                None => None,
            };
            if debug() {
                if let Some((shape, at)) = &made {
                    let all: Vec<(usize, f32)> = refs.iter().zip(&whose).filter_map(|(m, w)| w.map(|j| (j, fits(shape, *at, m)))).collect();
                    eprintln!("  fits {all:?}");
                }
                eprintln!("pattern {pattern}, paints {group:?}: {}, worst {worst:?}", made.as_ref().map_or("no shape".to_string(), |(s, _)| format!("{}x{} sure {}", s.w, s.h, s.alpha.iter().filter(|a| **a > 0.5).count())));
            }
            if let Some((j, _)) = worst.filter(|_| group.len() > 1) {
                used[j] = PaintUse::LeftOut;
                group.retain(|g| *g != j);
                continue;
            }
            // Nothing they all show: the paint without which the most is shown goes (a paint with no cursor in it).
            if sure(&made) < 6 && group.len() > 2 {
                let without = group
                    .iter()
                    .map(|j| {
                        let rest: Vec<usize> = group.iter().copied().filter(|g| g != j).collect();
                        let (m, _, v) = build(&rest, &d);
                        let r: Vec<Member<'_>> = m.iter().map(|(s, d, p)| (s, *d, *p)).collect();
                        (*j, sure(&shape_of(&r, &v, rest.len(), true)))
                    })
                    .max_by_key(|(_, k)| *k);
                if let Some((j, k)) = without.filter(|(_, k)| *k >= 6) {
                    if debug() {
                        eprintln!("pattern {pattern}: without paint {j} ({k} sure)");
                    }
                    used[j] = PaintUse::LeftOut;
                    group.retain(|g| *g != j);
                    continue;
                }
            }
            for j in &group {
                let r = root(&same, *j);
                used[*j] = if r != *j && group.contains(&r) { PaintUse::SameAs(paints[r].paint.frame) } else { PaintUse::Used };
            }
            self.paints.extend(paints.iter().zip(&used).map(|(p, u)| PaintSeen { frame: p.paint.frame, pattern, used: *u }));
            match made {
                Some((mut shape, at)) => {
                    shape.pattern = pattern;
                    shape.left_out = n - group.len();
                    self.shapes.push(shape);
                    self.at.push(at);
                    self.members.push(members);
                    self.votes.push(votes);
                }
                None => self.unlearned.push(pattern),
            }
            return;
        }
    }

    /// Shots where shape `i` was found for sure (each with where the
    /// shape's top-left lies in it), learned again with them. Shots on a
    /// spot of the screen it already has (the same background) add nothing.
    pub fn grow(&mut self, i: usize, found: Vec<(Shot, [f64; 2])>) {
        let Some(members) = self.members.get_mut(i) else { return };
        let at = self.at[i];
        let shape = &self.shapes[i];
        let mut added = 0;
        for (s, top) in found {
            // Only a find that looks like the shape is the cursor on another
            // background. Another colour (a yellow twin), another brightness
            // (the cursor dimmed) or no cursor at all (a look-alike patch of
            // scenery) would wipe out every pixel it differs on.
            if !looks_like(shape, &s, top).is_some_and(|(luma, colour)| luma <= GROW_LUMA && colour <= GROW_COLOUR) {
                continue;
            }
            // (Where each shot has the shape's top-left on the screen.)
            let here = [s.origin[0] as f64 + top[0], s.origin[1] as f64 + top[1]];
            let fresh = members.iter().all(|(m, d, _)| {
                let p = [m.origin[0] as f64 + at[0] as f64 + d[0], m.origin[1] as f64 + at[1] as f64 + d[1]];
                (p[0] - here[0]).abs() > shape.w as f64 || (p[1] - here[1]).abs() > shape.h as f64
            });
            if fresh {
                // The reference's `at` is its `top`; its own say.
                self.votes[i].push(members.len());
                members.push((s, [top[0] - at[0] as f64, top[1] - at[1] as f64], None));
                added += 1;
            }
        }
        if added == 0 {
            return;
        }
        let (paints, pattern, left_out) = (shape.paints, shape.pattern, shape.left_out);
        let refs: Vec<Member<'_>> = members.iter().map(|(s, d, p)| (s, *d, *p)).collect();
        if let Some((mut shape, at)) = shape_of(&refs, &self.votes[i], paints, true) {
            (shape.pattern, shape.left_out) = (pattern, left_out);
            self.shapes[i] = shape;
            self.at[i] = at;
        }
    }
}

/// What a model is learned from, as a number: a tracker's jobs and its
/// learning in the background share one model ([`model_slot`]).
pub fn model_key(video: &std::path::Path, looks: &[LookSpec], k: [f64; 2], lo: FrameIndex, hi: FrameIndex) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(video.to_string_lossy().as_bytes());
    h.update(format!("{looks:?} {k:?} {lo} {hi}").as_bytes());
    u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().expect("8 bytes"))
}

/// A shape ready to match at one resolution: its pixels (offsets from its
/// top-left), each's weight, and weight × (value − mean).
#[derive(Clone, Debug)]
struct Taps {
    w: usize,
    h: usize,
    offs: Vec<(i64, i64)>,
    weight: Vec<f32>,
    centred: Vec<f32>,
    total: f32,
    /// Σ weight × (value − mean)².
    var: f32,
    /// Each pixel's colour (U, V) (full size only; empty: none).
    colour: Vec<(f32, f32)>,
}

impl Taps {
    fn new(value: &[f32], alpha: &[f32], colour: Option<(&[f32], &[f32])>, w: usize, h: usize) -> Option<Taps> {
        let on: Vec<usize> = (0..w * h).filter(|i| alpha[*i] > 0.05).collect();
        let total: f32 = on.iter().map(|i| alpha[*i]).sum();
        if on.len() < 4 || total <= 0.0 {
            return None;
        }
        let mean = on.iter().map(|i| alpha[*i] * value[*i]).sum::<f32>() / total;
        let var: f32 = on.iter().map(|i| alpha[*i] * (value[*i] - mean).powi(2)).sum();
        if var < 1.0 {
            return None;
        }
        Some(Taps {
            w,
            h,
            offs: on.iter().map(|i| ((i % w) as i64, (i / w) as i64)).collect(),
            weight: on.iter().map(|i| alpha[*i]).collect(),
            centred: on.iter().map(|i| alpha[*i] * (value[*i] - mean)).collect(),
            total,
            var,
            colour: colour.map(|(u, v)| on.iter().map(|i| (u[*i], v[*i])).collect()).unwrap_or_default(),
        })
    }

    /// How far its colour is from the frame's with its top-left on (x, y)
    /// (full size): the mean difference of U and V, weighted (0: none to compare).
    fn colour_off(&self, frame: &Frame, x: i64, y: i64) -> f32 {
        if self.colour.is_empty() {
            return 0.0;
        }
        let mut d = 0.0;
        for (k, (tu, tv)) in self.colour.iter().enumerate() {
            let (dx, dy) = self.offs[k];
            let (u, v) = frame.chroma(x + dx, y + dy);
            d += self.weight[k] * ((u - tu).abs() + (v - tv).abs()) / 2.0;
        }
        d / self.total
    }

    /// The match (−1 to 1) with its top-left on (x, y) of `img` (which it must fit in).
    fn score(&self, img: &Img, x: usize, y: usize) -> f32 {
        let (mut s1, mut s2, mut sc) = (0.0f32, 0.0f32, 0.0f32);
        let base = y * img.w + x;
        for k in 0..self.offs.len() {
            let (dx, dy) = self.offs[k];
            let p = img.px[base + dy as usize * img.w + dx as usize];
            let w = self.weight[k];
            s1 += w * p;
            s2 += w * p * p;
            sc += self.centred[k] * p;
        }
        let var = s2 - s1 * s1 / self.total;
        // (Too flat to be the cursor: under a tenth of its contrast, dimmed or not.)
        if var * 100.0 < self.var {
            return 0.0;
        }
        sc / (self.var * var).sqrt()
    }
}

/// A frame's luma at 1, ½, ¼ … of its size, and the frame in colour.
pub struct Pyramid {
    pub frame: Frame,
    smaller: Vec<Img>,
}

impl Pyramid {
    pub fn new(frame: Frame, levels: usize) -> Pyramid {
        let mut smaller: Vec<Img> = Vec::new();
        for _ in 1..levels {
            let next = smaller.last().unwrap_or(&frame.y).half();
            smaller.push(next);
        }
        Pyramid { frame, smaller }
    }

    pub fn full(&self) -> &Img {
        &self.frame.y
    }

    fn level(&self, n: usize) -> &Img {
        if n == 0 { &self.frame.y } else { &self.smaller[n - 1] }
    }
}

/// A match's colour this far off its pattern's (chroma levels, the mean of U and V) costs nothing …
const COLOUR_FREE: f32 = 10.0;
/// … and this far, everything (a yellow twin of a white arrow, a grey box for a white one).
const COLOUR_GONE: f32 = 40.0;

/// Where a shape was found: its top-left (rendition px, to a fraction), its tip there, and how well it matched.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Found {
    pub shape: usize,
    pub at: [f64; 2],
    pub tip: [f64; 2],
    pub score: f32,
}

/// Finds a model's shapes in frames (see the module docs).
pub struct Finder {
    pub shapes: Vec<Shape>,
    /// Per shape, per level (full size first): its taps (None: too small there).
    taps: Vec<Vec<Option<Taps>>>,
    /// Per shape: the level it is searched at first.
    coarse: Vec<usize>,
    /// Levels a frame's pyramid needs.
    pub levels: usize,
}

/// A shape is searched first at the level where its smaller side is about this many pixels.
const COARSE_PX: f64 = 7.0;

impl Finder {
    pub fn new(shapes: Vec<Shape>) -> Finder {
        let mut taps = Vec::new();
        let mut coarse = Vec::new();
        for s in &shapes {
            let mut level = vec![Taps::new(&s.value, &s.alpha, Some((&s.u, &s.v)), s.w, s.h)];
            let (mut v, mut a, mut w, mut h) = (s.value.clone(), s.alpha.clone(), s.w, s.h);
            let c = ((w.min(h) as f64 / COARSE_PX).log2().floor().max(0.0) as usize).min(3);
            for _ in 0..c {
                let (iv, ia) = (Img { w, h, px: v }, Img { w, h, px: a });
                let (hv, ha) = (iv.half(), ia.half());
                (w, h, v, a) = (hv.w, hv.h, hv.px, ha.px);
                level.push(Taps::new(&v, &a, None, w, h));
            }
            // (The coarsest level it has taps at.)
            let c = (0..level.len()).rev().find(|l| level[*l].is_some()).unwrap_or(0);
            coarse.push(c);
            taps.push(level);
        }
        let levels = coarse.iter().copied().max().unwrap_or(0) + 1;
        Finder { shapes, taps, coarse, levels }
    }

    /// The best places for each shape in `region` (`[x0, y0, x1, y1]`,
    /// rendition px) of the frame: up to `per_shape` each, best first
    /// overall, scoring at least `min`.
    pub fn find(&self, frame: &Pyramid, region: [f64; 4], per_shape: usize, min: f32) -> Vec<Found> {
        let mut out = Vec::new();
        for i in 0..self.shapes.len() {
            let c = self.coarse[i];
            let Some(t) = self.taps[i][c].as_ref() else { continue };
            let img = frame.level(c);
            let s = (1usize << c) as f64;
            let (x0, y0) = (((region[0] / s).floor().max(0.0)) as usize, ((region[1] / s).floor().max(0.0)) as usize);
            let x1 = ((region[2] / s).ceil() as usize).min(img.w).saturating_sub(t.w);
            let y1 = ((region[3] / s).ceil() as usize).min(img.h).saturating_sub(t.h);
            if x1 < x0 || y1 < y0 {
                continue;
            }
            let map = scan(t, img, [x0, y0, x1 + 1, y1 + 1]);
            let (mw, mh) = (x1 + 1 - x0, y1 + 1 - y0);
            // The best places (local peaks), a little looser than asked at the coarse level.
            let mut peaks: Vec<(usize, usize, f32)> = Vec::new();
            for y in 0..mh {
                for x in 0..mw {
                    let v = map[y * mw + x];
                    if v < min - 0.25 {
                        continue;
                    }
                    let peak = (y.saturating_sub(1)..(y + 2).min(mh)).all(|yy| (x.saturating_sub(1)..(x + 2).min(mw)).all(|xx| map[yy * mw + xx] <= v));
                    if peak {
                        peaks.push((x + x0, y + y0, v));
                    }
                }
            }
            peaks.sort_by(|a, b| b.2.total_cmp(&a.2));
            let mut kept: Vec<Found> = Vec::new();
            for (x, y, _) in peaks.into_iter().take(per_shape * 3) {
                let Some(f) = self.refine(i, frame, [(x as f64) * s, (y as f64) * s], s as i64) else { continue };
                if f.score >= min && kept.iter().all(|k| (k.at[0] - f.at[0]).abs() > 2.0 || (k.at[1] - f.at[1]).abs() > 2.0) {
                    kept.push(f);
                }
                if kept.len() >= per_shape {
                    break;
                }
            }
            out.extend(kept);
        }
        out.sort_by(|a, b| b.score.total_cmp(&a.score));
        out
    }

    /// Shape `i` at full size within `reach` px of top-left `near`, to a fraction of a pixel.
    pub fn refine(&self, i: usize, frame: &Pyramid, near: [f64; 2], reach: i64) -> Option<Found> {
        let t = self.taps[i][0].as_ref()?;
        let img = frame.full();
        let (cx, cy) = (near[0].round() as i64, near[1].round() as i64);
        let (x0, y0) = ((cx - reach).max(0), (cy - reach).max(0));
        let (x1, y1) = ((cx + reach).min(img.w as i64 - t.w as i64), (cy + reach).min(img.h as i64 - t.h as i64));
        if x1 < x0 || y1 < y0 {
            return None;
        }
        let mut best: Option<(i64, i64, f32)> = None;
        for y in y0..=y1 {
            for x in x0..=x1 {
                let v = t.score(img, x as usize, y as usize);
                if best.is_none_or(|b| v > b.2) {
                    best = Some((x, y, v));
                }
            }
        }
        let (x, y, v) = best?;
        // A parabola through the neighbours, each way.
        let at = |x: i64, y: i64| ((0..=img.w as i64 - t.w as i64).contains(&x) && (0..=img.h as i64 - t.h as i64).contains(&y)).then(|| t.score(img, x as usize, y as usize));
        let sub = |a: Option<f32>, b: Option<f32>| match (a, b) {
            (Some(a), Some(b)) => {
                let den = a - 2.0 * v + b;
                if den < 0.0 { (0.5 * (a - b) / den).clamp(-0.5, 0.5) as f64 } else { 0.0 }
            }
            _ => 0.0,
        };
        let pos = [x as f64 + sub(at(x - 1, y), at(x + 1, y)), y as f64 + sub(at(x, y - 1), at(x, y + 1))];
        let tip = self.shapes[i].tip;
        // Its colour: a match of the right shape in another colour isn't it.
        let off = t.colour_off(&frame.frame, x, y);
        let v = v * (1.0 - smoothstep(COLOUR_FREE, COLOUR_GONE, off));
        Some(Found { shape: i, at: pos, tip: [pos[0] + tip[0], pos[1] + tip[1]], score: v })
    }
}

/// Every position's score in `[x0, y0, x1, y1)` (top-lefts) of `img`, row-major;
/// big searches split across a few threads.
fn scan(t: &Taps, img: &Img, r: [usize; 4]) -> Vec<f32> {
    let (w, h) = (r[2] - r[0], r[3] - r[1]);
    let mut out = vec![0.0; w * h];
    let work = w * h * t.offs.len();
    let threads = if work > 2_000_000 { std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 4) } else { 1 };
    let rows = h.div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        for (n, chunk) in out.chunks_mut(rows * w).enumerate() {
            scope.spawn(move || {
                for (j, row) in chunk.chunks_mut(w).enumerate() {
                    let y = r[1] + n * rows + j;
                    for (i, v) in row.iter_mut().enumerate() {
                        *v = t.score(img, r[0] + i, y);
                    }
                }
            });
        }
    });
    out
}

/// What a frame's search found, chosen frame by frame (see the module docs).
#[derive(Default)]
pub struct Chooser {
    /// Where the cursor's tip was on the frame before (rendition px).
    pub prev: Option<[f64; 2]>,
    /// Places where a good match has stayed put: where, and for how many
    /// frames in a row (up to the frame before).
    still: Vec<([f64; 2], u32)>,
}

/// Found for sure.
pub const SURE: f32 = 0.85;
/// A match that stays put this many frames is *still*: where another moves,
/// the one that moves is the cursor (a look-alike in the scenery stays put).
const STILL_FRAMES: u32 = 8;
/// Matches this close to the best are all in the running.
const TIE: f32 = 0.15;

impl Chooser {
    /// The cursor among `found` (best first), scoring at least `min`; None:
    /// not visible. Of those about as good as the best: one that moves over
    /// one that stays put (a look-alike in the scenery), then one near where
    /// it was (within `reach`), then the best.
    pub fn choose(&mut self, found: &[Found], min: f32, reach: f64) -> Option<Found> {
        let good: Vec<Found> = found.iter().copied().filter(|f| f.score >= min).collect();
        // Which have stayed put.
        let mut still = Vec::new();
        for f in &good {
            let n = self.still.iter().find(|(p, _)| (p[0] - f.tip[0]).hypot(p[1] - f.tip[1]) <= 1.0).map_or(1, |(_, n)| n + 1);
            still.push((f.tip, n));
        }
        self.still = still;
        let best = good.first().copied()?;
        let is_still = |f: &Found| self.still.iter().any(|(p, n)| *n >= STILL_FRAMES && (p[0] - f.tip[0]).hypot(p[1] - f.tip[1]) <= 1.0);
        let tied: Vec<Found> = good.iter().copied().filter(|f| f.score >= best.score - TIE).collect();
        let moving = tied.iter().any(|f| !is_still(f));
        let near = |f: &Found| self.prev.is_some_and(|p| (p[0] - f.tip[0]).hypot(p[1] - f.tip[1]) <= reach);
        let pick = tied.iter().copied().max_by_key(|f| (!(moving && is_still(f)), near(f), (f.score * 1000.0) as i64)).unwrap_or(best);
        self.prev = Some(pick.tip);
        Some(pick)
    }
}

// ------------------------------------------------------------------ the job

/// Frames before and after each paint that are shots too.
const NEAR: FrameIndex = 3;
/// Frames across the tracked range where the shapes are looked for to learn more ([`Model::grow`]).
const GROW_FRAMES: usize = 12;
/// Learning more looks this many frames past the first and last paints (at least).
const GROW_PAD: FrameIndex = 300;
/// Decoded frames a backward job keeps at once (luma only).
const BACK_SEGMENT: FrameIndex = 48;
/// Near where it was, this far (rendition px), wins a near tie.
const REACH: f64 = 60.0;

/// Models learned lately, by what they were learned from ([`model_key`]):
/// a tracker's two jobs (forward and backward) and its learning in the
/// background learn once. Each entry is locked while it is learned, so the
/// others wait for the first.
type Slot = std::sync::Mutex<Option<std::sync::Arc<Model>>>;
static MODELS: std::sync::Mutex<Vec<(u64, std::sync::Arc<Slot>)>> = std::sync::Mutex::new(Vec::new());

fn model_slot(key: u64) -> std::sync::Arc<Slot> {
    let mut models = MODELS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, m)) = models.iter().find(|(k, _)| *k == key) {
        return m.clone();
    }
    if models.len() >= 8 {
        models.remove(0);
    }
    let m = std::sync::Arc::new(std::sync::Mutex::new(None));
    models.push((key, m.clone()));
    m
}

impl super::Worker {
    /// A decoded frame, in colour.
    fn colour_frame(&self, frame: &[u8]) -> Frame {
        let (w, h) = (self.spec.video.width as usize, self.spec.video.height as usize);
        Frame::from_nv12(frame, w, h)
    }

    /// Frames `lo..=hi` (clamped to the video), decoded in one go.
    fn decode_run(&self, lo: FrameIndex, hi: FrameIndex) -> anyhow::Result<Vec<(FrameIndex, Frame)>> {
        let last = self.spec.grid.frame_count() - 1;
        let (lo, hi) = (lo.max(0), hi.min(last));
        let mut stream = tt_media::FrameStream::start(&self.spec.video, self.presented(lo), &self.spec.decode)?;
        let (mut held, mut buf) = (None, Vec::new());
        let mut out = Vec::new();
        for f in lo..=hi {
            if self.cancelled() {
                break;
            }
            self.read_to(&mut stream, &mut held, &mut buf, f)?;
            out.push((f, self.colour_frame(&buf)));
        }
        Ok(out)
    }

    /// What the paints show (learned once for both jobs). None: cancelled.
    pub(super) fn cursor_model(&self) -> anyhow::Result<Option<std::sync::Arc<Model>>> {
        let s = &self.spec;
        let key = model_key(&s.video.path, &s.looks, s.k, s.lo, s.lo + s.guide.len() as FrameIndex);
        let slot = model_slot(key);
        let mut held = slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = held.as_ref() {
            let _ = self.tx.send(super::Msg::CursorShapes(m.learned()));
            return Ok(Some(m.clone()));
        }
        let k = s.k;
        let mut paints = Vec::new();
        for look in s.looks.iter() {
            let frames = self.decode_run(look.frame - NEAR, look.frame + NEAR)?;
            if self.cancelled() {
                return Ok(None);
            }
            let Some((_, here)) = frames.iter().find(|(f, _)| *f == look.frame) else { continue };
            let Some(shot) = Shot::of_paint(look, k, here) else { continue };
            let near: Vec<Shot> = frames.iter().filter(|(f, _)| (*f - look.frame).abs() == NEAR).filter_map(|(f, img)| Shot::near_paint(look, k, *f, img)).collect();
            let mut shot = shot;
            shot.mark_moving(&near);
            paints.push(PaintShots::new(shot, look.pattern, near));
        }
        let mut model = learn(paints, CENTRED * k[0].max(k[1]));
        // (Shown at once; learning more takes a few seconds.)
        let _ = self.tx.send(super::Msg::CursorShapes(model.learned()));
        // Learn more where the shapes are found for sure, across the range.
        if !model.shapes.is_empty() {
            let finder = Finder::new(model.shapes.clone());
            // (Near the paints: a cursor looks the same there; a long video may change.)
            let first = s.looks.iter().map(|l| l.frame).min().unwrap_or(s.anchor);
            let last = s.looks.iter().map(|l| l.frame).max().unwrap_or(s.anchor);
            let pad = (last - first).max(GROW_PAD);
            let (lo, hi) = ((first - pad).max(s.lo), (last + pad).min(s.lo + s.guide.len() as FrameIndex - 1));
            let mut found: Vec<Vec<(Shot, [f64; 2])>> = vec![Vec::new(); model.shapes.len()];
            for n in 0..GROW_FRAMES {
                let f = lo + ((hi - lo) as f64 * (n as f64 + 0.5) / GROW_FRAMES as f64) as FrameIndex;
                if s.looks.iter().any(|l| l.frame == f) {
                    continue;
                }
                let frame = self.decode_one(f)?;
                if self.cancelled() {
                    return Ok(None);
                }
                let frame = self.colour_frame(&frame);
                let region = self.cursor_region(f, frame.y.w, frame.y.h);
                let pyramid = Pyramid::new(frame, finder.levels);
                for hit in finder.find(&pyramid, region, 1, SURE) {
                    let sh = &model.shapes[hit.shape];
                    found[hit.shape].push(Shot::found(f, &pyramid.frame, hit.at, sh.w, sh.h));
                }
            }
            for (i, shots) in found.into_iter().enumerate() {
                model.grow(i, shots);
            }
        }
        tracing::info!("{}: learned {} cursor pattern(s) from {} paint(s)", s.label, model.shapes.len(), s.looks.len());
        let _ = self.tx.send(super::Msg::CursorShapes(model.learned()));
        let model = std::sync::Arc::new(model);
        *held = Some(model.clone());
        Ok(Some(model))
    }

    /// Where frame `f` is searched (rendition px `[x0, y0, x1, y1]`): the
    /// whole frame, or with a guide its box (× the search setting, and a
    /// margin for the shape).
    fn cursor_region(&self, f: FrameIndex, w: usize, h: usize) -> [f64; 4] {
        if self.spec.root {
            return [0.0, 0.0, w as f64, h as f64];
        }
        let (g, k, s) = (self.guide(f), self.spec.k, self.spec.search);
        let (hw, hh) = ((g[4] - g[2]) / 2.0 * s + 48.0, (g[5] - g[3]) / 2.0 * s + 48.0);
        [((g[0] - hw) * k[0]).max(0.0), ((g[1] - hh) * k[1]).max(0.0), ((g[0] + hw) * k[0]).min(w as f64), ((g[1] + hh) * k[1]).min(h as f64)]
    }

    /// One frame: the cursor found (or not visible: lost, held where it was), sent.
    fn cursor_frame(&mut self, finder: &Finder, chooser: &mut Chooser, f: FrameIndex, frame: Frame) {
        let region = self.cursor_region(f, frame.y.w, frame.y.h);
        let pyramid = Pyramid::new(frame, finder.levels);
        let min = self.spec.settings.min_score;
        let found = finder.find(&pyramid, region, 3, min);
        let k = self.spec.k;
        let src = |p: [f64; 2]| [p[0] / k[0], p[1] / k[1]];
        let (pos, rect, score, lost) = match chooser.choose(&found, min, REACH) {
            Some(hit) => {
                let sh = &finder.shapes[hit.shape];
                (src(hit.tip), [src(hit.at), src([hit.at[0] + sh.w as f64, hit.at[1] + sh.h as f64])], hit.score, false)
            }
            None => {
                let at = chooser.prev.map(src).unwrap_or_else(|| {
                    let g = self.guide(f);
                    [g[0], g[1]]
                });
                (at, [[at[0] - 8.0, at[1] - 8.0], [at[0] + 8.0, at[1] + 8.0]], found.first().map_or(0.0, |h| h.score), true)
            }
        };
        let g = self.guide(f);
        let outside = !self.spec.root && (!(g[2]..=g[4]).contains(&pos[0]) || !(g[3]..=g[5]).contains(&pos[1]));
        let flags = if lost { crate::LOST } else { 0 } | if outside { crate::OUTSIDE } else { 0 };
        self.out.push((f, [pos[0], pos[1], rect[0][0], rect[0][1], rect[1][0], rect[1][1], score as f64, flags as f64].map(|v| v as f32)));
        self.shared.at.store(f, std::sync::atomic::Ordering::Relaxed);
        if self.out.len() >= super::FLUSH_FRAMES || self.flushed.elapsed() >= super::FLUSH_EVERY {
            self.flush();
        }
    }

    /// A cursor tracker's job: learn (or take what the other job learned), then find it frame by frame.
    pub(super) fn run_cursor(&mut self) -> anyhow::Result<()> {
        let Some(model) = self.cursor_model()? else { return Ok(()) };
        if model.shapes.is_empty() {
            anyhow::bail!("no pattern learned from the paints: paint over the cursor on a few frames where it is on different backgrounds, or paint one tightly");
        }
        let finder = Finder::new(model.shapes.clone());
        let mut chooser = Chooser::default();
        let k = self.spec.k;
        if let Some((p, _)) = self.spec.resume {
            chooser.prev = Some([p[0] * k[0], p[1] * k[1]]);
        }
        self.shared.set_phase(super::Phase::Tracking);
        let (from, to) = (self.spec.from, self.spec.to);
        match self.spec.side {
            super::Side::Forward => {
                let mut stream: Option<tt_media::FrameStream> = None;
                let (mut held, mut buf) = (None, Vec::new());
                for f in from..=to {
                    if !self.wait_for(f, || stream = None) {
                        return Ok(());
                    }
                    if stream.is_none() {
                        held = None;
                        stream = Some(tt_media::FrameStream::start(&self.spec.video, self.presented(f), &self.spec.decode)?);
                    }
                    self.read_to(stream.as_mut().expect("opened"), &mut held, &mut buf, f)?;
                    let frame = self.colour_frame(&buf);
                    self.cursor_frame(&finder, &mut chooser, f, frame);
                }
            }
            super::Side::Backward => {
                let mut hi = from;
                while hi >= to {
                    let lo = (hi - BACK_SEGMENT + 1).max(to);
                    let frames = self.decode_run(lo, hi)?;
                    for (f, img) in frames.into_iter().rev() {
                        if !self.wait_for(f, || {}) {
                            return Ok(());
                        }
                        self.cursor_frame(&finder, &mut chooser, f, img);
                    }
                    if self.cancelled() {
                        return Ok(());
                    }
                    hi = lo - 1;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 12 × 18 arrow: a white body in a black outline (1 = outline, 2 = body).
    fn arrow() -> Vec<Vec<u8>> {
        (0..18)
            .map(|y: usize| {
                (0..12)
                    .map(|x: usize| {
                        let inside = x <= y * 2 / 3 && y < 16 && x < 11;
                        let edge = x == 0 || x == y * 2 / 3 || y == 15;
                        if !inside { 0 } else if edge { 1 } else { 2 }
                    })
                    .collect()
            })
            .collect()
    }

    /// A frame `w × h` of busy background (`seed` changes it) with the arrow's tip at `tip`.
    fn frame(w: usize, h: usize, seed: u32, tip: [usize; 2]) -> Img {
        let mut px: Vec<f32> = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f32, (i / w) as f32);
                // (Blocky noise, a different pattern per seed.)
                let (bx, by) = ((x / 3.0) as u32, (y / 3.0) as u32);
                let mut v = bx.wrapping_mul(73_856_093).wrapping_add(by.wrapping_mul(19_349_663)).wrapping_add(seed.wrapping_mul(2_654_435_761));
                for _ in 0..3 {
                    v ^= v >> 15;
                    v = v.wrapping_mul(0x2c1b_3c6d);
                    v ^= v >> 12;
                }
                30.0 + (v % 200) as f32
            })
            .collect();
        for (y, row) in arrow().iter().enumerate() {
            for (x, c) in row.iter().enumerate() {
                if *c > 0 {
                    px[(tip[1] + y) * w + tip[0] + x] = if *c == 1 { 10.0 } else { 245.0 };
                }
            }
        }
        Img { w, h, px }
    }

    /// A loose paint: a 70 px square around a point a few px off the arrow.
    fn paint(f: FrameIndex, tip: [usize; 2]) -> LookSpec {
        LookSpec { frame: f, center: [tip[0] as f64 + 9.0, tip[1] as f64 + 4.0], half: [35.0, 35.0], mask: None, pattern: 0 }
    }

    /// A paint on a frame of background `seed` with the arrow at `tip`, as
    /// the job makes it: with the frames just before and after, where the
    /// arrow is `moves` away (the screen the same).
    fn painted(f: FrameIndex, seed: u32, tip: [usize; 2], moves: [[i64; 2]; 2]) -> PaintShots {
        let look = paint(f, tip);
        let mut shot = Shot::of_paint(&look, [1.0, 1.0], &Frame::grey(frame(220, 180, seed, tip))).expect("shot");
        let near: Vec<Shot> = moves
            .iter()
            .map(|m| frame(220, 180, seed, [(tip[0] as i64 + m[0]) as usize, (tip[1] as i64 + m[1]) as usize]))
            .map(|img| Shot::near_paint(&look, [1.0, 1.0], f + 1, &Frame::grey(img)).expect("near"))
            .collect();
        shot.mark_moving(&near);
        PaintShots::new(shot, 0, near)
    }

    /// Loose paints on three frames, each a different (busy) background, the
    /// arrow moving: the arrow is learned (its outline sure, the background
    /// around it not), and found on a fourth frame, exactly, its tip at the
    /// arrow's point.
    #[test]
    fn learns_the_arrow_from_loose_paints_and_finds_it() {
        let tips = [[60, 50], [130, 90], [40, 120], [150, 30]];
        let shots: Vec<PaintShots> = (0..3).map(|i| painted(i as i64 * 10, i as u32 + 1, tips[i], [[14, 7], [-15, -9]])).collect();
        let model = learn(shots, CENTRED);
        assert_eq!(model.shapes.len(), 1, "one shape");
        let s = &model.shapes[0];
        assert!(s.w <= 16 && s.h <= 22, "about the arrow's size: {}×{}", s.w, s.h);
        let sure = s.alpha.iter().filter(|a| **a > 0.5).count();
        assert!(sure >= 80, "most of the arrow: {sure} px");
        let finder = Finder::new(model.shapes.clone());
        let img = frame(220, 180, 7, tips[3]);
        let found = finder.find(&Pyramid::new(Frame::grey(img), finder.levels), [0.0, 0.0, 220.0, 180.0], 3, 0.6);
        let f = found.first().expect("found");
        assert!(f.score > 0.9, "score {}", f.score);
        let truth = [tips[3][0] as f64 + 0.5, tips[3][1] as f64];
        assert!((f.tip[0] - truth[0]).abs() < 1.01 && (f.tip[1] - truth[1]).abs() < 1.01, "tip {:?} vs {:?}", f.tip, truth);
    }

    /// Learning more ([`Model::grow`]) takes a find of the arrow on a new
    /// background, not one in another colour (a yellow twin of the arrow:
    /// its body would be lost), nor one a lot dimmer: those are left out.
    #[test]
    fn a_twin_in_another_colour_or_brightness_is_not_learned_from() {
        let tips = [[60, 50], [130, 90], [40, 120]];
        let shots: Vec<PaintShots> = (0..3).map(|i| painted(i as i64 * 10, i as u32 + 1, tips[i], [[14, 7], [-15, -9]])).collect();
        let mut model = learn(shots, CENTRED);
        assert_eq!(model.shapes.len(), 1);
        let finder = Finder::new(model.shapes.clone());
        let plain = Frame::grey(frame(220, 180, 7, [150, 30]));
        let hit = *finder.find(&Pyramid::new(plain.clone(), finder.levels), [0.0, 0.0, 220.0, 180.0], 1, 0.6).first().expect("found");
        let sh = model.shapes[0].clone();
        let find = |f: &Frame| Shot::found(0, f, hit.at, sh.w, sh.h);
        // The same pixels, yellow; the same pixels, a third as bright.
        let yellow = Frame { y: plain.y.clone(), u: Img { px: vec![40.0; plain.u.px.len()], ..plain.u.clone() }, v: Img { px: vec![160.0; plain.v.px.len()], ..plain.v.clone() } };
        let dim = Frame { y: Img { px: plain.y.px.iter().map(|p| 16.0 + (p - 16.0) / 3.0).collect(), ..plain.y.clone() }, ..plain.clone() };
        for odd in [&yellow, &dim] {
            model.grow(0, vec![find(odd)]);
            assert_eq!(model.shapes[0], sh, "learned from a find that isn't it");
        }
        model.grow(0, vec![find(&plain)]);
        assert_eq!(model.shapes[0].shots, sh.shots + 1, "the arrow on a new background is learned from");
    }

    /// One paint and the frames near it (the cursor moved, the screen
    /// didn't): those are enough to learn the arrow.
    #[test]
    fn one_paint_and_the_frames_near_it() {
        let model = learn(vec![painted(10, 3, [80, 70], [[12, 6], [-14, -8]])], CENTRED);
        assert_eq!(model.shapes.len(), 1);
        let s = &model.shapes[0];
        assert!(s.w <= 16 && s.h <= 22, "about the arrow's size: {}×{}", s.w, s.h);
        let finder = Finder::new(model.shapes.clone());
        let img = frame(220, 180, 9, [150, 40]);
        let f = *finder.find(&Pyramid::new(Frame::grey(img), finder.levels), [0.0, 0.0, 220.0, 180.0], 3, 0.6).first().expect("found");
        assert!((f.tip[0] - 150.5).abs() < 1.01 && (f.tip[1] - 40.0).abs() < 1.01, "tip {:?}", f.tip);
    }

    /// The cursor is the same picture on every paint, so what isn't is
    /// background (on request: "white backgrounds should not be merging into
    /// the cursor … there's a painted area where the cursor is clearly not on
    /// a white background … sometimes there is a loading blue windows circle
    /// next to it but on other frames there isn't"): three paints on a white
    /// page and one on a dark busy one, a spinner beside the arrow on two of
    /// them. Only the arrow is learned: no white around it, no spinner.
    #[test]
    fn what_any_paint_shows_otherwise_is_background() {
        let (w, h) = (220usize, 180usize);
        // A white page, or the busy blocks darkened; the arrow at `tip`; the spinner (mid grey, a ring) beside it.
        let make = |white: bool, seed: u32, tip: [usize; 2], spinner: bool| {
            let mut img = frame(w, h, seed, tip);
            let shape = arrow();
            for y in 0..h {
                for x in 0..w {
                    let i = y * w + x;
                    let on = (x as i64 - tip[0] as i64, y as i64 - tip[1] as i64);
                    if (0..12).contains(&on.0) && (0..18).contains(&on.1) && shape[on.1 as usize][on.0 as usize] > 0 {
                        continue;
                    }
                    img.px[i] = if white { 240.0 } else { img.px[i] * 0.35 };
                    let r = ((on.0 - 16) as f64).hypot((on.1 - 20) as f64);
                    if spinner && (4.0..7.0).contains(&r) {
                        img.px[i] = 105.0;
                    }
                }
            }
            img
        };
        let paints: Vec<PaintShots> = [(true, 1, [60usize, 40usize], true), (true, 2, [120, 70], false), (true, 3, [50, 110], true), (false, 4, [140, 30], false)]
            .iter()
            .enumerate()
            .map(|(k, (white, seed, tip, spinner))| {
                let look = LookSpec { frame: k as i64 * 10, center: [tip[0] as f64 + 12.0, tip[1] as f64 + 12.0], half: [36.0, 36.0], mask: None, pattern: 0 };
                let mut shot = Shot::of_paint(&look, [1.0, 1.0], &Frame::grey(make(*white, *seed, *tip, *spinner))).expect("shot");
                let near: Vec<Shot> = [[14i64, 7i64], [-15, -9]]
                    .iter()
                    .map(|m| make(*white, *seed, [(tip[0] as i64 + m[0]) as usize, (tip[1] as i64 + m[1]) as usize], *spinner))
                    .map(|img| Shot::near_paint(&look, [1.0, 1.0], look.frame + 1, &Frame::grey(img)).expect("near"))
                    .collect();
                shot.mark_moving(&near);
                PaintShots::new(shot, 0, near)
            })
            .collect();
        let model = learn(paints, CENTRED);
        assert_eq!(model.shapes.len(), 1);
        let s = &model.shapes[0];
        // Where the arrow is in the shape: found on a fresh dark frame with a spinner, its tip there.
        let finder = Finder::new(model.shapes.clone());
        let f = *finder.find(&Pyramid::new(Frame::grey(make(false, 9, [100, 80], true)), finder.levels), [0.0, 0.0, w as f64, h as f64], 3, 0.6).first().expect("found");
        assert!((f.tip[0] - 100.5).abs() < 1.01 && (f.tip[1] - 80.0).abs() < 1.01, "tip {:?}", f.tip);
        let (ox, oy) = (100.0 - f.at[0], 80.0 - f.at[1]);
        let shape = arrow();
        let (mut stray, mut sure) = (Vec::new(), 0);
        for y in 0..s.h {
            for x in 0..s.w {
                if s.alpha[y * s.w + x] <= 0.5 {
                    continue;
                }
                sure += 1;
                // (In the arrow's pixels, a pixel of slack for the fraction it was lined up by.)
                let (ax, ay) = ((x as f64 - ox).round() as i64, (y as f64 - oy).round() as i64);
                let near_arrow = (-1..=1).any(|dy| {
                    (-1..=1).any(|dx| {
                        let (px, py) = (ax + dx, ay + dy);
                        (0..12).contains(&px) && (0..18).contains(&py) && shape[py as usize][px as usize] > 0
                    })
                });
                if !near_arrow {
                    stray.push((ax, ay, s.value[y * s.w + x] as i64));
                }
            }
        }
        assert!(sure >= 80, "the arrow: {sure} px");
        assert!(stray.is_empty(), "sure pixels off the arrow (white page, spinner): {stray:?}");
    }

    /// A look-alike that stays put loses to the cursor, which moves: also
    /// as the cursor leaves it, having rested on it.
    #[test]
    fn a_look_alike_that_stays_put_loses_ties() {
        let mut c = Chooser::default();
        let f = |tip: [f64; 2], score: f32| Found { shape: 0, at: tip, tip, score };
        let decoy = [100.0, 100.0];
        let mut last = None;
        for x in 0..10 {
            last = c.choose(&[f([20.0 + 3.0 * x as f64, 30.0], 0.97), f(decoy, 0.99)], 0.6, 40.0);
        }
        // (It matches better, but stays put while the cursor moves.)
        assert_eq!(last.map(|p| p.tip), Some([47.0, 30.0]));
        // It rests on the look-alike …
        assert_eq!(c.choose(&[f(decoy, 0.99)], 0.6, 40.0).map(|p| p.tip), Some(decoy));
        // … and flicks away: both match, it was on the look-alike, yet it goes.
        assert_eq!(c.choose(&[f(decoy, 0.99), f([300.0, 200.0], 0.97)], 0.6, 40.0).map(|p| p.tip), Some([300.0, 200.0]));
    }
}

