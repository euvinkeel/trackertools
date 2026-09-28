//! Accuracy on the cursor fixture (`cargo xtask fixtures`: a mouse cursor
//! over changing stripes, bright scenery, dark foliage with icon changes, and
//! a floor it flicks across), run end to end through the tracker's jobs and
//! measured against the exact hotspot. Skipped when the fixture hasn't been
//! generated.
//!
//! The guide is a rough pass like a sketch: the path smoothed over ~4 frames
//! (it can't follow a flick) plus a slow wander, in a box that grows with
//! speed. The looks are what a user makes with the Track tool: a rectangle
//! around the cursor, masked (the auto mask's job), on the first frame and on
//! a frame of each other icon.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bevy_ecs::name::Name;
use tt_core::op::Output;
use tt_core::signal::SignalStore;
use tt_core::sketch::BOX_CHANNELS;
use tt_core::transport::Transport;
use tt_core::view::SourceSize;
use tt_core::{AppBuilder, Core, CoreModules};
use tt_media::{DecodeOptions, VideoIndex};
use tt_track::look::{Look, MASK_N};
use tt_track::runner::{Footage, settled};
use tt_track::{TrackModule, Tracker};

pub struct Truth {
    pub hotspot: Vec<[f64; 2]>,
    pub icon: Vec<String>,
    pub stretches: Vec<(String, usize, usize)>,
    /// Per icon: its outline around the hotspot.
    pub shapes: Vec<(String, Vec<[f64; 2]>)>,
}

fn fixtures() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TT_FIXTURES") {
        return Some(PathBuf::from(dir)).filter(|p| p.join("cursor_540p60.mp4").exists());
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().map(|d| d.join("fixtures")).find(|p| p.join("cursor_540p60.mp4").exists())
}

fn truth(dir: &std::path::Path) -> Truth {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("cursor_truth.json")).expect("truth")).expect("json");
    let pt = |p: &serde_json::Value| [p[0].as_f64().expect("x"), p[1].as_f64().expect("y")];
    Truth {
        hotspot: v["hotspot"].as_array().expect("hotspot").iter().map(pt).collect(),
        icon: v["icon"].as_array().expect("icon").iter().map(|s| s.as_str().expect("icon").to_string()).collect(),
        stretches: v["stretches"].as_array().expect("stretches").iter().map(|s| (s[0].as_str().expect("name").to_string(), s[1].as_u64().expect("a") as usize, s[2].as_u64().expect("b") as usize)).collect(),
        shapes: v["shapes"].as_array().expect("shapes").iter().map(|s| (s[0].as_str().expect("icon").to_string(), s[1].as_array().expect("poly").iter().map(pt).collect())).collect(),
    }
}

impl Truth {
    fn shape(&self, icon: &str) -> &[[f64; 2]] {
        &self.shapes.iter().find(|(i, _)| i == icon).expect("shape").1
    }

    /// The rectangle a user drags around the icon on frame `f`: its outline's bounds, 2 px out.
    fn rect(&self, f: usize) -> ([f64; 2], [f64; 2]) {
        let poly = self.shape(&self.icon[f]);
        let (lo, hi) = poly.iter().fold(([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]), |(lo, hi), p| ([lo[0].min(p[0]), lo[1].min(p[1])], [hi[0].max(p[0]), hi[1].max(p[1])]));
        let h = self.hotspot[f];
        ([h[0] + (lo[0] + hi[0]) / 2.0, h[1] + (lo[1] + hi[1]) / 2.0], [(hi[0] - lo[0]) / 2.0 + 2.0, (hi[1] - lo[1]) / 2.0 + 2.0])
    }

    /// Where the tracker's point should be on frame `f`: its look's centre
    /// relative to the hotspot, for whichever icon is showing.
    pub fn point(&self, f: usize) -> [f64; 2] {
        let first = (0..self.icon.len()).find(|g| self.icon[*g] == self.icon[f]).expect("icon");
        let (c, _) = self.rect(first);
        let (h0, h) = (self.hotspot[first], self.hotspot[f]);
        [h[0] + c[0] - h0[0], h[1] + c[1] - h0[1]]
    }

