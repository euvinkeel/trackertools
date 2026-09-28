//! egui key events → toolkit-independent keys → actions via the world's Keymap.

use tt_core::input::{Action, Key, Keymap, KeysHeld, Mods};

pub fn actions(events: &[egui::Event], keymap: &Keymap) -> Vec<Action> {
    let mut out = Vec::new();
    for event in events {
        let egui::Event::Key { key, pressed: true, repeat, modifiers, .. } = event else { continue };
        let Some(k) = map_key(*key) else { continue };
        let mods = Mods { ctrl: modifiers.ctrl || modifiers.command, shift: modifiers.shift, alt: modifiers.alt };
        if let Some(action) = keymap.lookup(k, mods, *repeat) {
            out.push(action);
        }
    }
    out
}

/// egui walks keyboard focus through widgets with Tab (and a focused button
/// then also takes Space/Enter). This app is driven by its own keymap, so Tab
/// is taken out of egui's input and kept for the keymap, except while a text
/// field has focus (Tab moves between fields there).
pub fn take_tab(ctx: &egui::Context, raw: &mut egui::RawInput, taken: &mut Vec<egui::Event>) {
    if ctx.egui_wants_keyboard_input() {
        return;
    }
    raw.events.retain(|e| {
        let tab = matches!(e, egui::Event::Key { key: egui::Key::Tab, .. });
        if tab {
            taken.push(e.clone());
        }
        !tab
    });
}

/// Keys and modifiers held right now.
pub fn held(input: &egui::InputState) -> KeysHeld {
    let m = input.modifiers;
    KeysHeld {
        keys: input.keys_down.iter().filter_map(|k| map_key(*k)).collect(),
        mods: Mods { ctrl: m.ctrl || m.command, shift: m.shift, alt: m.alt },
    }
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
        E::Escape => Key::Escape,
        E::Tab => Key::Tab,
        E::Delete => Key::Delete,
        E::F2 => Key::F2,
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
