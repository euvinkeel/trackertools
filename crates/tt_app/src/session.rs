//! Session memory between launches: recent files and where you were. A small
//! JSON file in the per-user data dir; the real project file (SQLite
//! `.ttproj`, DESIGN §12) arrives with M2 and will absorb this.

use std::path::{Path, PathBuf};

use bevy_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use tt_core::autospeed::AutoSpeed;
use tt_core::capture::{SketchDefaults, WheelMode};
use tt_core::input::{Action, PendingActions};
use tt_core::sketch::SketchParams;
use tt_core::time::{FrameIndex, WallClock};
use tt_core::transport::Transport;
use tt_core::view::ViewDefaults;
use tt_track::export::StabilizerDefaults;
use tt_track::look::LookDefaults;
use tt_core::{AppBuilder, Class, Module, Set};

use crate::media::{Media, OpenRequest};
use crate::update::Updater;
use crate::panels::viewport::PointerView;

const MAX_RECENT: usize = 10;
/// Project files remembered per video.
const MAX_PROJECTS: usize = 20;
/// The settings' layout version (see [`SettingsFile::apply`]).
const SETTINGS_VERSION: u32 = 4;

#[derive(Serialize, Deserialize, Default, Clone, PartialEq)]
struct SessionFile {
    recent: Vec<PathBuf>,
    /// Frame of the most recent file when last seen.
    frame: FrameIndex,
    /// Each video's project files, the one open last first. A video without
    /// an entry opens its own project (in the data folder).
    #[serde(default)]
    projects: std::collections::BTreeMap<PathBuf, Vec<PathBuf>>,
    /// Read leniently: a bad value (another build's, a hand edit) resets the
    /// settings, not the recent files with them.
    #[serde(default, deserialize_with = "lenient")]
    settings: SettingsFile,
}

fn lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<SettingsFile, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(value).unwrap_or_else(|e| {
        tracing::warn!("session settings unreadable ({e}); using the defaults");
        SettingsFile::default()
    }))
}

/// The user's settings (the Settings tab), remembered between launches.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
struct SettingsFile {
    /// [`SETTINGS_VERSION`] when written; files without it are older.
    #[serde(default)]
    version: u32,
    wheel: WheelMode,
    stroke_scale: f32,
    stroke_falloff: f32,
    /// What new sketches start with (a preset, or a sketch's "Use for new sketches").
    new_sketches: SketchParams,
    /// New views keep a steady zoom (`FrameParams::lock_zoom`), or only pan (`pan_only`).
    view_lock_zoom: bool,
    view_pan_only: bool,
    /// How steadily new views pan: smoothing (s) and dead zone (a fraction of the view).
    view_pan_damping: f32,
    view_dead_zone: f32,
    /// While holding a stroke: hide the pointer; the clear window's radius (pt, 0 = off).
    hide_pointer: bool,
    clear_radius: f32,
    /// The selected box's moving outline; in a view, outside its box dimmed.
    marching_ants: bool,
    dim_outside_view: bool,
    /// Paint trackers' points' paths on the video.
    paint_paths: bool,
    /// Anticipatory speed: on/off and its knobs.
    auto_speed: AutoSpeed,
    /// New tracker looks get their mask painted automatically.
    auto_mask_looks: bool,
    /// The Resolve stabilizer's spring smoothing, seconds.
    stabilize_smooth_position: f32,
    stabilize_smooth_rotation: f32,
    /// Stabilizers undo the rotation too; they hold what they follow in the middle of the picture.
    stabilize_rotation: bool,
    stabilize_centre: bool,
    /// Rendered stabilized videos: zoom just enough to hide the black edges, else
    /// by the zoom; where the picture is moved (a part of its width and height).
    stabilize_fill: bool,
    stabilize_zoom: f32,
    stabilize_offset: [f32; 2],
    /// Look for a new version at start.
    check_for_updates: bool,
    /// A version whose update prompt was answered with Skip.
    skipped_update: Option<String>,
    /// Start CoTracker's engine when a video opens (`cotracker::EarlyStart`).
    cotracker_start_early: bool,
}

