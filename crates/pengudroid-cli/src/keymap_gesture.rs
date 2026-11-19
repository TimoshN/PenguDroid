//! Native multi-pointer touch state & Keyboard/Mouse <-> Xbox 360 Gamepad state.
//!
//! No touch gestures are synthesized from keys anymore: [`GamepadState`]
//! translates Keyboard/Mouse <-> Xbox 360 Controller controls natively, while
//! [`TouchState`] folds real kernel/pointer multitouch events into Android
//! `MotionEvent` pointer frames.

use std::collections::{HashMap, HashSet};

use pengudroid_input::keymap::{Action, GamepadButton, Keymap};
use pengudroid_input::uinput::GamepadHostEvent;

// ---------------------------------------------------------------------------
// Native multi-pointer touch frame accumulator
// ---------------------------------------------------------------------------

pub const MOUSE_POINTER: i32 = 0;

/// Android MotionEvent pointer IDs are limited to 0..=31. ID 0 is reserved for
/// the compositor/mouse pointer; every hardware (device, slot) pair receives a
/// stable ID from the remaining range until its ACTION_UP.
#[derive(Default)]
pub struct KernelPointerIds {
    assigned: HashMap<(usize, i32), i32>,
}

impl KernelPointerIds {
    pub fn activate(&mut self, reader: usize, slot: i32) -> Option<i32> {
        let key = (reader, slot);
        if let Some(id) = self.assigned.get(&key) {
            return Some(*id);
        }
        let id = (1..=31).find(|id| !self.assigned.values().any(|used| used == id))?;
        self.assigned.insert(key, id);
        Some(id)
    }

    pub fn get(&self, reader: usize, slot: i32) -> Option<i32> {
        self.assigned.get(&(reader, slot)).copied()
    }

    pub fn release(&mut self, reader: usize, slot: i32) {
        self.assigned.remove(&(reader, slot));
    }
}

/// Single finger event from the host pointer or kernel evdev touchscreen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TouchEvent {
    Down { id: i32, x: f32, y: f32 },
    Move { id: i32, x: f32, y: f32 },
    Up { id: i32, x: f32, y: f32 },
}

/// Position of one active finger inside a [`Frame`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pointer {
    pub id: i32,
    pub x: f32,
    pub y: f32,
}

/// Complete `MotionEvent` frame to hand to `MultiTouchJni`.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub action: i32,
    pub starts_gesture: bool,
    pub pointers: Vec<Pointer>,
}

const ACTION_DOWN: i32 = 0;
const ACTION_UP: i32 = 1;
const ACTION_MOVE: i32 = 2;
const ACTION_POINTER_DOWN: i32 = 5;
const ACTION_POINTER_UP: i32 = 6;
const ACTION_POINTER_INDEX_SHIFT: i32 = 8;

/// Multi-pointer touch state machine for active kernel/host fingers.
#[derive(Default)]
pub struct TouchState {
    active: Vec<Pointer>,
}

impl TouchState {
    pub fn is_idle(&self) -> bool {
        self.active.is_empty()
    }

