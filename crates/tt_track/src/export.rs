//! Export: two trackers as a stabilizer for DaVinci Resolve's Fusion page.
//!
//! The two points give, per frame, a position (their midpoint) and an angle
//! (the line through them). A Fusion `Transform` tool, animated on every frame
//! where both points are good, moves and turns each frame so the two points
//! land where they are on the reference frame: the picture holds still in
//! position and rotation, as Resolve's own Tracker does with "Steady Position"
//! and "Steady Angle". It is pasted into the Fusion page as text (a
//! `.setting`), between MediaIn and MediaOut.
//!
//! Fusion's coordinates: `Center` is normalized (0–1 across the frame, y up)
//! and is where the image's centre goes; `Angle` is in degrees,
//! counter-clockwise, about the pivot (left at the image's centre). The
//! rotation is in pixels, so the math here is in pixels (y up) and only the
//! result is normalized. Frames where either point is missing, lost or outside
//! get no key: Fusion interpolates linearly across them.

use bevy_ecs::prelude::*;
use std::fmt::Write;
use tt_core::time::FrameIndex;

/// One tracker's point on each frame it has a good one (source pixels, y down).
pub fn good_points(world: &World, tracker: Entity) -> Vec<(FrameIndex, [f64; 2])> {
    let Some(sig) = tt_core::span::output(world, tracker) else { return Vec::new() };
    let Some((lo, hi)) = sig.present_hull() else { return Vec::new() };
    (lo..=hi)
        .filter_map(|f| {
            let v = sig.get(f)?;
            (crate::flags(v) == 0 && v[0].is_finite() && v[1].is_finite()).then(|| (f, [v[0] as f64, v[1] as f64]))
        })
        .collect()
}

/// A stabilizing transform per frame: where the image's centre goes
/// (normalized, y up) and the angle (degrees, counter-clockwise, unwrapped).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Steady {
    pub frame: FrameIndex,
    pub center: [f64; 2],
    pub angle: f64,
}

/// The transforms that hold the line from `a` to `b` where it is on frame
/// `reference` (or, if either point is missing there, the first frame both
/// have). `size` is the source's width and height in pixels.
pub fn steady(a: &[(FrameIndex, [f64; 2])], b: &[(FrameIndex, [f64; 2])], size: [f64; 2], reference: FrameIndex) -> Vec<Steady> {
    let [w, h] = size;
    let lookup: std::collections::HashMap<FrameIndex, [f64; 2]> = b.iter().copied().collect();
    // Both points, y up, per frame both have.
    let both: Vec<(FrameIndex, [f64; 2], [f64; 2])> =
        a.iter().filter_map(|&(f, p)| lookup.get(&f).map(|q| (f, [p[0], h - p[1]], [q[0], h - q[1]]))).collect();
    let Some(&(_, ra, rb)) = both.iter().find(|(f, ..)| *f == reference).or(both.first()) else { return Vec::new() };
    let mid = |p: [f64; 2], q: [f64; 2]| [(p[0] + q[0]) / 2.0, (p[1] + q[1]) / 2.0];
    let angle = |p: [f64; 2], q: [f64; 2]| (q[1] - p[1]).atan2(q[0] - p[0]);
    let (m_ref, t_ref) = (mid(ra, rb), angle(ra, rb));
    let c = [w / 2.0, h / 2.0];
    let mut out = Vec::with_capacity(both.len());
    let mut last: Option<f64> = None;
    for (f, p, q) in both {
        // Turn by φ (counter-clockwise) about the image's centre, then move
        // the centre so this frame's midpoint lands on the reference's.
        let mut phi = t_ref - angle(p, q);
        if let Some(prev) = last {
            // Unwrapped: no 360° jump between neighbouring keys.
            phi -= std::f64::consts::TAU * ((phi - prev) / std::f64::consts::TAU).round();
        }
        last = Some(phi);
        let m = mid(p, q);
        let (s, co) = phi.sin_cos();
        let d = [m[0] - c[0], m[1] - c[1]];
        let centre = [m_ref[0] - (co * d[0] - s * d[1]), m_ref[1] - (s * d[0] + co * d[1])];
        out.push(Steady { frame: f, center: [centre[0] / w, centre[1] / h], angle: phi.to_degrees() });
    }
    out
}

