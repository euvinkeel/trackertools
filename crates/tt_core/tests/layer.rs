//! Layers in a running core (tt_core::layer): attached to a subject, kept up
//! to date by the graph, dragged in the Select tool (fixed, then keyed with
//! auto-key; one undo step each), selected by a click on the picture, saved
//! and loaded.

mod common;

use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use common::{Driver, HOLD, PRESS, UP};
use tt_core::history::{History, undo};
use tt_core::layer::{AutoKey, LayerParams, attach, placed_at};
use tt_core::op::Output;
use tt_core::persist::{load, save};
use tt_core::selection::Selection;
use tt_core::signal::SignalStore;
use tt_core::subject::make_subject;
use tt_core::tool::{ActiveTool, Tool};
use tt_core::transport::Transport;

/// A point moving right 2 px a frame.
fn point(w: &mut World) -> Entity {
    let sig = w.resource_mut::<SignalStore>().create(6);
    {
        let mut store = w.resource_mut::<SignalStore>();
        let s = store.get_mut(sig).expect("created");
        for f in 0..600 {
            let x = 100.0 + 2.0 * f as f32;
            s.set(f, &[x, 300.0, x - 10.0, 290.0, x + 10.0, 310.0]);
        }
    }
    w.spawn((Name::new("A"), Output(sig))).id()
}

fn layered(d: &mut Driver) -> (Entity, Entity) {
    let a = point(&mut d.core.world);
    let s = make_subject(&mut d.core.world, &[a], 100).expect("a subject");
    d.frames(2, |_| [0.0, 0.0], UP);
    let params = LayerParams { media: "face.png".into(), media_size: [40.0, 20.0], ..LayerParams::default() };
    let l = attach(&mut d.core.world, s, params, 100).expect("attached");
    d.frames(2, |_| [0.0, 0.0], UP);
    (s, l)
}

#[test]
fn a_layer_rides_on_its_subject_and_drags_and_keys() {
    let mut d = Driver::new();
    let (_, l) = layered(&mut d);
    let w = &d.core.world;
    let p = w.get::<LayerParams>(l).expect("params");
    // Made as tall as the subject's box (20 px, its member's): scale 1 for a 20 px tall picture.
    let scale = p.scale.value as f64;
    assert!((scale - 1.0).abs() < 1e-6, "{scale}");
    let at = |w: &World, f| placed_at(w, l, f).expect("placed");
    assert_eq!(at(w, 100).at, [300.0, 300.0], "on the subject's point");
    assert_eq!(at(w, 200).at, [500.0, 300.0], "and moves with it");
    assert!((at(w, 100).scale[0] - scale).abs() < 1e-6);

    // A click on its picture selects it.
    d.core.world.resource_mut::<ActiveTool>().0 = Tool::Select;
    d.core.world.resource_mut::<Transport>().seek(100);
    d.frames(2, |_| [0.0, 0.0], UP);
    d.core.world.resource_mut::<Selection>().clear();
    d.frame(|_| [302.0, 301.0], PRESS);
    d.frame(|_| [302.0, 301.0], UP);
    assert_eq!(d.core.world.resource::<Selection>().primary(), Some(l), "a click on the picture selects it");

    // A drag of 30 px right moves its fixed offset (no keys, no auto-key): everywhere.
    let drag = |d: &mut Driver, dx: f64| {
        let t0 = d.now;
        let path = move |t: f64| {
            let u = ((t - t0 - 0.02) / 0.1).clamp(0.0, 1.0);
            [300.0 + dx * u, 300.0]
        };
        d.frame(path, PRESS);
        d.frames(30, path, HOLD);
        d.frame(path, UP);
        d.frames(2, |_| [0.0, 0.0], UP);
    };
    drag(&mut d, 30.0);
    let w = &d.core.world;
    assert!((at(w, 100).at[0] - 330.0).abs() < 1e-3 && (at(w, 400).at[0] - 930.0).abs() < 1e-3, "the fixed offset: on every frame");
    assert!(w.get::<LayerParams>(l).unwrap().offset_x.keys.is_empty());
    assert_eq!(w.resource::<History>().undo_label(), Some("Move face"), "one undo step");

    // With auto-key, a drag on frame 200 keys it there (and the first key holds everywhere).
    d.core.world.resource_mut::<AutoKey>().0 = true;
    d.core.world.resource_mut::<Transport>().seek(200);
    d.frames(2, |_| [0.0, 0.0], UP);
    let t0 = d.now;
    let path = move |t: f64| {
        let u = ((t - t0 - 0.02) / 0.1).clamp(0.0, 1.0);
        [530.0 - 20.0 * u, 300.0]
    };
    d.frame(path, PRESS);
    d.frames(30, path, HOLD);
    d.frame(path, UP);
    d.frames(2, |_| [0.0, 0.0], UP);
    let w = &d.core.world;
    let keys = &w.get::<LayerParams>(l).unwrap().offset_x.keys;
    assert_eq!(keys.iter().map(|k| (k.frame, k.value.round())).collect::<Vec<_>>(), vec![(200, 10.0)]);
    assert!((at(w, 200).at[0] - 510.0).abs() < 1e-3);
    undo(&mut d.core.world);
    d.frames(2, |_| [0.0, 0.0], UP);
    assert!(d.core.world.get::<LayerParams>(l).unwrap().offset_x.keys.is_empty(), "one undo takes the key back");
}

