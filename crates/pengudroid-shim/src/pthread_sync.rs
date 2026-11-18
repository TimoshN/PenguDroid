//! AArch64 Bionic/glibc mutex compatibility and condition-variable glue.
//!
//! Bionic's LP64 pthread_mutex_t is 40 bytes, while glibc's AArch64 mutex is
//! 48 bytes. Direct pthread_mutex_init/lock calls therefore overwrite eight
//! bytes beyond the guest object. Host mutexes live in a side table. Condition
//! variables have the same 48-byte size and stay in place, but every wait must
//! translate its guest mutex argument to the corresponding host mutex.

use std::collections::BTreeMap;
use std::ffi::{c_int, c_void};
use std::sync::{LazyLock, Mutex};

use super::dlsym_next;

#[repr(C, align(8))]
struct HostMutex(libc::pthread_mutex_t);

const BIONIC_ATTR_SIZE: usize = 56;

/// Convert the Bionic attribute to glibc's larger AArch64 object without
/// reading or writing beyond the guest's 56-byte allocation.
unsafe fn host_attr(guest: *const c_void) -> Option<libc::pthread_attr_t> {
    if guest.is_null() {
        return None;
    }
    let mut host: libc::pthread_attr_t = std::mem::zeroed();
    std::ptr::copy_nonoverlapping(
        guest as *const u8,
        &mut host as *mut libc::pthread_attr_t as *mut u8,
        BIONIC_ATTR_SIZE,
    );
    Some(host)
}

#[no_mangle]
pub unsafe extern "C" fn __pengudroid_bionic_pthread_attr_init(guest: *mut c_void) -> c_int {
    type Real = unsafe extern "C" fn(*mut libc::pthread_attr_t) -> c_int;
    let Some(real) = dlsym_next::<Real>(b"pthread_attr_init\0") else {
        return libc::ENOSYS;
    };
    if guest.is_null() {
        return libc::EINVAL;
    }
    let mut host: libc::pthread_attr_t = std::mem::zeroed();
    let result = real(&mut host);
    if result == 0 {
        std::ptr::copy_nonoverlapping(
            &host as *const libc::pthread_attr_t as *const u8,
            guest as *mut u8,
            BIONIC_ATTR_SIZE,
        );
    }
    result
}

#[no_mangle]
pub unsafe extern "C" fn __pengudroid_bionic_pthread_attr_destroy(_guest: *mut c_void) -> c_int {
    // Bionic attributes contain no owned resources. glibc's destroy has no
    // dynamic cleanup to perform either, but its object must not be required.
    0
}

#[no_mangle]
pub unsafe extern "C" fn __pengudroid_bionic_pthread_create(
    thread: *mut libc::pthread_t,
    attr: *const c_void,
    start: Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>,
    arg: *mut c_void,
) -> c_int {
    type Real = unsafe extern "C" fn(
        *mut libc::pthread_t,
        *const libc::pthread_attr_t,
        Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>,
        *mut c_void,
    ) -> c_int;
    let Some(real) = dlsym_next::<Real>(b"pthread_create\0") else {
        return libc::ENOSYS;
    };
    let host = host_attr(attr);
    real(
        thread,
        host.as_ref().map_or(std::ptr::null(), |attr| attr as *const libc::pthread_attr_t),
        start,
        arg,
    )
}

static HOST_MUTEXES: LazyLock<Mutex<BTreeMap<usize, Box<HostMutex>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

fn lock() -> std::sync::MutexGuard<'static, BTreeMap<usize, Box<HostMutex>>> {
    HOST_MUTEXES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn invalid() -> c_int {
    unsafe { *libc::__errno_location() = libc::EINVAL };
    libc::EINVAL
}

/// Native-only diagnostics for HWUI's synchronous task queue. Java stack
/// traces stop at nSetLightGeometry; these probes show whether RenderThread is
/// stuck taking its queue mutex or sleeping on its condition variable.
fn trace_render_thread(operation: &str, object: *const c_void) {
    static ENABLED: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("PENGUDROID_TRACE_SYNC").is_some());
    let all = std::env::var_os("PENGUDROID_TRACE_SYNC_ALL").is_some();
    if !*ENABLED && !all {
        return;
    }
    let mut name = [0 as libc::c_char; 64];
    unsafe {
        if libc::pthread_getname_np(libc::pthread_self(), name.as_mut_ptr(), name.len()) != 0 {
            return;
        }
    }
    let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }.to_string_lossy();
    if name.contains("Render") || all {
        eprintln!("[sync] {name} {operation} object={object:p}");
    }
}

