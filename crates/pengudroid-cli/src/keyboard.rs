//! Physical keyboard state for Android input, independent of JNI.
use std::collections::HashMap;

#[derive(Default)]
pub struct Keyboard {
    held: HashMap<u32, (i64, i32)>,
    caps_lock: bool,
}

pub struct Key {
    pub code: i32,
    pub action: i32,
    pub down_time: i64,
    pub repeat: i32,
    pub meta: i32,
}

impl Keyboard {
    pub fn event(&mut self, scan: u32, pressed: bool, now: i64) -> Option<Key> {
        let code = android_keycode(scan)?;
        let (down_time, repeat) = if pressed {
            let entry = self.held.entry(scan).or_insert((now, -1));
            entry.1 = entry.1.saturating_add(1);
            if scan == 58 && entry.1 == 0 {
                self.caps_lock = !self.caps_lock;
            }
            *entry
        } else {
            self.held.remove(&scan).unwrap_or((now, 0))
        };
        // Android meta flags include both the side-specific and aggregate bits.
        let mut meta = if self.caps_lock { 0x100000 } else { 0 };
        for (scan, bits) in [
            (42, 0x41), (54, 0x81), // shift
            (56, 0x12), (100, 0x22), // alt
            (29, 0x3000), (97, 0x5000), // ctrl
            (125, 0x30000), (126, 0x50000), // meta
        ] {
            if self.held.contains_key(&scan) {
                meta |= bits;
            }
        }
        Some(Key { code, action: if pressed { 0 } else { 1 }, down_time, repeat, meta })
    }
}

