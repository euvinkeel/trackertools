//! Cursor trackers (`Method::Cursor`, `job::cursor`) end to end, from loose
//! paints, on no guide (the whole frame searched):
//! - the cursor fixture (`cargo xtask fixtures`; see `tests/cursor.rs`):
//!   measured against the exact hotspot. Skipped when it isn't generated.
//! - a real clip and a track of it made by hand (`TT_CURSOR_BENCH`: the
//!   JSON a tracker's *Save its motion as data* writes, its video beside
//!   it): measured against that track. Skipped without it.
//!
//! A tracker's point is the middle of its shape's top edge, the hand's is
//! wherever its tracker put it: each is compared with its constant offset
//! (per shape) taken out, so what counts is how steadily and how often it
//! is on the cursor.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tt_core::signal::SignalStore;
use tt_core::span::{Span, set_span};
use tt_core::transport::Transport;
use tt_core::view::SourceSize;
use tt_core::{AppBuilder, CoreModules};
use tt_media::{DecodeOptions, VideoIndex};
use tt_track::look::Look;
use tt_track::runner::{CursorShapes, Footage, settled};
use tt_track::{Method, NewTrackers, TrackModule, TrackRun};

/// A loose paint: a scribble of radius `r` around `at` (a few px off the cursor, as a hand does).
fn loose(f: i64, at: [f64; 2], r: f64, wobble: f64, pattern: u32) -> Look {
    let path: Vec<[f64; 2]> = (0..12).map(|i| {
        let t = i as f64 / 11.0 * std::f64::consts::TAU;
        [at[0] + wobble + 0.4 * r * t.cos(), at[1] + 0.6 * wobble + 0.4 * r * t.sin()]
    }).collect();
    let (c, h, mask) = tt_track::tool::paint_look(&path, r);
    Look { mask, pattern, ..Look::new(f, c, h) }
}

/// Track `video` (frames `lo..=hi`) with a cursor tracker painted with
/// `paints`: per frame its point and flags, the shapes learned, the seconds.
type Out = Vec<Option<([f64; 2], u32, f32)>>;

/// Per frame, the size of the box found (which pattern it was).
type Boxes = Vec<Option<[i64; 2]>>;

fn track(video: &Path, lo: i64, hi: i64, paints: Vec<Look>) -> (Out, Vec<tt_track::job::cursor::Shape>, f64) {
    let (out, _, shapes, secs) = track_boxes(video, lo, hi, paints);
    (out, shapes, secs)
}

fn track_boxes(video: &Path, lo: i64, hi: i64, paints: Vec<Look>) -> (Out, Boxes, Vec<tt_track::job::cursor::Shape>, f64) {
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    *core.world.resource_mut::<NewTrackers>() = NewTrackers { method: Method::Cursor, run: TrackRun::Both };
    let index = Arc::new(VideoIndex::open(video).expect("video opens"));
    let w = &mut core.world;
    {
        let mut tr = w.resource_mut::<Transport>();
        tr.fps = index.fps;
        tr.frame_count = index.frame_count();
    }
    w.insert_resource(SourceSize { width: index.width as f64, height: index.height as f64 });
    w.insert_resource(Footage { original: index, proxy: None, decode: DecodeOptions::default() });
    let mut paints = paints.into_iter();
    let op = tt_track::add_unguided_tracker(w, paints.next().expect("a paint")).expect("tracker");
    for p in paints {
        tt_track::add_look(w, op, p).expect("paint");
    }
    set_span(w, op, Span::new(lo, hi));
    let start = Instant::now();
    for _ in 0..3 {
        core.run_pre_ui();
    }
    while !settled(&core.world, op) {
        assert!(start.elapsed() < Duration::from_secs(1800), "still tracking");
        core.run_pre_ui();
        std::thread::sleep(Duration::from_millis(2));
    }
    let secs = start.elapsed().as_secs_f64();
    let w = &core.world;
    assert!(w.get::<tt_core::op::OpError>(op).is_none(), "{:?}", w.get::<tt_core::op::OpError>(op));
    let out = w.resource::<SignalStore>().get(w.get::<tt_core::op::Output>(op).expect("output").0).expect("signal");
    let frames = (lo..=hi).map(|f| out.get(f).map(|v| ([v[0] as f64, v[1] as f64], tt_track::flags(v), v[6]))).collect();
    let boxes = (lo..=hi).map(|f| out.get(f).filter(|v| tt_track::flags(v) == 0).map(|v| [(v[4] - v[2]).round() as i64, (v[5] - v[3]).round() as i64])).collect();
    let shapes = w.get::<CursorShapes>(op).map(|s| s.learned.shapes.clone()).unwrap_or_default();
    (frames, boxes, shapes, secs)
}

