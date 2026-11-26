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

/// One entry of "who owns which pthread key". Filled from
/// `pengudroid_hybris_key_create` with the return address resolved through
/// `dladdr`, so a TLS collision between two libcs can be attributed to the
/// library that allocated the key instead of being guessed from offsets.
pub const MAX_KEY_OWNERS: usize = 64;

#[repr(C)]
pub struct KeyOwner {
    /// The `pthread_key_t` value handed out by the host.
    pub key: u32,
    /// `_aligned` padding so the struct has the same layout on ARMv7/AArch64.
    pub _pad: u32,
    /// Library path (or `"?"`) that called `pthread_key_create`.
    pub library: [u8; 96],
    /// Symbol name inside that library, or `"?"`.
    pub symbol: [u8; 64],
    /// Return address inside the caller, for `addr2line` when stripped.
    pub caller: usize,
}

static KEY_OWNERS: [std::sync::Mutex<Option<KeyOwner>>; MAX_KEY_OWNERS] =
    [const { std::sync::Mutex::new(None) }; MAX_KEY_OWNERS];

/// `dladdr` from the host glibc, declared by hand: this bridge is kept
/// dependency-free so that the object loaded into the Hybris namespace stays
/// minimal. Layouts are the glibc ABI (pointer-sized fields).
#[repr(C)]
struct DlInfo {
    fname: *const std::ffi::c_char,
    fbase: *mut c_void,
    sname: *const std::ffi::c_char,
    saddr: *mut c_void,
}

extern "C" {
    fn dladdr(addr: *mut c_void, info: *mut DlInfo) -> c_int;
}

unsafe fn record_key_owner(key: u32, caller: *mut c_void) {
    let mut info: DlInfo = std::mem::zeroed();
    let found = dladdr(caller, &mut info) != 0;
    let library = if found && !info.fname.is_null() {
        std::ffi::CStr::from_ptr(info.fname).to_bytes()
    } else {
        b"?"
    };
    let symbol = if found && !info.sname.is_null() {
        std::ffi::CStr::from_ptr(info.sname).to_bytes()
    } else {
        b"?"
    };
    let mut owner = KeyOwner {
        key,
        _pad: 0,
        library: [0; 96],
        symbol: [0; 64],
        caller: caller as usize,
    };
    let n = library.len().min(owner.library.len() - 1);
    owner.library[..n].copy_from_slice(&library[..n]);
    let n = symbol.len().min(owner.symbol.len() - 1);
    owner.symbol[..n].copy_from_slice(&symbol[..n]);
    let slot = (key as usize) % MAX_KEY_OWNERS;
    if let Ok(mut guard) = KEY_OWNERS[slot].lock() {
        *guard = Some(owner);
    }
    // Trace each allocation live: a vendor library that issues binder calls
    // right after creating its TLS key can abort before any later snapshot.
    if std::env::var_os("PENGUDROID_HYBRIS_TLS_BRIDGE").is_some() {
        let lib_end = library.iter().position(|&c| c == 0).unwrap_or(library.len());
        let sym_end = symbol.iter().position(|&c| c == 0).unwrap_or(symbol.len());
        let line = format!(
            "[pthread-bridge] key={key} created by {}!{} caller={:#x}\n",
            String::from_utf8_lossy(&library[..lib_end]),
            String::from_utf8_lossy(&symbol[..sym_end]),
            caller as usize,
        );
        extern "C" {
            fn write(fd: c_int, buf: *const c_void, n: usize) -> isize;
        }
        unsafe { write(2, line.as_ptr().cast(), line.len()) };
    }
}

/// Copy out the recorded owners (`gleshim` diagnostics). Returns how many
/// entries were written.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_hybris_pthread_key_owners(
    out: *mut KeyOwner,
    cap: usize,
) -> usize {
    let mut written = 0;
    for slot in KEY_OWNERS.iter() {
        if written >= cap {
            break;
        }
        if let Ok(guard) = slot.lock() {
            if let Some(owner) = guard.as_ref() {
                out.add(written).write(KeyOwner {
                    key: owner.key,
                    _pad: 0,
                    library: owner.library,
                    symbol: owner.symbol,
                    caller: owner.caller,
                });
                written += 1;
            }
        }
    }
    written
}

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
    // Capture the return address before any call clobbers `lr`.
    let caller = return_address();
    KEY_CREATES.fetch_add(1, Ordering::Relaxed);
    let result = host_key_create(key, destructor);
    if result == 0 && !key.is_null() {
        LAST_KEY.store(key.read() as usize, Ordering::Relaxed);
        // The return address names the allocating library: this is what makes
        // a shared TLS slot between Bionic and Hybris attributable.
        record_key_owner(key.read(), caller);
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

/// The caller's return address (`lr`), for `dladdr`. Must be read in the
/// function whose caller is wanted: `lr` holds that return address only in
/// the callee's own frame, so this is `#[inline(always)]` and is used inside
/// `pengudroid_hybris_key_create`.
#[inline(always)]
fn return_address() -> *mut c_void {
    let lr: usize;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "arm")))]
    { lr = 0; }
    #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
    unsafe {
        #[cfg(target_arch = "aarch64")]
        std::arch::asm!("mov {}, x30", out(reg) lr, options(nomem, nostack, preserves_flags));
        #[cfg(target_arch = "arm")]
        std::arch::asm!("mov {}, lr", out(reg) lr, options(nomem, nostack, preserves_flags));
    }
    lr as *mut c_void
}