    pub fn apply(&mut self, event: TouchEvent) -> Option<Frame> {
        match event {
            TouchEvent::Down { id, x, y } => {
                if let Some(existing) = self.active.iter_mut().find(|p| p.id == id) {
                    existing.x = x;
                    existing.y = y;
                    return Some(Frame {
                        action: ACTION_MOVE,
                        starts_gesture: false,
                        pointers: self.active.clone(),
                    });
                }
                let idx = self.active.len();
                self.active.push(Pointer { id, x, y });
                let (starts_gesture, action) = if idx == 0 {
                    (true, ACTION_DOWN)
                } else {
                    (false, ACTION_POINTER_DOWN | ((idx as i32) << ACTION_POINTER_INDEX_SHIFT))
                };
                Some(Frame {
                    action,
                    starts_gesture,
                    pointers: self.active.clone(),
                })
            }
            TouchEvent::Move { id, x, y } => {
                let p = self.active.iter_mut().find(|p| p.id == id)?;
                p.x = x;
                p.y = y;
                Some(Frame {
                    action: ACTION_MOVE,
                    starts_gesture: false,
                    pointers: self.active.clone(),
                })
            }
            TouchEvent::Up { id, x, y } => {
                let idx = self.active.iter().position(|p| p.id == id)?;
                self.active[idx].x = x;
                self.active[idx].y = y;
                let pointers = self.active.clone();
                let action = if self.active.len() == 1 {
                    ACTION_UP
                } else {
                    ACTION_POINTER_UP | ((idx as i32) << ACTION_POINTER_INDEX_SHIFT)
                };
                self.active.remove(idx);
                Some(Frame {
                    action,
                    starts_gesture: false,
                    pointers,
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Keyboard/Mouse <-> Xbox 360 Controller state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct GamepadAxes {
    pub lx: f32,
    pub ly: f32,
    pub rx: f32,
    pub ry: f32,
    pub lt: f32,
    pub rt: f32,
    pub hat_x: f32,
    pub hat_y: f32,
}

#[derive(Debug, Default)]
pub struct GamepadOutput {
    pub pad_buttons: Vec<(GamepadButton, bool)>,
    pub key_events: Vec<(u16, bool)>,
    pub axes: GamepadAxes,
    pub axes_changed: bool,
    pub look_active_changed: Option<bool>,
}

/// Combines keyboard/mouse keymap bindings + mouse-look + optional physical
/// controller state into clean Xbox 360 button edges and normalized axes.
#[derive(Default)]
pub struct GamepadState {
    held: HashMap<u16, Action>,
    physical_axes: GamepadAxes,
    physical_down: HashSet<GamepadButton>,
    virtual_down: HashSet<GamepadButton>,
    look_active: bool,
    look_sensitivity: f32,
    look_rx: f32,
    look_ry: f32,
}

fn normalize_stick(mut x: f32, mut y: f32) -> (f32, f32) {
    let len = (x * x + y * y).sqrt();
    if len > 1.0 {
        x /= len;
        y /= len;
    }
    (x.clamp(-1.0, 1.0), y.clamp(-1.0, 1.0))
}

impl GamepadState {
    pub fn look_active(&self) -> bool {
        self.look_active
    }

    pub fn look_nonzero(&self) -> bool {
        self.look_rx.abs() > 0.005 || self.look_ry.abs() > 0.005
    }

    pub fn is_down(&self, code: u16) -> bool {
        self.held.contains_key(&code)
    }

    pub fn axes(&self) -> GamepadAxes {
        self.compute_axes()
    }

    fn compute_axes(&self) -> GamepadAxes {
        let mut lx = self.physical_axes.lx;
        let mut ly = self.physical_axes.ly;
        let mut rx = self.physical_axes.rx + self.look_rx;
        let mut ry = self.physical_axes.ry + self.look_ry;
        let mut hat_x = self.physical_axes.hat_x;
        let mut hat_y = self.physical_axes.hat_y;
        let mut lt = self.physical_axes.lt;
        let mut rt = self.physical_axes.rt;

        for action in self.held.values() {
            match *action {
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
                    hat_x += dx;
                    hat_y += dy;
                }
                Action::Pad(GamepadButton::Tl2) => lt = 1.0,
                Action::Pad(GamepadButton::Tr2) => rt = 1.0,
                _ => {}
            }
        }
        let (lx, ly) = normalize_stick(lx, ly);
        let (rx, ry) = normalize_stick(rx, ry);
        GamepadAxes {
            lx,
            ly,
            rx,
            ry,
            lt: lt.clamp(0.0, 1.0),
            rt: rt.clamp(0.0, 1.0),
            hat_x: hat_x.clamp(-1.0, 1.0),
            hat_y: hat_y.clamp(-1.0, 1.0),
        }
    }

    fn sync_virtual_buttons(&mut self, out: &mut GamepadOutput) {
        let mut want = self.physical_down.clone();
        for action in self.held.values() {
            if let Action::Pad(btn) = *action {
                want.insert(btn);
            }
        }
        for &btn in want.difference(&self.virtual_down) {
            out.pad_buttons.push((btn, true));
        }
        for &btn in self.virtual_down.difference(&want) {
            out.pad_buttons.push((btn, false));
        }
        self.virtual_down = want;
    }

    pub fn handle_key(&mut self, action: &Action, code: u16, pressed: bool) -> GamepadOutput {
        let mut out = GamepadOutput::default();
        let before = self.compute_axes();

        if pressed {
            if self.held.contains_key(&code) {
                return out;
            }
            if let Action::MouseLook { sensitivity } = *action {
                self.held.insert(code, action.clone());
                self.look_active = !self.look_active;
                self.look_sensitivity = sensitivity;
                if !self.look_active {
                    self.look_rx = 0.0;
                    self.look_ry = 0.0;
                }
                out.look_active_changed = Some(self.look_active);
            } else {
                self.held.insert(code, action.clone());
                if let Action::Key(target) = *action {
                    out.key_events.push((target, true));
                }
            }
        } else if let Some(prev) = self.held.remove(&code) {
            if let Action::Key(target) = prev {
                out.key_events.push((target, false));
            }
        }

        self.sync_virtual_buttons(&mut out);
        out.axes = self.compute_axes();
        out.axes_changed = out.axes != before;
        out
    }

    pub fn handle_look_motion(&mut self, dx: f32, dy: f32) -> GamepadOutput {
        let mut out = GamepadOutput::default();
        if !self.look_active {
            return out;
        }
        let before = self.compute_axes();
        // Relative mouse deltas in pixels -> right analog stick [-1, 1] deflection.
        let scale = 0.035 * self.look_sensitivity;
        self.look_rx = (dx * scale).clamp(-1.0, 1.0);
        self.look_ry = (dy * scale).clamp(-1.0, 1.0);
        out.axes = self.compute_axes();
        out.axes_changed = out.axes != before;
        out
    }

    pub fn decay_look(&mut self) -> GamepadOutput {
        let mut out = GamepadOutput::default();
        if !self.look_nonzero() {
            return out;
        }
        let before = self.compute_axes();
        self.look_rx = 0.0;
        self.look_ry = 0.0;
        out.axes = self.compute_axes();
        out.axes_changed = out.axes != before;
        out
    }

    pub fn cancel_look(&mut self) -> GamepadOutput {
        let mut out = GamepadOutput::default();
        let before = self.compute_axes();
        if self.look_active {
            self.look_active = false;
            out.look_active_changed = Some(false);
        }
        self.look_rx = 0.0;
        self.look_ry = 0.0;
        out.axes = self.compute_axes();
        out.axes_changed = out.axes != before;
        out
    }

    /// Integrate an event from a physical Linux gamepad (`/dev/input/eventN`).
    pub fn apply_physical_event(&mut self, keymap: Option<&Keymap>, ev: GamepadHostEvent) -> GamepadOutput {
        let mut out = GamepadOutput::default();
        let before = self.compute_axes();
        match ev {
            GamepadHostEvent::Button { code, pressed } => {
                if let Some(action) = keymap.and_then(|m| m.action_for(code)) {
                    return self.handle_key(action, code, pressed);
                }
                if let Some(btn) = GamepadButton::from_linux_code(code) {
                    if pressed {
                        self.physical_down.insert(btn);
                    } else {
                        self.physical_down.remove(&btn);
                    }
                    self.sync_virtual_buttons(&mut out);
                }
            }
            GamepadHostEvent::LeftX(v) => self.physical_axes.lx = v,
            GamepadHostEvent::LeftY(v) => self.physical_axes.ly = v,
            GamepadHostEvent::RightX(v) => self.physical_axes.rx = v,
            GamepadHostEvent::RightY(v) => self.physical_axes.ry = v,
            GamepadHostEvent::LeftTrigger(v) => self.physical_axes.lt = v,
            GamepadHostEvent::RightTrigger(v) => self.physical_axes.rt = v,
            GamepadHostEvent::HatX(v) => self.physical_axes.hat_x = v,
            GamepadHostEvent::HatY(v) => self.physical_axes.hat_y = v,
        }
        out.axes = self.compute_axes();
        out.axes_changed = out.axes != before;
        out
    }
}

#[cfg(test)]
mod tests {
    use pengudroid_input::keymap::Dir;

    use super::*;

    #[test]
    fn wasd_normalizes_diagonals_to_unit_circle() {
        let mut pad = GamepadState::default();
        pad.handle_key(&Action::LeftStick(Dir::Up), 17, true);
        let out = pad.handle_key(&Action::LeftStick(Dir::Right), 32, true);
        assert!(out.axes_changed);
        let len = (out.axes.lx * out.axes.lx + out.axes.ly * out.axes.ly).sqrt();
        assert!((len - 1.0).abs() < 1e-4);
        assert!((out.axes.lx - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-4);
        assert!((out.axes.ly + std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-4);
    }

    #[test]
    fn gamepad_buttons_and_remapped_keys_emit_edges() {
        let mut pad = GamepadState::default();
        let out = pad.handle_key(&Action::Pad(GamepadButton::Tr2), 18, true);
        assert_eq!(out.pad_buttons, vec![(GamepadButton::Tr2, true)]);
        assert!((out.axes.rt - 1.0).abs() < 1e-5);
        let out = pad.handle_key(&Action::Pad(GamepadButton::Tr2), 18, false);
        assert_eq!(out.pad_buttons, vec![(GamepadButton::Tr2, false)]);
        assert!((out.axes.rt - 0.0).abs() < 1e-5);

        let map = Keymap::parse("PAD_A = key SPACE\n").unwrap();
        let ev = GamepadHostEvent::Button { code: 0x130, pressed: true };
        let out = pad.apply_physical_event(Some(&map), ev);
        assert_eq!(out.key_events, vec![(57, true)]);
        assert!(out.pad_buttons.is_empty());
    }

    #[test]
    fn mouselook_toggles_and_decays_right_stick() {
        let mut pad = GamepadState::default();
        let toggle = Action::MouseLook { sensitivity: 1.5 };
        let out = pad.handle_key(&toggle, 56, true);
        assert_eq!(out.look_active_changed, Some(true));
        pad.handle_key(&toggle, 56, false);

        let moved = pad.handle_look_motion(20.0, -10.0);
        assert!(moved.axes.rx > 0.5 && moved.axes.ry < -0.2);

        let decayed = pad.decay_look();
        assert_eq!(decayed.axes.rx, 0.0);
        assert_eq!(decayed.axes.ry, 0.0);
    }

    #[test]
    fn multi_touch_state_tracks_concurrent_kernel_fingers() {
        let mut ts = TouchState::default();
        let f1 = ts.apply(TouchEvent::Down { id: 0, x: 10.0, y: 20.0 }).unwrap();
        assert_eq!(f1.action, ACTION_DOWN);
        assert!(f1.starts_gesture);
        let f2 = ts.apply(TouchEvent::Down { id: 1, x: 50.0, y: 60.0 }).unwrap();
        assert_eq!(f2.action, ACTION_POINTER_DOWN | (1 << ACTION_POINTER_INDEX_SHIFT));
        assert!(!f2.starts_gesture);
        let f3 = ts.apply(TouchEvent::Up { id: 0, x: 10.0, y: 20.0 }).unwrap();
        assert_eq!(f3.action, ACTION_POINTER_UP);
        let f4 = ts.apply(TouchEvent::Up { id: 1, x: 50.0, y: 60.0 }).unwrap();
        assert_eq!(f4.action, ACTION_UP);
        assert!(ts.is_idle());
    }

    #[test]
    fn kernel_pointer_ids_stay_in_android_range_and_recycle() {
        let mut ids = KernelPointerIds::default();
        let first = ids.activate(0, 0).unwrap();
        let second = ids.activate(0, 1).unwrap();
        assert_eq!(first, 1);
        assert_eq!(second, 2);
        assert_eq!(ids.get(0, 1), Some(second));
        assert_eq!(ids.activate(1, 1), Some(3));
        ids.release(0, 1);
        assert_eq!(ids.get(0, 1), None);
        assert_eq!(ids.activate(0, 99), Some(second));
        for slot in 2..=29 {
            assert!(ids.activate(2, slot).is_some());
        }
        assert!((1..=31).all(|id| ids.assigned.values().any(|used| *used == id)));
        assert_eq!(ids.activate(3, 100), None);
    }
}
