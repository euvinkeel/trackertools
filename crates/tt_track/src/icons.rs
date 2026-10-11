//! Cursor icons for template trackers *(on request: "store common mouse
//! icons like the windows pointers, roblox pointers, straight from the
//! source … an optional button like 'Import common icons' and we choose
//! what pack to auto insert … it would for the old template trackers, so
//! it'd be available there first"; then: "have the ez icon imports
//! available for the dumb template trackers. those legit are just way more
//! reliable")*.
//!
//! An icon is one more look of a tracker: matched like the looks cut from
//! the video, its transparency the mask (`job`'s `look_templates`). Icons
//! aren't shipped with the app: a pack is read from this computer's own
//! files — Windows' cursor scheme (the registry's, else its default
//! cursors) and a Roblox install's — so a cursor that changes icon is
//! followed without a look on a frame of each icon.
//!
//! The size the icon has in the video (screen scaling, the recording's
//! resolution) comes from the tracker's own looks: the job tries sizes and
//! keeps the one that matches them best ([`IconFit`]), unless one is set.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;

/// One cursor icon: its pixels (RGBA, row-major, cropped to what shows) and
/// its hotspot (icon px from its top-left: the point the cursor is at).
#[derive(Reflect, Clone, Debug, PartialEq, Default)]
pub struct Icon {
    /// The pack it came from ("Windows", "Roblox").
    pub pack: String,
    pub name: String,
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
    pub hotspot: [f32; 2],
}

/// A template tracker's cursor icons, and the size of each pack's in the video.
#[derive(Component, Reflect, Clone, Debug, PartialEq, Default)]
#[reflect(Component)]
pub struct CursorIcons {
    pub icons: Vec<Icon>,
    /// Packs whose size is set; the others' is what matches the tracker's looks best.
    #[reflect(default)]
    pub sizes: Vec<PackSize>,
}

/// A pack's icons' size in the video: video px per icon px.
#[derive(Reflect, Clone, Debug, PartialEq)]
pub struct PackSize {
    pub pack: String,
    pub size: f32,
}

impl CursorIcons {
    /// The packs in it, in the order they were added.
    pub fn packs(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for i in &self.icons {
            if !out.contains(&i.pack) {
                out.push(i.pack.clone());
            }
        }
        out
    }

    /// Pack `pack`'s size where it is set.
    pub fn size_of(&self, pack: &str) -> Option<f32> {
        self.sizes.iter().find(|s| s.pack == pack).map(|s| s.size)
    }
}

/// What a tracker's job made of its icons: each pack's size, and the look
/// the icons lined up with (their point is that look's), if any.
#[derive(Component, Clone, Debug, PartialEq, Default)]
pub struct IconFit {
    pub packs: Vec<PackFit>,
    pub lined_up: Option<String>,
}

/// A pack's size as matched: set or found from the looks (`auto`), and
/// whether its icons match one of the looks well at it.
#[derive(Clone, Debug, PartialEq)]
pub struct PackFit {
    pub pack: String,
    pub size: f32,
    pub auto: bool,
    pub matched: bool,
}

/// Icons found on this computer, to add to a tracker.
#[derive(Clone, Debug)]
pub struct Pack {
    pub name: &'static str,
    /// Where they were read from.
    pub source: PathBuf,
    pub icons: Vec<Icon>,
}

/// The packs on this computer (none where nothing is found), read once.
pub fn packs() -> &'static [Pack] {
    static PACKS: std::sync::OnceLock<Vec<Pack>> = std::sync::OnceLock::new();
    PACKS.get_or_init(|| [windows_pack(), roblox_pack()].into_iter().flatten().collect())
}

/// Windows' cursors, by role (the scheme's registry value, its default file).
/// The I-beam and crosshair are left out: Windows draws them inverted (each
/// pixel flips what is behind it), so they look different on every background.
const WINDOWS: [(&str, &str, &str); 12] = [
    ("Arrow", "Arrow", "aero_arrow.cur"),
    ("Hand", "Hand", "aero_link.cur"),
    ("Busy arrow", "AppStarting", "aero_working.ani"),
    ("Help", "Help", "aero_helpsel.cur"),
    ("Unavailable", "No", "aero_unavail.cur"),
    ("Move", "SizeAll", "aero_move.cur"),
    ("Resize up/down", "SizeNS", "aero_ns.cur"),
    ("Resize left/right", "SizeWE", "aero_ew.cur"),
    ("Resize \u{2196}\u{2198}", "SizeNWSE", "aero_nwse.cur"),
    ("Resize \u{2197}\u{2199}", "SizeNESW", "aero_nesw.cur"),
    ("Pen", "NWPen", "aero_pen.cur"),
    ("Up", "UpArrow", "aero_up.cur"),
];

