//! Jev trackers (`Method::Jev`; dev only, experimental): TypeSafe's Jev, a
//! "System One" model behind an API, asked where the painted subject is
//! *(on request: "Paint over the subject multiple times, perhaps 3 or 5
//! examples, all contained within a sketch so we dont send full images. From
//! then on, automatically see how jev places a box over where it thinks the
//! subject is (with a confidence or occluded output thing too)")*.
//!
//! - **Examples:** each paint's frame through the tracker's view (the same
//!   crop that follows the guide as CoTracker's, [`CROP_W`] × [`CROP_H`]), the
//!   subject shown as the tracker's [`JevExamples`] says: the rest darkened,
//!   the painted area outlined, or only around it (scaled up).
//! - **Frames asked about:** every `every`th frame after the last paint,
//!   `frames` of them (forward only), each the same crop. Full frames are
//!   never sent.
//! - **One request a frame**, three questions about the same images: a Box
//!   (where the subject is), a Noul (it can be seen), and a Choice (visible,
//!   partly hidden, not in view). A Box always answers a box, so the other
//!   two say whether to believe it. [`IN_FLIGHT`] requests at once.
//! - **Output:** on a frame asked about, the box's centre and the box
//!   (source px), its confidence as the score, lost where Jev says the
//!   subject can't be seen; on a paint's frame, the paint; every other frame
//!   is left out ([`OFF`]): nothing in between is made up. What Jev answered
//!   goes to the app too ([`Answer`]).
//! - **The API key:** `TYPESAFE_API_KEY`, else (a Mac) the Keychain's
//!   `trackertools-typesafe` item, else `typesafe-key` in the data folder
//!   ([`api_key`], [`save_api_key`]). It goes to curl in a header file
//!   readable only by this user, never on a command line.
//! - `TT_JEV_URL` replaces the API's address (an `http://127.0.0.1` one: a
//!   test's fake).

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use tt_core::time::FrameIndex;
use tt_core::view::SpaceMap;

use super::learned::{CROP_H, CROP_W};
use super::{LookSpec, Phase, Side, Worker};
use crate::image::Grid;
use crate::{JevExamples, LOST, OFF, OUTSIDE};

/// The model asked: pinned (Box needs 1.14; `jev-preview` can change under us).
pub const MODEL: &str = "jev-1.14.0";
pub const URL: &str = "https://api.typesafe.ai/v1/systemone";
/// Requests in flight at once.
pub const IN_FLIGHT: usize = 4;
/// An image's sides, px: Jev takes 256 to 1568 a side and at least 78,400
/// pixels (280 × 280 is both).
const MIN_SIDE: usize = 280;
const MAX_SIDE: usize = 1568;
/// The Keychain item (a Mac) that holds the API key.
pub const KEYCHAIN_SERVICE: &str = "trackertools-typesafe";
const KEYCHAIN_ACCOUNT: &str = "api";
/// The outline drawn around a painted area ([`JevExamples::Outlined`]).
const OUTLINE: [u8; 3] = [255, 0, 255];

/// What Jev answered about a frame.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Answer {
    pub frame: FrameIndex,
    /// The box's confidence (0–1).
    pub confidence: f32,
    /// "The subject can be seen" (0–1).
    pub visible: f32,
    /// `visible`, `partly_hidden` or `not_in_view`, and each one's probability.
    pub view: String,
    pub view_p: Vec<(String, f32)>,
    /// Input tokens the request cost (images included).
    pub tokens: u64,
    /// How long the request took.
    pub ms: u32,
    /// The request failed: why (the frame is lost).
    pub error: Option<String>,
}

/// A request's answers as read: the box (fractions of the image), and the rest.
struct Reply {
    edges: [f64; 4],
    answer: Answer,
}

