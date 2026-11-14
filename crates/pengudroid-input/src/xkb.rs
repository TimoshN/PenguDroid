//! The host keyboard layout, through the system's libxkbcommon.
//!
//! Wayland sends raw evdev keycodes plus an XKB keymap and modifier state; the
//! characters a key types (Turkish `ı`/`ş`/`ğ`, `@` on AltGr+Q, a layout the
//! user switched to with a hotkey) live only in that keymap. This module turns
//! a key press into the Unicode character the user's own layout produces, so
//! the guest receives text, not a US-layout guess.
//!
//! libxkbcommon is loaded with `dlopen` (it is on every Wayland desktop) so the
//! daemon gains no build dependency; without it, keys still work as keycodes.

use std::ffi::{c_char, c_int, c_void, CStr};

type ContextNew = unsafe extern "C" fn(c_int) -> *mut c_void;
type KeymapNewFromString = unsafe extern "C" fn(*mut c_void, *const c_char, c_int, c_int) -> *mut c_void;
type StateNew = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type StateUnref = unsafe extern "C" fn(*mut c_void);
type KeymapUnref = unsafe extern "C" fn(*mut c_void);
type StateUpdateMask = unsafe extern "C" fn(*mut c_void, u32, u32, u32, u32, u32, u32) -> c_int;
type StateKeyGetUtf32 = unsafe extern "C" fn(*mut c_void, u32) -> u32;
type KeymapModGetIndex = unsafe extern "C" fn(*mut c_void, *const c_char) -> u32;
type KeymapNewFromNames = unsafe extern "C" fn(*mut c_void, *const RuleNames, c_int) -> *mut c_void;

#[repr(C)]
struct RuleNames {
    rules: *const c_char,
    model: *const c_char,
    layout: *const c_char,
    variant: *const c_char,
    options: *const c_char,
}

struct Api {
    context_new: ContextNew,
    keymap_new_from_string: KeymapNewFromString,
    keymap_unref: KeymapUnref,
    state_new: StateNew,
    state_unref: StateUnref,
    state_update_mask: StateUpdateMask,
    state_key_get_utf32: StateKeyGetUtf32,
    keymap_mod_get_index: KeymapModGetIndex,
    keymap_new_from_names: KeymapNewFromNames,
}

fn api() -> Option<&'static Api> {
    static API: std::sync::OnceLock<Option<Api>> = std::sync::OnceLock::new();
    API.get_or_init(|| unsafe {
        let handle = libc::dlopen(c"libxkbcommon.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if handle.is_null() {
            return None;
        }
        macro_rules! sym {
            ($name:literal) => {{
                let p = libc::dlsym(handle, concat!($name, "\0").as_ptr() as *const c_char);
                if p.is_null() {
                    return None;
                }
                std::mem::transmute::<*mut c_void, _>(p)
            }};
        }
        Some(Api {
            context_new: sym!("xkb_context_new"),
            keymap_new_from_string: sym!("xkb_keymap_new_from_string"),
            keymap_unref: sym!("xkb_keymap_unref"),
            state_new: sym!("xkb_state_new"),
            state_unref: sym!("xkb_state_unref"),
            state_update_mask: sym!("xkb_state_update_mask"),
            state_key_get_utf32: sym!("xkb_state_key_get_utf32"),
            keymap_mod_get_index: sym!("xkb_keymap_mod_get_index"),
            keymap_new_from_names: sym!("xkb_keymap_new_from_names"),
        })
    })
    .as_ref()
}

/// One compiled keymap and its modifier/layout state.
pub struct Layout {
    keymap: *mut c_void,
    state: *mut c_void,
}

// The pointers are only used from the daemon's single Wayland dispatch thread.
unsafe impl Send for Layout {}

impl Layout {
    /// Compile the XKB text keymap the compositor sent (`wl_keyboard.keymap`).
    pub fn from_keymap_text(text: &CStr) -> Option<Layout> {
        let api = api()?;
        unsafe {
            let context = (api.context_new)(0);
            if context.is_null() {
                return None;
            }
            // XKB_KEYMAP_FORMAT_TEXT_V1 = 1, no compile flags. The keymap keeps
            // its own reference to the context.
            Self::with_keymap((api.keymap_new_from_string)(context, text.as_ptr(), 1, 0))
        }
    }