fn make_host(guest_attr: *const c_void) -> Option<Box<HostMutex>> {
    type AttrInit = unsafe extern "C" fn(*mut libc::pthread_mutexattr_t) -> c_int;
    type AttrType = unsafe extern "C" fn(*mut libc::pthread_mutexattr_t, c_int) -> c_int;
    type AttrDestroy = unsafe extern "C" fn(*mut libc::pthread_mutexattr_t) -> c_int;
    type Init =
        unsafe extern "C" fn(*mut libc::pthread_mutex_t, *const libc::pthread_mutexattr_t) -> c_int;

    unsafe {
        let attr_init = dlsym_next::<AttrInit>(b"pthread_mutexattr_init\0")?;
        let attr_destroy = dlsym_next::<AttrDestroy>(b"pthread_mutexattr_destroy\0")?;
        let init = dlsym_next::<Init>(b"pthread_mutex_init\0")?;
        let mut host_attr: libc::pthread_mutexattr_t = std::mem::zeroed();
        if attr_init(&mut host_attr) != 0 {
            return None;
        }
        if !guest_attr.is_null() {
            // Bionic mutexattr_t is a long whose payload is the PTHREAD_MUTEX_*
            // kind. The numeric NORMAL/RECURSIVE/ERRORCHECK values match glibc.
            let kind = *(guest_attr as *const c_int);
            let set_type = dlsym_next::<AttrType>(b"pthread_mutexattr_settype\0")?;
            if set_type(&mut host_attr, kind) != 0 {
                attr_destroy(&mut host_attr);
                return None;
            }
        }
        let mut host = Box::<HostMutex>::new(std::mem::zeroed());
        let result = init(&mut host.0, &host_attr);
        attr_destroy(&mut host_attr);
        if result != 0 {
            *libc::__errno_location() = result;
            return None;
        }
        Some(host)
    }
}

/// Return the host mutex. `create` supports Bionic static initializers, which
/// never pass through pthread_mutex_init and are lazily given a default host
/// mutex on their first lock/condition wait.
fn host_mutex(guest: *mut c_void, create: bool) -> *mut libc::pthread_mutex_t {
    if guest.is_null() {
        return std::ptr::null_mut();
    }
    let mut map = lock();
    if let Some(host) = map.get_mut(&(guest as usize)) {
        return &mut host.0;
    }
    if !create {
        return std::ptr::null_mut();
    }
    // Bionic static initializers encode NORMAL/RECURSIVE/ERRORCHECK in bits
    // 14..15 of the first word. Preserve that kind: treating every static
    // mutex as NORMAL can deadlock a framework recursive-mutex path.
    let kind = unsafe { (*(guest as *const i32) >> 14) & 3 };
    let Some(mut host) = make_host(&kind as *const c_int as *const c_void) else {
        return std::ptr::null_mut();
    };
    let pointer = &mut host.0 as *mut libc::pthread_mutex_t;
    map.insert(guest as usize, host);
    pointer
}

#[no_mangle]
pub unsafe extern "C" fn __pengudroid_bionic_pthread_mutex_init(guest: *mut c_void, attr: *const c_void) -> c_int {
    if guest.is_null() {
        return invalid();
    }
    let Some(host) = make_host(attr) else {
        return unsafe { *libc::__errno_location() };
    };
    lock().insert(guest as usize, host);
    0
}

#[no_mangle]
pub unsafe extern "C" fn __pengudroid_bionic_pthread_mutex_destroy(guest: *mut c_void) -> c_int {
    type Real = unsafe extern "C" fn(*mut libc::pthread_mutex_t) -> c_int;
    let Some(real) = (unsafe { dlsym_next::<Real>(b"pthread_mutex_destroy\0") }) else {
        return libc::ENOSYS;
    };
    let Some(host) = lock().remove(&(guest as usize)) else {
        return invalid();
    };
    let mut host = host;
    unsafe { real(&mut host.0) }
}

#[no_mangle]
pub unsafe extern "C" fn __pengudroid_bionic_pthread_mutex_lock(guest: *mut c_void) -> c_int {
    type Real = unsafe extern "C" fn(*mut libc::pthread_mutex_t) -> c_int;
    let Some(real) = (unsafe { dlsym_next::<Real>(b"pthread_mutex_lock\0") }) else {
        return libc::ENOSYS;
    };
    let host = host_mutex(guest, true);
    if host.is_null() {
        invalid()
    } else {
        trace_render_thread("mutex lock enter", guest);
        let result = unsafe { real(host) };
        trace_render_thread("mutex lock return", guest);
        result
    }
}

