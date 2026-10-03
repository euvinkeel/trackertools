//! Export: trackers as a stabilizer for DaVinci Resolve's Fusion page.
//!
//! Two or more trackers give, per frame, the rotation and shift that best take
//! their points on the reference frame to where they are on this frame (a
//! least-squares rigid fit): the camera's motion. A Fusion `Transform` tool,
//! animated on every frame where at least two trackers are good, undoes it, so
//! the points land where they are on the reference frame: the picture holds
//! still in position and rotation, as Resolve's own Tracker does with "Steady
//! Position" and "Steady Angle". It is pasted into the Fusion page as text (a
//! `.setting`), between MediaIn and MediaOut.
//!
//! Anything with a point a frame counts as a tracker here: a sketch's point is
//! the hand's path, as refined as it was drawn. One alone holds its point
//! still in position only (a rotation takes two).
//!
//! A subject (tt_core::subject) already has a path and an angle, the
//! pushed motion of its members plus its own keys: [`stabilize_path`] and
//! [`follow_path`] take that final data as it is.
//!
//! The same motion, applied instead of undone, is a *follower* ([`follow`],
//! [`fusion_follow_setting`]): a Merge whose foreground (a Text+ to start
//! with) moves, and with two or more points turns, with what was tracked,
//! over footage left as it is.
//!
//! The rotation is only as precise as the trackers are far apart: its error is
//! theirs over their spread (two points 90 px apart, each off by 0.15 px, turn
//! the picture by 0.1° at random). More trackers, farther apart, measure it
//! better. A spring ([`spring`], zero-phase) can smooth the measured motion
//! first, rotation and position apart: the stabilizer then lets motion faster
//! than the spring through, which takes out the trackers' jitter (and the
//! correction of real shake that fast).
//!
//! Fusion's coordinates: `Center` is normalized (0–1 across the frame, y up)
//! and is where the image's centre goes; `Angle` is in degrees,
//! counter-clockwise, about the pivot (left at the image's centre). The
//! rotation is in pixels, so the math here is in pixels (y up) and only the
//! result is normalized. Frames where fewer than two trackers are good get no
//! key: Fusion interpolates linearly across them.
//!
//! Time. A comp's frame numbers are not the source's, so the keys sit on
//! source frames and the Transform looks them up itself. Measured in Resolve
//! 21 (free) with frame-numbered clips, through its scripting bridge:
//! - A clip opened on the Fusion page gets a comp at the clip's own frame rate
//!   (a 50 fps clip on a 60 fps timeline: a 50 fps comp; Resolve converts
//!   after Fusion), whose frame 0 is the clip's first frame in the edit: a
//!   clip trimmed to start on source frame 37 has `comp.GlobalStart` = −37.
//! - A Fusion Clip's comp runs at the timeline's rate from the Fusion Clip's
//!   start and shows source frame `start + floor(t × source fps / timeline
//!   fps)`. Nothing in the comp says what `start` is.
//!
//! So `Center` and `Angle` are expressions that read three curves keyed on
//! source frames (hidden inputs `SourceX`, `SourceY`, `SourceAngle`) at
//! `SourceFrame`, an expression too ([`SOURCE_FRAME`]). It shows in the
//! Inspector, next to *Clip Starts At Source Frame* (−1: from the trim; set it
//! for a Fusion Clip) and *Source FPS*.

use bevy_ecs::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::f64::consts::TAU;
use std::fmt::Write;
use tt_core::time::FrameIndex;

/// How much the stabilizer smooths the motion it measured before undoing it:
/// seconds of [`spring`] (0 holds the trackers exactly still). A user setting;
/// the app remembers it.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct StabilizerDefaults {
    pub smooth_position: f32,
    pub smooth_rotation: f32,
}

impl Default for StabilizerDefaults {
    /// Position held exactly; rotation through a light spring, which takes
    /// most of the jitter out of trackers close together and barely touches
    /// trackers far apart (on real footage with two points 90 px apart, 0.05 s
    /// cut the stabilized picture's frame-to-frame turn from 0.35° to 0.11°, p95).
    fn default() -> Self {
        Self { smooth_position: 0.0, smooth_rotation: 0.05 }
    }
}

/// One tracker's point on each frame it has a good one (source pixels, y
/// down); for any other box producer (a sketch), its point on every frame it
/// has one (they have no flags).
pub fn good_points(world: &World, tracker: Entity) -> Vec<(FrameIndex, [f64; 2])> {
    tt_core::subject::points_of(world, tracker)
}

/// A subject's path (tt_core::subject), its final data: on each frame it
/// has one, its point (source px, y down) and its angle (radians, clockwise
/// on screen).
pub fn subject_path(world: &World, subject: Entity) -> Vec<(FrameIndex, [f64; 2], f64)> {
    let Some(sig) = tt_core::span::output(world, subject) else { return Vec::new() };
    let Some((lo, hi)) = sig.present_hull() else { return Vec::new() };
    (lo..=hi).filter_map(|f| sig.get(f).filter(|v| v.len() > 6 && v[0].is_finite() && v[1].is_finite()).map(|v| (f, [v[0] as f64, v[1] as f64], v[6] as f64))).collect()
}

/// A stabilizing transform per frame: where the image's centre goes
/// (normalized, y up) and the angle (degrees, counter-clockwise, unwrapped).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Steady {
    pub frame: FrameIndex,
    pub center: [f64; 2],
    pub angle: f64,
}

/// Seconds of spring on the measured motion's position and rotation ([`stabilize`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Smoothing {
    pub position: f64,
    pub rotation: f64,
}

impl From<StabilizerDefaults> for Smoothing {
    fn from(d: StabilizerDefaults) -> Self {
        Self { position: d.smooth_position.max(0.0) as f64, rotation: d.smooth_rotation.max(0.0) as f64 }
    }
}

/// What [`stabilize`] or [`follow`] made.
#[derive(Clone, Debug)]
pub struct Stabilization {
    pub keys: Vec<Steady>,
    /// The frame held still (the stabilizer), or where the follower starts
    /// from its points' place (the follower).
    pub reference: FrameIndex,
    /// How many trackers took part (one never on a frame with another can't).
    pub used: usize,
    /// The trackers' spread on the reference frame (px): the root-mean-square
    /// distance of their points from their centre, the rotation's lever.
    pub spread: f64,
    /// The measured rotation's noise, before smoothing (degrees a frame, a
    /// robust estimate): the trackers' error over their spread, plus any roll
    /// faster than a few frames.
    pub jitter: f64,
}