impl Default for SettingsFile {
    fn default() -> Self {
        let s = tt_core::sketch::Stroke::default();
        let (v, p, st) = (ViewDefaults::default(), PointerView::default(), StabilizerDefaults::default());
        Self {
            version: SETTINGS_VERSION,
            wheel: WheelMode::default(),
            stroke_scale: s.scale,
            stroke_falloff: s.falloff,
            new_sketches: SketchParams::default(),
            view_lock_zoom: v.params.lock_zoom,
            view_pan_only: v.params.pan_only,
            view_pan_damping: v.params.pan_damping,
            view_dead_zone: v.params.dead_zone,
            hide_pointer: p.hide_pointer,
            clear_radius: p.clear_radius,
            marching_ants: p.ants,
            dim_outside_view: p.dim_outside,
            paint_paths: p.paint_paths,
            auto_speed: AutoSpeed::default(),
            auto_mask_looks: LookDefaults::default().auto_mask,
            stabilize_smooth_position: st.smooth_position,
            stabilize_smooth_rotation: st.smooth_rotation,
            stabilize_rotation: st.rotation,
            stabilize_centre: st.centre,
            stabilize_fill: st.fill,
            stabilize_zoom: st.zoom,
            stabilize_offset: st.offset,
            check_for_updates: Updater::default().check_on_start,
            skipped_update: None,
            cotracker_start_early: true,
        }
    }
}

impl SettingsFile {
    #[allow(clippy::too_many_arguments)]
    fn of(d: &SketchDefaults, v: &ViewDefaults, p: &PointerView, a: &AutoSpeed, l: &LookDefaults, st: &StabilizerDefaults, u: &Updater, early: bool) -> Self {
        Self {
            version: SETTINGS_VERSION,
            wheel: d.wheel,
            stroke_scale: d.stroke.scale,
            stroke_falloff: d.stroke.falloff,
            new_sketches: d.params.clone(),
            view_lock_zoom: v.params.lock_zoom,
            view_pan_only: v.params.pan_only,
            view_pan_damping: v.params.pan_damping,
            view_dead_zone: v.params.dead_zone,
            hide_pointer: p.hide_pointer,
            clear_radius: p.clear_radius,
            marching_ants: p.ants,
            dim_outside_view: p.dim_outside,
            paint_paths: p.paint_paths,
            auto_speed: a.clone(),
            auto_mask_looks: l.auto_mask,
            stabilize_smooth_position: st.smooth_position,
            stabilize_smooth_rotation: st.smooth_rotation,
            stabilize_rotation: st.rotation,
            stabilize_centre: st.centre,
            stabilize_fill: st.fill,
            stabilize_zoom: st.zoom,
            stabilize_offset: st.offset,
            check_for_updates: u.check_on_start,
            skipped_update: u.skipped.clone(),
            cotracker_start_early: early,
        }
    }

    fn apply(&self, world: &mut World) {
        let mut d = world.resource_mut::<SketchDefaults>();
        d.stroke.scale = self.stroke_scale.clamp(tt_core::capture::SCALE_RANGE.0, tt_core::capture::SCALE_RANGE.1);
        // Version 2 made holds retakes (no falloff) and gave the wheel back to
        // zooming: older files keep their size but take the new wheel and falloff.
        if self.version >= 2 {
            d.wheel = self.wheel;
            d.stroke.falloff = self.stroke_falloff.clamp(0.0, 5.0);
        }
        // Version 4 made the wheel hold still while holding a stroke: the old
        // default (zoom) takes the new one; a wheel you chose stays.
        if self.version < 4 && d.wheel == WheelMode::Zoom {
            d.wheel = WheelMode::Still;
        }
        d.params = self.new_sketches.clone();
        {
            let mut v = world.resource_mut::<ViewDefaults>();
            v.params.lock_zoom = self.view_lock_zoom;
            v.params.pan_only = self.view_pan_only;
            v.params.pan_damping = self.view_pan_damping.clamp(0.0, 5.0);
            v.params.dead_zone = self.view_dead_zone.clamp(0.0, 0.5);
        }
        *world.resource_mut::<PointerView>() = PointerView {
            hide_pointer: self.hide_pointer,
            clear_radius: self.clear_radius.clamp(0.0, 200.0),
            ants: self.marching_ants,
            dim_outside: self.dim_outside_view,
            paint_paths: self.paint_paths,
        };
        // Version 3 turned anticipatory speed on by default.
        *world.resource_mut::<AutoSpeed>() = AutoSpeed { enabled: self.auto_speed.enabled || self.version < 3, ..self.auto_speed.clone() };
        if let Some(mut l) = world.get_resource_mut::<LookDefaults>() {
            l.auto_mask = self.auto_mask_looks;
        }
        if let Some(mut e) = world.get_resource_mut::<crate::cotracker::EarlyStart>() {
            e.enabled = self.cotracker_start_early;
        }
        if let Some(mut u) = world.get_resource_mut::<Updater>() {
            u.check_on_start = self.check_for_updates;
            u.skipped = self.skipped_update.clone();
        }
        if let Some(mut st) = world.get_resource_mut::<StabilizerDefaults>() {
            st.smooth_position = self.stabilize_smooth_position.clamp(0.0, 2.0);
            st.smooth_rotation = self.stabilize_smooth_rotation.clamp(0.0, 2.0);
            st.rotation = self.stabilize_rotation;
            st.centre = self.stabilize_centre;
            st.fill = self.stabilize_fill;
            st.zoom = if self.stabilize_zoom.is_finite() { self.stabilize_zoom.clamp(1.0, 4.0) } else { 1.0 };
            st.offset = self.stabilize_offset.map(|v| if v.is_finite() { v.clamp(-0.5, 0.5) } else { 0.0 });
        }
    }
}