#[no_mangle]
pub unsafe extern "C" fn __pengudroid_bionic_pthread_mutex_trylock(guest: *mut c_void) -> c_int {
    type Real = unsafe extern "C" fn(*mut libc::pthread_mutex_t) -> c_int;
    let Some(real) = (unsafe { dlsym_next::<Real>(b"pthread_mutex_trylock\0") }) else {
        return libc::ENOSYS;
    };
    let host = host_mutex(guest, true);
    if host.is_null() {
        invalid()
    } else {
        unsafe { real(host) }
    }
}

#[no_mangle]
pub unsafe extern "C" fn __pengudroid_bionic_pthread_mutex_unlock(guest: *mut c_void) -> c_int {
    type Real = unsafe extern "C" fn(*mut libc::pthread_mutex_t) -> c_int;
    let Some(real) = (unsafe { dlsym_next::<Real>(b"pthread_mutex_unlock\0") }) else {
        return libc::ENOSYS;
    };
    let host = host_mutex(guest, false);
    if host.is_null() {
        invalid()
    } else {
        trace_render_thread("mutex unlock", guest);
        unsafe { real(host) }
    }
}

#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_timedlock(
    guest: *mut c_void,
    timeout: *const libc::timespec,
) -> c_int {
    type Real = unsafe extern "C" fn(*mut libc::pthread_mutex_t, *const libc::timespec) -> c_int;
    let Some(real) = (unsafe { dlsym_next::<Real>(b"pthread_mutex_timedlock\0") }) else {
        return libc::ENOSYS;
    };
    let host = host_mutex(guest, true);
    if host.is_null() {
        invalid()
    } else {
        unsafe { real(host, timeout) }
    }
}

#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_clocklock(
    guest: *mut c_void,
    clock: libc::clockid_t,
    timeout: *const libc::timespec,
) -> c_int {
    type Real = unsafe extern "C" fn(
        *mut libc::pthread_mutex_t,
        libc::clockid_t,
        *const libc::timespec,
    ) -> c_int;
    let Some(real) = (unsafe { dlsym_next::<Real>(b"pthread_mutex_clocklock\0") }) else {
        return libc::ENOSYS;
    };
    let host = host_mutex(guest, true);
    if host.is_null() {
        invalid()
    } else {
        unsafe { real(host, clock, timeout) }
    }
}

#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_timedlock_monotonic_np(
    guest: *mut c_void,
    timeout: *const libc::timespec,
) -> c_int {
    unsafe { pthread_mutex_clocklock(guest, libc::CLOCK_MONOTONIC, timeout) }
}

#[repr(C, align(8))]
struct HostCond(libc::pthread_cond_t);

static HOST_CONDS: LazyLock<Mutex<BTreeMap<usize, Box<HostCond>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

fn cond_lock() -> std::sync::MutexGuard<'static, BTreeMap<usize, Box<HostCond>>> {
    HOST_CONDS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn host_cond(guest: *mut c_void, create: bool) -> *mut libc::pthread_cond_t {
    if guest.is_null() {
        return std::ptr::null_mut();
    }
    let mut map = cond_lock();
    if let Some(host) = map.get_mut(&(guest as usize)) {
        return &mut host.0;
    }
    if !create {
        return std::ptr::null_mut();
    }
    type Init = unsafe extern "C" fn(*mut libc::pthread_cond_t, *const c_void) -> c_int;
    let Some(init) = (unsafe { dlsym_next::<Init>(b"pthread_cond_init\0") }) else {
        return std::ptr::null_mut();
    };
    let mut host = Box::<HostCond>::new(unsafe { std::mem::zeroed() });
    if unsafe { init(&mut host.0, std::ptr::null()) } != 0 {
        return std::ptr::null_mut();
    }
    let pointer = &mut host.0 as *mut libc::pthread_cond_t;
    map.insert(guest as usize, host);
    pointer
}

#[no_mangle]
pub unsafe extern "C" fn __pengudroid_bionic_pthread_cond_init(
    guest: *mut c_void,
    _attr: *const c_void,
) -> c_int {
    if guest.is_null() {
        return invalid();
    }
    if host_cond(guest, true).is_null() { invalid() } else { 0 }
}

#[no_mangle]
pub unsafe extern "C" fn __pengudroid_bionic_pthread_cond_destroy(guest: *mut c_void) -> c_int {
    type Real = unsafe extern "C" fn(*mut libc::pthread_cond_t) -> c_int;
    let Some(real) = (unsafe { dlsym_next::<Real>(b"pthread_cond_destroy\0") }) else {
        return libc::ENOSYS;
    };
    let Some(host) = cond_lock().remove(&(guest as usize)) else {
        return invalid();
    };
    let mut host = host;
    unsafe { real(&mut host.0) }
}

