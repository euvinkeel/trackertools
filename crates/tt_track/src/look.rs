//! Looks (DESIGN §6.3): what a tracker's subject looks like. A look is an
//! entity on the tracker's `look` inputs, like a stroke on a sketch: a frame,
//! a rectangle there, and optionally a painted mask of which pixels are the
//! subject. The first look is the seed: the tracker starts on its frame,
//! exactly at its centre. Every look is a template the tracker matches.

use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;
use tt_core::op::Inputs;
use tt_core::time::FrameIndex;

/// Mask cells per side, laid over a look's rectangle.
pub const MASK_N: usize = 32;

#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component)]
pub struct Look {
    /// The frame it was taken on.
    pub frame: FrameIndex,
    /// Its rectangle there (source px): centre …
    pub x: f32,
    pub y: f32,
    /// … and half-size.
    pub half_w: f32,
    pub half_h: f32,
    /// Which pixels are the subject: `MASK_N × MASK_N` cells over the
    /// rectangle, row-major, 0–255. Empty = none painted (centre-weighted).
    pub mask: Vec<u8>,
}

impl Look {
    pub fn new(frame: FrameIndex, center: [f64; 2], half: [f64; 2]) -> Self {
        Self { frame, x: center[0] as f32, y: center[1] as f32, half_w: half[0].max(1.0) as f32, half_h: half[1].max(1.0) as f32, mask: Vec::new() }
    }

    pub fn center(&self) -> [f64; 2] {
        [self.x as f64, self.y as f64]
    }

    pub fn half(&self) -> [f64; 2] {
        [self.half_w.max(1.0) as f64, self.half_h.max(1.0) as f64]
    }

    /// `[x, y, left, top, right, bottom]` (source px).
    pub fn rect(&self) -> [f64; 6] {
        let ([x, y], [w, h]) = (self.center(), self.half());
        [x, y, x - w, y - h, x + w, y + h]
    }

    /// The mask, if one is painted (at least one cell on).
    pub fn painted(&self) -> Option<&[u8]> {
        (self.mask.len() == MASK_N * MASK_N && self.mask.iter().any(|c| *c > 0)).then_some(&self.mask[..])
    }
}

/// How new looks start (a user setting; the app remembers it).
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct LookDefaults {
    /// Paint a new look's mask from its pixels ([`LookMasker`]), so only the
    /// subject counts without a trip to the Look editor.
    pub auto_mask: bool,
}

impl Default for LookDefaults {
    fn default() -> Self {
        Self { auto_mask: true }
    }
}

/// Paints a look's mask from its pixels (None: nothing stands out, or its
/// frame isn't decoded). The app installs it: it has the decoded frames.
#[derive(Resource, Default, Clone, Copy)]
pub struct LookMasker(pub Option<MaskFn>);

/// Paints a look's mask from the pixels of its frame (see [`LookMasker`]).
pub type MaskFn = fn(&World, &Look) -> Option<Vec<u8>>;

/// `look` with its mask painted automatically, if the setting is on and a masker can.
pub fn auto_masked(world: &World, mut look: Look) -> Look {
    let on = world.get_resource::<LookDefaults>().is_some_and(|d| d.auto_mask);
    if on
        && look.painted().is_none()
        && let Some(mask) = world.get_resource::<LookMasker>().and_then(|m| m.0).and_then(|f| f(world, &look))
    {
        look.mask = mask;
    }
    look
}

/// A tracker's looks, in order (the first is the seed): live ones (not
/// deleted, nor references a reopened project couldn't resolve).
pub fn looks_of(world: &World, tracker: Entity) -> Vec<Entity> {
    world
        .get::<Inputs>(tracker)
        .map(|i| i.0.iter().filter(|(s, _)| s == "look").map(|(_, e)| *e).filter(|e| world.get::<Look>(*e).is_some() && world.get::<bevy_ecs::entity_disabling::Disabled>(*e).is_none()).collect())
        .unwrap_or_default()
}
