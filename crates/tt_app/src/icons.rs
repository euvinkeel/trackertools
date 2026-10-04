//! Icons drawn as shapes, never as font symbols (a symbol the font lacks
//! shows as an empty box): what each kind of thing is, in the visual
//! language's hues (`style`), the same in the outliner, the timeline, the
//! inspector and the viewport; and what a tracker is doing (a spinner while
//! it starts or tracks).

use bevy_ecs::prelude::*;
use egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, Vec2};
use tt_track::Method;

use crate::style;

/// What a thing is, as an icon shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph {
    Sketch,
    Stroke,
    View,
    Template,
    CoTracker,
    Manual,
    Look,
    ResetPoint,
    Subject,
    Other,
}

impl Glyph {
    pub fn of(world: &World, e: Entity) -> Glyph {
        if let Some(op) = world.get::<tt_core::op::Operator>(e) {
            return match op.kind.as_str() {
                "sketch" => Glyph::Sketch,
                "frame" => Glyph::View,
                "subject" => Glyph::Subject,
                "track" => Glyph::tracker(world.get::<tt_track::Tracker>(e).map_or(Method::Template, |t| t.method)),
                _ => Glyph::Other,
            };
        }
        if world.get::<tt_core::sketch::Capture>(e).is_some() {
            return Glyph::Stroke;
        }
        if world.get::<tt_track::look::Look>(e).is_some() {
            let method = tt_track::look::owner_of(world, e).and_then(|t| world.get::<tt_track::Tracker>(t)).map(|t| t.method);
            return if method == Some(Method::CoTracker) { Glyph::ResetPoint } else { Glyph::Look };
        }
        Glyph::Other
    }

    pub fn tracker(method: Method) -> Glyph {
        match method {
            Method::Template => Glyph::Template,
            Method::CoTracker => Glyph::CoTracker,
            Method::Manual => Glyph::Manual,
        }
    }

    pub fn color(self) -> Color32 {
        match self {
            Glyph::Sketch | Glyph::Stroke | Glyph::Manual => style::HAND,
            Glyph::View => style::VIEW,
            Glyph::Template | Glyph::CoTracker => style::AUTO,
            Glyph::Look | Glyph::ResetPoint => style::PIN,
            Glyph::Subject => style::SUBJECT,
            Glyph::Other => style::MUTED,
        }
    }

    /// What it is, in words (tooltips).
    pub fn name(self) -> &'static str {
        match self {
            Glyph::Sketch => "sketch: a box you drew over time",
            Glyph::Stroke => "stroke: one press of the Sketch tool",
            Glyph::View => "view: a sketch's framing",
            Glyph::Template => "template tracker: matches the patterns you showed it",
            Glyph::CoTracker => "CoTracker: follows one pixel with a learned model",
            Glyph::Manual => "manual dot: only what you drew",
            Glyph::Look => "look: a pattern a template tracker matches",
            Glyph::ResetPoint => "reset point: the pixel a CoTracker follows from here on",
            Glyph::Subject => "subject: one position its members carry together",
            Glyph::Other => "item",
        }
    }
}

/// `g` painted in the square `rect`; `strong` (selected) in full colour, else dimmer.
pub fn paint(painter: &Painter, rect: Rect, g: Glyph, strong: bool) {
    let c = g.color().gamma_multiply(if strong { 1.0 } else { 0.75 });
    let s = rect.width().min(rect.height());
    let o = rect.center();
    let stroke = Stroke::new((s / 9.0).clamp(1.0, 2.0), c);
    let r = s * 0.38;
    match g {
        // A box with its point: what a sketch is on every frame.
        Glyph::Sketch => {
            painter.rect_stroke(Rect::from_center_size(o, Vec2::new(2.0 * r, 1.6 * r)), 2.0, stroke, egui::StrokeKind::Middle);
            painter.circle_filled(o, s * 0.08, c);
        }
        // A hand's wiggle.
        Glyph::Stroke => {
            let pts: Vec<Pos2> = (0..=8).map(|i| o + Vec2::new(-r + 2.0 * r * i as f32 / 8.0, 0.35 * r * ((i as f32) * 1.3).sin())).collect();
            painter.add(Shape::line(pts, stroke));
        }
        // A viewfinder: corners only.
        Glyph::View => {
            let b = Rect::from_center_size(o, Vec2::new(2.0 * r, 1.6 * r));
            let t = r * 0.45;
            for (p, dx, dy) in [(b.left_top(), 1.0, 1.0), (b.right_top(), -1.0, 1.0), (b.left_bottom(), 1.0, -1.0), (b.right_bottom(), -1.0, -1.0)] {
                painter.add(Shape::line(vec![p + Vec2::new(dx * t, 0.0), p, p + Vec2::new(0.0, dy * t)], stroke));
            }
        }
        // As a tracker shows on the video: its pattern's box and its point.
        Glyph::Template => {
            painter.rect_stroke(Rect::from_center_size(o, Vec2::splat(1.6 * r)), 1.5, stroke, egui::StrokeKind::Middle);
            crosshair(painter, o, r * 0.55, stroke);
        }
        // A learned point tracker: a ring and its point.
        Glyph::CoTracker => {
            painter.circle_stroke(o, r * 0.85, stroke);
            crosshair(painter, o, r * 0.5, stroke);
        }
        // Drawn by hand: a solid dot.
        Glyph::Manual => {
            painter.circle_filled(o, r * 0.62, c);
            painter.circle_stroke(o, r * 0.62, Stroke::new(1.0, Color32::from_black_alpha(160)));
        }
        Glyph::Look => {
            painter.rect_stroke(Rect::from_center_size(o, Vec2::new(1.5 * r, 1.2 * r)), 0.0, stroke, egui::StrokeKind::Middle);
        }
        Glyph::ResetPoint => diamond(painter, o, r * 0.85, stroke, None),
        // As a subject shows on the video: a diamond with its heading.
        Glyph::Subject => {
            diamond(painter, o, r * 0.75, stroke, None);
            painter.line_segment([o + Vec2::new(r * 0.75, 0.0), o + Vec2::new(r * 1.15, 0.0)], stroke);
        }
        Glyph::Other => {
            painter.circle_stroke(o, r * 0.4, stroke);
        }
    }
}

