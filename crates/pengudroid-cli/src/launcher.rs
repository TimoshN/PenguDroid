//! `pengudroid launch` — a Proton-style front end.
//!
//! A "prefix" is a per-application directory that holds the app's writable
//! state and its environment overrides, exactly like Proton's `compatdata`.
//! Launching picks the best backend for the APK:
//!
//! * **native** — a host-ABI `NativeActivity` app. Its own `libmain.so` runs on
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
/// `native` wins whenever the APK ships a host-ABI `NativeActivity` payload,
/// because that path needs no Java runtime at all. Otherwise the presence of
/// `classes.dex` means ART is the only option.
pub fn plan(apk_path: &str) -> Result<(ApkInfo, Plan), String> {
    let path = std::path::Path::new(apk_path);
    if crate::bundle::is_bundle(path) {
        return plan_bundle(path);
    }
    let info = apk::read_apk(apk_path)?;
    let entries = apk::apk_entries(apk_path).unwrap_or_default();
    let host_abi = crate::nativelib::host_abi();
    let native_prefix = format!("lib/{host_abi}/");
    let has_host_lib = entries
        .iter()
        .any(|e| e.starts_with(&native_prefix) && e.ends_with(".so"));
    let has_dex = entries
        .iter()
        .any(|e| e.ends_with(".dex") || e == "classes.dex");
    let has_native_activity = entries
        .iter()
        .any(|e| e.starts_with(&native_prefix) && e.ends_with("libmain.so"));

    if has_native_activity {
        Ok((
            info,
            Plan {
                backend: Backend::Native,
                reason: format!("{host_abi} NativeActivity payload (libmain.so)"),
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
                reason: if has_host_lib {
                    format!("{host_abi} libraries but no NativeActivity entry point")
                } else {
                    "DEX only, needs the Android runtime".into()
                },
            },
        ))
    } else {
        Err(format!(
            "{apk_path} has neither a {host_abi} native payload nor classes.dex"
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
    let mut signature_apk: Option<String> = None;
    let mut app_args: Vec<String> = Vec::new();
    let mut log_arg: Option<String> = None;
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
            "--log" => log_arg = it.next().cloned(),
            "--signature-apk" => signature_apk = it.next().cloned(),
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
    if let Some(path) = log_arg {
        if let Err(error) = enable_launch_log(&path) {
            eprintln!("pengudroid launch: cannot open launch log {path}: {error}");
            return 1;
        }
    }
    configure_aurora_user_dirs();
    let Some(apk_path) = apk_path.map(expand_user_path).map(resolve_package_apk_path) else {
        eprintln!(
            "usage: pengudroid launch <app.apk> [--backend auto|native|art] [--prefix DIR] \
             [--sysroot DIR] [--width PX] [--height PX] [--density DPI] [-- ARGS]"
        );
        return 2;
    };
    if let Some(path) = signature_apk {
        // SAFETY: launch option parsing is still single-threaded. The value is
        // inherited by the ART helper and consumed before app binding.
        unsafe { std::env::set_var("PENGUDROID_SIGNATURE_APK", expand_user_path(path)) };
    }

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

    if bundle_selected_abi.as_deref() == Some("armeabi-v7a") && !cfg!(target_arch = "arm") {
        eprintln!(
            "pengudroid launch: {} only ships 32-bit armeabi-v7a libraries; \
             this PenguDroid build requires a 64-bit arm64-v8a or x86_64 runtime",
            info.package
        );
        return 1;
    }

    let _launch_lock = match acquire_launch_lock(&info.package) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!("pengudroid launch: {} is already running", info.package);
            return 0;
        }
        Err(error) => {
            eprintln!("pengudroid launch: cannot create instance lock: {error}");
            return 1;
        }
    };

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
            let mut surface_size = (width.unwrap_or(720), height.unwrap_or(1280));
            let mut cmd = std::process::Command::new(std::env::current_exe().unwrap_or_default());
            cmd.arg("art-test")
                .arg("--sysroot")
                .arg(&sysroot)
                .arg("--classpath")
                .arg(&app_path);
            for split in &split_paths {
                cmd.arg("--classpath").arg(split);
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
                surface_size = (
                    width.unwrap_or(geometry.width.max(1) as u32),
                    height.unwrap_or(geometry.height.max(1) as u32),
                );
                cmd.env(crate::display::SURFACE_W_ENV, geometry.width.to_string());
                cmd.env(crate::display::SURFACE_H_ENV, geometry.height.to_string());
            }
            // Show a host window immediately instead of leaving Lipstick's
            // splash up while ART interprets the application startup code.
            // With no explicit size, Aurora takes the current wl_output mode
            // instead of inventing a fixed phone resolution.
            let use_wayland_output = aurora_hybris_host()
                && width.is_none() && height.is_none()
                && std::env::var_os(crate::display::DISPLAY_ENV).is_none();
            let requested_size = if use_wayland_output { (0, 0) } else { surface_size };
            let _loading_surface = match precreate_loading_surface(
                requested_size.0, requested_size.1, &info.package,
            ) {
                Ok((client, id, output_size)) => {
                    if use_wayland_output {
                        cmd.env(crate::display::SURFACE_W_ENV, output_size.0.to_string());
                        cmd.env(crate::display::SURFACE_H_ENV, output_size.1.to_string());
                        println!(
                            "  wayland : output {}x{} px",
                            output_size.0, output_size.1,
                        );
                    }
                    cmd.env("PENGUDROID_SURFACE_ID", id.to_string());
                    Some((client, id, output_size))
                }
                Err(error) => {
                    eprintln!("pengudroid launch: loading surface unavailable: {error}");
                    None
                }
            };
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
            // Export MangoHud configuration only when the overlay was
            // explicitly requested. A non-empty MANGOHUD_CONFIG itself enables
            // the in-frame overlay, so setting it unconditionally enabled the
            // FPS widget for every production launcher invocation.
            let fps_requested = std::env::var("PENGUDROID_FPS_METER")
                .is_ok_and(|v| v == "1" || v == "true")
                || std::env::var("MANGOHUD").is_ok_and(|v| v == "1" || v == "true");
            if std::env::var_os("MANGOHUD").is_none()
                && std::env::var("PENGUDROID_FPS_METER").is_ok_and(|v| v == "1" || v == "true")
            {
                cmd.env("MANGOHUD", "1");
            }
            if fps_requested && std::env::var_os("MANGOHUD_CONFIG").is_none() {
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
            // Native ARM64 payloads need no translation on an AArch64 host.
            // Berberis in this tree is strictly an ARM64-to-x86_64 bridge.
            let needs_arm64_bridge = cfg!(target_arch = "x86_64") && is_arm64_only_payload(
                bundle_selected_abi.as_deref(),
                &app_path,
                &split_paths,
            );
            let explicit_bridge = std::env::var("PENGUDROID_NATIVE_BRIDGE_LIB").ok();
            #[cfg(not(target_arch = "x86_64"))]
            if explicit_bridge.is_some() {
                eprintln!("pengudroid launch: the bundled Berberis bridge is only supported on x86_64 hosts");
                return 1;
            }
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
            } else if needs_arm64_bridge {
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
            } else if needs_arm64_bridge {
                eprintln!(
                    "  warning : ARM64-only payload detected, but libberberis_arm64.so was not found \
                     (set PENGUDROID_NATIVE_BRIDGE_LIB)"
                );
            }
            // JIT improves warm scrolling, but nterp recovery is not reliable
            // during cold Sailjail launches (and ARM32 cannot create its code
            // cache). Keep Xint as the safe default; explicit 1 enables the
            // performance experiment, explicit 0 forces the interpreter.
            let jit_flag = |value: &str| {
                let value = value.trim();
                value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
            };
            let jit = std::env::var("PENGUDROID_ART_JIT").ok().map(|v| jit_flag(&v))
                .or_else(|| env.iter().find(|(key, _)| key == "PENGUDROID_ART_JIT")
                    .map(|(_, value)| jit_flag(value)))
                .unwrap_or(false);
            if !jit {
                cmd.arg("--vm-option").arg("-Xint");
            } else {
                eprintln!("pengudroid launch: ART JIT enabled (without -Xint)");
            }
            if !app_args.is_empty() {
                cmd.arg("--").args(&app_args);
            }
            apply_aurora_package_env(&mut cmd, &info.package, &env);
            apply_env(&mut cmd, &env);
            if let Some(dir) = asset_root(&app_path, &prefix, &split_paths) {
                cmd.current_dir(dir);
            }
            let status = run_child(cmd);
            finish_art_application(status, &_loading_surface);
            status
        }
    }
}

