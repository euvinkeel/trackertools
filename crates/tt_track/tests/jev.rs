//! Jev trackers (`job::jev`) against a fake Jev: an HTTP server in this test
//! (`TT_JEV_URL`) that answers every frame with a box at the middle of the
//! crop (the crop follows the guide, which is on the sprite: so the box's
//! centre is the sprite's), and "not in view" on every third frame. On the
//! sprite fixture (`cargo xtask fixtures`); skipped without it.
//!
//! - Frames asked about: every `every`th after the last paint, `frames` of
//!   them; their boxes come out (source px), lost where Jev says the subject
//!   can't be seen. A paint's frame shows the paint; every other frame is
//!   left out (`OFF`), the backward side too.
//! - Each request: the key as a bearer token, the images header, a JSON
//!   `request` part, and one image part per paint plus the frame.
//! - What Jev answered comes out for the app (`JevAnswers`).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use tt_core::op::{OpError, Output};
use tt_core::signal::SignalStore;
use tt_core::sketch::BOX_CHANNELS;
use tt_core::span::{Span, set_span};
use tt_core::transport::Transport;
use tt_core::view::SourceSize;
use tt_core::{AppBuilder, Core, CoreModules};
use tt_media::{DecodeOptions, VideoIndex};
use tt_track::look::Look;
use tt_track::runner::{Footage, JevAnswers, TrackJobs, coverage, settled};
use tt_track::{JevSettings, Method, NewTrackers, TrackModule, TrackRun, Tracker};

const NAME: &str = "sprite_1080p60.mp4";

fn fixture() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TT_FIXTURES") {
        return Some(PathBuf::from(dir).join(NAME)).filter(|p| p.exists());
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().map(|d| d.join("fixtures").join(NAME)).find(|p| p.exists())
}

fn truth(f: i64) -> [f64; 2] {
    let t = f as f64 / 60.0;
    let p = [950.0 + 500.0 * (0.9 * t).sin() + 60.0 * (5.3 * t).sin(), 530.0 + 300.0 * (1.3 * t + 0.7).sin() + 40.0 * (4.1 * t).sin()];
    p.map(|v| 2.0 * (v.floor() / 2.0).floor() + 10.5)
}

/// The guide's point on frame `f`: the sprite, or (`TT_JEV_ROUGH=<px>`, the
/// live test) up to that far off it, as a rough sketch is.
fn rough(f: i64) -> [f64; 2] {
    let r: f64 = std::env::var("TT_JEV_ROUGH").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let t = truth(f);
    [t[0] + r * (f as f64 / 23.0).sin(), t[1] + r * (f as f64 / 31.0).cos()]
}

/// One request as the fake saw it.
#[derive(Debug, Clone)]
struct Seen {
    frame: i64,
    auth: String,
    images_header: bool,
    parts: Vec<String>,
}

/// A fake Jev on 127.0.0.1: its address, and what it saw.
fn fake_jev() -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let url = format!("http://{}/v1/systemone", listener.local_addr().expect("address"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let log = log.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(conn.try_clone().expect("clone"));
                let mut conn = conn;
                let (mut length, mut auth, mut images_header, mut expect) = (0usize, String::new(), false, false);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    if lower.starts_with("authorization:") {
                        auth = line["authorization:".len()..].trim().to_string();
                    }
                    images_header |= lower.starts_with("typesafe-danger-zone-images: true");
                    expect |= lower.starts_with("expect: 100-continue");
                }
                if expect {
                    let _ = conn.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
                }
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).expect("the body");
                let text = String::from_utf8_lossy(&body);
                let parts: Vec<String> = text.split("Content-Disposition: form-data; name=\"").skip(1).map(|p| p.split('"').next().unwrap_or("").to_string()).collect();
                // The frame number, from the request's references (`tt-jev-<pid>-<frame>-<n>-frame`).
                let frame: i64 = text.split("tt-jev-").nth(1).and_then(|r| r.split('-').nth(1)).and_then(|f| f.parse().ok()).unwrap_or(-1);
                log.lock().unwrap().push(Seen { frame, auth, images_header, parts });
                let gone = frame % 3 == 0;
                let answer = serde_json::json!({
                    "model": "jev-1.14.0",
                    "answers": {
                        "where": { "type": "box", "box": { "left": 0.45, "right": 0.55, "top": 0.4, "bottom": 0.6 }, "confidence": 0.75 },
                        "visible": { "type": "noul", "noul": if gone { 0.1 } else { 0.95 } },
                        "view": { "type": "choice", "choice": if gone { "not_in_view" } else { "visible" }, "probabilities": { "visible": 0.9, "partly_hidden": 0.05, "not_in_view": 0.05 } }
                    },
                    "usage": { "input_tokens": 999, "output_tokens": 3 }
                })
                .to_string();
                let _ = write!(conn, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}", answer.len());
            });
        }
    });
    (url, seen)
}

