//! Per-app key mappings: keyboard/mouse <-> Xbox 360 gamepad controls.
//!
//! Maps physical keyboard keys and mouse buttons/motion to native Xbox 360
//! gamepad controls (`pad`, `lstick`, `rstick`, `dpad`, `mouselook`), or maps
//! physical gamepad buttons back to keyboard/mouse keys (`key`).
//!
//! ```text
//! # Native PC Xbox 360 Controller mapping
//! W         = lstick up
//! A         = lstick left
//! S         = lstick down
//! D         = lstick right
//! SPACE     = pad a
//! LEFTSHIFT = pad b
//! F         = pad x
//! R         = pad y
//! Q         = pad lb
//! E         = pad rb
//! LEFTALT   = mouselook 1.5
//! PAD_A     = key SPACE
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// An Xbox 360 controller button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GamepadButton {
    South,     // A (BTN_SOUTH = 0x130, KEYCODE_BUTTON_A = 96)
    East,      // B (BTN_EAST  = 0x131, KEYCODE_BUTTON_B = 97)
    North,     // X (BTN_NORTH = 0x133, KEYCODE_BUTTON_X = 99)
    West,      // Y (BTN_WEST  = 0x134, KEYCODE_BUTTON_Y = 100)
    Tl,        // LB (BTN_TL = 0x136, KEYCODE_BUTTON_L1 = 102)
    Tr,        // RB (BTN_TR = 0x137, KEYCODE_BUTTON_R1 = 103)
    Tl2,       // LT digital (BTN_TL2 = 0x138, KEYCODE_BUTTON_L2 = 104)
    Tr2,       // RT digital (BTN_TR2 = 0x139, KEYCODE_BUTTON_R2 = 105)
    Select,    // Back/View (BTN_SELECT = 0x13a, KEYCODE_BUTTON_SELECT = 109)
    Start,     // Start/Menu (BTN_START = 0x13b, KEYCODE_BUTTON_START = 108)
    Mode,      // Guide/Xbox (BTN_MODE = 0x13c, KEYCODE_BUTTON_MODE = 110)
    Thumbl,    // L3 (BTN_THUMBL = 0x13d, KEYCODE_BUTTON_THUMBL = 106)
    Thumbr,    // R3 (BTN_THUMBR = 0x13e, KEYCODE_BUTTON_THUMBR = 107)
    DpadUp,    // (BTN_DPAD_UP = 0x220, KEYCODE_DPAD_UP = 19)
    DpadDown,  // (BTN_DPAD_DOWN = 0x221, KEYCODE_DPAD_DOWN = 20)
    DpadLeft,  // (BTN_DPAD_LEFT = 0x222, KEYCODE_DPAD_LEFT = 21)
    DpadRight, // (BTN_DPAD_RIGHT = 0x223, KEYCODE_DPAD_RIGHT = 22)
}

impl GamepadButton {
    pub fn parse(text: &str) -> Option<GamepadButton> {
        match text.trim().to_ascii_lowercase().as_str() {
            "a" | "south" | "btn_a" | "btn_south" => Some(Self::South),
            "b" | "east" | "btn_b" | "btn_east" => Some(Self::East),
            "x" | "north" | "btn_x" | "btn_north" => Some(Self::North),
            "y" | "west" | "btn_y" | "btn_west" => Some(Self::West),
            "lb" | "l1" | "tl" | "btn_tl" => Some(Self::Tl),
            "rb" | "r1" | "tr" | "btn_tr" => Some(Self::Tr),
            "lt" | "l2" | "tl2" | "btn_tl2" => Some(Self::Tl2),
            "rt" | "r2" | "tr2" | "btn_tr2" => Some(Self::Tr2),
            "back" | "select" | "view" | "btn_select" => Some(Self::Select),
            "start" | "menu" | "btn_start" => Some(Self::Start),
            "guide" | "mode" | "xbox" | "btn_mode" => Some(Self::Mode),
            "l3" | "thumbl" | "ls" | "btn_thumbl" => Some(Self::Thumbl),
            "r3" | "thumbr" | "rs" | "btn_thumbr" => Some(Self::Thumbr),
            "dpad_up" | "dup" => Some(Self::DpadUp),
            "dpad_down" | "ddown" => Some(Self::DpadDown),
            "dpad_left" | "dleft" => Some(Self::DpadLeft),
            "dpad_right" | "dright" => Some(Self::DpadRight),
            _ => None,
        }
    }

