//! Per-user Android runtime discovery and provisioning.
//!
//! Apps that need ART cannot run without Android's own runtime binaries, so the
//! API-37 runtime payload remains necessary; an extracted GSI is no longer
//! required when a modular store is supplied. What must not be a dependency is
//! *the user remembering where it is*: this module owns the one cache location
//! (`$XDG_CACHE_HOME/pengudroid`, else `~/.cache/pengudroid`), finds an
//! already-prepared tree there (including an assembled modular runtime), and
//! can prepare one on demand through
//! `tools/prepare-android17.py` — the same verified, offline extractor the
//! developers use. `pengudroid launch` and the desktop app both resolve the
//! runtime through this, so `--sysroot` is only ever needed to override it.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Directory name of the prepared runtime inside the cache.
pub const RUNTIME_DIR: &str = "android17-x86_64";
/// Cache directory name this project owns.
const CACHE_NAME: &str = "pengudroid";
const LEGACY_MODULAR_DIR: &str = "android17-x86_64-modular";
const MODULAR_DIR: &str = "android17-x86_64-modular-current";
const MODULAR_VERSIONS: &str = "android17-x86_64-modular-versions";
/// Relative location of the preparation tool inside a checkout.
const PREPARE_SCRIPT: &str = "tools/prepare-android17.py";

/// Preserve the AppImage's private libc and dependency closure in helpers.
pub fn host_library_path(paths: &str) -> String {
    match std::env::var("PENGUDROID_PORTABLE_LIBS") {
        Ok(libs) if !libs.is_empty() => format!("{libs}:{paths}"),
        _ => paths.to_string(),
    }
}

fn non_empty(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// `$XDG_CACHE_HOME/pengudroid`, else `$HOME/.cache/pengudroid`.
pub fn cache_root() -> Option<PathBuf> {
    if let Some(dir) = non_empty("XDG_CACHE_HOME") {
        return Some(dir.join(CACHE_NAME));
    }
    non_empty("HOME").map(|home| home.join(".cache").join(CACHE_NAME))
}

/// The canonical prepared-runtime location, whether or not it exists yet.
pub fn cached_runtime() -> Option<PathBuf> {
    cache_root().map(|base| base.join("runtime").join(RUNTIME_DIR))
}

/// The runtime assembled from independently packaged modules.
pub fn modular_runtime() -> Option<PathBuf> {
    cache_root().map(|base| base.join("runtime").join(MODULAR_DIR))
}

/// Explicit overrides, then the modular runtime, checkout, and legacy cache.
pub fn locate() -> Option<PathBuf> {
    let existing = |path: PathBuf| path.is_dir().then_some(path);
    if let Some(explicit) = non_empty("PENGUDROID_RUNTIME").and_then(&existing) {
        return Some(explicit);
    }
    if let Some(explicit) = non_empty("PENGUDROID_ART_SYSROOT").and_then(&existing) {
        return Some(explicit);
    }
    if let Some(explicit) = non_empty("PENGUDROID_SYSROOT").and_then(&existing) {
        return Some(explicit);
    }
    let prefer_components = module_store().is_some();
    let legacy = cache_root().map(|base| base.join("runtime").join(LEGACY_MODULAR_DIR));
    for path in [modular_runtime(), legacy].into_iter().flatten() {
        if path.join("pengudroid-modular-runtime.json").is_file()
            && (!prefer_components || path.join(pengudroid_runtime::DESCRIPTOR).is_file())
            && crate::sysroot::Layout::detect(&path).is_ok() {
            return std::fs::canonicalize(&path).ok();
        }
    }
    // A supplied/bundled component store is authoritative for automatic
    // discovery. Let provisioning migrate old tree installs; explicit overrides
    // above retain their existing priority.
    if prefer_components { return None; }
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(tree) = existing(cwd.join(".toolchain/sysroot-android17")) {
            return Some(tree);
        }
    }
    cached_runtime().and_then(existing)
}

fn ancestors(start: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    let mut current = Some(start);
    std::iter::from_fn(move || {
        let path = current?;
        current = path.parent();
        Some(path.to_path_buf())
    })
}

/// The preparation tool: `$PENGUDROID_PREPARE_SCRIPT`, else a checkout found by
/// walking up from the executable and from the working directory.
pub fn prepare_script() -> Option<PathBuf> {
    find_tool("PENGUDROID_PREPARE_SCRIPT", PREPARE_SCRIPT)
}

fn find_tool(override_name: &str, relative: &str) -> Option<PathBuf> {
    if let Some(explicit) = non_empty(override_name) {
        return explicit.is_file().then_some(explicit);
    }
    let mut roots = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let bundled = dir.join("../share/pengudroid/setup").join(relative);
            if bundled.is_file() {
                return Some(bundled);
            }
            roots.extend(ancestors(dir).take(4));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.extend(ancestors(&cwd).take(4));
    }
    roots
        .into_iter()
        .map(|root| root.join(relative))
        .find(|script| script.is_file())
}

