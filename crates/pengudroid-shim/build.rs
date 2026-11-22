//! Build script for the Bionic shim.
//!
//! Three jobs:
//!
//! 1. Compile a small generated C file (see `src/vlog.rs`) into
//!    `libpengudroid_vlog.a`, which the shim links in. Rust cannot define
//!    C-variadic functions on stable, so the variadic Bionic log entry points
//!    (`__android_log_print`, `__android_log_assert`) are written in C and
//!    forward to Rust, where the format string is expanded with `vsnprintf`.
//! 2. Link the shim against a tiny version script that gives it the
//!    Android-style version nodes `LIBC`, `LIBM` and `LD`.
//! 3. Compile the x86_64 jump and read-only syscall boundaries (`src/jump.rs`,
//!    `src/syscall_paths.rs`) into the cdylib only, retaining their exports.
//!
//! Why the version nodes: a real Android `.so` carries a `VERNEED` demanding that
//! its `libc.so` provide version `LIBC` (and `libm.so` provide `LIBM`, `libdl.so`
//! provide `LD`). By exporting those nodes from the shim, `pengudroid libpath`
//! can point the Bionic sonames at the shim and satisfy that requirement without
//! shipping a separate stub library. The shim's other symbols stay global and
//! unversioned.
//!
//! The variadic entry points are optional without a C compiler (with a cargo
//! warning). The x86_64 GNU/Linux jump boundary requires an assembler driver;
//! silently omitting it or substituting glibc's saves would be unsafe.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

#[path = "src/vlog.rs"]
mod vlog;
#[path = "src/pathshim.rs"]
mod pathshim;
#[path = "src/jump.rs"]
mod jump;
#[path = "src/jump_aarch64.rs"]
mod jump_aarch64;
#[path = "src/jump_arm.rs"]
mod jump_arm;
#[path = "src/syscall_paths.rs"]
mod syscall_paths;

fn main() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    compile_c_shims(&out_dir);
    compile_jump_shim(&out_dir);
    compile_math_shim(&out_dir);
    compile_syscall_paths(&out_dir);
    write_version_script(&out_dir);

    println!("cargo:rustc-check-cfg=cfg(has_variadic_log)");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/vlog.rs");
    println!("cargo:rerun-if-changed=src/pathshim.rs");
    println!("cargo:rerun-if-changed=src/jump.rs");
    println!("cargo:rerun-if-changed=src/jump_aarch64.rs");
    println!("cargo:rerun-if-changed=src/jump_arm.rs");
    println!("cargo:rerun-if-changed=src/syscall_paths.rs");
}

fn bionic_pthread_symbols() -> &'static [&'static str] {
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("aarch64") {
        return &[];
    }
    &[
        "pthread_mutex_lock",
        "pthread_mutex_unlock",
        "pthread_mutex_trylock",
        "pthread_mutex_destroy",
        "pthread_mutex_init",
        "pthread_cond_wait",
        "pthread_cond_timedwait",
        "pthread_cond_clockwait",
        "pthread_attr_init",
        "pthread_attr_destroy",
        "pthread_create",
    ]
}

fn host_pthread_symbols() -> &'static [&'static str] {
    if !matches!(std::env::var("CARGO_CFG_TARGET_ARCH").as_deref(), Ok("aarch64") | Ok("arm")) {
        return &[];
    }
    &[
        "__pengudroid_host_pthread_mutex_lock",
        "__pengudroid_host_pthread_mutex_unlock",
        "__pengudroid_host_pthread_mutex_trylock",
        "__pengudroid_host_pthread_mutex_destroy",
        "__pengudroid_host_pthread_mutex_init",
        "__pengudroid_host_pthread_cond_wait",
        "__pengudroid_host_pthread_cond_timedwait",
        "__pengudroid_host_pthread_cond_clockwait",
        "__pengudroid_host_pthread_attr_init",
        "__pengudroid_host_pthread_attr_destroy",
        "__pengudroid_host_pthread_create",
    ]
}