/// The camera's motion on one frame: a point `p` of the reference frame is at
/// R(angle)(p − anchor) + at (px, y up; angle in radians, counter-clockwise).
#[derive(Clone, Copy, Debug)]
struct Motion {
    angle: f64,
    at: [f64; 2],
}

/// The least-squares rotation and shift taking each pair's first point to its
/// second (at least two pairs; px, y up), as the angle and where `anchor` goes.
fn rigid(pairs: &[([f64; 2], [f64; 2])], anchor: [f64; 2]) -> Motion {
    let n = pairs.len() as f64;
    let (mut from, mut to) = ([0.0; 2], [0.0; 2]);
    for (a, b) in pairs {
        from = [from[0] + a[0] / n, from[1] + a[1] / n];
        to = [to[0] + b[0] / n, to[1] + b[1] / n];
    }
    let (mut sin, mut cos) = (0.0, 0.0);
    for (a, b) in pairs {
        let (p, q) = ([a[0] - from[0], a[1] - from[1]], [b[0] - to[0], b[1] - to[1]]);
        sin += p[0] * q[1] - p[1] * q[0];
        cos += p[0] * q[0] + p[1] * q[1];
    }
    let angle = sin.atan2(cos);
    let (s, c) = angle.sin_cos();
    let d = [anchor[0] - from[0], anchor[1] - from[1]];
    Motion { angle, at: [c * d[0] - s * d[1] + to[0], s * d[0] + c * d[1] + to[1]] }
}

/// The transforms that hold the trackers' points (one list per tracker, from
/// [`good_points`]) where they are on frame `reference` (or, if fewer than two
/// are there, the first frame with two). `size` is the source's width and
/// height in pixels, `fps` its rate (for the smoothing's seconds).
///
/// A tracker with no point on the reference frame still takes part: its place
/// there is where the others say the camera put it, averaged over the frames
/// it shares with them. A single tracker holds its point in position only
/// (the angle stays 0). None if no frame has two trackers (one, for one).
pub fn stabilize(tracks: &[Vec<(FrameIndex, [f64; 2])>], size: [f64; 2], reference: FrameIndex, smoothing: Smoothing, fps: f64) -> Option<Stabilization> {
    Some(undone(&measure(tracks, size, reference, smoothing, fps)?, size))
}

/// The stabilizer for a subject's path ([`subject_path`]): it holds the
/// subject still in position and angle, as it is on frame `reference` (or its
/// first frame). Smoothing and the rest as [`stabilize`]'s.
pub fn stabilize_path(path: &[(FrameIndex, [f64; 2], f64)], size: [f64; 2], reference: FrameIndex, smoothing: Smoothing, fps: f64) -> Option<Stabilization> {
    Some(undone(&measure_path(path, size, reference, smoothing, fps, true)?, size))
}

/// The follower for a subject's path: on each frame its point, and its angle
/// (as Fusion's, counter-clockwise).
pub fn follow_path(path: &[(FrameIndex, [f64; 2], f64)], size: [f64; 2], reference: FrameIndex, smoothing: Smoothing, fps: f64) -> Option<Stabilization> {
    Some(applied(&measure_path(path, size, reference, smoothing, fps, false)?, size))
}

/// The keys that undo `m`'s motion (the stabilizer's).
fn undone(m: &Measured, size: [f64; 2]) -> Stabilization {
    let [w, h] = size;
    // Undo the (smoothed) motion: turn by −angle about the image's centre, and
    // put the centre where that brings the anchor back to its reference place.
    let c = [w / 2.0, h / 2.0];
    let keys = m
        .frames
        .iter()
        .map(|&f| {
            let i = (f - m.first) as usize;
            let phi = -m.angle[i];
            let (s, co) = phi.sin_cos();
            let d = [c[0] - m.at[i][0], c[1] - m.at[i][1]];
            let centre = [co * d[0] - s * d[1] + m.anchor[0], s * d[0] + co * d[1] + m.anchor[1]];
            Steady { frame: f, center: [centre[0] / w, centre[1] / h], angle: phi.to_degrees() }
        })
        .collect();
    Stabilization { keys, reference: m.reference, used: m.used, spread: m.spread, jitter: m.jitter }
}

/// The keys that move something with the trackers' points (the follower):
/// on each frame, where their anchor (the centre of their points on the
/// reference frame) has gone, normalized and y up as Fusion's `Center` is,
/// and how far they have turned since the reference frame (degrees,
/// counter-clockwise, as Fusion's `Angle`). One point: where it is, angle 0.
/// Arguments and None as [`stabilize`]'s.
pub fn follow(tracks: &[Vec<(FrameIndex, [f64; 2])>], size: [f64; 2], reference: FrameIndex, smoothing: Smoothing, fps: f64) -> Option<Stabilization> {
    Some(applied(&measure(tracks, size, reference, smoothing, fps)?, size))
}

/// The keys that apply `m`'s motion (the follower's).
fn applied(m: &Measured, size: [f64; 2]) -> Stabilization {
    let [w, h] = size;
    let keys = m
        .frames
        .iter()
        .map(|&f| {
            let i = (f - m.first) as usize;
            Steady { frame: f, center: [m.at[i][0] / w, m.at[i][1] / h], angle: m.angle[i].to_degrees() }
        })
        .collect();
    Stabilization { keys, reference: m.reference, used: m.used, spread: m.spread, jitter: m.jitter }
}