/// Map a Linux input keycode (`KEY_*`) to an Android `KeyEvent` key code.
///
/// Physical US-layout keys, editing/navigation keys and modifiers. The host
/// compositor keymap and IME text composition are not translated here.
/// Unknown keys return `None` rather than dispatching a wrong character.
pub fn android_keycode(linux: u32) -> Option<i32> {
    Some(match linux {
        // Desktop keyboards have no dedicated Back key. Escape is the
        // conventional stand-in (the Android emulator maps it the same way);
        // `KEYCODE_ESCAPE` is not handled by most apps, so offering it would
        // leave the user unable to leave a screen without the mouse.
        1 => 4, // KEY_ESC -> KEYCODE_BACK
        2 => 8, // KEY_1
        3 => 9,
        4 => 10,
        5 => 11,
        6 => 12,
        7 => 13,
        8 => 14,
        9 => 15,
        10 => 16,
        11 => 7,  // KEY_0
        12 => 69, // KEY_MINUS
        13 => 70, // KEY_EQUAL
        14 => 67, // KEY_BACKSPACE
        15 => 61, // KEY_TAB
        // q w e r t y u i o p
        16 => 45,
        17 => 51,
        18 => 33,
        19 => 46,
        20 => 48,
        21 => 53,
        22 => 49,
        23 => 37,
        24 => 43,
        25 => 44,
        26 => 71, // KEY_LEFTBRACE
        27 => 72, // KEY_RIGHTBRACE
        28 => 66, // KEY_ENTER
        // a s d f g h j k l
        29 => 113, // KEY_LEFTCTRL
        30 => 29,
        31 => 47,
        32 => 32,
        33 => 34,
        34 => 35,
        35 => 36,
        36 => 38,
        37 => 39,
        38 => 40,
        39 => 74, // KEY_SEMICOLON
        40 => 75, // KEY_APOSTROPHE
        41 => 68, // KEY_GRAVE
        42 => 59, // KEY_LEFTSHIFT
        43 => 73, // KEY_BACKSLASH
        // z x c v b n m
        44 => 54,
        45 => 52,
        46 => 31,
        47 => 50,
        48 => 30,
        49 => 42,
        50 => 41,
        51 => 55,  // KEY_COMMA
        52 => 56,  // KEY_DOT
        53 => 76,  // KEY_SLASH
        54 => 60, // KEY_RIGHTSHIFT
        56 => 57, // KEY_LEFTALT
        58 => 115, // KEY_CAPSLOCK
        97 => 114, // KEY_RIGHTCTRL
        100 => 58, // KEY_RIGHTALT
        102 => 122, // KEY_HOME
        104 => 92, // KEY_PAGEUP
        107 => 123, // KEY_END
        109 => 93, // KEY_PAGEDOWN
        110 => 124, // KEY_INSERT
        111 => 112, // KEY_DELETE
        125 => 117, // KEY_LEFTMETA
        126 => 118, // KEY_RIGHTMETA
        158 => 4, // KEY_BACK -> KEYCODE_BACK (laptop/media keyboards)
        57 => 62,  // KEY_SPACE
        103 => 19, // KEY_UP
        105 => 21, // KEY_LEFT
        106 => 22, // KEY_RIGHT
        108 => 20, // KEY_DOWN
        _ => return None,
    })
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_and_back_keys_reach_android_back() {
        // Without this a desktop user cannot leave an Android screen from the
        // keyboard: `KEYCODE_ESCAPE` is not a substitute for `KEYCODE_BACK`.
        assert_eq!(android_keycode(1), Some(4));
        assert_eq!(android_keycode(158), Some(4));
        // The rest of the table stays untouched.
        assert_eq!(android_keycode(2), Some(8));
        assert_eq!(android_keycode(28), Some(66));
    }

    #[test]
    fn shift_chord_retains_other_side_until_released() {
        let mut keyboard = Keyboard::default();
        keyboard.event(42, true, 10);
        keyboard.event(54, true, 11);
        assert_eq!(keyboard.event(30, true, 12).unwrap().meta, 0xc1);
        assert_eq!(keyboard.event(42, false, 13).unwrap().meta, 0x81);
        assert_eq!(keyboard.event(30, false, 14).unwrap().meta, 0x81);
        assert_eq!(keyboard.event(54, false, 15).unwrap().meta, 0);
    }

    #[test]
    fn repeat_and_release_keep_original_down_time() {
        let mut keyboard = Keyboard::default();
        assert_eq!(keyboard.event(30, true, 10).unwrap().repeat, 0);
        let repeat = keyboard.event(30, true, 20).unwrap();
        assert_eq!((repeat.down_time, repeat.repeat), (10, 1));
        let up = keyboard.event(30, false, 30).unwrap();
        assert_eq!((up.down_time, up.action), (10, 1));
        assert_eq!(keyboard.event(30, true, 40).unwrap().down_time, 40);
    }

    #[test]
    fn caps_lock_toggles_once_per_press() {
        let mut keyboard = Keyboard::default();
        keyboard.event(58, true, 1);
        keyboard.event(58, true, 2);
        keyboard.event(58, false, 3);
        assert_eq!(keyboard.event(30, true, 4).unwrap().meta, 0x100000);
        keyboard.event(58, true, 5);
        assert_eq!(keyboard.event(30, false, 6).unwrap().meta, 0);
    }


    #[test]
    fn shortcuts_and_editing_keys_reach_android() {
        let mut keyboard = Keyboard::default();
        keyboard.event(29, true, 1);
        let select_all = keyboard.event(30, true, 2).unwrap();
        assert_eq!((select_all.code, select_all.meta), (29, 0x3000));
        assert_eq!(android_keycode(111), Some(112)); // forward delete
        assert_eq!(android_keycode(102), Some(122)); // home
        assert!(keyboard.event(999, true, 3).is_none());
    }
}

/// What one Android key types under each modifier combination.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyChars {
    pub code: i32,
    pub base: char,
    pub shift: char,
    pub caps: Option<char>,
    pub caps_shift: Option<char>,
    pub altgr: Option<char>,
    pub altgr_shift: Option<char>,
}