    /// A look on frame `f`, masked over the icon (what the auto mask paints).
    pub fn look(&self, f: usize) -> Look {
        let (c, half) = self.rect(f);
        let poly = self.shape(&self.icon[f]);
        let h = self.hotspot[f];
        let mask = (0..MASK_N * MASK_N)
            .map(|k| {
                let (i, j) = ((k % MASK_N) as f64 + 0.5, (k / MASK_N) as f64 + 0.5);
                let p = [c[0] - half[0] + i / MASK_N as f64 * 2.0 * half[0] - h[0], c[1] - half[1] + j / MASK_N as f64 * 2.0 * half[1] - h[1]];
                if inside(p, poly) { 255 } else { 0 }
            })
            .collect();
        Look { mask, ..Look::new(f as i64, c, half) }
    }
}

fn inside(p: [f64; 2], poly: &[[f64; 2]]) -> bool {
    let mut inside = false;
    for i in 0..poly.len() {
        let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
        if (a[1] > p[1]) != (b[1] > p[1]) && p[0] < a[0] + (p[1] - a[1]) / (b[1] - a[1]) * (b[0] - a[0]) {
            inside = !inside;
        }
    }
    inside
}

/// The rough pass: the hotspot smoothed over ±`reach` frames (σ 4: a hand
/// can't follow a flick), a slow wander of a few px, and a box that grows
/// with speed. `[x, y, left, top, right, bottom]` per frame.
fn guide(t: &Truth) -> Vec<[f32; 6]> {
    let n = t.hotspot.len();
    let (sigma, reach) = (4.0f64, 12i64);
    (0..n)
        .map(|f| {
            let (mut acc, mut w) = ([0.0; 2], 0.0);
            for g in f as i64 - reach..=f as i64 + reach {
                let k = (-((g - f as i64) as f64).powi(2) / (2.0 * sigma * sigma)).exp();
                let p = t.hotspot[g.clamp(0, n as i64 - 1) as usize];
                acc = [acc[0] + k * p[0], acc[1] + k * p[1]];
                w += k;
            }
            let s = f as f64 / 60.0;
            let (x, y) = (acc[0] / w + 5.0 + 4.0 * (2.3 * s).sin(), acc[1] / w + 9.0 + 3.0 * (1.7 * s + 0.5).cos());
            let a = t.hotspot[(f + 2).min(n - 1)];
            let b = t.hotspot[f.saturating_sub(2)];
            let speed = (a[0] - b[0]).hypot(a[1] - b[1]) / 4.0;
            let half = (30.0 + 1.5 * speed).min(160.0);
            [x, y, x - half, y - half, x + half, y + half].map(|v| v as f32)
        })
        .collect()
}

/// A tracked frame: its point (source px), score and flags.
pub type Frame = Option<([f64; 2], f32, u32)>;

