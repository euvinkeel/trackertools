//! Actions and the keymap (DESIGN §14): keys are data, not code.
//!
//! The host (tt_app) translates raw key presses into [`Action`]s through the
//! [`Keymap`] resource and queues them in [`PendingActions`]. Modules apply the
//! actions they own in [`Set::Intents`](crate::Set::Intents). Nothing reads
//! keys directly, so rebinding never touches feature code.

use bevy_ecs::prelude::*;

use crate::app::{AppBuilder, Module};
use crate::time::FrameIndex;
use crate::tool::Tool;

/// Something the user asked for — from a key, a button or a panel gesture.
/// Features add variants as they arrive. Payload variants (e.g. `Seek`) are
/// emitted by panels, not bound to keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    Seek(FrameIndex),
    /// Index into [`crate::transport::RATES`].
    SetRate(u8),
    ToggleLoop,
    TogglePlay,
    StepForward,
    StepBackward,
    JumpForward,
    JumpBackward,
    GoToStart,
    GoToEnd,
    FasterPlayback,
    SlowerPlayback,
    /// L: play forward; again, twice as fast (DaVinci-style shuttle).
    ShuttleForward,
    /// J: play backward; again, twice as fast.
    ShuttleBackward,
    FrameAll,
    OpenFile,
    Undo,
    Redo,
    /// Switch to a tool, or back to Select if it is already active.
    Tool(Tool),
    /// Abandon the gesture in progress (Esc); with none, return to Select.
    Cancel,
    /// Clear the selection.
    DeselectAll,
    /// Show the selected sketch's view in the viewport (creating it on first use).
    EnterView,
    /// Back out to the parent view (or the source).
    ExitView,
    /// Delete the selection (and what belongs to it).
    Delete,
    /// Duplicate the selected sketches.
    Duplicate,
    /// Select every sketch.
    SelectAll,
    /// Rename the selected entity (the outliner edits the name).
    Rename,
    /// Track the selected sketches from the playhead (re-seed selected trackers there).
    Track,
    /// Switch the selected trackers off from the playhead on (or on again there).
    SwitchTracker,
    /// Switch the selected trackers off everywhere (or on everywhere).
    SwitchTrackerEverywhere,
    /// Scrubbing snaps the playhead to the edges of timeline objects, or stops snapping.
    ToggleSnap,
    /// Mark the in point (an export's first frame) at the playhead (`crate::marks`).
    MarkIn,
    /// Mark the out point (an export's last frame) at the playhead.
    MarkOut,
    /// Clear the in and out points.
    ClearMarks,
    /// Go to the in point (the start, if none).
    GoToIn,
    /// Go to the out point (the end, if none).
    GoToOut,
}

/// A key on the keyboard, independent of any UI toolkit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Space,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Home,
    End,
    Escape,
    Tab,
    Delete,
    F2,
    Letter(char),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

impl Mods {
    pub const NONE: Mods = Mods { ctrl: false, shift: false, alt: false };
    pub const SHIFT: Mods = Mods { ctrl: false, shift: true, alt: false };
    pub const CTRL: Mods = Mods { ctrl: true, shift: false, alt: false };
}

/// A key chord. `repeat` bindings also fire on key auto-repeat (stepping).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Binding {
    pub key: Key,
    pub mods: Mods,
    pub repeat: bool,
}

/// Key → action table. Defaults are Blender-like (DESIGN §14).
#[derive(Resource, Debug, Clone)]
pub struct Keymap {
    pub bindings: Vec<(Binding, Action)>,
}

