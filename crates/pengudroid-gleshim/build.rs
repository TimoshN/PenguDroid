//! Generate AArch64 tail-call forwarders for the host GLES API.
//!
//! A dlmopen namespace deliberately keeps Hybris out of the guest symbol
//! scope, but it also makes its GLES exports RTLD_LOCAL. These tiny stubs are
//! the explicit boundary: guest code calls a local `gl*`, which tail-branches
//! to the function pointer initialized from the isolated host driver.

use std::fmt::Write as _;
use std::path::PathBuf;

#[path = "arm_gl_signatures.rs"]
mod arm_gl_signatures;

const RESERVED: &[&str] = &[
    // These have real Rust implementations in lib.rs (mostly FBO handling and
    // diagnostics), so exporting a second tail-call stub would be a duplicate.
    "glBindFramebuffer",
    "glClear",
    "glClearColor",
    "glFinish",
    "glFlush",
    "glGetError",
    "glGetString",
    "glViewport",
];

fn main() {
    println!("cargo:rerun-if-changed=gl_exports.txt");
    println!("cargo:rerun-if-changed=build.rs");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let architecture = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if architecture == "arm" {
        build_arm_forwarders(&out);
        return;
    }
    if architecture != "aarch64" {
        std::fs::write(
            out.join("gl_forwarders.rs"),
            "unsafe fn init_host_gl_forwarders(\n    _gles_handle: *mut c_void,\n    _get_proc_address: Option<EglGetProcAddress>,\n) {}\n",
        )
        .expect("write non-AArch64 GL initializer");
        return;
    }

    let mut assembly = String::from(".text\n.p2align 2\n");
    let mut rust = String::from("static GL_FORWARDER_NAMES: &[&str] = &[\n");
    let mut dynamic_list = String::from("{\n");
    let mut table_index = 0usize;
    for line in include_str!("gl_exports.txt").lines() {
        let name = line.trim();
        if name.is_empty() || RESERVED.contains(&name) {
            continue;
        }
        assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
        writeln!(rust, "    \"{name}\",").unwrap();
        writeln!(dynamic_list, "    {name};").unwrap();
        let internal = format!("__pd_forward_{name}");
        writeln!(
            assembly,
            ".globl {internal}\n.type {internal}, %function\n{internal}:"
        )
        .unwrap();
        writeln!(assembly, "    adrp x16, __pd_gl_ptr_{table_index}").unwrap();
        writeln!(
            assembly,
            "    ldr x16, [x16, #:lo12:__pd_gl_ptr_{table_index}]"
        )
        .unwrap();
        // A missing optional extension is a clean null return rather than a
        // branch to address zero. Void callers simply ignore x0.
        writeln!(
            assembly,
            "    cbz x16, 1f\n    br x16\n1:\n    mov x0, xzr\n    ret"
        )
        .unwrap();
        writeln!(assembly, ".size {internal}, .-{internal}").unwrap();
        // Explicit default versions survive Rust's anonymous export map and
        // satisfy Android's versioned libGLESv2 imports.
        writeln!(assembly, ".symver {internal}, {name}@@LIBGLESV2\n").unwrap();
        table_index += 1;
    }
    rust.push_str(
        "];

#[cfg(target_arch = \"aarch64\")]
unsafe fn init_host_gl_forwarders(
    gles_handle: *mut c_void,
    get_proc_address: Option<EglGetProcAddress>,
) {
    extern \"C\" {
        static mut __pd_gl_ptr_0: usize;
    }
    let table = std::ptr::addr_of_mut!(__pd_gl_ptr_0);
    for (index, name) in GL_FORWARDER_NAMES.iter().enumerate() {
        let mut ptr = sym_ptr(gles_handle, name).unwrap_or(std::ptr::null_mut());
        if ptr.is_null() {
            if let (Some(get_proc_address), Ok(c_name)) = (get_proc_address, CString::new(*name)) {
                ptr = get_proc_address(c_name.as_ptr());
            }
        }
        *table.add(index) = ptr as usize;
    }
}

#[cfg(not(target_arch = \"aarch64\"))]
unsafe fn init_host_gl_forwarders(
    _gles_handle: *mut c_void,
    _get_proc_address: Option<EglGetProcAddress>,
) {}
",
    );

    // AIDL's C++ BpSurfaceComposer::getSupportedFrameTimestamps returns a
    // non-trivial binder::Status through x8, not an int through x0. Status is
    // { int32 exception, int32 error, String8 message }; a null String8 is its
    // default empty value. Produce EX_NONE directly and leave the caller's
    // vector empty, avoiding a SurfaceFlinger transaction on RenderThread.
    assembly.push_str(
        ".globl __pd_hosted_supported_frame_timestamps\n\
         .type __pd_hosted_supported_frame_timestamps, %function\n\
         __pd_hosted_supported_frame_timestamps:\n\
         stp x29, x30, [sp, #-32]!\n\
         mov x29, sp\n\
         str x8, [sp, #16]\n\
         bl __pengudroid_status_ctor\n\
         mov x16, x0\n\
         ldr x8, [sp, #16]\n\
         ldp x29, x30, [sp], #32\n\
         mov x0, x8\n\
         mov w1, wzr\n\
         mov w2, wzr\n\
         br x16\n\
         .size __pd_hosted_supported_frame_timestamps, .-__pd_hosted_supported_frame_timestamps\n\
         .symver __pd_hosted_supported_frame_timestamps, _ZN7android3gui17BpSurfaceComposer27getSupportedFrameTimestampsEPNSt3__16vectorINS0_10FrameEventENS2_9allocatorIS4_EEEE@@LIBGUI\n\
         .symver __pd_hosted_supported_frame_timestamps, _ZN7android3gui24BpDisplayEventConnection12setVsyncRateEi@@LIBGUI\n",
    );

    // Keep one contiguous, writable pointer table. The first label is exposed
    // to Rust; all others are referenced only by their corresponding stub.
    assembly.push_str(".section .data,\"aw\",%progbits\n.p2align 3\n");
    for index in 0..table_index {
        if index == 0 {
            assembly.push_str(".globl __pd_gl_ptr_0\n.type __pd_gl_ptr_0, %object\n");
        }
        writeln!(assembly, "__pd_gl_ptr_{index}: .xword 0").unwrap();
    }
    if table_index == 0 {
        // There is always a real table on AArch64; keep the assembler input
        // valid if the export list is accidentally emptied.
        assembly.push_str("__pd_gl_ptr_0: .xword 0\n");
    }

    dynamic_list.push_str("};\n");
    std::fs::write(out.join("gl_forwarders.S"), assembly).expect("write GL assembly");
    std::fs::write(out.join("gl_forwarders.rs"), rust).expect("write GL initializers");
    let dynamic_path = out.join("gl_forwarders.dynamic");
    std::fs::write(&dynamic_path, dynamic_list).expect("write GL dynamic list");
    // Rust's anonymous cdylib version script otherwise marks assembly-defined
    // symbols local even though they are .globl in the object file. A named
    // version node both exports the internal implementations and defines the
    // version used by the .symver public aliases.
    let version_path = out.join("gl_forwarders.map");
    std::fs::write(
        &version_path,
        "LIBGLESV2 { global: __pd_forward_*; };\nLIBGUI { global: __pd_hosted_supported_frame_timestamps; };\n",
    )
        .expect("write GL version script");
    println!(
        "cargo:rustc-cdylib-link-arg=-Wl,--dynamic-list={}",
        dynamic_path.display()
    );
    println!(
        "cargo:rustc-cdylib-link-arg=-Wl,--version-script={}",
        version_path.display()
    );
    cc::Build::new()
        .file(out.join("gl_forwarders.S"))
        .compile("pengudroid_gl_forwarders");
}