/// A subject's path as a measurement: its point (y up) and its angle
/// (counter-clockwise, unwrapped), on every frame from its first to its last
/// (gaps linear), smoothed. `relative`: the angle from the reference frame's
/// (the stabilizer holds it as it is there); else as it is (the follower).
fn measure_path(path: &[(FrameIndex, [f64; 2], f64)], size: [f64; 2], reference: FrameIndex, smoothing: Smoothing, fps: f64, relative: bool) -> Option<Measured> {
    let h = size[1];
    let (lo, hi) = (path.first()?.0, path.last()?.0);
    let reference = if path.iter().any(|p| p.0 == reference) { reference } else { lo };
    let n = (hi - lo + 1) as usize;
    let mut known = vec![None; n];
    let mut last: Option<f64> = None;
    for &(f, [x, y], a) in path {
        // y down, clockwise → y up, counter-clockwise.
        let mut a = -a;
        if let Some(prev) = last {
            a -= TAU * ((a - prev) / TAU).round();
        }
        last = Some(a);
        known[(f - lo) as usize] = Some([a, x, h - y]);
    }
    let filled = fill_linear(&known);
    let at_ref = filled[(reference - lo) as usize];
    let channel = |k: usize| filled.iter().map(|v| v[k]).collect::<Vec<f64>>();
    let mut angle = channel(0);
    if relative {
        angle.iter_mut().for_each(|a| *a -= at_ref[0]);
    }
    let jitter = {
        let mut d2: Vec<f64> = (1..n.saturating_sub(1)).map(|i| (angle[i - 1] - 2.0 * angle[i] + angle[i + 1]).abs()).collect();
        d2.sort_by(f64::total_cmp);
        d2.get(d2.len() / 2).map_or(0.0, |m| (m * 1.4826 / 6f64.sqrt()).to_degrees())
    };
    let (angle, ax, ay) = (spring(&angle, smoothing.rotation * fps), spring(&channel(1), smoothing.position * fps), spring(&channel(2), smoothing.position * fps));
    let at: Vec<[f64; 2]> = ax.into_iter().zip(ay).map(|(x, y)| [x, y]).collect();
    let anchor = at[(reference - lo) as usize];
    Some(Measured { frames: path.iter().map(|p| p.0).collect(), first: lo, angle, at, anchor, reference, used: 1, spread: 0.0, jitter })
}

/// The points' motion, smoothed, on every frame from the first fitted to the
/// last: a point `p` of the reference frame is at R(angle)(p − anchor) + at
/// (px, y up). `frames`: those with a fit (the keys).
struct Measured {
    frames: Vec<FrameIndex>,
    first: FrameIndex,
    angle: Vec<f64>,
    at: Vec<[f64; 2]>,
    anchor: [f64; 2],
    reference: FrameIndex,
    used: usize,
    spread: f64,
    jitter: f64,
}

fn measure(tracks: &[Vec<(FrameIndex, [f64; 2])>], size: [f64; 2], reference: FrameIndex, smoothing: Smoothing, fps: f64) -> Option<Measured> {
    let h = size[1];
    let pts: Vec<HashMap<FrameIndex, [f64; 2]>> = tracks.iter().map(|t| t.iter().map(|&(f, p)| (f, [p[0], h - p[1]])).collect()).collect();
    let frames: BTreeSet<FrameIndex> = pts.iter().flat_map(|m| m.keys().copied()).collect();
    // Points a frame needs: two for a rotation, or one when there is only one
    // (a fit to one point is the shift alone: its angle comes out 0).
    let need = pts.len().min(2);
    let present = |f: FrameIndex| pts.iter().filter(|m| m.contains_key(&f)).count();
    let reference = if present(reference) >= need { reference } else { frames.iter().copied().find(|&f| present(f) >= need)? };
    let mut refs: Vec<Option<[f64; 2]>> = pts.iter().map(|m| m.get(&reference).copied()).collect();
    let centre = |refs: &[Option<[f64; 2]>]| {
        let known: Vec<[f64; 2]> = refs.iter().flatten().copied().collect();
        let n = known.len() as f64;
        known.iter().fold([0.0; 2], |s, p| [s[0] + p[0] / n, s[1] + p[1] / n])
    };
    let fit = |refs: &[Option<[f64; 2]>], anchor: [f64; 2]| -> BTreeMap<FrameIndex, Motion> {
        frames
            .iter()
            .filter_map(|&f| {
                let pairs: Vec<([f64; 2], [f64; 2])> = refs.iter().zip(&pts).filter_map(|(r, m)| Some(((*r)?, *m.get(&f)?))).collect();
                (pairs.len() >= need.max(1)).then(|| (f, rigid(&pairs, anchor)))
            })
            .collect()
    };
    let mut anchor = centre(&refs);
    let mut motion = fit(&refs, anchor);
    // Trackers not on the reference frame: their place on it, from where the
    // others say the camera was on the frames they share.
    let mut placed = false;
    for (r, m) in refs.iter_mut().zip(&pts).filter(|(r, _)| r.is_none()) {
        let back: Vec<[f64; 2]> = m
            .iter()
            .filter_map(|(f, q)| {
                let mo = motion.get(f)?;
                let (s, c) = (-mo.angle).sin_cos();
                let d = [q[0] - mo.at[0], q[1] - mo.at[1]];
                Some([c * d[0] - s * d[1] + anchor[0], s * d[0] + c * d[1] + anchor[1]])
            })
            .collect();
        if !back.is_empty() {
            let n = back.len() as f64;
            *r = Some(back.iter().fold([0.0; 2], |s, p| [s[0] + p[0] / n, s[1] + p[1] / n]));
            placed = true;
        }
    }
    if placed {
        anchor = centre(&refs);
        motion = fit(&refs, anchor);
    }
    let used = refs.iter().flatten().count();
    let spread = (refs.iter().flatten().map(|p| (p[0] - anchor[0]).powi(2) + (p[1] - anchor[1]).powi(2)).sum::<f64>() / used as f64).sqrt();

    // The motion on every frame from the first fitted to the last (gaps
    // linear), the angle unwrapped (no 360° jump between neighbours), smoothed.
    let (&lo, &hi) = (motion.keys().next()?, motion.keys().next_back()?);
    let n = (hi - lo + 1) as usize;
    let mut known = vec![None; n];
    let mut last: Option<f64> = None;
    for (&f, mo) in &motion {
        let mut a = mo.angle;
        if let Some(prev) = last {
            a -= TAU * ((a - prev) / TAU).round();
        }
        last = Some(a);
        known[(f - lo) as usize] = Some([a, mo.at[0], mo.at[1]]);
    }
    let filled = fill_linear(&known);
    let channel = |k: usize| filled.iter().map(|v| v[k]).collect::<Vec<f64>>();
    let (angle, ax, ay) = (channel(0), channel(1), channel(2));
    // The angle's noise: second differences of white noise σ have σ√6; their median is robust to a few real jolts.
    let mut d2: Vec<f64> = (1..n.saturating_sub(1))
        .filter(|&i| known[i - 1].is_some() && known[i].is_some() && known[i + 1].is_some())
        .map(|i| (angle[i - 1] - 2.0 * angle[i] + angle[i + 1]).abs())
        .collect();
    d2.sort_by(f64::total_cmp);
    let jitter = d2.get(d2.len() / 2).map_or(0.0, |m| (m * 1.4826 / 6f64.sqrt()).to_degrees());
    let (angle, ax, ay) = (spring(&angle, smoothing.rotation * fps), spring(&ax, smoothing.position * fps), spring(&ay, smoothing.position * fps));
    let at = ax.into_iter().zip(ay).map(|(x, y)| [x, y]).collect();
    Some(Measured { frames: motion.keys().copied().collect(), first: lo, angle, at, anchor, reference, used, spread, jitter })
}