impl Worker {
    /// A Jev tracker's job: the examples from its paints, then Jev asked
    /// about every Nth frame after the last one (see the module docs).
    pub(super) fn run_jev(&mut self) -> Result<()> {
        let (from, to, side) = (self.spec.from, self.spec.to, self.spec.side);
        let settings = self.spec.jev.clone();
        let paints: Vec<LookSpec> = self.spec.looks.iter().filter(|l| l.mask.is_some()).cloned().collect();
        let samples: Vec<FrameIndex> = match (side, paints.iter().map(|l| l.frame).max()) {
            (Side::Forward, Some(last)) => (1..=settings.frames.max(1) as FrameIndex).map(|i| last + i * settings.every.max(1) as FrameIndex).filter(|f| (from..=to).contains(f)).collect(),
            _ => Vec::new(),
        };
        let dir: FrameIndex = if side == Side::Forward { 1 } else { -1 };
        let mut next = from;
        if samples.is_empty() {
            self.jev_skip_to(to + dir, &mut next, &paints);
            return Ok(());
        }
        let key = api_key().ok_or_else(|| anyhow!("no TypeSafe API key: enter it in the tracker's Inspector, or set TYPESAFE_API_KEY"))?;
        let url = endpoint()?;
        self.shared.set_phase(Phase::Tracking);

        // The examples: each paint through the view, at the scale every frame asked about fits.
        let scale = self.crop_scale(paints.iter().map(|l| l.frame).chain(samples.iter().copied()));
        let mut examples = Vec::new();
        for l in &paints {
            let frame = self.decode_one(l.frame)?;
            if self.cancelled() {
                return Ok(());
            }
            let (grid, rgb) = self.crop(&frame, l.frame, scale);
            let (w, h, px) = example(&rgb, grid, self.map(l.frame), l, settings.examples);
            examples.push(png(&px, w, h)?);
        }
        tracing::info!("{}: Jev asked about {} frames (every {} after frame {}), {} examples ({:?})", self.spec.label, samples.len(), settings.every, samples[0] - settings.every as FrameIndex, examples.len(), settings.examples);
        let examples = std::sync::Arc::new(examples);

        let mut stream: Option<tt_media::FrameStream> = None;
        let (mut held, mut buf) = (None, Vec::new());
        let mut pending: VecDeque<(FrameIndex, Grid, JoinHandle<Result<Reply>>)> = VecDeque::new();
        for &f in &samples {
            if !self.wait_for(f, || stream = None) {
                return Ok(());
            }
            if stream.is_none() {
                held = None;
                stream = Some(tt_media::FrameStream::start(&self.spec.video, self.presented(f), &self.spec.decode)?);
            }
            self.read_to(stream.as_mut().expect("opened"), &mut held, &mut buf, f)?;
            let (grid, rgb) = self.crop(&buf, f, scale);
            let image = png(&rgb, CROP_W, CROP_H)?;
            let (url, key, examples, style) = (url.clone(), key.clone(), examples.clone(), settings.examples);
            pending.push_back((f, grid, std::thread::spawn(move || ask(&url, &key, &examples, &image, style, f))));
            while pending.len() >= IN_FLIGHT {
                let (f, grid, h) = pending.pop_front().expect("one");
                self.jev_answered(f, grid, h, &mut next, &paints)?;
            }
        }
        while let Some((f, grid, h)) = pending.pop_front() {
            if self.cancelled() {
                return Ok(());
            }
            self.jev_answered(f, grid, h, &mut next, &paints)?;
        }
        self.jev_skip_to(to + dir, &mut next, &paints);
        Ok(())
    }

    /// Frame `f`'s answer is in: the frames before it left out, then it.
    fn jev_answered(&mut self, f: FrameIndex, grid: Grid, h: JoinHandle<Result<Reply>>, next: &mut FrameIndex, paints: &[LookSpec]) -> Result<()> {
        let reply = h.join().map_err(|_| anyhow!("the request for frame {f} stopped on an error"))?;
        self.jev_skip_to(f, next, paints);
        let answer = match reply {
            Ok(Reply { edges: [left, top, right, bottom], answer }) => {
                let map = *self.map(f);
                let corner = |x: f64, y: f64| map.to_source(grid.to_view([x * CROP_W as f64, y * CROP_H as f64]));
                let (a, b) = (corner(left, top), corner(right, bottom));
                let c = [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0];
                let lost = answer.visible < 0.5 || answer.view == "not_in_view";
                let flags = if lost { LOST } else { 0 } | if self.outside(f, map.from_source(c)) { OUTSIDE } else { 0 };
                self.out.push((f, [c[0], c[1], a[0], a[1], b[0], b[1], answer.confidence as f64, flags as f64].map(|v| v as f32)));
                answer
            }
            Err(e) => {
                let e = format!("{e:#}");
                if e.starts_with(REFUSED) {
                    bail!("{e}");
                }
                tracing::warn!("{}: Jev on frame {f}: {e}", self.spec.label);
                let g = self.guide(f);
                self.out.push((f, [g[0], g[1], g[0] - 8.0, g[1] - 8.0, g[0] + 8.0, g[1] + 8.0, 0.0, LOST as f64].map(|v| v as f32)));
                Answer { frame: f, error: Some(e), ..Answer::default() }
            }
        };
        let _ = self.tx.send(super::Msg::Jev(vec![answer]));
        self.shared.at.store(f, Ordering::Relaxed);
        *next = f + if self.spec.side == Side::Forward { 1 } else { -1 };
        self.flush();
        Ok(())
    }