/// Draw the shapes learned (`#` dark, `o` light, `.` unsure), for the log.
fn show(shapes: &[tt_track::job::cursor::Shape]) {
    for (i, s) in shapes.iter().enumerate() {
        eprintln!("shape {i} (pattern {}): {}×{} px, tip {:?}, from {} shots ({} paints, {} left out)", s.pattern + 1, s.w, s.h, s.tip, s.shots, s.paints, s.left_out);
        for y in 0..s.h {
            let row: String = (0..s.w)
                .map(|x| {
                    let (a, v) = (s.alpha[y * s.w + x], s.value[y * s.w + x]);
                    if a > 0.5 { if v < 100.0 { '#' } else { 'o' } } else if a > 0.05 { '.' } else { ' ' }
                })
                .collect();
            eprintln!("  |{row}|");
        }
    }
}

/// Errors against `truth` (per frame; None: not visible there), each frame's
/// offset less the median offset of its group (`group`: the icon): `(median,
/// p95, share within 3 px, frames lost where it is visible, frames found where it isn't)`.
fn score(label: &str, out: &[Option<([f64; 2], u32, f32)>], truth: &[Option<[f64; 2]>], group: &[String]) -> (f64, f64, f64, usize, usize) {
    let mut offsets: std::collections::BTreeMap<&str, Vec<[f64; 2]>> = Default::default();
    for (i, t) in truth.iter().enumerate() {
        if let (Some(t), Some((p, 0, _))) = (t, out[i]) {
            offsets.entry(group[i].as_str()).or_default().push([p[0] - t[0], p[1] - t[1]]);
        }
    }
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    };
    let bias: std::collections::BTreeMap<&str, [f64; 2]> =
        offsets.iter().map(|(k, v)| (*k, [median(&mut v.iter().map(|o| o[0]).collect()), median(&mut v.iter().map(|o| o[1]).collect())])).collect();
    let (mut e, mut lost, mut ghost) = (Vec::new(), 0, 0);
    for (i, t) in truth.iter().enumerate() {
        match (t, out[i]) {
            (Some(_), Some((_, f, _))) if f != 0 => lost += 1,
            (Some(t), Some((p, _, _))) => {
                let b = bias.get(group[i].as_str()).copied().unwrap_or_default();
                e.push((p[0] - t[0] - b[0]).hypot(p[1] - t[1] - b[1]));
            }
            (None, Some((_, 0, _))) => ghost += 1,
            _ => {}
        }
    }
    let n = truth.iter().filter(|t| t.is_some()).count().max(1);
    let within = e.iter().filter(|v| **v < 3.0).count() as f64 / n as f64;
    let (med, p95) = (median(&mut e.clone()), {
        let mut s = e.clone();
        s.sort_by(f64::total_cmp);
        s.get(s.len() * 95 / 100).copied().unwrap_or(f64::INFINITY)
    });
    eprintln!("{label}: median {med:.2} px, p95 {p95:.2} px, within 3 px {:.1}%, lost where visible {lost}, found where not {ghost}; offsets {bias:?}", within * 100.0);
    (med, p95, within, lost, ghost)
}

fn fixtures() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TT_FIXTURES") {
        return Some(PathBuf::from(dir)).filter(|p| p.join("cursor_540p60.mp4").exists());
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().map(|d| d.join("fixtures")).find(|p| p.join("cursor_540p60.mp4").exists())
}