/// Scripted runs (the sketch demo, the step benchmark) start from the
/// built-in settings and leave the user's alone.
fn scripted() -> bool {
    ["TT_SKETCH_DEMO", "TT_BENCH_STEPS", "TT_SCENE_DEMO"].iter().any(|v| std::env::var_os(v).is_some())
}

#[derive(Resource)]
pub struct Session {
    file: SessionFile,
    saved: SessionFile,
    /// Seek to apply once the reopened file is ready.
    pending_seek: Option<FrameIndex>,
    seen_generation: u64,
    last_save: f64,
    /// Settings are neither applied nor saved ([`scripted`]).
    scripted: bool,
}

impl Session {
    pub fn recent(&self) -> &[PathBuf] {
        &self.file.recent
    }

    /// The project files used with `video`, the one open last first.
    pub fn projects_of(&self, video: &Path) -> &[PathBuf] {
        self.file.projects.get(video).map_or(&[], Vec::as_slice)
    }

    /// `project` is the one open on `video` now (first in its list).
    pub fn note_project(&mut self, video: &Path, project: &Path) {
        let list = self.file.projects.entry(video.to_path_buf()).or_default();
        list.retain(|p| p != project);
        list.insert(0, project.to_path_buf());
        list.truncate(MAX_PROJECTS);
        self.save();
    }

    /// Take `project` off `video`'s list (the file is gone).
    pub fn forget_project(&mut self, video: &Path, project: &Path) {
        if let Some(list) = self.file.projects.get_mut(video) {
            list.retain(|p| p != project);
        }
        self.save();
    }

    fn path() -> PathBuf {
        tt_media::proxy::data_dir().join("session.json")
    }

    fn load() -> Self {
        let file: SessionFile = std::fs::read(Self::path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).map_err(|e| tracing::warn!("session file unreadable ({e}); starting afresh")).ok())
            .unwrap_or_default();
        Self { saved: file.clone(), file, pending_seek: None, seen_generation: 0, last_save: 0.0, scripted: scripted() }
    }

    pub fn save(&mut self) {
        if self.file == self.saved {
            return;
        }
        let path = Self::path();
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(path.parent().unwrap())?;
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(&self.file)?)?;
            std::fs::rename(tmp, &path)
        };
        match write() {
            Ok(()) => self.saved = self.file.clone(),
            Err(e) => tracing::warn!("could not save session: {e}"),
        }
    }

    /// Save now, to resume at `frame` (the one shown, even while playing):
    /// trackertools starts again after an error ([`crate::recover`]).
    pub fn save_at(&mut self, frame: FrameIndex) {
        self.file.frame = frame;
        self.save();
    }

    /// Reopen the most recent file at its last frame (startup without a file argument).
    pub fn restore_into(&mut self, request: &mut OpenRequest) {
        if let Some(last) = self.file.recent.first().filter(|p| p.exists()) {
            tracing::info!("resuming {} at frame {}", last.display(), self.file.frame);
            request.0 = Some(last.clone());
            self.pending_seek = Some(self.file.frame);
        }
    }
}

