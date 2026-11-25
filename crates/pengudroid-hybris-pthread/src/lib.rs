//! Unify Hybris/Bionic pthread keys with the host glibc key allocator.
//!
//! The LIBC exports satisfy Hybris' Bionic references and branch to explicitly
//! versioned host-glibc implementations. Counters are deliberately tiny and
//! allocation-free so gleshim can verify whether an internal-Hybris DSO really
//! resolves pthread calls through this bridge instead of assuming it does.

use std::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(target_arch = "aarch64")]
std::arch::global_asm!(
    r#"
    .symver pengudroid_hybris_key_create, pthread_key_create@LIBC
    .symver pengudroid_hybris_key_delete, pthread_key_delete@LIBC
    .symver pengudroid_hybris_getspecific, pthread_getspecific@LIBC
    .symver pengudroid_hybris_setspecific, pthread_setspecific@LIBC
    .symver pengudroid_hybris_once, pthread_once@LIBC
    .symver __pengudroid_host_key_create, pthread_key_create@GLIBC_2.34
    .symver __pengudroid_host_key_delete, pthread_key_delete@GLIBC_2.34
    .symver __pengudroid_host_getspecific, pthread_getspecific@GLIBC_2.34
    .symver __pengudroid_host_setspecific, pthread_setspecific@GLIBC_2.34
    .symver __pengudroid_host_once, pthread_once@GLIBC_2.34
"#
);

#[cfg(target_arch = "arm")]
std::arch::global_asm!(
    r#"
    .symver pengudroid_hybris_key_create, pthread_key_create@LIBC
    .symver pengudroid_hybris_key_delete, pthread_key_delete@LIBC
    .symver pengudroid_hybris_getspecific, pthread_getspecific@LIBC
    .symver pengudroid_hybris_setspecific, pthread_setspecific@LIBC
    .symver pengudroid_hybris_once, pthread_once@LIBC
    .symver __pengudroid_host_key_create, pthread_key_create@GLIBC_2.4
    .symver __pengudroid_host_key_delete, pthread_key_delete@GLIBC_2.4
    .symver __pengudroid_host_getspecific, pthread_getspecific@GLIBC_2.4
    .symver __pengudroid_host_setspecific, pthread_setspecific@GLIBC_2.4
    .symver __pengudroid_host_once, pthread_once@GLIBC_2.4
"#
);

extern "C" {
    #[link_name = "__pengudroid_host_key_create"]
    fn host_key_create(
        key: *mut u32,
        destructor: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
    #[link_name = "__pengudroid_host_key_delete"]
    fn host_key_delete(key: u32) -> c_int;
    #[link_name = "__pengudroid_host_getspecific"]
    fn host_getspecific(key: u32) -> *mut c_void;
    #[link_name = "__pengudroid_host_setspecific"]
    fn host_setspecific(key: u32, value: *mut c_void) -> c_int;
    #[link_name = "__pengudroid_host_once"]
    fn host_once(once: *mut c_int, routine: Option<unsafe extern "C" fn()>) -> c_int;
}

static KEY_CREATES: AtomicUsize = AtomicUsize::new(0);
static KEY_DELETES: AtomicUsize = AtomicUsize::new(0);
static GETSPECIFIC: AtomicUsize = AtomicUsize::new(0);
static SETSPECIFIC: AtomicUsize = AtomicUsize::new(0);
static ONCE: AtomicUsize = AtomicUsize::new(0);
static LAST_KEY: AtomicUsize = AtomicUsize::new(usize::MAX);

#[repr(C)]
pub struct BridgeStats {
    pub key_creates: usize,
    pub key_deletes: usize,
    pub getspecific: usize,
    pub setspecific: usize,
    pub once: usize,
    pub last_key: usize,
}

/// Return an allocation-free snapshot for gleshim diagnostics. The layout is
/// native word-sized independently on AArch64 and ARMv7.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_hybris_pthread_stats(out: *mut BridgeStats) {
    if out.is_null() {
        return;
    }
    out.write(BridgeStats {
        key_creates: KEY_CREATES.load(Ordering::Relaxed),
        key_deletes: KEY_DELETES.load(Ordering::Relaxed),
        getspecific: GETSPECIFIC.load(Ordering::Relaxed),
        setspecific: SETSPECIFIC.load(Ordering::Relaxed),
        once: ONCE.load(Ordering::Relaxed),
        last_key: LAST_KEY.load(Ordering::Relaxed),
    });
}

#[no_mangle]
pub unsafe extern "C" fn pengudroid_hybris_key_create(
    key: *mut u32,
    destructor: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    KEY_CREATES.fetch_add(1, Ordering::Relaxed);
    let result = host_key_create(key, destructor);
    if result == 0 && !key.is_null() {
        LAST_KEY.store(key.read() as usize, Ordering::Relaxed);
    }
    result
}

#[no_mangle]
pub unsafe extern "C" fn pengudroid_hybris_key_delete(key: u32) -> c_int {
    KEY_DELETES.fetch_add(1, Ordering::Relaxed);
    host_key_delete(key)
}

#[no_mangle]
pub unsafe extern "C" fn pengudroid_hybris_getspecific(key: u32) -> *mut c_void {
    GETSPECIFIC.fetch_add(1, Ordering::Relaxed);
    host_getspecific(key)
}

#[no_mangle]
pub unsafe extern "C" fn pengudroid_hybris_setspecific(key: u32, value: *mut c_void) -> c_int {
    SETSPECIFIC.fetch_add(1, Ordering::Relaxed);
    host_setspecific(key, value)
}

#[no_mangle]
pub unsafe extern "C" fn pengudroid_hybris_once(
    once: *mut c_int,
    routine: Option<unsafe extern "C" fn()>,
) -> c_int {
    ONCE.fetch_add(1, Ordering::Relaxed);
    host_once(once, routine)
}