/// The cursor fixture from thirteen loose paints (a scribble 3× the
/// arrow's size, a few px off it): seven of the arrow on different scenery
/// (pattern 1), three of the hand (2), three of the I-beam (3).
#[test]
fn finds_the_fixture_cursor_from_loose_paints() {
    let Some(dir) = fixtures() else {
        eprintln!("skipped: cursor_540p60.mp4 not found (cargo xtask fixtures, or set TT_FIXTURES)");
        return;
    };
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("cursor_truth.json")).expect("truth")).expect("json");
    let hot: Vec<[f64; 2]> = v["hotspot"].as_array().expect("hotspot").iter().map(|p| [p[0].as_f64().expect("x"), p[1].as_f64().expect("y")]).collect();
    let icon: Vec<String> = v["icon"].as_array().expect("icon").iter().map(|s| s.as_str().expect("icon").to_string()).collect();
    let stretches: Vec<(String, usize, usize)> =
        v["stretches"].as_array().expect("stretches").iter().map(|s| (s[0].as_str().expect("n").to_string(), s[1].as_u64().expect("a") as usize, s[2].as_u64().expect("b") as usize)).collect();
    let paints: Vec<Look> = [20usize, 180, 260, 480, 560, 700, 960, 335, 345, 355, 378, 388, 398]
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let h = hot[*f];
            let (centre, pattern) = match icon[*f].as_str() {
                "ibeam" => (h, 2),
                "hand" => ([h[0] + 5.0, h[1] + 9.0], 1),
                _ => ([h[0] + 5.0, h[1] + 9.0], 0),
            };
            loose(*f as i64, centre, 22.0, if i % 2 == 0 { 4.0 } else { -3.0 }, pattern)
        })
        .collect();
    let n = hot.len() as i64;
    let (out, shapes, secs) = track(&dir.join("cursor_540p60.mp4"), 0, n - 1, paints);
    show(&shapes);
    eprintln!("{} frames in {secs:.1} s ({:.0} fps)", n, n as f64 / secs);
    let truth: Vec<Option<[f64; 2]>> = hot.iter().map(|h| Some(*h)).collect();
    let (_, _, within, _, _) = score("fixture, all", &out, &truth, &icon);
    for (name, a, b) in &stretches {
        score(&format!("  {name}"), &out[*a..*b], &truth[*a..*b], &icon[*a..*b]);
    }
    assert!(shapes.len() >= 3, "the arrow, the hand and the I-beam: {} shapes", shapes.len());
    assert!(within > 0.9, "on the cursor on {:.1}% of frames", within * 100.0);
}

