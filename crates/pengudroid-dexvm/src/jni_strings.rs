//! JNI UTF-16 and modified UTF-8 strings, including NUL and lone surrogates.
use crate::jni_host::{jni_state, raise_exception};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
fn encode(units: &[u16]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for &unit in units {
        if unit == 0 {
            bytes.extend([0xc0, 0x80]);
        } else if unit < 0x80 {
            bytes.push(unit as u8);
        } else if unit < 0x800 {
            bytes.extend([0xc0 | (unit >> 6) as u8, 0x80 | (unit & 63) as u8]);
        } else {
            bytes.extend([
                0xe0 | (unit >> 12) as u8,
                0x80 | ((unit >> 6) & 63) as u8,
                0x80 | (unit & 63) as u8,
            ]);
        }
    }
    bytes
}
fn decode(bytes: &[u8]) -> Result<Vec<u16>, ()> {
    let mut units = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        i += 1;
        if b > 0 && b < 0x80 {
            units.push(b as u16);
            continue;
        }
        let extra = if b & 0xe0 == 0xc0 {
            1
        } else if b & 0xf0 == 0xe0 {
            2
        } else {
            return Err(());
        };
        let mut unit = (b & if extra == 1 { 31 } else { 15 }) as u16;
        for _ in 0..extra {
            let next = *bytes.get(i).ok_or(())?;
            i += 1;
            if next & 0xc0 != 0x80 {
                return Err(());
            }
            unit = (unit << 6) | (next & 63) as u16;
        }
        if (extra == 1 && unit < 0x80 && !(unit == 0 && b == 0xc0)) || (extra == 2 && unit < 0x800)
        {
            return Err(());
        }
        units.push(unit);
    }
    Ok(units)
}
fn units(string: *mut c_void) -> Option<Vec<u16>> {
    jni_state().lock().unwrap().heap.utf16(string as usize)
}
pub unsafe extern "C" fn new_string(_: *mut c_void, chars: *const u16, len: c_int) -> *mut c_void {
    if len < 0 || (chars.is_null() && len != 0) {
        raise_exception(
            "Ljava/lang/IllegalArgumentException;",
            "invalid JNI UTF-16 input",
        );
        return std::ptr::null_mut();
    }
    let text = if len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(chars, len as usize)
    };
    jni_state().lock().unwrap().heap.new_utf16_string(text) as *mut c_void
}
pub unsafe extern "C" fn get_string_length(_: *mut c_void, string: *mut c_void) -> c_int {
    units(string).map(|v| v.len() as c_int).unwrap_or(0)
}
pub unsafe extern "C" fn get_string_chars(
    _: *mut c_void,
    string: *mut c_void,
    copy: *mut u8,
) -> *const u16 {
    let Some(mut text) = units(string) else {
        return std::ptr::null();
    };
    text.push(0);
    let pin = text.into_boxed_slice();
    let ptr = pin.as_ptr();
    jni_state()
        .lock()
        .unwrap()
        .registry
        .utf16_pins
        .insert(ptr as usize, pin);
    if !copy.is_null() {
        *copy = 1;
    }
    ptr
}
pub unsafe extern "C" fn release_string_chars(_: *mut c_void, _: *mut c_void, chars: *const u16) {
    jni_state()
        .lock()
        .unwrap()
        .registry
        .utf16_pins
        .remove(&(chars as usize));
}
pub unsafe extern "C" fn new_string_utf(_: *mut c_void, bytes: *const c_char) -> *mut c_void {
    if bytes.is_null() {
        return std::ptr::null_mut();
    }
    match decode(CStr::from_ptr(bytes).to_bytes()) {
        Ok(text) => jni_state().lock().unwrap().heap.new_utf16_string(&text) as *mut c_void,
        Err(()) => {
            raise_exception(
                "Ljava/lang/IllegalArgumentException;",
                "invalid modified UTF-8",
            );
            std::ptr::null_mut()
        }
    }
}
pub unsafe extern "C" fn get_string_utf_length(_: *mut c_void, string: *mut c_void) -> c_int {
    units(string)
        .map(|v| encode(&v).len() as c_int)
        .unwrap_or(0)
}
pub unsafe extern "C" fn get_string_utf_chars(
    _: *mut c_void,
    string: *mut c_void,
    copy: *mut u8,
) -> *const c_char {
    let Some(text) = units(string) else {
        return std::ptr::null();
    };
    let pin = CString::new(encode(&text)).unwrap();
    let ptr = pin.as_ptr();
    jni_state()
        .lock()
        .unwrap()
        .registry
        .utf_pins
        .insert(ptr as usize, pin);
    if !copy.is_null() {
        *copy = 1;
    }
    ptr
}
pub unsafe extern "C" fn release_string_utf_chars(
    _: *mut c_void,
    _: *mut c_void,
    chars: *const c_char,
) {
    jni_state()
        .lock()
        .unwrap()
        .registry
        .utf_pins
        .remove(&(chars as usize));
}
fn region(string: *mut c_void, start: c_int, len: c_int) -> Option<Vec<u16>> {
    let text = units(string)?;
    let end = (start as usize).checked_add(len as usize);
    if start < 0 || len < 0 || end.map_or(true, |end| end > text.len()) {
        raise_exception(
            "Ljava/lang/StringIndexOutOfBoundsException;",
            "JNI string region out of bounds",
        );
        return None;
    }
    Some(text[start as usize..end.unwrap()].to_vec())
}
pub unsafe extern "C" fn get_string_region(
    _: *mut c_void,
    string: *mut c_void,
    start: c_int,
    len: c_int,
    out: *mut u16,
) {
    if let Some(text) = region(string, start, len) {
        if !out.is_null() {
            std::ptr::copy_nonoverlapping(text.as_ptr(), out, text.len());
        }
    }
}
pub unsafe extern "C" fn get_string_utf_region(
    _: *mut c_void,
    string: *mut c_void,
    start: c_int,
    len: c_int,
    out: *mut c_char,
) {
    if let Some(text) = region(string, start, len) {
        let bytes = encode(&text);
        if !out.is_null() {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.cast(), bytes.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn jni_utf16_and_modified_utf8_roundtrip_nul_supplementary_and_lone_surrogates() {
        unsafe {
            let env = crate::jni_host::jni_env_ptr();
            let text = [0, 0x41, 0xe9, 0xd83d, 0xde00, 0xd800];
            let string = new_string(env, text.as_ptr(), text.len() as i32);
            assert_eq!(get_string_length(env, string), 6);
            assert_eq!(get_string_utf_length(env, string), 14);
            let mut copy = 0;
            let pin = get_string_chars(env, string, &mut copy);
            let second = get_string_chars(env, string, std::ptr::null_mut());
            assert_eq!(copy, 1);
            assert_eq!(std::slice::from_raw_parts(pin, 6), &text);
            release_string_chars(env, string, second);
            assert_eq!(std::slice::from_raw_parts(pin, 6), &text);
            let utf = get_string_utf_chars(env, string, &mut copy);
            assert_eq!(CStr::from_ptr(utf).to_bytes(), encode(&text));
            let cloned = new_string_utf(env, utf);
            release_string_utf_chars(env, string, utf);
            assert_eq!(units(cloned).unwrap(), text);
            release_string_chars(env, string, pin);
            let mut part = [0u16; 3];
            get_string_region(env, string, 2, 3, part.as_mut_ptr());
            assert_eq!(part, [0xe9, 0xd83d, 0xde00]);
            get_string_region(env, string, 6, 1, part.as_mut_ptr());
            assert!(crate::jni_host::take_pending_exception().is_some());
            for bad in [
                vec![0xf0, 0x9f, 0x98, 0x80],
                vec![0xc1, 0x81],
                vec![0xe0, 0x80, 0x80],
            ] {
                assert!(decode(&bad).is_err());
            }
        }
    }
}
