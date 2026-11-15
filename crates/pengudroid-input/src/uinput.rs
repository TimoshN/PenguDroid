//! Virtual input devices through `/dev/uinput`.
//!
//! This is the "real native Linux call" half of PenguDroid's input: instead of
//! synthesising an Android `MotionEvent` and hoping the app's view tree is what
//! receives it, the events are written into the kernel's input subsystem as a
//! genuine multitouch device. Anything that reads input the way a device does —
//! a game's own `AInputQueue`, an SDL2/Unity/Unreal loop, a compositor — sees
//! them exactly as it would see a finger on a touchscreen.
//!
//! Two devices are provided:
//!
//! * [`VirtualTouch`] — a multitouch panel (`ABS_MT_*` + `BTN_TOUCH`), which is
//!   what games expect. Coordinates are in the panel's own units, so the caller
//!   decides the resolution.
//! * [`VirtualKeyboard`] — a plain keyboard, used by the keymapper to turn
//!   physical keys into taps and holds.
//!
//! Both are created with the legacy `uinput_user_dev` layout rather than
//! `UI_DEV_SETUP`/`UI_ABS_SETUP`: it is what every kernel since 2.6 understands,
//! and it carries the axis ranges in one write.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Kernel ABI
// ---------------------------------------------------------------------------

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;

const SYN_REPORT: u16 = 0;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_Z: u16 = 0x02;
const ABS_RX: u16 = 0x03;
const ABS_RY: u16 = 0x04;
const ABS_RZ: u16 = 0x05;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;

const BTN_TOUCH: u16 = 0x14a;

const GAMEPAD_BUTTONS: [u16; 17] = [
    0x130, 0x131, 0x133, 0x134, 0x136, 0x137, 0x138, 0x139, 0x13a, 0x13b, 0x13c, 0x13d, 0x13e,
    0x220, 0x221, 0x222, 0x223,
];

/// `EVIOCGABS(axis)` (`_IOR('E', 0x40 + axis, struct input_absinfo)` = 24 bytes).
const fn eviocgabs(axis: u16) -> libc::c_ulong {
    ((2u64 << 30) | (24u64 << 16) | ((b'E' as u64) << 8) | (0x40 + axis as u64)) as libc::c_ulong
}

/// `struct input_absinfo`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputAbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

/// `ABS_CNT` — the size of the per-axis arrays in `uinput_user_dev`.
const ABS_CNT: usize = 64;
const UINPUT_MAX_NAME_SIZE: usize = 80;

/// `_IOW('U', nr, int)`.
const fn iow_u(nr: u32) -> libc::c_ulong {
    ((1u64 << 30) | (4u64 << 16) | ((b'U' as u64) << 8) | nr as u64) as libc::c_ulong
}

/// `_IO('U', nr)`.
const fn io_u(nr: u32) -> libc::c_ulong {
    (((b'U' as u64) << 8) | nr as u64) as libc::c_ulong
}

const UI_SET_EVBIT: libc::c_ulong = iow_u(100);
const UI_SET_KEYBIT: libc::c_ulong = iow_u(101);
const UI_SET_ABSBIT: libc::c_ulong = iow_u(103);
const UI_DEV_CREATE: libc::c_ulong = io_u(1);
const UI_DEV_DESTROY: libc::c_ulong = io_u(2);

/// `struct input_id`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

/// `struct uinput_user_dev` (the pre-`UI_DEV_SETUP` layout).
#[repr(C)]
struct UinputUserDev {
    name: [u8; UINPUT_MAX_NAME_SIZE],
    id: InputId,
    ff_effects_max: u32,
    absmax: [i32; ABS_CNT],
    absmin: [i32; ABS_CNT],
    absfuzz: [i32; ABS_CNT],
    absflat: [i32; ABS_CNT],
}

impl UinputUserDev {
    fn new(name: &str) -> UinputUserDev {
        let mut dev = UinputUserDev {
            name: [0; UINPUT_MAX_NAME_SIZE],
            // BUS_VIRTUAL: the device is not attached to real hardware.
            id: InputId {
                bustype: 0x06,
                vendor: 0x5044,  // "PD"
                product: 0x0001,
                version: 1,
            },
            ff_effects_max: 0,
            absmax: [0; ABS_CNT],
            absmin: [0; ABS_CNT],
            absfuzz: [0; ABS_CNT],
            absflat: [0; ABS_CNT],
        };
        let bytes = name.as_bytes();
        let n = bytes.len().min(UINPUT_MAX_NAME_SIZE - 1);
        dev.name[..n].copy_from_slice(&bytes[..n]);
        dev
    }
}

/// `struct input_event` on a 64-bit kernel: `timeval` (16) + type/code/value (8).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct InputEvent {
    pub sec: i64,
    pub usec: i64,
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

/// A created uinput device.
pub struct VirtualDevice {
    file: File,
    name: String,
    /// `/dev/input/eventN`, resolved after creation.
    node: Option<PathBuf>,
}

