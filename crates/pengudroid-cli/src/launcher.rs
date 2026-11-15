//! `pengudroid launch` — a Proton-style front end.
//!
//! A "prefix" is a per-application directory that holds the app's writable
//! state and its environment overrides, exactly like Proton's `compatdata`.
//! Launching picks the best backend for the APK:
//!
//! * **native** — an x86_64 `NativeActivity` app. Its own `libmain.so` runs on
//!   the host GPU through the shim, no Java runtime involved.
//! * **art** — a Java application. ART is started from an extracted Android
//!   system tree and the APK's DEX files are put on the system class path.
//!
//! The choice does not depend on the graphics vendor: everything goes through
//! the host's EGL/Vulkan, so NVIDIA, AMD and Intel drivers are handled the same.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::apk::{self, ApkInfo};

/// Which runtime to use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    Native,
    Art,
}

impl Backend {
    fn name(self) -> &'static str {
        match self {
            Backend::Native => "native",
            Backend::Art => "art",
        }
    }
}

/// Why a backend was chosen, for the launch banner.
pub struct Plan {
    pub backend: Backend,
    pub reason: String,
}

/// Decide which backend can actually run `apk`.
///
/// `native` wins whenever the APK ships an x86_64 `NativeActivity` payload,
/// because that path needs no Java runtime at all. Otherwise the presence of
/// `classes.dex` means ART is the only option.
pub fn plan(apk_path: &str) -> Result<(ApkInfo, Plan), String> {
    let path = std::path::Path::new(apk_path);
    if crate::bundle::is_bundle(path) {
        return plan_bundle(path);
    }
    let info = apk::read_apk(apk_path)?;
    let entries = apk::apk_entries(apk_path).unwrap_or_default();
    let has_x86_64_lib = entries
        .iter()
        .any(|e| e.starts_with("lib/x86_64/") && e.ends_with(".so"));
    let has_dex = entries
        .iter()
        .any(|e| e.ends_with(".dex") || e == "classes.dex");
    let has_native_activity = entries
        .iter()
        .any(|e| e.starts_with("lib/x86_64/") && e.ends_with("libmain.so"));

    if has_native_activity {
        Ok((
            info,
            Plan {
                backend: Backend::Native,
                reason: "x86_64 NativeActivity payload (libmain.so)".into(),
            },
        ))
    } else if has_dex {
        // Native libraries alone do not make an app runnable without ART: the
        // Java entry point still has to exist. Only a NativeActivity payload
        // (`lib/<abi>/libmain.so` + the NativeActivity manifest entry) can skip
        // the Java runtime.
        Ok((
            info,
            Plan {
                backend: Backend::Art,
                reason: if has_x86_64_lib {
                    "x86_64 libraries but no NativeActivity entry point".into()
                } else {
                    "DEX only, needs the Android runtime".into()
                },
            },
        ))
    } else {
        Err(format!(
            "{apk_path} has neither an x86_64 native payload nor classes.dex"
        ))
    }
}

/// Decide a backend for a bundle, whose libraries live in an ABI split.
fn plan_bundle(path: &std::path::Path) -> Result<(ApkInfo, Plan), String> {
    let b = crate::bundle::Bundle::open(path)?;
    let info = b.info()?;
    let host = crate::nativelib::host_abi();
    let libs = b.native_libs()?;
    let host_lib = libs.iter().any(|(_, l)| l.abi == host);
    let host_native_activity = libs
        .iter()
        .any(|(_, l)| l.abi == host && l.file == "libmain.so");
    let has_dex = b
        .base_bytes()
        .ok()
        .and_then(|d| apk::extract_named_bytes(&d, "classes.dex").ok())
        .is_some();

    if host_native_activity {
        Ok((
            info,
            Plan {
                backend: Backend::Native,
                reason: format!("bundle ships a {host} NativeActivity payload"),
            },
        ))
    } else if !b.abis.is_empty() && !host_lib {
        Ok((
            info,
            Plan {
                backend: Backend::Art,
                reason: format!(
                    "bundle is {}-only; ART plus an ARM translator is required",
                    b.abis.join("/")
                ),
            },
        ))
    } else if has_dex {
        Ok((
            info,
            Plan {
                backend: Backend::Art,
                reason: if host_lib {
                    format!("bundle has {host} libraries but no NativeActivity entry point")
                } else {
                    "DEX only, needs the Android runtime".into()
                },
            },
        ))
    } else {
        Err(format!(
            "{}: bundle has neither native libraries nor classes.dex",
            path.display()
        ))
    }
}

