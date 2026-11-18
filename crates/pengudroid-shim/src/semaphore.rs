//! Bionic/glibc unnamed-semaphore compatibility.
//!
//! Bionic's LP64 `sem_t` is 16 bytes (a count plus three reserved words),
//! whereas glibc's is 32 bytes. Passing a guest pointer to glibc `sem_init`
//! therefore overwrites the following 16 bytes, often inside a heap object.
//! Keep the real host semaphore in a side table and leave the Bionic object
//! opaque. Process-shared semaphores are rejected: a process-local side table
//! cannot represent them.

use std::collections::BTreeMap;
use std::ffi::{c_int, c_uint, c_void};
use std::sync::{LazyLock, Mutex};

use super::dlsym_next;

#[repr(C, align(8))]
struct HostSem(libc::sem_t);

static HOST_SEMS: LazyLock<Mutex<BTreeMap<usize, Box<HostSem>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

fn lock() -> std::sync::MutexGuard<'static, BTreeMap<usize, Box<HostSem>>> {
    HOST_SEMS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn host_sem(guest: *mut c_void) -> *mut libc::sem_t {
    if guest.is_null() {
        return std::ptr::null_mut();
    }
    lock()
        .get_mut(&(guest as usize))
        .map(|sem| &mut sem.0 as *mut libc::sem_t)
        .unwrap_or(std::ptr::null_mut())
}

fn invalid() -> c_int {
    unsafe { *libc::__errno_location() = libc::EINVAL };
    -1
}

#[no_mangle]
pub unsafe extern "C" fn sem_init(guest: *mut c_void, shared: c_int, value: c_uint) -> c_int {
    if guest.is_null() {
        return invalid();
    }
    if shared != 0 {
        unsafe { *libc::__errno_location() = libc::ENOSYS };
        return -1;
    }
    type Real = unsafe extern "C" fn(*mut libc::sem_t, c_int, c_uint) -> c_int;
    let Some(real) = dlsym_next::<Real>(b"sem_init\0") else {
        unsafe { *libc::__errno_location() = libc::ENOSYS };
        return -1;
    };
    let mut host = Box::<HostSem>::new(unsafe { std::mem::zeroed() });
    if real(&mut host.0, 0, value) != 0 {
        return -1;
    }
    lock().insert(guest as usize, host);
    0
}

#[no_mangle]
pub unsafe extern "C" fn sem_destroy(guest: *mut c_void) -> c_int {
    type Real = unsafe extern "C" fn(*mut libc::sem_t) -> c_int;
    let Some(real) = dlsym_next::<Real>(b"sem_destroy\0") else {
        unsafe { *libc::__errno_location() = libc::ENOSYS };
        return -1;
    };
    let Some(host) = lock().remove(&(guest as usize)) else {
        return invalid();
    };
    let mut host = host;
    real(&mut host.0)
}

#[no_mangle]
pub unsafe extern "C" fn sem_wait(guest: *mut c_void) -> c_int {
    type Real = unsafe extern "C" fn(*mut libc::sem_t) -> c_int;
    let Some(real) = dlsym_next::<Real>(b"sem_wait\0") else {
        unsafe { *libc::__errno_location() = libc::ENOSYS };
        return -1;
    };
    let host = host_sem(guest);
    if host.is_null() {
        invalid()
    } else {
        real(host)
    }
}

#[no_mangle]
pub unsafe extern "C" fn sem_trywait(guest: *mut c_void) -> c_int {
    type Real = unsafe extern "C" fn(*mut libc::sem_t) -> c_int;
    let Some(real) = dlsym_next::<Real>(b"sem_trywait\0") else {
        unsafe { *libc::__errno_location() = libc::ENOSYS };
        return -1;
    };
    let host = host_sem(guest);
    if host.is_null() {
        invalid()
    } else {
        real(host)
    }
}

#[no_mangle]
pub unsafe extern "C" fn sem_post(guest: *mut c_void) -> c_int {
    type Real = unsafe extern "C" fn(*mut libc::sem_t) -> c_int;
    let Some(real) = dlsym_next::<Real>(b"sem_post\0") else {
        unsafe { *libc::__errno_location() = libc::ENOSYS };
        return -1;
    };
    let host = host_sem(guest);
    if host.is_null() {
        invalid()
    } else {
        real(host)
    }
}

#[no_mangle]
pub unsafe extern "C" fn sem_getvalue(guest: *mut c_void, value: *mut c_int) -> c_int {
    type Real = unsafe extern "C" fn(*mut libc::sem_t, *mut c_int) -> c_int;
    let Some(real) = dlsym_next::<Real>(b"sem_getvalue\0") else {
        unsafe { *libc::__errno_location() = libc::ENOSYS };
        return -1;
    };
    let host = host_sem(guest);
    if host.is_null() {
        invalid()
    } else {
        real(host, value)
    }
}

#[no_mangle]
pub unsafe extern "C" fn sem_timedwait(
    guest: *mut c_void,
    timeout: *const libc::timespec,
) -> c_int {
    type Real = unsafe extern "C" fn(*mut libc::sem_t, *const libc::timespec) -> c_int;
    let Some(real) = dlsym_next::<Real>(b"sem_timedwait\0") else {
        unsafe { *libc::__errno_location() = libc::ENOSYS };
        return -1;
    };
    let host = host_sem(guest);
    if host.is_null() {
        invalid()
    } else {
        real(host, timeout)
    }
}

#[no_mangle]
pub unsafe extern "C" fn sem_clockwait(
    guest: *mut c_void,
    clock: libc::clockid_t,
    timeout: *const libc::timespec,
) -> c_int {
    type Real =
        unsafe extern "C" fn(*mut libc::sem_t, libc::clockid_t, *const libc::timespec) -> c_int;
    let Some(real) = dlsym_next::<Real>(b"sem_clockwait\0") else {
        unsafe { *libc::__errno_location() = libc::ENOSYS };
        return -1;
    };
    let host = host_sem(guest);
    if host.is_null() {
        invalid()
    } else {
        real(host, clock, timeout)
    }
}

#[no_mangle]
pub unsafe extern "C" fn sem_timedwait_monotonic_np(
    guest: *mut c_void,
    timeout: *const libc::timespec,
) -> c_int {
    sem_clockwait(guest, libc::CLOCK_MONOTONIC, timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    struct BionicSemWithCanary {
        bytes: [u8; 16],
        canary: [u8; 16],
    }

    #[test]
    fn bionic_semaphore_does_not_overwrite_its_canary() {
        let mut guest = BionicSemWithCanary {
            bytes: [0xa5; 16],
            canary: [0x5a; 16],
        };
        let ptr = guest.bytes.as_mut_ptr() as *mut c_void;
        unsafe {
            assert_eq!(sem_init(ptr, 0, 1), 0);
            assert_eq!(sem_wait(ptr), 0);
            assert_eq!(sem_post(ptr), 0);
            assert_eq!(sem_wait(ptr), 0);
            assert_eq!(sem_destroy(ptr), 0);
        }
        assert_eq!(guest.bytes, [0xa5; 16]);
        assert_eq!(guest.canary, [0x5a; 16]);
    }

    #[test]
    fn unknown_semaphore_is_rejected_without_calling_glibc() {
        let mut guest = [0u8; 16];
        unsafe {
            assert_eq!(sem_post(guest.as_mut_ptr() as *mut c_void), -1);
            assert_eq!(*libc::__errno_location(), libc::EINVAL);
        }
    }
}