impl VirtualDevice {
    fn open() -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .open(Path::new("/dev/uinput"))
            .or_else(|_| OpenOptions::new().write(true).open(Path::new("/dev/input/uinput")))
    }

    fn set_bit(&self, request: libc::c_ulong, bit: u16) -> io::Result<()> {
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), request, bit as libc::c_int) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Write one `input_event`.
    pub fn emit(&mut self, kind: u16, code: u16, value: i32) -> io::Result<()> {
        let ev = InputEvent {
            kind,
            code,
            value,
            ..InputEvent::default()
        };
        self.write_events(&[ev])
    }

    fn write_events(&mut self, events: &[InputEvent]) -> io::Result<()> {
        use std::io::Write;
        let bytes = unsafe {
            std::slice::from_raw_parts(
                events.as_ptr() as *const u8,
                std::mem::size_of_val(events),
            )
        };
        self.file.write_all(bytes)
    }

    /// `SYN_REPORT`: ends one atomic event frame.
    pub fn sync(&mut self) -> io::Result<()> {
        self.emit(EV_SYN, SYN_REPORT, 0)
    }

    /// The device's name as the kernel knows it.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `/dev/input/eventN`, resolved once the kernel has published the device.
    pub fn node(&self) -> Option<&Path> {
        self.node.as_deref()
    }

    /// Look the device up in `/sys/class/input` by name.
    ///
    /// `UI_DEV_CREATE` returns before udev has made the node, so this retries
    /// briefly; a caller that only needs to inject can ignore the result.
    fn resolve_node(&mut self) {
        for _ in 0..50 {
            if let Some(path) = find_event_node(&self.name) {
                self.node = Some(path);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

impl Drop for VirtualDevice {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY);
        }
    }
}

/// Find `/dev/input/eventN` whose sysfs name matches `name`.
pub fn find_event_node(name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir("/sys/class/input").ok()?;
    for entry in entries.flatten() {
        let file = entry.file_name();
        let file = file.to_string_lossy();
        if !file.starts_with("event") {
            continue;
        }
        let name_path = entry.path().join("device/name");
        let Ok(found) = std::fs::read_to_string(&name_path) else {
            continue;
        };
        if found.trim() == name {
            return Some(PathBuf::from("/dev/input").join(file.as_ref()));
        }
    }
    None
}

/// A virtual multitouch panel.
///
/// Coordinates are in the panel's units: `width`/`height` are the maximum values
/// reported for `ABS_MT_POSITION_X`/`_Y`, so a caller working in screen pixels
/// passes the screen size and needs no scaling.
pub struct VirtualTouch {
    dev: VirtualDevice,
    width: i32,
    height: i32,
    /// Current slot, so a second finger can be added later without reworking the
    /// API: `ABS_MT_SLOT` is emitted whenever it changes.
    slot: i32,
    tracking: i32,
}