/// Where an app's private data lives (`/data/data/<pkg>`, its external files,
/// downloaded game resources): `$PENGUDROID_DATA_ROOT/<pkg>`, by default
/// `$XDG_DATA_HOME/pengudroid/data/<pkg>`. It must survive a reboot and must
/// not live in RAM: `/tmp` is a tmpfs on most desktops, and a game that
/// downloads 20+ GB of resources would fill memory and lose them on reboot.
///
/// Data an earlier version left in `/tmp/pengudroid-data/<pkg>` is moved here
/// the first time, so an existing login survives the change.
pub fn app_data_dir(package: &str) -> PathBuf {
    let root = std::env::var_os("PENGUDROID_DATA_ROOT")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
                .unwrap_or_else(std::env::temp_dir)
                .join("pengudroid/data")
        });
    let dir = root.join(package);
    let legacy = std::env::temp_dir().join("pengudroid-data").join(package);
    if !dir.exists() && legacy.is_dir() && legacy != dir {
        match move_tree(&legacy, &dir) {
            Ok(()) => println!("data: moved {} to {}", legacy.display(), dir.display()),
            Err(e) => eprintln!("data: could not move {} to {}: {e}", legacy.display(), dir.display()),
        }
    }
    dir
}

/// Move a directory tree, across filesystems if need be (tmpfs to disk).
fn move_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    // A partial copy goes to a sibling name first, so an interrupted move
    // never looks like a complete data directory.
    let staging = to.with_extension("moving");
    let _ = std::fs::remove_dir_all(&staging);
    copy_tree(from, &staging)?;
    std::fs::rename(&staging, to)?;
    std::fs::remove_dir_all(from)
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// The default prefix for a package: `$XDG_DATA_HOME/pengudroid/prefixes/<pkg>`.
pub fn default_prefix(package: &str) -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    data.join("pengudroid/prefixes").join(package)
}

/// Create the prefix layout and return the environment it defines.
///
/// The layout mirrors what an app expects to be writable: `files`, `cache`,
/// `code_cache` and a `shared_prefs` directory, plus `pengudroid.env` for the
/// user's own overrides (`KEY=VALUE`, `#` comments).
pub fn create_prefix(prefix: &Path, package: &str) -> Result<Vec<(String, String)>, String> {
    for sub in ["files", "cache", "code_cache", "shared_prefs"] {
        let dir = prefix.join(sub);
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let env_file = prefix.join("pengudroid.env");
    if !env_file.exists() {
        let mut f = std::fs::File::create(&env_file)
            .map_err(|e| format!("cannot create {}: {e}", env_file.display()))?;
        let _ = writeln!(
            f,
            "# PenguDroid environment overrides for {package}\n\
             # One KEY=VALUE per line; lines starting with '#' are ignored.\n\
             # Example:\n\
             #   PENGUDROID_WINDOW_WIDTH=1600\n"
        );
    }
    let mut out = vec![
        ("ANDROID_DATA".to_string(), prefix.display().to_string()),
        (
            "PENGUDROID_PREFIX".to_string(),
            prefix.display().to_string(),
        ),
        ("HOME".to_string(), prefix.display().to_string()),
    ];
    let text = std::fs::read_to_string(&env_file).unwrap_or_default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok(out)
}

/// The ART runtime to use: `--sysroot`, else `$PENGUDROID_ART_SYSROOT`, else the
/// per-user cache / in-checkout tree. `--sysroot` is an override, not a requirement.
fn art_sysroot(explicit: Option<&str>) -> Option<PathBuf> {
    // Absolute, because the guest runs with the app's asset directory as its
    // working directory: a relative `--sysroot` would resolve against that.
    let absolute = |p: PathBuf| std::fs::canonicalize(&p).unwrap_or(p);
    if let Some(p) = explicit {
        return Some(absolute(PathBuf::from(p)));
    }
    if let Some(p) = std::env::var_os("PENGUDROID_ART_SYSROOT") {
        return Some(absolute(PathBuf::from(p)));
    }
    crate::runtime::locate()
}

/// Ask once, on a terminal, before spending a gigabyte of bandwidth. Anything
/// non-interactive (the desktop app, scripts, CI) gets instructions instead of
/// a prompt it could never answer.
fn offer_runtime_install() -> Option<PathBuf> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return None;
    }
    if std::env::var_os("PENGUDROID_AUTO_RUNTIME").is_some_and(|v| v == "0") {
        return None;
    }
    eprint!(
        "pengudroid launch: no Android runtime found. Download the pinned API-37 runtime now \
         (one time, ~1 GiB)? [y/N] "
    );
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return None;
    }
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes" | "e" | "evet") {
        return None;
    }
    match crate::runtime::provision(true) {
        Ok(tree) => Some(tree),
        Err(error) => {
            eprintln!("pengudroid launch: {error}");
            None
        }
    }
}