/// A key's values on frame `f`: linear between keys, held beyond them (as
/// Fusion's linear curves do). None without keys.
pub fn key_at(keys: &[Steady], f: FrameIndex) -> Option<Steady> {
    let (first, last) = (keys.first()?, keys.last()?);
    if f <= first.frame {
        return Some(Steady { frame: f, ..*first });
    }
    if f >= last.frame {
        return Some(Steady { frame: f, ..*last });
    }
    let i = keys.partition_point(|k| k.frame <= f);
    let (a, b) = (&keys[i - 1], &keys[i]);
    let u = (f - a.frame) as f64 / (b.frame - a.frame) as f64;
    let mix = |x: f64, y: f64| x + (y - x) * u;
    Some(Steady { frame: f, center: [mix(a.center[0], b.center[0]), mix(a.center[1], b.center[1])], angle: mix(a.angle, b.angle) })
}

/// What a stabilizer key does, rendered: the map from output pixels to
/// source pixels (both y down; `tt_media::render::Affine`) of the Fusion
/// Transform it stands for, zoomed in by `zoom` about the picture's centre.
pub fn stabilizer_map(key: &Steady, size: [f64; 2], zoom: f64) -> [f64; 6] {
    let [w, h] = size;
    let (s, c) = key.angle.to_radians().sin_cos();
    let (cx, cy) = (key.center[0] * w, key.center[1] * h);
    // The Transform (y up) is q = R(φ)(p − mid) + centre: undone, p = R(−φ)(q − centre) + mid.
    let source = |qx: f64, qy: f64| {
        let (zx, zy) = (w / 2.0 + (qx - w / 2.0) / zoom, h / 2.0 + (qy - h / 2.0) / zoom);
        let (ux, uy) = (zx - cx, (h - zy) - cy);
        [c * ux + s * uy + w / 2.0, h - (-s * ux + c * uy + h / 2.0)]
    };
    let (o, ex, ey) = (source(0.0, 0.0), source(1.0, 0.0), source(0.0, 1.0));
    [ex[0] - o[0], ey[0] - o[0], o[0], ex[1] - o[1], ey[1] - o[1], o[1]]
}

/// The least zoom (from 1 up to `max`) at which the stabilized picture
/// covers the whole frame on every one of `frames`: no black edges.
pub fn zoom_to_fill(keys: &[Steady], size: [f64; 2], frames: std::ops::Range<FrameIndex>, max: f64) -> f64 {
    let [w, h] = size;
    let covers = |zoom: f64| {
        frames.clone().filter_map(|f| key_at(keys, f)).all(|k| {
            let m = stabilizer_map(&k, size, zoom);
            [[0.0, 0.0], [w, 0.0], [0.0, h], [w, h]].iter().all(|q| {
                let (x, y) = (m[0] * q[0] + m[1] * q[1] + m[2], m[3] * q[0] + m[4] * q[1] + m[5]);
                (-1e-6..=w + 1e-6).contains(&x) && (-1e-6..=h + 1e-6).contains(&y)
            })
        })
    };
    if covers(1.0) {
        return 1.0;
    }
    if !covers(max) {
        return max;
    }
    let (mut lo, mut hi) = (1.0, max);
    for _ in 0..40 {
        let mid = (lo + hi) / 2.0;
        if covers(mid) { hi = mid } else { lo = mid }
    }
    hi
}

/// Where a follower's keys put the thing on frame `f`: its point (source px,
/// y down) and its turn (radians, clockwise on screen).
pub fn follower_at(keys: &[Steady], size: [f64; 2], f: FrameIndex) -> Option<([f64; 2], f64)> {
    let k = key_at(keys, f)?;
    Some(([k.center[0] * size[0], size[1] - k.center[1] * size[1]], -k.angle.to_radians()))
}

/// Linear interpolation across the `None` runs between known values (the
/// first and last are known).
fn fill_linear(v: &[Option<[f64; 3]>]) -> Vec<[f64; 3]> {
    let mut out: Vec<[f64; 3]> = Vec::with_capacity(v.len());
    let mut prev: Option<usize> = None;
    for (i, x) in v.iter().enumerate() {
        let Some(x) = x else { continue };
        if let Some(p) = prev {
            let a = v[p].expect("known");
            for k in p + 1..i {
                let u = (k - p) as f64 / (i - p) as f64;
                out.push(std::array::from_fn(|j| a[j] + (x[j] - a[j]) * u));
            }
        }
        out.push(*x);
        prev = Some(i);
    }
    out
}

/// A critically damped spring following `v` (a value a frame), run forwards
/// and then backwards over its own output, so it doesn't lag (zero-phase);
/// `tau` is its time constant in frames (≤ 0: `v` as it is). The ends are
/// padded with an odd reflection and the spring starts moving with them, so
/// a steady drift runs through unchanged instead of bending to a standstill.
pub fn spring(v: &[f64], tau: f64) -> Vec<f64> {
    if tau <= 0.0 || v.len() < 3 {
        return v.to_vec();
    }
    let pad = ((8.0 * tau).ceil() as usize).min(v.len() - 1);
    let (a, z) = (v[0], v[v.len() - 1]);
    let mut x: Vec<f64> = (1..=pad).rev().map(|k| 2.0 * a - v[k]).collect();
    x.extend_from_slice(v);
    x.extend((1..=pad).map(|k| 2.0 * z - v[v.len() - 1 - k]));
    // Exact over a frame with the target held: x → u + (d + (v + ωd))e^−ω.
    let (w, e) = (1.0 / tau, (-1.0 / tau).exp());
    let pass = |x: &mut [f64]| {
        // Started as it follows a steady drift m: L behind, at speed V (the
        // fixed point of the step above for a target moving m a frame).
        let m = x[1] - x[0];
        let k = 1.0 - e + e * w;
        let lag = m / (1.0 - e - e * w + e * e * w * w / k);
        let (mut p, mut vel) = (x[0] - lag, e * w * w * lag / k);
        for u in x.iter_mut() {
            let d = p - *u;
            let j = vel + w * d;
            p = *u + (d + j) * e;
            vel = (vel - w * j) * e;
            *u = p;
        }
    };
    pass(&mut x);
    x.reverse();
    pass(&mut x);
    x.reverse();
    x[pad..pad + v.len()].to_vec()
}