/// The fixture, the guide (a bare box signal), and a tracker seeded on
/// frame 5 with a look for each other icon, and on each of `patches`.
/// `setup` adjusts its settings.
pub fn track(patches: &[usize], setup: impl FnOnce(&mut Tracker)) -> Option<(Truth, Vec<Frame>, f64)> {
    let Some(dir) = fixtures() else {
        eprintln!("skipped: cursor_540p60.mp4 not found (cargo xtask fixtures, or set TT_FIXTURES)");
        return None;
    };
    let t = truth(&dir);
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core: Core = app.build();
    let index = Arc::new(VideoIndex::open(dir.join("cursor_540p60.mp4")).expect("fixture opens"));
    let w = &mut core.world;
    {
        let mut tr = w.resource_mut::<Transport>();
        tr.fps = index.fps;
        tr.frame_count = index.frame_count();
    }
    w.insert_resource(SourceSize { width: index.width as f64, height: index.height as f64 });
    w.insert_resource(Footage { original: index, proxy: None, decode: DecodeOptions::default() });
    let sig = w.resource_mut::<SignalStore>().create(BOX_CHANNELS);
    {
        let mut store = w.resource_mut::<SignalStore>();
        let s = store.get_mut(sig).expect("created");
        for (f, b) in guide(&t).iter().enumerate() {
            s.set(f as i64, b);
        }
    }
    let g = w.spawn((Name::new("Guide"), Output(sig))).id();
    let op = tt_track::add_tracker_with_look(w, g, t.look(5)).expect("tracker");
    for &f in [340, 380].iter().chain(patches) {
        tt_track::add_look(w, op, t.look(f)).expect("look");
    }
    let mut params = w.get::<Tracker>(op).expect("tracker").clone();
    setup(&mut params);
    w.entity_mut(op).insert(params);
    let start = Instant::now();
    for _ in 0..3 {
        core.run_pre_ui();
    }
    while !settled(&core.world, op) {
        assert!(start.elapsed() < Duration::from_secs(600), "still tracking");
        core.run_pre_ui();
        std::thread::sleep(Duration::from_millis(2));
    }
    let secs = start.elapsed().as_secs_f64();
    let w = &core.world;
    let out = w.resource::<SignalStore>().get(w.get::<Output>(op).expect("output").0).expect("signal");
    let values = (0..t.hotspot.len() as i64).map(|f| out.get(f).map(|v| ([v[0] as f64, v[1] as f64], v[6], tt_track::flags(v)))).collect();
    Some((t, values, secs))
}

/// Per stretch: `(name, median, p95, max error px, share within 3 px, flagged frames)`.
pub fn report(label: &str, t: &Truth, out: &[Frame], secs: f64) -> Vec<(String, f64, f64, f64, f64, usize)> {
    let mut rows = Vec::new();
    let mut all = Vec::new();
    for (name, a, b) in t.stretches.iter().cloned().chain([("all".to_string(), 0, t.hotspot.len())]) {
        let mut e: Vec<f64> = (a..b).map(|f| out[f].map_or(f64::INFINITY, |(p, _, _)| (p[0] - t.point(f)[0]).hypot(p[1] - t.point(f)[1]))).collect();
        let flagged = (a..b).filter(|f| out[*f].is_some_and(|(_, _, fl)| fl != 0)).count();
        let on = e.iter().filter(|v| **v < 3.0).count() as f64 / e.len() as f64;
        e.sort_by(f64::total_cmp);
        let row = (name, e[e.len() / 2], e[e.len() * 95 / 100], e[e.len() - 1], on, flagged);
        if row.0 == "all" {
            all.push(row.clone());
        }
        rows.push(row);
    }
    if std::env::var_os("TT_DUMP").is_some() {
        for (f, v) in out.iter().enumerate() {
            let (p, s, fl) = v.unwrap_or(([f64::NAN; 2], 0.0, 99));
            let e = (p[0] - t.point(f)[0]).hypot(p[1] - t.point(f)[1]);
            if e.is_nan() || e >= 1.0 || fl != 0 {
                eprintln!("  frame {f}: error {e:.2} score {s:.2} flags {fl} at ({:.1}, {:.1}), truth ({:.1}, {:.1})", p[0], p[1], t.point(f)[0], t.point(f)[1]);
            }
        }
    }
    eprintln!("{label} ({:.0} fps):", t.hotspot.len() as f64 / secs);
    for (name, med, p95, max, on, fl) in &rows {
        eprintln!("  {name:9} median {med:6.2} px  p95 {p95:7.2}  max {max:7.2}  within 3 px {:5.1}%  flagged {fl}", on * 100.0);
    }
    rows
}

#[test]
fn tracks_the_cursor_through_everything() {
    let Some((t, out, secs)) = track(&[], |_| {}) else { return };
    let rows = report("cursor fixture", &t, &out, secs);
    for (name, med, _, max, on, _) in &rows {
        match name.as_str() {
            // (It rests on a look-alike, then flicks off it: see below.)
            "decoy" => assert!(*on > 0.97, "{name}: on the cursor on {:.1}% of frames", on * 100.0),
            // What the matching options are for (see below): a yellow twin
            // takes a frame; dimmed to 30%, the cursor is too unlike its look.
            "colour" => assert!(*on > 0.99, "{name}: {:.1}%", on * 100.0),
            "dimmed" | "all" => {}
            _ => assert!(*on == 1.0 && *max < 0.5, "{name}: {:.1}% within 3 px, max {max:.2} px", on * 100.0),
        }
        if name != "dimmed" {
            assert!(*med < 0.15, "{name}: median {med:.2} px");
        }
    }
}