    /// Every frame from `next` up to (not including) `end` that Jev isn't
    /// asked about: a paint's frame shows the paint, the rest are left out.
    fn jev_skip_to(&mut self, end: FrameIndex, next: &mut FrameIndex, paints: &[LookSpec]) {
        let dir: FrameIndex = if self.spec.side == Side::Forward { 1 } else { -1 };
        while (end - *next) * dir > 0 {
            let f = *next;
            let row = match paints.iter().find(|l| l.frame == f) {
                Some(l) => [l.center[0], l.center[1], l.center[0] - l.half[0], l.center[1] - l.half[1], l.center[0] + l.half[0], l.center[1] + l.half[1], 1.0, 0.0],
                None => {
                    let g = self.guide(f);
                    [g[0], g[1], g[0], g[1], g[0], g[1], 0.0, OFF as f64]
                }
            };
            self.out.push((f, row.map(|v| v as f32)));
            if self.out.len() >= 256 {
                self.flush();
            }
            *next += dir;
        }
        self.flush();
    }
}

// ------------------------------------------------------------ the examples

/// Paint `l` as an example of its subject, from its frame's crop `rgb`
/// (`grid` in the view `map`): `(w, h, RGB)`.
fn example(rgb: &[u8], grid: Grid, map: &SpaceMap, l: &LookSpec, style: JevExamples) -> (usize, usize, Vec<u8>) {
    let inside = painted_pixels(grid, map, l);
    match style {
        JevExamples::Dimmed => {
            let dim = |c: u8| (c as f32 * 0.3) as u8;
            let px = rgb.chunks(3).zip(&inside).flat_map(|(p, i)| if *i { [p[0], p[1], p[2]] } else { [dim(p[0]), dim(p[1]), dim(p[2])] }).collect();
            (CROP_W, CROP_H, px)
        }
        JevExamples::Outlined => {
            let mut px = rgb.to_vec();
            let at = |x: i64, y: i64| x >= 0 && y >= 0 && (x as usize) < CROP_W && (y as usize) < CROP_H && inside[y as usize * CROP_W + x as usize];
            for y in 0..CROP_H as i64 {
                for x in 0..CROP_W as i64 {
                    // Outside the paint, within 2 px of it: the outline.
                    let near = (-2..=2).any(|dy| (-2..=2).any(|dx| at(x + dx, y + dy)));
                    if !at(x, y) && near {
                        let i = (y as usize * CROP_W + x as usize) * 3;
                        px[i..i + 3].copy_from_slice(&OUTLINE);
                    }
                }
            }
            (CROP_W, CROP_H, px)
        }
        JevExamples::Tight => {
            let (mut x0, mut y0, mut x1, mut y1) = (CROP_W, CROP_H, 0, 0);
            for (i, _) in inside.iter().enumerate().filter(|(_, v)| **v) {
                let (x, y) = (i % CROP_W, i / CROP_W);
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
            }
            if x1 <= x0 {
                return (CROP_W, CROP_H, rgb.to_vec());
            }
            // A quarter of its size around it (8 px at least).
            let (mx, my) = (((x1 - x0) / 4).max(8), ((y1 - y0) / 4).max(8));
            let (x0, y0, x1, y1) = (x0.saturating_sub(mx), y0.saturating_sub(my), (x1 + mx).min(CROP_W), (y1 + my).min(CROP_H));
            let (w, h) = (x1 - x0, y1 - y0);
            let mut cut = Vec::with_capacity(w * h * 3);
            for y in y0..y1 {
                cut.extend_from_slice(&rgb[(y * CROP_W + x0) * 3..(y * CROP_W + x1) * 3]);
            }
            fit(&cut, w, h)
        }
    }
}