/// The Fusion `.setting` text: a Transform named `name` animated with
/// `keys`. A key for source frame `f` goes on Fusion frame `f - first`
/// (`first`: the source frame the Fusion clip starts on).
pub fn fusion_setting(name: &str, keys: &[Steady], first: FrameIndex) -> String {
    let spline = |out: &mut String, id: &str, value: &dyn Fn(&Steady) -> f64, colour: (u8, u8, u8)| {
        let _ = writeln!(out, "\t\t{id} = BezierSpline {{");
        let _ = writeln!(out, "\t\t\tSplineColor = {{ Red = {}, Green = {}, Blue = {} }},", colour.0, colour.1, colour.2);
        let _ = writeln!(out, "\t\t\tNameSet = true,");
        let _ = writeln!(out, "\t\t\tKeyFrames = {{");
        // Linear keys, with the handles Fusion writes for them (a third of the
        // way to each neighbour, on the line).
        let at = |k: &Steady| ((k.frame - first) as f64, value(k));
        for (i, k) in keys.iter().enumerate() {
            let (t, v) = at(k);
            let mut handles = String::new();
            if let Some(p) = i.checked_sub(1).map(|j| at(&keys[j])) {
                let _ = write!(handles, " LH = {{ {:.7}, {:.7} }},", t + (p.0 - t) / 3.0, v + (p.1 - v) / 3.0);
            }
            if let Some(n) = keys.get(i + 1).map(at) {
                let _ = write!(handles, " RH = {{ {:.7}, {:.7} }},", t + (n.0 - t) / 3.0, v + (n.1 - v) / 3.0);
            }
            let _ = writeln!(out, "\t\t\t\t[{}] = {{ {v:.7},{handles} Flags = {{ Linear = true }} }},", k.frame - first);
        }
        let _ = writeln!(out, "\t\t\t}}");
        let _ = writeln!(out, "\t\t}},");
    };
    let mut s = String::from("{\n\tTools = ordered() {\n");
    spline(&mut s, &format!("{name}CenterX"), &|k| k.center[0], (250, 59, 49));
    spline(&mut s, &format!("{name}CenterY"), &|k| k.center[1], (252, 206, 35));
    let _ = write!(
        s,
        "\t\t{name}Center = XYPath {{\n\t\t\tShowKeyPoints = false,\n\t\t\tDrawMode = \"ModifyOnly\",\n\t\t\tInputs = {{\n\
         \t\t\t\tX = Input {{ SourceOp = \"{name}CenterX\", Source = \"Value\", }},\n\
         \t\t\t\tY = Input {{ SourceOp = \"{name}CenterY\", Source = \"Value\", }},\n\t\t\t}},\n\t\t}},\n"
    );
    spline(&mut s, &format!("{name}Angle"), &|k| k.angle, (116, 192, 252));
    let _ = write!(
        s,
        "\t\t{name} = Transform {{\n\t\t\tNameSet = true,\n\t\t\tInputs = {{\n\
         \t\t\t\tCenter = Input {{ SourceOp = \"{name}Center\", Source = \"Value\", }},\n\
         \t\t\t\tAngle = Input {{ SourceOp = \"{name}Angle\", Source = \"Value\", }},\n\t\t\t}},\n\
         \t\t\tViewInfo = OperatorInfo {{ Pos = {{ 220, 50 }} }},\n\t\t}},\n\t}},\n\tActiveTool = \"{name}\"\n}}\n"
    );
    s
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
        let keys = steady(&a, &b, size, 30);
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
    fn the_setting_has_a_transform_with_a_key_per_good_frame() {
        let keys = [Steady { frame: 10, center: [0.5, 0.5], angle: 0.0 }, Steady { frame: 12, center: [0.51, 0.49], angle: -1.5 }];
        let s = fusion_setting("Stabilize", &keys, 10);
        assert!(s.contains("Stabilize = Transform {"));
        assert!(s.contains("Center = Input { SourceOp = \"StabilizeCenter\""));
        assert!(s.contains("[0] = { 0.5000000, RH = { 0.6666667, 0.5033333 }, Flags"), "{s}");
        assert!(s.contains("[2] = { -1.5000000, LH = { 1.3333333, -1.0000000 }, Flags"), "{s}");
        assert_eq!(s.matches('{').count(), s.matches('}').count());
    }
}
