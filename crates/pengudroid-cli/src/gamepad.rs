//! Game controllers: read them from the kernel and hand them to the app.
//!
//! Wayland has no gamepad protocol, so controllers are read straight from
//! their `evdev` nodes (`/dev/input/event*`, which logind makes readable for
//! the seat's user). Every controller is read; none is grabbed, so the desktop
//! and other programs keep seeing it too.
//!
//! What a control does is decided per app:
//!
//! * a control bound in the app's keymap (`PAD_A = tap 1100 600`,
//!   `PAD_LS_UP = stick 230 520 110 up`, `PAD_B = key ESC`,
//!   `PAD_LS = pointer 14`) becomes that touch gesture, keyboard key or mouse
//!   action — this is how a touch-only game, or a desktop-style app, is played
//!   with a controller;
//! * an unbound control reaches the app as a real Android controller:
//!   `KEYCODE_BUTTON_*`/`KEYCODE_DPAD_*` key events and `SOURCE_JOYSTICK`
//!   motion with `AXIS_X/Y/Z/RZ/HAT_*/LTRIGGER/RTRIGGER`, which is what games
//!   with native controller support (Unity, Unreal, SDL) listen for.
//!
//! `PENGUDROID_GAMEPAD=off` disables controllers; `=mouse` uses the built-in
//! desktop layout below for apps without pad bindings of their own.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;

use pengudroid_input::keymap::{Action, Keymap};

// Linux `BTN_*` codes; the keymap names them `PAD_*`.
pub const PAD_A: u16 = 0x130;
pub const PAD_B: u16 = 0x131;
pub const PAD_X: u16 = 0x133;
pub const PAD_Y: u16 = 0x134;
pub const PAD_LB: u16 = 0x136;
pub const PAD_RB: u16 = 0x137;
pub const PAD_LT: u16 = 0x138;
pub const PAD_RT: u16 = 0x139;
pub const PAD_SELECT: u16 = 0x13a;
pub const PAD_START: u16 = 0x13b;
pub const PAD_HOME: u16 = 0x13c;
pub const PAD_L3: u16 = 0x13d;
pub const PAD_R3: u16 = 0x13e;
pub const PAD_UP: u16 = 0x220;
pub const PAD_DOWN: u16 = 0x221;
pub const PAD_LEFT: u16 = 0x222;
pub const PAD_RIGHT: u16 = 0x223;
pub use pengudroid_input::keymap::{PAD_LS, PAD_LS_UP, PAD_RS, PAD_RS_UP};

const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_Z: u16 = 0x02;
const ABS_RX: u16 = 0x03;
const ABS_RY: u16 = 0x04;
const ABS_RZ: u16 = 0x05;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;

/// Stick deflection below this is rest.
const DEADZONE: f32 = 0.18;
/// A stick direction or trigger counts as pressed above this, and released
/// again below [`RELEASE`] (hysteresis, so a resting thumb does not chatter).
const PRESS: f32 = 0.5;
const RELEASE: f32 = 0.35;

/// One raw controller event, axis values already normalised.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PadEvent {
    Button { code: u16, pressed: bool },
    /// Sticks in `-1..=1` (deadzone applied), triggers in `0..=1`, hats in
    /// `-1`, `0`, `1`.
    Axis { code: u16, value: f32 },
    /// A controller was unplugged: release everything it held.
    Gone,
}

/// What the input pump does for a controller event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Out {
    /// A Linux key (or a bound `PAD_*` code) through the keyboard path, so
    /// keyboard bindings and text input apply to it as to a real key.
    Key { code: u16, pressed: bool },
    /// A mouse button at the current cursor.
    MouseButton { code: u16, pressed: bool },
    /// Move the cursor (hover, or drag while the left button is down).
    Cursor { x: f32, y: f32 },
    /// An Android `KEYCODE_*` from a gamepad (`SOURCE_GAMEPAD`).
    AndroidKey { keycode: i32, pressed: bool },
    /// The joystick axes changed: send one `SOURCE_JOYSTICK` motion event
    /// with [`PadMapper::android_axes`].
    Motion,
}

/// Android keycode for an unbound button.
pub fn android_keycode(code: u16) -> Option<i32> {
    Some(match code {
        PAD_A => 96,
        PAD_B => 97,
        PAD_X => 99,
        PAD_Y => 100,
        PAD_LB => 102,
        PAD_RB => 103,
        PAD_LT => 104,
        PAD_RT => 105,
        PAD_L3 => 106,
        PAD_R3 => 107,
        PAD_START => 108,
        PAD_SELECT => 109,
        PAD_HOME => 110,
        PAD_UP => 19,
        PAD_DOWN => 20,
        PAD_LEFT => 21,
        PAD_RIGHT => 22,
        _ => return None,
    })
}