impl VirtualTouch {
    /// Create the panel. Requires write access to `/dev/uinput`.
    pub fn create(name: &str, width: u32, height: u32) -> io::Result<VirtualTouch> {
        let file = VirtualDevice::open()?;
        let mut dev = VirtualDevice {
            file,
            name: name.to_string(),
            node: None,
        };
        dev.set_bit(UI_SET_EVBIT, EV_KEY)?;
        dev.set_bit(UI_SET_EVBIT, EV_ABS)?;
        dev.set_bit(UI_SET_EVBIT, EV_SYN)?;
        dev.set_bit(UI_SET_KEYBIT, BTN_TOUCH)?;
        for axis in [
            ABS_X,
            ABS_Y,
            ABS_MT_SLOT,
            ABS_MT_TOUCH_MAJOR,
            ABS_MT_POSITION_X,
            ABS_MT_POSITION_Y,
            ABS_MT_TRACKING_ID,
        ] {
            dev.set_bit(UI_SET_ABSBIT, axis)?;
        }

        let mut setup = UinputUserDev::new(name);
        setup.absmin[ABS_X as usize] = 0;
        setup.absmax[ABS_X as usize] = width as i32;
        setup.absmin[ABS_Y as usize] = 0;
        setup.absmax[ABS_Y as usize] = height as i32;
        setup.absmax[ABS_MT_POSITION_X as usize] = width as i32;
        setup.absmax[ABS_MT_POSITION_Y as usize] = height as i32;
        // A tracking id is a free-running tag; the kernel only needs a range.
        setup.absmin[ABS_MT_TRACKING_ID as usize] = -1;
        setup.absmax[ABS_MT_TRACKING_ID as usize] = 65535;
        setup.absmax[ABS_MT_SLOT as usize] = 9;
        setup.absmax[ABS_MT_TOUCH_MAJOR as usize] = 255;

        use std::io::Write;
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &setup as *const UinputUserDev as *const u8,
                std::mem::size_of::<UinputUserDev>(),
            )
        };
        dev.file.write_all(bytes)?;
        let rc = unsafe { libc::ioctl(dev.file.as_raw_fd(), UI_DEV_CREATE) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        dev.resolve_node();
        Ok(VirtualTouch {
            dev,
            width: width as i32,
            height: height as i32,
            slot: 0,
            tracking: 1,
        })
    }

    pub fn node(&self) -> Option<&Path> {
        self.dev.node()
    }

    pub fn size(&self) -> (i32, i32) {
        (self.width, self.height)
    }

    /// Clamp a coordinate into the panel.
    fn clamp(&self, x: f32, y: f32) -> (i32, i32) {
        let x = x.round().clamp(0.0, self.width as f32) as i32;
        let y = y.round().clamp(0.0, self.height as f32) as i32;
        (x, y)
    }

    /// Finger down at `(x, y)` on `slot`.
    pub fn touch_down(&mut self, slot: i32, x: f32, y: f32) -> io::Result<()> {
        let (x, y) = self.clamp(x, y);
        self.slot = slot.clamp(0, 9);
        self.tracking += 1;
        let tracking = self.tracking;
        self.dev.emit(EV_ABS, ABS_MT_SLOT, self.slot)?;
        self.dev.emit(EV_ABS, ABS_MT_TRACKING_ID, tracking)?;
        self.dev.emit(EV_ABS, ABS_MT_POSITION_X, x)?;
        self.dev.emit(EV_ABS, ABS_MT_POSITION_Y, y)?;
        self.dev.emit(EV_ABS, ABS_MT_TOUCH_MAJOR, 8)?;
        if self.slot == 0 {
            self.dev.emit(EV_ABS, ABS_X, x)?;
            self.dev.emit(EV_ABS, ABS_Y, y)?;
            self.dev.emit(EV_KEY, BTN_TOUCH, 1)?;
        }
        self.dev.sync()
    }

    /// Move the finger on `slot`.
    pub fn touch_move(&mut self, slot: i32, x: f32, y: f32) -> io::Result<()> {
        let (x, y) = self.clamp(x, y);
        self.slot = slot.clamp(0, 9);
        self.dev.emit(EV_ABS, ABS_MT_SLOT, self.slot)?;
        self.dev.emit(EV_ABS, ABS_MT_POSITION_X, x)?;
        self.dev.emit(EV_ABS, ABS_MT_POSITION_Y, y)?;
        if self.slot == 0 {
            self.dev.emit(EV_ABS, ABS_X, x)?;
            self.dev.emit(EV_ABS, ABS_Y, y)?;
        }
        self.dev.sync()
    }

    /// Lift the finger on `slot`.
    pub fn touch_up(&mut self, slot: i32) -> io::Result<()> {
        self.slot = slot.clamp(0, 9);
        self.dev.emit(EV_ABS, ABS_MT_SLOT, self.slot)?;
        self.dev.emit(EV_ABS, ABS_MT_TRACKING_ID, -1)?;
        if self.slot == 0 {
            self.dev.emit(EV_KEY, BTN_TOUCH, 0)?;
        }
        self.dev.sync()
    }

    /// Finger down at `(x, y)` on slot 0.
    pub fn down(&mut self, x: f32, y: f32) -> io::Result<()> {
        self.touch_down(0, x, y)
    }

    /// Move the finger on slot 0.
    pub fn move_to(&mut self, x: f32, y: f32) -> io::Result<()> {
        self.touch_move(0, x, y)
    }

    /// Finger up on slot 0.
    pub fn up(&mut self) -> io::Result<()> {
        self.touch_up(0)
    }

    /// A complete tap: down, a short hold, up.
    pub fn tap(&mut self, x: f32, y: f32) -> io::Result<()> {
        self.down(x, y)?;
        std::thread::sleep(std::time::Duration::from_millis(40));
        self.up()
    }

    /// A drag from `(x0, y0)` to `(x1, y1)` in `steps` moves.
    pub fn swipe(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, steps: u32) -> io::Result<()> {
        let steps = steps.max(1);
        self.down(x0, y0)?;
        for i in 1..=steps {
            let t = i as f32 / steps as f32;
            self.move_to(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t)?;
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
        self.up()
    }
}

/// A virtual keyboard, used by the keymapper.
pub struct VirtualKeyboard {
    dev: VirtualDevice,
}

impl VirtualKeyboard {
    pub fn create(name: &str) -> io::Result<VirtualKeyboard> {
        let file = VirtualDevice::open()?;
        let mut dev = VirtualDevice {
            file,
            name: name.to_string(),
            node: None,
        };
        dev.set_bit(UI_SET_EVBIT, EV_KEY)?;
        dev.set_bit(UI_SET_EVBIT, EV_SYN)?;
        // Every key the kernel knows: the keymapper may bind any of them.
        for code in 1..=255u16 {
            dev.set_bit(UI_SET_KEYBIT, code)?;
        }
        let setup = UinputUserDev::new(name);
        use std::io::Write;
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &setup as *const UinputUserDev as *const u8,
                std::mem::size_of::<UinputUserDev>(),
            )
        };
        dev.file.write_all(bytes)?;
        let rc = unsafe { libc::ioctl(dev.file.as_raw_fd(), UI_DEV_CREATE) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        dev.resolve_node();
        Ok(VirtualKeyboard { dev })
    }

    pub fn node(&self) -> Option<&Path> {
        self.dev.node()
    }

    /// Press or release a key by Linux keycode (`KEY_A` = 30, ...).
    pub fn key(&mut self, code: u16, pressed: bool) -> io::Result<()> {
        self.dev.emit(EV_KEY, code, i32::from(pressed))?;
        self.dev.sync()
    }
}

/// A virtual Xbox 360 Controller (`Microsoft X-Box 360 pad`, `045e:028e`).
pub struct VirtualGamepad {
    dev: VirtualDevice,
}