/// The cursor rests on a look-alike, then flicks away: the tracker stays on
/// the look-alike (it scores as well). A look placed where it missed (frame
/// 648, as a user patches) pins that frame; tracked back from it, the
/// frames before it are mended too.
#[test]
fn a_look_where_it_slipped_mends_the_frames_before_it() {
    let Some((t, one_way, secs)) = track(&[648], |p| p.fuse = false) else { return };
    let before = report("patched at 648, one way", &t, &one_way, secs);
    let (t, both, secs) = track(&[648], |_| {}).expect("fixture");
    let after = report("patched at 648, both ways", &t, &both, secs);
    let decoy = |rows: &[(String, f64, f64, f64, f64, usize)]| rows.iter().find(|r| r.0 == "decoy").expect("decoy").clone();
    // One way, the frame before the look stays on the look-alike; tracked
    // back from the look, it is mended. (Two frames mid-flick, where the
    // cursor leaves the look-alike at ~110 px per frame, fool both passes.)
    assert!(decoy(&after).4 > decoy(&before).4, "both ways {:.3} vs one way {:.3}", decoy(&after).4, decoy(&before).4);
    assert!(decoy(&after).4 >= 0.98);
    let off = |out: &[Frame], f: usize| out[f].map_or(f64::INFINITY, |(p, _, _)| (p[0] - t.point(f)[0]).hypot(p[1] - t.point(f)[1]));
    assert!(off(&one_way, 643) > 100.0 && off(&both, 643) < 1.0, "frame 643: one way {:.1} px off, both ways {:.2}", off(&one_way, 643), off(&both, 643));
}

/// The tracker's matching options (`Tracker::matching`), each on the
/// stretch it is for, and nowhere worse: comparing colour keeps it off the
/// yellow twin; a looser contrast slack follows the cursor into the dimmed
/// picture. `TT_MATCHING=1` also prints other settings side by side.
#[test]
fn matching_options_fix_their_stretches() {
    type Setup = fn(&mut Tracker);
    let mut runs: Vec<(&str, Setup)> = vec![("default", |_| {}), ("colour", |p| p.matching.colour = true), ("contrast 3x", |p| p.matching.contrast = 3.0)];
    if std::env::var_os("TT_MATCHING").is_some() {
        runs.extend([
            ("colour 12", (|p| (p.matching.colour, p.matching.colour_slack) = (true, 12.0)) as Setup),
            ("contrast 4x", |p| p.matching.contrast = 4.0),
            ("brightness 2", |p| p.matching.brightness = 2.0),
        ]);
    }
    let mut on: Vec<(&str, Vec<(String, f64)>)> = Vec::new();
    for (name, setup) in runs {
        let Some((t, out, secs)) = track(&[], setup) else { return };
        on.push((name, report(name, &t, &out, secs).into_iter().map(|r| (r.0, r.4)).collect()));
    }
    let share = |run: &str, stretch: &str| on.iter().find(|(n, _)| *n == run).and_then(|(_, rows)| rows.iter().find(|(s, _)| s == stretch)).map_or(0.0, |(_, v)| *v);
    assert_eq!(share("colour", "colour"), 1.0, "colour keeps it off the yellow twin");
    assert_eq!(share("contrast 3x", "dimmed"), 1.0, "a looser contrast slack follows the dimmed cursor");
    for stretch in ["changing", "bright", "icons", "flicks", "decoy", "colour", "dimmed"] {
        for run in ["colour", "contrast 3x"] {
            assert!(share(run, stretch) >= share("default", stretch), "{run} is worse than the default on {stretch}");
        }
    }
}
