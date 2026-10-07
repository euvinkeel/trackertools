//! The project of the open video (DESIGN §12): a `.ttproj` file. Each video
//! has its own project in the per-user data dir, keyed like its proxy; a
//! video can have more projects anywhere (New project…, Save project as…),
//! and the one used last on it opens with it (the session remembers each
//! video's list, [`crate::session::Session::projects_of`]). The open project
//! loads when the video opens, autosaves shortly after edits settle, and
//! saves before another video or project opens and on exit. Open project…
//! opens a `.ttproj` with its video (the `media.path` it was saved with).

use std::path::{Path, PathBuf};

use bevy_ecs::prelude::*;
use tt_core::history::History;
use tt_core::persist::{self, ProjectMeta};
use tt_core::time::WallClock;
use tt_core::{AppBuilder, Class, Module, Set};

use crate::media::{Media, OpenRequest, StatusLine};
use crate::session::Session;

/// Seconds without further edits before an autosave.
const AUTOSAVE_AFTER: f64 = 1.5;
/// A project file's extension.
pub const EXTENSION: &str = "ttproj";

#[derive(Resource, Default)]
pub struct ProjectFile {
    pub path: Option<PathBuf>,
    saved_revision: u64,
    seen_generation: u64,
    /// (revision, wall time) of the last change seen, for the debounce.
    last_change: (u64, f64),
    /// CoTracker trackers the status line says are paused since the project opened (0: it says nothing of them).
    paused_line: usize,
    /// The project to open with the next video (Open project… on another video's project).
    open_next: Option<PathBuf>,
}

impl ProjectFile {
    pub fn dirty(&self, history: &History) -> bool {
        self.path.is_some() && history.revision() != self.saved_revision
    }
}

/// A video's own project: in the data folder, keyed like its proxy.
pub fn own_project(video: &Path) -> Option<PathBuf> {
    tt_media::proxy::source_key(video).ok().map(|k| tt_media::proxy::data_dir().join("projects").join(format!("{k}.{EXTENSION}")))
}

/// What to call a project in the UI: "Own project" for a video's own, else its file name.
pub fn project_name(video: &Path, project: &Path) -> String {
    if own_project(video).as_deref() == Some(project) {
        "Own project".to_string()
    } else {
        project.file_stem().map_or_else(|| project.display().to_string(), |n| n.to_string_lossy().into_owned())
    }
}

/// Save now if there are unsaved changes (autosave, video switch, exit).
pub fn save_if_dirty(world: &mut World) {
    let dirty = {
        let pf = world.resource::<ProjectFile>();
        pf.dirty(world.resource::<History>())
    };
    if dirty {
        save_now(world);
    }
}

/// Save the project to its file now, changed or not.
fn save_now(world: &mut World) -> bool {
    let Some(path) = world.resource::<ProjectFile>().path.clone() else { return false };
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
            true
        }
        Err(e) => {
            tracing::warn!("could not save project {}: {e:#}", path.display());
            world.resource_mut::<StatusLine>().0 = Some((format!("Could not save the project to {}: {e}", path.display()), true));
            false
        }
    }
}

/// The status line after CoTracker trackers were paused on open (ASD-STE100).
fn paused_on_open(n: usize) -> String {
    let which = if n == 1 { "1 CoTracker tracker is paused".to_string() } else { format!("{n} CoTracker trackers are paused") };
    format!("{which}: CoTracker does not start when a project opens. To start one, select it. Then click Back, Both or Forward.")
}

