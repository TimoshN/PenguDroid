//! AArch64 Android 17 / Aurora Hybris TLS layout.
//!
//! Every offset here is specific to the verified AArch64 GSI and Dialog-S
//! Hybris libraries. Do not share these values with the ARMv7 backend.

use std::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};

use super::clear_legacy_tls_key;
use crate::{cast_fn, mapped_elf_base, sym_ptr};

fn clear_legacy_linux_binder_key(old: u32) {
    clear_legacy_tls_key(old);
}

fn clear_legacy_hybris_hidl_key(old: u32) {
    clear_legacy_tls_key(old);
}

/// Separate guest android::IPCThreadState from Hybris hardware::IPCThreadState.
pub fn rekey_linux_binder() {
    static DONE: AtomicBool = AtomicBool::new(false);
    static LEGACY_KEY: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(2);
    if DONE.load(Ordering::Relaxed) {
        clear_legacy_linux_binder_key(LEGACY_KEY.load(Ordering::Relaxed));
        return;
    }
    unsafe {
        let Some(binder) = mapped_elf_base("/data/lib/libbinder.so") else {
            eprintln!("[gleshim] guest libbinder mapping is absent for TLS separation");
            return;
        };
        // Guest android::IPCThreadState is 496 bytes; Hybris' hardware state is
        // 520. If both use the same low Bionic key, Hybris reads +512 beyond the
        // guest object and interprets BR_REPLY as mCallRestriction.
        if std::ptr::read_unaligned((binder + 0x54458) as *const u64) != 0xa9014ff4a9be7bfd {
            eprintln!("[gleshim] guest IPCThreadState signature mismatch");
            return;
        }
        type KeyCreate =
            unsafe extern "C" fn(*mut u32, Option<unsafe extern "C" fn(*mut c_void)>) -> c_int;
        type GetSpecific = unsafe extern "C" fn(u32) -> *mut c_void;
        type SetSpecific = unsafe extern "C" fn(u32, *mut c_void) -> c_int;
        let handle = libc::dlopen(c"libc.so.6".as_ptr(), libc::RTLD_NOW);
        if handle.is_null() {
            return;
        }
        let (Some(create), Some(get), Some(set)) = (
            sym_ptr(handle, "pthread_key_create").and_then(cast_fn),
            sym_ptr(handle, "pthread_getspecific").and_then(cast_fn),
            sym_ptr(handle, "pthread_setspecific").and_then(cast_fn),
        ) else {
            return;
        };
        let (create, get, set): (KeyCreate, GetSpecific, SetSpecific) = (create, get, set);
        let key = (binder + 0xe8298) as *mut u32;
        let old = *key;
        if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
            eprintln!("[gleshim] guest Binder TLS key before separation={old}");
        }
        if old >= 16 {
            DONE.store(true, Ordering::Relaxed);
            return;
        }
        let current = get(old);
        let mut new = 0u32;
        while new < 16 {
            if create(&mut new, None) != 0 {
                return;
            }
        }
        if set(new, current) != 0 {
            return;
        }
        *key = new;
        if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
            eprintln!(
                "[gleshim] separated guest Binder TLS key {old} -> {new} (state={current:p})"
            );
        }
        // Leave no stale state under the old key for Hybris' different 520-byte
        // hardware::IPCThreadState. Its next self() constructs its own object.
        LEGACY_KEY.store(old, Ordering::Relaxed);
        clear_legacy_linux_binder_key(old);
        DONE.store(true, Ordering::Relaxed);
    }
}

