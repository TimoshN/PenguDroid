use std::{env, path::PathBuf, process::Command};
fn main() {
    println!("cargo:rerun-if-changed=src/native_abi.c");
    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-env-changed=AR");
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let object = out.join("native_abi.o");
    let status = Command::new(env::var_os("CC").unwrap_or("cc".into()))
        .args(["-std=c11", "-O2", "-fPIC", "-c", "src/native_abi.c", "-o"])
        .arg(&object)
        .status()
        .expect("building DexVM native calls requires a C compiler and libffi development headers");
    assert!(
        status.success(),
        "cannot compile DexVM native ABI helper (install libffi development headers)"
    );
    assert!(Command::new(env::var_os("AR").unwrap_or("ar".into()))
        .arg("crs")
        .arg(out.join("libpengu_native_abi.a"))
        .arg(&object)
        .status()
        .unwrap()
        .success());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=pengu_native_abi");
    println!("cargo:rustc-link-lib=ffi");
}