/// Load the project at `p` into the (cleared) document. A file that can't be
/// read is kept aside (the next autosave would otherwise overwrite it).
fn load_into(world: &mut World, p: &Path) -> bool {
    match persist::load(world, p) {
        Ok(()) => {
            tracing::info!("loaded project {}", p.display());
            // CoTracker loads a model onto the graphics card: never by itself when a project opens.
            let paused = tt_track::pause_cotrackers_on_open(world).len();
            if paused > 0 {
                tracing::info!("paused {paused} CoTracker tracker(s) on open");
                world.resource_mut::<StatusLine>().0 = Some((paused_on_open(paused), false));
                world.resource_mut::<ProjectFile>().paused_line = paused;
                // (A start after an error says it too.)
                if let Some(mut notice) = world.get_resource_mut::<crate::recover::Notice>() {
                    notice.paused = paused;
                }
            }
            true
        }
        Err(e) => {
            persist::clear_document(world);
            let aside = p.with_extension(format!("unreadable.{EXTENSION}"));
            let kept = std::fs::copy(p, &aside).is_ok();
            tracing::warn!("could not load project {}: {e:#}; {}", p.display(), if kept { format!("kept as {}", aside.display()) } else { "could not copy it aside".into() });
            world.resource_mut::<StatusLine>().0 =
                Some((format!("Couldn't read the project (kept as {}); starting fresh", aside.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned())), true));
            false
        }
    }
}

/// From now on the document is the project at `path` (saved as it is), on `video`.
fn adopt(world: &mut World, video: &Path, path: Option<PathBuf>) {
    world.resource_mut::<ProjectMeta>().0.insert("media.path".into(), video.display().to_string());
    if let Some(p) = &path
        && let Some(mut session) = world.get_resource_mut::<Session>()
    {
        session.note_project(video, p);
    }
    let revision = world.resource::<History>().revision();
    let mut pf = world.resource_mut::<ProjectFile>();
    pf.path = path;
    pf.saved_revision = revision;
    pf.last_change = (revision, 0.0);
}

/// The open video, if any.
fn video(world: &World) -> Option<PathBuf> {
    world.get_resource::<Media>().map(|m| m.index().path.clone())
}

/// `path` with the project extension.
fn with_extension(path: PathBuf) -> PathBuf {
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case(EXTENSION)) { path } else { path.with_extension(EXTENSION) }
}