/// Windows' cursors, as this computer's scheme has them.
pub fn windows_pack() -> Option<Pack> {
    let root = PathBuf::from(std::env::var_os("SystemRoot")?).join("Cursors");
    let scheme = windows_scheme();
    let icons: Vec<Icon> = WINDOWS
        .iter()
        .filter_map(|(name, role, file)| {
            let path = match scheme.iter().find(|(r, _)| r == role) {
                Some((_, p)) if p.as_os_str().is_empty() => return None,
                Some((_, p)) => p.clone(),
                None => root.join(file),
            };
            let bytes = std::fs::read(&path).ok()?;
            let (w, h, rgba, hot) = read_cursor(&bytes, 32).map_err(|e| tracing::debug!("{}: {e:#}", path.display())).ok()?;
            Some(crop(Icon { pack: "Windows".into(), name: name.to_string(), w, h, rgba, hotspot: hot }))
        })
        .collect();
    (!icons.is_empty()).then_some(Pack { name: "Windows", source: root, icons })
}

/// The current user's cursor scheme: each role's file (empty: Windows' own,
/// drawn inverted), as the registry has them.
fn windows_scheme() -> Vec<(String, PathBuf)> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let mut cmd = std::process::Command::new("reg");
    cmd.args(["query", r"HKCU\Control Panel\Cursors"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let Ok(out) = cmd.output() else { return Vec::new() };
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut parts = l.trim().splitn(3, "    ");
            let (role, kind) = (parts.next()?, parts.next()?);
            (kind.starts_with("REG_")).then(|| {
                let value = parts.next().unwrap_or("").trim();
                let expanded = value.replace("%SystemRoot%", &root).replace("%SYSTEMROOT%", &root);
                (role.to_string(), PathBuf::from(expanded))
            })
        })
        .filter(|(role, _)| WINDOWS.iter().any(|(_, r, _)| r == role))
        .collect()
}

/// Roblox's player cursors (the newest install that has them). Roblox
/// draws a cursor image centred on the mouse: the hotspot is its middle.
pub fn roblox_pack() -> Option<Pack> {
    let versions = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Roblox").join("Versions");
    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&versions)
        .ok()?
        .flatten()
        .map(|e| e.path().join("content/textures/Cursors/KeyboardMouse"))
        .filter(|d| d.join("ArrowFarCursor.png").is_file())
        .map(|d| (std::fs::metadata(&d).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), d))
        .collect();
    dirs.sort();
    let (_, dir) = dirs.pop()?;
    let icons: Vec<Icon> = [("Arrow", "ArrowFarCursor.png"), ("Hand", "ArrowCursor.png"), ("I-beam", "IBeamCursor.png")]
        .iter()
        .filter_map(|(name, file)| {
            let (w, h, rgba) = read_png(&std::fs::read(dir.join(file)).ok()?).ok()?;
            Some(crop(Icon { pack: "Roblox".into(), name: name.to_string(), w, h, rgba, hotspot: [w as f32 / 2.0, h as f32 / 2.0] }))
        })
        .collect();
    (!icons.is_empty()).then_some(Pack { name: "Roblox", source: dir, icons })
}

/// A `.cur`, `.ico` or animated `.ani` (its first frame): the image nearest
/// `size` px with an alpha channel, as `(w, h, RGBA, hotspot)`.
pub fn read_cursor(bytes: &[u8], size: u32) -> Result<(u32, u32, Vec<u8>, [f32; 2])> {
    if bytes.get(..4) == Some(b"RIFF") {
        return read_cursor(first_ani_frame(bytes)?, size);
    }
    let u16_at = |o: usize| bytes.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |o: usize| bytes.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let (Some(0), Some(kind @ (1 | 2)), Some(n)) = (u16_at(0), u16_at(2), u16_at(4)) else { bail!("not a cursor or icon file") };
    let mut best: Option<(u32, u32, Vec<u8>, [f32; 2])> = None;
    for i in 0..n as usize {
        let e = 6 + 16 * i;
        let (Some(hx), Some(hy), Some(len), Some(off)) = (u16_at(e + 4), u16_at(e + 6), u32_at(e + 8), u32_at(e + 12)) else { bail!("cut short") };
        let data = bytes.get(off as usize..(off + len) as usize).context("an image past the end")?;
        let image = if data.starts_with(b"\x89PNG") { read_png(data).ok() } else { read_dib(data).ok() };
        let Some((w, h, rgba)) = image else { continue };
        // (An icon's directory has planes and bits there, not a hotspot.)
        let hot = if kind == 2 { [hx as f32, hy as f32] } else { [0.0, 0.0] };
        let closer = best.as_ref().is_none_or(|(bw, _, _, _)| (w as i64 - size as i64).abs() < (*bw as i64 - size as i64).abs());
        if closer {
            best = Some((w, h, rgba, hot));
        }
    }
    best.context("no image with an alpha channel in it")
}