/// The Settings tab's values, kept in the session file.
#[allow(clippy::too_many_arguments)]
fn track_settings(
    defaults: Res<SketchDefaults>,
    views: Res<ViewDefaults>,
    pointer: Res<PointerView>,
    auto: Res<AutoSpeed>,
    looks: Res<LookDefaults>,
    stabilizer: Res<StabilizerDefaults>,
    updater: Res<Updater>,
    early: Option<Res<crate::cotracker::EarlyStart>>,
    mut session: ResMut<Session>,
) {
    let settings = SettingsFile::of(&defaults, &views, &pointer, &auto, &looks, &stabilizer, &updater, early.is_none_or(|e| e.enabled));
    if session.file.settings != settings && !session.scripted {
        session.file.settings = settings;
    }
}

fn track_session(
    media: Option<Res<Media>>,
    transport: Res<Transport>,
    clock: Res<WallClock>,
    mut session: ResMut<Session>,
    mut actions: ResMut<PendingActions>,
) {
    let Some(media) = media else {
        if clock.now - session.last_save > 2.0 {
            session.last_save = clock.now;
            session.save();
        }
        return;
    };
    if media.generation != session.seen_generation {
        // A file was just opened: it becomes the most recent one.
        session.seen_generation = media.generation;
        let path = media.index().path.clone();
        session.file.recent.retain(|p| p != &path);
        session.file.recent.insert(0, path);
        session.file.recent.truncate(MAX_RECENT);
        match session.pending_seek.take() {
            Some(f) if f > 0 => actions.push(Action::Seek(f)),
            _ => session.file.frame = 0,
        }
        session.save();
        return;
    }
    if !transport.playing {
        session.file.frame = transport.frame();
    }
    if clock.now - session.last_save > 2.0 {
        session.last_save = clock.now;
        session.save();
    }
}

pub struct SessionModule;