impl VirtualGamepad {
    pub fn create(name: &str) -> io::Result<VirtualGamepad> {
        let file = VirtualDevice::open()?;
        let mut dev = VirtualDevice {
            file,
            name: name.to_string(),
            node: None,
        };
        dev.set_bit(UI_SET_EVBIT, EV_KEY)?;
        dev.set_bit(UI_SET_EVBIT, EV_ABS)?;
        dev.set_bit(UI_SET_EVBIT, EV_SYN)?;
        for btn in GAMEPAD_BUTTONS {
            dev.set_bit(UI_SET_KEYBIT, btn)?;
        }
        for axis in [ABS_X, ABS_Y, ABS_Z, ABS_RX, ABS_RY, ABS_RZ, ABS_HAT0X, ABS_HAT0Y] {
            dev.set_bit(UI_SET_ABSBIT, axis)?;
        }

        let mut setup = UinputUserDev::new(name);
        setup.id = InputId {
            bustype: 0x03, // BUS_USB
            vendor: 0x045e,
            product: 0x028e,
            version: 0x0110,
        };
        for axis in [ABS_X, ABS_Y, ABS_RX, ABS_RY] {
            setup.absmin[axis as usize] = -32768;
            setup.absmax[axis as usize] = 32767;
            setup.absfuzz[axis as usize] = 16;
            setup.absflat[axis as usize] = 128;
        }
        for axis in [ABS_Z, ABS_RZ] {
            setup.absmin[axis as usize] = 0;
            setup.absmax[axis as usize] = 255;
        }
        for axis in [ABS_HAT0X, ABS_HAT0Y] {
            setup.absmin[axis as usize] = -1;
            setup.absmax[axis as usize] = 1;
        }

        use std::io::Write;
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &setup as *const UinputUserDev as *const u8,
                std::mem::size_of::<UinputUserDev>(),
            )
        };
        dev.file.write_all(bytes)?;
        let rc = unsafe { libc::ioctl(dev.file.as_raw_fd(), UI_DEV_CREATE) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        dev.resolve_node();
        Ok(VirtualGamepad { dev })
    }

    pub fn node(&self) -> Option<&Path> {
        self.dev.node()
    }

    pub fn button(&mut self, code: u16, pressed: bool) -> io::Result<()> {
        self.dev.emit(EV_KEY, code, i32::from(pressed))?;
        self.dev.sync()
    }

    pub fn left_stick(&mut self, x: f32, y: f32) -> io::Result<()> {
        self.dev.emit(EV_ABS, ABS_X, (x.clamp(-1.0, 1.0) * 32767.0).round() as i32)?;
        self.dev.emit(EV_ABS, ABS_Y, (y.clamp(-1.0, 1.0) * 32767.0).round() as i32)?;
        self.dev.sync()
    }

    pub fn right_stick(&mut self, x: f32, y: f32) -> io::Result<()> {
        self.dev.emit(EV_ABS, ABS_RX, (x.clamp(-1.0, 1.0) * 32767.0).round() as i32)?;
        self.dev.emit(EV_ABS, ABS_RY, (y.clamp(-1.0, 1.0) * 32767.0).round() as i32)?;
        self.dev.sync()
    }

    pub fn triggers(&mut self, left: f32, right: f32) -> io::Result<()> {
        self.dev.emit(EV_ABS, ABS_Z, (left.clamp(0.0, 1.0) * 255.0).round() as i32)?;
        self.dev.emit(EV_ABS, ABS_RZ, (right.clamp(0.0, 1.0) * 255.0).round() as i32)?;
        self.dev.sync()
    }

    pub fn hat(&mut self, x: f32, y: f32) -> io::Result<()> {
        self.dev.emit(EV_ABS, ABS_HAT0X, x.round().clamp(-1.0, 1.0) as i32)?;
        self.dev.emit(EV_ABS, ABS_HAT0Y, y.round().clamp(-1.0, 1.0) as i32)?;
        self.dev.sync()
    }

    pub fn emit_state(
        &mut self,
        buttons: &[(u16, bool)],
        lx: f32,
        ly: f32,
        rx: f32,
        ry: f32,
        lt: f32,
        rt: f32,
        hat_x: f32,
        hat_y: f32,
    ) -> io::Result<()> {
        for &(code, down) in buttons {
            self.dev.emit(EV_KEY, code, i32::from(down))?;
        }
        self.dev.emit(EV_ABS, ABS_X, (lx.clamp(-1.0, 1.0) * 32767.0).round() as i32)?;
        self.dev.emit(EV_ABS, ABS_Y, (ly.clamp(-1.0, 1.0) * 32767.0).round() as i32)?;
        self.dev.emit(EV_ABS, ABS_RX, (rx.clamp(-1.0, 1.0) * 32767.0).round() as i32)?;
        self.dev.emit(EV_ABS, ABS_RY, (ry.clamp(-1.0, 1.0) * 32767.0).round() as i32)?;
        self.dev.emit(EV_ABS, ABS_Z, (lt.clamp(0.0, 1.0) * 255.0).round() as i32)?;
        self.dev.emit(EV_ABS, ABS_RZ, (rt.clamp(0.0, 1.0) * 255.0).round() as i32)?;
        self.dev.emit(EV_ABS, ABS_HAT0X, hat_x.round().clamp(-1.0, 1.0) as i32)?;
        self.dev.emit(EV_ABS, ABS_HAT0Y, hat_y.round().clamp(-1.0, 1.0) as i32)?;
        self.dev.sync()
    }
}

// ---------------------------------------------------------------------------
// Reading real devices (the keymapper's input side)
// ---------------------------------------------------------------------------

/// An opened `/dev/input/eventN` that yields key events.
pub struct EventReader {
    file: File,
    path: PathBuf,
}