/// `pengudroid launch <app.apk> [options]`
pub fn launch(args: &[String]) -> i32 {
    let mut apk_path: Option<String> = None;
    let mut prefix_arg: Option<String> = None;
    let mut sysroot_arg: Option<String> = None;
    let mut backend_arg: Option<String> = None;
    let mut component: Option<String> = None;
    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    let mut density: Option<i32> = None;
    let mut app_args: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--prefix" => prefix_arg = it.next().cloned(),
            "--sysroot" => sysroot_arg = it.next().cloned(),
            "--backend" => backend_arg = it.next().cloned(),
            "--component" => component = it.next().cloned(),
            "--width" => match it.next().and_then(|v| v.parse().ok()) {
                Some(v) => width = Some(v),
                None => {
                    eprintln!("pengudroid launch: --width needs a number");
                    return 2;
                }
            },
            "--height" => match it.next().and_then(|v| v.parse().ok()) {
                Some(v) => height = Some(v),
                None => {
                    eprintln!("pengudroid launch: --height needs a number");
                    return 2;
                }
            },
            "--density" => match it.next().and_then(|v| v.parse().ok()) {
                Some(v) => density = Some(v),
                None => {
                    eprintln!("pengudroid launch: --density needs a number (dpi)");
                    return 2;
                }
            },
            "--" => {
                app_args.extend(it.by_ref().cloned());
                break;
            }
            other if other.starts_with('-') => {
                eprintln!("pengudroid launch: unknown option {other}");
                return 2;
            }
            other => apk_path = Some(other.to_string()),
        }
    }
    let Some(apk_path) = apk_path else {
        eprintln!(
            "usage: pengudroid launch <app.apk> [--backend auto|native|art] [--prefix DIR] \
             [--sysroot DIR] [--width PX] [--height PX] [--density DPI] [-- ARGS]"
        );
        return 2;
    };

    let (info, planned) = match plan(&apk_path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("pengudroid launch: {e}");
            return 1;
        }
    };
    let backend = match backend_arg.as_deref() {
        None | Some("auto") => planned.backend,
        Some("native") => Backend::Native,
        Some("art") => Backend::Art,
        Some(other) => {
            eprintln!("pengudroid launch: unknown backend {other}");
            return 2;
        }
    };

    let prefix = prefix_arg
        .map(PathBuf::from)
        .unwrap_or_else(|| default_prefix(&info.package));
    let env = match create_prefix(&prefix, &info.package) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("pengudroid launch: {e}");
            return 1;
        }
    };

    // A bundle is materialised into the prefix once: `base.apk`, the split for
    // the architecture we intend to use, and the OBB payload. Everything after
    // this point works on an ordinary APK path.
    let mut app_path = apk_path.clone();
    // Every other member of a split bundle has to reach the app's class path as
    // well: a `config.<abi>` split carries native libraries, but Roblox's bundle
    // layout also names components that live beside the base. `art-test` puts
    // each extra `--classpath` into `ApplicationInfo.splitSourceDirs`.
    let mut split_paths: Vec<String> = Vec::new();
    let mut bundle_selected_abi: Option<String> = None;
    if crate::bundle::is_bundle(std::path::Path::new(&apk_path)) {
        match crate::bundle::Bundle::open(std::path::Path::new(&apk_path)) {
            Ok(b) => {
                let dir = prefix.join("bundle");
                let abi = bundle_abi(&b, backend);
                bundle_selected_abi = abi.clone();
                match b.extract_to(&dir, abi.as_deref()) {
                    Ok(written) => {
                        println!("  bundle  : {} ({} members -> {})", b.kind.name(), written.len(), dir.display());
                        if !b.obb.is_empty() {
                            println!("  obb     : {} file(s)", b.obb.len());
                        }
                        if let Some(base) = written.first() {
                            app_path = base.display().to_string();
                        }
                        for extra in written.iter().skip(1) {
                            split_paths.push(extra.display().to_string());
                        }
                        if !split_paths.is_empty() {
                            println!("  splits  : {}", split_paths.len());
                        }
                    }
                    Err(e) => {
                        eprintln!("pengudroid launch: {e}");
                        return 1;
                    }
                }
            }
            Err(e) => {
                eprintln!("pengudroid launch: {e}");
                return 1;
            }
        }
    }

    // The guest runs in the asset directory, so every path handed to it has to be
    // absolute.
    app_path = std::fs::canonicalize(&app_path)
        .map(|p| p.display().to_string())
        .unwrap_or(app_path);
    for split in &mut split_paths {
        if let Ok(p) = std::fs::canonicalize(&*split) {
            *split = p.display().to_string();
        }
    }

    println!("PenguDroid launch");
    println!("  package : {}", info.package);
    if let Some(v) = &info.version_name {
        println!("  version : {v}");
    }
    println!("  apk     : {apk_path}");
    println!("  prefix  : {}", prefix.display());
    println!("  cpu     : {}", host_simd_tier());
    println!("  backend : {} ({})", backend.name(), planned.reason);
    if backend != planned.backend {
        println!("  note    : forced with --backend");
    }
    let _ = std::io::Write::flush(&mut std::io::stdout());

    if bundle_selected_abi.as_deref() == Some("armeabi-v7a") {
        eprintln!(
            "pengudroid launch: {} only ships 32-bit armeabi-v7a libraries; \
             PenguDroid's runtime and Berberis translator require 64-bit arm64-v8a or x86_64",
            info.package
        );
        return 1;
    }

    match backend {
        Backend::Native => {
            let mut cmd = std::process::Command::new(std::env::current_exe().unwrap_or_default());
            cmd.arg("native").arg(&app_path);
            if let Some(c) = &component {
                cmd.arg("--component").arg(c);
            }
            if let Some(w) = width {
                cmd.arg("--width").arg(w.to_string());
            }
            if let Some(h) = height {
                cmd.arg("--height").arg(h.to_string());
            }
            apply_env(&mut cmd, &env);
            if let Some(dir) = asset_root(&app_path, &prefix, &split_paths) {
                cmd.current_dir(dir);
            }
            run_child(cmd)
        }
        Backend::Art => {
            let Some(sysroot) = art_sysroot(sysroot_arg.as_deref()).or_else(offer_runtime_install) else {
                eprintln!(
                    "pengudroid launch: this app needs ART; run `pengudroid runtime --download` \
                     once, pass --sysroot <dir>, or set PENGUDROID_ART_SYSROOT"
                );
                return 1;
            };
            if let Err(error) = crate::runner::ensure_daemon(&pengudroid_proto::default_socket_path()) {
                eprintln!("pengudroid launch: {error}");
                return 1;
            }
            let mut cmd = std::process::Command::new(std::env::current_exe().unwrap_or_default());
            cmd.arg("art-test")
                .arg("--sysroot")
                .arg(&sysroot)
                .arg("--classpath")
                .arg(&app_path);
            for split in &split_paths {
                cmd.arg("--classpath").arg(split);
            }
            // `<uses-library>` jars (`org.apache.http.legacy`): the APK is on
            // the system class loader's path, so its classes resolve their
            // references there and the jars have to be on the same path.
            for lib in &info.uses_libraries {
                let jar = std::path::Path::new(&sysroot).join("system/framework").join(format!("{lib}.jar"));
                if jar.is_file() {
                    cmd.arg("--classpath").arg(&jar);
                }
            }
            // The guest geometry is one decision for the whole tree, so it is
            // passed to the ART launcher rather than set piecemeal here.
            if let Some(w) = width {
                cmd.arg("--width").arg(w.to_string());
            }
            if let Some(h) = height {
                cmd.arg("--height").arg(h.to_string());
            }
            if let Some(d) = density {
                cmd.arg("--density").arg(d.to_string());
            }
            // The framework's JNI natives live in libandroid_runtime.so and
            // are registered by calling every exported `register_*` entry
            // point, which is what app_process does. libhwui.so registers the
            // renderer's natives (`ThreadedRenderer`, `RenderNode`), without
            // which hardware drawing cannot start.
            cmd.arg("--register-natives")
                .arg("libandroid_runtime.so,libhwui.so");
            // An Activity has no `main(String[])`; the closest honest entry
            // point is the app_process one, `ActivityThread.main`, which is
            // what a device's zygote calls for every app. `--component` only
            // picks *which* activity the host launches, never the entry point.
            let main = "android.app.ActivityThread".to_string();
            // With `--bind-app` the host plays the part `system_server` plays on
            // a device: it calls `handleBindApplication` and then
            // `handleLaunchActivity` for the manifest's launcher activity. That
            // is the only path that reaches `ViewRootImpl.performTraversals` and
            // therefore the only one that draws.
            // `--component` overrides only the initial activity. Later activity
            // transitions still need the hosted framework lifecycle.
            let activity = component
                .clone()
                .or_else(|| info.main_activity_name().map(|s| s.to_string()));
            // The display is created in the orientation the activity asks
            // for: a landscape-only game must not be laid out on a portrait
            // panel. `PENGUDROID_DISPLAY=WxH` picks an exact size, and an
            // explicit `PENGUDROID_SURFACE_W`/`_H` pair is left alone.
            if std::env::var_os(crate::display::SURFACE_W_ENV).is_none()
                && std::env::var_os(crate::display::SURFACE_H_ENV).is_none()
            {
                let orientation = activity
                    .as_deref()
                    .and_then(|name| info.screen_orientation(&info.resolve(name)));
                let requested = std::env::var(crate::display::DISPLAY_ENV).ok();
                let geometry = match crate::display::choose(requested.as_deref(), orientation) {
                    Ok(geometry) => geometry,
                    Err(e) => {
                        eprintln!("pengudroid launch: {e}");
                        return 2;
                    }
                };
                println!(
                    "  display : {}x{} ({})",
                    geometry.width,
                    geometry.height,
                    if requested.as_deref().is_some_and(|t| !t.trim().is_empty()) {
                        crate::display::DISPLAY_ENV
                    } else if geometry.is_landscape() {
                        "the activity is landscape-only"
                    } else {
                        "portrait default"
                    }
                );
                cmd.env(crate::display::SURFACE_W_ENV, geometry.width.to_string());
                cmd.env(crate::display::SURFACE_H_ENV, geometry.height.to_string());
            }
            if let Some(activity) = &activity {
                cmd.arg("--bind-app");
                cmd.env("PENGUDROID_LAUNCH_ACTIVITY", activity);
                println!("  activity: {activity}");
            } else {
                println!(
                    "  note    : no launcher activity in the manifest; starting via \
                     ActivityThread.main"
                );
            }
            // The prefix overrides the guest's `HOME`, so the keymap for this
            // package has to be resolved against the host's configuration now
            // and passed through (the GUI writes the same path).
            cmd.env(
                "PENGUDROID_KEYMAP_PATH",
                pengudroid_input::keymap::keymap_path(&info.package),
            );
            // Export MangoHud configuration to the guest runtime when enabled.
            if std::env::var_os("MANGOHUD").is_none()
                && std::env::var("PENGUDROID_FPS_METER").is_ok_and(|v| v == "1" || v == "true")
            {
                cmd.env("MANGOHUD", "1");
            }
            if std::env::var_os("MANGOHUD_CONFIG").is_none() {
                cmd.env("MANGOHUD_CONFIG", "fps,frametime,gpu_stats,position=top-left");
            }
            // Same for app data: resolved against the host's HOME, before the
            // prefix replaces it, so data persists across launches and prefixes.
            if std::env::var_os("PENGUDROID_DATA_ROOT").is_none() {
                if let Some(root) = app_data_dir(&info.package).parent() {
                    cmd.env("PENGUDROID_DATA_ROOT", root);
                }
            }
            cmd.arg("--main").arg(main);
            // Enable the ARM64 translator whenever explicitly requested via
            // `PENGUDROID_NATIVE_BRIDGE_LIB` OR automatically when launching an
            // ARM64-only app (`arm64-v8a` without `x86_64`) and an installed
            // bridge tree (next to the exe or in the AppImage cache) is found.
            let arm64_only = is_arm64_only_payload(
                bundle_selected_abi.as_deref(),
                &app_path,
                &split_paths,
            );
            let use_bridge = needs_native_bridge(&std::iter::once(app_path.clone()).chain(split_paths.iter().cloned()).collect::<Vec<_>>());
            if !use_bridge { cmd.env_remove("PENGUDROID_NATIVE_BRIDGE_LIB"); }
            let explicit_bridge = std::env::var("PENGUDROID_NATIVE_BRIDGE_LIB").ok().filter(|_| use_bridge);
            let bridge_setup = if let Some(explicit) = explicit_bridge {
                let path = PathBuf::from(&explicit);
                let dir = if path.is_dir() {
                    path.clone()
                } else {
                    path.parent().map(Path::to_path_buf).unwrap_or_else(|| path.clone())
                };
                let guest = std::env::var_os("PENGUDROID_ARM64_GUEST_ROOT")
                    .map(PathBuf::from)
                    .or_else(|| guest_root_for_bridge(&dir));
                Some((path, dir, guest))
            } else if arm64_only {
                find_arm64_bridge_dir().map(|(dir, guest)| (dir.clone(), dir, guest))
            } else {
                None
            };
            if let Some((bridge_arg, bridge_dir, guest_root)) = bridge_setup {
                cmd.arg("--bridge-lib").arg(&bridge_arg);
                let name = std::env::var("PENGUDROID_NATIVE_BRIDGE_NAME")
                    .unwrap_or_else(|_| "libberberis_arm64.so".to_string());
                cmd.arg("--native-bridge").arg(name);
                cmd.arg("--init-bridge").arg(pengudroid_nativebridge::ARM64_ISA);
                cmd.arg("--xmx").arg("128m");
                if std::env::var_os("PENGUDROID_NATIVE_BRIDGE_LIB").is_none() {
                    cmd.env("PENGUDROID_NATIVE_BRIDGE_LIB", &bridge_arg);
                }
                if std::env::var_os("PENGUDROID_GL_FORWARDERS").is_none()
                    && bridge_dir.join("libEGL.so").exists()
                {
                    cmd.env("PENGUDROID_GL_FORWARDERS", &bridge_dir);
                }
                if std::env::var_os("PENGUDROID_ARM64_GUEST_ROOT").is_none() {
                    if let Some(root) = guest_root {
                        cmd.env("PENGUDROID_ARM64_GUEST_ROOT", root);
                    }
                }
                if std::env::var_os("PENGUDROID_BRIDGE_TLS").is_none() {
                    cmd.env("PENGUDROID_BRIDGE_TLS", "1");
                }
                if std::env::var_os("BERBERIS_FLAGS").is_none() {
                    cmd.env("BERBERIS_FLAGS", "glibc-host-thread-id-handoff");
                }
                if std::env::var_os("BERBERIS_MODE").is_none() {
                    cmd.env("BERBERIS_MODE", "two-gear");
                }
                if std::env::var_os("PENGUDROID_GLESHIM").is_none() {
                    if let Some(g) = crate::nativelib::find_gleshim() {
                        cmd.env("PENGUDROID_GLESHIM", g);
                    }
                }
                println!("  bridge  : arm64 ({})", bridge_dir.display());
            } else if arm64_only {
                eprintln!(
                    "  warning : ARM64-only payload detected, but libberberis_arm64.so was not found \
                     (set PENGUDROID_NATIVE_BRIDGE_LIB)"
                );
            }
            // The JIT crashes in `nterp_op_invoke_virtual` on this framework
            // build, so the interpreter is the only working execution mode.
            cmd.arg("--vm-option").arg("-Xint");
            // Android 17 permits JNI initialization of a previously unset final
            // static field only in a debuggable-at-init runtime. Native-compiled
            // initializers (including Kotlin Companion objects) need this.
            // This enables runtime semantics, not a JDWP listening endpoint.
            cmd.arg("--vm-option").arg("-Xcompiler-option")
                .arg("--vm-option").arg("--debuggable");
            if !app_args.is_empty() {
                cmd.arg("--").args(&app_args);
            }
            apply_env(&mut cmd, &env);
            if let Some(dir) = asset_root(&app_path, &prefix, &split_paths) {
                cmd.current_dir(dir);
            }
            run_child(cmd)
        }
    }
}

