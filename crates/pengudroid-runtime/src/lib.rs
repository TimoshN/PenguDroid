//! Resolve Android ABI paths to independent runtime components, without a sysroot.
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

pub const DESCRIPTOR: &str = "pengudroid-runtime.json";
pub const ENV: &str = "PENGUDROID_RUNTIME_MANIFEST";

#[derive(Clone, Debug)]
pub struct Runtime {
    pub root: PathBuf,
    pub components: BTreeMap<String, PathBuf>,
    pub properties: BTreeMap<String, String>,
    mounts: Vec<(PathBuf, PathBuf)>,
}

fn relative(value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    if value.is_empty() || value.len() > 4095 || value.contains('\0') || path.components().any(|p| !matches!(p, Component::Normal(_))) {
        return Err(format!("invalid component-relative path: {value:?}"));
    }
    Ok(path.to_path_buf())
}

fn guest(value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    let allowed = ["/system", "/apex", "/system_ext", "/product", "/vendor", "/odm", "/data", "/mnt", "/linkerconfig", "/vendor_dlkm", "/odm_dlkm"];
    if value.len() > 4095 || value.contains('\0') || !path.is_absolute() || !allowed.iter().any(|p| path.starts_with(p))
        || path.components().any(|p| matches!(p, Component::ParentDir | Component::CurDir | Component::Prefix(_))) {
        return Err(format!("invalid Android mount: {value:?}"));
    }
    Ok(path.to_path_buf())
}

impl Runtime {
    pub fn load(descriptor: &Path) -> Result<Self, String> {
        let metadata = std::fs::metadata(descriptor).map_err(|e| format!("{}: {e}", descriptor.display()))?;
        if metadata.len() > 8 * 1024 * 1024 { return Err("runtime descriptor exceeds 8 MiB".into()); }
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(descriptor).map_err(|e| e.to_string())?)
            .map_err(|e| format!("invalid runtime descriptor: {e}"))?;
        if value["schema"].as_u64() != Some(1) || value["target"].as_str() != Some("android17-x86_64") {
            return Err("unsupported component runtime schema/target".into());
        }
        let root = std::fs::canonicalize(descriptor.parent().ok_or("descriptor has no parent")?)
            .map_err(|e| e.to_string())?;
        let host = |value: &str| -> Result<PathBuf, String> {
            let path = root.join(relative(value)?);
            // Component mount roots are real files/directories. Do not use
            // realpath here: the preload shim virtualizes linker realpaths.
            let mut current = root.clone();
            for part in relative(value)?.components() {
                current.push(part);
                let metadata = std::fs::symlink_metadata(&current)
                    .map_err(|e| format!("missing runtime target {}: {e}", current.display()))?;
                if metadata.file_type().is_symlink() {
                    return Err(format!("component mount target contains a symlink: {value}"));
                }
            }
            Ok(path)
        };
        let mut components = BTreeMap::new();
        for (name, directory) in value["components"].as_object().ok_or("missing runtime components")? {
            if name.is_empty() || name.contains('/') || name.contains('\0') { return Err("invalid component name".into()); }
            components.insert(name.clone(), host(directory.as_str().ok_or("invalid component directory")?)?);
        }
        for required in ["apex-com.android.art", "apex-com.android.i18n", "framework", "config", "native", "platform"] {
            if !components.contains_key(required) { return Err(format!("missing required runtime component: {required}")); }
        }
        let mut properties = BTreeMap::new();
        for (key, val) in value["properties"].as_object().ok_or("missing runtime properties")? {
            if !key.starts_with("ro.build.version.") { return Err(format!("non-version runtime property: {key}")); }
            properties.insert(key.clone(), val.as_str().ok_or("invalid runtime property")?.to_string());
        }
        if properties.get("ro.build.version.sdk").map(String::as_str) != Some("37") {
            return Err("component runtime requires API 37".into());
        }
        let mut mounts = BTreeMap::new();
        for mount in value["mounts"].as_array().ok_or("missing runtime mounts")? {
            let source = guest(mount["guest"].as_str().ok_or("invalid guest mount")?)?;
            let destination = host(mount["host"].as_str().ok_or("invalid host mount")?)?;
            if mounts.insert(source, destination).is_some() { return Err("duplicate Android mount".into()); }
        }
        let mut mounts: Vec<_> = mounts.into_iter().collect();
        mounts.sort_by_key(|(guest, _)| std::cmp::Reverse(guest.as_os_str().len()));
        let runtime = Self { root, components, properties, mounts };
        let art = runtime.resolve(Path::new("/apex/com.android.art/lib64/libart.so"))
            .ok_or("no ART component mapping")?;
        use std::io::Read;
        let mut header = [0u8; 20];
        std::fs::File::open(&art).and_then(|mut f| f.read_exact(&mut header))
            .map_err(|e| format!("cannot inspect component ART {}: {e}", art.display()))?;
        if &header[..6] != b"\x7fELF\x02\x01" || header[18..20] != [62, 0] {
            return Err("component ART must be ELF64 little-endian x86_64".into());
        }
        Ok(runtime)
    }

    pub fn descriptor(&self) -> PathBuf { self.root.join(DESCRIPTOR) }

    /// Match path components, not string prefixes. Resolve '..' before choosing
    /// a mount, so /system/framework/../etc stays in the virtual coordinate space.
    pub fn resolve(&self, path: &Path) -> Option<PathBuf> {
        if !path.is_absolute() { return None; }
        let mut normalized = PathBuf::from("/");
        for part in path.components() {
            match part {
                Component::Normal(part) => normalized.push(part),
                Component::ParentDir => { normalized.pop(); }
                Component::RootDir | Component::CurDir => {}
                _ => return None,
            }
        }
        self.mounts.iter().find_map(|(guest, host)| normalized.strip_prefix(guest).ok().map(|suffix| if suffix.as_os_str().is_empty() { host.clone() } else { host.join(suffix) }))
    }

    /// Keep a guest linker's executable/config paths in the same namespace.
    pub fn virtualize(&self, path: &Path) -> Option<PathBuf> {
        self.mounts.iter().filter_map(|(guest, host)| path.strip_prefix(host).ok()
            .map(|suffix| (host.as_os_str().len(), if suffix.as_os_str().is_empty() { guest.clone() } else { guest.join(suffix) })))
            .max_by_key(|(length, _)| *length).map(|(_, path)| path)
    }

    pub fn component(&self, name: &str) -> Result<PathBuf, String> {
        self.components.get(name).cloned().ok_or_else(|| format!("missing runtime component: {name}"))
    }
}

