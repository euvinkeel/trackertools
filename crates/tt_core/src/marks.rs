//! In and out points: the part of the video an export covers, marked on the
//! timeline as in an editor. I marks the in point at the playhead, O the out
//! point, Alt+X clears both; Shift+I / Shift+O go to them. Both frames are
//! included; an end not marked is the video's own. Marking an in point after
//! the out point (or an out before the in) clears the other one, as editors
//! do. Saved with the project (in [`ProjectMeta`]); marking is an undo step
//! like any edit, and dragging a mark on the timeline is one.

use std::ops::Range;

use bevy_ecs::prelude::*;

use crate::app::{AppBuilder, Module, Set};
use crate::history::{History, edit};
use crate::input::{Action, PendingActions};
use crate::persist::ProjectMeta;
use crate::time::FrameIndex;
use crate::transport::Transport;

const IN: &str = "marks.in";
const OUT: &str = "marks.out";

/// The in and out points (grid frames, both included); `None`: not marked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Marks {
    pub mark_in: Option<FrameIndex>,
    pub mark_out: Option<FrameIndex>,
}

impl Marks {
    pub fn is_set(&self) -> bool {
        self.mark_in.is_some() || self.mark_out.is_some()
    }

    /// The frames they cover in a video of `count` frames (half-open): from
    /// the in point to the out point, both included; the whole video if
    /// neither is marked.
    pub fn frames(&self, count: FrameIndex) -> Range<FrameIndex> {
        if count <= 0 {
            return 0..0;
        }
        let last = count - 1;
        let first = self.mark_in.unwrap_or(0).clamp(0, last);
        first..self.mark_out.unwrap_or(last).clamp(first, last) + 1
    }

    /// The in point at `f`; an out point before it goes.
    pub fn with_in(self, f: FrameIndex) -> Self {
        Self { mark_in: Some(f), mark_out: self.mark_out.filter(|o| *o >= f) }
    }

    /// The out point at `f`; an in point after it goes.
    pub fn with_out(self, f: FrameIndex) -> Self {
        Self { mark_in: self.mark_in.filter(|i| *i <= f), mark_out: Some(f) }
    }
}

/// The project's in and out points.
pub fn marks(world: &World) -> Marks {
    let Some(meta) = world.get_resource::<ProjectMeta>() else { return Marks::default() };
    let get = |k: &str| meta.0.get(k).and_then(|v| v.parse().ok());
    Marks { mark_in: get(IN), mark_out: get(OUT) }
}

/// Mark them: one undo step labelled `label` (or part of the open gesture).
/// Returns whether anything changed.
pub fn set_marks(world: &mut World, new: Marks, label: &str) -> bool {
    if marks(world) == new {
        return false;
    }
    edit(world, label, |tx| {
        tx.modify_resource::<ProjectMeta>(|m| {
            for (key, value) in [(IN, new.mark_in), (OUT, new.mark_out)] {
                match value {
                    Some(f) => m.0.insert(key.to_string(), f.to_string()),
                    None => m.0.remove(key),
                };
            }
        })
    })
}

/// Start dragging a mark on the timeline: its edits are one undo step until [`end_drag`].
pub fn begin_drag(world: &mut World, which: &str) {
    world.resource_mut::<History>().begin(format!("Move {which} point"));
}

pub fn end_drag(world: &mut World) {
    world.resource_mut::<History>().end();
}

fn apply_mark_actions(world: &mut World) {
    use Action::*;
    let actions = world.resource_mut::<PendingActions>().take(|a| matches!(a, MarkIn | MarkOut | ClearMarks | GoToIn | GoToOut));
    for a in actions {
        let t = world.resource::<Transport>();
        if !t.has_media() {
            continue;
        }
        let (here, count) = (t.frame(), t.frame_count);
        let m = marks(world);
        match a {
            MarkIn => {
                set_marks(world, m.with_in(here), "Mark in");
            }
            MarkOut => {
                set_marks(world, m.with_out(here), "Mark out");
            }
            ClearMarks => {
                set_marks(world, Marks::default(), "Clear in and out");
            }
            GoToIn => world.resource_mut::<Transport>().seek(m.frames(count).start),
            GoToOut => world.resource_mut::<Transport>().seek(m.frames(count).end - 1),
            _ => {}
        }
    }
}

pub struct MarksModule;

impl Module for MarksModule {
    fn build(&self, app: &mut AppBuilder) {
        app.add_systems(apply_mark_actions.in_set(Set::Intents));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marks_cover_in_to_out_both_included() {
        let none = Marks::default();
        assert_eq!(none.frames(100), 0..100, "nothing marked: the whole video");
        assert_eq!(none.with_in(20).frames(100), 20..100, "an in point alone: to the end");
        assert_eq!(none.with_out(49).frames(100), 0..50, "an out point alone: from the start");
        assert_eq!(none.with_in(20).with_out(49).frames(100), 20..50);
        assert_eq!(none.with_in(20).with_out(20).frames(100), 20..21, "one frame");
        assert_eq!(Marks { mark_in: Some(-5), mark_out: Some(400) }.frames(100), 0..100, "clamped to the video");
        assert_eq!(none.frames(0), 0..0);
    }

    #[test]
    fn marking_past_the_other_end_clears_it() {
        let m = Marks::default().with_in(20).with_out(49);
        assert_eq!(m.with_in(60), Marks { mark_in: Some(60), mark_out: None }, "an in after the out: the out goes");
        assert_eq!(m.with_out(10), Marks { mark_in: None, mark_out: Some(10) }, "an out before the in: the in goes");
        assert_eq!(m.with_in(30), Marks { mark_in: Some(30), mark_out: Some(49) }, "inside: kept");
    }
}