/// An icon the height of a line of text, as a widget (its tooltip says what it is).
pub fn icon(ui: &mut egui::Ui, g: Glyph, strong: bool) -> egui::Response {
    let h = ui.text_style_height(&egui::TextStyle::Body).max(12.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(h), egui::Sense::hover());
    paint(ui.painter(), rect, g, strong);
    response.on_hover_text(g.name())
}

pub fn crosshair(painter: &Painter, o: Pos2, r: f32, stroke: Stroke) {
    painter.line_segment([o - Vec2::new(r, 0.0), o + Vec2::new(r, 0.0)], stroke);
    painter.line_segment([o - Vec2::new(0.0, r), o + Vec2::new(0.0, r)], stroke);
}

/// A diamond around `o` (a reset point's mark), filled with `fill` if given.
pub fn diamond(painter: &Painter, o: Pos2, r: f32, stroke: Stroke, fill: Option<Color32>) {
    let pts = vec![o + Vec2::new(0.0, -r), o + Vec2::new(r, 0.0), o + Vec2::new(0.0, r), o + Vec2::new(-r, 0.0)];
    match fill {
        Some(f) => {
            painter.add(Shape::convex_polygon(pts, f, stroke));
        }
        None => {
            painter.add(Shape::closed_line(pts, stroke));
        }
    }
}

/// A folding triangle as a button (pointing right, or down when `open`).
pub fn fold_button(ui: &mut egui::Ui, open: bool) -> egui::Response {
    let h = ui.text_style_height(&egui::TextStyle::Body).max(12.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(h, h), egui::Sense::click());
    let color = if response.hovered() { style::TEXT } else { style::MUTED };
    fold(ui.painter(), rect, open, color);
    response
}

/// A small cross as a button (clear, remove).
pub fn cross_button(ui: &mut egui::Ui, tip: &str) -> egui::Response {
    let h = ui.text_style_height(&egui::TextStyle::Body).max(12.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(h, h), egui::Sense::click());
    let c = if response.hovered() { style::TEXT } else { style::MUTED };
    let (o, r) = (rect.center(), h * 0.25);
    let s = Stroke::new(1.5, c);
    ui.painter().line_segment([o - Vec2::splat(r), o + Vec2::splat(r)], s);
    ui.painter().line_segment([o + Vec2::new(-r, r), o + Vec2::new(r, -r)], s);
    response.on_hover_text(tip)
}

/// A folding triangle (pointing right, or down when `open`), for tree rows.
pub fn fold(painter: &Painter, rect: Rect, open: bool, color: Color32) {
    let o = rect.center();
    let r = rect.width().min(rect.height()) * 0.28;
    let pts = if open {
        vec![o + Vec2::new(-r, -r * 0.6), o + Vec2::new(r, -r * 0.6), o + Vec2::new(0.0, r * 0.7)]
    } else {
        vec![o + Vec2::new(-r * 0.6, -r), o + Vec2::new(r * 0.7, 0.0), o + Vec2::new(-r * 0.6, r)]
    };
    painter.add(Shape::convex_polygon(pts, color, Stroke::NONE));
}