impl Default for Keymap {
    fn default() -> Self {
        use Action::*;
        let b = |key, mods, repeat| Binding { key, mods, repeat };
        Self {
            bindings: vec![
                (b(Key::Space, Mods::NONE, false), TogglePlay),
                (b(Key::ArrowRight, Mods::NONE, true), StepForward),
                (b(Key::ArrowLeft, Mods::NONE, true), StepBackward),
                (b(Key::ArrowUp, Mods::NONE, true), JumpForward),
                (b(Key::ArrowDown, Mods::NONE, true), JumpBackward),
                (b(Key::ArrowLeft, Mods::SHIFT, false), GoToStart),
                (b(Key::ArrowRight, Mods::SHIFT, false), GoToEnd),
                (b(Key::Home, Mods::NONE, false), FrameAll),
                // Playback speed (= capture speed): Q slower, E faster; [ / ] too.
                (b(Key::Letter('e'), Mods::NONE, false), FasterPlayback),
                (b(Key::Letter('q'), Mods::NONE, false), SlowerPlayback),
                (b(Key::Letter(']'), Mods::NONE, false), FasterPlayback),
                (b(Key::Letter('['), Mods::NONE, false), SlowerPlayback),
                // Shuttle, as in DaVinci Resolve: J backward, K play/pause, L forward.
                (b(Key::Letter('l'), Mods::NONE, false), ShuttleForward),
                (b(Key::Letter('j'), Mods::NONE, false), ShuttleBackward),
                (b(Key::Letter('k'), Mods::NONE, false), TogglePlay),
                (b(Key::Letter('o'), Mods::CTRL, false), OpenFile),
                (b(Key::Letter('z'), Mods::CTRL, true), Undo),
                (b(Key::Letter('z'), Mods { ctrl: true, shift: true, alt: false }, true), Redo),
                (b(Key::Letter('y'), Mods::CTRL, true), Redo),
                // Blender: D + drag draws (annotate); here it arms the Sketch tool.
                (b(Key::Letter('d'), Mods::NONE, false), Tool(crate::tool::Tool::Sketch)),
                (b(Key::Escape, Mods::NONE, false), Cancel),
                (b(Key::Letter('a'), Mods { ctrl: false, shift: false, alt: true }, false), DeselectAll),
                // Like entering a group (Blender's Tab into edit mode, Figma's Enter).
                (b(Key::Tab, Mods::NONE, false), EnterView),
                (b(Key::Tab, Mods::SHIFT, false), ExitView),
                (b(Key::Letter('x'), Mods::NONE, false), Delete),
                (b(Key::Delete, Mods::NONE, false), Delete),
                (b(Key::Letter('d'), Mods::SHIFT, false), Duplicate),
                (b(Key::Letter('a'), Mods::NONE, false), SelectAll),
                (b(Key::F2, Mods::NONE, false), Rename),
                (b(Key::Letter('t'), Mods::NONE, false), Tool(crate::tool::Tool::Track)),
                (b(Key::Letter('h'), Mods::NONE, false), SwitchTracker),
                (b(Key::Letter('h'), Mods::SHIFT, false), SwitchTrackerEverywhere),
                // M: draw a tracker's point by hand (a manual dot, or over a tracker's results).
                (b(Key::Letter('m'), Mods::NONE, false), Tool(crate::tool::Tool::Draw)),
                (b(Key::Letter('n'), Mods::NONE, false), ToggleSnap),
                // In and out points, as in DaVinci Resolve.
                (b(Key::Letter('i'), Mods::NONE, false), MarkIn),
                (b(Key::Letter('o'), Mods::NONE, false), MarkOut),
                (b(Key::Letter('x'), Mods { ctrl: false, shift: false, alt: true }, false), ClearMarks),
                (b(Key::Letter('i'), Mods::SHIFT, false), GoToIn),
                (b(Key::Letter('o'), Mods::SHIFT, false), GoToOut),
            ],
        }
    }
}

impl Keymap {
    /// The action bound to a key press, if any.
    pub fn lookup(&self, key: Key, mods: Mods, repeat: bool) -> Option<Action> {
        self.bindings
            .iter()
            .find(|(b, _)| b.key == key && b.mods == mods && (!repeat || b.repeat))
            .map(|(_, a)| *a)
    }