impl EventReader {
    pub fn open(path: &Path) -> io::Result<EventReader> {
        let file = OpenOptions::new().read(true).open(path)?;
        Ok(EventReader {
            file,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Block until one event arrives.
    pub fn read_event(&mut self) -> io::Result<InputEvent> {
        use std::io::Read;
        let mut buf = [0u8; std::mem::size_of::<InputEvent>()];
        self.file.read_exact(&mut buf)?;
        // SAFETY: `InputEvent` is `repr(C)` and the buffer is exactly its size.
        Ok(unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const InputEvent) })
    }

    /// Wait for at most `timeout`, then consume one physical input record.
    /// Key auto-repeat is ignored here: mapping a held key must not repeatedly
    /// fire tap/swipe actions, and idle keyboards must not prevent shutdown.
    pub fn read_key_timeout(&mut self, timeout: std::time::Duration) -> io::Result<Option<(u16, bool)>> {
        let mut fd = libc::pollfd { fd: self.file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let rc = unsafe { libc::poll(&mut fd, 1, timeout.as_millis().min(i32::MAX as u128) as i32) };
        if rc < 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::Interrupted { Ok(None) } else { Err(error) };
        }
        if rc == 0 { return Ok(None); }
        if fd.revents & (libc::POLLERR | libc::POLLNVAL | libc::POLLHUP) != 0 {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "input device disconnected"));
        }
        let event = self.read_event()?;
        if event.kind == EV_KEY && (event.value == 0 || event.value == 1) {
            Ok(Some((event.code, event.value == 1)))
        } else { Ok(None) }
    }

    /// Block until a key event, returning `(code, pressed)`.
    pub fn read_key(&mut self) -> io::Result<(u16, bool)> {
        loop {
            let ev = self.read_event()?;
            if ev.kind == EV_KEY {
                return Ok((ev.code, ev.value != 0));
            }
        }
    }
}

/// Every readable keyboard-like device under `/dev/input`.
///
/// The capability mask lives in sysfs, not in `/dev`: a keyboard sets bits well
/// past the pointer range, so a mask longer than 32 bits means "this device has
/// keys we can map". Devices whose mask cannot be read are skipped rather than
/// guessed at.
pub fn keyboards() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/input") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        if !name.starts_with("event") {
            continue;
        }
        let caps = entry.path().join("device/capabilities/key");
        let Ok(mask) = std::fs::read_to_string(&caps) else {
            continue;
        };
        // 32 bits of mask = 8 hex digits; a keyboard always sets more.
        if mask.trim().len() > 8 {
            out.push(PathBuf::from("/dev/input").join(&name));
        }
    }
    out.sort();
    out
}

fn read_abs_info(fd: libc::c_int, axis: u16) -> Option<InputAbsInfo> {
    let mut info = InputAbsInfo::default();
    let rc = unsafe { libc::ioctl(fd, eviocgabs(axis), &mut info as *mut InputAbsInfo) };
    if rc < 0 || info.maximum <= info.minimum {
        None
    } else {
        Some(info)
    }
}

fn sysfs_bit_set(hex_mask: &str, bit: u16) -> bool {
    let groups: Vec<u64> = hex_mask
        .split_whitespace()
        .filter_map(|w| u64::from_str_radix(w, 16).ok())
        .collect();
    // Sysfs prints the bitmask most-significant word first; each word is usize bits
    // on Linux, wait — match standard 64-bit word indexing from the tail.
    let word_idx = (bit / 64) as usize;
    let bit_idx = (bit % 64) as u32;
    let rev = groups.len().checked_sub(1 + word_idx);
    rev.is_some_and(|i| (groups[i] >> bit_idx) & 1 != 0)
}

/// Physical/virtual gamepads under `/dev/input` (`BTN_SOUTH` / `ABS_X`).
pub fn gamepads(exclude: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/input") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("event") {
            continue;
        }
        let dev_path = PathBuf::from("/dev/input").join(&name);
        if exclude.contains(&dev_path) {
            continue;
        }
        let Ok(key_mask) = std::fs::read_to_string(entry.path().join("device/capabilities/key")) else {
            continue;
        };
        if sysfs_bit_set(&key_mask, 0x130) {
            out.push(dev_path);
        }
    }
    out.sort();
    out
}

/// Touchscreens under `/dev/input` (`BTN_TOUCH` + `ABS_X` or `ABS_MT_POSITION_X`).
pub fn touchscreens(exclude: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/input") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("event") {
            continue;
        }
        let dev_path = PathBuf::from("/dev/input").join(&name);
        if exclude.contains(&dev_path) {
            continue;
        }
        let Ok(key_mask) = std::fs::read_to_string(entry.path().join("device/capabilities/key")) else {
            continue;
        };
        let Ok(abs_mask) = std::fs::read_to_string(entry.path().join("device/capabilities/abs")) else {
            continue;
        };
        if sysfs_bit_set(&key_mask, BTN_TOUCH)
            && (sysfs_bit_set(&abs_mask, ABS_MT_POSITION_X) || sysfs_bit_set(&abs_mask, ABS_X))
        {
            out.push(dev_path);
        }
    }
    out.sort();
    out
}

/// Normalized physical/kernel gamepad events.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GamepadHostEvent {
    Button { code: u16, pressed: bool },
    LeftX(f32),
    LeftY(f32),
    RightX(f32),
    RightY(f32),
    LeftTrigger(f32),
    RightTrigger(f32),
    HatX(f32),
    HatY(f32),
}

