//! Actions and the keymap (DESIGN §14): keys are data, not code.
//!
//! The host (tt_app) translates raw key presses into [`Action`]s through the
//! [`Keymap`] resource and queues them in [`PendingActions`]. Modules apply the
//! actions they own in [`Set::Intents`](crate::Set::Intents). Nothing reads
//! keys directly, so rebinding never touches feature code.

use bevy_ecs::prelude::*;

use crate::app::{AppBuilder, Module};
use crate::time::FrameIndex;

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
    FrameAll,
    OpenFile,
    Undo,
    Redo,
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
                (b(Key::Letter(']'), Mods::NONE, false), FasterPlayback),
                (b(Key::Letter('['), Mods::NONE, false), SlowerPlayback),
                (b(Key::Letter('o'), Mods::CTRL, false), OpenFile),
                (b(Key::Letter('z'), Mods::CTRL, true), Undo),
                (b(Key::Letter('z'), Mods { ctrl: true, shift: true, alt: false }, true), Redo),
                (b(Key::Letter('y'), Mods::CTRL, true), Redo),
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
            Key::ArrowLeft => "←".into(),
            Key::ArrowRight => "→".into(),
            Key::ArrowUp => "↑".into(),
            Key::ArrowDown => "↓".into(),
            Key::Home => "Home".into(),
            Key::End => "End".into(),
            Key::Letter(c) => c.to_ascii_uppercase().to_string(),
        });
        Some(s)
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
        app.init_resource::<Keymap>().init_resource::<PendingActions>();
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
