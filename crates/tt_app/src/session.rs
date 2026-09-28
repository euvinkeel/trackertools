//! Session memory between launches: recent files and where you were. A small
//! JSON file in the per-user data dir; the real project file (SQLite
//! `.ttproj`, DESIGN §12) arrives with M2 and will absorb this.

use std::path::PathBuf;

use bevy_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use tt_core::capture::{SketchDefaults, WheelMode};
use tt_core::input::{Action, PendingActions};
use tt_core::sketch::SketchParams;
use tt_core::time::{FrameIndex, WallClock};
use tt_core::transport::Transport;
use tt_core::{AppBuilder, Class, Module, Set};

use crate::media::{Media, OpenRequest};

const MAX_RECENT: usize = 10;

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
    wheel: WheelMode,
    stroke_scale: f32,
    stroke_falloff: f32,
    /// What new sketches start with (a preset, or a sketch's "Use for new sketches").
    new_sketches: SketchParams,
}

impl Default for SettingsFile {
    fn default() -> Self {
        let s = tt_core::sketch::Stroke::default();
        Self { wheel: WheelMode::default(), stroke_scale: s.scale, stroke_falloff: s.falloff, new_sketches: SketchParams::default() }
    }
}

impl SettingsFile {
    fn of(d: &SketchDefaults) -> Self {
        Self { wheel: d.wheel, stroke_scale: d.stroke.scale, stroke_falloff: d.stroke.falloff, new_sketches: d.params.clone() }
    }

    fn apply(&self, d: &mut SketchDefaults) {
        d.wheel = self.wheel;
        d.stroke.scale = self.stroke_scale.clamp(tt_core::capture::SCALE_RANGE.0, tt_core::capture::SCALE_RANGE.1);
        d.stroke.falloff = self.stroke_falloff.clamp(0.0, 5.0);
        d.params = self.new_sketches.clone();
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

fn track_session(
    media: Option<Res<Media>>,
    transport: Res<Transport>,
    clock: Res<WallClock>,
    defaults: Res<SketchDefaults>,
    mut session: ResMut<Session>,
    mut actions: ResMut<PendingActions>,
) {
    let settings = SettingsFile::of(&defaults);
    if session.file.settings != settings && !session.scripted {
        session.file.settings = settings;
    }
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
            session.file.settings.apply(&mut app.world_mut().resource_mut::<SketchDefaults>());
        }
        app.declare::<Session>(Class::Session).insert_resource(session).add_systems(track_session.in_set(Set::Prepare));
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
        let text = serde_json::to_string(&SettingsFile::of(&chosen)).unwrap();
        let mut restored = SketchDefaults::default();
        serde_json::from_str::<SettingsFile>(&text).unwrap().apply(&mut restored);
        assert_eq!(restored.params, chosen.params);
    }
}