/// The US physical layout, used when the desktop's keymap is unavailable.
pub fn us_keys() -> Vec<KeyChars> {
    let mut keys: Vec<KeyChars> = (0..26)
        .map(|i| {
            let (lower, upper) = ((b'a' + i as u8) as char, (b'A' + i as u8) as char);
            KeyChars { code: 29 + i, base: lower, shift: upper, caps: Some(upper), caps_shift: Some(lower), altgr: None, altgr_shift: None }
        })
        .collect();
    let plain = |code: i32, base: char, shift: char| KeyChars { code, base, shift, caps: None, caps_shift: None, altgr: None, altgr_shift: None };
    for (i, (base, shift)) in "0123456789".chars().zip(")!@#$%^&*(".chars()).enumerate() {
        keys.push(plain(7 + i as i32, base, shift));
    }
    for (code, base, shift) in [
        (55, ',', '<'), (56, '.', '>'), (62, ' ', ' '), (61, '\t', '\t'),
        (66, '\n', '\n'), (68, '`', '~'), (69, '-', '_'), (70, '=', '+'),
        (71, '[', '{'), (72, ']', '}'), (73, '\\', '|'), (74, ';', ':'),
        (75, '\'', '"'), (76, '/', '?'),
    ] {
        keys.push(plain(code, base, shift));
    }
    keys.sort_by_key(|k| k.code);
    keys
}

/// What each key types in the desktop's own layout (first layout group), so
/// Android's KeyCharacterMap answers `getUnicodeChar` the way the user's
/// keyboard is labelled: Turkish `ı` on I, `i` on the apostrophe key, `@` on
/// AltGr+Q. Key codes stay physical, so games keep their bindings.
pub fn layout_keys(layout: &mut pengudroid_input::xkb::Layout) -> Vec<KeyChars> {
    let mut keys = Vec::new();
    for linux in 1..=127u32 {
        let Some(code) = android_keycode(linux) else { continue };
        // Tab and Enter are control characters, which XKB does not report as text.
        if code == 61 || code == 66 {
            let c = if code == 61 { '\t' } else { '\n' };
            keys.push(KeyChars { code, base: c, shift: c, caps: None, caps_shift: None, altgr: None, altgr_shift: None });
            continue;
        }
        let Some(base) = layout.character_with(linux, 0, false, false, false) else { continue };
        let shift = layout.character_with(linux, 0, true, false, false).unwrap_or(base);
        let caps = layout.character_with(linux, 0, false, false, true).filter(|c| *c != base);
        let caps_shift = caps.and_then(|_| layout.character_with(linux, 0, true, false, true));
        let altgr = layout.character_with(linux, 0, false, true, false).filter(|c| *c != base);
        let altgr_shift = layout.character_with(linux, 0, true, true, false).filter(|c| Some(*c) != altgr && *c != shift);
        keys.push(KeyChars { code, base, shift, caps, caps_shift, altgr, altgr_shift });
    }
    keys.sort_by_key(|k| k.code);
    keys.dedup_by_key(|k| k.code);
    keys
}

/// The desktop's layout from the keymap the daemon published, or US.
pub fn desktop_keys() -> (Vec<KeyChars>, &'static str) {
    let text = std::fs::read(pengudroid_proto::keymap_path()).ok().and_then(|bytes| std::ffi::CString::new(bytes).ok());
    match text.and_then(|t| pengudroid_input::xkb::Layout::from_keymap_text(&t)) {
        Some(mut layout) => (layout_keys(&mut layout), "desktop keymap"),
        None => (us_keys(), "US default"),
    }
}