/// Highest SIMD ISA tier supported by the host CPU at runtime.
fn host_simd_tier() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2")
            && std::arch::is_x86_feature_detected!("fma")
            && std::arch::is_x86_feature_detected!("bmi2")
        {
            return "x86_64 (AVX2+FMA+BMI2)";
        }
        if std::arch::is_x86_feature_detected!("sse4.2")
            && std::arch::is_x86_feature_detected!("popcnt")
        {
            return "x86_64 (SSE4.2+POPCNT)";
        }
        "x86_64 (baseline SSE2)"
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        std::env::consts::ARCH
    }
}

/// Which ABI split to keep for the chosen backend: the host ABI for a native
/// run, and for ART the architecture the translator can handle best (arm64
/// before armv7, because 32-bit translation needs a 32-bit host process).
fn bundle_abi(b: &crate::bundle::Bundle, backend: Backend) -> Option<String> {
    if b.abis.is_empty() {
        return None;
    }
    match backend {
        Backend::Native => {
            let host = crate::nativelib::host_abi();
            if b.abis.iter().any(|a| a == host) {
                Some(host.to_string())
            } else {
                Some(b.abis[0].clone())
            }
        }
        Backend::Art => {
            // Prefer the host ABI when the bundle has it. An ARM-only bundle has
            // no x86_64 split, so pick the architecture the native bridge can
            // translate: arm64-v8a before armeabi-v7a. Falling back to
            // `abis.first()` made the choice depend on split order, which for a
            // Play bundle is not a contract.
            let host = crate::nativelib::host_abi();
            if b.abis.iter().any(|a| a == host) {
                Some(host.to_string())
            } else if b.abis.iter().any(|a| a == "arm64-v8a") {
                Some("arm64-v8a".to_string())
            } else if b.abis.iter().any(|a| a == "armeabi-v7a") {
                Some("armeabi-v7a".to_string())
            } else {
                b.abis.first().cloned()
            }
        }
    }
}