/// Descriptors are validated by the launcher before preload. A configured but
/// invalid descriptor must never silently select an unrelated host/sysroot path.
pub fn configured() -> Option<&'static Runtime> {
    static RUNTIME: OnceLock<Option<Runtime>> = OnceLock::new();
    thread_local! { static LOADING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
    // Host filesystem calls used to load a descriptor can reenter preload's
    // realpath/readlink wrappers. They must remain host paths during loading.
    if LOADING.with(|flag| flag.get()) { return None; }
    RUNTIME.get_or_init(|| std::env::var_os(ENV).map(|path| {
        LOADING.with(|flag| flag.set(true));
        let runtime = Runtime::load(Path::new(&path));
        LOADING.with(|flag| flag.set(false));
        runtime.unwrap_or_else(|e| panic!("invalid component runtime: {e}"))
    })).as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Fixture(PathBuf);
    impl Drop for Fixture { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!("pd-components-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir(&root).unwrap();
            let fixture = Self(root);
            let mut components = serde_json::Map::new();
            for name in ["apex-com.android.art", "apex-com.android.i18n", "framework", "config", "native", "platform"] {
                let path = format!("components/{}", name.trim_start_matches("apex-"));
                std::fs::create_dir_all(fixture.0.join(&path)).unwrap();
                components.insert(name.into(), path.into());
            }
            let art = fixture.0.join("components/com.android.art/lib64/libart.so");
            std::fs::create_dir_all(art.parent().unwrap()).unwrap();
            let mut header = [0u8; 20]; header[..6].copy_from_slice(b"\x7fELF\x02\x01"); header[18] = 62;
            std::fs::write(art, header).unwrap();
            let value = json!({"schema":1, "target":"android17-x86_64", "components":components,
                "properties":{"ro.build.version.sdk":"37","ro.build.version.release":"17"},
                "mounts":[{"guest":"/system","host":"components/platform"},
                    {"guest":"/system/framework","host":"components/framework"},
                    {"guest":"/system/etc","host":"components/config"},
                    {"guest":"/apex/com.android.art","host":"components/com.android.art"}]});
            fixture.save(&value);
            fixture
        }
        fn save(&self, value: &serde_json::Value) { std::fs::write(self.0.join(DESCRIPTOR), serde_json::to_vec(value).unwrap()).unwrap(); }
        fn value(&self) -> serde_json::Value { serde_json::from_slice(&std::fs::read(self.0.join(DESCRIPTOR)).unwrap()).unwrap() }
        fn load(&self) -> Result<Runtime, String> { Runtime::load(&self.0.join(DESCRIPTOR)) }
    }

    #[test]
    fn resolves_component_boundaries_without_system_tree_or_build_prop() {
        let f = Fixture::new(); let runtime = f.load().unwrap();
        assert!(!f.0.join("system").exists()); assert!(!f.0.join("apex").exists());
        assert_eq!(runtime.resolve(Path::new("/system/framework/a.jar")), Some(f.0.join("components/framework/a.jar")));
        assert_eq!(runtime.resolve(Path::new("/system/framework/../etc/fonts.xml")), Some(f.0.join("components/config/fonts.xml")));
        for path in ["/usr/lib/libc.so.6", "/etc/passwd", "/system-other/file", "relative/file"] {
            assert_eq!(runtime.resolve(Path::new(path)), None, "{path}");
        }
        use std::os::unix::ffi::OsStrExt;
        // Linker namespace matching compares strings; a trailing slash here
        // breaks dir.system=/system even though Path equality ignores it.
        assert_eq!(runtime.virtualize(&f.0.join("components/platform")).unwrap().as_os_str().as_bytes(), b"/system");
        let physical = f.0.join("components/com.android.art/javalib/core-oj.jar");
        assert_eq!(runtime.virtualize(&physical), Some(PathBuf::from("/apex/com.android.art/javalib/core-oj.jar")));
    }

    #[test]
    fn refuses_mount_traversal_host_mounts_and_missing_components() {
        for (key, value, expected) in [("host", "../escape", "component-relative"),
            ("guest", "/proc", "Android mount"), ("guest", "/system/../proc", "Android mount")] {
            let f = Fixture::new(); let mut data = f.value();
            data["mounts"][0][key] = value.into(); f.save(&data);
            assert!(f.load().unwrap_err().contains(expected));
        }
        let f = Fixture::new(); let mut data = f.value();
        data["components"].as_object_mut().unwrap().remove("native"); f.save(&data);
        assert!(f.load().unwrap_err().contains("missing required runtime component"));
    }

    #[test]
    fn refuses_symlinked_component_mount_roots() {
        let f = Fixture::new();
        let native = f.0.join("components/native");
        std::fs::remove_dir(&native).unwrap();
        std::os::unix::fs::symlink("/tmp", native).unwrap();
        assert!(f.load().unwrap_err().contains("symlink"));
    }

    #[test]
    fn checks_descriptor_identity_and_actual_art_machine() {
        let f = Fixture::new(); let mut data = f.value();
        data["properties"]["ro.build.version.sdk"] = "33".into(); f.save(&data);
        assert!(f.load().unwrap_err().contains("API 37"));
        data["properties"]["ro.build.version.sdk"] = "37".into(); f.save(&data);
        let path = f.0.join("components/com.android.art/lib64/libart.so");
        let mut header = std::fs::read(&path).unwrap(); header[18] = 183; std::fs::write(path, header).unwrap();
        assert!(f.load().unwrap_err().contains("x86_64"));
    }

    #[test]
    fn rejects_duplicate_guest_mounts_and_preserves_native_file_names() {
        let f = Fixture::new(); let mut data = f.value();
        let duplicate = data["mounts"][0].clone(); data["mounts"].as_array_mut().unwrap().push(duplicate); f.save(&data);
        assert!(f.load().unwrap_err().contains("duplicate Android mount"));
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let f = Fixture::new(); let runtime = f.load().unwrap();
        let guest = Path::new(std::ffi::OsStr::from_bytes(b"/system/etc/invalid-\xff"));
        let host = runtime.resolve(guest).unwrap().into_os_string().into_vec();
        assert!(host.ends_with(b"/components/config/invalid-\xff"));
    }
}