pub fn rekey_hybris_hidl() {
    static DONE: AtomicBool = AtomicBool::new(false);
    static LEGACY_KEY: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(2);
    if DONE.load(Ordering::Relaxed) {
        clear_legacy_hybris_hidl_key(LEGACY_KEY.load(Ordering::Relaxed));
        return;
    }
    unsafe {
        let Some(base) = mapped_elf_base("/usr/libexec/droid-hybris/system/lib64/libhidlbase.so")
        else {
            return;
        };
        if std::ptr::read_unaligned((base + 0x88af0) as *const u64) != 0xa9014ff4a9be7bfd {
            return;
        }
        type KeyCreate =
            unsafe extern "C" fn(*mut u32, Option<unsafe extern "C" fn(*mut c_void)>) -> c_int;
        type SetSpecific = unsafe extern "C" fn(u32, *mut c_void) -> c_int;
        let handle = libc::dlopen(c"libc.so.6".as_ptr(), libc::RTLD_NOW);
        if handle.is_null() {
            return;
        }
        let (Some(create), Some(set)) = (
            sym_ptr(handle, "pthread_key_create").and_then(cast_fn),
            sym_ptr(handle, "pthread_setspecific").and_then(cast_fn),
        ) else {
            return;
        };
        let (create, set): (KeyCreate, SetSpecific) = (create, set);
        // Initialize gTLS before replacing it; otherwise the first self() call
        // overwrites the new key with the library's default low key.
        type IpcSelf = unsafe extern "C" fn() -> *mut c_void;
        let self_fn: IpcSelf = std::mem::transmute(base + 0x88af0);
        let _ = self_fn();
        let key = (base + 0xa5c9c) as *mut u32;
        let old = *key;
        if old >= 16 {
            DONE.store(true, Ordering::Relaxed);
            return;
        }
        let mut new = 0u32;
        while new < 16 {
            if create(&mut new, None) != 0 {
                return;
            }
        }
        // Do not migrate the corrupt object. A fresh hardware::IPCThreadState
        // must be constructed under the new key on each thread.
        let _ = set(old, std::ptr::null_mut());
        *key = new;
        LEGACY_KEY.store(old, Ordering::Relaxed);
        DONE.store(true, Ordering::Relaxed);
        if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
            eprintln!("[gleshim] separated Hybris HIDL TLS key {old} -> {new}");
        }
    }
}

pub fn rekey_egl() {
    rekey_linux_binder();
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.load(Ordering::Relaxed) {
        return;
    }
    let Some(egl) = mapped_elf_base("/usr/libexec/droid-hybris/system/lib64/libEGL.so") else {
        if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
            eprintln!("[gleshim] rekey: internal libEGL mapping is absent");
        }
        return;
    };
    let Some(bionic) = mapped_elf_base("/usr/libexec/droid-hybris/system/lib64/libc.so") else {
        if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
            eprintln!("[gleshim] rekey: Hybris Bionic libc mapping is absent");
        }
        return;
    };
    type KeyCreate =
        unsafe extern "C" fn(*mut u32, Option<unsafe extern "C" fn(*mut c_void)>) -> c_int;
    type GetSpecific = unsafe extern "C" fn(u32) -> *mut c_void;
    type SetSpecific = unsafe extern "C" fn(u32, *mut c_void) -> c_int;
    // These offsets are valid only for the verified Aurora Dialog-S Hybris
    // build. Check the actual mapped instructions before calling or writing.
    let signatures = [
        (bionic + 0xbf920, 0xaa1f03eab0002b48u64),
        (bionic + 0xbfbf0, 0x72b0000852801028u64),
        (bionic + 0xbfc50, 0x72b0000852801028u64),
        (egl + 0x1cb40, 0x910003fda9bf7bfdu64),
    ];
    if signatures.iter().any(|(address, expected)| unsafe {
        std::ptr::read_unaligned(*address as *const u64) != *expected
    }) {
        if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
            eprintln!("[gleshim] rekey: Hybris ELF signatures do not match");
        }
        return;
    }
    let key = (egl + 0x31a40) as *mut u32;
    let old = unsafe { *key };
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        eprintln!("[gleshim] rekey: egl={egl:#x} bionic={bionic:#x} old key={old}");
    }
    if old == u32::MAX {
        return;
    }
    let current = unsafe {
        let get: GetSpecific = std::mem::transmute(bionic + 0xbfbf0);
        get(old)
    };
    let mut new = 0u32;
    unsafe {
        let create: KeyCreate = std::mem::transmute(bionic + 0xbf920);
        // Bionic encodes the allocation flag in the high bit; the actual TLS
        // index is in the low 31 bits. Keep EGL above ART's low guest keys.
        while (new & 0x7fff_ffff) < 16 {
            if create(&mut new, None) != 0 {
                return;
            }
        }
        if !current.is_null() {
            let set: SetSpecific = std::mem::transmute(bionic + 0xbfc50);
            if set(new, current) != 0 {
                return;
            }
            let _ = set(old, std::ptr::null_mut());
        }
        *key = new;
    }
    DONE.store(true, Ordering::Relaxed);
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        eprintln!("[gleshim] moved Hybris EGL Bionic TLS key {old} -> {new}");
    }
}