fn setup() -> Option<(Core, Entity)> {
    let Some(fixture) = fixture() else {
        eprintln!("skipped: {NAME} not found (cargo xtask fixtures, or set TT_FIXTURES)");
        return None;
    };
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    *core.world.resource_mut::<NewTrackers>() = NewTrackers { method: Method::Jev, run: TrackRun::Both };
    let index = Arc::new(VideoIndex::open(&fixture).expect("fixture opens"));
    let w = &mut core.world;
    {
        let mut t = w.resource_mut::<Transport>();
        t.fps = index.fps;
        t.frame_count = index.frame_count();
    }
    w.insert_resource(SourceSize { width: index.width as f64, height: index.height as f64 });
    w.insert_resource(Footage { original: index, proxy: None, decode: DecodeOptions::default() });
    let sig = w.resource_mut::<SignalStore>().create(BOX_CHANNELS);
    {
        let mut store = w.resource_mut::<SignalStore>();
        let s = store.get_mut(sig).expect("created");
        for f in 0..1200 {
            let c = rough(f);
            s.set(f, &[c[0], c[1], c[0] - 35.0, c[1] - 35.0, c[0] + 35.0, c[1] + 35.0].map(|v| v as f32));
        }
    }
    let guide = w.spawn((Name::new("Guide"), Output(sig))).id();
    Some((core, guide))
}

