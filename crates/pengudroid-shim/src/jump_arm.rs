//! ARM32 Bionic non-local jump boundary, assembled by `build.rs`.
//!
//! Bionic's ARM32 `sigjmp_buf` is 64 32-bit words. Its private layout reserves
//! word 0 for the signal-mask flag/cookie, words 1..2 for the kernel signal
//! mask, words 4..19 for d8..d15, word 20 for FPSCR, and words 21..30 for
//! r4..r11, LR and SP. Passing this buffer to glibc would corrupt adjacent
//! memory, so this boundary saves/restores it directly and uses the raw kernel
//! rt_sigprocmask ABI (an eight-byte signal mask).

pub const SYMBOLS: &[&str] = &[
    "sigsetjmp",
    "siglongjmp",
    "setjmp",
    "_setjmp",
    "longjmp",
    "_longjmp",
];

pub const ASSEMBLY: &str = r#"
.syntax unified
.arch armv7-a
.fpu vfpv3-d16
.arm
.text

.macro jump_function name
    .balign 4
    .global \name
    .type \name, %function
\name:
.endm

.macro jump_end name
    .size \name, .-\name
.endm

/* int sigsetjmp(void* env, int save_signal_mask). */
jump_function sigsetjmp
    push {r7}                  /* r7 is callee-saved; syscalls need it as r7. */
    mov r12, r0
    /* Private cookie, with Bionic's save-mask bit in bit zero. */
    ldr r2, .Lcookie
    ands r1, r1, #1
    orr r1, r2, r1
    str r1, [r12, #0]
    beq 1f
    mov r0, #0
    mov r1, #0
    add r2, r12, #4
    mov r3, #8
    mov r7, #175               /* __NR_rt_sigprocmask on ARM32 */
    svc #0
1:
    add r1, r12, #16
    vstmia r1, {d8-d15}
    vmrs r1, fpscr
    str r1, [r12, #80]
    add r1, r12, #84
    stmia r1, {r4-r6}
    ldr r2, [sp]
    str r2, [r1, #12]         /* original r7, not __NR_rt_sigprocmask. */
    add r2, r1, #16
    stmia r2, {r8-r11}
    str lr, [r1, #32]
    mov r2, sp
    str r2, [r1, #36]
    mov r2, #0
    str r2, [r12, #124]        /* private checksum/reserved word */
    mov r0, #0
    pop {r7}
    bx lr
.balign 4
.Lcookie:
    .word 0x50444a00
jump_end sigsetjmp

/* Bionic setjmp saves the signal mask; _setjmp does not. */
jump_function setjmp
    mov r1, #1
    b sigsetjmp
jump_end setjmp

jump_function _setjmp
    mov r1, #0
    b sigsetjmp
jump_end _setjmp

/* void siglongjmp(void* env, int value). */
jump_function siglongjmp
    push {r1}                  /* preserve the return value across svc */
    mov r12, r0
    ldr r2, [r12, #0]
    tst r2, #1
    beq 1f
    mov r0, #2                 /* SIG_SETMASK */
    add r1, r12, #4
    mov r2, #0
    mov r3, #8
    mov r7, #175
    svc #0
1:
    add r2, r12, #16
    vldmia r2, {d8-d15}
    ldr r2, [r12, #80]
    vmsr fpscr, r2
    add r2, r12, #84
    ldmia r2, {r4-r11}
    ldr lr, [r2, #32]
    ldr r12, [r2, #36]
    pop {r3}
    cmp r3, #0
    moveq r3, #1
    mov r0, r3
    mov sp, r12
    bx lr
jump_end siglongjmp

jump_function longjmp
    b siglongjmp
jump_end longjmp

jump_function _longjmp
    b siglongjmp
jump_end _longjmp

.section .note.GNU-stack,"",%progbits
"#;