/// The built-in layout for `PENGUDROID_GAMEPAD=mouse`: the left stick moves a
/// cursor, A clicks, B is Back, the d-pad is the arrow keys.
pub fn desktop_layout() -> Keymap {
    Keymap::parse(
        "PAD_LS = pointer 14\n\
         PAD_A = key MOUSE_LEFT\n\
         PAD_X = key MOUSE_RIGHT\n\
         PAD_B = key ESC\n\
         PAD_START = key ENTER\n\
         PAD_SELECT = key TAB\n\
         PAD_UP = key UP\n\
         PAD_DOWN = key DOWN\n\
         PAD_LEFT = key LEFT\n\
         PAD_RIGHT = key RIGHT\n\
         PAD_LB = key PAGEUP\n\
         PAD_RB = key PAGEDOWN\n",
    )
    .expect("built-in gamepad layout parses")
}

/// Turns controller events into pump actions for one app.
pub struct PadMapper {
    keymap: Option<Keymap>,
    /// Raw axis state, normalised.
    axes: HashMap<u16, f32>,
    /// Buttons currently down, including the synthetic ones (d-pad from the
    /// hat, trigger and stick directions), so every press gets one release.
    down: HashSet<u16>,
    cursor: (f32, f32),
    bounds: (f32, f32),
}

impl PadMapper {
    pub fn new(keymap: Option<Keymap>, width: f32, height: f32) -> PadMapper {
        PadMapper {
            keymap,
            axes: HashMap::new(),
            down: HashSet::new(),
            cursor: (width / 2.0, height / 2.0),
            bounds: (width, height),
        }
    }

    fn action(&self, code: u16) -> Option<&Action> {
        self.keymap.as_ref().and_then(|m| m.action_for(code))
    }

    /// A button edge (real, or synthesised from an axis).
    fn button(&mut self, code: u16, pressed: bool, out: &mut Vec<Out>) {
        if pressed == self.down.contains(&code) {
            return;
        }
        if pressed {
            self.down.insert(code);
        } else {
            self.down.remove(&code);
        }
        match self.action(code) {
            Some(Action::Key { code: key }) => {
                let key = *key;
                if is_mouse_button(key) {
                    // The click lands where the controller's cursor is, even
                    // before the stick has moved it.
                    out.push(Out::Cursor { x: self.cursor.0, y: self.cursor.1 });
                    out.push(Out::MouseButton { code: key, pressed });
                } else {
                    out.push(Out::Key { code: key, pressed });
                }
            }
            // `pointer` belongs to a whole stick; on a button it does nothing.
            Some(Action::Pointer { .. }) => {}
            // A touch gesture: the keyboard path runs the binding for it.
            Some(_) => out.push(Out::Key { code, pressed }),
            None => {
                // Stick directions without a binding are not buttons on a
                // real controller; the joystick motion already carries them.
                if let Some(keycode) = android_keycode(code) {
                    out.push(Out::AndroidKey { keycode, pressed });
                }
            }
        }
    }

    pub fn event(&mut self, event: PadEvent) -> Vec<Out> {
        let mut out = Vec::new();
        match event {
            PadEvent::Button { code, pressed } => self.button(code, pressed, &mut out),
            PadEvent::Gone => {
                let held: Vec<u16> = self.down.iter().copied().collect();
                for code in held {
                    self.button(code, false, &mut out);
                }
                self.axes.clear();
                out.push(Out::Motion);
            }
            PadEvent::Axis { code, value } => {
                let previous = self.axes.insert(code, value).unwrap_or(0.0);
                if previous == value {
                    return out;
                }
                match code {
                    ABS_HAT0X => {
                        self.button(PAD_LEFT, value < 0.0, &mut out);
                        self.button(PAD_RIGHT, value > 0.0, &mut out);
                    }
                    ABS_HAT0Y => {
                        self.button(PAD_UP, value < 0.0, &mut out);
                        self.button(PAD_DOWN, value > 0.0, &mut out);
                    }
                    ABS_Z => self.threshold(PAD_LT, value, &mut out),
                    ABS_RZ => self.threshold(PAD_RT, value, &mut out),
                    ABS_X | ABS_Y => self.stick_dirs(PAD_LS_UP, ABS_X, ABS_Y, &mut out),
                    ABS_RX | ABS_RY => self.stick_dirs(PAD_RS_UP, ABS_RX, ABS_RY, &mut out),
                    _ => {}
                }
                // Controls the keymap does not take over reach the app as
                // joystick motion.
                let owned = match code {
                    ABS_X | ABS_Y => self.stick_bound(PAD_LS, PAD_LS_UP),
                    ABS_RX | ABS_RY => self.stick_bound(PAD_RS, PAD_RS_UP),
                    ABS_Z => self.action(PAD_LT).is_some(),
                    ABS_RZ => self.action(PAD_RT).is_some(),
                    ABS_HAT0X => self.action(PAD_LEFT).is_some() || self.action(PAD_RIGHT).is_some(),
                    ABS_HAT0Y => self.action(PAD_UP).is_some() || self.action(PAD_DOWN).is_some(),
                    _ => false,
                };
                if !owned {
                    out.push(Out::Motion);
                }
            }
        }
        out
    }

