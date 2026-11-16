//! Exercise module installation and discovery without a real Android image.
use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

fn write(root: &Path, name: &str, data: &[u8]) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, data).unwrap();
}

#[test]
fn modular_install_survives_source_removal_and_rejects_incomplete_classpath() {
    let fixture = Fixture(std::env::temp_dir().join(format!("pd-modules-{}", std::process::id())));
    std::fs::create_dir(&fixture.0).unwrap();
    let source = fixture.0.join("source");
    let store = fixture.0.join("store");
    let cache = fixture.0.join("cache");
    write(&source, "system/build.prop", b"ro.build.version.sdk=37\n");
    let mut elf = [0u8; 64];
    elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
    elf[18] = 62;
    write(&source, "system/apex/com.android.art/lib64/libart.so", &elf);
    for jar in ["core-oj", "core-libart", "okhttp", "bouncycastle", "apache-xml"] {
        write(&source, &format!("system/apex/com.android.art/javalib/{jar}.jar"), b"jar fixture");
    }
    write(&source, "system/apex/com.android.i18n/javalib/core-icu4j.jar", b"jar fixture");
    write(&source, "system/lib64/libc.so", &elf);
    write(&source, "system/framework/framework.jar", b"jar fixture");
    let location = b"/system/framework/framework.jar";
    let mut fragment = vec![0x0a, (location.len() + 2) as u8, 0x0a, location.len() as u8];
    fragment.extend(location);
    write(&source, "system/etc/classpaths/bootclasspath.pb", &fragment);
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
    let script = repo.join("tools/runtime-modules.py");
    let pack = |destination: &Path| {
        let result = Command::new("python3").arg(&script).arg("pack").arg(&source)
            .arg("--output").arg(destination).output().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    };
    pack(&store);
    // Another module set with a missing required jar must never be published.
    std::fs::remove_file(source.join("system/framework/framework.jar")).unwrap();
    let incomplete = fixture.0.join("incomplete-store");
    pack(&incomplete);
    write(&source, "system/framework/framework.jar", b"updated jar fixture");
    let updated = fixture.0.join("updated-store");
    pack(&updated);
    std::fs::remove_dir_all(&source).unwrap();

    let cli = |cache: &Path, args: &[&std::ffi::OsStr]| {
        Command::new(env!("CARGO_BIN_EXE_pengudroid"))
            .arg("runtime").args(args)
            .env("XDG_CACHE_HOME", cache)
            .env("PENGUDROID_MODULES_SCRIPT", &script)
            .env("PENGUDROID_PYTHON", "python3")
            .env_remove("PENGUDROID_ART_SYSROOT").env_remove("PENGUDROID_SYSROOT")
            .env_remove("PENGUDROID_RUNTIME_MODULES")
            .current_dir(&fixture.0).output().unwrap()
    };
    let result = cli(&cache, &["--modules".as_ref(), store.as_os_str()]);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let status = cli(&cache, &["--status".as_ref()]);
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("android17-x86_64-modular"));
    let current = cache.join("pengudroid/runtime/android17-x86_64-modular-current");
    let first = std::fs::canonicalize(&current).unwrap();
    assert!(!first.join("system").exists(), "component installs must not materialize a sysroot");
    assert!(!first.join("apex").exists());
    assert!(!first.join("components/platform/build.prop").exists());
    assert!(first.join(pengudroid_runtime::DESCRIPTOR).is_file());
    let result = cli(&cache, &["--modules".as_ref(), store.as_os_str()]);
    assert!(result.status.success());
    assert_eq!(std::fs::canonicalize(&current).unwrap(), first, "same store must be idempotent");
    let result = cli(&cache, &["--modules".as_ref(), updated.as_os_str()]);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let second = std::fs::canonicalize(&current).unwrap();
    assert_ne!(first, second);
    assert!(first.join("components/framework/framework.jar").is_file(), "keep files for running guests");
    let result = cli(&cache, &["--modules".as_ref(), incomplete.as_os_str()]);
    assert!(!result.status.success());
    assert_eq!(std::fs::canonicalize(&current).unwrap(), second, "failed update must retain active generation");

    // A pre-existing tree/current symlink must not bypass component migration.
    let auto_cache = fixture.0.join("auto-cache");
    let old = auto_cache.join("pengudroid/runtime/android17-x86_64");
    let result = Command::new("python3").arg(&script).arg("assemble").arg(&store)
        .args(["--layout", "legacy", "--output"]).arg(&old).output().unwrap();
    assert!(result.status.success());
    let auto_current = old.with_file_name("android17-x86_64-modular-current");
    std::os::unix::fs::symlink("android17-x86_64", &auto_current).unwrap();
    // Provisioning from a store must not use the GSI downloader, even on upgrade.
    let result = Command::new(env!("CARGO_BIN_EXE_pengudroid"))
        .args(["runtime", "--download"])
        .env("XDG_CACHE_HOME", &auto_cache)
        .env("PENGUDROID_MODULES_SCRIPT", &script)
        .env("PENGUDROID_RUNTIME_MODULES", &store)
        .env("PENGUDROID_PREPARE_SCRIPT", "/missing/must-not-download.py")
        .env("PENGUDROID_PYTHON", "python3")
        .env_remove("PENGUDROID_ART_SYSROOT").env_remove("PENGUDROID_SYSROOT")
        .current_dir(&fixture.0).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));

    let migrated = std::fs::canonicalize(&auto_current).unwrap();
    assert!(migrated.join(pengudroid_runtime::DESCRIPTOR).is_file());
    assert!(!migrated.join("system").exists());
    assert!(old.join("system/framework/framework.jar").is_file(), "retain legacy files for existing guests");
    let explicit = Command::new(env!("CARGO_BIN_EXE_pengudroid")).args(["runtime", "--status"])
        .env("XDG_CACHE_HOME", &auto_cache).env("PENGUDROID_RUNTIME_MODULES", &store)
        .env("PENGUDROID_SYSROOT", &old).env_remove("PENGUDROID_ART_SYSROOT").env_remove("PENGUDROID_RUNTIME")
        .current_dir(&fixture.0).output().unwrap();
    assert!(explicit.status.success());
    assert!(String::from_utf8_lossy(&explicit.stdout).contains(old.to_str().unwrap()), "explicit legacy overrides must remain authoritative");

    // Match a packaged AppDir: discover both the store and installer beside
    // the executable without a checkout or module-related environment values.
    let usr = fixture.0.join("app/usr");
    std::fs::create_dir_all(usr.join("bin")).unwrap();
    let bundled_binary = usr.join("bin/pengudroid");
    std::fs::copy(env!("CARGO_BIN_EXE_pengudroid"), &bundled_binary).unwrap();
    let bundled_store = usr.join("share/pengudroid/runtime-modules");
    std::fs::create_dir_all(&bundled_store).unwrap();
    for entry in std::fs::read_dir(&store).unwrap() {
        let path = entry.unwrap().path();
        std::fs::copy(&path, bundled_store.join(path.file_name().unwrap())).unwrap();
    }
    let bundled_script = usr.join("share/pengudroid/setup/tools/runtime-modules.py");
    std::fs::create_dir_all(bundled_script.parent().unwrap()).unwrap();
    std::fs::copy(&script, bundled_script).unwrap();
    let result = Command::new(bundled_binary).args(["runtime", "--download"])
        .env("XDG_CACHE_HOME", fixture.0.join("bundled-cache"))
        .env("PENGUDROID_PYTHON", "python3")
        .env("PENGUDROID_PREPARE_SCRIPT", "/missing/must-not-download.py")
        .env_remove("PENGUDROID_RUNTIME_MODULES").env_remove("PENGUDROID_MODULES_SCRIPT")
        .env_remove("PENGUDROID_ART_SYSROOT").env_remove("PENGUDROID_SYSROOT")
        .current_dir(&fixture.0).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));

    let rejected_cache = fixture.0.join("rejected-cache");
    let result = cli(&rejected_cache, &["--modules".as_ref(), incomplete.as_os_str()]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("required boot classpath jar"));
    let runtime_dir = rejected_cache.join("pengudroid/runtime");
    assert!(!runtime_dir.join("android17-x86_64-modular-current").exists());
    assert_eq!(std::fs::read_dir(runtime_dir.join("android17-x86_64-modular-versions")).unwrap().count(), 0);
}
