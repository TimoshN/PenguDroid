//! Unify Hybris/Bionic pthread keys with the host glibc key allocator.
//!
//! Aurora's isolated Hybris namespace otherwise loads its own Bionic libc.
//! Bionic and glibc can then both allocate (for example) key 2.  When their
//! per-thread slots coincide, Hybris `eglGetError` treats Android libbinder's
//! `IPCThreadState*` as its EGL error record and overwrites `mProcess`.
//!
//! This DSO is deliberately tiny and contains no EGL entry points.  It is
//! loaded first into the isolated namespace.  Its LIBC exports satisfy Hybris'
//! Bionic references, while each body branches to the explicitly versioned
//! host-glibc implementation.  Caller-owned `pthread_once_t` and destructor
//! callbacks retain their normal C ABI.

#[cfg(target_arch = "aarch64")]
std::arch::global_asm!(r#"
    .text
    .p2align 2

    .macro HYBRIS_PTHREAD_FORWARD name, host
    .global __pengudroid_hybris_\name
    .type __pengudroid_hybris_\name, %function
    .symver __pengudroid_hybris_\name, \name@LIBC
    .symver __pengudroid_host_\name, \host@GLIBC_2.34
__pengudroid_hybris_\name:
    b __pengudroid_host_\name
    .size __pengudroid_hybris_\name, .-__pengudroid_hybris_\name
    .endm

    HYBRIS_PTHREAD_FORWARD pthread_key_create, pthread_key_create
    HYBRIS_PTHREAD_FORWARD pthread_key_delete, pthread_key_delete
    HYBRIS_PTHREAD_FORWARD pthread_getspecific, pthread_getspecific
    HYBRIS_PTHREAD_FORWARD pthread_setspecific, pthread_setspecific
    HYBRIS_PTHREAD_FORWARD pthread_once, pthread_once
"#);

#[cfg(target_arch = "arm")]
std::arch::global_asm!(r#"
    .syntax unified
    .arch armv7-a
    .arm
    .text
    .p2align 2

    .macro HYBRIS_PTHREAD_FORWARD name
    .global __pengudroid_hybris_\name
    .type __pengudroid_hybris_\name, %function
    .symver __pengudroid_hybris_\name, \name@LIBC
    .symver __pengudroid_host_\name, \name@GLIBC_2.4
__pengudroid_hybris_\name:
    b __pengudroid_host_\name
    .size __pengudroid_hybris_\name, .-__pengudroid_hybris_\name
    .endm

    HYBRIS_PTHREAD_FORWARD pthread_key_create
    HYBRIS_PTHREAD_FORWARD pthread_key_delete
    HYBRIS_PTHREAD_FORWARD pthread_getspecific
    HYBRIS_PTHREAD_FORWARD pthread_setspecific
    HYBRIS_PTHREAD_FORWARD pthread_once
"#);