impl Module for SessionModule {
    fn build(&self, app: &mut AppBuilder) {
        let session = Session::load();
        // The remembered settings replace the built-in defaults.
        if !session.scripted {
            session.file.settings.apply(app.world_mut());
        }
        app.declare::<Session>(Class::Session).insert_resource(session).add_systems((track_settings, track_session).chain().in_set(Set::Prepare));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bad_setting_resets_the_settings_but_keeps_the_recent_files() {
        let text = r#"{"recent": ["C:/clips/a.mp4"], "frame": 12, "settings": {"wheel": "Sideways", "stroke_scale": 2.0}}"#;
        let file: SessionFile = serde_json::from_str(text).expect("the file still reads");
        assert_eq!(file.recent, vec![PathBuf::from("C:/clips/a.mp4")]);
        assert_eq!(file.frame, 12);
        assert_eq!(file.settings, SettingsFile::default());
    }

    #[test]
    fn the_preset_new_sketches_use_is_remembered() {
        let chosen = SketchDefaults { params: SketchParams::preset("Loose").unwrap(), ..SketchDefaults::default() };
        let text = serde_json::to_string(&SettingsFile::of(&chosen, &ViewDefaults::default(), &PointerView::default(), &AutoSpeed::default(), &LookDefaults::default(), &StabilizerDefaults::default(), &Updater::default(), true)).unwrap();
        let mut world = World::new();
        world.init_resource::<SketchDefaults>();
        world.init_resource::<ViewDefaults>();
        world.init_resource::<PointerView>();
        world.init_resource::<AutoSpeed>();
        serde_json::from_str::<SettingsFile>(&text).unwrap().apply(&mut world);
        assert_eq!(world.resource::<SketchDefaults>().params, chosen.params);
    }

    #[test]
    fn auto_speed_and_its_knobs_are_remembered() {
        let knobs = AutoSpeed { enabled: true, comfort: 450.0, look_ahead: 0.0, ..AutoSpeed::default() };
        let text = serde_json::to_string(&SettingsFile::of(&SketchDefaults::default(), &ViewDefaults::default(), &PointerView::default(), &knobs, &LookDefaults::default(), &StabilizerDefaults::default(), &Updater::default(), true)).unwrap();
        let mut world = World::new();
        world.init_resource::<SketchDefaults>();
        world.init_resource::<ViewDefaults>();
        world.init_resource::<PointerView>();
        world.init_resource::<AutoSpeed>();
        serde_json::from_str::<SettingsFile>(&text).unwrap().apply(&mut world);
        assert_eq!(*world.resource::<AutoSpeed>(), knobs);
        // A session file from before auto speed reads with it off.
        let old: SettingsFile = serde_json::from_str(r#"{"wheel": "Size", "stroke_scale": 1.0}"#).unwrap();
        assert_eq!(old.auto_speed, AutoSpeed::default());
    }

    #[test]
    fn the_stabilizers_smoothing_is_remembered() {
        let chosen = StabilizerDefaults { smooth_position: 0.2, smooth_rotation: 0.4, rotation: false, centre: false, fill: true, zoom: 1.75, offset: [0.125, -0.25] };
        let text = serde_json::to_string(&SettingsFile::of(
            &SketchDefaults::default(),
            &ViewDefaults::default(),
            &PointerView::default(),
            &AutoSpeed::default(),
            &LookDefaults::default(),
            &chosen,
            &Updater::default(),
            true,
        ))
        .unwrap();
        let mut world = World::new();
        world.init_resource::<SketchDefaults>();
        world.init_resource::<ViewDefaults>();
        world.init_resource::<PointerView>();
        world.init_resource::<AutoSpeed>();
        world.init_resource::<StabilizerDefaults>();
        serde_json::from_str::<SettingsFile>(&text).unwrap().apply(&mut world);
        assert_eq!(*world.resource::<StabilizerDefaults>(), chosen);
        // A session file from before it reads with the defaults.
        let old: SettingsFile = serde_json::from_str(r#"{"wheel": "Size", "stroke_scale": 1.0}"#).unwrap();
        assert_eq!((old.stabilize_smooth_position, old.stabilize_smooth_rotation, old.stabilize_rotation, old.stabilize_centre), (0.0, 0.05, true, true));
        // From before the zoom and position: the whole picture, where it is.
        assert_eq!((old.stabilize_fill, old.stabilize_zoom, old.stabilize_offset), (false, 1.0, [0.0, 0.0]));
        // Values out of range (a hand edit) are brought back in.
        let wild: SettingsFile = serde_json::from_str(r#"{"stabilize_zoom": 40.0, "stabilize_offset": [3.0, -0.1]}"#).unwrap();
        wild.apply(&mut world);
        let st = world.resource::<StabilizerDefaults>();
        assert_eq!((st.zoom, st.offset), (4.0, [0.5, -0.1]));
    }

    #[test]
    fn settings_from_before_retakes_take_the_new_wheel_and_falloff() {
        let text = r#"{"wheel": "Size", "stroke_scale": 2.0, "stroke_falloff": 0.2}"#;
        let mut world = World::new();
        world.init_resource::<SketchDefaults>();
        world.init_resource::<ViewDefaults>();
        world.init_resource::<PointerView>();
        world.init_resource::<AutoSpeed>();
        serde_json::from_str::<SettingsFile>(text).unwrap().apply(&mut world);
        let d = world.resource::<SketchDefaults>();
        assert_eq!((d.wheel, d.stroke.falloff, d.stroke.scale), (WheelMode::Still, 0.0, 2.0));
    }

    #[test]
    fn the_old_default_wheel_holds_still_and_a_chosen_one_stays() {
        let read = |text: &str| {
            let mut world = World::new();
            world.init_resource::<SketchDefaults>();
            world.init_resource::<ViewDefaults>();
            world.init_resource::<PointerView>();
            world.init_resource::<AutoSpeed>();
            serde_json::from_str::<SettingsFile>(text).unwrap().apply(&mut world);
            world.resource::<SketchDefaults>().wheel
        };
        assert_eq!(read(r#"{"version": 3, "wheel": "Zoom"}"#), WheelMode::Still);
        assert_eq!(read(r#"{"version": 3, "wheel": "Falloff"}"#), WheelMode::Falloff);
        assert_eq!(read(r#"{"version": 4, "wheel": "Zoom"}"#), WheelMode::Zoom);
    }
}