/// ART owns the Android lifecycle. Once it exits, do not leave the early
/// Wayland surface (or a last black buffer) looking like a live application.
/// Standalone Aurora packages own their daemon, so terminate that process too;
/// on a regular shared desktop daemon only this application's surface is
/// destroyed.
fn finish_art_application(
    status: i32,
    loading_surface: &Option<(crate::Client, u32, (u32, u32))>,
) {
    eprintln!("pengudroid launch: ART exited; closing the application (status {status})");
    let Some((client, surface_id, _)) = loading_surface else { return };
    if let Err(error) = client.request(&pengudroid_proto::Message::DestroySurface { surface_id: *surface_id }) {
        eprintln!("pengudroid launch: cannot destroy ART surface: {error}");
        return;
    }
    if aurora_package_data_dir().is_some() {
        if let Err(error) = crate::client::stop() {
            eprintln!("pengudroid launch: cannot stop application daemon: {error}");
        }
    }
}

/// Use package-owned executable libraries when running from an Aurora desktop
/// launcher. Home and /tmp are noexec there, and the desktop session does not
/// carry the environment used by interactive art-test debugging.
/// Current APM package's data directory, derived from the resolved executable
/// (/opt/app/<package>/<version>/bin/<executable>). This supports standalone
/// Android application packages instead of assuming the MaxNext package.
fn aurora_package_data_dir() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    // Inside Firejail current_exe may be /usr/bin/<package> even though APM
    // stores its data below /opt/app. The executable name is the package name.
    if let Some(package) = executable.file_name() {
        let data = PathBuf::from("/opt/app").join(package).join("current/data");
        if data.is_dir() { return Some(data); }
    }
    let executable = std::fs::canonicalize(&executable).unwrap_or(executable);
    let bin = executable.parent()?;
    if bin.file_name()? != "bin" { return None; }
    let data = bin.parent()?.parent()?.join("data");
    data.is_dir().then_some(data)
}