/// The Fusion expression for the source frame shown on the comp's frame
/// `time` (see the module doc). With `ClipStart` −1 it is the comp's time
/// counted from the clip's first frame (`time − comp.GlobalStart`, in the
/// clip's frames: the trim comes with it); otherwise `ClipStart` plus the
/// comp's time. Either is converted from the comp's rate to the source's by
/// time, floored as Resolve picks frames; at equal rates it is left as it is
/// (fractional on motion-blur sub-frames). Equal rates are the usual case: a
/// clip of the tracked file opened on the Fusion page. They differ in a
/// Fusion Clip (the timeline's rate) or with another encode of the footage.
pub const SOURCE_FRAME: &str = "iif(abs(SourceFPS - comp:GetPrefs(\"Comp.FrameFormat.Rate\")) < 0.01, \
     iif(ClipStart < 0, time - comp.GlobalStart, ClipStart + time), \
     iif(ClipStart < 0, floor((time - comp.GlobalStart) * SourceFPS / comp:GetPrefs(\"Comp.FrameFormat.Rate\") + 0.0001), \
     ClipStart + floor(time * SourceFPS / comp:GetPrefs(\"Comp.FrameFormat.Rate\") + 0.0001)))";

/// The Fusion `.setting` text: a Transform named `name` animated with
/// `keys` (on source frames of a `source_fps` source), finding its frame in
/// the source by itself (module doc).
pub fn fusion_setting(name: &str, keys: &[Steady], source_fps: f64) -> String {
    let (curves, inputs, controls) = animated(name, keys, source_fps);
    format!(
        "{{\n\tTools = ordered() {{\n{curves}\t\t{name} = Transform {{\n\t\t\tNameSet = true,\n\t\t\tInputs = {{\n{inputs}\t\t\t}},\n\
         \t\t\tViewInfo = OperatorInfo {{ Pos = {{ 220, 50 }} }},\n\t\t\tUserControls = ordered() {{\n{controls}\t\t\t}},\n\t\t}},\n\t}},\n\tActiveTool = \"{name}\"\n}}\n"
    )
}

/// The follower's Fusion `.setting` text ([`follow`]'s keys): a Merge named
/// `name` whose `Center` and `Angle` follow the keys, its foreground a Text+
/// (`{name}Text`) to start with. Pasted with MediaIn1 selected, the footage is
/// its background; anything can replace the Text+ on its foreground.
pub fn fusion_follow_setting(name: &str, keys: &[Steady], source_fps: f64) -> String {
    let (curves, inputs, controls) = animated(name, keys, source_fps);
    format!(
        "{{\n\tTools = ordered() {{\n{curves}\
         \t\t{name}Text = TextPlus {{\n\t\t\tInputs = {{\n\t\t\t\tStyledText = Input {{ Value = \"Text\", }},\n\t\t\t\tSize = Input {{ Value = 0.08, }},\n\t\t\t}},\n\
         \t\t\tViewInfo = OperatorInfo {{ Pos = {{ 220, -16 }} }},\n\t\t}},\n\
         \t\t{name} = Merge {{\n\t\t\tNameSet = true,\n\t\t\tInputs = {{\n\t\t\t\tForeground = Input {{ SourceOp = \"{name}Text\", Source = \"Output\", }},\n{inputs}\t\t\t}},\n\
         \t\t\tViewInfo = OperatorInfo {{ Pos = {{ 220, 50 }} }},\n\t\t\tUserControls = ordered() {{\n{controls}\t\t\t}},\n\t\t}},\n\t}},\n\tActiveTool = \"{name}\"\n}}\n"
    )
}