/// Android 17's native KeyCharacterMap parcel, including the JNI wrapper's
/// device id and presence flag. Native read/writeToParcel validate this layout
/// again at runtime. See frameworks/native/libs/input/KeyCharacterMap.cpp.
pub fn character_map_parcel(keys: &[KeyChars]) -> Vec<u8> {
    const SHIFT: i32 = 0x1;
    const ALT_RIGHT: i32 = 0x20;
    const CAPS: i32 = 0x100000;
    // Device id, presence, empty length-prefixed String8 (length and NUL),
    // keyboard type (FULL = 4), overlay flag and key count.
    let mut words = vec![-1i32, 1, 0, 0, 4, 0, keys.len() as i32];
    for key in keys {
        let number = if key.base.is_ascii_digit() { key.base as i32 } else { 0 };
        let label = key.shift.to_uppercase().next().unwrap_or(key.shift);
        words.extend([key.code, label as i32, number]);
        // Most specific first: native getKeyBehavior takes the first match.
        let mut behaviors = Vec::new();
        if let Some(c) = key.altgr_shift { behaviors.push((ALT_RIGHT | SHIFT, c)); }
        if let Some(c) = key.altgr { behaviors.push((ALT_RIGHT, c)); }
        if let Some(c) = key.caps_shift { behaviors.push((CAPS | SHIFT, c)); }
        if let Some(c) = key.caps { behaviors.push((CAPS, c)); }
        behaviors.push((SHIFT, key.shift));
        behaviors.push((0, key.base));
        for (meta, c) in behaviors {
            words.extend([1, meta, c as i32, 0, 0]);
        }
        words.push(0); // end of behavior list
    }
    words.extend([0, 0, 0]); // no key-code, scan-code or HID usage overlays
    words.into_iter().flat_map(i32::to_le_bytes).collect()
}

/// The US map, as the parcel above.
pub fn virtual_character_map_parcel() -> Vec<u8> {
    character_map_parcel(&us_keys())
}

#[cfg(test)]
mod parcel_tests {
    use super::*;

    #[test]
    fn turkish_desktop_layout_becomes_the_android_character_map() {
        let Some(mut tr) = pengudroid_input::xkb::Layout::from_names("tr", "") else {
            eprintln!("skip: no libxkbcommon/xkeyboard-config");
            return;
        };
        let keys = layout_keys(&mut tr);
        let key = |code: i32| keys.iter().find(|k| k.code == code).cloned().unwrap();
        // KEYCODE_I types dotless ı / I; the apostrophe key types i / İ.
        assert_eq!((key(37).base, key(37).shift), ('ı', 'I'));
        assert_eq!((key(75).base, key(75).shift), ('i', 'İ'));
        assert_eq!(key(75).caps, Some('İ'));
        // KEYCODE_SEMICOLON is ş, KEYCODE_LEFT_BRACKET ğ, AltGr+Q (KEYCODE_Q) @.
        assert_eq!(key(74).base, 'ş');
        assert_eq!(key(71).base, 'ğ');
        assert_eq!(key(45).altgr, Some('@'));
        // Tab and Enter still type their control characters.
        assert_eq!(key(61).base, '\t');
        assert_eq!(key(66).base, '\n');
        // The parcel is well formed for any layout.
        let bytes = character_map_parcel(&keys);
        assert_eq!(bytes.len() % 4, 0);
    }

    #[test]
    fn us_fallback_matches_the_previous_table() {
        let keys = us_keys();
        assert_eq!(keys.iter().find(|k| k.code == 29).map(|k| (k.base, k.shift)), Some(('a', 'A')));
        assert_eq!(keys.iter().find(|k| k.code == 8).map(|k| (k.base, k.shift)), Some(('1', '!')));
    }

    #[test]
    fn api37_character_map_uses_string8_and_all_three_overlay_lists() {
        let bytes = virtual_character_map_parcel();
        assert_eq!(bytes.len() % 4, 0);
        let words: Vec<_> = bytes.chunks_exact(4)
            .map(|word| i32::from_le_bytes(word.try_into().unwrap())).collect();
        assert_eq!(&words[..6], &[-1, 1, 0, 0, 4, 0]);
        assert!(words[6] > 0);
        let mut cursor = 7;
        let mut previous_key = -1;
        for _ in 0..words[6] {
            assert!(words[cursor] > previous_key, "native keys must be sorted");
            previous_key = words[cursor];
            cursor += 3; // key code, display label, digit
            while words[cursor] != 0 {
                assert_eq!(words[cursor], 1, "behavior presence flag");
                cursor += 5; // presence, meta, character, fallback, replacement
            }
            cursor += 1;
        }
        assert_eq!(&words[cursor..], &[0, 0, 0]);
    }
}