/// A file about to be written afresh: the old one (and SQLite's journal) goes first.
fn replace_file(path: &Path) -> std::io::Result<()> {
    for p in [path.to_path_buf(), PathBuf::from(format!("{}-journal", path.display()))] {
        match std::fs::remove_file(&p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// Save the open project as `path`; from now on it saves there. (Its old file keeps what it had.)
pub fn save_as(world: &mut World, path: PathBuf) -> bool {
    let Some(video) = video(world) else { return false };
    save_as_on(world, &video, path)
}

fn save_as_on(world: &mut World, video: &Path, path: PathBuf) -> bool {
    let path = with_extension(path);
    if world.resource::<ProjectFile>().path.as_deref() != Some(&path)
        && let Err(e) = replace_file(&path)
    {
        world.resource_mut::<StatusLine>().0 = Some((format!("Could not replace {}: {e}", path.display()), true));
        return false;
    }
    let old = world.resource::<ProjectFile>().path.clone();
    world.resource_mut::<ProjectFile>().path = Some(path.clone());
    if !save_now(world) {
        world.resource_mut::<ProjectFile>().path = old;
        return false;
    }
    adopt(world, video, Some(path.clone()));
    world.resource_mut::<StatusLine>().0 = Some((format!("Saved as {}. Changes now save to this file.", project_name(video, &path)), false));
    true
}

/// Start a new, empty project on the open video, saved at `path`. The open one is saved first.
pub fn new_project(world: &mut World, path: PathBuf) -> bool {
    let Some(video) = video(world) else { return false };
    new_project_on(world, &video, path)
}

fn new_project_on(world: &mut World, video: &Path, path: PathBuf) -> bool {
    let path = with_extension(path);
    save_if_dirty(world);
    if let Err(e) = replace_file(&path) {
        world.resource_mut::<StatusLine>().0 = Some((format!("Could not replace {}: {e}", path.display()), true));
        return false;
    }
    persist::clear_document(world);
    world.resource_mut::<ProjectFile>().paused_line = 0;
    adopt(world, video, Some(path.clone()));
    save_now(world);
    world.resource_mut::<StatusLine>().0 = Some((format!("New project {}. The project before it is saved.", project_name(video, &path)), false));
    true
}

/// Whether a project saved with the video at `saved` is for the video at `open`.
fn same_video(saved: &Path, open: &Path) -> bool {
    saved == open || matches!((saved.canonicalize(), open.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

/// Open the project at `path`: on the open video if it was made for it,
/// else with its own video (opened first). The open project is saved first.
pub fn open_project(world: &mut World, path: PathBuf) -> bool {
    let open = video(world);
    open_project_on(world, open, path)
}

fn open_project_on(world: &mut World, open: Option<PathBuf>, path: PathBuf) -> bool {
    let saved_video = match persist::read_meta(&path) {
        Ok(meta) => meta.get("media.path").map(PathBuf::from),
        Err(e) => {
            world.resource_mut::<StatusLine>().0 = Some((format!("Could not open {}: {e}", path.display()), true));
            return false;
        }
    };
    let here = match (&saved_video, &open) {
        (Some(s), Some(o)) => same_video(s, o),
        // Its video is unknown: on the open one.
        (None, Some(_)) => true,
        (_, None) => false,
    };
    if here || saved_video.as_ref().is_none_or(|s| !s.exists()) {
        let Some(video) = open else {
            let missing = saved_video.map_or_else(|| "its video".to_string(), |s| s.display().to_string());
            world.resource_mut::<StatusLine>().0 = Some((format!("Could not find {missing}. Open that video first. Then open the project again."), true));
            return false;
        };
        save_if_dirty(world);
        persist::clear_document(world);
        world.resource_mut::<ProjectFile>().paused_line = 0;
        load_into(world, &path);
        adopt(world, &video, Some(path.clone()));
        if !here {
            world.resource_mut::<StatusLine>().0 = Some((
                format!("{}'s video was not found, so it is open on this video. Changes save to {}.", project_name(&video, &path), path.display()),
                false,
            ));
        }
        return true;
    }
    // Another video's project: open that video, and this project with it.
    let video = saved_video.expect("checked");
    world.resource_mut::<ProjectFile>().open_next = Some(path);
    world.resource_mut::<OpenRequest>().0 = Some(video);
    true
}

/// Forget `project` on the open video's list (its file is gone).
pub fn forget(world: &mut World, project: &Path) {
    if let Some(video) = video(world)
        && let Some(mut session) = world.get_resource_mut::<Session>()
    {
        session.forget_project(&video, project);
    }
}

fn track_project(world: &mut World) {
    let Some((generation, video)) = world.get_resource::<Media>().map(|m| (m.generation, m.index().path.clone())) else {
        return;
    };

    // A different video was opened: save the old project, then switch.
    if generation != world.resource::<ProjectFile>().seen_generation {
        save_if_dirty(world);
        world.resource_mut::<ProjectFile>().paused_line = 0;
        persist::clear_document(world);
        // Asked for (Open project…), else the one used last on this video, else its own.
        let asked = world.resource_mut::<ProjectFile>().open_next.take();
        let last = world.get_resource::<Session>().and_then(|s| s.projects_of(&video).iter().find(|p| p.exists()).cloned());
        let path = asked.or(last).or_else(|| own_project(&video));
        if let Some(p) = path.as_ref().filter(|p| p.exists()) {
            load_into(world, p);
        }
        world.resource_mut::<ProjectFile>().seen_generation = generation;
        adopt(world, &video, path);
        return;
    }

    follow_paused_line(world);

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

/// The open-time line follows the trackers: once one is asked to track
/// again it counts one less, and goes when none is left (unless another
/// message has replaced it).
fn follow_paused_line(world: &mut World) {
    let shown = world.resource::<ProjectFile>().paused_line;
    if shown == 0 {
        return;
    }
    let now = world.query::<&tt_track::PausedOnOpen>().iter(world).count();
    if now == shown {
        return;
    }
    let ours = world.resource::<StatusLine>().0.as_ref().is_some_and(|(m, _)| *m == paused_on_open(shown));
    if ours {
        world.resource_mut::<StatusLine>().0 = (now > 0).then(|| (paused_on_open(now), false));
    }
    world.resource_mut::<ProjectFile>().paused_line = if ours { now } else { 0 };
}

pub struct ProjectModule;

impl Module for ProjectModule {
    fn build(&self, app: &mut AppBuilder) {
        app.declare::<ProjectFile>(Class::Session).init_resource::<ProjectFile>().add_systems(track_project.in_set(Set::Prepare));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ecs::name::Name;
    use tt_core::history::edit;
    use tt_core::op::{Inputs, Operator};
    use tt_core::CoreModules;

    fn world() -> World {
        let mut app = AppBuilder::new();
        app.add_module(CoreModules).add_module(tt_track::TrackModule);
        let mut world = app.build().world;
        world.init_resource::<ProjectFile>();
        world.init_resource::<StatusLine>();
        world.init_resource::<OpenRequest>();
        world
    }

    /// One more operator named `name`, as an undo step (a change to save).
    fn add(world: &mut World, name: &str) {
        edit(world, "add", |tx| {
            tx.spawn((Name::new(name.to_string()), Operator { kind: "subject".into() }, Inputs(Vec::new())));
        });
    }

    fn names(world: &mut World) -> Vec<String> {
        let mut q = world.query::<(&Name, &Operator)>();
        let mut v: Vec<String> = q.iter(world).map(|(n, _)| n.to_string()).collect();
        v.sort();
        v
    }

    /// Two projects on one video: Save as forks, New starts empty, Open
    /// switches back; each file keeps its own; a project of another video
    /// opens that video first.
    #[test]
    fn a_video_has_several_projects() {
        let dir = std::env::temp_dir().join(format!("tt-projects-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("clip.mp4");
        let other = dir.join("other.mp4");
        std::fs::write(&video, b"not really a video").unwrap();
        std::fs::write(&other, b"nor this").unwrap();
        let mut w = world();
        let (a, b, c) = (dir.join("a.ttproj"), dir.join("b"), dir.join("c.ttproj"));

        // Project A, with one thing in it.
        adopt(&mut w, &video, Some(a.clone()));
        add(&mut w, "first");
        save_if_dirty(&mut w);
        // Save as B (the extension is added): B has it, and changes go to B only.
        assert!(save_as_on(&mut w, &video, b.clone()));
        let b = dir.join("b.ttproj");
        assert_eq!(w.resource::<ProjectFile>().path.as_deref(), Some(b.as_path()));
        add(&mut w, "second");
        save_if_dirty(&mut w);
        // New project C: empty, and A and B are kept.
        assert!(new_project_on(&mut w, &video, c.clone()));
        assert!(names(&mut w).is_empty(), "a new project is empty");
        assert!(c.exists());
        add(&mut w, "third");
        save_if_dirty(&mut w);
        // Back to each: what it had.
        for (p, want) in [(&a, vec!["first"]), (&b, vec!["first", "second"]), (&c, vec!["third"])] {
            assert!(open_project_on(&mut w, Some(video.clone()), p.clone()));
            assert_eq!(names(&mut w), want, "{}", p.display());
            assert_eq!(w.resource::<ProjectFile>().path.as_deref(), Some(p.as_path()));
            assert!(!w.resource::<ProjectFile>().dirty(w.resource::<History>()), "nothing to save right after opening");
        }

        // A project of another video: that video opens, and this project with it.
        adopt(&mut w, &other, Some(dir.join("o.ttproj")));
        add(&mut w, "elsewhere");
        save_if_dirty(&mut w);
        adopt(&mut w, &video, Some(c.clone()));
        assert!(open_project_on(&mut w, Some(video.clone()), dir.join("o.ttproj")));
        assert_eq!(w.resource::<OpenRequest>().0.as_deref(), Some(other.as_path()));
        assert_eq!(w.resource::<ProjectFile>().open_next.as_deref(), Some(dir.join("o.ttproj").as_path()));

        // Not a project: nothing changes, and the status line says why.
        assert!(!open_project_on(&mut w, Some(video.clone()), video.clone()));
        assert!(w.resource::<StatusLine>().0.as_ref().is_some_and(|(_, error)| *error));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