    fn stick_bound(&self, whole: u16, first_dir: u16) -> bool {
        self.action(whole).is_some() || (first_dir..first_dir + 4).any(|c| self.action(c).is_some())
    }

    fn threshold(&mut self, code: u16, value: f32, out: &mut Vec<Out>) {
        let pressed = if self.down.contains(&code) { value > RELEASE } else { value > PRESS };
        self.button(code, pressed, out);
    }

    /// Bound stick directions (`PAD_LS_UP`... in up/down/left/right order)
    /// press and release like keys, so a `stick` binding walks.
    fn stick_dirs(&mut self, first: u16, x_axis: u16, y_axis: u16, out: &mut Vec<Out>) {
        let x = self.axes.get(&x_axis).copied().unwrap_or(0.0);
        let y = self.axes.get(&y_axis).copied().unwrap_or(0.0);
        for (offset, value) in [(0u16, -y), (1, y), (2, -x), (3, x)] {
            let code = first + offset;
            if self.action(code).is_some() {
                self.threshold(code, value, out);
            }
        }
    }

    /// Called about every 16 ms: a stick bound to `pointer` moves the cursor.
    pub fn tick(&mut self, dt_ms: f32) -> Vec<Out> {
        let mut out = Vec::new();
        for (whole, x_axis, y_axis) in [(PAD_LS, ABS_X, ABS_Y), (PAD_RS, ABS_RX, ABS_RY)] {
            let Some(Action::Pointer { speed }) = self.action(whole).cloned() else { continue };
            let x = self.axes.get(&x_axis).copied().unwrap_or(0.0);
            let y = self.axes.get(&y_axis).copied().unwrap_or(0.0);
            if x == 0.0 && y == 0.0 {
                continue;
            }
            // Quadratic response: fine aim near the centre, speed at the rim.
            let scale = speed * dt_ms / 16.0;
            let (dx, dy) = (x * x.abs() * scale, y * y.abs() * scale);
            self.cursor.0 = (self.cursor.0 + dx).clamp(0.0, self.bounds.0 - 1.0);
            self.cursor.1 = (self.cursor.1 + dy).clamp(0.0, self.bounds.1 - 1.0);
            out.push(Out::Cursor { x: self.cursor.0, y: self.cursor.1 });
        }
        out
    }

    pub fn cursor(&self) -> (f32, f32) {
        self.cursor
    }

    /// `(AXIS_*, value)` for the joystick motion event.
    pub fn android_axes(&self) -> Vec<(i32, f32)> {
        let get = |code| self.axes.get(&code).copied().unwrap_or(0.0);
        vec![
            (0, get(ABS_X)),       // AXIS_X
            (1, get(ABS_Y)),       // AXIS_Y
            (11, get(ABS_RX)),     // AXIS_Z
            (14, get(ABS_RY)),     // AXIS_RZ
            (15, get(ABS_HAT0X)),  // AXIS_HAT_X
            (16, get(ABS_HAT0Y)),  // AXIS_HAT_Y
            (17, get(ABS_Z)),      // AXIS_LTRIGGER
            (18, get(ABS_RZ)),     // AXIS_RTRIGGER
            (23, get(ABS_Z)),      // AXIS_BRAKE
            (22, get(ABS_RZ)),     // AXIS_GAS
        ]
    }
}

fn is_mouse_button(code: u16) -> bool {
    (0x110..=0x117).contains(&code)
}

// ---------------------------------------------------------------------------
// evdev
// ---------------------------------------------------------------------------

/// `struct input_absinfo`.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct AbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

const fn ioc_read(nr: u32, size: u32) -> libc::c_ulong {
    ((2u32 << 30) | (size << 16) | (0x45 << 8) | nr) as libc::c_ulong
}

fn test_bit(bits: &[u8], bit: usize) -> bool {
    bits.get(bit / 8).is_some_and(|b| b & (1 << (bit % 8)) != 0)
}

