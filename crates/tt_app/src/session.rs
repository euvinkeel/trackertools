//! Session memory between launches: recent files and where you were. A small
//! JSON file in the per-user data dir; the real project file (SQLite
//! `.ttproj`, DESIGN §12) arrives with M2 and will absorb this.

use std::path::PathBuf;

use bevy_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use tt_core::input::{Action, PendingActions};
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
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        base.join("trackertools").join("session.json")
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

fn track_session(
    media: Option<Res<Media>>,
    transport: Res<Transport>,
    clock: Res<WallClock>,
    mut session: ResMut<Session>,
    mut actions: ResMut<PendingActions>,
) {
    let Some(media) = media else { return };
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
        app.declare::<Session>(Class::Session)
            .insert_resource(Session::load())
            .add_systems(track_session.in_set(Set::Prepare));
    }
}