fn jump_symbols() -> &'static [&'static str] {
    let gnu_linux = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu");
    if !gnu_linux {
        return &[];
    }
    match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => jump::SYMBOLS,
        Ok("aarch64") => jump_aarch64::SYMBOLS,
        Ok("arm") => jump_arm::SYMBOLS,
        _ => &[],
    }
}

/// Keep assembly exports out of the rlib: host Rust executables must not gain
/// an unversioned jump dispatcher that could bind its host fallback to itself.
fn compile_jump_shim(out_dir: &PathBuf) {
    if jump_symbols().is_empty() {
        return;
    }
    let architecture = std::env::var("CARGO_CFG_TARGET_ARCH").expect("target architecture");
    let (stem, assembly) = match architecture.as_str() {
        "x86_64" => ("jump", jump::ASSEMBLY),
        "aarch64" => ("jump_aarch64", jump_aarch64::ASSEMBLY),
        "arm" => ("jump_arm", jump_arm::ASSEMBLY),
        _ => return,
    };
    let source = out_dir.join(format!("{stem}.S"));
    let object = out_dir.join(format!("{stem}.o"));
    std::fs::write(&source, assembly).expect("write jump boundary");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let mut command = Command::new(cc);
    command.args(["-c", "-O2", "-fPIC", "-fvisibility=default", "-x"]);
    command.arg("assembler-with-cpp");
    let status = command
        .arg(&source)
        .arg("-o")
        .arg(&object)
        .status()
        .expect("a working compiler is required for the Bionic jump boundary");
    assert!(status.success(), "failed to compile the Bionic jump boundary");
    println!("cargo:rustc-cdylib-link-arg={}", object.display());
    for sym in jump_symbols().iter().chain(host_pthread_symbols()) {
        println!("cargo:rustc-cdylib-link-arg=-Wl,--undefined={sym}");
    }
}