/// What both tools share: the curves keyed on source frames (`{name}SourceX`,
/// `…Y`, `…Angle`), the inputs that read them at the tool's source frame
/// (`Center`, `Angle`, the controls, the hidden curve inputs) and the user
/// controls' definitions.
fn animated(name: &str, keys: &[Steady], source_fps: f64) -> (String, String, String) {
    let spline = |out: &mut String, id: &str, value: &dyn Fn(&Steady) -> f64, colour: (u8, u8, u8)| {
        let _ = writeln!(out, "\t\t{id} = BezierSpline {{");
        let _ = writeln!(out, "\t\t\tSplineColor = {{ Red = {}, Green = {}, Blue = {} }},", colour.0, colour.1, colour.2);
        let _ = writeln!(out, "\t\t\tNameSet = true,");
        let _ = writeln!(out, "\t\t\tKeyFrames = {{");
        // Linear keys, with the handles Fusion writes for them (a third of the
        // way to each neighbour, on the line).
        let at = |k: &Steady| (k.frame as f64, value(k));
        for (i, k) in keys.iter().enumerate() {
            let (t, v) = at(k);
            let mut handles = String::new();
            if let Some(p) = i.checked_sub(1).map(|j| at(&keys[j])) {
                let _ = write!(handles, " LH = {{ {:.7}, {:.7} }},", t + (p.0 - t) / 3.0, v + (p.1 - v) / 3.0);
            }
            if let Some(n) = keys.get(i + 1).map(at) {
                let _ = write!(handles, " RH = {{ {:.7}, {:.7} }},", t + (n.0 - t) / 3.0, v + (n.1 - v) / 3.0);
            }
            let _ = writeln!(out, "\t\t\t\t[{}] = {{ {v:.7},{handles} Flags = {{ Linear = true }} }},", k.frame);
        }
        let _ = writeln!(out, "\t\t\t}}");
        let _ = writeln!(out, "\t\t}},");
    };
    let mut curves = String::new();
    spline(&mut curves, &format!("{name}SourceX"), &|k| k.center[0], (250, 59, 49));
    spline(&mut curves, &format!("{name}SourceY"), &|k| k.center[1], (252, 206, 35));
    spline(&mut curves, &format!("{name}SourceAngle"), &|k| k.angle, (116, 192, 252));
    // Lua string literals: the expressions quote the input names they read.
    let lua = |e: &str| e.replace('\\', "\\\\").replace('"', "\\\"");
    let center = lua("Point(self:GetValue(\"SourceX\", SourceFrame), self:GetValue(\"SourceY\", SourceFrame))");
    let angle = lua("self:GetValue(\"SourceAngle\", SourceFrame)");
    let frame = lua(SOURCE_FRAME);
    let inputs = format!(
        "\t\t\t\tCenter = Input {{ Value = {{ 0.5, 0.5 }}, Expression = \"{center}\", }},\n\
         \t\t\t\tAngle = Input {{ Value = 0, Expression = \"{angle}\", }},\n\
         \t\t\t\tClipStart = Input {{ Value = -1, }},\n\
         \t\t\t\tSourceFPS = Input {{ Value = {source_fps}, }},\n\
         \t\t\t\tSourceFrame = Input {{ Value = 0, Expression = \"{frame}\", }},\n\
         \t\t\t\tSourceX = Input {{ SourceOp = \"{name}SourceX\", Source = \"Value\", }},\n\
         \t\t\t\tSourceY = Input {{ SourceOp = \"{name}SourceY\", Source = \"Value\", }},\n\
         \t\t\t\tSourceAngle = Input {{ SourceOp = \"{name}SourceAngle\", Source = \"Value\", }},\n"
    );
    let control = |id: &str, label: &str, extra: &str| {
        format!("\t\t\t\t{id} = {{ LINKS_Name = \"{label}\", LINKID_DataType = \"Number\", INPID_InputControl = \"ScrewControl\", ICS_ControlPage = \"Controls\",{extra} }},\n")
    };
    let hidden = " INP_Default = 0, IC_Visible = false,";
    let controls = [
        control("ClipStart", "Clip Starts At Source Frame (-1 = from the trim)", " INP_Integer = true, INP_Default = -1, INP_MinAllowed = -1, INP_MinScale = -1, INP_MaxScale = 10000,"),
        control("SourceFPS", "Source FPS", &format!(" INP_Default = {source_fps}, INP_MinAllowed = 1, INP_MinScale = 1, INP_MaxScale = 240,")),
        control("SourceFrame", "Source Frame (trackertools' frame number)", " INP_Default = 0,"),
        control("SourceX", "Source Center X", hidden),
        control("SourceY", "Source Center Y", hidden),
        control("SourceAngle", "Source Angle", hidden),
    ]
    .concat();
    (curves, inputs, controls)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies a key the way Fusion's Transform does (pivot at the centre):
    /// p ↦ R(φ)(p − c) + centre, in pixels, y up.
    fn apply(k: &Steady, p: [f64; 2], [w, h]: [f64; 2]) -> [f64; 2] {
        let (s, co) = k.angle.to_radians().sin_cos();
        let d = [p[0] - w / 2.0, p[1] - h / 2.0];
        [co * d[0] - s * d[1] + k.center[0] * w, s * d[0] + co * d[1] + k.center[1] * h]
    }

    #[test]
    fn both_points_land_where_they_are_on_the_reference_frame() {
        let size = [1920.0, 1080.0];
        // A camera that drifts and rolls (past ±180° too): both points turn about a wandering spot.
        let frame = |f: i64| {
            let t = f as f64;
            let (cx, cy, roll) = (900.0 + 3.0 * t, 500.0 - 2.0 * t, (t * 4.0).to_radians());
            let at = |dx: f64, dy: f64| [cx + dx * roll.cos() - dy * roll.sin(), cy + dx * roll.sin() + dy * roll.cos()];
            (at(-120.0, -40.0), at(80.0, 150.0))
        };
        let a: Vec<_> = (0..100).map(|f| (f, frame(f).0)).collect();
        let b: Vec<_> = (0..100).filter(|f| f % 9 != 4).map(|f| (f, frame(f).1)).collect();
        let st = stabilize(&[a, b.clone()], size, 30, Smoothing::default(), 30.0).expect("a stabilization");
        assert_eq!((st.reference, st.used), (30, 2));
        let keys = st.keys;
        assert_eq!(keys.len(), b.len(), "a key wherever both points are good");
        let up = |p: [f64; 2]| [p[0], size[1] - p[1]];
        let (ra, rb) = frame(30);
        for k in &keys {
            let (pa, pb) = frame(k.frame);
            for (p, r) in [(pa, ra), (pb, rb)] {
                let got = apply(k, up(p), size);
                let want = up(r);
                assert!((got[0] - want[0]).hypot(got[1] - want[1]) < 1e-6, "frame {}: {got:?} vs {want:?}", k.frame);
            }
        }
        assert!(keys.windows(2).all(|w| (w[1].angle - w[0].angle).abs() < 10.0), "unwrapped");
        let r = keys.iter().find(|k| k.frame == 30).unwrap();
        assert!(r.angle.abs() < 1e-9 && (r.center[0] - 0.5).abs() < 1e-9 && (r.center[1] - 0.5).abs() < 1e-9, "the reference frame stays as it is");
    }

    #[test]
    fn any_number_of_trackers_land_where_they_are_on_the_reference_frame() {
        let size = [3840.0, 2160.0];
        // Five points of a still scene; the camera drifts and rolls.
        let scene = [[-600.0, -300.0], [500.0, -250.0], [40.0, 420.0], [-200.0, 150.0], [700.0, 380.0]];
        let at = |f: i64, s: [f64; 2]| {
            let t = f as f64;
            let (roll, cx, cy) = (0.1 * (0.05 * t).sin(), 1900.0 + 40.0 * (0.03 * t).sin(), 1080.0 + 25.0 * (0.05 * t).cos());
            [cx + s[0] * roll.cos() - s[1] * roll.sin(), cy + s[0] * roll.sin() + s[1] * roll.cos()]
        };
        // The fifth starts after the reference frame, the fourth stops before the end.
        let tracks: Vec<Vec<(i64, [f64; 2])>> = (0..5)
            .map(|i| (0..200).filter(|&f| (i != 4 || f >= 120) && (i != 3 || f < 150)).map(|f| (f, at(f, scene[i]))).collect())
            .collect();
        let st = stabilize(&tracks, size, 60, Smoothing::default(), 50.0).expect("a stabilization");
        assert_eq!((st.reference, st.used, st.keys.len()), (60, 5, 200));
        let up = |p: [f64; 2]| [p[0], size[1] - p[1]];
        for k in &st.keys {
            for s in scene {
                let (got, want) = (apply(k, up(at(k.frame, s)), size), up(at(60, s)));
                assert!((got[0] - want[0]).hypot(got[1] - want[1]) < 1e-6, "frame {}: {got:?} vs {want:?}", k.frame);
            }
        }
        assert!((st.spread - 559.8).abs() < 0.1, "spread {}", st.spread);
    }

    #[test]
    fn one_point_alone_is_held_in_position_only() {
        // A hand-drawn path (a sketch's point): it wanders, and skips some frames.
        let size = [1920.0, 1080.0];
        let path: Vec<(i64, [f64; 2])> = (0..120)
            .filter(|f| f % 17 != 5)
            .map(|f| (f, [900.0 + 80.0 * (0.07 * f as f64).sin(), 500.0 + 40.0 * (0.05 * f as f64).cos()]))
            .collect();
        let st = stabilize(std::slice::from_ref(&path), size, 40, Smoothing::default(), 50.0).expect("a stabilization");
        assert_eq!((st.reference, st.used, st.keys.len()), (40, 1, path.len()));
        let up = |p: [f64; 2]| [p[0], size[1] - p[1]];
        let held = up(path.iter().find(|(f, _)| *f == 40).unwrap().1);
        for (k, (f, p)) in st.keys.iter().zip(&path) {
            assert_eq!((k.frame, k.angle), (*f, 0.0), "no rotation from one point");
            let got = apply(k, up(*p), size);
            assert!((got[0] - held[0]).hypot(got[1] - held[1]) < 1e-9, "frame {f}: {got:?} vs {held:?}");
        }
    }

    #[test]
    fn trackers_farther_apart_and_the_spring_steady_the_rotation() {
        // The camera rolls slowly (±2°); each tracked point is off by up to ±0.25 px at random.
        let size = [3840.0, 2160.0];
        let roll = |f: i64| 2f64.to_radians() * (0.7 * f as f64 / 50.0).sin();
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        let mut noise = move || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 0.5
        };
        let mut tracks = |scene: &[[f64; 2]]| -> Vec<Vec<(i64, [f64; 2])>> {
            scene
                .iter()
                .map(|s| {
                    (0..250)
                        .map(|f| {
                            let r = roll(f);
                            (f, [1920.0 + s[0] * r.cos() - s[1] * r.sin() + noise(), 1080.0 + s[0] * r.sin() + s[1] * r.cos() + noise()])
                        })
                        .collect()
                })
                .collect()
        };
        // How much the stabilized picture still turns from one frame to the next (rms, degrees):
        // the key turns it by its angle, the camera by its roll (y down: clockwise on screen).
        let wobble = |st: &Stabilization| {
            let left: Vec<f64> = st.keys.iter().map(|k| k.angle - roll(k.frame).to_degrees()).collect();
            (left.windows(2).map(|w| (w[1] - w[0]).powi(2)).sum::<f64>() / (left.len() - 1) as f64).sqrt()
        };
        let close = tracks(&[[-45.0, 0.0], [45.0, 0.0]]);
        let far = tracks(&[[-600.0, -300.0], [600.0, -300.0], [-600.0, 300.0], [600.0, 300.0]]);
        let none = Smoothing::default();
        let (c, f) = (stabilize(&close, size, 100, none, 50.0).unwrap(), stabilize(&far, size, 100, none, 50.0).unwrap());
        let sprung = stabilize(&close, size, 100, Smoothing { position: 0.0, rotation: 0.05 }, 50.0).unwrap();
        let (wc, wf, ws) = (wobble(&c), wobble(&f), wobble(&sprung));
        eprintln!("rotation wobble: two points 90 px apart {wc:.4}°, four far apart {wf:.4}°, the close two through a 0.05 s spring {ws:.4}°");
        assert!(wf < wc / 10.0, "farther apart, steadier: {wf} vs {wc}");
        assert!(ws < wc / 3.0, "the spring takes the jitter out: {ws} vs {wc}");
        assert!(c.jitter > 5.0 * f.jitter, "the jitter estimate tells them apart: {} vs {}", c.jitter, f.jitter);
        // And the spring still follows the camera's roll (±2°): what's left of it varies little.
        // (Its mean is the reference frame's own error, held on every frame: a still tilt.)
        let left: Vec<f64> = sprung.keys.iter().map(|k| k.angle - roll(k.frame).to_degrees()).collect();
        let mean = left.iter().sum::<f64>() / left.len() as f64;
        let rms = (left.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / left.len() as f64).sqrt();
        assert!(rms < 0.06, "the roll is still taken out: {rms}° left (rms)");
    }

    #[test]
    fn the_follower_goes_where_the_points_go_and_turns_with_them() {
        let size = [1920.0, 1080.0];
        // Two points on something that moves and turns (y down, as tracked).
        let at = |f: i64, s: [f64; 2]| {
            let t = f as f64;
            let (turn, cx, cy) = (0.02 * t, 600.0 + 5.0 * t, 400.0 + 2.0 * t);
            [cx + s[0] * turn.cos() - s[1] * turn.sin(), cy + s[0] * turn.sin() + s[1] * turn.cos()]
        };
        let tracks: Vec<Vec<(i64, [f64; 2])>> = [[-50.0, 0.0], [50.0, 0.0]].iter().map(|s| (0..60).map(|f| (f, at(f, *s))).collect()).collect();
        let fl = follow(&tracks, size, 10, Smoothing::default(), 50.0).expect("a follower");
        for k in &fl.keys {
            // The anchor is the points' centre: the moving thing's centre here.
            let c = at(k.frame, [0.0, 0.0]);
            assert!((k.center[0] * size[0] - c[0]).abs() < 1e-6 && (k.center[1] * size[1] - (size[1] - c[1])).abs() < 1e-6, "frame {}", k.frame);
            // Turning clockwise on screen (y down) is a negative Fusion angle.
            assert!((k.angle + (0.02 * (k.frame - 10) as f64).to_degrees()).abs() < 1e-6, "frame {}: {}", k.frame, k.angle);
        }
        // One point: just where it is.
        let one = follow(&tracks[..1], size, 10, Smoothing::default(), 50.0).expect("a follower");
        assert!(one.keys.iter().all(|k| k.angle == 0.0 && (k.center[0] * size[0] - at(k.frame, [-50.0, 0.0])[0]).abs() < 1e-6));
        // The Fusion text: a Merge following the curves, a Text+ on its foreground.
        let s = fusion_follow_setting("Follow", &fl.keys, 50.0);
        assert!(s.contains("FollowText = TextPlus {") && s.contains("StyledText = Input { Value = \"Text\", }"), "{s}");
        assert!(s.contains("Follow = Merge {") && s.contains("Foreground = Input { SourceOp = \"FollowText\", Source = \"Output\", }"), "{s}");
        assert!(s.contains("Center = Input { Value = { 0.5, 0.5 }, Expression = \"Point(self:GetValue(") && s.contains("ActiveTool = \"Follow\""));
        assert_eq!(s.matches('{').count(), s.matches('}').count());
        assert_eq!(s.matches('"').count() % 2, 0);
    }

    #[test]
    fn a_subjects_path_is_held_still_or_followed_as_it_is() {
        let size = [1920.0, 1080.0];
        // A subject moving and turning clockwise on screen (y down) 1° a frame.
        let path: Vec<(i64, [f64; 2], f64)> = (0..50).map(|f| (f, [500.0 + 3.0 * f as f64, 400.0 - f as f64], (f as f64).to_radians())).collect();
        let held = stabilize_path(&path, size, 20, Smoothing::default(), 50.0).expect("a stabilizer");
        let up = |p: [f64; 2]| [p[0], size[1] - p[1]];
        for (k, (f, p, _)) in held.keys.iter().zip(&path) {
            // Its point lands where it is on frame 20, and the picture turns back by its turn since then.
            let got = apply(k, up(*p), size);
            let want = up(path[20].1);
            assert!((got[0] - want[0]).hypot(got[1] - want[1]) < 1e-6, "frame {f}");
            assert!((k.angle - (*f - 20) as f64).abs() < 1e-9, "frame {f}: {}", k.angle);
        }
        let followed = follow_path(&path, size, 20, Smoothing::default(), 50.0).expect("a follower");
        for (k, (f, p, a)) in followed.keys.iter().zip(&path) {
            assert!((k.center[0] * size[0] - p[0]).abs() < 1e-9 && (k.center[1] * size[1] - (size[1] - p[1])).abs() < 1e-9, "frame {f}");
            assert!((k.angle + a.to_degrees()).abs() < 1e-9, "Fusion turns the other way: frame {f}");
        }
    }

    #[test]
    fn the_rendered_map_is_the_fusion_transform_undone_and_the_zoom_hides_the_edges() {
        let size = [1920.0, 1080.0];
        let keys = [
            Steady { frame: 0, center: [0.52, 0.47], angle: 3.0 },
            Steady { frame: 10, center: [0.48, 0.51], angle: -4.0 },
        ];
        let up = |p: [f64; 2]| [p[0], size[1] - p[1]];
        for f in [0, 4, 10, 25] {
            let k = key_at(&keys, f).expect("a key");
            let m = stabilizer_map(&k, size, 1.0);
            for q in [[100.0, 200.0], [1500.0, 900.0], [960.0, 540.0]] {
                // The source pixel the render takes for q, through Fusion's Transform, lands on q.
                let p = [m[0] * q[0] + m[1] * q[1] + m[2], m[3] * q[0] + m[4] * q[1] + m[5]];
                let back = apply(&k, up(p), size);
                assert!((back[0] - q[0]).abs() < 1e-6 && (back[1] - (size[1] - q[1])).abs() < 1e-6, "frame {f}: {q:?} → {p:?} → {back:?}");
            }
        }
        assert_eq!(key_at(&keys, 5).expect("a key").angle, -0.5, "linear between keys");
        // Zoomed in just enough, every corner of the output shows the picture; a little less, one doesn't.
        let z = zoom_to_fill(&keys, size, 0..30, 3.0);
        assert!(z > 1.05 && z < 1.5, "{z}");
        assert!(zoom_to_fill(&keys, size, 0..30, z * 0.99) >= z * 0.99, "less doesn't do");
        assert!(zoom_to_fill(&keys, size, 4..6, 3.0) < z, "just the frames in between, nearly as they were: less to hide");
        assert_eq!(zoom_to_fill(&[Steady { frame: 0, center: [0.5, 0.5], angle: 0.0 }], size, 0..5, 3.0), 1.0, "a still picture needs none");
        // A follower's key read back as a point and a turn (y down, clockwise).
        let (at, turn) = follower_at(&keys, size, 0).expect("a point");
        assert!((at[0] - 0.52 * 1920.0).abs() < 1e-9 && (at[1] - (1080.0 - 0.47 * 1080.0)).abs() < 1e-9 && (turn + 3f64.to_radians()).abs() < 1e-12);
    }

    #[test]
    fn the_spring_has_no_lag_and_lets_a_steady_drift_through() {
        let ramp: Vec<f64> = (0..100).map(|i| 3.0 + 0.5 * i as f64).collect();
        let s = spring(&ramp, 4.0);
        let worst = ramp.iter().zip(&s).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
        assert!(worst < 1e-9, "a steady drift goes through, ends included: off by {worst}");
        // A step: zero-phase, so it's crossed half-way, at the step.
        let step: Vec<f64> = (0..200).map(|i| if i < 100 { 0.0 } else { 1.0 }).collect();
        let s = spring(&step, 5.0);
        assert!((s[99] + s[100] - 1.0).abs() < 1e-6, "symmetric about the step: {} {}", s[99], s[100]);
        // Both passes together spread it over about ±2τ.
        assert!(s[70] < 0.01 && s[129] > 0.99, "{} {}", s[70], s[129]);
        assert_eq!(spring(&step, 0.0), step);
    }

    #[test]
    fn the_setting_has_a_transform_with_a_key_per_good_frame() {
        let keys = [Steady { frame: 10, center: [0.5, 0.5], angle: 0.0 }, Steady { frame: 12, center: [0.51, 0.49], angle: -1.5 }];
        let s = fusion_setting("Stabilize", &keys, 50.0);
        assert!(s.contains("Stabilize = Transform {"));
        // Keys on source frames; the Transform reads them at its source frame.
        assert!(s.contains("StabilizeSourceX = BezierSpline {"));
        assert!(s.contains("[10] = { 0.5000000, RH = { 10.6666667, 0.5033333 }, Flags"), "{s}");
        assert!(s.contains("[12] = { -1.5000000, LH = { 11.3333333, -1.0000000 }, Flags"), "{s}");
        assert!(s.contains("SourceAngle = Input { SourceOp = \"StabilizeSourceAngle\""));
        assert!(s.contains(r#"Center = Input { Value = { 0.5, 0.5 }, Expression = "Point(self:GetValue(\"SourceX\", SourceFrame), self:GetValue(\"SourceY\", SourceFrame))", }"#), "{s}");
        assert!(s.contains(r#"Expression = "iif(abs(SourceFPS - comp:GetPrefs(\"Comp.FrameFormat.Rate\")) < 0.01, iif(ClipStart < 0, time - comp.GlobalStart, ClipStart + time), "#), "{s}");
        assert!(s.contains("SourceFPS = Input { Value = 50, }") && s.contains("ClipStart = Input { Value = -1, }"), "{s}");
        assert_eq!(s.matches('{').count(), s.matches('}').count());
        // Every quote inside an expression is escaped, so the Lua strings close where they should.
        assert_eq!(s.matches('"').count() % 2, 0);
    }
}
