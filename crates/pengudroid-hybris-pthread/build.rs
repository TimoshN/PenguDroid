fn main() {
    let manifest = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let map = manifest.join("exports.map");
    println!("cargo:rustc-link-arg=-Wl,--version-script={}", map.display());
    println!("cargo:rerun-if-changed={}", map.display());
}
