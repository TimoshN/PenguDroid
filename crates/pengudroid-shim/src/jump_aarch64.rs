//! AArch64 Bionic non-local jump boundary, assembled by `build.rs`.
//!
//! Bionic and glibc have different `jmp_buf` layouts and sizes. Calling
//! glibc's `__sigsetjmp` with Bionic's 264-byte `sigjmp_buf` overwrites adjacent
//! memory, while wrapping it in Rust or C is complicated by `returns_twice`.
//! The assembly below uses Bionic's documented AArch64 layout directly:
//!
//! - words 0..1: signal-mask flag and the 64-bit kernel signal mask;
//! - words 2..15: LR/SP and x18..x30;
//! - words 16..23: d8..d15;
//! - word 24: zero checksum/reserved word.
//!
//! No glibc buffer or allocation is involved, so longjmp cannot resurrect a
//! Rust/C owner for a side-table allocation. PAC instructions are intentional:
//! they are NOPs on CPUs without pointer authentication and preserve signed LR
//! on CPUs with it. Signal masks are saved with rt_sigprocmask directly, whose
//! kernel ABI uses the same 64-bit mask as Bionic on AArch64.

pub const SYMBOLS: &[&str] = &[
    "sigsetjmp",
    "siglongjmp",
    "setjmp",
    "_setjmp",
    "longjmp",
    "_longjmp",
];

pub const ASSEMBLY: &str = r#"
.text

.macro jump_function name
    .balign 16
    .global \name
    .type \name, %function
\name:
.endm

.macro jump_end name
    .size \name, .-\name
.endm

/* int sigsetjmp(void* env, int save_signal_mask). */
jump_function sigsetjmp
    paciasp
    str x1, [x0]                 /* signal-mask flag */
    str x30, [x0, #16]
    mov x12, sp
    str x12, [x0, #24]
    stp x28, x29, [x0, #32]
    stp x26, x27, [x0, #48]
    stp x24, x25, [x0, #64]
    stp x22, x23, [x0, #80]
    stp x20, x21, [x0, #96]
    stp x18, x19, [x0, #112]
    stp d14, d15, [x0, #128]
    stp d12, d13, [x0, #144]
    stp d10, d11, [x0, #160]
    stp d8, d9, [x0, #176]
    str xzr, [x0, #192]          /* reserved/checksum */

    tbz w1, #0, 1f
    mov x2, x0
    mov x0, #0                   /* how is ignored with a NULL new mask */
    mov x1, #0
    add x2, x2, #8               /* old kernel signal mask */
    mov x3, #8                   /* kernel sigset_t size on AArch64 */
    mov x8, #135                 /* __NR_rt_sigprocmask */
    svc #0
1:
    mov w0, #0
    autiasp
    ret
jump_end sigsetjmp

/* Bionic setjmp saves the signal mask; _setjmp does not. */
jump_function setjmp
    mov w1, #1
    b sigsetjmp
jump_end setjmp

jump_function _setjmp
    mov w1, #0
    b sigsetjmp
jump_end _setjmp

/* void siglongjmp(void* env, int value). */
jump_function siglongjmp
    mov x9, x0
    mov w10, w1
    ldr x11, [x9]
    tbz w11, #0, 1f

    mov x0, #2                   /* SIG_SETMASK */
    add x1, x9, #8
    mov x2, #0
    mov x3, #8
    mov x8, #135                 /* __NR_rt_sigprocmask */
    svc #0

1:
    ldr x30, [x9, #16]
    ldr x12, [x9, #24]           /* restored SP, applied last */
    ldp x28, x29, [x9, #32]
    ldp x26, x27, [x9, #48]
    ldp x24, x25, [x9, #64]
    ldp x22, x23, [x9, #80]
    ldp x20, x21, [x9, #96]
    ldp x18, x19, [x9, #112]
    ldp d14, d15, [x9, #128]
    ldp d12, d13, [x9, #144]
    ldp d10, d11, [x9, #160]
    ldp d8, d9, [x9, #176]

    cmp w10, wzr
    csinc w0, w10, wzr, ne       /* longjmp(env, 0) returns 1 */
    mov sp, x12
    autiasp
    ret
jump_end siglongjmp

jump_function longjmp
    b siglongjmp
jump_end longjmp

jump_function _longjmp
    b siglongjmp
jump_end _longjmp

.section .note.GNU-stack,"",%progbits
"#;
