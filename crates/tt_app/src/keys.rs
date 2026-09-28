//! egui key events → toolkit-independent keys → actions via the world's Keymap.

use tt_core::input::{Action, Key, Keymap, Mods};

pub fn actions(input: &egui::InputState, keymap: &Keymap) -> Vec<Action> {
    let mut out = Vec::new();
    for event in &input.events {
        let egui::Event::Key { key, pressed: true, repeat, modifiers, .. } = event else { continue };
        let Some(k) = map_key(*key) else { continue };
        let mods = Mods { ctrl: modifiers.ctrl || modifiers.command, shift: modifiers.shift, alt: modifiers.alt };
        if let Some(action) = keymap.lookup(k, mods, *repeat) {
            out.push(action);
        }
    }
    out
}

fn map_key(key: egui::Key) -> Option<Key> {
    use egui::Key as E;
    Some(match key {
        E::Space => Key::Space,
        E::ArrowLeft => Key::ArrowLeft,
        E::ArrowRight => Key::ArrowRight,
        E::ArrowUp => Key::ArrowUp,
        E::ArrowDown => Key::ArrowDown,
        E::Home => Key::Home,
        E::End => Key::End,
        E::OpenBracket => Key::Letter('['),
        E::CloseBracket => Key::Letter(']'),
        other => {
            let mut chars = other.name().chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None; // named keys we don't bind (F1, Tab, …)
            }
            Key::Letter(c.to_ascii_lowercase())
        }
    })
}