    /// Human-readable chord for an action (help overlay, tooltips).
    pub fn chord_for(&self, action: Action) -> Option<String> {
        let (b, _) = self.bindings.iter().find(|(_, a)| *a == action)?;
        Some(b.chord())
    }
}

impl Binding {
    /// Human-readable chord, e.g. `Shift+D`.
    pub fn chord(&self) -> String {
        let b = self;
        let mut s = String::new();
        if b.mods.ctrl {
            s.push_str("Ctrl+");
        }
        if b.mods.alt {
            s.push_str("Alt+");
        }
        if b.mods.shift {
            s.push_str("Shift+");
        }
        s.push_str(&match b.key {
            Key::Space => "Space".into(),
            // (In words: the app's text font has no arrows, which showed as empty boxes.)
            Key::ArrowLeft => "Left".into(),
            Key::ArrowRight => "Right".into(),
            Key::ArrowUp => "Up".into(),
            Key::ArrowDown => "Down".into(),
            Key::Home => "Home".into(),
            Key::End => "End".into(),
            Key::Escape => "Esc".into(),
            Key::Tab => "Tab".into(),
            Key::Delete => "Delete".into(),
            Key::F2 => "F2".into(),
            Key::Letter(c) => c.to_ascii_uppercase().to_string(),
        });
        s
    }
}

/// Keys and modifiers held down right now (the host refreshes it every app frame).
#[derive(Resource, Debug, Default, Clone)]
pub struct KeysHeld {
    pub keys: Vec<Key>,
    pub mods: Mods,
}

impl KeysHeld {
    pub fn contains(&self, key: Key) -> bool {
        self.keys.contains(&key)
    }
}

/// Actions queued this frame, drained by the modules that own them.
#[derive(Resource, Debug, Default)]
pub struct PendingActions(pub Vec<Action>);

impl PendingActions {
    pub fn push(&mut self, action: Action) {
        self.0.push(action);
    }

    /// Remove and return the actions matching `pred`, keeping the rest queued.
    pub fn take(&mut self, pred: impl Fn(Action) -> bool) -> Vec<Action> {
        let (taken, kept): (Vec<_>, Vec<_>) = self.0.drain(..).partition(|a| pred(*a));
        self.0 = kept;
        taken
    }
}

pub struct InputModule;

impl Module for InputModule {
    fn build(&self, app: &mut AppBuilder) {
        app.declare::<Keymap>(crate::Class::Session)
            .declare::<KeysHeld>(crate::Class::Derived)
            .declare::<PendingActions>(crate::Class::Derived)
            .init_resource::<Keymap>()
            .init_resource::<KeysHeld>()
            .init_resource::<PendingActions>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeat_only_fires_repeatable_bindings() {
        let k = Keymap::default();
        assert_eq!(k.lookup(Key::ArrowRight, Mods::NONE, true), Some(Action::StepForward));
        assert_eq!(k.lookup(Key::Space, Mods::NONE, false), Some(Action::TogglePlay));
        assert_eq!(k.lookup(Key::Space, Mods::NONE, true), None);
        assert_eq!(k.lookup(Key::ArrowRight, Mods::SHIFT, false), Some(Action::GoToEnd));
        assert_eq!(k.lookup(Key::Letter('q'), Mods::NONE, false), Some(Action::SlowerPlayback));
        assert_eq!(k.lookup(Key::Letter('e'), Mods::NONE, false), Some(Action::FasterPlayback));
        assert_eq!(k.chord_for(Action::FasterPlayback).as_deref(), Some("E"), "E is the one shown");
    }

    #[test]
    fn take_partitions_queue() {
        let mut q = PendingActions::default();
        q.push(Action::TogglePlay);
        q.push(Action::FrameAll);
        let t = q.take(|a| a == Action::TogglePlay);
        assert_eq!(t, vec![Action::TogglePlay]);
        assert_eq!(q.0, vec![Action::FrameAll]);
    }
}