/// An animated cursor's first frame (a whole `.cur` inside its `fram` list).
fn first_ani_frame(bytes: &[u8]) -> Result<&[u8]> {
    fn chunks(b: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
        let mut at = 0;
        std::iter::from_fn(move || {
            let id = b.get(at..at + 4)?;
            let len = u32::from_le_bytes(b.get(at + 4..at + 8)?.try_into().ok()?) as usize;
            let body = b.get(at + 8..at + 8 + len)?;
            at += 8 + len + len % 2;
            Some((id, body))
        })
    }
    if bytes.get(8..12) != Some(b"ACON") {
        bail!("not an animated cursor");
    }
    for (id, body) in chunks(&bytes[12..]) {
        if id == b"LIST"
            && body.get(..4) == Some(b"fram")
            && let Some((_, icon)) = chunks(&body[4..]).find(|(id, _)| *id == b"icon")
        {
            return Ok(icon);
        }
    }
    bail!("no frames in the animated cursor")
}

/// A 32-bit DIB from an icon file (bottom-up BGRA, then the AND mask): `(w, h, RGBA)`.
fn read_dib(d: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let i32_at = |o: usize| d.get(o..o + 4).map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let (Some(w), Some(h2)) = (i32_at(4), i32_at(8)) else { bail!("cut short") };
    let bpp = d.get(14..16).map(|b| u16::from_le_bytes([b[0], b[1]])).context("cut short")?;
    if bpp != 32 {
        // (Monochrome and palette cursors: many are drawn inverted, no fixed picture.)
        bail!("{bpp}-bit image");
    }
    let header = i32_at(0).context("cut short")? as usize;
    let (w, h) = (w.unsigned_abs() as usize, (h2.unsigned_abs() / 2) as usize);
    let px = d.get(header..header + w * h * 4).context("pixels cut short")?;
    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let s = ((h - 1 - y) * w + x) * 4;
            rgba[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&[px[s + 2], px[s + 1], px[s], px[s + 3]]);
        }
    }
    // No alpha in it (an older icon): the AND mask says what shows.
    if rgba.chunks(4).all(|p| p[3] == 0) {
        let stride = w.div_ceil(32) * 4;
        let mask = d.get(header + w * h * 4..).context("no mask")?;
        for y in 0..h {
            for x in 0..w {
                let bit = mask.get((h - 1 - y) * stride + x / 8).is_some_and(|b| b >> (7 - x % 8) & 1 == 1);
                rgba[(y * w + x) * 4 + 3] = if bit { 0 } else { 255 };
            }
        }
    }
    Ok((w as u32, h as u32, rgba))
}

/// A PNG, as `(w, h, RGBA)`.
pub fn read_png(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16 | png::Transformations::ALPHA);
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size().context("too big")?];
    let info = reader.next_frame(&mut buf)?;
    let (w, h) = (info.width, info.height);
    let px = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::GrayscaleAlpha => px.chunks(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Rgb => px.chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::Grayscale => px.iter().flat_map(|g| [*g, *g, *g, 255]).collect(),
        other => bail!("PNG colour type {other:?}"),
    };
    Ok((w, h, rgba))
}

/// `icon` cut to the pixels that show (and one around), its hotspot moved with it.
pub fn crop(icon: Icon) -> Icon {
    let (w, h) = (icon.w as usize, icon.h as usize);
    let shows = |x: usize, y: usize| icon.rgba[(y * w + x) * 4 + 3] > 8;
    let xs = (0..w).filter(|x| (0..h).any(|y| shows(*x, y)));
    let ys = (0..h).filter(|y| (0..w).any(|x| shows(x, *y)));
    let (Some(x0), Some(x1), Some(y0), Some(y1)) = (xs.clone().min(), xs.max(), ys.clone().min(), ys.max()) else { return icon };
    let (x0, y0, x1, y1) = (x0.saturating_sub(1), y0.saturating_sub(1), (x1 + 2).min(w), (y1 + 2).min(h));
    let mut rgba = Vec::with_capacity((x1 - x0) * (y1 - y0) * 4);
    for y in y0..y1 {
        rgba.extend_from_slice(&icon.rgba[(y * w + x0) * 4..(y * w + x1) * 4]);
    }
    Icon { w: (x1 - x0) as u32, h: (y1 - y0) as u32, rgba, hotspot: [icon.hotspot[0] - x0 as f32, icon.hotspot[1] - y0 as f32], ..icon }
}