/// True when the payload ships `arm64-v8a` native libraries and no `x86_64` ones.
fn is_arm64_only_payload(
    bundle_abi: Option<&str>,
    app_path: &str,
    split_paths: &[String],
) -> bool {
    if let Some(abi) = bundle_abi {
        return abi == "arm64-v8a";
    }
    let mut has_arm64 = false;
    let mut has_x86_64 = false;
    for path in std::iter::once(app_path).chain(split_paths.iter().map(String::as_str)) {
        if let Ok(entries) = apk::apk_entries(path) {
            has_arm64 |= entries
                .iter()
                .any(|e| e.starts_with("lib/arm64-v8a/") && e.ends_with(".so"));
            has_x86_64 |= entries
                .iter()
                .any(|e| e.starts_with("lib/x86_64/") && e.ends_with(".so"));
        }
    }
    has_arm64 && !has_x86_64
}

/// Find the companion `guest-arm64` sysroot tree next to an installed bridge directory.
fn guest_root_for_bridge(bridge_dir: &Path) -> Option<PathBuf> {
    for anc in bridge_dir.ancestors().take(5) {
        for rel in ["share/pengudroid/guest-arm64", "guest-arm64"] {
            let candidate = anc.join(rel);
            if candidate.is_dir() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Discover an installed ARM64 native-bridge directory (`libberberis_arm64.so`)
/// next to the running binary or inside the cached AppImage tree.
fn find_arm64_bridge_dir() -> Option<(PathBuf, Option<PathBuf>)> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("../lib/pengudroid/bridge"));
            candidates.push(dir.join("bridge"));
        }
    }
    if let Some(cache) = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    {
        if let Ok(entries) = std::fs::read_dir(cache.join("pengudroid/appimage")) {
            let mut dirs: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path().join("usr/lib/pengudroid/bridge"))
                .collect();
            dirs.sort();
            candidates.extend(dirs);
        }
    }
    if let Some(data) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    {
        candidates.push(data.join("pengudroid/bridge"));
    }
    candidates.push(PathBuf::from("/usr/lib/pengudroid/bridge"));

    for dir in candidates {
        if dir.join("libberberis_arm64.so").is_file() {
            let canon = std::fs::canonicalize(&dir).unwrap_or(dir);
            let guest = guest_root_for_bridge(&canon);
            return Some((canon, guest));
        }
    }
    None
}

