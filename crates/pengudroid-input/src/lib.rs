//! Native Linux input for PenguDroid.
//!
//! * [`uinput`] provides kernel `/dev/uinput` virtual devices (`VirtualTouch`,
//!   `VirtualGamepad`, `VirtualKeyboard`) and evdev readers (`KernelTouchReader`,
//!   `GamepadEventReader`, `EventReader`) so touch and Xbox 360 gamepad input
//!   pass through real native Linux input calls across all apps.
//! * [`keymap`] stores per-package Keyboard/Mouse <-> Xbox 360 Gamepad
//!   bindings (no keymapper-to-touch synthesis).
//! * [`presets`] holds built-in Xbox 360 PC-style control presets.
//! * [`mapper`] translates physical keyboards/gamepads through a `Keymap` onto
//!   kernel `uinput` devices.

pub mod keymap;
pub mod mapper;
pub mod presets;
pub mod uinput;
pub mod xkb;

pub use keymap::{Action, Binding, Dir, GamepadButton, Keymap};
pub use mapper::KeyMapper;
pub use uinput::{
    EventReader, GamepadEventReader, GamepadHostEvent, InputEvent, KernelTouchEvent,
    KernelTouchPhase, KernelTouchReader, VirtualGamepad, VirtualKeyboard, VirtualTouch,
};