fn aurora_package_name() -> Option<String> {
    // data -> version -> package
    Some(aurora_package_data_dir()?.parent()?.parent()?.file_name()?
        .to_string_lossy().into_owned())
}

fn aurora_application_name() -> Option<String> {
    aurora_package_name()?.rsplit('.').next().map(str::to_owned)
}

fn theme_scaled_density_dpi(ratio: f64) -> Option<i32> {
    let dpi = (ratio * 1.5 * 160.0).round();
    (ratio.is_finite() && ratio > 0.0 && (80.0..=960.0).contains(&dpi))
        .then_some(dpi as i32)
}

fn aurora_theme_pixel_ratio_from_libdconf() -> Option<f64> {
    use std::ffi::{c_char, c_double, c_void, CString};

    type DconfClientNew = unsafe extern "C" fn() -> *mut c_void;
    type DconfClientRead =
        unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;
    type VariantGetDouble = unsafe extern "C" fn(*mut c_void) -> c_double;
    type Unref = unsafe extern "C" fn(*mut c_void);

    // Sailjail's --private-bin intentionally hides /usr/bin/dconf. The shared
    // library remains available and reads the same whitelisted user database
    // without spawning an executable outside the application's private bin.
    unsafe {
        let library = CString::new("libdconf.so.1").ok()?;
        let handle = libc::dlopen(library.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
        if handle.is_null() {
            return None;
        }
        let result = (|| {
            let symbol = |name: &str| {
                let name = CString::new(name).ok()?;
                let symbol = libc::dlsym(handle, name.as_ptr());
                (!symbol.is_null()).then_some(symbol)
            };
            let new: DconfClientNew = std::mem::transmute(symbol("dconf_client_new")?);
            let read: DconfClientRead = std::mem::transmute(symbol("dconf_client_read")?);
            let get_double: VariantGetDouble =
                std::mem::transmute(symbol("g_variant_get_double")?);
            let variant_unref: Unref = std::mem::transmute(symbol("g_variant_unref")?);
            let object_unref: Unref = std::mem::transmute(symbol("g_object_unref")?);
            let key = CString::new("/desktop/sailfish/silica/theme_pixel_ratio").ok()?;
            let client = new();
            if client.is_null() {
                return None;
            }
            let value = read(client, key.as_ptr());
            object_unref(client);
            if value.is_null() {
                return None;
            }
            let ratio = get_double(value);
            variant_unref(value);
            Some(ratio)
        })();
        // DConfClient is registered in GLib's global type table. Unloading
        // libdconf after the first read makes a later read fail during type
        // registration, so retain the library for this launcher's lifetime.
        result
    }
}

fn visible_statusbar_inset(visible: bool, statusbar: i32, cutout: i32) -> i32 {
    let statusbar = if visible { statusbar } else { 0 };
    statusbar.max(cutout).clamp(0, 4096)
}

/// Read the optional Lipstick status-bar/cutout heights without a device-
/// specific fallback. Missing dconf keys contribute zero. The extended app
/// surface requests a visible status bar; an explicit boolean visibility key,
/// when present, takes precedence.
fn aurora_top_inset_from_dconf_using(client: *mut std::ffi::c_void) -> Option<i32> {
    use std::ffi::{c_char, c_void, CString};
    type DconfClientNew = unsafe extern "C" fn() -> *mut c_void;
    type DconfClientRead = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;
    type VariantGetBoolean = unsafe extern "C" fn(*mut c_void) -> i32;
    type VariantGetInt = unsafe extern "C" fn(*mut c_void) -> i32;
    type Unref = unsafe extern "C" fn(*mut c_void);
    unsafe {
        let library = CString::new("libdconf.so.1").ok()?;
        let handle = libc::dlopen(library.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
        if handle.is_null() { return None; }
        let symbol = |name: &str| {
            let name = CString::new(name).ok()?;
            let symbol = libc::dlsym(handle, name.as_ptr());
            (!symbol.is_null()).then_some(symbol)
        };
        let new: DconfClientNew = std::mem::transmute(symbol("dconf_client_new")?);
        let read: DconfClientRead = std::mem::transmute(symbol("dconf_client_read")?);
        let get_boolean: VariantGetBoolean = std::mem::transmute(symbol("g_variant_get_boolean")?);
        let get_int: VariantGetInt = std::mem::transmute(symbol("g_variant_get_int32")?);
        let variant_unref: Unref = std::mem::transmute(symbol("g_variant_unref")?);
        let object_unref: Unref = std::mem::transmute(symbol("g_object_unref")?);
        let owned = client.is_null();
        let client = if owned { new() } else { client };
        if client.is_null() { return None; }
        let read_variant = |key: &str| {
            let key = CString::new(key).ok()?;
            let value = read(client, key.as_ptr());
            (!value.is_null()).then_some(value)
        };
        let read_bool = |key: &str| {
            let value = read_variant(key)?;
            let result = get_boolean(value) != 0;
            variant_unref(value);
            Some(result)
        };
        let read_int = |key: &str| {
            let value = read_variant(key)?;
            let result = get_int(value);
            variant_unref(value);
            Some(result)
        };
        let as_inset = read_bool(
            "/desktop/sailfish/silica/statusbar/statusbarAsInset",
        ).unwrap_or(false);
        let statusbar = if as_inset {
            read_int("/desktop/sailfish/silica/statusbar/height").unwrap_or(0)
        } else { 0 };
        let cutout = read_int("/desktop/sailfish/silica/cutout_area/height").unwrap_or(0);
        let visible = read_bool("/desktop/sailfish/silica/statusbar/visible").unwrap_or(true);
        if owned { object_unref(client); }
        let inset = visible_statusbar_inset(visible, statusbar, cutout);
        println!(
            "pengudroid launch: dconf statusbarAsInset={as_inset} statusbar={statusbar}px cutout={cutout}px visible={visible} -> top inset={inset}px"
        );
        Some(inset)
    }
}

pub(crate) fn aurora_top_inset_from_dconf() -> Option<i32> {
    aurora_top_inset_from_dconf_using(std::ptr::null_mut())
}

pub(crate) fn aurora_top_inset_from_dconf_client(
    client: *mut std::ffi::c_void,
) -> Option<i32> {
    aurora_top_inset_from_dconf_using(client)
}

unsafe extern "C" fn dconf_inset_changed(
    client: *mut std::ffi::c_void, _prefix: *const std::ffi::c_char,
    _changes: *const *const std::ffi::c_char, _tag: *const std::ffi::c_char,
    user_data: *mut std::ffi::c_void,
) {
    let Some(height) = aurora_top_inset_from_dconf_client(client) else { return };
    let fd = user_data as usize as i32;
    let bytes = height.to_le_bytes();
    if libc::write(fd, bytes.as_ptr() as *const std::ffi::c_void, bytes.len()) == 4 {
        println!("pengudroid launch: forwarded changed dconf top inset={height}px");
    }
}

/// Run DConfClient's changed signal in the launcher, where host dconf access is
/// known to work, and forward only the computed integer to the ART child.
fn spawn_aurora_dconf_inset_listener(write_fd: i32) {
    std::thread::spawn(move || {
        let run = || -> Option<()> {
            use std::ffi::{c_char, c_void, CString};
            type New = unsafe extern "C" fn() -> *mut c_void;
            type Watch = unsafe extern "C" fn(*mut c_void, *const c_char);
            type LoopNew = unsafe extern "C" fn(*mut c_void, i32) -> *mut c_void;
            type LoopRun = unsafe extern "C" fn(*mut c_void);
            type SignalConnect = unsafe extern "C" fn(
                *mut c_void, *const c_char, *mut c_void, *mut c_void,
                *mut c_void, i32,
            ) -> u64;
            unsafe {
                let library = CString::new("libdconf.so.1").ok()?;
                let handle = libc::dlopen(
                    library.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL,
                );
                if handle.is_null() { return None; }
                let symbol = |name: &str| {
                    let name = CString::new(name).ok()?;
                    let symbol = libc::dlsym(handle, name.as_ptr());
                    (!symbol.is_null()).then_some(symbol)
                };
                let new: New = std::mem::transmute(symbol("dconf_client_new")?);
                let watch: Watch = std::mem::transmute(symbol("dconf_client_watch_sync")?);
                let connect: SignalConnect =
                    std::mem::transmute(symbol("g_signal_connect_data")?);
                let loop_new: LoopNew = std::mem::transmute(symbol("g_main_loop_new")?);
                let loop_run: LoopRun = std::mem::transmute(symbol("g_main_loop_run")?);
                let client = new();
                if client.is_null() { return None; }
                let signal = CString::new("changed").ok()?;
                if connect(
                    client, signal.as_ptr(), dconf_inset_changed as *mut c_void,
                    write_fd as usize as *mut c_void, std::ptr::null_mut(), 0,
                ) == 0 {
                    return None;
                }
                for path in [
                    "/desktop/sailfish/silica/statusbar/",
                    "/desktop/sailfish/silica/cutout_area/",
                ] {
                    let path = CString::new(path).ok()?;
                    watch(client, path.as_ptr());
                }
                let main_loop = loop_new(std::ptr::null_mut(), 0);
                if main_loop.is_null() { return None; }
                println!("pengudroid launch: listening for dconf status-bar/cutout changes");
                loop_run(main_loop);
            }
            Some(())
        };
        let _ = run();
    });
}

fn aurora_theme_density_dpi() -> Option<(f64, i32)> {
    let ratio = aurora_theme_pixel_ratio_from_libdconf().or_else(|| {
        // Keep a generic-host fallback; on Aurora the in-process path above is
        // required because application launchers run inside Sailjail.
        let output = std::process::Command::new("dconf")
            .args(["read", "/desktop/sailfish/silica/theme_pixel_ratio"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        std::str::from_utf8(&output.stdout).ok()?.trim().parse::<f64>().ok()
    })?;
    theme_scaled_density_dpi(ratio).map(|dpi| (ratio, dpi))
}

fn aurora_hybris_host() -> bool {
    let egl = if cfg!(target_arch = "arm") {
        "/usr/libexec/droid-hybris/system/lib/libEGL.so"
    } else {
        "/usr/libexec/droid-hybris/system/lib64/libEGL.so"
    };
    Path::new("/usr/lib/libhybris-common.so.1").is_file() && Path::new(egl).is_file()
}

fn apply_aurora_package_env(
    cmd: &mut std::process::Command,
    package: &str,
    configured_env: &[(String, String)],
) {
    if !cfg!(any(target_arch = "aarch64", target_arch = "arm")) {
        return;
    }
    let Some(lib) = aurora_package_data_dir().map(|data| data.join("lib")) else { return };
    if !lib.join("libart.so").is_file() {
        return;
    }
    let overridden = |name: &str| {
        std::env::var_os(name).is_some()
            || configured_env.iter().any(|(key, _)| key == name)
    };
    cmd.env("PENGUDROID_ART_LIBS", &lib);
    let hybris = aurora_hybris_host();
    if hybris {
        if !overridden("PENGUDROID_VSYNC_ON_PRESENT") {
            // Hosted IDisplayEventConnection.requestNextVsync is now a real
            // one-shot scheduler operation. Do not also emit a second pulse
            // from every present. Setting 1 retains the old recovery/
            // compatibility behavior as an explicit override.
            cmd.env("PENGUDROID_VSYNC_ON_PRESENT", "0");
        }
        if !overridden("PENGUDROID_VSYNC_INTERVAL_US") {
            cmd.env("PENGUDROID_VSYNC_INTERVAL_US", "33333");
        }
        if !overridden("PENGUDROID_CLEAR_FOCUS_ON_OUTSIDE") {
            // Keep MAX's phone/country editors focused when tapping elsewhere;
            // its nested login/BottomSheet flow relies on that focus. Other
            // Hybris apps retain the compatibility outside-tap hide.
            let enabled = if package == "ru.oneme.app" { "0" } else { "1" };
            cmd.env("PENGUDROID_CLEAR_FOCUS_ON_OUTSIDE", enabled);
        }
        if !overridden("PENGUDROID_DENSITY_DPI") {
            if let Some((ratio, dpi)) = aurora_theme_density_dpi() {
                // Match the physical panel size across apps, but derive the
                // logical Android dp scale from Silica's current theme.
                cmd.env("PENGUDROID_DENSITY_DPI", dpi.to_string());
                println!(
                    "pengudroid launch: theme_pixel_ratio={ratio} -> Android densityDpi={dpi} (x1.5)"
                );
            }
        }
    }
    let force_shm = overridden("PENGUDROID_FORCE_SHM")
        && !overridden("PENGUDROID_FORCE_DMABUF");
    if hybris && !force_shm && !overridden("PENGUDROID_FORCE_DMABUF") {
        // This Lipstick exposes neither linux-dmabuf nor wl_drm. Native gralloc
        // with an fd-backed wl_shm pool avoids the costly glReadPixels path.
        if !overridden("PENGUDROID_GRALLOC") {
            cmd.env("PENGUDROID_GRALLOC", "1");
        }
        if !overridden("PENGUDROID_GRALLOC_SHM") {
            cmd.env("PENGUDROID_GRALLOC_SHM", "1");
        }
        // The daemon's WLEGL path imports guest gralloc buffers as EGLImages
        // and presents through its own EGLSurface/eglSwapBuffers. Use it by
        // default on Hybris; PENGUDROID_GRALLOC_WLEGL=0 restores wl_shm.
        if cfg!(any(target_arch = "aarch64", target_arch = "arm"))
            && !overridden("PENGUDROID_GRALLOC_WLEGL")
        {
            // Complete on AArch64 since e660dda. On ARM32 the daemon uses the
            // manual android_wlegl wl_buffer transport, falls back to
            // hybris_gralloc_retain when hybris_gralloc_import_buffer is
            // missing, and loads the libgralloc/eglplatformcommon graph as a
            // set; the guest still lands on wl_shm if any of this fails.
            cmd.env("PENGUDROID_GRALLOC_WLEGL", "1");
            if cfg!(target_arch = "arm") {
                // The daemon attaches the buffer to Lipstick directly; the
                // guest has to deliver it upright (GPU Y-flip).
                cmd.env("PENGUDROID_WLEGL_BUFFER", "1");
            }
        }
        // The compositor frame clock is likewise opt-in
        // (PENGUDROID_VSYNC_COMPOSITOR=1); the existing hosted
        // requestNextVsync path remains the default.
    } else if !overridden("PENGUDROID_FORCE_SHM") {
        // Preserve the verified fallback on other package hosts.
        cmd.env("PENGUDROID_FORCE_SHM", "1");
    }
    if std::env::var_os("PENGUDROID_FORCE_TRAVERSAL").is_none() {
        cmd.env("PENGUDROID_FORCE_TRAVERSAL", "1");
    }
    if std::env::var_os("PENGUDROID_SUPPRESS_DESTRUCTORS").is_none() {
        cmd.env("PENGUDROID_SUPPRESS_DESTRUCTORS", "1");
    }
    if std::env::var_os("PENGUDROID_NO_CODEC2_SERVICE").is_none() {
        cmd.env("PENGUDROID_NO_CODEC2_SERVICE", "1");
    }
    if !overridden("PENGUDROID_STATUSBAR_HEIGHT") {
        let height = aurora_top_inset_from_dconf().unwrap_or(0);
        cmd.env("PENGUDROID_STATUSBAR_HEIGHT", height.to_string());
        let watch_disabled = std::env::var_os("PENGUDROID_DCONF_STATUSBAR_WATCH").as_deref()
            == Some(std::ffi::OsStr::new("0"))
            || configured_env.iter().any(|(key, value)| {
                key == "PENGUDROID_DCONF_STATUSBAR_WATCH" && value == "0"
            });
        if !watch_disabled {
            let mut fds = [0; 2];
            if unsafe { libc::pipe(fds.as_mut_ptr()) } == 0 {
                cmd.env("PENGUDROID_DCONF_INSET_FD", fds[0].to_string());
                spawn_aurora_dconf_inset_listener(fds[1]);
            } else {
                eprintln!("pengudroid launch: cannot create dconf inset pipe: {}", std::io::Error::last_os_error());
            }
        }
    }
    let app_lib = lib.join(package);
    if app_lib.is_dir() {
        cmd.env("PENGUDROID_NATIVE_LIB_DIR", &app_lib);
        cmd.env("PENGUDROID_NATIVE_LIB_DIRS", &app_lib);
    }
}

fn configure_aurora_user_dirs() {
    let Some(package_data) = aurora_package_data_dir() else { return };
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return };
    let Some(application) = aurora_application_name() else { return };
    let data = home.join(format!(".local/share/io.github.denizomer10/{application}"));
    let cache = home.join(format!(".cache/io.github.denizomer10/{application}"));
    // Environment mutation is process-global. This happens on the launcher
    // thread before any runtime, daemon or render thread has been created.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", &data);
        std::env::set_var("XDG_CACHE_HOME", &cache);
    }
    let packaged = package_data.join("runtime").join(crate::runtime::RUNTIME_DIR);
    let runtime = home.join(format!("Documents/PenguDroid/runtime/{}", crate::runtime::RUNTIME_DIR));
    let private_runtime = cache.join(format!("pengudroid/runtime/{}", crate::runtime::RUNTIME_DIR));
    // New RPMs contain the framework directly. Prefer that read-only tree and
    // never unpack it at launch; legacy extracted trees remain usable overrides
    // for existing installations and development packages.
    for candidate in [packaged, runtime, private_runtime] {
        if candidate.join("system/framework/framework.jar").is_file() {
            eprintln!("pengudroid launch: Android runtime {}", candidate.display());
            unsafe { std::env::set_var("PENGUDROID_ART_SYSROOT", candidate) };
            break;
        }
    }
}

fn aurora_cache_dir() -> Option<PathBuf> {
    aurora_package_data_dir()?;
    let application = aurora_application_name()?;
    // The application home cache is whitelisted in every Firejail instance
    // and needs no privileged /srv/shared provisioning.
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(format!(".cache/io.github.denizomer10/{application}/cache"))
    })
}