#[test]
fn a_layer_saves_and_loads() {
    let mut d = Driver::new();
    let (_, l) = layered(&mut d);
    tt_core::layer::set_params(&mut d.core.world, l, "edit", |p| {
        p.rotation.set(150, 45.0, true);
        p.end = tt_core::layer::EndMode::PingPong;
        p.size = tt_core::layer::SizeMode::Height;
    });
    d.frames(2, |_| [0.0, 0.0], UP);
    let want: Vec<_> = [100, 150, 300].iter().map(|f| placed_at(&d.core.world, l, *f)).collect();
    let path = std::env::temp_dir().join(format!("tt_layer_{}.ttproj", std::process::id()));
    save(&mut d.core.world, &path).expect("saves");
    let mut e = Driver::new();
    load(&mut e.core.world, &path).expect("loads");
    e.frames(2, |_| [0.0, 0.0], UP);
    let w = &mut e.core.world;
    let mut q = w.query::<(Entity, &LayerParams)>();
    let (l2, p) = q.iter(w).map(|(e, p)| (e, p.clone())).next().expect("the layer came back");
    assert_eq!((p.end, p.size, p.rotation.keys.len()), (tt_core::layer::EndMode::PingPong, tt_core::layer::SizeMode::Height, 1));
    let got: Vec<_> = [100, 150, 300].iter().map(|f| placed_at(w, l2, *f)).collect();
    assert_eq!(got, want);
    let _ = std::fs::remove_file(path);
}

/// A press-drag-release in the Select tool from `from` to `to` (source px; the test's view is the source).
fn drag(d: &mut Driver, from: [f64; 2], to: [f64; 2]) {
    drag_with(d, from, to, false);
}

fn drag_with(d: &mut Driver, from: [f64; 2], to: [f64; 2], alt: bool) {
    let t0 = d.now;
    let path = move |t: f64| {
        let u = ((t - t0 - 0.02) / 0.1).clamp(0.0, 1.0);
        [from[0] + (to[0] - from[0]) * u, from[1] + (to[1] - from[1]) * u]
    };
    d.frame(path, common::Input { alt, ..PRESS });
    d.frames(30, path, common::Input { alt, ..HOLD });
    d.frame(path, common::Input { alt, ..UP });
    d.frames(2, |_| [0.0, 0.0], UP);
}