/// Whether the open node is a game controller (gamepad buttons and a stick).
fn is_gamepad(fd: i32) -> bool {
    let mut keys = [0u8; 0x300 / 8];
    let mut abs = [0u8; 8];
    // SAFETY: the buffers are as large as the lengths encoded in the requests.
    unsafe {
        if libc::ioctl(fd, ioc_read(0x20 + EV_KEY as u32, keys.len() as u32), keys.as_mut_ptr()) < 0
            || libc::ioctl(fd, ioc_read(0x20 + EV_ABS as u32, abs.len() as u32), abs.as_mut_ptr()) < 0
        {
            return false;
        }
    }
    (test_bit(&keys, PAD_A as usize) || test_bit(&keys, 0x120 /* BTN_TRIGGER */))
        && test_bit(&abs, ABS_X as usize)
}

fn abs_info(fd: i32, code: u16) -> Option<AbsInfo> {
    let mut info = AbsInfo::default();
    // SAFETY: `info` is a `struct input_absinfo`, the size the request names.
    let rc = unsafe {
        libc::ioctl(fd, ioc_read(0x40 + code as u32, std::mem::size_of::<AbsInfo>() as u32), &mut info)
    };
    (rc >= 0 && info.maximum > info.minimum).then_some(info)
}

fn normalise(code: u16, raw: i32, info: &AbsInfo) -> f32 {
    let (min, max) = (info.minimum as f32, info.maximum as f32);
    match code {
        ABS_HAT0X | ABS_HAT0Y => raw.signum() as f32,
        ABS_Z | ABS_RZ if info.minimum >= 0 => ((raw as f32 - min) / (max - min)).clamp(0.0, 1.0),
        _ => {
            let centre = (min + max) / 2.0;
            let v = ((raw as f32 - centre) / ((max - min) / 2.0)).clamp(-1.0, 1.0);
            if v.abs() < DEADZONE {
                0.0
            } else {
                // Rescale so the response starts at 0 just outside the deadzone.
                v.signum() * (v.abs() - DEADZONE) / (1.0 - DEADZONE)
            }
        }
    }
}

