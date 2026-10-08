//! Switching a tracker off (DESIGN §6.5) (on request: "enable or disable a
//! tracker easily … marked as disabled if they go off the rails and then can
//! come back again when you know they're good, and at that point other
//! trackers would contribute/take over if they are enabled"):
//!
//! - A tracker's **off keys** ([`TrackerOff`]): from each key's frame on, it
//!   is off or on again; before the first, on. They are part of the
//!   document (saved, undone).
//! - Where it is off, its output frames carry the [`crate::OFF`] flag
//!   (`human::compose`): what reads trackers' flags leaves them out there, as
//!   it does lost frames: a subject goes on with its other members, an
//!   export's points without it. Its results stay as they are (switching it
//!   on again shows them again): nothing is tracked again.
//! - **H** switches the selected trackers off from the playhead on (or on
//!   again, where they are off); **Shift+H** off everywhere (or on
//!   everywhere, where any of it is off). The Inspector has the same, the
//!   timeline hatches where a tracker is off, and the video draws it grey there.

use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;
use tt_core::history::edit;
use tt_core::input::{Action, PendingActions};
use tt_core::selection::Selection;
use tt_core::time::FrameIndex;
use tt_core::transport::Transport;

use crate::is_tracker;

/// A tracker's off keys: `(frame, off)`, sorted by frame: from that frame on
/// it is off (true) or on (false). None or empty: on everywhere.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component)]
pub struct TrackerOff(pub Vec<(FrameIndex, bool)>);

impl TrackerOff {
    /// Off on frame `f`.
    pub fn is_off(&self, f: FrameIndex) -> bool {
        self.0.iter().rev().find(|(k, _)| *k <= f).is_some_and(|(_, off)| *off)
    }

    /// Off from `f` on (`off`), or on again: a key there, the redundant ones gone.
    pub fn set_from(&mut self, f: FrameIndex, off: bool) {
        self.0.retain(|(k, _)| *k != f);
        self.0.push((f, off));
        self.0.sort_by_key(|(k, _)| *k);
        self.tidy();
    }

    /// Keys that change nothing (the same as the state before them) go.
    fn tidy(&mut self) {
        let mut state = false;
        self.0.retain(|(_, off)| {
            let keep = *off != state;
            state = *off;
            keep
        });
    }

    /// The stretches it is off within `lo..hi`.
    pub fn off_ranges(&self, lo: FrameIndex, hi: FrameIndex) -> Vec<std::ops::Range<FrameIndex>> {
        let mut out = Vec::new();
        let mut from = self.is_off(lo).then_some(lo);
        for (k, off) in self.0.iter().filter(|(k, _)| *k > lo && *k < hi) {
            match (from, off) {
                (None, true) => from = Some(*k),
                (Some(s), false) => {
                    out.push(s..*k);
                    from = None;
                }
                _ => {}
            }
        }
        if let Some(s) = from {
            out.push(s..hi);
        }
        out
    }

    pub fn any_off(&self) -> bool {
        self.0.iter().any(|(_, off)| *off)
    }
}

/// Whether `tracker` is off on frame `f`.
pub fn is_off(world: &World, tracker: Entity, f: FrameIndex) -> bool {
    world.get::<TrackerOff>(tracker).is_some_and(|o| o.is_off(f))
}

/// Switch `trackers` off from frame `f` on, or on again (`off` false), one undo step.
pub fn switch_from(world: &mut World, trackers: &[Entity], f: FrameIndex, off: bool) {
    let what = if off { "Switch off" } else { "Switch on" };
    edit(world, what, |tx| {
        for t in trackers {
            let mut keys = tx.world().get::<TrackerOff>(*t).cloned().unwrap_or_default();
            keys.set_from(f, off);
            tx.insert(*t, keys);
        }
    });
}

/// Switch `trackers` off everywhere, or on everywhere (`off` false), one undo step.
pub fn switch_everywhere(world: &mut World, trackers: &[Entity], off: bool) {
    let what = if off { "Switch off everywhere" } else { "Switch on everywhere" };
    edit(world, what, |tx| {
        for t in trackers {
            tx.insert(*t, TrackerOff(if off { vec![(FrameIndex::MIN, true)] } else { Vec::new() }));
        }
    });
}

/// `Set::Intents`: H and Shift+H on the selected trackers (module docs).
pub fn switch_actions(world: &mut World) {
    let asked = world.resource_mut::<PendingActions>().take(|a| matches!(a, Action::SwitchTracker | Action::SwitchTrackerEverywhere));
    if asked.is_empty() {
        return;
    }
    let selected: Vec<Entity> = world.resource::<Selection>().entities.iter().copied().filter(|e| is_tracker(world, *e)).collect();
    if selected.is_empty() {
        return;
    }
    let f = world.resource::<Transport>().frame();
    for a in asked {
        match a {
            // Off here (where any is on), else on again.
            Action::SwitchTracker => {
                let off = selected.iter().any(|t| !is_off(world, *t, f));
                switch_from(world, &selected, f, off);
            }
            Action::SwitchTrackerEverywhere => {
                let off = !selected.iter().any(|t| world.get::<TrackerOff>(*t).is_some_and(TrackerOff::any_off));
                switch_everywhere(world, &selected, off);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_say_where_it_is_off() {
        let mut o = TrackerOff::default();
        assert!(!o.is_off(0));
        o.set_from(100, true);
        o.set_from(150, false);
        assert!(!o.is_off(99) && o.is_off(100) && o.is_off(149) && !o.is_off(150));
        assert_eq!(o.off_ranges(0, 300), vec![100..150]);
        assert_eq!(o.off_ranges(120, 300), vec![120..150]);
        // Off again from 200: two stretches; switching on where it is on changes nothing.
        o.set_from(200, true);
        o.set_from(50, false);
        assert_eq!(o.0, vec![(100, true), (150, false), (200, true)]);
        assert_eq!(o.off_ranges(0, 300), vec![100..150, 200..300]);
        // On again from 100: the first stretch goes.
        o.set_from(100, false);
        assert_eq!(o.off_ranges(0, 300), vec![200..300]);
    }
}
