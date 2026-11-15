//! The keymapper: physical keyboard/mouse <-> Xbox 360 gamepad via uinput.
//!
//! No touch synthesis occurs here; touch is handled universally at the native
//! kernel input layer. `KeyMapper` translates bound keyboard keys into Xbox 360
//! controller events (`VirtualGamepad`) and mapped gamepad buttons back into
//! keyboard keys (`VirtualKeyboard`).

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::keymap::{Action, Keymap};
use crate::uinput::{EventReader, VirtualGamepad, VirtualKeyboard};

/// A running keymapper.
pub struct KeyMapper {
    stop: Arc<AtomicBool>,
    /// Bindings can be replaced while the mapper runs (the GUI edits them).
    bindings: Arc<Mutex<BTreeMap<u16, Action>>>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

fn recompute_axes(
    bindings: &BTreeMap<u16, Action>,
    held: &HashSet<u16>,
    pad: &mut VirtualGamepad,
    buttons: &[(u16, bool)],
) {
    let (mut lx, mut ly) = (0.0f32, 0.0f32);
    let (mut rx, mut ry) = (0.0f32, 0.0f32);
    let (mut hx, mut hy) = (0.0f32, 0.0f32);
    let (mut lt, mut rt) = (0.0f32, 0.0f32);
    for &code in held {
        if let Some(action) = bindings.get(&code) {
            match action {
                Action::LeftStick(dir) => {
                    let (dx, dy) = dir.vector();
                    lx += dx;
                    ly += dy;
                }
                Action::RightStick(dir) => {
                    let (dx, dy) = dir.vector();
                    rx += dx;
                    ry += dy;
                }
                Action::Dpad(dir) => {
                    let (dx, dy) = dir.vector();
                    hx += dx;
                    hy += dy;
                }
                Action::Pad(btn) if matches!(btn, crate::keymap::GamepadButton::Tl2) => lt = 1.0,
                Action::Pad(btn) if matches!(btn, crate::keymap::GamepadButton::Tr2) => rt = 1.0,
                _ => {}
            }
        }
    }
    let _ = pad.emit_state(buttons, lx, ly, rx, ry, lt, rt, hx, hy);
}

impl KeyMapper {
    /// Start mapping physical keyboards (`devices`) to `gamepad` (and optional `keyboard`).
    pub fn start(
        devices: Vec<PathBuf>,
        gamepad: Arc<Mutex<VirtualGamepad>>,
        keyboard: Option<Arc<Mutex<VirtualKeyboard>>>,
        keymap: &Keymap,
    ) -> KeyMapper {
        let stop = Arc::new(AtomicBool::new(false));
        let bindings = Arc::new(Mutex::new(crate::keymap::index(keymap)));
        let held_codes = Arc::new(Mutex::new(HashSet::<u16>::new()));
        let mut threads = Vec::new();
        for path in devices {
            let stop = Arc::clone(&stop);
            let bindings = Arc::clone(&bindings);
            let gamepad = Arc::clone(&gamepad);
            let keyboard = keyboard.clone();
            let held_codes = Arc::clone(&held_codes);
            threads.push(std::thread::spawn(move || {
                let Ok(mut reader) = EventReader::open(&path) else {
                    return;
                };
                let mut owned = HashSet::<u16>::new();
                while !stop.load(Ordering::Relaxed) {
                    let (code, pressed) = match reader.read_key_timeout(std::time::Duration::from_millis(25)) {
                        Ok(Some(key)) => key,
                        Ok(None) => continue,
                        Err(_) => break,
                    };
                    let map_snapshot = bindings.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    let Some(action) = map_snapshot.get(&code).cloned() else {
                        continue;
                    };
                    let mut all_held = held_codes.lock().unwrap_or_else(|e| e.into_inner());
                    if pressed {
                        owned.insert(code);
                        all_held.insert(code);
                    } else {
                        owned.remove(&code);
                        all_held.remove(&code);
                    }
                    match action {
                        Action::Pad(btn) => {
                            let mut pad = gamepad.lock().unwrap_or_else(|e| e.into_inner());
                            recompute_axes(
                                &map_snapshot,
                                &all_held,
                                &mut pad,
                                &[(btn.linux_code(), pressed)],
                            );
                        }
                        Action::LeftStick(_) | Action::RightStick(_) | Action::Dpad(_) => {
                            let mut pad = gamepad.lock().unwrap_or_else(|e| e.into_inner());
                            recompute_axes(&map_snapshot, &all_held, &mut pad, &[]);
                        }
                        Action::Key(target) => {
                            if let Some(kb) = keyboard.as_ref() {
                                let _ = kb.lock().unwrap_or_else(|e| e.into_inner()).key(target, pressed);
                            }
                        }
                        Action::MouseLook { .. } => {}
                    }
                }
                if !owned.is_empty() {
                    let map_snapshot = bindings.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    let mut all_held = held_codes.lock().unwrap_or_else(|e| e.into_inner());
                    let mut rel_buttons = Vec::new();
                    for code in owned {
                        all_held.remove(&code);
                        if let Some(Action::Pad(btn)) = map_snapshot.get(&code) {
                            rel_buttons.push((btn.linux_code(), false));
                        } else if let Some(Action::Key(target)) = map_snapshot.get(&code) {
                            if let Some(kb) = keyboard.as_ref() {
                                let _ = kb.lock().unwrap_or_else(|e| e.into_inner()).key(*target, false);
                            }
                        }
                    }
                    let mut pad = gamepad.lock().unwrap_or_else(|e| e.into_inner());
                    recompute_axes(&map_snapshot, &all_held, &mut pad, &rel_buttons);
                }
            }));
        }
        KeyMapper {
            stop,
            bindings,
            threads,
        }
    }

    /// How many keys are currently bound (for status messages).
    pub fn binding_count(&self) -> usize {
        self.bindings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Replace the bindings without restarting the threads.
    pub fn set_keymap(&self, keymap: &Keymap) {
        *self.bindings.lock().unwrap_or_else(|e| e.into_inner()) = crate::keymap::index(keymap);
    }

    /// Stop every thread and wait for them.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bindings_can_be_replaced_while_running() {
        let keymap = Keymap::parse("W = lstick up\n").expect("parse");
        let bindings = Arc::new(Mutex::new(crate::keymap::index(&keymap)));
        assert!(bindings.lock().unwrap().contains_key(&17));
        let next = Keymap::parse("A = lstick left\n").expect("parse");
        *bindings.lock().unwrap() = crate::keymap::index(&next);
        assert!(!bindings.lock().unwrap().contains_key(&17));
        assert!(bindings.lock().unwrap().contains_key(&30));
    }
}
