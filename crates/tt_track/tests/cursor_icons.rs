//! Template trackers with cursor icons (`icons`): on a real clip against a
//! track of it made by hand (`TT_ICON_BENCH`: the JSON a tracker's *Save its
//! motion as data* writes, its video beside it), guided by a rough pass
//! made from that track (smoothed as a hand's sketch is, a slow wander, a
//! box growing with speed), from looks on a few frames (`TT_ICON_LOOKS`;
//! by default the first), tracked without icons and with the packs found
//! on this computer. Skipped without the clip, or without a pack.
//!
//! Its point is the look's centre (the icons line up with it), compared with
//! the hand-made point less their median offset.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bevy_ecs::name::Name;
use tt_core::op::Output;
use tt_core::signal::SignalStore;
use tt_core::sketch::BOX_CHANNELS;
use tt_core::span::{Span, set_span};
use tt_core::transport::Transport;
use tt_core::view::SourceSize;
use tt_core::{AppBuilder, CoreModules};
use tt_media::{DecodeOptions, VideoIndex};
use tt_track::icons::{CursorIcons, IconFit};
use tt_track::look::Look;
use tt_track::runner::{Footage, settled};
use tt_track::{Method, NewTrackers, TrackModule, TrackRun};

type Out = Vec<Option<([f64; 2], u32)>>;

/// A rough pass over `truth` (from `lo`; gaps hold the last point): a
/// hand's sketch can't follow a flick (σ 4 frames), wanders a few px, and
/// its box grows with speed. `[x, y, left, top, right, bottom]`.
fn rough(truth: &[Option<[f64; 2]>]) -> Vec<[f32; 6]> {
    let mut last = truth.iter().flatten().next().copied().unwrap_or([0.0, 0.0]);
    let pts: Vec<[f64; 2]> = truth.iter().map(|t| {
        last = t.unwrap_or(last);
        last
    }).collect();
    let n = pts.len() as i64;
    (0..n)
        .map(|f| {
            let (mut acc, mut w) = ([0.0; 2], 0.0);
            for g in f - 12..=f + 12 {
                let k = (-((g - f) as f64).powi(2) / 32.0).exp();
                let p = pts[g.clamp(0, n - 1) as usize];
                acc = [acc[0] + k * p[0], acc[1] + k * p[1]];
                w += k;
            }
            let s = f as f64 / 60.0;
            let (x, y) = (acc[0] / w + 4.0 * (2.3 * s).sin(), acc[1] / w + 3.0 * (1.7 * s + 0.5).cos());
            let (a, b) = (pts[(f + 2).min(n - 1) as usize], pts[(f - 2).max(0) as usize]);
            let half = (30.0 + 1.5 * (a[0] - b[0]).hypot(a[1] - b[1]) / 4.0).min(160.0);
            [x, y, x - half, y - half, x + half, y + half].map(|v| v as f32)
        })
        .collect()
}

/// Track `video` (frames `lo..=hi`, guided by `guide`) with a template tracker from `looks`, with `icons`.
fn track(video: &Path, lo: i64, hi: i64, guide: &[[f32; 6]], looks: &[Look], icons: Option<CursorIcons>) -> (Out, Option<IconFit>, f64) {
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    *core.world.resource_mut::<NewTrackers>() = NewTrackers { method: Method::Template, run: TrackRun::Both };
    let index = Arc::new(VideoIndex::open(video).expect("video opens"));
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
        for (i, b) in guide.iter().enumerate() {
            s.set(lo + i as i64, b);
        }
    }
    let g = w.spawn((Name::new("Guide"), Output(sig))).id();
    let op = tt_track::add_tracker_with_look(w, g, looks[0].clone()).expect("tracker");
    for l in &looks[1..] {
        tt_track::add_look(w, op, l.clone()).expect("look");
    }
    if let Some(i) = icons {
        w.entity_mut(op).insert(i);
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
    let frames = (lo..=hi).map(|f| out.get(f).map(|v| ([v[0] as f64, v[1] as f64], tt_track::flags(v)))).collect();
    (frames, w.get::<IconFit>(op).cloned(), secs)
}