/// Locate an interpreter: `$PENGUDROID_PYTHON`, else `python3` on `PATH`.
fn python() -> String {
    non_empty("PENGUDROID_PYTHON")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "python3".to_string())
}

/// What a provisioning run needs to know, separated from the environment so it
/// can be built and asserted without touching process globals.
pub struct Provisioning {
    pub script: PathBuf,
    pub python: String,
    pub cache_root: PathBuf,
    pub download: bool,
}

impl Provisioning {
    /// Resolve everything a provisioning run needs, or explain what is missing.
    pub fn resolve(download: bool) -> Result<Provisioning, String> {
        let script = prepare_script().ok_or_else(|| {
            format!(
                "cannot find {PREPARE_SCRIPT}; run from a PenguDroid checkout or set \
                 PENGUDROID_PREPARE_SCRIPT"
            )
        })?;
        let cache_root = cache_root().ok_or_else(|| {
            "cannot resolve the cache directory (HOME and XDG_CACHE_HOME are unset); pass \
             --sysroot <dir> explicitly"
                .to_string()
        })?;
        Ok(Provisioning { script, python: python(), cache_root, download })
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.python);
        command
            .arg("-B")
            .arg(&self.script)
            .arg("--cache")
            .arg("--output")
            .arg(self.cache_root.join("runtime").join(RUNTIME_DIR));
        if self.download {
            command.arg("--download");
        }
        command
    }

    /// Run the extractor; the caller's terminal carries its progress output.
    pub fn run(&self) -> Result<PathBuf, String> {
        let status = self
            .command()
            .status()
            .map_err(|e| format!("cannot run {}: {e}", self.python))?;
        if !status.success() {
            return Err(format!(
                "runtime preparation failed ({status}); the extractor's message above says why"
            ));
        }
        let runtime = self.cache_root.join("runtime").join(RUNTIME_DIR);
        if !runtime.is_dir() {
            return Err(format!("preparation reported success but {} is missing", runtime.display()));
        }
        Ok(runtime)
    }
}

/// Make sure a runtime exists, preparing (and optionally downloading) one if
/// the cache is empty. Returns the tree to run against.
pub fn provision(download: bool) -> Result<PathBuf, String> {
    if let Some(store) = module_store() {
        return install_modules(&store);
    }
    if let Some(cached) = cached_runtime().filter(|p| p.is_dir()) {
        return Ok(cached);
    }
    Provisioning::resolve(download)?.run()
}

/// An explicitly configured or bundled offline module store.
pub(crate) fn module_store() -> Option<PathBuf> {
    // An explicit source is authoritative, including a missing/broken store.
    if let Some(store) = non_empty("PENGUDROID_RUNTIME_MODULES") {
        return Some(store);
    }
    let exe = std::env::current_exe().ok()?;
    let bundled = exe.parent()?.join("../share/pengudroid/runtime-modules");
    bundled.join("runtime-modules.json").is_file().then_some(bundled)
}

/// Install immutable generations and atomically select one for future launches.
fn install_modules(store: &Path) -> Result<PathBuf, String> {
    let script = find_tool("PENGUDROID_MODULES_SCRIPT", "tools/runtime-modules.py")
        .ok_or("cannot find tools/runtime-modules.py; run from a PenguDroid checkout or set PENGUDROID_MODULES_SCRIPT")?;
    let output = modular_runtime().ok_or("cannot resolve the runtime cache directory")?;
    if let Ok(metadata) = std::fs::symlink_metadata(&output) {
        if !metadata.file_type().is_symlink() {
            return Err(format!("active modular runtime must be a symlink: {}", output.display()));
        }
    }
    let source_manifest = std::fs::read(store.join("runtime-modules.json"))
        .map_err(|error| format!("cannot read module store {}: {error}", store.display()))?;
    if std::fs::read(output.join("pengudroid-modular-runtime.json")).ok().as_ref() == Some(&source_manifest)
        && output.join(pengudroid_runtime::DESCRIPTOR).is_file()
        && crate::sysroot::Layout::detect(&output).is_ok() {
        return std::fs::canonicalize(&output).map_err(|error| error.to_string());
    }
    let unique = format!("{}-{}", std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| error.to_string())?.as_nanos());
    let versions = output.parent().unwrap().join(MODULAR_VERSIONS);
    std::fs::create_dir_all(&versions).map_err(|error| error.to_string())?;
    let staging = versions.join(format!(".{unique}"));
    let generation = versions.join(&unique);
    let status = Command::new(python()).arg("-B").arg(script)
        .arg("assemble").arg(store).arg("--output").arg(&staging)
        .status().map_err(|error| format!("cannot assemble runtime modules: {error}"))?;
    if !status.success() {
        return Err(format!("runtime module assembly failed ({status})"));
    }
    // Verify the real boot classpath before this can become the active runtime.
    if let Err(error) = crate::sysroot::Layout::detect(&staging) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&staging, &generation) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!("cannot publish modular generation: {error}"));
    }
    let pending = output.with_file_name(format!(".{MODULAR_DIR}-{unique}"));
    let activate = || -> std::io::Result<()> {
        std::os::unix::fs::symlink(Path::new(MODULAR_VERSIONS).join(&unique), &pending)?;
        std::fs::rename(&pending, &output)
    };
    if let Err(error) = activate() {
        let _ = std::fs::remove_file(pending);
        let _ = std::fs::remove_dir_all(generation);
        return Err(format!("cannot activate modular runtime: {error}"));
    }
    // Running guests use canonical paths. Keep older generations so updates
    // cannot change their libraries/resources while they are executing.
    Ok(generation)
}

