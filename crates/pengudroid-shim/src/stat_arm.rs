//! ARM32 Bionic/glibc `struct stat` translation.
//!
//! Bionic uses the 64-bit kernel stat layout (104 bytes, 64-bit st_size at
//! offset 48). Aurora's 32-bit glibc stat is 88 bytes with a 32-bit st_size at
//! offset 44. Passing the guest buffer directly both misreports file sizes and
//! breaks ART's image validation.

use std::ffi::{c_char, c_int, CStr, CString};
use std::sync::OnceLock;

use super::dlsym_next;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BionicTimespec {
    tv_sec: i32,
    tv_nsec: i32,
}

#[repr(C, align(8))]
struct BionicStat {
    st_dev: u64,
    __pad0: [u8; 4],
    __st_ino: u32,
    st_mode: u32,
    st_nlink: u32,
    st_uid: u32,
    st_gid: u32,
    st_rdev: u64,
    __pad3: [u8; 4],
    st_size: i64,
    st_blksize: u32,
    st_blocks: u64,
    st_atim: BionicTimespec,
    st_mtim: BionicTimespec,
    st_ctim: BionicTimespec,
    st_ino: u64,
}

const _: () = {
    assert!(std::mem::size_of::<BionicStat>() == 104);
    assert!(std::mem::offset_of!(BionicStat, st_size) == 48);
    assert!(std::mem::offset_of!(BionicStat, st_blocks) == 64);
    assert!(std::mem::offset_of!(BionicStat, st_ino) == 96);
};

unsafe fn convert(host: *const libc::stat, guest: *mut BionicStat) {
    if guest.is_null() { return; }
    let host = &*host;
    let out = &mut *guest;
    std::ptr::write_bytes(out, 0, 1);
    out.st_dev = host.st_dev;
    out.__st_ino = host.st_ino as u32;
    out.st_mode = host.st_mode;
    out.st_nlink = host.st_nlink;
    out.st_uid = host.st_uid;
    out.st_gid = host.st_gid;
    out.st_rdev = host.st_rdev;
    out.st_size = i64::from(host.st_size);
    out.st_blksize = host.st_blksize as u32;
    out.st_blocks = host.st_blocks as u64;
    out.st_atim = BionicTimespec { tv_sec: host.st_atime as i32, tv_nsec: host.st_atime_nsec as i32 };
    out.st_mtim = BionicTimespec { tv_sec: host.st_mtime as i32, tv_nsec: host.st_mtime_nsec as i32 };
    out.st_ctim = BionicTimespec { tv_sec: host.st_ctime as i32, tv_nsec: host.st_ctime_nsec as i32 };
    out.st_ino = host.st_ino as u64;
}

unsafe fn mapped_path(path: *const c_char) -> ( *const c_char, Option<CString> ) {
    if path.is_null() { return (path, None); }
    let bytes = CStr::from_ptr(path).to_bytes();
    match super::rootfs::map_bytes(bytes).and_then(|path| CString::new(path).ok()) {
        Some(mapped) => (mapped.as_ptr(), Some(mapped)),
        None => (path, None),
    }
}

macro_rules! path_stat {
    ($name:ident, $symbol:literal) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(path: *const c_char, guest: *mut BionicStat) -> c_int {
            type Real = unsafe extern "C" fn(*const c_char, *mut libc::stat) -> c_int;
            static NEXT: OnceLock<Option<Real>> = OnceLock::new();
            let Some(real) = *NEXT.get_or_init(|| dlsym_next($symbol)) else {
                unsafe { *libc::__errno_location() = libc::ENOSYS };
                return -1;
            };
            let (path, _mapped) = mapped_path(path);
            let mut host: libc::stat = std::mem::zeroed();
            let result = real(path, &mut host);
            if result == 0 { convert(&host, guest); }
            result
        }
    };
}

path_stat!(stat, b"stat\0");
path_stat!(lstat, b"lstat\0");

#[no_mangle]
pub unsafe extern "C" fn stat64(path: *const c_char, guest: *mut BionicStat) -> c_int {
    stat(path, guest)
}

#[no_mangle]
pub unsafe extern "C" fn lstat64(path: *const c_char, guest: *mut BionicStat) -> c_int {
    lstat(path, guest)
}

#[no_mangle]
pub unsafe extern "C" fn fstat(fd: c_int, guest: *mut BionicStat) -> c_int {
    type Real = unsafe extern "C" fn(c_int, *mut libc::stat) -> c_int;
    static NEXT: OnceLock<Option<Real>> = OnceLock::new();
    let Some(real) = *NEXT.get_or_init(|| dlsym_next(b"fstat\0")) else {
        *libc::__errno_location() = libc::ENOSYS;
        return -1;
    };
    let mut host: libc::stat = std::mem::zeroed();
    let result = real(fd, &mut host);
    if result == 0 { convert(&host, guest); }
    result
}

#[no_mangle]
pub unsafe extern "C" fn fstat64(fd: c_int, guest: *mut BionicStat) -> c_int {
    fstat(fd, guest)
}

#[no_mangle]
pub unsafe extern "C" fn fstatat(
    fd: c_int,
    path: *const c_char,
    guest: *mut BionicStat,
    flags: c_int,
) -> c_int {
    type Real = unsafe extern "C" fn(c_int, *const c_char, *mut libc::stat, c_int) -> c_int;
    static NEXT: OnceLock<Option<Real>> = OnceLock::new();
    let Some(real) = *NEXT.get_or_init(|| dlsym_next(b"fstatat\0")) else {
        *libc::__errno_location() = libc::ENOSYS;
        return -1;
    };
    let (path, _mapped) = mapped_path(path);
    let mut host: libc::stat = std::mem::zeroed();
    let result = real(fd, path, &mut host, flags);
    if result == 0 { convert(&host, guest); }
    result
}

#[no_mangle]
pub unsafe extern "C" fn fstatat64(
    fd: c_int,
    path: *const c_char,
    guest: *mut BionicStat,
    flags: c_int,
) -> c_int {
    fstatat(fd, path, guest, flags)
}