#[no_mangle]
pub unsafe extern "C" fn __pengudroid_bionic_pthread_cond_signal(guest: *mut c_void) -> c_int {
    type Real = unsafe extern "C" fn(*mut libc::pthread_cond_t) -> c_int;
    let Some(real) = (unsafe { dlsym_next::<Real>(b"pthread_cond_signal\0") }) else {
        return libc::ENOSYS;
    };
    let host = host_cond(guest, true);
    if host.is_null() { invalid() } else { unsafe { real(host) } }
}

#[no_mangle]
pub unsafe extern "C" fn __pengudroid_bionic_pthread_cond_broadcast(guest: *mut c_void) -> c_int {
    type Real = unsafe extern "C" fn(*mut libc::pthread_cond_t) -> c_int;
    let Some(real) = (unsafe { dlsym_next::<Real>(b"pthread_cond_broadcast\0") }) else {
        return libc::ENOSYS;
    };
    let host = host_cond(guest, true);
    if host.is_null() { invalid() } else { unsafe { real(host) } }
}

#[no_mangle]
pub unsafe extern "C" fn __pengudroid_bionic_pthread_cond_wait(cond: *mut c_void, mutex: *mut c_void) -> c_int {
    type Real = unsafe extern "C" fn(*mut c_void, *mut libc::pthread_mutex_t) -> c_int;
    let Some(real) = (unsafe { dlsym_next::<Real>(b"pthread_cond_wait\0") }) else {
        return libc::ENOSYS;
    };
    let host = host_mutex(mutex, true);
    let host_cond = host_cond(cond, true);
    if host.is_null() || host_cond.is_null() {
        invalid()
    } else {
        trace_render_thread("cond wait enter", cond);
        if std::env::var_os("PENGUDROID_TRACE_SYNC_ALL").is_some() {
            eprintln!("[sync] cond={cond:p} host_cond={host_cond:p} guest_mutex={mutex:p} host_mutex={host:p}");
        }
        let result = unsafe { real(host_cond as *mut c_void, host) };
        trace_render_thread("cond wait return", cond);
        result
    }
}

#[no_mangle]
pub unsafe extern "C" fn __pengudroid_bionic_pthread_cond_timedwait(
    cond: *mut c_void,
    mutex: *mut c_void,
    timeout: *const libc::timespec,
) -> c_int {
    type Real = unsafe extern "C" fn(
        *mut c_void,
        *mut libc::pthread_mutex_t,
        *const libc::timespec,
    ) -> c_int;
    let Some(real) = (unsafe { dlsym_next::<Real>(b"pthread_cond_timedwait\0") }) else {
        return libc::ENOSYS;
    };
    let host = host_mutex(mutex, true);
    let host_cond = host_cond(cond, true);
    if host.is_null() || host_cond.is_null() {
        invalid()
    } else {
        trace_render_thread("cond timedwait enter", cond);
        let result = unsafe { real(host_cond as *mut c_void, host, timeout) };
        trace_render_thread("cond timedwait return", cond);
        result
    }
}

#[no_mangle]
pub unsafe extern "C" fn __pengudroid_bionic_pthread_cond_clockwait(
    cond: *mut c_void,
    mutex: *mut c_void,
    clock: libc::clockid_t,
    timeout: *const libc::timespec,
) -> c_int {
    type Real = unsafe extern "C" fn(
        *mut c_void,
        *mut libc::pthread_mutex_t,
        libc::clockid_t,
        *const libc::timespec,
    ) -> c_int;
    let Some(real) = (unsafe { dlsym_next::<Real>(b"pthread_cond_clockwait\0") }) else {
        return libc::ENOSYS;
    };
    let host = host_mutex(mutex, true);
    let host_cond = host_cond(cond, true);
    if host.is_null() || host_cond.is_null() {
        invalid()
    } else {
        trace_render_thread("cond clockwait enter", cond);
        let result = unsafe { real(host_cond as *mut c_void, host, clock, timeout) };
        trace_render_thread("cond clockwait return", cond);
        result
    }
}

#[no_mangle]
pub unsafe extern "C" fn pthread_cond_timedwait_monotonic_np(
    cond: *mut c_void,
    mutex: *mut c_void,
    timeout: *const libc::timespec,
) -> c_int {
    unsafe { __pengudroid_bionic_pthread_cond_clockwait(cond, mutex, libc::CLOCK_MONOTONIC, timeout) }
}