    /// Compile a layout by name (`"tr"`, `"us"`), as xkeyboard-config defines it.
    pub fn from_names(layout: &str, variant: &str) -> Option<Layout> {
        let api = api()?;
        let layout = std::ffi::CString::new(layout).ok()?;
        let variant = std::ffi::CString::new(variant).ok()?;
        let names = RuleNames {
            rules: std::ptr::null(),
            model: std::ptr::null(),
            layout: layout.as_ptr(),
            variant: variant.as_ptr(),
            options: std::ptr::null(),
        };
        unsafe {
            let context = (api.context_new)(0);
            if context.is_null() {
                return None;
            }
            Self::with_keymap((api.keymap_new_from_names)(context, &names, 0))
        }
    }

    unsafe fn with_keymap(keymap: *mut c_void) -> Option<Layout> {
        let api = api()?;
        if keymap.is_null() {
            return None;
        }
        let state = (api.state_new)(keymap);
        if state.is_null() {
            (api.keymap_unref)(keymap);
            return None;
        }
        Some(Layout { keymap, state })
    }

    /// What evdev key `keycode` types in layout `group` with exactly these
    /// modifiers held. AltGr is XKB's third level (`Mod5` in every stock
    /// layout). Leaves the live modifier state cleared.
    pub fn character_with(&mut self, keycode: u32, group: u32, shift: bool, altgr: bool, caps: bool) -> Option<char> {
        let api = api()?;
        let keymap = self.keymap;
        let bit = |name: &CStr| -> u32 {
            let index = unsafe { (api.keymap_mod_get_index)(keymap, name.as_ptr()) };
            if index < 32 { 1 << index } else { 0 }
        };
        let mut depressed = 0;
        if shift { depressed |= bit(c"Shift"); }
        if altgr { depressed |= bit(c"Mod5"); }
        let locked = if caps { bit(c"Lock") } else { 0 };
        self.update_modifiers(depressed, 0, locked, group);
        let c = self.character(keycode);
        self.update_modifiers(0, 0, 0, group);
        c
    }

    /// `wl_keyboard.modifiers`: the compositor is the authority on Shift,
    /// AltGr, Caps Lock and which layout group (e.g. TR or US) is active.
    pub fn update_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        if let Some(api) = api() {
            unsafe { (api.state_update_mask)(self.state, depressed, latched, locked, 0, 0, group) };
        }
    }

    /// The character evdev key `keycode` types now, if it types one.
    /// Control characters (Enter, Tab, Backspace, Escape, Ctrl chords) are not
    /// text and stay key events.
    pub fn character(&self, keycode: u32) -> Option<char> {
        let api = api()?;
        // XKB keycodes are evdev codes + 8.
        let code = unsafe { (api.state_key_get_utf32)(self.state, keycode + 8) };
        char::from_u32(code).filter(|c| !c.is_control())
    }
}

impl Drop for Layout {
    fn drop(&mut self) {
        if let Some(api) = api() {
            unsafe {
                (api.state_unref)(self.state);
                (api.keymap_unref)(self.keymap);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keymap(layout: &str) -> Option<Layout> {
        Layout::from_names(layout, "")
    }

    #[test]
    fn turkish_layout_types_turkish_letters() {
        let Some(mut tr) = keymap("tr") else { eprintln!("skip: no libxkbcommon/xkeyboard-config"); return };
        // evdev: KEY_I = 23, KEY_APOSTROPHE (TR `i`) = 40, KEY_SEMICOLON = 39,
        // KEY_LEFTBRACE = 26, KEY_A = 30.
        assert_eq!(tr.character(23), Some('ı'));
        assert_eq!(tr.character(40), Some('i'));
        assert_eq!(tr.character(39), Some('ş'));
        assert_eq!(tr.character(26), Some('ğ'));
        assert_eq!(tr.character(30), Some('a'));
        // Shift (mod index 0 -> mask 1): dotted capital İ on the `i` key.
        tr.update_modifiers(1, 0, 0, 0);
        assert_eq!(tr.character(40), Some('İ'));
        assert_eq!(tr.character(23), Some('I'));
        tr.update_modifiers(0, 0, 0, 0);
        // AltGr+Q is @, and Caps Lock capitalises the dotted i to İ.
        assert_eq!(tr.character_with(16, 0, false, true, false), Some('@'));
        assert_eq!(tr.character_with(40, 0, false, false, true), Some('İ'));
        assert_eq!(tr.character_with(23, 0, false, false, true), Some('I'));
        // Enter and Backspace are keys, not text.
        assert_eq!(tr.character(28), None);
        assert_eq!(tr.character(14), None);
    }

    #[test]
    fn us_layout_matches_the_keycode_table() {
        let Some(us) = keymap("us") else { eprintln!("skip: no libxkbcommon/xkeyboard-config"); return };
        assert_eq!(us.character(23), Some('i'));
        assert_eq!(us.character(39), Some(';'));
    }
}