    pub fn from_linux_code(code: u16) -> Option<GamepadButton> {
        match code {
            0x130 => Some(Self::South),
            0x131 => Some(Self::East),
            0x133 => Some(Self::North),
            0x134 => Some(Self::West),
            0x136 => Some(Self::Tl),
            0x137 => Some(Self::Tr),
            0x138 => Some(Self::Tl2),
            0x139 => Some(Self::Tr2),
            0x13a => Some(Self::Select),
            0x13b => Some(Self::Start),
            0x13c => Some(Self::Mode),
            0x13d => Some(Self::Thumbl),
            0x13e => Some(Self::Thumbr),
            0x220 => Some(Self::DpadUp),
            0x221 => Some(Self::DpadDown),
            0x222 => Some(Self::DpadLeft),
            0x223 => Some(Self::DpadRight),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::South => "a",
            Self::East => "b",
            Self::North => "x",
            Self::West => "y",
            Self::Tl => "lb",
            Self::Tr => "rb",
            Self::Tl2 => "lt",
            Self::Tr2 => "rt",
            Self::Select => "back",
            Self::Start => "start",
            Self::Mode => "guide",
            Self::Thumbl => "l3",
            Self::Thumbr => "r3",
            Self::DpadUp => "dpad_up",
            Self::DpadDown => "dpad_down",
            Self::DpadLeft => "dpad_left",
            Self::DpadRight => "dpad_right",
        }
    }

    pub fn linux_code(self) -> u16 {
        match self {
            Self::South => 0x130,
            Self::East => 0x131,
            Self::North => 0x133,
            Self::West => 0x134,
            Self::Tl => 0x136,
            Self::Tr => 0x137,
            Self::Tl2 => 0x138,
            Self::Tr2 => 0x139,
            Self::Select => 0x13a,
            Self::Start => 0x13b,
            Self::Mode => 0x13c,
            Self::Thumbl => 0x13d,
            Self::Thumbr => 0x13e,
            Self::DpadUp => 0x220,
            Self::DpadDown => 0x221,
            Self::DpadLeft => 0x222,
            Self::DpadRight => 0x223,
        }
    }

    pub fn android_keycode(self) -> i32 {
        match self {
            Self::South => 96,      // KEYCODE_BUTTON_A
            Self::East => 97,       // KEYCODE_BUTTON_B
            Self::North => 99,      // KEYCODE_BUTTON_X
            Self::West => 100,      // KEYCODE_BUTTON_Y
            Self::Tl => 102,        // KEYCODE_BUTTON_L1
            Self::Tr => 103,        // KEYCODE_BUTTON_R1
            Self::Tl2 => 104,       // KEYCODE_BUTTON_L2
            Self::Tr2 => 105,       // KEYCODE_BUTTON_R2
            Self::Select => 109,    // KEYCODE_BUTTON_SELECT
            Self::Start => 108,     // KEYCODE_BUTTON_START
            Self::Mode => 110,      // KEYCODE_BUTTON_MODE
            Self::Thumbl => 106,    // KEYCODE_BUTTON_THUMBL
            Self::Thumbr => 107,    // KEYCODE_BUTTON_THUMBR
            Self::DpadUp => 19,     // KEYCODE_DPAD_UP
            Self::DpadDown => 20,   // KEYCODE_DPAD_DOWN
            Self::DpadLeft => 21,   // KEYCODE_DPAD_LEFT
            Self::DpadRight => 22,  // KEYCODE_DPAD_RIGHT
        }
    }
}

/// What a bound key/button performs.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Hold/release an Xbox 360 gamepad button (`pad a`, `pad rt`, ...).
    Pad(GamepadButton),
    /// Push the left analog stick in `Dir` (`lstick up`).
    LeftStick(Dir),
    /// Push the right analog stick in `Dir` (`rstick up`).
    RightStick(Dir),
    /// Push the D-pad hat in `Dir` (`dpad up`).
    Dpad(Dir),
    /// Toggle mouse-look driving the right analog stick (`AXIS_Z`/`AXIS_RZ`).
    MouseLook { sensitivity: f32 },
    /// Remap an input (e.g. a gamepad button) to a keyboard/mouse keycode (`key SPACE`).
    Key(u16),
}