fn build_arm_forwarders(out: &PathBuf) {
    let mut assembly = String::from(
        ".syntax unified\n.arch armv7-a\n.fpu vfpv3-d16\n.arm\n.text\n",
    );
    let mut rust = String::from("static GL_FORWARDER_NAMES: &[&str] = &[\n");
    let mut dynamic_list = String::from("{\n");
    let mut table_index = 0usize;

    for line in include_str!("gl_exports.txt").lines() {
        let name = line.trim();
        if name.is_empty() { continue; }
        // On ARM the scalar-float Rust implementation would itself use the
        // wrong (hard-float) caller ABI, so glClearColor uses the assembly
        // boundary too. The other reserved functions have integer-only APIs.
        let arm_clear_color = name == "glClearColor";
        if RESERVED.contains(&name) && !arm_clear_color { continue; }
        writeln!(rust, "    \"{name}\",").unwrap();
        writeln!(dynamic_list, "    {name};").unwrap();
        let internal = format!("__pd_forward_{name}");
        writeln!(
            assembly,
            ".globl {internal}\n.type {internal}, %function\n{internal}:"
        ).unwrap();

        let signature = arm_gl_signatures::signature(name);
        let supported = signature.map_or(true, |signature| {
            signature.chars().filter(|&kind| kind == 'i').count() <= 4
                && signature.chars().filter(|&kind| kind == 'f').count() <= 16
                && signature.chars().filter(|&kind| kind == 'd').count() <= 8
        });
        if !supported {
            // These optional NV path functions need outgoing stack arguments.
            // Fail closed rather than jumping with a partially translated ABI.
            assembly.push_str("    mov r0, #0\n    bx lr\n");
        } else {
            assembly.push_str(
                "    sub sp, sp, #24\n    stmia sp, {r0-r3}\n    str lr, [sp, #16]\n",
            );
            writeln!(assembly, "    movw r0, #{table_index}").unwrap();
            assembly.push_str(
                "    blx __pd_arm_lookup\n    mov r12, r0\n",
            );
            if let Some(signature) = signature {
                let mut soft_core = 0usize;
                let mut soft_stack = 0usize;
                let mut spilled = false;
                let mut hard_core = 0usize;
                let mut hard_float = 0usize;
                let mut hard_double = 0usize;
                for kind in signature.chars() {
                    let words = if kind == 'd' { 2 } else { 1 };
                    if kind == 'd' && soft_core % 2 != 0 { soft_core += 1; }
                    let source = if !spilled && soft_core + words <= 4 {
                        let source = soft_core * 4;
                        soft_core += words;
                        source
                    } else {
                        spilled = true;
                        if kind == 'd' && soft_stack % 8 != 0 { soft_stack += 4; }
                        let source = 24 + soft_stack;
                        soft_stack += words * 4;
                        source
                    };
                    match kind {
                        'i' => {
                            writeln!(assembly, "    ldr r{hard_core}, [sp, #{source}]").unwrap();
                            hard_core += 1;
                        }
                        'f' => {
                            writeln!(assembly, "    vldr s{hard_float}, [sp, #{source}]").unwrap();
                            hard_float += 1;
                        }
                        'd' => {
                            writeln!(assembly, "    vldr d{hard_double}, [sp, #{source}]").unwrap();
                            hard_double += 1;
                        }
                        _ => unreachable!(),
                    }
                }
            } else {
                assembly.push_str("    ldmia sp, {r0-r3}\n");
            }
            assembly.push_str(
                "    ldr lr, [sp, #16]\n    add sp, sp, #24\n    cmp r12, #0\n    beq 1f\n    bx r12\n1:\n    mov r0, #0\n    bx lr\n",
            );
        }
        writeln!(assembly, ".size {internal}, .-{internal}").unwrap();
        writeln!(assembly, ".symver {internal}, {name}@@LIBGLESV2\n").unwrap();
        table_index += 1;
    }

    rust.push_str(
        "];

#[no_mangle]
unsafe extern \"C\" fn __pd_arm_lookup(index: usize) -> *mut c_void {
    extern \"C\" { static mut __pd_gl_ptr_0: usize; }
    std::ptr::addr_of!(__pd_gl_ptr_0).add(index).read() as *mut c_void
}

unsafe fn init_host_gl_forwarders(
    gles_handle: *mut c_void,
    get_proc_address: Option<EglGetProcAddress>,
) {
    extern \"C\" { static mut __pd_gl_ptr_0: usize; }
    let table = std::ptr::addr_of_mut!(__pd_gl_ptr_0);
    for (index, name) in GL_FORWARDER_NAMES.iter().enumerate() {
        let mut ptr = sym_ptr(gles_handle, name).unwrap_or(std::ptr::null_mut());
        if ptr.is_null() {
            if let (Some(get_proc_address), Ok(c_name)) = (get_proc_address, CString::new(*name)) {
                ptr = get_proc_address(c_name.as_ptr());
            }
        }
        *table.add(index) = ptr as usize;
    }
}
",
    );
    assembly.push_str(".section .data,\"aw\",%progbits\n.p2align 2\n");
    for index in 0..table_index {
        if index == 0 {
            assembly.push_str(".globl __pd_gl_ptr_0\n.type __pd_gl_ptr_0, %object\n");
        }
        writeln!(assembly, "__pd_gl_ptr_{index}: .word 0").unwrap();
    }
    dynamic_list.push_str("};\n");
    std::fs::write(out.join("gl_forwarders.S"), assembly).expect("write ARM GL assembly");
    std::fs::write(out.join("gl_forwarders.rs"), rust).expect("write ARM GL initializers");
    let dynamic_path = out.join("gl_forwarders.dynamic");
    std::fs::write(&dynamic_path, dynamic_list).expect("write ARM GL dynamic list");
    let version_path = out.join("gl_forwarders.map");
    std::fs::write(
        &version_path,
        "LIBGLESV2 { global: __pd_forward_*; __pd_arm_lookup; };\n",
    ).expect("write ARM GL version script");
    println!("cargo:rustc-cdylib-link-arg=-Wl,--dynamic-list={}", dynamic_path.display());
    println!("cargo:rustc-cdylib-link-arg=-Wl,--version-script={}", version_path.display());
    cc::Build::new()
        .file(out.join("gl_forwarders.S"))
        .compile("pengudroid_gl_forwarders");
}