/// Watch `/dev/input` for controllers and stream their events to `tx`, with
/// hotplug (rescanned every two seconds). Returns immediately.
pub fn spawn_readers(tx: Sender<PadEvent>) {
    std::thread::Builder::new()
        .name("pd-gamepads".into())
        .spawn(move || {
            let open: std::sync::Arc<std::sync::Mutex<HashSet<String>>> = Default::default();
            loop {
                let Ok(dir) = std::fs::read_dir("/dev/input") else { return };
                for entry in dir.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if !name.starts_with("event") || open.lock().unwrap().contains(&name) {
                        continue;
                    }
                    let path = entry.path();
                    let Ok(file) = std::fs::File::open(&path) else { continue };
                    use std::os::fd::AsRawFd;
                    if !is_gamepad(file.as_raw_fd()) {
                        continue;
                    }
                    open.lock().unwrap().insert(name.clone());
                    let (tx, open) = (tx.clone(), open.clone());
                    let _ = std::thread::Builder::new().name(format!("pd-{name}")).spawn(move || {
                        read_device(file, &path, &tx);
                        let _ = tx.send(PadEvent::Gone);
                        open.lock().unwrap().remove(&name);
                    });
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        })
        .ok();
}

fn read_device(file: std::fs::File, path: &std::path::Path, tx: &Sender<PadEvent>) {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    let mut infos = HashMap::new();
    for code in [ABS_X, ABS_Y, ABS_Z, ABS_RX, ABS_RY, ABS_RZ, ABS_HAT0X, ABS_HAT0Y] {
        if let Some(info) = abs_info(fd, code) {
            infos.insert(code, info);
        }
    }
    let mut name = [0u8; 128];
    // EVIOCGNAME(len)
    let len = unsafe { libc::ioctl(fd, ioc_read(0x06, name.len() as u32), name.as_mut_ptr()) };
    let name = String::from_utf8_lossy(&name[..len.clamp(0, 128) as usize]).trim_end_matches('\0').to_string();
    println!("gamepad: {} ({})", name, path.display());
    let mut file = file;
    let mut buf = [0u8; 24];
    while file.read_exact(&mut buf).is_ok() {
        let kind = u16::from_ne_bytes([buf[16], buf[17]]);
        let code = u16::from_ne_bytes([buf[18], buf[19]]);
        let value = i32::from_ne_bytes(buf[20..24].try_into().unwrap());
        let event = match kind {
            // Auto-repeat (2) is not a new press.
            EV_KEY if value != 2 => PadEvent::Button { code: remap_button(code), pressed: value != 0 },
            EV_ABS => match infos.get(&code) {
                Some(info) => PadEvent::Axis { code, value: normalise(code, value, info) },
                None => continue,
            },
            _ => continue,
        };
        if tx.send(event).is_err() {
            return;
        }
    }
    println!("gamepad: {} disconnected", name);
}

/// Older drivers report the d-pad as `BTN_TRIGGER_HAPPY1..4`.
fn remap_button(code: u16) -> u16 {
    match code {
        0x2c0 => PAD_LEFT,
        0x2c1 => PAD_RIGHT,
        0x2c2 => PAD_UP,
        0x2c3 => PAD_DOWN,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(text: &str) -> PadMapper {
        PadMapper::new(Some(Keymap::parse(text).unwrap()), 1280.0, 720.0)
    }

    #[test]
    fn unbound_buttons_are_android_gamepad_keys() {
        let mut m = PadMapper::new(None, 1280.0, 720.0);
        assert_eq!(m.event(PadEvent::Button { code: PAD_A, pressed: true }),
            vec![Out::AndroidKey { keycode: 96, pressed: true }]);
        assert_eq!(m.event(PadEvent::Button { code: PAD_A, pressed: false }),
            vec![Out::AndroidKey { keycode: 96, pressed: false }]);
        // The hat is the d-pad, and also joystick motion.
        assert_eq!(m.event(PadEvent::Axis { code: ABS_HAT0Y, value: -1.0 }),
            vec![Out::AndroidKey { keycode: 19, pressed: true }, Out::Motion]);
    }

    #[test]
    fn bound_buttons_run_the_keymap() {
        let mut m = map("PAD_A = tap 10 10\nPAD_B = key ESC\nPAD_X = key MOUSE_LEFT\n");
        assert_eq!(m.event(PadEvent::Button { code: PAD_A, pressed: true }),
            vec![Out::Key { code: PAD_A, pressed: true }]);
        assert_eq!(m.event(PadEvent::Button { code: PAD_B, pressed: true }),
            vec![Out::Key { code: 1, pressed: true }]);
        assert_eq!(m.event(PadEvent::Button { code: PAD_X, pressed: true }),
            vec![Out::Cursor { x: 640.0, y: 360.0 }, Out::MouseButton { code: 0x110, pressed: true }]);
    }

    #[test]
    fn stick_directions_press_with_hysteresis_and_hide_motion() {
        let mut m = map("PAD_LS_UP = stick 230 520 110 up\n");
        assert_eq!(m.event(PadEvent::Axis { code: ABS_Y, value: -0.6 }),
            vec![Out::Key { code: PAD_LS_UP, pressed: true }]);
        assert!(m.event(PadEvent::Axis { code: ABS_Y, value: -0.4 }).is_empty());
        assert_eq!(m.event(PadEvent::Axis { code: ABS_Y, value: -0.1 }),
            vec![Out::Key { code: PAD_LS_UP, pressed: false }]);
        // The right stick is unbound: motion.
        assert_eq!(m.event(PadEvent::Axis { code: ABS_RX, value: 0.5 }), vec![Out::Motion]);
    }

    #[test]
    fn pointer_stick_moves_the_cursor() {
        let mut m = map("PAD_LS = pointer 10\n");
        assert!(m.event(PadEvent::Axis { code: ABS_X, value: 1.0 }).is_empty());
        assert_eq!(m.tick(16.0), vec![Out::Cursor { x: 650.0, y: 360.0 }]);
        m.event(PadEvent::Axis { code: ABS_X, value: 0.0 });
        assert!(m.tick(16.0).is_empty());
    }

    #[test]
    fn unplugging_releases_everything() {
        let mut m = PadMapper::new(None, 100.0, 100.0);
        m.event(PadEvent::Button { code: PAD_START, pressed: true });
        assert_eq!(m.event(PadEvent::Gone),
            vec![Out::AndroidKey { keycode: 108, pressed: false }, Out::Motion]);
    }

    #[test]
    fn axes_normalise_with_deadzone() {
        let stick = AbsInfo { minimum: -32768, maximum: 32767, ..Default::default() };
        assert_eq!(normalise(ABS_X, 0, &stick), 0.0);
        assert!((normalise(ABS_X, 32767, &stick) - 1.0).abs() < 1e-3);
        let trigger = AbsInfo { minimum: 0, maximum: 255, ..Default::default() };
        assert_eq!(normalise(ABS_Z, 255, &trigger), 1.0);
        assert!(desktop_layout().action_for(PAD_LS).is_some());
    }
}