/// A joystick direction, in screen space (`Up` is toward smaller `y`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    /// The unit vector this direction contributes.
    pub fn vector(self) -> (f32, f32) {
        match self {
            Dir::Up => (0.0, -1.0),
            Dir::Down => (0.0, 1.0),
            Dir::Left => (-1.0, 0.0),
            Dir::Right => (1.0, 0.0),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Dir::Up => "up",
            Dir::Down => "down",
            Dir::Left => "left",
            Dir::Right => "right",
        }
    }

    fn parse(text: &str) -> Option<Dir> {
        match text {
            "up" => Some(Dir::Up),
            "down" => Some(Dir::Down),
            "left" => Some(Dir::Left),
            "right" => Some(Dir::Right),
            _ => None,
        }
    }
}

/// One key binding.
#[derive(Debug, Clone, PartialEq)]
pub struct Binding {
    /// Linux keycode (`KEY_*`).
    pub code: u16,
    pub action: Action,
}

/// A package's keymap.
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    pub bindings: Vec<Binding>,
}

impl Keymap {
    /// Parse the line format. Unknown lines are reported, not silently dropped.
    pub fn parse(text: &str) -> Result<Keymap, String> {
        let mut bindings = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, rest)) = line.split_once('=') else {
                return Err(format!("line {}: expected `KEY = action …`", index + 1));
            };
            let code = keycode(key.trim())
                .ok_or_else(|| format!("line {}: unknown key `{}`", index + 1, key.trim()))?;
            let action = parse_action(rest.trim())
                .map_err(|e| format!("line {}: {e}", index + 1))?;
            bindings.push(Binding { code, action });
        }
        Ok(Keymap { bindings })
    }

    /// Render back to the line format.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for b in &self.bindings {
            let key = keyname(b.code);
            let action = match b.action {
                Action::Pad(btn) => format!("pad {}", btn.name()),
                Action::LeftStick(dir) => format!("lstick {}", dir.name()),
                Action::RightStick(dir) => format!("rstick {}", dir.name()),
                Action::Dpad(dir) => format!("dpad {}", dir.name()),
                Action::MouseLook { sensitivity } => format!("mouselook {sensitivity}"),
                Action::Key(target) => format!("key {}", keyname(target)),
            };
            out.push_str(&format!("{key} = {action}\n"));
        }
        out
    }

    pub fn action_for(&self, code: u16) -> Option<&Action> {
        self.bindings.iter().find(|b| b.code == code).map(|b| &b.action)
    }
}

fn parse_action(text: &str) -> Result<Action, String> {
    let mut parts = text.split_whitespace();
    let kind = parts.next().ok_or("missing action")?;
    let words: Vec<&str> = parts.collect();
    match kind {
        "pad" => {
            let [btn] = words.as_slice() else {
                return Err("pad needs button name (e.g. a, b, x, y, lb, rb, lt, rt, start, back)".into());
            };
            let btn = GamepadButton::parse(btn)
                .ok_or_else(|| format!("unknown gamepad button `{btn}`"))?;
            Ok(Action::Pad(btn))
        }
        "lstick" | "left_stick" => {
            let [dir] = words.as_slice() else {
                return Err("lstick needs up|down|left|right".into());
            };
            let dir = Dir::parse(dir).ok_or("lstick needs up|down|left|right")?;
            Ok(Action::LeftStick(dir))
        }
        "rstick" | "right_stick" => {
            let [dir] = words.as_slice() else {
                return Err("rstick needs up|down|left|right".into());
            };
            let dir = Dir::parse(dir).ok_or("rstick needs up|down|left|right")?;
            Ok(Action::RightStick(dir))
        }
        "dpad" => {
            let [dir] = words.as_slice() else {
                return Err("dpad needs up|down|left|right".into());
            };
            let dir = Dir::parse(dir).ok_or("dpad needs up|down|left|right")?;
            Ok(Action::Dpad(dir))
        }
        "mouselook" => {
            let sensitivity = match words.as_slice() {
                [] => 1.5,
                [val] => {
                    let s = val.parse::<f32>().map_err(|_| format!("`{val}` is not a number"))?;
                    if !s.is_finite() || !(0.05..=20.0).contains(&s) {
                        return Err("mouselook sensitivity must be between 0.05 and 20".into());
                    }
                    s
                }
                _ => return Err("mouselook takes optional sensitivity".into()),
            };
            Ok(Action::MouseLook { sensitivity })
        }
        "key" => {
            let [target] = words.as_slice() else {
                return Err("key needs target key name (e.g. SPACE, W, MOUSE_LEFT)".into());
            };
            let code = keycode(target)
                .ok_or_else(|| format!("unknown target key `{target}`"))?;
            Ok(Action::Key(code))
        }
        other => Err(format!("unknown action `{other}`")),
    }
}

