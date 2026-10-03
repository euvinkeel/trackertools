//! In and out points in a running core (tt_core::marks): marked at the
//! playhead by their actions (I, O, Alt+X), gone to (Shift+I, Shift+O),
//! undone, saved and loaded.

mod common;

use common::{Driver, UP};
use tt_core::history::{redo, undo};
use tt_core::input::Action;
use tt_core::marks::{Marks, marks};
use tt_core::persist::{load, save};

/// One app frame with `a` asked for.
fn act(d: &mut Driver, a: Action) {
    d.frame(|_| [0.0, 0.0], common::Input { action: Some(a), ..UP });
}

#[test]
fn in_and_out_are_marked_at_the_playhead_one_undo_step_each() {
    let mut d = Driver::new();
    act(&mut d, Action::Seek(120));
    act(&mut d, Action::MarkIn);
    act(&mut d, Action::Seek(480));
    act(&mut d, Action::MarkOut);
    assert_eq!(marks(&d.core.world), Marks { mark_in: Some(120), mark_out: Some(480) });
    assert_eq!(marks(&d.core.world).frames(600), 120..481, "both frames included");
    act(&mut d, Action::GoToIn);
    assert_eq!(d.transport().frame(), 120);
    act(&mut d, Action::GoToOut);
    assert_eq!(d.transport().frame(), 480);

    undo(&mut d.core.world);
    assert_eq!(marks(&d.core.world), Marks { mark_in: Some(120), mark_out: None }, "the out point was one step");
    redo(&mut d.core.world);
    act(&mut d, Action::ClearMarks);
    assert!(!marks(&d.core.world).is_set());
    act(&mut d, Action::GoToOut);
    assert_eq!(d.transport().frame(), 599, "none: the end");
    undo(&mut d.core.world);
    assert_eq!(marks(&d.core.world), Marks { mark_in: Some(120), mark_out: Some(480) }, "clearing undoes");

    // Marking again where they are changes nothing (no undo step).
    act(&mut d, Action::Seek(120));
    let before = d.core.world.resource::<tt_core::history::History>().revision();
    act(&mut d, Action::MarkIn);
    assert_eq!(d.core.world.resource::<tt_core::history::History>().revision(), before);
}

#[test]
fn the_marks_save_and_load_with_the_project() {
    let mut d = Driver::new();
    act(&mut d, Action::Seek(30));
    act(&mut d, Action::MarkIn);
    act(&mut d, Action::Seek(90));
    act(&mut d, Action::MarkOut);
    let path = std::env::temp_dir().join(format!("tt_marks_{}.ttproj", std::process::id()));
    save(&mut d.core.world, &path).expect("saves");
    let mut e = Driver::new();
    assert!(!marks(&e.core.world).is_set());
    load(&mut e.core.world, &path).expect("loads");
    assert_eq!(marks(&e.core.world), Marks { mark_in: Some(30), mark_out: Some(90) });
    let _ = std::fs::remove_file(path);
}