fn compile_math_shim(out_dir: &PathBuf) {
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("arm") {
        return;
    }
    const UNARY_DOUBLE: &[&str] = &[
        "sin", "cos", "tan", "asin", "acos", "atan", "sinh", "cosh", "tanh",
        "asinh", "acosh", "atanh", "exp", "exp2", "expm1", "log", "log2", "log10",
        "log1p", "sqrt", "cbrt", "floor", "ceil", "trunc", "round", "nearbyint",
        "rint", "fabs",
    ];
    const BINARY_DOUBLE: &[&str] = &[
        "pow", "atan2", "fmod", "hypot", "remainder", "fdim", "fmin", "fmax", "copysign",
    ];
    const UNARY_FLOAT: &[&str] = &[
        "sinf", "cosf", "tanf", "asinf", "acosf", "atanf", "sinhf", "coshf", "tanhf",
        "asinhf", "acoshf", "atanhf", "expf", "exp2f", "expm1f", "logf", "log2f", "log10f",
        "log1pf", "sqrtf", "cbrtf", "floorf", "ceilf", "truncf", "roundf", "nearbyintf",
        "rintf", "fabsf",
    ];
    const BINARY_FLOAT: &[&str] = &[
        "powf", "atan2f", "fmodf", "hypotf", "remainderf", "fdimf", "fminf", "fmaxf", "copysignf",
    ];

    let mut assembly = String::from(".syntax unified\n.arch armv7-a\n.fpu vfpv3-d16\n.arm\n.text\n");
    let mut rust = String::from(
        "static MATH_NAMES: &[&str] = &[\n",
    );
    let mut index = 0usize;
    for (names, kind) in [
        (UNARY_DOUBLE, 0u8), (BINARY_DOUBLE, 1u8),
        (UNARY_FLOAT, 2u8), (BINARY_FLOAT, 3u8),
    ] {
        for name in names {
            writeln!(rust, "    \"{name}\",").unwrap();
            let internal = format!("__pengudroid_math_{name}");
            writeln!(assembly, ".globl {internal}\n.type {internal}, %function\n{internal}:").unwrap();
            assembly.push_str("    sub sp, sp, #24\n    stmia sp, {r0-r3}\n    str lr, [sp, #16]\n");
            writeln!(assembly, "    movw r0, #{index}").unwrap();
            assembly.push_str("    blx __pengudroid_math_lookup\n    mov r12, r0\n    cmp r12, #0\n    beq 1f\n");
            match kind {
                0 => assembly.push_str("    vldr d0, [sp, #0]\n"),
                1 => assembly.push_str("    vldr d0, [sp, #0]\n    vldr d1, [sp, #8]\n"),
                2 => assembly.push_str("    vldr s0, [sp, #0]\n"),
                3 => assembly.push_str("    vldr s0, [sp, #0]\n    vldr s1, [sp, #4]\n"),
                _ => unreachable!(),
            }
            assembly.push_str("    blx r12\n");
            if kind < 2 {
                assembly.push_str("    vmov r0, r1, d0\n");
            } else {
                assembly.push_str("    vmov r0, s0\n");
            }
            assembly.push_str("    b 2f\n1:\n    mov r0, #0\n    mov r1, #0\n2:\n    ldr lr, [sp, #16]\n    add sp, sp, #24\n    bx lr\n");
            writeln!(assembly, ".size {internal}, .-{internal}").unwrap();
            writeln!(assembly, ".symver {internal}, {name}@@LIBM\n").unwrap();
            index += 1;
        }
    }
    rust.push_str(
        "];
static MATH_HOSTS: [std::sync::atomic::AtomicUsize; MATH_NAMES.len()] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; MATH_NAMES.len()];

#[no_mangle]
unsafe extern \"C\" fn __pengudroid_math_lookup(index: usize) -> *mut c_void {
    use std::sync::atomic::Ordering;
    let cached = MATH_HOSTS[index].load(Ordering::Acquire);
    if cached != 0 { return cached as *mut c_void; }
    let next = -1isize as *mut c_void;
    let name = std::ffi::CString::new(MATH_NAMES[index]).unwrap();
    let pointer = libc::dlsym(next, name.as_ptr());
    if !pointer.is_null() {
        MATH_HOSTS[index].store(pointer as usize, Ordering::Release);
    }
    pointer
}
",
    );
    let source = out_dir.join("math_arm.S");
    let object = out_dir.join("math_arm.o");
    std::fs::write(&source, assembly).expect("write ARM math boundary");
    std::fs::write(out_dir.join("math_arm.rs"), rust).expect("write ARM math lookup");
    let status = Command::new(std::env::var("CC").unwrap_or_else(|_| "cc".into()))
        .args(["-c", "-O2", "-fPIC", "-x", "assembler-with-cpp"])
        .arg(&source).arg("-o").arg(&object).status()
        .expect("working compiler for ARM math boundary");
    assert!(status.success(), "failed to compile ARM math boundary");
    let map = out_dir.join("math_arm.map");
    std::fs::write(&map, "LIBM { global: __pengudroid_math_*; };\n").unwrap();
    println!("cargo:rustc-cdylib-link-arg={}", object.display());
    println!("cargo:rustc-cdylib-link-arg=-Wl,--version-script={}", map.display());
}

fn syscall_path_symbols() -> &'static [&'static str] {
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64")
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu")
        && std::env::var("CARGO_CFG_TARGET_POINTER_WIDTH").as_deref() == Ok("64")
    {
        syscall_paths::SYMBOLS
    } else {
        &[]
    }
}

fn compile_syscall_paths(out_dir: &PathBuf) {
    if syscall_path_symbols().is_empty() {
        return;
    }
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    for (name, language, source) in [
        ("syscall_paths_c", "c", syscall_paths::source()),
        ("syscall_paths_asm", "assembler-with-cpp", syscall_paths::assembly()),
    ] {
        let source_path = out_dir.join(format!("{name}.src"));
        let object = out_dir.join(format!("{name}.o"));
        std::fs::write(&source_path, source).expect("write syscall path source");
        let status = Command::new(&cc)
            .args(["-c", "-O2", "-fPIC", "-fvisibility=hidden", "-x", language])
            .arg(&source_path)
            .arg("-o")
            .arg(&object)
            .status()
            .expect("a C compiler is required for the syscall path boundary");
        assert!(status.success(), "failed to compile {name}");
        println!("cargo:rustc-cdylib-link-arg={}", object.display());
    }
    for symbol in syscall_path_symbols() {
        println!("cargo:rustc-cdylib-link-arg=-Wl,--undefined={symbol}");
    }
}