/// A real clip, against a track of it made by hand (`TT_CURSOR_BENCH`: the
/// saved motion JSON). Painted loosely on `TT_CURSOR_PAINTS` frames (comma
/// separated; by default eight spread over the track).
#[test]
fn real_clip_against_a_hand_made_track() {
    let Some(json) = std::env::var_os("TT_CURSOR_BENCH").map(PathBuf::from) else { return };
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&json).expect("json")).expect("json");
    let video = PathBuf::from(v["video"].as_str().expect("video"));
    let frames = v["tracked"][0]["frames"].as_array().expect("frames");
    let (lo, hi) = (v["first_frame"].as_i64().expect("first"), v["last_frame"].as_i64().expect("last"));
    let mut truth: Vec<Option<[f64; 2]>> = vec![None; (hi - lo + 1) as usize];
    for f in frames {
        if f["trusted"].as_bool() == Some(true) {
            truth[(f["frame"].as_i64().expect("frame") - lo) as usize] = Some([f["x"].as_f64().expect("x"), f["y"].as_f64().expect("y")]);
        }
    }
    let picks: Vec<i64> = match std::env::var("TT_CURSOR_PAINTS") {
        Ok(s) => s.split(',').map(|x| x.trim().parse().expect("a frame")).collect(),
        Err(_) => (0..8).map(|i| lo + (hi - lo) * (2 * i + 1) / 16).collect(),
    };
    let size = std::env::var("TT_CURSOR_RADIUS").ok().and_then(|s| s.parse().ok()).unwrap_or(30.0);
    // (`TT_CURSOR_PATTERNS`: each paint's pattern, comma separated, from 1; by default all the first.)
    let patterns: Vec<u32> = std::env::var("TT_CURSOR_PATTERNS").map(|s| s.split(',').map(|x| x.trim().parse::<u32>().expect("a pattern") - 1).collect()).unwrap_or_default();
    let paints: Vec<Look> = picks
        .iter()
        .enumerate()
        .filter_map(|(i, f)| truth[(f - lo) as usize].map(|t| loose(*f, t, size, if i % 2 == 0 { 5.0 } else { -4.0 }, patterns.get(i).copied().unwrap_or(0))))
        .collect();
    let (out, boxes, shapes, secs) = track_boxes(&video, lo, hi, paints);
    show(&shapes);
    eprintln!("{} frames in {secs:.1} s ({:.0} fps)", hi - lo + 1, (hi - lo + 1) as f64 / secs);
    // (Each pattern has its own offset from the hand-made point: group by the
    // pattern found, told by the size of its box.)
    let group: Vec<String> = boxes.iter().map(|b| b.map_or("lost".to_string(), |b| format!("{}x{}", b[0], b[1]))).collect();
    score("real clip", &out, &truth, &group);
    if std::env::var_os("TT_DUMP").is_some() {
        for (i, (o, t)) in out.iter().zip(&truth).enumerate() {
            eprintln!("  {} {:?} {:?} truth {:?}", lo + i as i64, o, boxes[i], t);
        }
    }
}

/// Patterns are learned as soon as they are painted, tracking or not (the
/// app shows them while you paint): a paused tracker learns, and tracks nothing.
#[test]
fn learns_while_paused() {
    let Some(dir) = fixtures() else { return };
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("cursor_truth.json")).expect("truth")).expect("json");
    let hot: Vec<[f64; 2]> = v["hotspot"].as_array().expect("hotspot").iter().map(|p| [p[0].as_f64().expect("x"), p[1].as_f64().expect("y")]).collect();
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    *core.world.resource_mut::<NewTrackers>() = NewTrackers { method: Method::Cursor, run: TrackRun::Paused };
    let index = Arc::new(VideoIndex::open(dir.join("cursor_540p60.mp4")).expect("video opens"));
    let w = &mut core.world;
    {
        let mut tr = w.resource_mut::<Transport>();
        tr.fps = index.fps;
        tr.frame_count = index.frame_count();
    }
    w.insert_resource(SourceSize { width: index.width as f64, height: index.height as f64 });
    w.insert_resource(Footage { original: index, proxy: None, decode: DecodeOptions::default() });
    let paint = |f: usize| loose(f as i64, [hot[f][0] + 5.0, hot[f][1] + 9.0], 22.0, 3.0, 0);
    let op = tt_track::add_unguided_tracker(w, paint(20)).expect("tracker");
    for f in [180, 260] {
        tt_track::add_look(w, op, paint(f)).expect("paint");
    }
    let start = Instant::now();
    loop {
        core.run_pre_ui();
        let c = core.world.get::<CursorShapes>(op).cloned();
        if c.as_ref().is_some_and(|c| !c.learning && !c.learned.shapes.is_empty()) {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(120), "still learning: {c:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
    let shapes = core.world.get::<CursorShapes>(op).expect("learned").learned.shapes.clone();
    assert_eq!(shapes.len(), 1);
    // (From three paints: about the arrow, 12 × 20, and a little around it.)
    assert!(shapes[0].w <= 22 && shapes[0].h <= 28, "the arrow: {}×{}", shapes[0].w, shapes[0].h);
    let w = &core.world;
    let out = w.resource::<SignalStore>().get(w.get::<tt_core::op::Output>(op).expect("output").0).expect("signal");
    assert!(out.present_hull().is_none(), "paused: nothing tracked");
}