/// Run until tracker `op` is settled and the fake has seen at least `n` requests.
fn until_asked(core: &mut Core, op: Entity, seen: &Arc<Mutex<Vec<Seen>>>, n: usize) {
    let start = Instant::now();
    for _ in 0..3 {
        core.run_pre_ui();
    }
    while !settled(&core.world, op) || seen.lock().unwrap().len() < n {
        assert!(start.elapsed() < Duration::from_secs(60), "still asking ({} of {n} seen)", seen.lock().unwrap().len());
        core.run_pre_ui();
        std::thread::sleep(Duration::from_millis(2));
    }
    let start = Instant::now();
    while core.world.resource::<TrackJobs>().threads() > 0 {
        assert!(start.elapsed() < Duration::from_secs(20), "job threads still alive");
        core.run_pre_ui();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The frames the fake was asked about from request `from` on, in order.
fn asked_since(seen: &Arc<Mutex<Vec<Seen>>>, from: usize) -> Vec<i64> {
    let mut f: Vec<i64> = seen.lock().unwrap()[from..].iter().map(|s| s.frame).collect();
    f.sort();
    f
}

/// The tracker's output on frame `f`: point, flags, score.
fn out_at(w: &World, op: Entity, f: i64) -> ([f64; 2], u32, f32) {
    let sig = w.resource::<SignalStore>().get(w.get::<Output>(op).expect("output").0).expect("signal");
    sig.get(f).map(|v| ([v[0] as f64, v[1] as f64], tt_track::flags(v), v[6])).unwrap_or_else(|| panic!("no result on {f}"))
}

#[test]
fn a_jev_tracker_asks_about_the_grid_in_the_range_asked() {
    let Some((mut core, guide)) = setup() else { return };
    let (url, seen) = fake_jev();
    // SAFETY: the only test in this binary that runs by default; nothing else reads these.
    unsafe {
        std::env::set_var("TT_JEV_URL", &url);
        std::env::set_var("TYPESAFE_API_KEY", "test-key-123");
    }
    let paint = |f: i64| {
        let at = truth(f);
        let (c, h, mask) = tt_track::tool::paint_look(&[at, [at[0] + 6.0, at[1] + 2.0]], 9.0);
        let mut look = Look::new(f, c, h);
        look.mask = mask;
        look
    };
    // Painted on 600 (the anchor) and 606; the grid: every 4 from 600 (572 … 596, 604 … 660), 606 left out.
    let op = tt_track::add_tracker_with_look(&mut core.world, guide, paint(600)).expect("tracker");
    set_span(&mut core.world, op, Span::new(570, 660));
    tt_track::add_look(&mut core.world, op, paint(606)).expect("a second paint");
    core.world.get_mut::<Tracker>(op).expect("settings").jev = JevSettings { every: 4, fill: false, ..JevSettings::default() };
    assert_eq!(tt_track::job::jev::grid(600, 4, &[600, 606], 600, 620), [604, 608, 612, 616, 620]);

    // It waits to be asked: nothing is sent while it is painted.
    for _ in 0..20 {
        core.run_pre_ui();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(seen.lock().unwrap().is_empty(), "a new Jev tracker is paused");
    assert_eq!(tt_track::run_of(&core.world, op), TrackRun::Paused);

    // Asked about 608–640: the grid's frames there, nothing else.
    tt_track::job::jev::ask(&mut core.world, op, 608, 640);
    let first: Vec<i64> = (608..=640).step_by(4).collect();
    until_asked(&mut core, op, &seen, first.len());
    let w = &core.world;
    assert!(w.get::<OpError>(op).is_none(), "{:?}", w.get::<OpError>(op));
    assert_eq!(coverage(w, op), Some(570..661), "both ways over its span");
    assert_eq!(asked_since(&seen, 0), first);
    for &f in &first {
        let (p, flags, score) = out_at(w, op, f);
        let d = (p[0] - truth(f)[0]).hypot(p[1] - truth(f)[1]);
        assert!(d < 1.0, "frame {f}: the box's centre is the crop's, the guide's point: {p:?} vs {:?}", truth(f));
        assert_eq!(flags & tt_track::LOST != 0, f % 3 == 0, "frame {f}: lost only where Jev can't see it");
        assert!((score - 0.75).abs() < 1e-6, "the box's confidence: {score}");
    }
    for f in [570, 596, 601, 604, 610, 644, 660] {
        assert_ne!(out_at(w, op, f).1 & tt_track::OFF, 0, "frame {f}: not asked about, left out");
    }
    let (p, flags, score) = out_at(w, op, 606);
    assert!(flags == 0 && score == 1.0 && (p[0] - truth(606)[0]).abs() < 6.0, "a paint's frame shows the paint: {p:?}");
    let answers = w.get::<JevAnswers>(op).expect("Jev's answers");
    assert_eq!(answers.answers.keys().copied().collect::<Vec<_>>(), first);
    assert!(answers.answers.values().all(|a| a.tokens == 999 && a.error.is_none() && a.view_p.len() == 3));
    assert_eq!(answers.answers[&612].view, "not_in_view");
    assert_eq!(answers.examples.len(), 2, "the examples it sent, one per paint");
    assert!(answers.answers.values().all(|a| a.image.as_ref().is_some_and(|i| i.starts_with(b"\x89PNG")) && a.edges == [0.45, 0.4, 0.55, 0.6]), "each frame's image and box");
    // What Jev sees is the sketch's box: the crop's sides (outside the 70 px square box) are grey, its middle isn't.
    let (w_px, h_px, px) = tt_track::icons::read_png(answers.answers[&608].image.as_ref().expect("image")).expect("a PNG");
    let at = |x: u32, y: u32| &px[((y * w_px + x) * 4) as usize..((y * w_px + x) * 4 + 3) as usize];
    assert_eq!((at(3, h_px / 2), at(w_px - 4, h_px / 2)), (&[128u8, 128, 128][..], &[128u8, 128, 128][..]), "outside the sketch's box: grey");
    assert_ne!(at(w_px / 2, h_px / 2), &[128u8, 128, 128][..], "inside: the picture");
    let requests = seen.lock().unwrap().clone();
    for s in &requests {
        assert_eq!(s.auth, "Bearer test-key-123");
        assert!(s.images_header, "the images header");
        assert_eq!(s.parts, ["request", "ex0", "ex1", "frame"], "frame {}", s.frame);
    }
    assert_eq!(tt_track::run_of(&core.world, op), TrackRun::Paused, "done: paused again");

    // Another range, before the first paint: asked there; the answers on 608–640 stay.
    let n = seen.lock().unwrap().len();
    tt_track::job::jev::ask(&mut core.world, op, 570, 599);
    let before: Vec<i64> = (572..=596).step_by(4).collect();
    until_asked(&mut core, op, &seen, n + before.len());
    assert_eq!(asked_since(&seen, n), before, "572–596 only");
    let w = &core.world;
    assert_eq!(w.get::<JevAnswers>(op).expect("answers").answers.len(), first.len() + before.len(), "the others kept");
    assert_eq!(out_at(w, op, 608).1, 0, "608's answer still stands");
    assert!((out_at(w, op, 584).0[0] - truth(584)[0]).abs() < 1.0, "584: asked (backward)");

    // Retry one frame: that frame alone.
    let n = seen.lock().unwrap().len();
    tt_track::job::jev::retry(&mut core.world, op, &[620]);
    until_asked(&mut core, op, &seen, n + 1);
    assert_eq!(asked_since(&seen, n), [620], "620 alone, asked again");

    // Filled: a setting (so everything asks again); the frames between two answers (or a paint and an answer) on the line between them.
    let n = seen.lock().unwrap().len();
    core.world.get_mut::<Tracker>(op).expect("settings").jev.fill = true;
    tt_track::job::jev::ask(&mut core.world, op, 600, 660);
    let after: Vec<i64> = (604..=660).step_by(4).collect();
    until_asked(&mut core, op, &seen, n + after.len());
    assert_eq!(asked_since(&seen, n), after, "604–660 only");
    let w = &core.world;
    let (a, b, (m, flags, _)) = (out_at(w, op, 616).0, out_at(w, op, 620).0, out_at(w, op, 618));
    assert!((m[0] - (a[0] + b[0]) / 2.0).abs() < 0.01 && (m[1] - (a[1] + b[1]) / 2.0).abs() < 0.01, "618: halfway between 616 and 620: {m:?}");
    assert_eq!(flags, 0, "618: found, as both ends are");
    assert_ne!(out_at(w, op, 610).1 & tt_track::LOST, 0, "610: next to 612, which Jev can't see: lost");
    assert_eq!(out_at(w, op, 602).1 & tt_track::OFF, 0, "602: between the paint on 600 and 604, filled");
    assert_ne!(out_at(w, op, 590).1 & tt_track::OFF, 0, "590: not asked about since the setting changed: left out");

    // The least confidence: frames go lost and back as it moves, with no request.
    let n = seen.lock().unwrap().len();
    core.world.get_mut::<Tracker>(op).expect("settings").jev.min_confidence = 0.8;
    for _ in 0..10 {
        core.run_pre_ui();
    }
    assert_ne!(out_at(&core.world, op, 616).1 & tt_track::LOST, 0, "616 (confidence 0.75): lost below 0.8");
    core.world.get_mut::<Tracker>(op).expect("settings").jev.min_confidence = 0.5;
    for _ in 0..10 {
        core.run_pre_ui();
    }
    assert_eq!(out_at(&core.world, op, 616).1, 0, "616: found again above 0.5");
    assert_eq!(seen.lock().unwrap().len(), n, "moving it asked nothing");

    // Paused: a new paint asks nothing by itself.
    tt_track::add_look(&mut core.world, op, paint(630)).expect("a third paint");
    for _ in 0..50 {
        core.run_pre_ui();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(seen.lock().unwrap().len(), n, "no request after a new paint");
}

/// The real Jev on the sprite (`TT_JEV_LIVE=1`; the key from the Keychain or
/// `TYPESAFE_API_KEY`; paid): painted on frames 600, 610 and 620, then asked
/// about `TT_JEV_FRAMES` frames (30) every `TT_JEV_EVERY` (5), for each
/// example style in `TT_JEV_STYLES` (dimmed,outlined,tight). Prints, per
/// style, the box centres' distance from the sprite (px), what Jev said it
/// sees, times and tokens; `TT_JEV_OUT=<file>` writes every answer as JSON.
#[test]
#[ignore]
fn live_jev_on_the_sprite() {
    if std::env::var_os("TT_JEV_LIVE").is_none() {
        eprintln!("skipped: TT_JEV_LIVE is not set (this sends paid requests)");
        return;
    }
    let num = |k: &str, d: u32| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let (frames, every) = (num("TT_JEV_FRAMES", 30), num("TT_JEV_EVERY", 5));
    let styles: Vec<tt_track::JevExamples> = std::env::var("TT_JEV_STYLES")
        .unwrap_or_else(|_| "dimmed,outlined,tight".into())
        .split(',')
        .map(|s| match s.trim() {
            "outlined" => tt_track::JevExamples::Outlined,
            "tight" => tt_track::JevExamples::Tight,
            _ => tt_track::JevExamples::Dimmed,
        })
        .collect();
    let mut all = Vec::new();
    for style in styles {
        let Some((mut core, guide)) = setup() else { return };
        let paint = |f: i64| {
            let at = truth(f);
            let (c, h, mask) = tt_track::tool::paint_look(&[[at[0] - 6.0, at[1] - 6.0], [at[0] + 6.0, at[1] + 6.0], [at[0] + 6.0, at[1] - 6.0], [at[0] - 6.0, at[1] + 6.0]], 9.0);
            let mut look = Look::new(f, c, h);
            look.mask = mask;
            look
        };
        let op = tt_track::add_tracker_with_look(&mut core.world, guide, paint(600)).expect("tracker");
        set_span(&mut core.world, op, Span::new(600, 620 + (frames * every) as i64));
        tt_track::add_look(&mut core.world, op, paint(610)).expect("paint");
        tt_track::add_look(&mut core.world, op, paint(620)).expect("paint");
        core.world.get_mut::<Tracker>(op).expect("settings").jev = JevSettings { every, frames, examples: style, ..JevSettings::default() };
        tt_track::job::jev::ask(&mut core.world, op, 621, 620 + (frames * every) as i64);
        let start = Instant::now();
        for _ in 0..3 {
            core.run_pre_ui();
        }
        while !settled(&core.world, op) {
            assert!(start.elapsed() < Duration::from_secs(900), "still asking");
            core.run_pre_ui();
            std::thread::sleep(Duration::from_millis(5));
        }
        let wall = start.elapsed().as_secs_f64();
        let w = &core.world;
        if let Some(e) = w.get::<OpError>(op) {
            panic!("{style:?}: {}", e.0);
        }
        let sig = w.resource::<SignalStore>().get(w.get::<Output>(op).expect("output").0).expect("signal");
        let answers = w.get::<JevAnswers>(op).cloned().unwrap_or_default();
        let (mut d, mut middle): (Vec<f64>, Vec<f64>) = (Vec::new(), Vec::new());
        let (mut tokens, mut ms, mut failed, mut not_seen) = (0u64, Vec::new(), 0, 0);
        for (f, a) in &answers.answers {
            tokens += a.tokens;
            if a.error.is_some() {
                failed += 1;
                eprintln!("  {f}: failed: {}", a.error.as_deref().unwrap_or_default());
                continue;
            }
            ms.push(a.ms);
            let v = sig.get(*f).expect("a result");
            let dist = (v[0] as f64 - truth(*f)[0]).hypot(v[1] as f64 - truth(*f)[1]);
            let (bw, bh) = (v[4] - v[2], v[5] - v[3]);
            if a.visible < 0.5 || a.view == "not_in_view" {
                not_seen += 1;
            }
            d.push(dist);
            middle.push((rough(*f)[0] - truth(*f)[0]).hypot(rough(*f)[1] - truth(*f)[1]));
            eprintln!("  {f}: off by {dist:6.1} px · box {bw:4.0}x{bh:<4.0} · conf {:.2} · seen {:.2} · {} · {} ms · {} tok", a.confidence, a.visible, a.view, a.ms, a.tokens);
            all.push(serde_json::json!({ "style": format!("{style:?}"), "frame": f, "error_px": dist, "box": [v[2], v[3], v[4], v[5]], "truth": truth(*f), "confidence": a.confidence, "visible": a.visible, "view": a.view, "ms": a.ms, "tokens": a.tokens }));
        }
        d.sort_by(f64::total_cmp);
        middle.sort_by(f64::total_cmp);
        ms.sort_unstable();
        let q = |v: &[f64], p: f64| v.get(((v.len() as f64 - 1.0) * p).round() as usize).copied().unwrap_or(f64::NAN);
        eprintln!(
            "{style:?}: {} answered ({failed} failed) in {wall:.0} s · off by median {:.1} px, 90% {:.1}, max {:.1} (guessing the crop's middle: median {:.1}, max {:.1}) · within 10 px {}/{} · said not seen {not_seen} · {} ms median · {tokens} tokens (${:.5})",
            answers.answers.len(),
            q(&d, 0.5),
            q(&d, 0.9),
            q(&d, 1.0),
            q(&middle, 0.5),
            q(&middle, 1.0),
            d.iter().filter(|x| **x <= 10.0).count(),
            d.len(),
            ms.get(ms.len() / 2).copied().unwrap_or(0),
            tokens as f64 * 0.042e-6
        );
        drop(core);
    }
    if let Some(out) = std::env::var_os("TT_JEV_OUT") {
        std::fs::write(out, serde_json::to_string_pretty(&all).expect("json")).expect("written");
    }
}