/// Compile the hand-written C translation units and bundle them into
/// `libpengudroid_vlog.a`.
///
/// * `vlog.c` — the C-variadic log entry points.
/// * `pathshim.c` — the C-variadic `open`/`openat` family, which need the
///   optional `mode_t` argument that stable Rust cannot express.
fn compile_c_shims(out_dir: &PathBuf) {
    let dir = out_dir.join("cshims");
    std::fs::create_dir_all(&dir).expect("create C build dir");
    let a_path = dir.join("libpengudroid_cshims.a");

    let units: [(&str, String); 2] = [
        ("vlog", vlog::source()),
        ("pathshim", pathshim::source()),
    ];

    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let mut objects = Vec::new();
    for (name, source) in &units {
        let c_path = dir.join(format!("{name}.c"));
        let o_path = dir.join(format!("{name}.o"));
        std::fs::write(&c_path, source).expect("write generated C");
        let compiled = Command::new(&cc)
            .args(["-c", "-O2", "-fPIC", "-fvisibility=default"])
            .arg(&c_path)
            .arg("-o")
            .arg(&o_path)
            .status();
        if !matches!(compiled, Ok(s) if s.success()) {
            println!(
                "cargo:warning=pengudroid-shim: no working C compiler; \
                 C-implemented entry points ({name}.c) are omitted"
            );
            return;
        }
        objects.push(o_path);
    }

    let ar = std::env::var("AR").unwrap_or_else(|_| "ar".to_string());
    let mut cmd = Command::new(&ar);
    cmd.arg("crs").arg(&a_path);
    for o in &objects {
        cmd.arg(o);
    }
    let archived = cmd.status();
    if matches!(archived, Ok(s) if s.success()) {
        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=static=pengudroid_cshims");
        // The archive members define only C entry points; nothing in the Rust
        // half references them, so without these the linker drops the objects
        // and the cdylib silently fails to export them. Force each one in;
        // `write_version_script` then re-declares them global.
        for sym in vlog::C_SYMBOLS.iter().chain(pathshim::C_SYMBOLS.iter()) {
            println!("cargo:rustc-cdylib-link-arg=-Wl,--undefined={sym}");
        }
        println!("cargo:rustc-cfg=has_variadic_log");
    } else {
        println!(
            "cargo:warning=pengudroid-shim: could not archive the C shims; \
             C-implemented entry points are omitted"
        );
    }
}