/// `BTN_LEFT`: the primary button. It is the touch pointer; a binding for it
/// takes effect only while mouse-look is on.
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const BTN_SIDE: u16 = 0x113;
pub const BTN_EXTRA: u16 = 0x114;

/// Linux keycode for a name, accepting both `KEY_W` and `W`.
pub fn keycode(name: &str) -> Option<u16> {
    let upper = name.trim().to_ascii_uppercase();
    let bare = upper.strip_prefix("KEY_").unwrap_or(&upper);
    let code = match bare {
        "ESC" | "ESCAPE" => 1,
        "1" => 2,
        "2" => 3,
        "3" => 4,
        "4" => 5,
        "5" => 6,
        "6" => 7,
        "7" => 8,
        "8" => 9,
        "9" => 10,
        "0" => 11,
        "MINUS" => 12,
        "EQUAL" => 13,
        "BACKSPACE" => 14,
        "TAB" => 15,
        "Q" => 16,
        "W" => 17,
        "E" => 18,
        "R" => 19,
        "T" => 20,
        "Y" => 21,
        "U" => 22,
        "I" => 23,
        "O" => 24,
        "P" => 25,
        "LEFTBRACE" => 26,
        "RIGHTBRACE" => 27,
        "ENTER" => 28,
        "LEFTCTRL" | "CTRL" => 29,
        "A" => 30,
        "S" => 31,
        "D" => 32,
        "F" => 33,
        "G" => 34,
        "H" => 35,
        "J" => 36,
        "K" => 37,
        "L" => 38,
        "SEMICOLON" => 39,
        "APOSTROPHE" => 40,
        "GRAVE" => 41,
        "LEFTSHIFT" | "SHIFT" => 42,
        "BACKSLASH" => 43,
        "Z" => 44,
        "X" => 45,
        "C" => 46,
        "V" => 47,
        "B" => 48,
        "N" => 49,
        "M" => 50,
        "COMMA" => 51,
        "DOT" => 52,
        "SLASH" => 53,
        "RIGHTSHIFT" => 54,
        "LEFTALT" | "ALT" => 56,
        "SPACE" => 57,
        "CAPSLOCK" => 58,
        "F1" => 59,
        "F2" => 60,
        "F3" => 61,
        "F4" => 62,
        "F5" => 63,
        "F6" => 64,
        "F7" => 65,
        "F8" => 66,
        "F9" => 67,
        "F10" => 68,
        "F11" => 87,
        "F12" => 88,
        "UP" => 103,
        "LEFT" => 105,
        "RIGHT" => 106,
        "DOWN" => 108,
        "HOME" => 102,
        "END" => 107,
        "PAGEUP" => 104,
        "PAGEDOWN" => 109,
        "INSERT" => 110,
        "DELETE" => 111,
        "RIGHTCTRL" => 97,
        "RIGHTALT" => 100,
        // Mouse buttons other than the left one, which is the touch itself.
        // They share the keycode space (`BTN_RIGHT`, `BTN_MIDDLE`, ...), so a
        // binding can put attack on the right button the way a PC game does.
        // The left button is the touch pointer, so its binding only applies
        // while mouse-look has the pointer locked and there is no cursor to
        // click with.
        "MOUSE_LEFT" | "BTN_LEFT" => BTN_LEFT,
        "MOUSE_RIGHT" | "BTN_RIGHT" => BTN_RIGHT,
        "MOUSE_MIDDLE" | "BTN_MIDDLE" => BTN_MIDDLE,
        "MOUSE_SIDE" | "BTN_SIDE" => BTN_SIDE,
        "MOUSE_EXTRA" | "BTN_EXTRA" => BTN_EXTRA,
        "PAD_A" | "BTN_SOUTH" => 0x130,
        "PAD_B" | "BTN_EAST" => 0x131,
        "PAD_X" | "BTN_NORTH" => 0x133,
        "PAD_Y" | "BTN_WEST" => 0x134,
        "PAD_LB" | "BTN_TL" => 0x136,
        "PAD_RB" | "BTN_TR" => 0x137,
        "PAD_LT" | "BTN_TL2" => 0x138,
        "PAD_RT" | "BTN_TR2" => 0x139,
        "PAD_BACK" | "PAD_SELECT" | "BTN_SELECT" => 0x13a,
        "PAD_START" | "BTN_START" => 0x13b,
        "PAD_GUIDE" | "PAD_MODE" | "BTN_MODE" => 0x13c,
        "PAD_L3" | "BTN_THUMBL" => 0x13d,
        "PAD_R3" | "BTN_THUMBR" => 0x13e,
        "PAD_UP" | "BTN_DPAD_UP" => 0x220,
        "PAD_DOWN" | "BTN_DPAD_DOWN" => 0x221,
        "PAD_LEFT" | "BTN_DPAD_LEFT" => 0x222,
        "PAD_RIGHT" | "BTN_DPAD_RIGHT" => 0x223,
        _ => return None,
    };
    Some(code)
}