/// Non-blocking kernel evdev reader for a gamepad device.
pub struct GamepadEventReader {
    file: File,
    path: PathBuf,
    abs: [Option<InputAbsInfo>; ABS_CNT],
}

impl GamepadEventReader {
    pub fn open(path: &Path) -> io::Result<GamepadEventReader> {
        let file = OpenOptions::new().read(true).open(path)?;
        let fd = file.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags >= 0 {
            unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        }
        let mut abs = [None; ABS_CNT];
        for axis in [ABS_X, ABS_Y, ABS_Z, ABS_RX, ABS_RY, ABS_RZ, ABS_HAT0X, ABS_HAT0Y] {
            abs[axis as usize] = read_abs_info(fd, axis);
        }
        Ok(GamepadEventReader { file, path: path.to_path_buf(), abs })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn normalize_signed(&self, axis: u16, raw: i32) -> f32 {
        let Some(info) = self.abs[axis as usize] else {
            return (raw as f32 / 32767.0).clamp(-1.0, 1.0);
        };
        let mid = 0.5 * (info.minimum as f32 + info.maximum as f32);
        let half = 0.5 * (info.maximum as f32 - info.minimum as f32);
        if half <= 0.0 {
            return 0.0;
        }
        let v = ((raw as f32 - mid) / half).clamp(-1.0, 1.0);
        if v.abs() < 0.06 { 0.0 } else { v }
    }

    fn normalize_trigger(&self, axis: u16, raw: i32) -> f32 {
        let Some(info) = self.abs[axis as usize] else {
            return (raw as f32 / 255.0).clamp(0.0, 1.0);
        };
        let span = (info.maximum - info.minimum) as f32;
        if span <= 0.0 {
            0.0
        } else {
            ((raw - info.minimum) as f32 / span).clamp(0.0, 1.0)
        }
    }

    /// Drain all currently available events without blocking.
    pub fn drain(&mut self) -> io::Result<Vec<GamepadHostEvent>> {
        use std::io::Read;
        let mut out = Vec::new();
        let mut buf = [0u8; std::mem::size_of::<InputEvent>()];
        loop {
            match self.file.read_exact(&mut buf) {
                Ok(()) => {
                    let ev = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const InputEvent) };
                    match ev.kind {
                        EV_KEY if GAMEPAD_BUTTONS.contains(&ev.code) && (ev.value == 0 || ev.value == 1) => {
                            out.push(GamepadHostEvent::Button { code: ev.code, pressed: ev.value == 1 });
                        }
                        EV_ABS => match ev.code {
                            ABS_X => out.push(GamepadHostEvent::LeftX(self.normalize_signed(ABS_X, ev.value))),
                            ABS_Y => out.push(GamepadHostEvent::LeftY(self.normalize_signed(ABS_Y, ev.value))),
                            ABS_RX => out.push(GamepadHostEvent::RightX(self.normalize_signed(ABS_RX, ev.value))),
                            ABS_RY => out.push(GamepadHostEvent::RightY(self.normalize_signed(ABS_RY, ev.value))),
                            ABS_Z => out.push(GamepadHostEvent::LeftTrigger(self.normalize_trigger(ABS_Z, ev.value))),
                            ABS_RZ => out.push(GamepadHostEvent::RightTrigger(self.normalize_trigger(ABS_RZ, ev.value))),
                            ABS_HAT0X => out.push(GamepadHostEvent::HatX((ev.value as f32).clamp(-1.0, 1.0))),
                            ABS_HAT0Y => out.push(GamepadHostEvent::HatY((ev.value as f32).clamp(-1.0, 1.0))),
                            _ => {}
                        },
                        _ => {}
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(out),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

/// Phase of a kernel touchscreen slot event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelTouchPhase {
    Down,
    Move,
    Up,
}

/// A normalized multitouch sample synthesized from kernel `/dev/input/eventN` records.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KernelTouchEvent {
    pub slot: i32,
    pub phase: KernelTouchPhase,
    pub norm_x: f32,
    pub norm_y: f32,
}

#[derive(Clone, Copy, Default)]
struct KernelSlotState {
    tracking_id: i32,
    active: bool,
    raw_x: i32,
    raw_y: i32,
    dirty: bool,
    pending_up: bool,
}

/// Non-blocking kernel `/dev/input/eventN` multitouch / single-touch reader.
pub struct KernelTouchReader {
    file: File,
    abs_x: Option<InputAbsInfo>,
    abs_y: Option<InputAbsInfo>,
    current_slot: usize,
    slots: [KernelSlotState; 10],
    mt_seen: bool,
    single_x: i32,
    single_y: i32,
    single_down: bool,
    single_dirty: bool,
}

impl KernelTouchReader {
    pub fn open(path: &Path) -> io::Result<KernelTouchReader> {
        let file = OpenOptions::new().read(true).open(path)?;
        let fd = file.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags >= 0 {
            unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        }
        let abs_x = read_abs_info(fd, ABS_MT_POSITION_X).or_else(|| read_abs_info(fd, ABS_X));
        let abs_y = read_abs_info(fd, ABS_MT_POSITION_Y).or_else(|| read_abs_info(fd, ABS_Y));
        Ok(KernelTouchReader {
            file,
            abs_x,
            abs_y,
            current_slot: 0,
            slots: [KernelSlotState { tracking_id: -1, ..KernelSlotState::default() }; 10],
            mt_seen: false,
            single_x: 0,
            single_y: 0,
            single_down: false,
            single_dirty: false,
        })
    }

    fn normalize(info: Option<InputAbsInfo>, val: i32) -> f32 {
        let Some(info) = info else {
            return 0.5;
        };
        let span = (info.maximum - info.minimum) as f32;
        if span <= 0.0 {
            0.0
        } else {
            ((val - info.minimum) as f32 / span).clamp(0.0, 1.0)
        }
    }

    fn feed(&mut self, ev: InputEvent, out: &mut Vec<KernelTouchEvent>) {
        match ev.kind {
            EV_ABS => match ev.code {
                ABS_MT_SLOT => {
                    self.mt_seen = true;
                    if (0..10).contains(&ev.value) {
                        self.current_slot = ev.value as usize;
                    }
                }
                ABS_MT_TRACKING_ID => {
                    self.mt_seen = true;
                    let slot = &mut self.slots[self.current_slot];
                    if ev.value >= 0 {
                        slot.tracking_id = ev.value;
                        slot.dirty = true;
                        slot.pending_up = false;
                    } else if slot.active {
                        slot.pending_up = true;
                    }
                }
                ABS_MT_POSITION_X => {
                    self.mt_seen = true;
                    let slot = &mut self.slots[self.current_slot];
                    slot.raw_x = ev.value;
                    slot.dirty = true;
                }
                ABS_MT_POSITION_Y => {
                    self.mt_seen = true;
                    let slot = &mut self.slots[self.current_slot];
                    slot.raw_y = ev.value;
                    slot.dirty = true;
                }
                ABS_X => {
                    self.single_x = ev.value;
                    self.single_dirty = true;
                }
                ABS_Y => {
                    self.single_y = ev.value;
                    self.single_dirty = true;
                }
                _ => {}
            },
            EV_KEY if ev.code == BTN_TOUCH => {
                let down = ev.value != 0;
                if down != self.single_down {
                    self.single_down = down;
                    self.single_dirty = true;
                }
            }
            EV_SYN if ev.code == SYN_REPORT => {
                if self.mt_seen {
                    for idx in 0..self.slots.len() {
                        let s = &mut self.slots[idx];
                        let nx = Self::normalize(self.abs_x, s.raw_x);
                        let ny = Self::normalize(self.abs_y, s.raw_y);
                        if s.pending_up {
                            s.pending_up = false;
                            s.active = false;
                            s.dirty = false;
                            s.tracking_id = -1;
                            out.push(KernelTouchEvent {
                                slot: idx as i32,
                                phase: KernelTouchPhase::Up,
                                norm_x: nx,
                                norm_y: ny,
                            });
                        } else if s.dirty && s.tracking_id >= 0 {
                            let phase = if !s.active {
                                s.active = true;
                                KernelTouchPhase::Down
                            } else {
                                KernelTouchPhase::Move
                            };
                            s.dirty = false;
                            out.push(KernelTouchEvent {
                                slot: idx as i32,
                                phase,
                                norm_x: nx,
                                norm_y: ny,
                            });
                        }
                    }
                } else if self.single_dirty {
                    self.single_dirty = false;
                    let nx = Self::normalize(self.abs_x, self.single_x);
                    let ny = Self::normalize(self.abs_y, self.single_y);
                    if self.single_down {
                        let phase = if !self.slots[0].active {
                            self.slots[0].active = true;
                            KernelTouchPhase::Down
                        } else {
                            KernelTouchPhase::Move
                        };
                        out.push(KernelTouchEvent { slot: 0, phase, norm_x: nx, norm_y: ny });
                    } else if self.slots[0].active {
                        self.slots[0].active = false;
                        out.push(KernelTouchEvent { slot: 0, phase: KernelTouchPhase::Up, norm_x: nx, norm_y: ny });
                    }
                }
            }
            _ => {}
        }
    }

    pub fn drain(&mut self) -> io::Result<Vec<KernelTouchEvent>> {
        use std::io::Read;
        let mut out = Vec::new();
        let mut buf = [0u8; std::mem::size_of::<InputEvent>()];
        loop {
            match self.file.read_exact(&mut buf) {
                Ok(()) => {
                    let ev = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const InputEvent) };
                    self.feed(ev, &mut out);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(out),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_input_timeout_and_repeat_filtering_allow_mapper_shutdown() {
        use std::io::Write;
        use std::os::fd::OwnedFd;
        let (read, mut write) = std::os::unix::net::UnixStream::pair().unwrap();
        read.set_nonblocking(true).unwrap();
        let mut reader = EventReader { file: File::from(OwnedFd::from(read)), path: PathBuf::new() };
        let timeout = std::time::Duration::from_millis(10);
        assert_eq!(reader.read_key_timeout(timeout).unwrap(), None);
        for (value, expected) in [(1, Some((17, true))), (2, None), (0, Some((17, false)))] {
            let event = InputEvent { sec: 0, usec: 0, kind: EV_KEY, code: 17, value };
            let bytes = unsafe { std::slice::from_raw_parts((&event as *const InputEvent).cast::<u8>(), std::mem::size_of::<InputEvent>()) };
            write.write_all(bytes).unwrap();
            assert_eq!(reader.read_key_timeout(timeout).unwrap(), expected);
        }
    }

    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        // `_IOW('U', 100, int)` = 0x40045564, `_IO('U', 1)` = 0x5501.
        assert_eq!(UI_SET_EVBIT, 0x4004_5564);
        assert_eq!(UI_SET_KEYBIT, 0x4004_5565);
        assert_eq!(UI_SET_ABSBIT, 0x4004_5567);
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_DEV_DESTROY, 0x5502);
    }

    #[test]
    fn user_dev_layout_matches_the_kernel() {
        // 80 name + 8 input_id + 4 ff_effects_max + 4 * 64 * 4 axis arrays.
        assert_eq!(std::mem::size_of::<UinputUserDev>(), 1116);
        assert_eq!(std::mem::size_of::<InputEvent>(), 24);
    }

    /// End-to-end: create a panel, tap, and read the events back from the node
    /// the kernel published. Skips when `/dev/uinput` is not writable.
    #[test]
    fn tap_is_visible_on_the_created_device() {
        let Ok(mut touch) = VirtualTouch::create("pengudroid-test-touch", 720, 1280) else {
            eprintln!("skipping: /dev/uinput is not writable");
            return;
        };
        let node = touch.node().map(|p| p.to_path_buf());
        let Some(node) = node else {
            eprintln!("skipping: the kernel did not publish an event node");
            return;
        };
        // Reading `/dev/input/eventN` needs the `input` group (or root); writing
        // to `/dev/uinput` does not. Injection is what matters in practice, so a
        // permission error here is a skip, not a failure.
        let mut reader = match EventReader::open(&node) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping read-back: {} is not readable ({e})", node.display());
                return;
            }
            Err(e) => panic!("open {}: {e}", node.display()),
        };
        touch.tap(100.0, 200.0).expect("tap");

        let mut saw_touch_down = false;
        let mut saw_touch_up = false;
        let mut x = None;
        for _ in 0..64 {
            let ev = reader.read_event().expect("read");
            if ev.kind == EV_KEY && ev.code == BTN_TOUCH {
                if ev.value == 1 {
                    saw_touch_down = true;
                } else {
                    saw_touch_up = true;
                }
            }
            if ev.kind == EV_ABS && ev.code == ABS_MT_POSITION_X {
                x = Some(ev.value);
            }
            if saw_touch_down && saw_touch_up {
                break;
            }
        }
        assert!(saw_touch_down, "no BTN_TOUCH press came back");
        assert!(saw_touch_up, "no BTN_TOUCH release came back");
        assert_eq!(x, Some(100), "the X coordinate did not survive the round trip");
    }

    #[test]
    fn sysfs_bit_set_matches_little_endian_hex_mask() {
        assert!(sysfs_bit_set("0000000100000000 0000000000000000", 0x60));
        // BTN_SOUTH (0x130 = 304) -> word 4, bit 48
        assert!(sysfs_bit_set("0001000000000000 0 0 0 0", 0x130));
        // BTN_TOUCH (0x14a = 330) -> word 5, bit 10
        assert!(sysfs_bit_set("400 0 0 0 0 0", 0x14a));
        assert!(!sysfs_bit_set("0", 0x130));
    }

    #[test]
    fn kernel_touch_reader_parses_multitouch_and_syn_report() {
        use std::io::Write;
        use std::os::fd::OwnedFd;
        let (read, mut write) = std::os::unix::net::UnixStream::pair().unwrap();
        read.set_nonblocking(true).unwrap();
        let mut reader = KernelTouchReader {
            file: File::from(OwnedFd::from(read)),
            abs_x: Some(InputAbsInfo { minimum: 0, maximum: 1000, ..Default::default() }),
            abs_y: Some(InputAbsInfo { minimum: 0, maximum: 2000, ..Default::default() }),
            current_slot: 0,
            slots: [KernelSlotState { tracking_id: -1, ..KernelSlotState::default() }; 10],
            mt_seen: false,
            single_x: 0,
            single_y: 0,
            single_down: false,
            single_dirty: false,
        };
        let emit = |w: &mut std::os::unix::net::UnixStream, kind: u16, code: u16, value: i32| {
            let ev = InputEvent { sec: 0, usec: 0, kind, code, value };
            let bytes = unsafe { std::slice::from_raw_parts((&ev as *const InputEvent).cast::<u8>(), std::mem::size_of::<InputEvent>()) };
            w.write_all(bytes).unwrap();
        };
        emit(&mut write, EV_ABS, ABS_MT_SLOT, 0);
        emit(&mut write, EV_ABS, ABS_MT_TRACKING_ID, 42);
        emit(&mut write, EV_ABS, ABS_MT_POSITION_X, 500);
        emit(&mut write, EV_ABS, ABS_MT_POSITION_Y, 1000);
        emit(&mut write, EV_SYN, SYN_REPORT, 0);
        emit(&mut write, EV_ABS, ABS_MT_TRACKING_ID, -1);
        emit(&mut write, EV_SYN, SYN_REPORT, 0);
        let events = reader.drain().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].phase, KernelTouchPhase::Down);
        assert!((events[0].norm_x - 0.5).abs() < 1e-4);
        assert!((events[0].norm_y - 0.5).abs() < 1e-4);
        assert_eq!(events[1].phase, KernelTouchPhase::Up);
    }
}