fn apply_env(cmd: &mut std::process::Command, env: &[(String, String)]) {
    for (k, v) in env {
        cmd.env(k, v);
    }
}

/// Materialise the APK's `assets/` and return the directory to run the app in.
///
/// Android apps read assets through `AssetManager`, which this project serves
/// straight out of the APK, so extracting them should be unnecessary. Roblox
/// does not: it opens `content/guac/defaultConfigs/GuacDefaultPolicy-GlobalDist.json`
/// with a **relative path**, i.e. against the process working directory. When that
/// open fails its configuration map stays empty, and the next lookup returns
/// null and `libroblox.so + 0x224e4af` dereferences it — the crash that blocked
/// this app for two sessions. Running from the asset root fixes it.
///
/// The directory is created once under the prefix; a marker file records that the
/// extraction finished, so a later launch does not repeat a ~60 MB unpack.
fn asset_root(apk: &str, prefix: &Path, splits: &[String]) -> Option<PathBuf> {
    let root = prefix.join("assets");
    let marker = root.join(".extracted");
    if !marker.exists() {
        // `assets/` is only in the base APK; a split never carries it.
        let Ok(bytes) = crate::apk::extract_prefix(apk, "assets/", &root) else {
            return None;
        };
        if bytes == 0 {
            return None;
        }
        let _ = std::fs::write(&marker, bytes.to_string());
        println!("  assets  : {bytes} byte(s) -> {}", root.display());
    }
    let _ = splits;
    Some(root)
}

