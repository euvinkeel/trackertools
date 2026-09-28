//! Backward playback (J) on a long-GOP clip: the decode service fills the
//! frames behind the playhead a keyframe group at a time (one decoder start
//! per group, read through to the frames needed), so playing backward in
//! real time finds nearly every frame decoded when it is shown. Skipped when
//! the fixture hasn't been generated (`cargo xtask fixtures`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tt_media::player::{Player, Want};
use tt_media::{DecodeOptions, FrameStream, VideoIndex};

fn fixture(name: &str) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TT_FIXTURES") {
        return Some(PathBuf::from(dir).join(name)).filter(|p| p.exists());
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().map(|d| d.join("fixtures").join(name)).find(|p| p.exists())
}

/// Play backward at `rate` for `secs` from `from` (presented frames, at the
/// clip's rate), as the app does: every ~4 ms the frame due now is wanted.
/// Returns the share of those moments when it was already decoded.
fn play_backward(player: &mut Player, fps: f64, from: usize, rate: f64, secs: f64) -> f64 {
    // A second paused first, as when J is pressed after looking at a frame.
    player.want(Want { frame: from, playing: false, rate, reverse: false });
    std::thread::sleep(Duration::from_secs(1));
    let start = Instant::now();
    let (mut shown, mut exact) = (0, 0);
    while start.elapsed().as_secs_f64() < secs {
        let f = from.saturating_sub((start.elapsed().as_secs_f64() * fps * rate) as usize);
        player.want(Want { frame: f, playing: true, rate, reverse: true });
        shown += 1;
        exact += usize::from(player.frame(f).is_some());
        std::thread::sleep(Duration::from_millis(4));
    }
    exact as f64 / shown as f64
}

/// How fast this machine decodes the clip into memory, as the player does
/// (a fresh buffer per frame, kept): frames/s.
fn decode_fps(index: &VideoIndex) -> f64 {
    let mut s = FrameStream::start(index, 0, &DecodeOptions::default()).expect("decoder");
    let (mut buf, mut kept) = (Vec::new(), Vec::new());
    let t = Instant::now();
    for _ in 0..120 {
        s.read(&mut buf).expect("read");
        kept.push(Arc::<[u8]>::from(buf.as_slice()));
    }
    120.0 / t.elapsed().as_secs_f64()
}

#[test]
fn playing_backward_decodes_a_group_at_a_time_and_keeps_up() {
    let Some(path) = fixture("counter_h264_gop250.mp4") else {
        eprintln!("skipped: counter_h264_gop250.mp4 not found (cargo xtask fixtures)");
        return;
    };
    let index = Arc::new(VideoIndex::open(&path).expect("opens"));
    let fps = index.fps.as_f64();
    let speed = decode_fps(&index);
    let mut player = Player::new(index.clone(), DecodeOptions::default(), 2 << 30);
    // 4 s backward from 580 at 1× crosses the groups starting at 500, 250 and 0.
    let share = play_backward(&mut player, fps, 580, 1.0, 4.0);
    let stats = player.stats();
    eprintln!(
        "backward at 1× on 1080p60, GOP 250: the exact frame was decoded on {:.1}% of UI frames ({} decoder starts; this machine decodes {speed:.0} fps)",
        share * 100.0,
        stats.spawns
    );
    assert!(stats.error.is_none(), "{:?}", stats.error);
    // The frame first shown, then each group once (and a spare).
    assert!(stats.spawns <= 6, "a decoder start per keyframe group, not per frame: {}", stats.spawns);
    // Real time needs decoding well above 60 fps, with a group's worth of lead (the user's desktop: ~650 fps).
    if speed >= 250.0 {
        assert!(share > 0.9, "{:.1}%", share * 100.0);
    } else {
        eprintln!("(not checked in real time: {speed:.0} fps is too slow to keep up with 1080p60 backward)");
    }
}