/// `pengudroid runtime [--status] [--download] [--modules STORE]`
pub fn runtime(args: &[String]) -> i32 {
    let mut download = false;
    let mut status_only = false;
    let mut modules = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--modules" => {
                let Some(store) = args.next().filter(|value| !value.starts_with('-')) else {
                    eprintln!("pengudroid runtime: --modules needs a STORE directory");
                    return 2;
                };
                modules = Some(PathBuf::from(store));
            }
            "--download" => download = true,
            "--status" | "--check" => status_only = true,
            other if other.starts_with('-') => {
                eprintln!("pengudroid runtime: unknown option {other}");
                return 2;
            }
            other => {
                eprintln!("pengudroid runtime: unexpected argument {other}");
                return 2;
            }
        }
    }
    if let Some(store) = modules {
        if download || status_only {
            eprintln!("pengudroid runtime: --modules cannot be combined with --download or --status");
            return 2;
        }
        return match install_modules(&store) {
            Ok(tree) => {
                println!("modular runtime: {}", tree.display());
                println!("apps now launch without the original system tree");
                0
            }
            Err(error) => {
                eprintln!("pengudroid runtime: {error}");
                1
            }
        };
    }
    let cached = cached_runtime();
    if let Some(tree) = locate() {
        println!("runtime: {}", tree.display());
        if let Some(cached) = cached.as_ref().filter(|cached| **cached != tree) {
            println!("cache:   {}", cached.display());
        }
        println!("apps run without --sysroot");
        return 0;
    }
    println!("runtime: none found");
    if let Some(cached) = &cached {
        println!("cache:   {} (not prepared)", cached.display());
    }
    if status_only {
        return 1;
    }
    match provision(download) {
        Ok(tree) => {
            println!("runtime: {}", tree.display());
            println!("apps now launch without --sysroot");
            0
        }
        Err(error) => {
            eprintln!("pengudroid runtime: {error}");
            if !download {
                eprintln!("hint: `pengudroid runtime --download` fetches the pinned image once");
            }
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_root_prefers_xdg_then_home() {
        // Pure path helpers, asserted through the same env lookup the process uses.
        let root = cache_root();
        if let Some(xdg) = non_empty("XDG_CACHE_HOME") {
            assert_eq!(root, Some(xdg.join(CACHE_NAME)));
        } else if let Some(home) = non_empty("HOME") {
            assert_eq!(root, Some(home.join(".cache").join(CACHE_NAME)));
        }
        if let Some(cached) = cached_runtime() {
            assert!(cached.ends_with(format!("runtime/{RUNTIME_DIR}")));
        }
    }

    #[test]
    fn provisioning_uses_the_cache_and_only_downloads_when_asked() {
        let plan = Provisioning {
            script: PathBuf::from("/checkout/tools/prepare-android17.py"),
            python: "python3".into(),
            cache_root: PathBuf::from("/cache/pengudroid"),
            download: false,
        };
        let args: Vec<String> = plan
            .command()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-B",
                "/checkout/tools/prepare-android17.py",
                "--cache",
                "--output",
                "/cache/pengudroid/runtime/android17-x86_64",
            ]
        );
        let download = Provisioning { download: true, ..plan };
        let args: Vec<String> = download
            .command()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.last().map(String::as_str), Some("--download"));
    }

    #[test]
    fn modules_reject_missing_store_and_conflicting_modes() {
        for args in [
            vec!["--modules"],
            vec!["--modules", "store", "--download"],
            vec!["--modules", "store", "--status"],
        ] {
            assert_eq!(runtime(&args.into_iter().map(String::from).collect::<Vec<_>>()), 2);
        }
    }

    #[test]
    fn ancestors_climb_out_of_the_binary_directory() {
        let seen: Vec<_> = ancestors(Path::new("/repo/target/debug")).map(|p| p.display().to_string()).collect();
        assert_eq!(seen, ["/repo/target/debug", "/repo/target", "/repo", "/"]);
    }
}