/// Capture both the launcher and its child in a persistent per-user log. A
/// bare filename is placed in the usual cache directory, which is especially
/// useful for desktop sandbox launches where stdout is not visible.
fn enable_launch_log(path: &str) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let path = expand_user_path(path.to_string());
    let path = if path.contains('/') {
        PathBuf::from(path)
    } else {
        let home = std::env::var_os("HOME").unwrap_or_default();
        if let Some(cache) = aurora_cache_dir() {
            cache.join(path)
        } else {
            PathBuf::from(home).join(".cache/pengudroid").join(path)
        }
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    unsafe { std::env::set_var("PENGUDROID_DAEMON_KEEP_LOG", "1") };
    for fd in [1, 2] {
        if unsafe { libc::dup2(file.as_raw_fd(), fd) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Expand a literal `$HOME/` or `~/` supplied by a desktop Exec entry. Desktop
/// launchers do not run a shell, so the launcher itself performs this mapping.
fn expand_user_path(path: String) -> String {
    let suffix = path.strip_prefix("$HOME/").or_else(|| path.strip_prefix("~/"));
    let Some(suffix) = suffix else { return path };
    let Some(home) = std::env::var_os("HOME") else { return path };
    PathBuf::from(home).join(suffix).display().to_string()
}

/// Resolve a bare APK filename from the package's own apps directory. This
/// keeps desktop entries relocatable: `launch MaxNext.apk` needs neither a
/// hardcoded home directory nor shell expansion. Explicit paths still win.
fn resolve_package_apk_path(path: String) -> String {
    if path.contains('/') || PathBuf::from(&path).is_file() {
        return path;
    }
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("PENGUDROID_APK_DIR") {
        dirs.push(PathBuf::from(dir));
    }
    if let Some(data) = aurora_package_data_dir() {
        dirs.push(data.join("apps"));
    }
    if let Some(package) = aurora_package_name() {
        dirs.push(PathBuf::from("/usr/share").join(package).join("apps"));
    }
    for dir in dirs {
        let candidate = dir.join(&path);
        if candidate.is_file() {
            return candidate.display().to_string();
        }
    }
    path
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

/// Create the visible loading window before starting ART. Keep the returned
/// client alive until the guest exits: the daemon ties surface ownership to
/// the creating process. The GL shim attaches through PENGUDROID_SURFACE_ID.
fn precreate_loading_surface(
    width: u32,
    height: u32,
    package: &str,
) -> Result<(crate::client::Client, u32, (u32, u32)), String> {
    use pengudroid_proto::{Message, Role};
    let socket = pengudroid_proto::default_socket_path();
    let client = crate::client::Client::connect_to(&socket)
        .map_err(|e| format!("cannot connect to {}: {e}", socket.display()))?;
    client.handshake(Role::Cli).map_err(|e| e.to_string())?;
    let title = match package {
        "ru.oneme.app" => "MaxNext".to_string(),
        "ru.sberbankmobile" => "СберБанк".to_string(),
        _ => format!("PenguDroid: {package}"),
    };
    let (reply, _fds) = client
        .request(&Message::CreateSurface { width, height, title })
        .map_err(|e| e.to_string())?;
    match reply {
        Message::SurfaceCreated { surface_id, .. } => {
            // create_toplevel is followed by the compositor's initial
            // configure. Query briefly so Android receives the actual Wayland
            // window size rather than the pre-create fallback.
            let mut geometry = None;
            for _ in 0..50 {
                if let Some((buffer_w, buffer_h, display_w, display_h)) =
                    client.surface_geometry(surface_id).ok().flatten()
                {
                    let (w, h) = if display_w > 0 && display_h > 0 {
                        (display_w, display_h)
                    } else {
                        (buffer_w, buffer_h)
                    };
                    if w > 0 && h > 0 {
                        geometry = Some((w, h));
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Ok((client, surface_id, geometry.unwrap_or((width, height))))
        }
        Message::Error { code, message } => Err(format!("daemon error {code}: {message}")),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

/// Keep one desktop launcher (and therefore one daemon/window) per Android
/// package. Lipstick's single-instance hint is not sufficient for direct D-Bus
/// or audb launches, so the lifetime of this open file follows `cmd.status()`.
fn acquire_launch_lock(package: &str) -> std::io::Result<Option<std::fs::File>> {
    use std::os::fd::AsRawFd;
    let socket = pengudroid_proto::default_socket_path();
    let runtime_dir = socket.parent().unwrap_or_else(|| Path::new("/tmp"));
    let dir = match (std::env::var_os("HOME"), aurora_application_name()) {
        (Some(home), Some(application)) if aurora_package_data_dir().is_some() => {
            // Unlike /run/user, this whitelisted home directory is shared by
            // all Firejail instances of the same standalone application.
            PathBuf::from(home)
                .join(format!(".cache/io.github.denizomer10/{application}/cache"))
        }
        _ => runtime_dir.to_path_buf(),
    };
    std::fs::create_dir_all(&dir)?;
    let file = std::fs::OpenOptions::new()
        .read(true).write(true).create(true)
        .open(dir.join(format!("launch-{package}.lock")))?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(file));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(error)
    }
}

fn run_child(mut cmd: std::process::Command) -> i32 {
    match cmd.status() {
        Ok(s) if s.success() => 0,
        Ok(s) => {
            eprintln!("pengudroid launch: guest exited with {s}");
            s.code().unwrap_or(1)
        }
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

    #[test]
    fn silica_theme_ratio_drives_android_density() {
        assert_eq!(theme_scaled_density_dpi(1.75), Some(420));
        assert_eq!(theme_scaled_density_dpi(2.0), Some(480));
        assert_eq!(theme_scaled_density_dpi(1.0), Some(240));
        assert_eq!(theme_scaled_density_dpi(0.0), None);
        assert_eq!(theme_scaled_density_dpi(f64::NAN), None);
    }

    #[test]
    fn statusbar_inset_uses_visibility_and_cutout_without_a_device_constant() {
        assert_eq!(visible_statusbar_inset(true, 80, 96), 96);
        assert_eq!(visible_statusbar_inset(true, 96, 80), 96);
        assert_eq!(visible_statusbar_inset(false, 96, 80), 80);
        assert_eq!(visible_statusbar_inset(true, 0, 0), 0);
    }

}