/// Which of the crop's pixels the paint covers.
fn painted_pixels(grid: Grid, map: &SpaceMap, l: &LookSpec) -> Vec<bool> {
    let Some((mask, n)) = l.mask.as_ref().and_then(|m| Some((m, super::paint::mask_side(m)?))) else { return vec![true; CROP_W * CROP_H] };
    let origin = [l.center[0] - l.half[0], l.center[1] - l.half[1]];
    let mut out = Vec::with_capacity(CROP_W * CROP_H);
    for y in 0..CROP_H {
        for x in 0..CROP_W {
            let s = map.to_source(grid.to_view([x as f64 + 0.5, y as f64 + 0.5]));
            let (u, v) = ((s[0] - origin[0]) / (2.0 * l.half[0]) * n as f64, (s[1] - origin[1]) / (2.0 * l.half[1]) * n as f64);
            out.push(u >= 0.0 && v >= 0.0 && (u as usize) < n && (v as usize) < n && mask[v as usize * n + u as usize] > 0);
        }
    }
    out
}

/// `w × h` RGB scaled (bilinear) so each side is at least [`MIN_SIDE`] and
/// at most [`MAX_SIDE`].
fn fit(rgb: &[u8], w: usize, h: usize) -> (usize, usize, Vec<u8>) {
    let k = (MIN_SIDE as f64 / w.min(h) as f64).max(1.0).min(MAX_SIDE as f64 / w.max(h) as f64);
    let (nw, nh) = (((w as f64 * k).round() as usize).clamp(MIN_SIDE, MAX_SIDE), ((h as f64 * k).round() as usize).clamp(MIN_SIDE, MAX_SIDE));
    let mut out = Vec::with_capacity(nw * nh * 3);
    for y in 0..nh {
        let sy = ((y as f64 + 0.5) * h as f64 / nh as f64 - 0.5).clamp(0.0, (h - 1) as f64);
        let (y0, fy) = (sy.floor() as usize, sy.fract());
        let y1 = (y0 + 1).min(h - 1);
        for x in 0..nw {
            let sx = ((x as f64 + 0.5) * w as f64 / nw as f64 - 0.5).clamp(0.0, (w - 1) as f64);
            let (x0, fx) = (sx.floor() as usize, sx.fract());
            let x1 = (x0 + 1).min(w - 1);
            for c in 0..3 {
                let p = |x: usize, y: usize| rgb[(y * w + x) * 3 + c] as f64;
                let v = (p(x0, y0) * (1.0 - fx) + p(x1, y0) * fx) * (1.0 - fy) + (p(x0, y1) * (1.0 - fx) + p(x1, y1) * fx) * fy;
                out.push(v.round().clamp(0.0, 255.0) as u8);
            }
        }
    }
    (nw, nh, out)
}

fn png(rgb: &[u8], w: usize, h: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut e = png::Encoder::new(&mut out, w as u32, h as u32);
    e.set_color(png::ColorType::Rgb);
    e.set_depth(png::BitDepth::Eight);
    e.write_header()?.write_image_data(rgb)?;
    Ok(out)
}

// ------------------------------------------------------------ the request

/// What an answer that refused the key starts with (the job stops: every frame would fail).
const REFUSED: &str = "TypeSafe refused the API key";

