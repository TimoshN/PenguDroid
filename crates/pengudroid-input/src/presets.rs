//! Built-in keyboard/mouse <-> Xbox 360 controller keymaps.
//!
//! All presets use standard Xbox 360 gamepad controls so games receive native
//! PC-style WASD/mouse/controller input without screen-coordinate touch hacks.

use crate::keymap::{keycode, Action, Binding, Dir, GamepadButton, Keymap};

fn build(entries: &[(&str, Action)]) -> Keymap {
    let bindings = entries
        .iter()
        .map(|(key, action)| Binding {
            code: keycode(key).expect("preset key names are valid"),
            action: action.clone(),
        })
        .collect();
    Keymap { bindings }
}

/// Default PC keyboard + mouse -> Xbox 360 controller layout.
pub fn default_xbox_pc_preset() -> Keymap {
    build(&[
        ("W", Action::LeftStick(Dir::Up)),
        ("A", Action::LeftStick(Dir::Left)),
        ("S", Action::LeftStick(Dir::Down)),
        ("D", Action::LeftStick(Dir::Right)),
        ("I", Action::RightStick(Dir::Up)),
        ("K", Action::RightStick(Dir::Down)),
        ("J", Action::RightStick(Dir::Left)),
        ("L", Action::RightStick(Dir::Right)),
        ("UP", Action::Dpad(Dir::Up)),
        ("DOWN", Action::Dpad(Dir::Down)),
        ("LEFT", Action::Dpad(Dir::Left)),
        ("RIGHT", Action::Dpad(Dir::Right)),
        ("SPACE", Action::Pad(GamepadButton::South)),
        ("LEFTSHIFT", Action::Pad(GamepadButton::East)),
        ("F", Action::Pad(GamepadButton::North)),
        ("R", Action::Pad(GamepadButton::West)),
        ("Q", Action::Pad(GamepadButton::Tl)),
        ("E", Action::Pad(GamepadButton::Tr)),
        ("1", Action::Pad(GamepadButton::Tl2)),
        ("2", Action::Pad(GamepadButton::Tr2)),
        ("ENTER", Action::Pad(GamepadButton::Start)),
        ("TAB", Action::Pad(GamepadButton::Select)),
        ("LEFTCTRL", Action::Pad(GamepadButton::Thumbl)),
        ("V", Action::Pad(GamepadButton::Thumbr)),
        ("LEFTALT", Action::MouseLook { sensitivity: 1.5 }),
    ])
}

/// Beach Buggy Racing 2 Xbox 360 driving + menu preset.
fn bbr2_preset() -> Keymap {
    build(&[
        ("W", Action::LeftStick(Dir::Up)),
        ("S", Action::LeftStick(Dir::Down)),
        ("A", Action::LeftStick(Dir::Left)),
        ("D", Action::LeftStick(Dir::Right)),
        ("UP", Action::Dpad(Dir::Up)),
        ("DOWN", Action::Dpad(Dir::Down)),
        ("LEFT", Action::Dpad(Dir::Left)),
        ("RIGHT", Action::Dpad(Dir::Right)),
        ("SPACE", Action::Pad(GamepadButton::South)),     // A: Powerup / Boost / Select
        ("E", Action::Pad(GamepadButton::Tr2)),           // RT: Gas / Throttle
        ("Q", Action::Pad(GamepadButton::Tl2)),           // LT: Brake / Reverse
        ("LEFTSHIFT", Action::Pad(GamepadButton::North)), // X: Drift / Powerup 2
        ("F", Action::Pad(GamepadButton::West)),          // Y: Special Ability
        ("R", Action::Pad(GamepadButton::East)),          // B: Back / Cancel
        ("ENTER", Action::Pad(GamepadButton::Start)),     // Start: Pause
        ("TAB", Action::Pad(GamepadButton::Select)),      // Back: Camera / HUD
        ("MOUSE_RIGHT", Action::Pad(GamepadButton::North)),
    ])
}

/// Packages that ship Beach Buggy Racing 2.
const BBR2_PACKAGES: &[&str] = &[
    "com.vectorunit.cobalt.googleplay",
    "com.vectorunit.purple.googleplay",
];

/// Packages that ship the Genshin Impact client.
const GENSHIN_PACKAGES: &[&str] = &[
    "com.miHoYo.GenshinImpact",
    "com.miHoYo.Yuanshen",
    "com.miHoYo.ys.bilibili",
];

/// Every package that has a built-in preset.
pub fn packages() -> Vec<&'static str> {
    let mut out = Vec::with_capacity(BBR2_PACKAGES.len() + GENSHIN_PACKAGES.len());
    out.extend_from_slice(BBR2_PACKAGES);
    out.extend_from_slice(GENSHIN_PACKAGES);
    out
}

/// The built-in Xbox 360 keymap for `package`, if any.
pub fn for_package(package: &str, width: f32, height: f32) -> Option<Keymap> {
    if !(width.is_finite() && height.is_finite()) || width < 1.0 || height < 1.0 {
        return None;
    }
    if BBR2_PACKAGES.contains(&package) {
        Some(bbr2_preset())
    } else if GENSHIN_PACKAGES.contains(&package) {
        Some(default_xbox_pc_preset())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bbr2_and_genshin_presets_map_to_xbox_gamepad() {
        let bbr2 = for_package("com.vectorunit.cobalt.googleplay", 1280.0, 720.0).expect("preset");
        assert_eq!(bbr2.action_for(keycode("A").unwrap()), Some(&Action::LeftStick(Dir::Left)));
        assert_eq!(bbr2.action_for(keycode("E").unwrap()), Some(&Action::Pad(GamepadButton::Tr2)));

        let genshin = for_package("com.miHoYo.GenshinImpact", 1920.0, 1080.0).expect("preset");
        assert_eq!(genshin.action_for(keycode("W").unwrap()), Some(&Action::LeftStick(Dir::Up)));
        assert_eq!(genshin.action_for(keycode("SPACE").unwrap()), Some(&Action::Pad(GamepadButton::South)));
    }

    #[test]
    fn every_preset_survives_a_round_trip() {
        for package in packages() {
            let map = for_package(package, 1280.0, 720.0).expect(package);
            let parsed = Keymap::parse(&map.render()).expect("render parses");
            assert_eq!(parsed.bindings, map.bindings);
        }
        let default = default_xbox_pc_preset();
        let parsed = Keymap::parse(&default.render()).expect("render parses");
        assert_eq!(parsed.bindings, default.bindings);
    }

    #[test]
    fn unknown_packages_and_bad_sizes_have_no_preset() {
        assert!(for_package("org.schabi.newpipe", 1280.0, 720.0).is_none());
        assert!(for_package("com.vectorunit.cobalt.googleplay", 0.0, 720.0).is_none());
        assert!(for_package("com.miHoYo.GenshinImpact", f32::NAN, 720.0).is_none());
    }
}
