//! The document as one tree (on request: "is it possible for sketches to be
//! made underneath a tracker or anything with a position … how to
//! generalize and unify the logic of this?").
//!
//! One relation places everything: what a thing was made *relative to*
//! ([`parent_of`]):
//! - a sketch drawn inside a view: what that view follows (a sketch, a
//!   tracker, a subject: anything with a position);
//! - a tracker: the sketch that guides it, else what the view it tracks in
//!   follows;
//! - a subject or a SpringFocus: nothing (it has several members; they are listed under it
//!   too, as references).
//!
//! The outliner and the timeline's lanes both walk [`tree`]: the roots in
//! the order things were made (subjects first), each with its children, as
//! deep as they go. Strokes, looks and views hang off their own owner and
//! are added by those panels. A parent cycle (only a damaged file makes one)
//! is broken where it closes: those are listed at the top.

use std::collections::{HashMap, HashSet};

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::prelude::*;
use tt_core::meta::creation_order;
use tt_core::op::Operator;
use tt_core::view::{followed, home_of};

/// What a node of the tree is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Node {
    Subject,
    Sketch,
    Tracker,
    Focus,
}

impl Node {
    pub fn of(world: &World, e: Entity) -> Option<Node> {
        match world.get::<Operator>(e)?.kind.as_str() {
            "subject" => Some(Node::Subject),
            "sketch" => Some(Node::Sketch),
            "track" => Some(Node::Tracker),
            "focus" => Some(Node::Focus),
            _ => None,
        }
    }
}

/// What `e` was made relative to (module docs); None: a root.
pub fn parent_of(world: &World, e: Entity) -> Option<Entity> {
    let live = |p: Entity| world.get_entity(p).is_ok_and(|r| !r.contains::<Disabled>());
    let parent = match Node::of(world, e)? {
        Node::Subject | Node::Focus => None,
        Node::Sketch => home_of(world, e).and_then(|v| followed(world, v)),
        Node::Tracker => tt_track::guide_of(world, e).or_else(|| home_of(world, e).and_then(|v| followed(world, v))),
    };
    parent.filter(|p| *p != e && live(*p) && Node::of(world, *p).is_some())
}

/// Every live node, `(entity, depth, kind)`, parents before their children,
/// siblings in the order they were made; roots: subjects first.
pub fn tree(world: &mut World) -> Vec<(Entity, usize, Node)> {
    let mut q = world.query_filtered::<Entity, Without<Disabled>>();
    let all: Vec<Entity> = q.iter(world).collect();
    let mut nodes: Vec<(Entity, Node)> = all.into_iter().filter_map(|e| Some((e, Node::of(world, e)?))).collect();
    let mut order: Vec<Entity> = nodes.iter().map(|(e, _)| *e).collect();
    creation_order(world, &mut order);
    let rank: HashMap<Entity, usize> = order.iter().enumerate().map(|(i, e)| (*e, i)).collect();
    nodes.sort_by_key(|(e, kind)| (!matches!(kind, Node::Subject | Node::Focus), rank[e]));
    let kinds: HashMap<Entity, Node> = nodes.iter().copied().collect();
    let mut children: HashMap<Entity, Vec<Entity>> = HashMap::new();
    let mut roots = Vec::new();
    for &(e, _) in &nodes {
        match parent_of(world, e).filter(|p| kinds.contains_key(p)) {
            Some(p) => children.entry(p).or_default().push(e),
            None => roots.push(e),
        }
    }
    fn walk(e: Entity, depth: usize, kinds: &HashMap<Entity, Node>, children: &HashMap<Entity, Vec<Entity>>, seen: &mut HashSet<Entity>, out: &mut Vec<(Entity, usize, Node)>) {
        if !seen.insert(e) {
            return;
        }
        out.push((e, depth, kinds[&e]));
        for c in children.get(&e).into_iter().flatten() {
            walk(*c, depth + 1, kinds, children, seen, out);
        }
    }
    let (mut out, mut seen) = (Vec::new(), HashSet::new());
    // Nodes in a parent cycle have no root: listed at the top anyway.
    for e in roots.into_iter().chain(nodes.iter().map(|(e, _)| *e)) {
        walk(e, 0, &kinds, &children, &mut seen, &mut out);
    }
    out
}

/// The chain of parents above `e`, nearest first (to unfold down to it).
pub fn ancestors(world: &World, e: Entity) -> Vec<Entity> {
    let mut out = Vec::new();
    let mut p = parent_of(world, e);
    while let Some(x) = p {
        if out.contains(&x) || out.len() > 64 {
            break;
        }
        out.push(x);
        p = parent_of(world, x);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ecs::name::Name;
    use tt_core::history::edit;
    use tt_core::op::{Inputs, Output};
    use tt_core::view::ensure_view;

    fn op(w: &mut World, name: &str, kind: &str, inputs: Vec<(&str, Entity)>) -> Entity {
        let mut made = None;
        edit(w, name, |tx| {
            let out = tx.create_signal(8);
            let inputs = inputs.into_iter().map(|(s, e)| (s.to_string(), e)).collect();
            made = Some(tx.spawn((Name::new(name.to_string()), Operator { kind: kind.into() }, Inputs(inputs), Output(out))));
        });
        made.unwrap()
    }

    /// A sketch drawn inside a tracker's view is under that tracker, the
    /// tracker under its sketch; subjects come first; a parent cycle is
    /// still listed.
    #[test]
    fn everything_sits_under_what_it_was_made_relative_to() {
        let mut app = tt_core::AppBuilder::new();
        app.add_module(tt_core::CoreModules).add_module(tt_track::TrackModule);
        let mut w = app.build().world;
        let a = op(&mut w, "Sketch A", "sketch", vec![]);
        let t = op(&mut w, "Tracker T", "track", vec![("guide", a)]);
        let tv = ensure_view(&mut w, t);
        let b = op(&mut w, "Sketch B", "sketch", vec![("space", tv)]);
        let lone = op(&mut w, "Tracker L", "track", vec![]);
        let s = op(&mut w, "Subject S", "subject", vec![("member", t)]);
        let bv = ensure_view(&mut w, b);
        let d = op(&mut w, "Tracker D", "track", vec![("space", bv)]);
        assert_eq!(parent_of(&w, b), Some(t), "drawn in the tracker's view");
        assert_eq!(parent_of(&w, d), Some(b), "a tracker tracking in a sketch's view");
        let got: Vec<(Entity, usize)> = tree(&mut w).into_iter().map(|(e, d, _)| (e, d)).collect();
        assert_eq!(got, vec![(s, 0), (a, 0), (t, 1), (b, 2), (d, 3), (lone, 0)]);
        assert_eq!(ancestors(&w, d), vec![b, t, a]);

        // Two sketches each drawn in the other's view (a damaged file): both still listed.
        let mut app = tt_core::AppBuilder::new();
        app.add_module(tt_core::CoreModules).add_module(tt_track::TrackModule);
        let mut w = app.build().world;
        let x = op(&mut w, "X", "sketch", vec![]);
        let xv = ensure_view(&mut w, x);
        let y = op(&mut w, "Y", "sketch", vec![("space", xv)]);
        let yv = ensure_view(&mut w, y);
        edit(&mut w, "loop", |tx| tx.modify::<Inputs>(x, |i| i.0.push(("space".into(), yv))));
        let got: Vec<Entity> = tree(&mut w).into_iter().map(|(e, _, _)| e).collect();
        assert_eq!(got.len(), 2);
        assert!(got.contains(&x) && got.contains(&y));
    }
}