/// The API's address: [`URL`], or `TT_JEV_URL` (https, or http to this computer only).
fn endpoint() -> Result<String> {
    let url = std::env::var("TT_JEV_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| URL.to_string());
    let lower = url.to_ascii_lowercase();
    let local = ["http://127.0.0.1:", "http://localhost:", "http://[::1]:"].iter().any(|p| lower.starts_with(p));
    if !lower.starts_with("https://") && !local {
        bail!("TT_JEV_URL must be an https address (or http to this computer): {url}");
    }
    Ok(url)
}

/// What the questions say about the examples, by how they show the subject.
fn intro(style: JevExamples) -> String {
    let how = match style {
        JevExamples::Dimmed => "In each picture the subject is at full brightness and everything else is darkened.",
        JevExamples::Outlined => "In each picture the subject is outlined in magenta.",
        JevExamples::Tight => "Each picture is cut closely around the subject.",
    };
    format!("The pictures in `examples` show one subject, on other frames of the same video. {how}")
}

/// The request's JSON: `examples` and `frame` in the state, each image an asset of its multipart part.
fn request(n_examples: usize, style: JevExamples, nonce: &str) -> serde_json::Value {
    let ex_ref = |i: usize| format!("tt-jev-{nonce}-example-{i}");
    let frame_ref = format!("tt-jev-{nonce}-frame");
    let mut assets = serde_json::Map::new();
    for i in 0..n_examples {
        assets.insert(ex_ref(i), serde_json::json!({ "type": "image", "part": format!("ex{i}") }));
    }
    assets.insert(frame_ref.clone(), serde_json::json!({ "type": "image", "part": "frame" }));
    let intro = intro(style);
    serde_json::json!({
        "model": MODEL,
        "state": { "examples": (0..n_examples).map(ex_ref).collect::<Vec<_>>(), "frame": frame_ref },
        "assets": assets,
        "questions": {
            "where": { "type": "box", "instructions": format!("{intro} Find that subject in `frame`.") },
            "visible": { "type": "noul", "instructions": format!("{intro} That subject can be seen in `frame`, at least in part.") },
            "view": {
                "type": "choice",
                "instructions": format!("{intro} How much of that subject can be seen in `frame`?"),
                "criteria": {
                    "visible": "All or nearly all of the subject can be seen.",
                    "partly_hidden": "Part of the subject is hidden behind something, or cut off by the edge of the picture.",
                    "not_in_view": "The subject cannot be seen."
                }
            }
        }
    })
}

/// Ask Jev about frame `f` (its crop as a PNG), with the examples.
fn ask(url: &str, key: &str, examples: &[Vec<u8>], frame: &[u8], style: JevExamples, f: FrameIndex) -> Result<Reply> {
    static N: AtomicU64 = AtomicU64::new(0);
    let nonce = format!("{}-{}-{}", std::process::id(), f, N.fetch_add(1, Ordering::Relaxed));
    let dir = std::env::temp_dir().join(format!("tt-jev-{nonce}"));
    std::fs::create_dir_all(&dir)?;
    let done = (|| {
        for (i, ex) in examples.iter().enumerate() {
            std::fs::write(dir.join(format!("ex{i}.png")), ex)?;
        }
        std::fs::write(dir.join("frame.png"), frame)?;
        std::fs::write(dir.join("request.json"), serde_json::to_vec(&request(examples.len(), style, &nonce))?)?;
        private_file(&dir.join("headers"), format!("Authorization: Bearer {key}\r\nTypeSafe-Danger-Zone-Images: true\r\n").as_bytes())?;
        let mut cmd = curl();
        cmd.current_dir(&dir).args(["-sS", "--max-time", "120", "--proto", "=https,http", "-X", "POST", "-H", "@headers", "-F", "request=<request.json;type=application/json"]);
        for i in 0..examples.len() {
            cmd.arg("-F").arg(format!("ex{i}=@ex{i}.png;type=image/png"));
        }
        cmd.args(["-F", "frame=@frame.png;type=image/png", "-w", "\n%{http_code}"]).arg(url);
        let t0 = Instant::now();
        let out = cmd.output().context("curl")?;
        let ms = t0.elapsed().as_millis() as u32;
        let text = String::from_utf8_lossy(&out.stdout);
        let (body, code) = text.rsplit_once('\n').unwrap_or(("", &text));
        if !out.status.success() {
            bail!("the request did not complete: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        match code.trim() {
            "200" => {}
            "401" | "403" => bail!("{REFUSED} ({code}): {}", short(body)),
            other => bail!("TypeSafe answered {other}: {}", short(body)),
        }
        let mut reply = read_reply(body).with_context(|| format!("TypeSafe's answer: {}", short(body)))?;
        reply.answer.frame = f;
        reply.answer.ms = ms;
        Ok(reply)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    done
}

fn short(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > 300 { format!("{}…", s.chars().take(300).collect::<String>()) } else { s.to_string() }
}

/// The answers of a 200 response.
fn read_reply(body: &str) -> Result<Reply> {
    let v: serde_json::Value = serde_json::from_str(body)?;
    let a = &v["answers"];
    let b = &a["where"]["box"];
    let edge = |k: &str| b[k].as_f64().with_context(|| format!("no box {k}"));
    let edges = [edge("left")?, edge("top")?, edge("right")?, edge("bottom")?].map(|e| e.clamp(0.0, 1.0));
    let num = |x: &serde_json::Value| x.as_f64().unwrap_or(0.0) as f32;
    // (Probabilities as an object of label → p, or a list of { option/label, probability }.)
    let view_p: Vec<(String, f32)> = match &a["view"]["probabilities"] {
        serde_json::Value::Object(m) => m.iter().map(|(k, p)| (k.clone(), num(p))).collect(),
        serde_json::Value::Array(l) => l
            .iter()
            .filter_map(|e| Some((e.get("option").or(e.get("label")).or(e.get("choice"))?.as_str()?.to_string(), num(e.get("probability").or(e.get("p"))?))))
            .collect(),
        _ => Vec::new(),
    };
    Ok(Reply {
        edges,
        answer: Answer {
            confidence: num(&a["where"]["confidence"]),
            visible: num(&a["visible"]["noul"]),
            view: a["view"]["choice"].as_str().unwrap_or_default().to_string(),
            view_p,
            tokens: v["usage"]["input_tokens"].as_u64().unwrap_or(0),
            ..Answer::default()
        },
    })
}

/// curl (Windows' own in System32, with no console window).
fn curl() -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let root = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        let own = root.join("System32").join("curl.exe");
        let mut cmd = Command::new(if own.is_file() { own } else { PathBuf::from("curl") });
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        cmd
    }
    #[cfg(not(windows))]
    Command::new("curl")
}

/// Write `bytes` to `path`, readable by this user only (on unix).
fn private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)?.write_all(bytes)
}