// Host Mesa/GLVND/LLVM are loaded into the same process, but their pthread
// objects have glibc layouts and must never enter the Bionic side tables.
// Android references are made unversioned by de-bionic, whereas host DSOs keep
// GLIBC symbol versions. Exporting direct GLIBC aliases gives the loader an
// exact, ABI-safe boundary without guessing an ABI from a pointer's contents.
#[cfg(target_arch = "aarch64")]
macro_rules! host_mutex_one_arg {
    ($name:ident, $symbol:literal) => {
        #[no_mangle]
        unsafe extern "C" fn $name(mutex: *mut c_void) -> c_int {
            type Real = unsafe extern "C" fn(*mut libc::pthread_mutex_t) -> c_int;
            let Some(real) = dlsym_next::<Real>($symbol) else {
                return libc::ENOSYS;
            };
            real(mutex as *mut libc::pthread_mutex_t)
        }
    };
}

#[cfg(target_arch = "aarch64")]
host_mutex_one_arg!(__pengudroid_host_pthread_mutex_lock, b"pthread_mutex_lock\0");
#[cfg(target_arch = "aarch64")]
host_mutex_one_arg!(__pengudroid_host_pthread_mutex_unlock, b"pthread_mutex_unlock\0");
#[cfg(target_arch = "aarch64")]
host_mutex_one_arg!(__pengudroid_host_pthread_mutex_trylock, b"pthread_mutex_trylock\0");
#[cfg(target_arch = "aarch64")]
host_mutex_one_arg!(__pengudroid_host_pthread_mutex_destroy, b"pthread_mutex_destroy\0");

#[cfg(target_arch = "aarch64")]
#[no_mangle]
unsafe extern "C" fn __pengudroid_host_pthread_mutex_init(
    mutex: *mut c_void,
    attr: *const c_void,
) -> c_int {
    type Real = unsafe extern "C" fn(
        *mut libc::pthread_mutex_t,
        *const libc::pthread_mutexattr_t,
    ) -> c_int;
    let Some(real) = dlsym_next::<Real>(b"pthread_mutex_init\0") else {
        return libc::ENOSYS;
    };
    real(mutex as *mut libc::pthread_mutex_t, attr as *const libc::pthread_mutexattr_t)
}

#[cfg(target_arch = "aarch64")]
macro_rules! host_cond_wait {
    ($name:ident, $symbol:literal, $(($arg:ident, $ty:ty)),*) => {
        #[no_mangle]
        unsafe extern "C" fn $name(
            cond: *mut c_void,
            mutex: *mut c_void,
            $($arg: $ty),*
        ) -> c_int {
            type Real = unsafe extern "C" fn(
                *mut libc::pthread_cond_t,
                *mut libc::pthread_mutex_t,
                $($ty),*
            ) -> c_int;
            let Some(real) = dlsym_next::<Real>($symbol) else {
                return libc::ENOSYS;
            };
            real(
                cond as *mut libc::pthread_cond_t,
                mutex as *mut libc::pthread_mutex_t,
                $($arg),*
            )
        }
    };
}

#[cfg(target_arch = "aarch64")]
macro_rules! host_cond_one_arg {
    ($name:ident, $symbol:literal) => {
        #[no_mangle]
        unsafe extern "C" fn $name(cond: *mut c_void) -> c_int {
            type Real = unsafe extern "C" fn(*mut libc::pthread_cond_t) -> c_int;
            let Some(real) = dlsym_next::<Real>($symbol) else {
                return libc::ENOSYS;
            };
            real(cond as *mut libc::pthread_cond_t)
        }
    };
}

#[cfg(target_arch = "aarch64")]
host_cond_one_arg!(__pengudroid_host_pthread_cond_signal, b"pthread_cond_signal\0");
#[cfg(target_arch = "aarch64")]
host_cond_one_arg!(__pengudroid_host_pthread_cond_broadcast, b"pthread_cond_broadcast\0");
#[cfg(target_arch = "aarch64")]
host_cond_one_arg!(__pengudroid_host_pthread_cond_destroy, b"pthread_cond_destroy\0");

#[cfg(target_arch = "aarch64")]
#[no_mangle]
unsafe extern "C" fn __pengudroid_host_pthread_cond_init(
    cond: *mut c_void,
    attr: *const c_void,
) -> c_int {
    type Real = unsafe extern "C" fn(*mut libc::pthread_cond_t, *const c_void) -> c_int;
    let Some(real) = dlsym_next::<Real>(b"pthread_cond_init\0") else {
        return libc::ENOSYS;
    };
    real(cond as *mut libc::pthread_cond_t, attr)
}

