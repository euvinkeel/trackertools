//! Each sketch's colour, and the trackers that follow it (on request: "add
//! color changing feature to sketch so it can be more visible depending on
//! the background", "trackers adopted the colors of the sketches they are
//! contained in").
//!
//! A sketch has its own colour when one was chosen ([`Tint`], the
//! Inspector), else the next of [`PALETTE`] in the order sketches were made.
//! A tracker whose guide is a sketch takes that sketch's colour for what it
//! found; one with no sketch keeps [`style::AUTO`]. Lost frames stay
//! [`style::LOST`], drawn frames [`style::HAND`], and a stroke being
//! recorded [`style::LIVE`], so those still read the same everywhere.

use std::collections::HashMap;

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::prelude::*;
use egui::Color32;
use tt_core::op::Operator;
use tt_core::sketch::Tint;

use crate::style;

/// Sketch colours in order. The first is the hand's orange; the rest keep
/// clear of the colours that mean something else (cyan, red, amber, purple,
/// blue).
pub const PALETTE: [Color32; 8] = [
    style::HAND,
    Color32::from_rgb(0xf4, 0x72, 0xb6), // pink
    Color32::from_rgb(0xa3, 0xe6, 0x35), // lime
    Color32::from_rgb(0x34, 0xd3, 0x99), // green
    Color32::from_rgb(0xe8, 0x79, 0xf9), // fuchsia
    Color32::from_rgb(0xfd, 0xe6, 0x8a), // pale yellow
    Color32::from_rgb(0xf9, 0xa8, 0xd4), // light pink
    Color32::from_rgb(0xd9, 0xf9, 0x9d), // pale lime
];

/// The colours of this frame's sketches (looked up once per drawing pass).
pub struct Colors {
    sketches: HashMap<Entity, Color32>,
}

impl Colors {
    pub fn new(world: &World) -> Self {
        let mut sketches: Vec<(u64, u32, Entity, Option<Tint>)> = Vec::new();
        if let Some(mut q) = world.try_query_filtered::<(Entity, &Operator, Option<&tt_core::meta::Created>, Option<&Tint>), Without<Disabled>>() {
            for (e, op, created, tint) in q.iter(world) {
                if op.kind == "sketch" {
                    sketches.push((created.map_or(u64::MAX, |c| c.0), e.index_u32(), e, tint.copied()));
                }
            }
        }
        sketches.sort_by_key(|(c, i, _, _)| (*c, *i));
        let sketches = sketches
            .into_iter()
            .enumerate()
            .map(|(i, (_, _, e, tint))| (e, tint.map_or(PALETTE[i % PALETTE.len()], |Tint([r, g, b])| Color32::from_rgb(r, g, b))))
            .collect();
        Self { sketches }
    }

    /// A sketch's colour (the hand's orange for anything else).
    pub fn sketch(&self, sketch: Entity) -> Color32 {
        self.sketches.get(&sketch).copied().unwrap_or(style::HAND)
    }

    /// What a tracker found, in its sketch's colour (cyan without one).
    pub fn tracker(&self, world: &World, tracker: Entity) -> Color32 {
        tt_track::guide_of(world, tracker).and_then(|g| self.sketches.get(&g).copied()).unwrap_or(style::AUTO)
    }

    /// The colour of `e` on the timeline and in the outliner: a sketch's
    /// (or its stroke's) own, a tracker's sketch's, else its kind's.
    pub fn of(&self, world: &World, e: Entity, kind: crate::icons::Glyph) -> Color32 {
        use crate::icons::Glyph;
        match kind {
            Glyph::Sketch => self.sketch(e),
            Glyph::Stroke => world
                .try_query::<(Entity, &Operator, &tt_core::op::Inputs)>()
                .and_then(|mut q| q.iter(world).find(|(_, o, i)| o.kind == "sketch" && i.0.iter().any(|(_, p)| *p == e)).map(|(s, _, _)| self.sketch(s)))
                .unwrap_or(style::HAND),
            Glyph::Template | Glyph::CoTracker => self.tracker(world, e),
            other => other.color(),
        }
    }
}

/// `c` as a Tint.
pub fn tint_of(c: Color32) -> Tint {
    Tint([c.r(), c.g(), c.b()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ecs::name::Name;
    use tt_core::history::{edit, undo};
    use tt_core::op::{Inputs, Output};
    use tt_core::CoreModules;

    /// Sketches take the palette in the order they were made, a chosen colour
    /// wins (one undo step), and a tracker takes its sketch's colour.
    #[test]
    fn sketches_and_their_trackers_share_a_colour() {
        let mut app = tt_core::AppBuilder::new();
        app.add_module(CoreModules).add_module(tt_track::TrackModule);
        let mut w = app.build().world;
        let mut made = Vec::new();
        for i in 0..3 {
            edit(&mut w, "sketch", |tx| {
                let out = tx.create_signal(6);
                made.push(tx.spawn((Name::new(format!("Sketch {i}")), Operator { kind: "sketch".into() }, Inputs(Vec::new()), Output(out))));
            });
        }
        let mut tracker = None;
        edit(&mut w, "tracker", |tx| {
            let out = tx.create_signal(tt_track::TRACK_CHANNELS);
            tracker = Some(tx.spawn((Operator { kind: "track".into() }, Inputs(vec![("guide".into(), made[1])]), Output(out))));
        });
        let tracker = tracker.unwrap();
        let c = Colors::new(&w);
        assert_eq!([c.sketch(made[0]), c.sketch(made[1]), c.sketch(made[2])], [PALETTE[0], PALETTE[1], PALETTE[2]]);
        assert_eq!(c.tracker(&w, tracker), PALETTE[1], "a tracker takes its sketch's colour");

        edit(&mut w, "colour", |tx| tx.insert(made[1], tint_of(Color32::WHITE)));
        let c = Colors::new(&w);
        assert_eq!(c.sketch(made[1]), Color32::WHITE);
        assert_eq!(c.tracker(&w, tracker), Color32::WHITE, "and follows it when it changes");
        assert_eq!(c.sketch(made[2]), PALETTE[2], "the others keep theirs");
        undo(&mut w);
        assert_eq!(Colors::new(&w).sketch(made[1]), PALETTE[1], "one undo step");
    }
}
