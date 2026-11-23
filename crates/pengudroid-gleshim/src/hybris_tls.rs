//! Linux/Hybris TLS separation, with independent architecture backends.
//!
//! The dispatch in this file contains no ELF offsets. Each backend validates
//! its own mapped instructions before changing a library's gTLS key.

#[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
use std::ffi::{c_int, c_void};

#[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
use super::{cast_fn, sym_ptr};

#[cfg(target_arch = "aarch64")]
#[path = "hybris_tls/aarch64.rs"]
mod imp;

#[cfg(target_arch = "arm")]
#[path = "hybris_tls/arm.rs"]
mod imp;

#[cfg(target_arch = "aarch64")]
pub fn rekey_linux_binder() {
    imp::rekey_linux_binder();
}

#[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
pub fn rekey_egl() {
    imp::rekey_egl();
}

#[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
pub fn rekey_gralloc() {
    if super::hybris_host() {
        imp::rekey_hybris_hidl();
    }
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "arm")))]
pub fn rekey_gralloc() {}

/// Clear a legacy key through the host pthread implementation. Bionic and
/// glibc key values are not interchangeable, so architecture backends validate
/// their ELF and gTLS layouts before reaching here.
#[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
fn clear_legacy_tls_key(old: u32) {
    thread_local! { static CLEARED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
    CLEARED.with(|cleared| {
        if cleared.get() {
            return;
        }
        type SetSpecific = unsafe extern "C" fn(u32, *mut c_void) -> c_int;
        let handle = unsafe { libc::dlopen(c"libc.so.6".as_ptr(), libc::RTLD_NOW) };
        if let Some(clear) = sym_ptr(handle, "pthread_setspecific").and_then(cast_fn) {
            let clear: SetSpecific = clear;
            let _ = unsafe { clear(old, std::ptr::null_mut()) };
        }
        cleared.set(true);
    });
}