/// Assign the three anchor symbols to `LIBC`/`LIBM`/`LD`; everything else stays
/// global, so the shim can serve as the provider of the Bionic sonames.
///
/// The C/assembly entry points are listed in the `LIBC` node as well: rustc
/// emits its own export list for a cdylib, and a symbol that appears in *no*
/// version node is hidden by it. Naming them here is what makes them visible.
fn write_version_script(out_dir: &PathBuf) {
    let map = out_dir.join("pengudroid-versions.map");
    let mut libc_globals = String::from("__pengudroid_ver_libc");
    // On AArch64 these are the default LIBC implementations. Keeping them
    // unversioned also satisfies Android, but would additionally satisfy a
    // host DSO's GLIBC reference before its direct GLIBC alias is considered.
    for symbol in bionic_pthread_symbols() {
        libc_globals.push_str("; ");
        libc_globals.push_str(symbol);
    }
    for sym in vlog::C_SYMBOLS
        .iter()
        .chain(pathshim::C_SYMBOLS.iter())
        .chain(jump_symbols().iter())
        .chain(host_pthread_symbols())
        .chain(syscall_path_symbols().iter())
    {
        libc_globals.push_str("; ");
        libc_globals.push_str(sym);
    }
    // Android's libc/libm/libdl/liblog/libdl_android and the APEX libraries
    // version their symbols against these nodes. A `VERNEED` is satisfied as
    // long as the node exists, so each gets an anchor symbol; the actual
    // symbols resolve by name once `de-bionic` has made the references
    // unversioned.
    //
    // The list is every version node the libraries in a LineageOS 20 system
    // tree demand from each other (collected with `readelf -V`). The shim
    // stands in for several of those libraries, so it has to satisfy all of
    // them; missing one makes `dlopen` fail with "version `X' not found".
    const NODES: &[&str] = &[
        "LIBC_N", "LIBC_O", "LIBC_OMR1", "LIBC_P", "LIBC_Q", "LIBC_R", "LIBC_S",
        "LIBM", "LD", "LIBLOG", "LIBLOG_L", "LIBLOG_M", "LIBLOG_O", "LIBLOG_Q",
        "LIBLOG_R", "LIBDL_ANDROID", "LIBANDROID", "LIBANDROIDICU_EXTERNAL_1",
        "LIBAAUDIO", "LIBADBCONNECTION_CLIENT_1", "LIBADB_PAIRING_AUTH",
        "LIBADB_PAIRING_CONNECTION", "LIBADB_PAIRING_SERVER", "LIBANDROIDIO_1",
        "LIBARTPALETTE_1", "LIBBINDER", "LIBBINDER_NDK", "LIBBINDER_NDK30",
        "LIBBINDER_NDK31", "LIBBINDER_NDK33", "LIBBINDER_NDK_PLATFORM",
        "LIBCGROUPRC", "LIBCGROUPRC_30", "LIBEGL", "LIBFT2", "LIBGLESV1_CM",
        "LIBGLESV2", "LIBHWUI", "LIBHWUI_PLATFORM", "LIBICU_31", "LIBICU_T",
        "LIBJNIGRAPHICS", "LIBMEDIAMETRICS_1", "LIBMEDIANDK", "LIBMEMUNREACHABLE",
        "LIBNATIVEDISPLAY", "LIBNATIVEDISPLAY_PLATFORM", "LIBNATIVEHELPER_S",
        "LIBNATIVELOADER_1", "LIBNATIVEWINDOW", "LIBNATIVEWINDOW_PLATFORM",
        "LIBNEURALNETWORKS_PACKAGE_INFO", "LIBSELINUX_R", "LIBSTATSPULL",
        "LIBSTATSSOCKET", "LIBSYNC", "LIBVNDKSUPPORT", "HEAPPROFD_API_S",
        "ZLIB_1.2.0", "ZLIB_1.2.2", "ZLIB_NDK",
        // Host DSOs retain these versions. Their pthread entry points are
        // direct glibc aliases; unversioned Android calls use the Bionic shim.
        "GLIBC_2.4", "GLIBC_2.17", "GLIBC_2.30", "GLIBC_2.34",
    ];
    let mut script = String::new();
    for node in NODES {
        let anchor = format!("__pengudroid_ver_{}", node.to_lowercase().replace('.', "_"));
        script.push_str(&format!("{node} {{ global: {anchor}; }};"));
    }
    // LLD processes a same-named versioned alias and its base implementation
    // in script order. LIBC comes last so the Android entry remains @@LIBC
    // instead of silently reverting to an unversioned symbol.
    script.push_str(&format!("LIBC {{ global: {libc_globals}; }};\n"));
    std::fs::write(&map, script).expect("write version script");

    // Applies to the cdylib target only, so the rlib/tests are unaffected.
    //
    // `--undefined-version` keeps a symbol written by an *earlier* version
    // script (rustc's export list) from being an error; without it GNU ld
    // complains the symbol already belongs to another version node.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // Rust's cdylib export list hides symbols it does not know about, which
        // includes the C-implemented variadic log entry points. Re-declare them
        // global here and force the export.
        println!(
            "cargo:rustc-cdylib-link-arg=-Wl,--version-script={},--undefined-version",
            map.display()
        );
        // A SONAME makes every compat-directory symlink (`libc.so`, `libdl.so`,
        // `ld-android.so`, ...) resolve to the *already-loaded* shim by name
        // instead of being mapped a second time under the alias. Without it the
        // loader maps the shim again for each alias, and an alias such as
        // `ld-android.so` then runs the real Android linker's constructor.
        println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,libpengudroid_shim.so");
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let _ = map;
}