/// The selected layer's handles: a corner scales it about its anchor, the
/// ring outside a corner turns it, Alt on its picture moves its anchor and
/// the picture stays put; each one undo step.
#[test]
fn the_selected_layers_handles_scale_turn_and_move_its_anchor() {
    let mut d = Driver::new();
    let (_, l) = layered(&mut d);
    d.core.world.resource_mut::<ActiveTool>().0 = Tool::Select;
    d.core.world.resource_mut::<Transport>().seek(100);
    d.frames(2, |_| [0.0, 0.0], UP);
    d.core.world.resource_mut::<Selection>().select_only(l);
    let at = |w: &World| placed_at(w, l, 100).expect("placed");
    let size = [40.0f32, 20.0];
    // Its bottom right corner is (320, 310) (40 × 20 around (300, 300)): drag it out to twice as far.
    let c = at(&d.core.world).corners(size)[2];
    assert_eq!(c, [320.0, 310.0]);
    drag(&mut d, c, [340.0, 320.0]);
    assert!((at(&d.core.world).scale[0] - 2.0).abs() < 1e-3, "twice the size: {:?}", at(&d.core.world));
    assert_eq!(d.core.world.resource::<History>().undo_label(), Some("Scale face"));
    // Just outside its top right corner (now (340, 280)): a quarter turn about its anchor.
    let c = at(&d.core.world).corners(size)[1];
    let from = [c[0] + 10.0, c[1] - 6.0];
    let (dx, dy) = (from[0] - 300.0, from[1] - 300.0);
    drag(&mut d, from, [300.0 - dy, 300.0 + dx]);
    assert!((at(&d.core.world).angle.to_degrees() - 90.0).abs() < 0.5, "a quarter turn: {}", at(&d.core.world).angle.to_degrees());
    assert_eq!(at(&d.core.world).at, [300.0, 300.0], "about its anchor: it stays put");
    assert_eq!(d.core.world.resource::<History>().undo_label(), Some("Turn face"));
    // Alt-drag on its picture: the anchor goes where the pointer lets go, and the picture doesn't move.
    let corners = at(&d.core.world).corners(size);
    drag_with(&mut d, [302.0, 304.0], [290.0, 310.0], true);
    let after = at(&d.core.world);
    assert!((after.at[0] - 290.0).abs() < 1e-3 && (after.at[1] - 310.0).abs() < 1e-3, "the anchor is where it was let go: {after:?}");
    for (a, b) in corners.iter().zip(after.corners(size)) {
        assert!((a[0] - b[0]).abs() < 1e-3 && (a[1] - b[1]).abs() < 1e-3, "the picture stays put: {a:?} → {b:?}");
    }
    undo(&mut d.core.world);
    d.frames(2, |_| [0.0, 0.0], UP);
    assert_eq!(at(&d.core.world).at, [300.0, 300.0], "one undo puts the anchor back");
}

/// Layers stack by depth (Front, Back…), one undo step each; smoothing
/// steadies what a layer follows without touching the tracking.
#[test]
fn layers_restack_and_smooth() {
    use tt_core::layer::{Restack, layers, restack, set_params};
    let mut d = Driver::new();
    let (s, a) = layered(&mut d);
    let params = LayerParams { media: "b.png".into(), media_size: [10.0, 10.0], ..LayerParams::default() };
    let b = attach(&mut d.core.world, s, params, 100).expect("attached");
    d.frames(2, |_| [0.0, 0.0], UP);
    assert_eq!(layers(&mut d.core.world), vec![a, b], "the order they were made");
    restack(&mut d.core.world, b, Restack::Back);
    assert_eq!(layers(&mut d.core.world), vec![b, a]);
    restack(&mut d.core.world, b, Restack::Up);
    assert_eq!(layers(&mut d.core.world), vec![a, b]);
    undo(&mut d.core.world);
    assert_eq!(layers(&mut d.core.world), vec![b, a], "one undo step");

    // A subject moving 2 px a frame, jittered ±3 px on alternate frames: smoothed, the layer barely jitters.
    let w = &mut d.core.world;
    set_params(w, a, "smooth", |p| p.smoothing = 0.1);
    d.frames(2, |_| [0.0, 0.0], UP);
    let x = |f: i64| placed_at(&d.core.world, a, f).expect("placed").at[0];
    assert!((x(300) - x(299) - 2.0).abs() < 1e-3, "a steady motion stays the same: {}", x(300) - x(299));
}