/// `icon` as a look to match, `per_px` patch px per icon px: its template
/// (square, its pixels in their own colour, weighted by how solid each is),
/// and its hotspot from the template's centre (patch px).
pub fn template_of(icon: &Icon, per_px: f64, tolerance: crate::ncc::Tolerance) -> Option<(crate::template::LookTemplate, [f64; 2])> {
    const PAD: usize = 2;
    const SUB: usize = 4;
    let (w, h) = (icon.w as usize, icon.h as usize);
    if w == 0 || h == 0 || icon.rgba.len() < w * h * 4 || per_px <= 0.0 {
        return None;
    }
    let r = ((w.max(h) as f64 * per_px / 2.0).ceil() as usize + 1).max(2);
    let n = 2 * r + 1;
    let size = n + 2 * PAD;
    let c = (PAD + r) as f64 + 0.5;
    // Premultiplied, so a soft edge doesn't drag the colour toward whatever its transparent pixels hold.
    let at = |x: i64, y: i64| -> [f64; 4] {
        if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 {
            return [0.0; 4];
        }
        let i = (y as usize * w + x as usize) * 4;
        let a = icon.rgba[i + 3] as f64 / 255.0;
        [icon.rgba[i] as f64 * a, icon.rgba[i + 1] as f64 * a, icon.rgba[i + 2] as f64 * a, a]
    };
    let mut luma = vec![128.0f32; size * size];
    let (mut cb, mut cr) = (vec![128.0f32; size * size], vec![128.0f32; size * size]);
    let mut alpha = vec![0.0f32; size * size];
    for py in 0..size {
        for px in 0..size {
            // Each patch pixel: the icon pixels under it, a few samples a side.
            let mut acc = [0.0f64; 4];
            for sy in 0..SUB {
                for sx in 0..SUB {
                    let u = (px as f64 + (sx as f64 + 0.5) / SUB as f64 - c) / per_px + w as f64 / 2.0;
                    let v = (py as f64 + (sy as f64 + 0.5) / SUB as f64 - c) / per_px + h as f64 / 2.0;
                    let p = at(u.floor() as i64, v.floor() as i64);
                    acc.iter_mut().zip(p).for_each(|(a, b)| *a += b);
                }
            }
            let k = (SUB * SUB) as f64;
            let a = acc[3] / k;
            let i = py * size + px;
            alpha[i] = a as f32;
            if a > 1e-3 {
                let [r8, g8, b8] = [acc[0] / k / a, acc[1] / k / a, acc[2] / k / a];
                // As the video has it (BT.709, video range).
                let y = 0.2126 * r8 + 0.7152 * g8 + 0.0722 * b8;
                luma[i] = (16.0 + y * 219.0 / 255.0) as f32;
                cb[i] = (128.0 + (b8 - y) / 1.8556 * 224.0 / 255.0) as f32;
                cr[i] = (128.0 + (r8 - y) / 1.5748 * 224.0 / 255.0) as f32;
            }
        }
    }
    let mut patch = crate::image::Patch::new(size, size, luma);
    patch.colour = Some(Box::new([cb, cr]));
    // The mask: one cell per template pixel, how solid it is.
    let mask: Vec<u8> = (0..n * n).map(|k| (alpha[(PAD + k / n) * size + PAD + k % n] * 255.0).round() as u8).collect();
    let t = crate::template::LookTemplate::cut(&patch, [c, c], [r, r], Some(mask), tolerance)?;
    let hot = [(icon.hotspot[0] as f64 - w as f64 / 2.0) * per_px, (icon.hotspot[1] as f64 - h as f64 / 2.0) * per_px];
    Some((t, hot))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `.cur` with one 4×3 32-bit image, hotspot (1, 2).
    fn cur() -> Vec<u8> {
        let (w, h) = (4usize, 3usize);
        let mut dib = Vec::new();
        dib.extend(40u32.to_le_bytes());
        dib.extend((w as i32).to_le_bytes());
        dib.extend((2 * h as i32).to_le_bytes());
        dib.extend(1u16.to_le_bytes());
        dib.extend(32u16.to_le_bytes());
        dib.extend([0u8; 24]);
        // Bottom-up rows: the bottom row red, opaque; the rest see-through.
        for y in 0..h {
            for _ in 0..w {
                dib.extend(if y == 0 { [0, 0, 255, 255] } else { [0, 0, 0, 0] });
            }
        }
        dib.extend(vec![0u8; 4 * h]);
        let mut f = Vec::new();
        f.extend([0, 0, 2, 0, 1, 0]);
        f.extend([w as u8, h as u8, 0, 0]);
        f.extend(1u16.to_le_bytes());
        f.extend(2u16.to_le_bytes());
        f.extend((dib.len() as u32).to_le_bytes());
        f.extend(22u32.to_le_bytes());
        f.extend(dib);
        f
    }

    #[test]
    fn reads_a_cursor_file_top_down_with_its_hotspot() {
        let (w, h, rgba, hot) = read_cursor(&cur(), 32).expect("reads");
        assert_eq!((w, h, hot), (4, 3, [1.0, 2.0]));
        // The bottom row (stored first) is red and opaque; the top see-through.
        assert_eq!(&rgba[(2 * 4) * 4..(2 * 4) * 4 + 4], &[255, 0, 0, 255]);
        assert_eq!(rgba[3], 0);
    }

    #[test]
    fn reads_an_animated_cursors_first_frame() {
        let icon = cur();
        let mut fram = b"fram".to_vec();
        fram.extend(b"icon");
        fram.extend((icon.len() as u32).to_le_bytes());
        fram.extend(&icon);
        let mut body = b"ACON".to_vec();
        body.extend(b"LIST");
        body.extend((fram.len() as u32).to_le_bytes());
        body.extend(&fram);
        let mut ani = b"RIFF".to_vec();
        ani.extend((body.len() as u32).to_le_bytes());
        ani.extend(body);
        assert_eq!(read_cursor(&ani, 32).expect("reads").3, [1.0, 2.0]);
    }

    /// An arrow-ish icon: a white wedge with a dark outline, its tip (hotspot) at its top-left.
    fn arrow() -> Icon {
        let (w, h) = (9u32, 13u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for y in 0..h as i64 {
            for x in 0..w as i64 {
                let inside = x <= y * 2 / 3 && y < 12;
                let edge = inside && (x == 0 || x == y * 2 / 3 || y == 11);
                let i = ((y * w as i64 + x) * 4) as usize;
                if inside {
                    let v = if edge { 20 } else { 245 };
                    rgba[i..i + 4].copy_from_slice(&[v, v, v, 255]);
                }
            }
        }
        Icon { pack: "Test".into(), name: "Arrow".into(), w, h, rgba, hotspot: [0.0, 0.0] }
    }

    #[test]
    fn an_icon_is_found_where_it_was_drawn_and_its_hotspot_with_it() {
        let icon = arrow();
        // A frame: a textured background, the icon drawn with its top-left at (31, 17) (video range, as decoded).
        let (fw, fh) = (80usize, 60usize);
        let mut data: Vec<f32> = (0..fw * fh).map(|i| 90.0 + 40.0 * (((i % fw) as f32 * 0.7).sin() * ((i / fw) as f32 * 0.4).cos())).collect();
        for y in 0..icon.h as usize {
            for x in 0..icon.w as usize {
                let k = (y * icon.w as usize + x) * 4;
                if icon.rgba[k + 3] > 0 {
                    data[(17 + y) * fw + 31 + x] = 16.0 + icon.rgba[k] as f32 * 219.0 / 255.0;
                }
            }
        }
        let frame = crate::image::Patch::new(fw, fh, data);
        let (t, hot) = template_of(&icon, 1.0, crate::ncc::Tolerance::default()).expect("a template");
        let m = crate::ncc::best_match(&frame, &t.template, [[0.0, 0.0], [fw as f64, fh as f64]], None).expect("a match");
        // Its centre where the icon's is, and its hotspot at the icon's tip.
        let centre = [31.0 + icon.w as f64 / 2.0, 17.0 + icon.h as f64 / 2.0];
        assert!((m.pos[0] - centre[0]).abs() < 0.6 && (m.pos[1] - centre[1]).abs() < 0.6, "found at {:?}, drawn at {centre:?}", m.pos);
        assert!(m.score > 0.95, "score {}", m.score);
        let tip = [m.pos[0] + hot[0], m.pos[1] + hot[1]];
        assert!((tip[0] - 31.0).abs() < 0.6 && (tip[1] - 17.0).abs() < 0.6, "tip at {tip:?}");
    }

    #[test]
    fn crops_to_what_shows_and_moves_the_hotspot() {
        let (w, h, rgba, hot) = read_cursor(&cur(), 32).expect("reads");
        let c = crop(Icon { w, h, rgba, hotspot: hot, ..Default::default() });
        // Only the bottom row shows: it and one row above, all four columns.
        assert_eq!((c.w, c.h, c.hotspot), (4, 2, [1.0, 1.0]));
    }
}