/// Inverse of [`keycode`], for rendering.
pub fn keyname(code: u16) -> String {
    const NAMES: &[(u16, &str)] = &[
        (1, "ESC"),
        (2, "1"),
        (3, "2"),
        (4, "3"),
        (5, "4"),
        (6, "5"),
        (7, "6"),
        (8, "7"),
        (9, "8"),
        (10, "9"),
        (11, "0"),
        (14, "BACKSPACE"),
        (15, "TAB"),
        (16, "Q"),
        (17, "W"),
        (18, "E"),
        (19, "R"),
        (20, "T"),
        (21, "Y"),
        (22, "U"),
        (23, "I"),
        (24, "O"),
        (25, "P"),
        (28, "ENTER"),
        (29, "LEFTCTRL"),
        (30, "A"),
        (31, "S"),
        (32, "D"),
        (33, "F"),
        (34, "G"),
        (35, "H"),
        (36, "J"),
        (37, "K"),
        (38, "L"),
        (42, "LEFTSHIFT"),
        (44, "Z"),
        (45, "X"),
        (46, "C"),
        (47, "V"),
        (48, "B"),
        (49, "N"),
        (50, "M"),
        (56, "LEFTALT"),
        (57, "SPACE"),
        (59, "F1"),
        (60, "F2"),
        (61, "F3"),
        (62, "F4"),
        (63, "F5"),
        (64, "F6"),
        (65, "F7"),
        (66, "F8"),
        (67, "F9"),
        (68, "F10"),
        (87, "F11"),
        (88, "F12"),
        (102, "HOME"),
        (103, "UP"),
        (104, "PAGEUP"),
        (105, "LEFT"),
        (106, "RIGHT"),
        (107, "END"),
        (108, "DOWN"),
        (109, "PAGEDOWN"),
        (110, "INSERT"),
        (111, "DELETE"),
        (12, "MINUS"),
        (13, "EQUAL"),
        (26, "LEFTBRACE"),
        (27, "RIGHTBRACE"),
        (39, "SEMICOLON"),
        (40, "APOSTROPHE"),
        (41, "GRAVE"),
        (43, "BACKSLASH"),
        (51, "COMMA"),
        (52, "DOT"),
        (53, "SLASH"),
        (54, "RIGHTSHIFT"),
        (58, "CAPSLOCK"),
        (97, "RIGHTCTRL"),
        (100, "RIGHTALT"),
        (BTN_LEFT, "MOUSE_LEFT"),
        (BTN_RIGHT, "MOUSE_RIGHT"),
        (BTN_MIDDLE, "MOUSE_MIDDLE"),
        (BTN_SIDE, "MOUSE_SIDE"),
        (BTN_EXTRA, "MOUSE_EXTRA"),
        (0x130, "PAD_A"),
        (0x131, "PAD_B"),
        (0x133, "PAD_X"),
        (0x134, "PAD_Y"),
        (0x136, "PAD_LB"),
        (0x137, "PAD_RB"),
        (0x138, "PAD_LT"),
        (0x139, "PAD_RT"),
        (0x13a, "PAD_BACK"),
        (0x13b, "PAD_START"),
        (0x13c, "PAD_GUIDE"),
        (0x13d, "PAD_L3"),
        (0x13e, "PAD_R3"),
        (0x220, "PAD_UP"),
        (0x221, "PAD_DOWN"),
        (0x222, "PAD_LEFT"),
        (0x223, "PAD_RIGHT"),
    ];
    NAMES
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, n)| (*n).to_string())
        .unwrap_or_else(|| format!("KEY_{code}"))
}