/// What a tracker is doing, for its spinner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    /// Its job is starting (decoding the first frames); `true`: CoTracker's
    /// Python worker is loading its model.
    Starting(bool),
    Tracking,
    /// Holding at the playhead (catch-up mode).
    Waiting,
    /// Asked to track, waiting for a free job slot or its inputs.
    Queued,
    Paused,
    /// Nothing to do: tracked as far as it was asked.
    Done,
    Failed,
}

impl Activity {
    pub fn of(world: &World, tracker: Entity) -> Activity {
        if world.get::<tt_core::op::OpError>(tracker).is_some() {
            return Activity::Failed;
        }
        let status = world.get::<tt_track::TrackStatus>(tracker);
        if let Some(s) = status {
            if let Some(model) = s.starting() {
                return Activity::Starting(model);
            }
            if s.busy() {
                let sides = [s.forward, s.backward];
                return if sides.iter().flatten().all(|x| x.waiting) { Activity::Waiting } else { Activity::Tracking };
            }
            if s.queued {
                return Activity::Queued;
            }
        }
        if tt_track::run_of(world, tracker) == tt_track::TrackRun::Paused { Activity::Paused } else { Activity::Done }
    }

    /// Something is happening (the panel keeps repainting).
    pub fn moving(self) -> bool {
        matches!(self, Activity::Starting(_) | Activity::Tracking | Activity::Queued)
    }

    /// In a few words.
    pub fn words(self) -> &'static str {
        match self {
            Activity::Starting(true) => "starting CoTracker: loading its model",
            Activity::Starting(false) => "starting",
            Activity::Tracking => "tracking",
            Activity::Waiting => "waiting for the playhead",
            Activity::Queued => "queued: waiting for a free slot",
            Activity::Paused => "paused",
            Activity::Done => "done",
            Activity::Failed => "stopped by an error",
        }
    }

    pub fn color(self) -> Color32 {
        match self {
            Activity::Starting(_) => style::LIVE,
            Activity::Tracking => style::AUTO,
            Activity::Waiting | Activity::Queued | Activity::Paused => style::MUTED,
            Activity::Done => style::AUTO,
            Activity::Failed => style::LOST,
        }
    }
}

/// The activity as a small animated mark centred on `o` (radius `r`): a
/// turning arc while it starts or tracks (amber while CoTracker loads), a
/// slow pulse while it waits, two bars when paused, a dot when done, a red
/// dot with a bar when it failed. `t`: seconds (the animation's clock).
pub fn activity(painter: &Painter, o: Pos2, r: f32, a: Activity, t: f64) {
    let c = a.color();
    match a {
        Activity::Starting(_) | Activity::Tracking | Activity::Queued => {
            // A ring with a turning arc (slower while queued).
            let speed = if a == Activity::Queued { 1.5 } else { 5.0 };
            painter.circle_stroke(o, r, Stroke::new(1.0, c.gamma_multiply(0.25)));
            let start = (t * speed) as f32;
            let pts: Vec<Pos2> = (0..=16).map(|i| start + i as f32 / 16.0 * 4.2).map(|ang| o + r * Vec2::new(ang.cos(), ang.sin())).collect();
            painter.add(Shape::line(pts, Stroke::new(1.8, c)));
        }
        Activity::Waiting => {
            let pulse = 0.55 + 0.45 * (t * 3.0).sin().abs() as f32;
            painter.circle_filled(o, r * 0.55, c.gamma_multiply(pulse));
        }
        Activity::Paused => {
            for dx in [-0.35, 0.35] {
                let x = o.x + dx * r;
                painter.line_segment([Pos2::new(x, o.y - r * 0.6), Pos2::new(x, o.y + r * 0.6)], Stroke::new(1.6, c));
            }
        }
        Activity::Done => {
            painter.circle_filled(o, r * 0.4, c.gamma_multiply(0.8));
        }
        Activity::Failed => {
            painter.circle_filled(o, r * 0.85, c);
            painter.line_segment([o - Vec2::new(0.0, r * 0.45), o + Vec2::new(0.0, r * 0.1)], Stroke::new(1.5, Color32::WHITE));
            painter.circle_filled(o + Vec2::new(0.0, r * 0.42), 0.9, Color32::WHITE);
        }
    }
}

