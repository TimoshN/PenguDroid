//! ARMv7 Android 17 / Aurora Hybris TLS layout.
//!
//! Offsets, allocation sizes and Thumb signatures were derived independently
//! from the ARM ELFs. None of the AArch64 backend's layout is reused.

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

/// Android 17 ARMv7 guest Binder, build-id
/// 431d0f7a3d2516285a810e5d94da765c.
pub fn rekey_linux_binder() {
    static DONE: AtomicBool = AtomicBool::new(false);
    static LEGACY_KEY: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(2);
    if DONE.load(Ordering::Relaxed) {
        clear_legacy_linux_binder_key(LEGACY_KEY.load(Ordering::Relaxed));
        return;
    }
    let Some(binder) = mapped_elf_base("/data/lib/libbinder.so") else {
        if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
            eprintln!("[gleshim] ARM guest libbinder mapping is absent for TLS separation");
        }
        return;
    };
    // Thumb android::IPCThreadState::self() is at +0x32190. The literal at
    // +0x3220c and add pc at +0x321ec resolve gTLS to +0x8afac. The constructor
    // allocation at +0x321f8 is 264 bytes, independently of AArch64's 496.
    let signatures = [
        (binder + 0x32190, 0xb5b0u16),
        (binder + 0x321e2, 0x480eu16),
        (binder + 0x321e4, 0x4478u16),
        (binder + 0x321e6, 0xf04cu16),
        (binder + 0x321ee, 0x6800u16),
        (binder + 0x321f8, 0xf44fu16),
        (binder + 0x321fa, 0x7084u16),
    ];
    let literal_matches =
        unsafe { std::ptr::read_unaligned((binder + 0x3220c) as *const u32) == 0x0005_8dbc };
    if !literal_matches
        || signatures.iter().any(|(address, expected)| unsafe {
            std::ptr::read_unaligned(*address as *const u16) != *expected
        })
    {
        if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
            eprintln!("[gleshim] ARM guest Binder IPCThreadState signatures do not match");
        }
        return;
    }
    type KeyCreate =
        unsafe extern "C" fn(*mut u32, Option<unsafe extern "C" fn(*mut c_void)>) -> c_int;
    type GetSpecific = unsafe extern "C" fn(u32) -> *mut c_void;
    type SetSpecific = unsafe extern "C" fn(u32, *mut c_void) -> c_int;
    let handle = unsafe { libc::dlopen(c"libc.so.6".as_ptr(), libc::RTLD_NOW) };
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
    unsafe {
        let key = (binder + 0x8afac) as *mut u32;
        let old = *key;
        if old == u32::MAX {
            return;
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
        // Some ARM Hybris namespaces expose a sentinel rather than a guest
        // pointer through the host key. Never migrate that sentinel as a state.
        let replacement = if (current as usize) >= 0x10000 {
            current
        } else {
            std::ptr::null_mut()
        };
        if set(new, replacement) != 0 {
            return;
        }
        *key = new;
        LEGACY_KEY.store(old, Ordering::Relaxed);
        clear_legacy_linux_binder_key(old);
        DONE.store(true, Ordering::Relaxed);
        if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
            eprintln!(
                "[gleshim] separated ARM guest Binder TLS key {old} -> {new} (state={replacement:p})"
            );
        }
    }
}

/// Aurora ARMv7 Hybris HIDL. Supported layouts are selected by instructions
/// and literal values, never by device name alone.
pub fn rekey_hybris_hidl() {
    static DONE: AtomicBool = AtomicBool::new(false);
    static LEGACY_KEY: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(2);
    if DONE.load(Ordering::Relaxed) {
        clear_legacy_hybris_hidl_key(LEGACY_KEY.load(Ordering::Relaxed));
        return;
    }
    unsafe {
        let Some(base) = mapped_elf_base("/usr/libexec/droid-hybris/system/lib/libhidlbase.so")
        else {
            if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
                eprintln!("[gleshim] ARM Hybris libhidlbase mapping is absent for TLS separation");
            }
            return;
        };
        #[derive(Clone, Copy)]
        struct HidlLayout {
            self_offset: usize,
            get_key_offset: usize,
            literal_offset: usize,
            literal_value: u32,
            gtls_offset: usize,
            allocation_offset: usize,
            allocation_value: u16,
            object_size: usize,
        }
        // Each entry is a different Aurora Hybris ELF. For both, get_key_offset
        // denotes `ldr r0, [pc, #32]; add r0, pc; ldr r0, [r0]` and the literal
        // resolves to gTLS. The allocation instruction is mov.w r0, #size.
        let layouts = [
            // Aurora 5.2, build-id 1e1fe142b7bef4a7232f134b94f12888.
            HidlLayout {
                self_offset: 0x5f8c8,
                get_key_offset: 0x5f940,
                literal_offset: 0x5f964,
                literal_value: 0x0001_340e,
                gtls_offset: 0x72d52,
                allocation_offset: 0x5f94e,
                allocation_value: 0x7086,
                object_size: 268,
            },
            // Aurora 5.1.5.105, build-id 2dc509b9a4e68db41049351ffac67f05.
            HidlLayout {
                self_offset: 0x5c6d0,
                get_key_offset: 0x5c742,
                literal_offset: 0x5c764,
                literal_value: 0x0001_29f4,
                gtls_offset: 0x6f13c,
                allocation_offset: 0x5c750,
                allocation_value: 0x7088,
                object_size: 272,
            },
        ];
        let read16 = |offset| std::ptr::read_unaligned((base + offset) as *const u16);
        let layout = layouts.iter().copied().find(|layout| {
            read16(layout.self_offset) == 0xb5b0
                && read16(layout.get_key_offset) == 0x4808
                && read16(layout.get_key_offset + 2) == 0x4478
                && read16(layout.get_key_offset + 4) == 0x6800
                && read16(layout.allocation_offset) == 0xf44f
                && read16(layout.allocation_offset + 2) == layout.allocation_value
                && std::ptr::read_unaligned((base + layout.literal_offset) as *const u32)
                    == layout.literal_value
        });
        let Some(layout) = layout else {
            if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
                eprintln!("[gleshim] ARM Hybris HIDL IPCThreadState signatures do not match");
            }
            return;
        };
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
        // Initialize gTLS before replacing it. Thumb function pointers carry
        // bit zero; mapped_elf_base() and ELF st_value do not include it here.
        type IpcSelf = unsafe extern "C" fn() -> *mut c_void;
        let self_fn: IpcSelf = std::mem::transmute(base + layout.self_offset + 1);
        let _ = self_fn();
        let key = (base + layout.gtls_offset) as *mut u32;
        let old = *key;
        if old == u32::MAX {
            return;
        }
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
        // Do not migrate a 264-byte guest android::IPCThreadState as the
        // 268-byte hardware::IPCThreadState. Construct fresh HIDL state.
        let _ = set(old, std::ptr::null_mut());
        *key = new;
        LEGACY_KEY.store(old, Ordering::Relaxed);
        clear_legacy_hybris_hidl_key(old);
        DONE.store(true, Ordering::Relaxed);
        if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
            eprintln!(
                "[gleshim] separated ARM Hybris HIDL TLS key {old} -> {new} (state_size={})",
                layout.object_size
            );
        }
    }
}

pub fn rekey_egl() {
    rekey_linux_binder();
}