// ------------------------------------------------------------ the API key

/// The file that holds the key where there is no Keychain.
fn key_file() -> PathBuf {
    tt_media::proxy::data_dir().join("typesafe-key")
}

/// The TypeSafe API key: `TYPESAFE_API_KEY`, else the Keychain (a Mac),
/// else the data folder's `typesafe-key`.
pub fn api_key() -> Option<String> {
    let clean = |s: String| Some(s.trim().to_string()).filter(|k| !k.is_empty());
    if let Some(k) = std::env::var("TYPESAFE_API_KEY").ok().and_then(clean) {
        return Some(k);
    }
    if cfg!(target_os = "macos")
        && let Ok(out) = Command::new("/usr/bin/security").args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-a", KEYCHAIN_ACCOUNT, "-w"]).stderr(Stdio::null()).output()
        && out.status.success()
        && let Some(k) = clean(String::from_utf8_lossy(&out.stdout).into_owned())
    {
        return Some(k);
    }
    std::fs::read_to_string(key_file()).ok().and_then(clean)
}

/// Where [`save_api_key`] keeps the key, for people.
pub fn key_place() -> String {
    if cfg!(target_os = "macos") { format!("the Keychain (item \"{KEYCHAIN_SERVICE}\")") } else { key_file().display().to_string() }
}