#[cfg(test)]
mod tests {
    /// Every string literal in the crates' sources (escapes `\u{..}` decoded;
    /// comments, char literals and lifetimes skipped), with where it is.
    fn literals(src: &str) -> Vec<(usize, String)> {
        let b: Vec<char> = src.chars().collect();
        let (mut i, mut line, mut out) = (0, 1, Vec::new());
        let at = |i: usize, s: &str| s.chars().enumerate().all(|(k, c)| b.get(i + k) == Some(&c));
        while i < b.len() {
            let c = b[i];
            if c == '\n' {
                line += 1;
                i += 1;
            } else if at(i, "//") {
                while i < b.len() && b[i] != '\n' {
                    i += 1;
                }
            } else if at(i, "/*") {
                while i < b.len() && !at(i, "*/") {
                    line += usize::from(b[i] == '\n');
                    i += 1;
                }
                i += 2;
            } else if c == '\'' {
                // A char literal ('x', '\n', '\u{..}', '"'), else a lifetime.
                let close = (i + 1..(i + 12).min(b.len())).find(|&j| b[j] == '\'' && (j > i + 2 || b[i + 1] != '\\'));
                i = match close {
                    Some(j) if j - i <= 11 && (b[i + 1] == '\\' || j == i + 2) => j + 1,
                    _ => i + 1,
                };
            } else if (c == 'r' && matches!(b.get(i + 1), Some('"' | '#'))) && (i == 0 || !(b[i - 1].is_alphanumeric() || b[i - 1] == '_')) {
                let mut j = i + 1;
                let mut hashes = String::new();
                while b.get(j) == Some(&'#') {
                    hashes.push('#');
                    j += 1;
                }
                if b.get(j) != Some(&'"') {
                    i += 1;
                    continue;
                }
                let end = format!("\"{hashes}");
                let start = j + 1;
                let mut k = start;
                while k < b.len() && !at(k, &end) {
                    k += 1;
                }
                let text: String = b[start..k.min(b.len())].iter().collect();
                out.push((line, text.clone()));
                line += text.matches('\n').count();
                i = k + end.chars().count();
            } else if c == '"' {
                let mut k = i + 1;
                let mut text = String::new();
                while k < b.len() && b[k] != '"' {
                    if b[k] == '\\' {
                        if b.get(k + 1) == Some(&'u') && b.get(k + 2) == Some(&'{') {
                            let close = (k + 3..b.len()).find(|&j| b[j] == '}').unwrap_or(k + 3);
                            let hex: String = b[k + 3..close].iter().collect();
                            text.extend(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32));
                            k = close + 1;
                        } else {
                            k += 2;
                        }
                        continue;
                    }
                    line += usize::from(b[k] == '\n');
                    text.push(b[k]);
                    k += 1;
                }
                out.push((line, text));
                i = k + 1;
            } else {
                i += 1;
            }
        }
        out
    }

    /// No string in the app uses a symbol egui's fonts can't draw (it shows as
    /// an empty box): every character outside ASCII in every literal, against
    /// the proportional family's fonts (the text font; the monospace family
    /// falls back to the same ones). Read from the fonts' own character maps:
    /// egui's `has_glyph` answers no for characters its replacement glyph's
    /// font has (NotoEmoji's ✔ and ▶, which draw fine).
    #[test]
    fn every_symbol_in_the_apps_text_can_be_drawn() {
        let defs = egui::FontDefinitions::default();
        let names = defs.families.get(&egui::FontFamily::Proportional).cloned().unwrap_or_default();
        let faces: Vec<ttf_parser::Face<'_>> = names.iter().filter_map(|n| defs.font_data.get(n)).map(|d| ttf_parser::Face::parse(&d.font, d.index).expect("a font")).collect();
        assert!(faces.len() >= 3, "egui's text font and its two emoji fallbacks");
        let drawable = |c: char| faces.iter().any(|f| f.glyph_index(c).is_some());
        assert!(drawable('\u{2714}') && drawable('\u{25b6}') && !drawable('\u{2190}'), "the check itself");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut missing: Vec<String> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut dirs = vec![root.clone()];
        while let Some(d) = dirs.pop() {
            for e in std::fs::read_dir(&d).expect("readable").flatten() {
                let p = e.path();
                if p.is_dir() {
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    if !matches!(name.as_str(), "target" | "tests" | "examples" | "benches") {
                        dirs.push(p);
                    }
                } else if p.extension().is_some_and(|x| x == "rs") && p.components().any(|c| c.as_os_str() == "src") {
                    let src = std::fs::read_to_string(&p).expect("utf-8");
                    for (line, text) in literals(&src) {
                        for c in text.chars().filter(|c| !c.is_ascii()) {
                            seen.insert(c);
                            if !drawable(c) {
                                missing.push(format!("U+{:04X} {c:?} in {}:{line}", c as u32, p.strip_prefix(&root).unwrap_or(&p).display()));
                            }
                        }
                    }
                }
            }
        }
        assert!(seen.contains(&'\u{b7}'), "the scan sees the app's text");
        assert!(missing.is_empty(), "symbols the fonts can't draw:\n{}", missing.join("\n"));
    }
}
