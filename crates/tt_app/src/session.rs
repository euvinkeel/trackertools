//! Session memory between launches: recent files and where you were. A small
//! JSON file in the per-user data dir; the real project file (SQLite
//! `.ttproj`, DESIGN §12) arrives with M2 and will absorb this.

use std::path::PathBuf;

use bevy_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use tt_core::capture::{SketchDefaults, WheelMode};
use tt_core::input::{Action, PendingActions};
use tt_core::time::{FrameIndex, WallClock};
use tt_core::transport::Transport;
use tt_core::view::ViewDefaults;
use tt_core::{AppBuilder, Class, Module, Set};

use crate::media::{Media, OpenRequest};
use crate::panels::viewport::PointerView;

const MAX_RECENT: usize = 10;

#[derive(Serialize, Deserialize, Default, Clone, PartialEq)]
struct SessionFile {
    recent: Vec<PathBuf>,
    /// Frame of the most recent file when last seen.
    frame: FrameIndex,
    #[serde(default)]
    settings: SettingsFile,
}

/// The user's settings (the Settings tab), remembered between launches.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
struct SettingsFile {
    wheel: WheelMode,
    stroke_scale: f32,
    stroke_falloff: f32,
    /// New views keep a steady zoom (`FrameParams::lock_zoom`).
    view_lock_zoom: bool,
    /// While holding a stroke: hide the pointer; the clear window's radius (pt, 0 = off).
    hide_pointer: bool,
    clear_radius: f32,
}

impl Default for SettingsFile {
    fn default() -> Self {
        let s = tt_core::sketch::Stroke::default();
        let (v, p) = (ViewDefaults::default(), PointerView::default());
        Self {
            wheel: WheelMode::default(),
            stroke_scale: s.scale,
            stroke_falloff: s.falloff,
            view_lock_zoom: v.params.lock_zoom,
            hide_pointer: p.hide_pointer,
            clear_radius: p.clear_radius,
        }
    }
}

impl SettingsFile {
    fn of(d: &SketchDefaults, v: &ViewDefaults, p: &PointerView) -> Self {
        Self {
            wheel: d.wheel,
            stroke_scale: d.stroke.scale,
            stroke_falloff: d.stroke.falloff,
            view_lock_zoom: v.params.lock_zoom,
            hide_pointer: p.hide_pointer,
            clear_radius: p.clear_radius,
        }
    }

    fn apply(&self, world: &mut World) {
        let mut d = world.resource_mut::<SketchDefaults>();
        d.wheel = self.wheel;
        d.stroke.scale = self.stroke_scale.clamp(tt_core::capture::SCALE_RANGE.0, tt_core::capture::SCALE_RANGE.1);
        d.stroke.falloff = self.stroke_falloff.clamp(0.0, 5.0);
        world.resource_mut::<ViewDefaults>().params.lock_zoom = self.view_lock_zoom;
        *world.resource_mut::<PointerView>() = PointerView { hide_pointer: self.hide_pointer, clear_radius: self.clear_radius.clamp(0.0, 200.0) };
    }
}

#[derive(Resource)]
pub struct Session {
    file: SessionFile,
    saved: SessionFile,
    /// Seek to apply once the reopened file is ready.
    pending_seek: Option<FrameIndex>,
    seen_generation: u64,
    last_save: f64,
}

impl Session {
    pub fn recent(&self) -> &[PathBuf] {
        &self.file.recent
    }

    fn path() -> PathBuf {
        tt_media::proxy::data_dir().join("session.json")
    }

    fn load() -> Self {
        let file: SessionFile =
            std::fs::read(Self::path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { saved: file.clone(), file, pending_seek: None, seen_generation: 0, last_save: 0.0 }
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
fn track_settings(defaults: Res<SketchDefaults>, views: Res<ViewDefaults>, pointer: Res<PointerView>, mut session: ResMut<Session>) {
    let settings = SettingsFile::of(&defaults, &views, &pointer);
    if session.file.settings != settings {
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
        session.file.settings.apply(app.world_mut());
        app.declare::<Session>(Class::Session).insert_resource(session).add_systems((track_settings, track_session).chain().in_set(Set::Prepare));
    }
}