fn run_child(mut cmd: std::process::Command) -> i32 {
    match cmd.status() {
        Ok(s) if s.success() => 0,
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("pengudroid launch: cannot start the guest: {e}");
            1
        }
    }
}

#[cfg(test)]
mod data_dir_tests {
    use super::*;

    #[test]
    fn moving_a_tree_keeps_files_links_and_nesting() {
        let base = std::env::temp_dir().join(format!("pd-move-{}", std::process::id()));
        let from = base.join("from");
        std::fs::create_dir_all(from.join("shared_prefs")).unwrap();
        std::fs::write(from.join("shared_prefs/login.xml"), "token").unwrap();
        std::os::unix::fs::symlink("shared_prefs/login.xml", from.join("link")).unwrap();
        let to = base.join("disk/data/pkg");
        // copy_tree is what a cross-device move falls back to.
        copy_tree(&from, &to).unwrap();
        assert_eq!(std::fs::read_to_string(to.join("shared_prefs/login.xml")).unwrap(), "token");
        assert_eq!(std::fs::read_link(to.join("link")).unwrap(), PathBuf::from("shared_prefs/login.xml"));
        std::fs::remove_dir_all(&to).unwrap();
        move_tree(&from, &to).unwrap();
        assert!(!from.exists());
        assert!(to.join("shared_prefs/login.xml").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_prefix_is_per_package() {
        let p = default_prefix("com.example.game");
        assert!(p.ends_with("com.example.game"));
        assert!(p.to_string_lossy().contains("pengudroid/prefixes"));
    }

    #[test]
    fn prefix_layout_and_env_overrides() {
        let dir = std::env::temp_dir().join(format!("pd-prefix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let env = create_prefix(&dir, "com.example").unwrap();
        for sub in ["files", "cache", "code_cache", "shared_prefs"] {
            assert!(dir.join(sub).is_dir(), "{sub} missing");
        }
        assert!(env.iter().any(|(k, _)| k == "ANDROID_DATA"));
        assert!(dir.join("pengudroid.env").is_file());

        std::fs::write(
            dir.join("pengudroid.env"),
            "# comment\n\nPENGUDROID_WINDOW_WIDTH=1600\nFOO=bar\n",
        )
        .unwrap();
        let env = create_prefix(&dir, "com.example").unwrap();
        assert!(env.contains(&("PENGUDROID_WINDOW_WIDTH".into(), "1600".into())));
        assert!(env.contains(&("FOO".into(), "bar".into())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backend_names_are_stable() {
        assert_eq!(Backend::Native.name(), "native");
        assert_eq!(Backend::Art.name(), "art");
    }
}

fn needs_native_bridge(apks: &[String]) -> bool {
    let mut arm = false;
    for apk in apks {
        let Ok(libs) = crate::apk::native_libs(apk) else { continue };
        for lib in libs {
            match lib.abi.as_str() {
                "x86_64" => return false,
                "arm64-v8a" | "armeabi-v7a" => arm = true,
                _ => {}
            }
        }
    }
    arm
}
