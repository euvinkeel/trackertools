//! Session memory between launches: recent files and where you were. A small
//! JSON file in the per-user data dir; the real project file (SQLite
//! `.ttproj`, DESIGN §12) arrives with M2 and will absorb this.

use std::path::PathBuf;

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
/// The settings' layout version (see [`SettingsFile::apply`]).
const SETTINGS_VERSION: u32 = 4;

#[derive(Serialize, Deserialize, Default, Clone, PartialEq)]
struct SessionFile {
    recent: Vec<PathBuf>,
    /// Frame of the most recent file when last seen.
    frame: FrameIndex,
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
    /// While holding a stroke: hide the pointer; the clear window's radius (pt, 0 = off).
    hide_pointer: bool,
    clear_radius: f32,
    /// Anticipatory speed: on/off and its knobs.
    auto_speed: AutoSpeed,
    /// New tracker looks get their mask painted automatically.
    auto_mask_looks: bool,
    /// The Resolve stabilizer's spring smoothing, seconds.
    stabilize_smooth_position: f32,
    stabilize_smooth_rotation: f32,
    /// Look for a new version at start.
    check_for_updates: bool,
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
            hide_pointer: p.hide_pointer,
            clear_radius: p.clear_radius,
            auto_speed: AutoSpeed::default(),
            auto_mask_looks: LookDefaults::default().auto_mask,
            stabilize_smooth_position: st.smooth_position,
            stabilize_smooth_rotation: st.smooth_rotation,
            check_for_updates: Updater::default().check_on_start,
        }
    }
}

impl SettingsFile {
    fn of(d: &SketchDefaults, v: &ViewDefaults, p: &PointerView, a: &AutoSpeed, l: &LookDefaults, st: &StabilizerDefaults, u: &Updater) -> Self {
        Self {
            version: SETTINGS_VERSION,
            wheel: d.wheel,
            stroke_scale: d.stroke.scale,
            stroke_falloff: d.stroke.falloff,
            new_sketches: d.params.clone(),
            view_lock_zoom: v.params.lock_zoom,
            view_pan_only: v.params.pan_only,
            hide_pointer: p.hide_pointer,
            clear_radius: p.clear_radius,
            auto_speed: a.clone(),
            auto_mask_looks: l.auto_mask,
            stabilize_smooth_position: st.smooth_position,
            stabilize_smooth_rotation: st.smooth_rotation,
            check_for_updates: u.check_on_start,
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
        }
        *world.resource_mut::<PointerView>() = PointerView { hide_pointer: self.hide_pointer, clear_radius: self.clear_radius.clamp(0.0, 200.0) };
        // Version 3 turned anticipatory speed on by default.
        *world.resource_mut::<AutoSpeed>() = AutoSpeed { enabled: self.auto_speed.enabled || self.version < 3, ..self.auto_speed.clone() };
        if let Some(mut l) = world.get_resource_mut::<LookDefaults>() {
            l.auto_mask = self.auto_mask_looks;
        }
        if let Some(mut u) = world.get_resource_mut::<Updater>() {
            u.check_on_start = self.check_for_updates;
        }
        if let Some(mut st) = world.get_resource_mut::<StabilizerDefaults>() {
            st.smooth_position = self.stabilize_smooth_position.clamp(0.0, 2.0);
            st.smooth_rotation = self.stabilize_smooth_rotation.clamp(0.0, 2.0);
        }
    }
}

/// Scripted runs (the sketch demo, the step benchmark) start from the
/// built-in settings and leave the user's alone.
fn scripted() -> bool {
    ["TT_SKETCH_DEMO", "TT_BENCH_STEPS"].iter().any(|v| std::env::var_os(v).is_some())
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
    mut session: ResMut<Session>,
) {
    let settings = SettingsFile::of(&defaults, &views, &pointer, &auto, &looks, &stabilizer, &updater);
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
        let text = serde_json::to_string(&SettingsFile::of(&chosen, &ViewDefaults::default(), &PointerView::default(), &AutoSpeed::default(), &LookDefaults::default(), &StabilizerDefaults::default(), &Updater::default())).unwrap();
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
        let text = serde_json::to_string(&SettingsFile::of(&SketchDefaults::default(), &ViewDefaults::default(), &PointerView::default(), &knobs, &LookDefaults::default(), &StabilizerDefaults::default(), &Updater::default())).unwrap();
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
        let chosen = StabilizerDefaults { smooth_position: 0.2, smooth_rotation: 0.4 };
        let text = serde_json::to_string(&SettingsFile::of(
            &SketchDefaults::default(),
            &ViewDefaults::default(),
            &PointerView::default(),
            &AutoSpeed::default(),
            &LookDefaults::default(),
            &chosen,
            &Updater::default(),
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
        assert_eq!((old.stabilize_smooth_position, old.stabilize_smooth_rotation), (0.0, 0.05));
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