#[cfg(target_arch = "aarch64")]
host_cond_wait!(__pengudroid_host_pthread_cond_wait, b"pthread_cond_wait\0",);
#[cfg(target_arch = "aarch64")]
host_cond_wait!(
    __pengudroid_host_pthread_cond_timedwait,
    b"pthread_cond_timedwait\0",
    (timeout, *const libc::timespec)
);
#[cfg(target_arch = "aarch64")]
host_cond_wait!(
    __pengudroid_host_pthread_cond_clockwait,
    b"pthread_cond_clockwait\0",
    (clock, libc::clockid_t),
    (timeout, *const libc::timespec)
);

#[cfg(target_arch = "aarch64")]
#[no_mangle]
unsafe extern "C" fn __pengudroid_host_pthread_attr_init(attr: *mut c_void) -> c_int {
    type Real = unsafe extern "C" fn(*mut libc::pthread_attr_t) -> c_int;
    let Some(real) = dlsym_next::<Real>(b"pthread_attr_init\0") else {
        return libc::ENOSYS;
    };
    real(attr as *mut libc::pthread_attr_t)
}

#[cfg(target_arch = "aarch64")]
#[no_mangle]
unsafe extern "C" fn __pengudroid_host_pthread_attr_destroy(attr: *mut c_void) -> c_int {
    type Real = unsafe extern "C" fn(*mut libc::pthread_attr_t) -> c_int;
    let Some(real) = dlsym_next::<Real>(b"pthread_attr_destroy\0") else {
        return libc::ENOSYS;
    };
    real(attr as *mut libc::pthread_attr_t)
}

#[cfg(target_arch = "aarch64")]
#[no_mangle]
unsafe extern "C" fn __pengudroid_host_pthread_create(
    thread: *mut libc::pthread_t,
    attr: *const c_void,
    start: Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>,
    arg: *mut c_void,
) -> c_int {
    type Real = unsafe extern "C" fn(
        *mut libc::pthread_t,
        *const libc::pthread_attr_t,
        Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>,
        *mut c_void,
    ) -> c_int;
    let Some(real) = dlsym_next::<Real>(b"pthread_create\0") else {
        return libc::ENOSYS;
    };
    real(thread, attr as *const libc::pthread_attr_t, start, arg)
}

