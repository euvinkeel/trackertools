//! The project of the open video (DESIGN §12, one video per project): a
//! `.ttproj` in the per-user data dir, keyed like the video's proxy. It loads
//! when the video opens, autosaves shortly after edits settle, and saves
//! before another video opens and on exit. ("Save As…" to a chosen place and
//! opening `.ttproj` files directly come later.)

use std::path::PathBuf;

use bevy_ecs::prelude::*;
use tt_core::history::History;
use tt_core::persist::{self, ProjectMeta};
use tt_core::time::WallClock;
use tt_core::{AppBuilder, Class, Module, Set};

use crate::media::Media;

/// Seconds without further edits before an autosave.
const AUTOSAVE_AFTER: f64 = 1.5;

#[derive(Resource, Default)]
pub struct ProjectFile {
    pub path: Option<PathBuf>,
    saved_revision: u64,
    seen_generation: u64,
    /// (revision, wall time) of the last change seen, for the debounce.
    last_change: (u64, f64),
}

impl ProjectFile {
    pub fn dirty(&self, history: &History) -> bool {
        self.path.is_some() && history.revision() != self.saved_revision
    }
}

/// Save now if there are unsaved changes (autosave, video switch, exit).
pub fn save_if_dirty(world: &mut World) {
    let (path, dirty) = {
        let pf = world.resource::<ProjectFile>();
        (pf.path.clone(), pf.dirty(world.resource::<History>()))
    };
    let (Some(path), true) = (path, dirty) else { return };
    let revision = world.resource::<History>().revision();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let t = std::time::Instant::now();
    match persist::save(world, &path) {
        Ok(stats) => {
            world.resource_mut::<ProjectFile>().saved_revision = revision;
            tracing::info!(
                "saved project in {:?}: {} entities, {} components, {} signals, {} new chunks ({} KB)",
                t.elapsed(),
                stats.entities,
                stats.components,
                stats.signals,
                stats.new_chunks,
                stats.new_bytes / 1024
            );
        }
        Err(e) => tracing::warn!("could not save project {}: {e:#}", path.display()),
    }
}

fn track_project(world: &mut World) {
    let Some((generation, video)) = world.get_resource::<Media>().map(|m| (m.generation, m.index().path.clone())) else {
        return;
    };

    // A different video was opened: save the old project, then switch.
    if generation != world.resource::<ProjectFile>().seen_generation {
        save_if_dirty(world);
        persist::clear_document(world);
        let path = tt_media::proxy::source_key(&video).ok().map(|k| tt_media::proxy::data_dir().join("projects").join(format!("{k}.ttproj")));
        if let Some(p) = path.as_ref().filter(|p| p.exists()) {
            match persist::load(world, p) {
                Ok(()) => tracing::info!("loaded project {}", p.display()),
                Err(e) => tracing::warn!("could not load project {}: {e:#}", p.display()),
            }
        }
        let meta = &mut world.resource_mut::<ProjectMeta>().0;
        meta.insert("media.path".into(), video.display().to_string());
        let revision = world.resource::<History>().revision();
        let mut pf = world.resource_mut::<ProjectFile>();
        pf.path = path;
        pf.seen_generation = generation;
        pf.saved_revision = revision;
        pf.last_change = (revision, 0.0);
        return;
    }

    // Autosave once edits have settled.
    let now = world.resource::<WallClock>().now;
    let revision = world.resource::<History>().revision();
    let mut pf = world.resource_mut::<ProjectFile>();
    if revision != pf.last_change.0 {
        pf.last_change = (revision, now);
    }
    let settled = now - pf.last_change.1 >= AUTOSAVE_AFTER && !world.resource::<History>().in_gesture();
    if settled {
        save_if_dirty(world);
    }
}

pub struct ProjectModule;

impl Module for ProjectModule {
    fn build(&self, app: &mut AppBuilder) {
        app.declare::<ProjectFile>(Class::Session).init_resource::<ProjectFile>().add_systems(track_project.in_set(Set::Prepare));
    }
}