/// Against `truth` (None: not visible), less the median offset: the share
/// within 3 px of the frames where it's visible, those lost there, and the
/// stretches where it was more than 10 px off (count, longest).
fn score(label: &str, out: &Out, truth: &[Option<[f64; 2]>]) -> f64 {
    let offs: Vec<[f64; 2]> = truth.iter().zip(out).filter_map(|(t, o)| Some(((*t)?, (*o)?)).filter(|(_, o)| o.1 == 0).map(|(t, o)| [o.0[0] - t[0], o.0[1] - t[1]])).collect();
    let med = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    };
    let bias = [med(offs.iter().map(|o| o[0]).collect()), med(offs.iter().map(|o| o[1]).collect())];
    let (mut within, mut lost, mut off_runs, mut run, mut longest) = (0, 0, 0, 0, 0);
    for (t, o) in truth.iter().zip(out) {
        let Some(t) = t else { continue };
        let e = match o {
            Some((_, f)) if *f != 0 => {
                lost += 1;
                None
            }
            Some((p, _)) => Some((p[0] - t[0] - bias[0]).hypot(p[1] - t[1] - bias[1])),
            None => None,
        };
        within += usize::from(e.is_some_and(|e| e < 3.0));
        if e.is_some_and(|e| e > 10.0) {
            if run == 0 {
                off_runs += 1;
            }
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let n = truth.iter().filter(|t| t.is_some()).count().max(1);
    let share = within as f64 / n as f64;
    eprintln!("{label}: within 3 px {:.1}% of {n}, lost {lost}, off by > 10 px in {off_runs} stretches (longest {longest} frames); offset {bias:.1?}", share * 100.0);
    share
}

/// A tracker's icons are part of the project: saved and opened again as they were.
#[test]
fn icons_are_saved_with_the_project() {
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    let icons = CursorIcons {
        icons: vec![tt_track::icons::Icon { pack: "Windows".into(), name: "Arrow".into(), w: 2, h: 1, rgba: vec![255, 255, 255, 255, 0, 0, 0, 128], hotspot: [0.0, 0.5] }],
        sizes: vec![tt_track::icons::PackSize { pack: "Windows".into(), size: 0.62 }],
    };
    let e = core.world.spawn((Name::new("Tracker 1"), icons.clone())).id();
    let dir = std::env::temp_dir().join(format!("tt-icons-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("icons.ttproj");
    let _ = std::fs::remove_file(&path);
    tt_core::persist::save(&mut core.world, &path).expect("saved");
    tt_core::persist::clear_document(&mut core.world);
    assert!(core.world.get_entity(e).is_err() || core.world.get::<CursorIcons>(e).is_none(), "cleared");
    tt_core::persist::load(&mut core.world, &path).expect("opened");
    let back: Vec<CursorIcons> = core.world.query::<&CursorIcons>().iter(&core.world).cloned().collect();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(back, vec![icons]);
}

#[test]
fn icons_on_a_real_clip() {
    let Some(json) = std::env::var_os("TT_ICON_BENCH").map(PathBuf::from) else { return };
    let packs = tt_track::icons::packs();
    let want: Vec<String> = std::env::var("TT_ICON_PACKS").map(|s| s.split(',').map(|p| p.trim().to_string()).collect()).unwrap_or_default();
    // (`TT_ICON_NAMES`: only these icons, by name.)
    let names: Vec<String> = std::env::var("TT_ICON_NAMES").map(|s| s.split(',').map(|p| p.trim().to_string()).collect()).unwrap_or_default();
    let icons: Vec<tt_track::icons::Icon> = packs
        .iter()
        .filter(|p| want.is_empty() || want.iter().any(|w| w == p.name))
        .flat_map(|p| p.icons.clone())
        .filter(|i| names.is_empty() || names.contains(&i.name))
        .collect();
    if icons.is_empty() {
        eprintln!("skipped: no cursor icons on this computer");
        return;
    }
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&json).expect("json")).expect("json");
    let video = PathBuf::from(v["video"].as_str().expect("video"));
    let (lo, hi) = (v["first_frame"].as_i64().expect("first"), v["last_frame"].as_i64().expect("last"));
    let mut truth: Vec<Option<[f64; 2]>> = vec![None; (hi - lo + 1) as usize];
    for f in v["tracked"][0]["frames"].as_array().expect("frames") {
        if f["trusted"].as_bool() == Some(true) {
            truth[(f["frame"].as_i64().expect("frame") - lo) as usize] = Some([f["x"].as_f64().expect("x"), f["y"].as_f64().expect("y")]);
        }
    }
    let half: f64 = std::env::var("TT_ICON_HALF").ok().and_then(|s| s.parse().ok()).unwrap_or(12.0);
    let frames: Vec<i64> = std::env::var("TT_ICON_LOOKS").map(|s| s.split(',').map(|f| f.trim().parse().expect("a frame")).collect()).unwrap_or_else(|_| vec![lo]);
    let looks: Vec<Look> = frames.iter().map(|f| Look::new(*f, truth[(f - lo) as usize].expect("the hand-made track has the look's frame"), [half, half])).collect();
    let guide = rough(&truth);
    eprintln!("{} icons: {:?}", icons.len(), icons.iter().map(|i| format!("{} {} {}x{}", i.pack, i.name, i.w, i.h)).collect::<Vec<_>>());
    let (plain, _, s0) = track(&video, lo, hi, &guide, &looks, None);
    score(&format!("one look ({s0:.0} s)"), &plain, &truth);
    // (`TT_ICON_SIZE`: every pack's size; by default found from the looks.)
    let size: f32 = std::env::var("TT_ICON_SIZE").ok().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let mut set = CursorIcons { icons, sizes: Vec::new() };
    if size > 0.0 {
        set.sizes = set.packs().into_iter().map(|pack| tt_track::icons::PackSize { pack, size }).collect();
    }
    let (with, fit, s1) = track(&video, lo, hi, &guide, &looks, Some(set));
    score(&format!("one look + icons ({s1:.0} s), {fit:?}"), &with, &truth);
    if std::env::var_os("TT_DUMP").is_some() {
        for (i, (a, b)) in plain.iter().zip(&with).enumerate() {
            eprintln!("  {} {:?} {:?} truth {:?}", lo + i as i64, a, b, truth[i]);
        }
    }
}