#[cfg(target_arch = "aarch64")]
std::arch::global_asm!(r#"
    .symver __pengudroid_bionic_pthread_mutex_lock, pthread_mutex_lock@@LIBC
    .symver __pengudroid_bionic_pthread_mutex_unlock, pthread_mutex_unlock@@LIBC
    .symver __pengudroid_bionic_pthread_mutex_trylock, pthread_mutex_trylock@@LIBC
    .symver __pengudroid_bionic_pthread_mutex_destroy, pthread_mutex_destroy@@LIBC
    .symver __pengudroid_bionic_pthread_mutex_init, pthread_mutex_init@@LIBC
    .symver __pengudroid_bionic_pthread_cond_init, pthread_cond_init@@LIBC
    .symver __pengudroid_bionic_pthread_cond_destroy, pthread_cond_destroy@@LIBC
    .symver __pengudroid_bionic_pthread_cond_signal, pthread_cond_signal@@LIBC
    .symver __pengudroid_bionic_pthread_cond_broadcast, pthread_cond_broadcast@@LIBC
    .symver __pengudroid_bionic_pthread_cond_wait, pthread_cond_wait@@LIBC
    .symver __pengudroid_bionic_pthread_cond_timedwait, pthread_cond_timedwait@@LIBC
    .symver __pengudroid_bionic_pthread_cond_clockwait, pthread_cond_clockwait@@LIBC
    .symver __pengudroid_bionic_pthread_attr_init, pthread_attr_init@@LIBC
    .symver __pengudroid_bionic_pthread_attr_destroy, pthread_attr_destroy@@LIBC
    .symver __pengudroid_bionic_pthread_create, pthread_create@@LIBC

    .symver __pengudroid_host_pthread_mutex_lock, pthread_mutex_lock@GLIBC_2.17
    .symver __pengudroid_host_pthread_mutex_unlock, pthread_mutex_unlock@GLIBC_2.17
    .symver __pengudroid_host_pthread_mutex_trylock, pthread_mutex_trylock@GLIBC_2.17
    .symver __pengudroid_host_pthread_mutex_destroy, pthread_mutex_destroy@GLIBC_2.17
    .symver __pengudroid_host_pthread_mutex_init, pthread_mutex_init@GLIBC_2.17
    .symver __pengudroid_host_pthread_cond_init, pthread_cond_init@GLIBC_2.17
    .symver __pengudroid_host_pthread_cond_destroy, pthread_cond_destroy@GLIBC_2.17
    .symver __pengudroid_host_pthread_cond_signal, pthread_cond_signal@GLIBC_2.17
    .symver __pengudroid_host_pthread_cond_broadcast, pthread_cond_broadcast@GLIBC_2.17
    .symver __pengudroid_host_pthread_cond_wait, pthread_cond_wait@GLIBC_2.17
    .symver __pengudroid_host_pthread_cond_timedwait, pthread_cond_timedwait@GLIBC_2.17
    .symver __pengudroid_host_pthread_cond_clockwait, pthread_cond_clockwait@GLIBC_2.30
    .symver __pengudroid_host_pthread_cond_clockwait, pthread_cond_clockwait@GLIBC_2.34
    .symver __pengudroid_host_pthread_attr_init, pthread_attr_init@GLIBC_2.17
    .symver __pengudroid_host_pthread_attr_destroy, pthread_attr_destroy@GLIBC_2.17
    .symver __pengudroid_host_pthread_create, pthread_create@GLIBC_2.17
    .symver __pengudroid_host_pthread_create, pthread_create@GLIBC_2.34
"#);

// Keep the original unversioned exports on x86_64, where Digitalis enters
// the shim through its bridge rather than through an AArch64 LIBC lookup.
// The test configuration also exposes plain Rust names to the unit tests.
macro_rules! bionic_one_arg_alias {
    ($name:ident, $implementation:ident) => {
        #[cfg(any(not(target_arch = "aarch64"), test))]
        #[cfg_attr(not(target_arch = "aarch64"), no_mangle)]
        pub unsafe extern "C" fn $name(pointer: *mut c_void) -> c_int {
            $implementation(pointer)
        }
    };
}
bionic_one_arg_alias!(pthread_attr_init, __pengudroid_bionic_pthread_attr_init);
bionic_one_arg_alias!(pthread_attr_destroy, __pengudroid_bionic_pthread_attr_destroy);
bionic_one_arg_alias!(pthread_mutex_lock, __pengudroid_bionic_pthread_mutex_lock);
bionic_one_arg_alias!(pthread_mutex_trylock, __pengudroid_bionic_pthread_mutex_trylock);
bionic_one_arg_alias!(pthread_mutex_unlock, __pengudroid_bionic_pthread_mutex_unlock);
bionic_one_arg_alias!(pthread_mutex_destroy, __pengudroid_bionic_pthread_mutex_destroy);

#[cfg(any(not(target_arch = "aarch64"), test))]
#[cfg_attr(not(target_arch = "aarch64"), no_mangle)]
pub unsafe extern "C" fn pthread_mutex_init(
    pointer: *mut c_void,
    attr: *const c_void,
) -> c_int {
    __pengudroid_bionic_pthread_mutex_init(pointer, attr)
}

#[cfg(any(not(target_arch = "aarch64"), test))]
#[cfg_attr(not(target_arch = "aarch64"), no_mangle)]
pub unsafe extern "C" fn pthread_cond_init(cond: *mut c_void, attr: *const c_void) -> c_int {
    __pengudroid_bionic_pthread_cond_init(cond, attr)
}

#[cfg(any(not(target_arch = "aarch64"), test))]
#[cfg_attr(not(target_arch = "aarch64"), no_mangle)]
pub unsafe extern "C" fn pthread_cond_destroy(cond: *mut c_void) -> c_int {
    __pengudroid_bionic_pthread_cond_destroy(cond)
}

#[cfg(any(not(target_arch = "aarch64"), test))]
#[cfg_attr(not(target_arch = "aarch64"), no_mangle)]
pub unsafe extern "C" fn pthread_cond_signal(cond: *mut c_void) -> c_int {
    __pengudroid_bionic_pthread_cond_signal(cond)
}

#[cfg(any(not(target_arch = "aarch64"), test))]
#[cfg_attr(not(target_arch = "aarch64"), no_mangle)]
pub unsafe extern "C" fn pthread_cond_broadcast(cond: *mut c_void) -> c_int {
    __pengudroid_bionic_pthread_cond_broadcast(cond)
}

#[cfg(any(not(target_arch = "aarch64"), test))]
#[cfg_attr(not(target_arch = "aarch64"), no_mangle)]
pub unsafe extern "C" fn pthread_cond_wait(
    cond: *mut c_void,
    mutex: *mut c_void,
) -> c_int {
    __pengudroid_bionic_pthread_cond_wait(cond, mutex)
}

#[cfg(any(not(target_arch = "aarch64"), test))]
#[cfg_attr(not(target_arch = "aarch64"), no_mangle)]
pub unsafe extern "C" fn pthread_cond_timedwait(
    cond: *mut c_void,
    mutex: *mut c_void,
    timeout: *const libc::timespec,
) -> c_int {
    __pengudroid_bionic_pthread_cond_timedwait(cond, mutex, timeout)
}

#[cfg(any(not(target_arch = "aarch64"), test))]
#[cfg_attr(not(target_arch = "aarch64"), no_mangle)]
pub unsafe extern "C" fn pthread_cond_clockwait(
    cond: *mut c_void,
    mutex: *mut c_void,
    clock: libc::clockid_t,
    timeout: *const libc::timespec,
) -> c_int {
    __pengudroid_bionic_pthread_cond_clockwait(cond, mutex, clock, timeout)
}

#[cfg(any(not(target_arch = "aarch64"), test))]
#[cfg_attr(not(target_arch = "aarch64"), no_mangle)]
pub unsafe extern "C" fn pthread_create(
    thread: *mut libc::pthread_t,
    attr: *const c_void,
    start: Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>,
    arg: *mut c_void,
) -> c_int {
    __pengudroid_bionic_pthread_create(thread, attr, start, arg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    struct BionicMutexWithCanary {
        bytes: [u8; 40],
        canary: [u8; 16],
    }

    #[test]
    fn bionic_attribute_does_not_overwrite_its_canary() {
        let mut bytes = [0xa5u8; 72];
        unsafe {
            assert_eq!(pthread_attr_init(bytes.as_mut_ptr() as *mut c_void), 0);
            assert_eq!(pthread_attr_destroy(bytes.as_mut_ptr() as *mut c_void), 0);
        }
        assert_eq!(&bytes[BIONIC_ATTR_SIZE..], &[0xa5u8; 16]);
    }

    #[test]
    fn recursive_static_initializer_keeps_its_type() {
        let mut guest = BionicMutexWithCanary {
            bytes: [0; 40],
            canary: [0x5a; 16],
        };
        guest.bytes[..4].copy_from_slice(&(1i32 << 14).to_ne_bytes());
        let pointer = guest.bytes.as_mut_ptr() as *mut c_void;
        unsafe {
            assert_eq!(pthread_mutex_lock(pointer), 0);
            assert_eq!(pthread_mutex_lock(pointer), 0);
            assert_eq!(pthread_mutex_unlock(pointer), 0);
            assert_eq!(pthread_mutex_unlock(pointer), 0);
            assert_eq!(pthread_mutex_destroy(pointer), 0);
        }
        assert_eq!(guest.canary, [0x5a; 16]);
    }

    #[test]
    fn bionic_mutex_does_not_overwrite_its_canary() {
        let mut guest = BionicMutexWithCanary {
            bytes: [0; 40],
            canary: [0x5a; 16],
        };
        let pointer = guest.bytes.as_mut_ptr() as *mut c_void;
        unsafe {
            assert_eq!(pthread_mutex_lock(pointer), 0); // static initializer
            assert_eq!(pthread_mutex_trylock(pointer), libc::EBUSY);
            assert_eq!(pthread_mutex_unlock(pointer), 0);
            assert_eq!(pthread_mutex_init(pointer, std::ptr::null()), 0);
            assert_eq!(pthread_mutex_lock(pointer), 0);
            assert_eq!(pthread_mutex_unlock(pointer), 0);
            assert_eq!(pthread_mutex_destroy(pointer), 0);
        }
        assert_eq!(guest.bytes, [0; 40]);
        assert_eq!(guest.canary, [0x5a; 16]);
    }

    #[test]
    fn bionic_cond_uses_an_aligned_host_object_without_guest_writes() {
        let mut storage = [0xa5u8; 64];
        let guest = unsafe { storage.as_mut_ptr().add(4) } as *mut c_void;
        assert_eq!((guest as usize) % 8, 4);
        unsafe {
            assert_eq!(pthread_cond_init(guest, std::ptr::null()), 0);
            let host = host_cond(guest, false);
            assert!(!host.is_null());
            assert_eq!((host as usize) % 8, 0);
            assert_eq!(pthread_cond_signal(guest), 0);
            assert_eq!(pthread_cond_broadcast(guest), 0);
            assert_eq!(pthread_cond_destroy(guest), 0);
        }
        assert_eq!(storage, [0xa5u8; 64]);
    }
}