/// Where keymaps live: `$XDG_CONFIG_HOME/pengudroid/keymaps/<package>.keymap`.
pub fn keymap_path(package: &str) -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("pengudroid").join("keymaps").join(format!("{package}.keymap"))
}

/// Load a package's keymap, or an empty one when there is no file yet.
pub fn load(package: &str) -> Keymap {
    std::fs::read_to_string(keymap_path(package))
        .ok()
        .and_then(|text| Keymap::parse(&text).ok())
        .unwrap_or_default()
}

/// Write a package's keymap, creating the directory if needed.
pub fn save(package: &str, keymap: &Keymap) -> std::io::Result<PathBuf> {
    let path = keymap_path(package);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, keymap.render())?;
    Ok(path)
}

/// A keymap's bindings indexed by keycode, for the event loop.
pub fn index(keymap: &Keymap) -> BTreeMap<u16, Action> {
    keymap
        .bindings
        .iter()
        .map(|b| (b.code, b.action.clone()))
        .collect()
}

/// Whether `path` looks like a keymap file.
pub fn is_keymap(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "keymap")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_action() {
        let text = "\
# a comment
W = lstick up
SPACE = pad a
UP = dpad up
I = rstick up
LEFTALT = mouselook 2
PAD_A = key SPACE
";
        let map = Keymap::parse(text).expect("parse");
        assert_eq!(map.bindings.len(), 6);
        assert_eq!(map.action_for(17), Some(&Action::LeftStick(Dir::Up)));
        assert_eq!(map.action_for(57), Some(&Action::Pad(GamepadButton::South)));
        assert_eq!(map.action_for(103), Some(&Action::Dpad(Dir::Up)));
        assert_eq!(map.action_for(23), Some(&Action::RightStick(Dir::Up)));
        assert_eq!(map.action_for(56), Some(&Action::MouseLook { sensitivity: 2.0 }));
        assert_eq!(map.action_for(0x130), Some(&Action::Key(57)));
    }

    #[test]
    fn round_trips() {
        let text = "W = lstick up\nSPACE = pad a\nMOUSE_RIGHT = pad b\nLEFTALT = mouselook 1.5\nPAD_X = key R\n";
        let map = Keymap::parse(text).expect("parse");
        assert_eq!(map.render(), text);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(Keymap::parse("NOPE = pad a").is_err());
        assert!(Keymap::parse("W = pad").is_err());
        assert!(Keymap::parse("W = pad nonexistent").is_err());
        assert!(Keymap::parse("W pad a").is_err());
        assert!(Keymap::parse("W = tap 100 200").is_err());
        assert!(Keymap::parse("W = lstick north").is_err());
        assert!(Keymap::parse("W = rstick up extra").is_err());
        assert!(Keymap::parse("W = dpad").is_err());
        assert!(Keymap::parse("W = mouselook 0").is_err());
        assert!(Keymap::parse("W = mouselook NaN").is_err());
        assert!(Keymap::parse("W = key NO_SUCH_KEY").is_err());
    }

    #[test]
    fn mouselook_defaults_to_1_5_sensitivity() {
        let map = Keymap::parse("LEFTALT = mouselook\n").expect("parse");
        assert_eq!(map.action_for(56), Some(&Action::MouseLook { sensitivity: 1.5 }));
        assert_eq!(map.render(), "LEFTALT = mouselook 1.5\n");
    }

    /// A name `render` cannot write back would make the GUI editor silently
    /// corrupt a profile on save.
    #[test]
    fn every_parsable_key_renders_to_a_parsable_name() {
        for code in 1..=0x223u16 {
            let name = keyname(code);
            if name.starts_with("KEY_") {
                continue;
            }
            assert_eq!(keycode(&name), Some(code), "{name}");
        }
        for name in ["MINUS", "GRAVE", "CAPSLOCK", "RIGHTSHIFT", "MOUSE_MIDDLE", "SLASH", "PAD_A", "PAD_RT", "PAD_UP"] {
            let code = keycode(name).expect(name);
            assert_eq!(keyname(code), name);
        }
    }

    #[test]
    fn key_names_round_trip() {
        for name in ["W", "A", "SPACE", "LEFTSHIFT", "F12", "UP", "PAD_A", "PAD_START"] {
            let code = keycode(name).expect(name);
            assert_eq!(keyname(code), name);
        }
        assert_eq!(keycode("KEY_W"), keycode("W"));
    }
}