/// Keep `key` for [`api_key`]: in the Keychain on a Mac (given to `security`
/// on its input, not its command line), else in the data folder (this user only).
pub fn save_api_key(key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c)) {
        bail!("the key has characters an API key doesn't have");
    }
    if cfg!(target_os = "macos") {
        let mut child = Command::new("/usr/bin/security").arg("-i").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().context("security")?;
        child
            .stdin
            .take()
            .expect("piped")
            .write_all(format!("add-generic-password -U -s {KEYCHAIN_SERVICE} -a {KEYCHAIN_ACCOUNT} -l \"trackertools: TypeSafe API key\" -w {key}\n").as_bytes())?;
        let out = child.wait_with_output()?;
        // (With TYPESAFE_API_KEY set, that is what api_key() gives: the Keychain can't be checked through it.)
        if !out.status.success() || (api_key().as_deref() != Some(key) && std::env::var_os("TYPESAFE_API_KEY").is_none()) {
            bail!("the Keychain did not keep the key: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        return Ok(());
    }
    let path = key_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    private_file(&path, key.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square_paint() -> LookSpec {
        // A 10 × 10 px square, painted whole, at (100, 100).
        LookSpec { frame: 0, center: [100.0, 100.0], half: [5.0, 5.0], mask: Some(vec![255; 32 * 32]), pattern: 0 }
    }

    fn crop_of(colour: u8) -> Vec<u8> {
        vec![colour; CROP_W * CROP_H * 3]
    }

    /// The crop centred on (100, 100) at 4 crop px a source px (an identity view).
    fn grid() -> (Grid, SpaceMap) {
        let scale = 4.0;
        (Grid { origin: [100.0 - CROP_W as f64 / 2.0 / scale, 100.0 - CROP_H as f64 / 2.0 / scale], scale }, SpaceMap { a: 1.0, b: [0.0, 0.0], canvas: [1920.0, 1080.0] })
    }

    #[test]
    fn examples_show_the_paint_three_ways() {
        let (g, m) = grid();
        let l = square_paint();
        let inside = painted_pixels(g, &m, &l);
        // 10 source px at 4 crop px each: 40 × 40 crop px in the middle.
        assert_eq!(inside.iter().filter(|v| **v).count(), 40 * 40);
        assert!(inside[(CROP_H / 2) * CROP_W + CROP_W / 2]);

        let (w, h, dim) = example(&crop_of(200), g, &m, &l, JevExamples::Dimmed);
        assert_eq!((w, h), (CROP_W, CROP_H));
        assert_eq!(dim[((CROP_H / 2) * CROP_W + CROP_W / 2) * 3], 200, "the paint at full brightness");
        assert_eq!(dim[0], 60, "the rest darkened");

        let (_, _, out) = example(&crop_of(200), g, &m, &l, JevExamples::Outlined);
        let magenta = out.chunks(3).filter(|p| *p == OUTLINE).count();
        assert!(magenta > 4 * 40 && magenta < 4 * 48 * 2, "{magenta} outline px around 40 × 40");

        let (w, h, tight) = example(&crop_of(200), g, &m, &l, JevExamples::Tight);
        assert!(w >= MIN_SIDE && h >= MIN_SIDE && w * h >= 78_400 && w <= MAX_SIDE, "{w} × {h}");
        assert_eq!(tight.len(), w * h * 3);
    }

    #[test]
    fn the_request_names_every_image_once() {
        let r = request(3, JevExamples::Dimmed, "n1");
        let assets = r["assets"].as_object().unwrap();
        assert_eq!(assets.len(), 4);
        assert_eq!(r["state"]["examples"].as_array().unwrap().len(), 3);
        assert_eq!(assets[r["state"]["frame"].as_str().unwrap()]["part"], "frame");
        assert_eq!(r["questions"]["where"]["type"], "box");
        assert_eq!(r["questions"]["view"]["criteria"].as_object().unwrap().len(), 3);
        assert_eq!(r["model"], MODEL);
    }

    #[test]
    fn an_answer_is_read() {
        let body = r#"{"model":"jev-1.14.0","answers":{"where":{"type":"box","box":{"top":0.25,"bottom":0.75,"left":0.5,"right":0.625},"confidence":0.8},"visible":{"type":"noul","noul":0.9},"view":{"type":"choice","choice":"partly_hidden","probabilities":{"visible":0.3,"partly_hidden":0.6,"not_in_view":0.1},"confidence":0.6}},"usage":{"input_tokens":1234,"output_tokens":5}}"#;
        let r = read_reply(body).unwrap();
        assert_eq!(r.edges, [0.5, 0.25, 0.625, 0.75]);
        assert_eq!((r.answer.confidence, r.answer.visible, r.answer.view.as_str(), r.answer.tokens), (0.8, 0.9, "partly_hidden", 1234));
        assert_eq!(r.answer.view_p.len(), 3);
        assert!(read_reply(r#"{"answers":{}}"#).is_err(), "no box");
    }

    #[test]
    fn only_https_or_this_computer() {
        // SAFETY: (test) nothing else in this binary reads TT_JEV_URL.
        unsafe { std::env::set_var("TT_JEV_URL", "http://example.com/v1") };
        assert!(endpoint().is_err());
        unsafe { std::env::set_var("TT_JEV_URL", "http://127.0.0.1:9/v1/systemone") };
        assert!(endpoint().is_ok());
        unsafe { std::env::remove_var("TT_JEV_URL") };
        assert_eq!(endpoint().unwrap(), URL);
    }
}
