#!/usr/bin/env python3
"""Derive a flat, native-only ARM64 bundle without ART/framework payloads.

The input is an already verified native-bridge guest bundle. This does not build
or invent native-bridge linker/libc replacements. Only named native ABI roots
and their DT_NEEDED closure are copied, never the source's entire Android tree.
"""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import tempfile

spec = importlib.util.spec_from_file_location("runtime_modules_for_pengu", Path(__file__).with_name("runtime-modules.py"))
modules = importlib.util.module_from_spec(spec)
spec.loader.exec_module(modules)
ROOT_LIB = Path("system/lib64/arm64")
SEEDS = ["libc.so", "libm.so", "libdl.so", "libdl_android.so", "ld-android.so",
    "libnative_bridge_vdso.so", "libc++.so", "liblog.so", "libz.so", "libandroid.so",
    "libEGL.so", "libGLESv1_CM.so", "libGLESv2.so", "libGLESv3.so", "libvulkan.so",
    "libOpenSLES.so", "libaaudio.so"]
FORBIDDEN_PREFIXES = ("libart", "libdex", "libprofile", "libandroid_runtime", "libandroidfw",
    "libhwui", "libgui", "libui.", "libbinder", "libjavacore", "libopenjdk", "libnativehelper",
    "libnativeloader", "libandroid_servers", "libstagefright", "libmedia", "libcamera")

def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream,"sha256").hexdigest()

def safe_file(source, relative):
    path = source / relative
    for parent in [path, *path.parents]:
        if parent == source:
            break
        if parent.is_symlink():
            raise ValueError(f"native input must not traverse a symlink: {relative}")
    if not path.is_file():
        raise ValueError(f"missing native ABI input: {relative}")
    return path

def native_paths(source, needed=None):
    if needed is None:
        needed = lambda path: modules.elf_needed(path, machine=183)
    files = {Path("system/bin/arm64/linker64"), Path("system/bin/arm64/app_process64")}
    pending = list(SEEDS)
    for helper in files:
        dependencies = needed(safe_file(source, helper))
        if dependencies is None:
            raise ValueError(f"native helper is not an ELF: {helper}")
        pending.extend(dependencies)
    if (source / ROOT_LIB / "libstdc++.so").is_file():
        pending.append("libstdc++.so")
    seen = set()
    while pending:
        name = pending.pop()
        if name == "linux-vdso.so.1":
            continue  # supplied by the translator; its guest stub is a seed
        if name in seen:
            continue
        if "/" in name or name.startswith(FORBIDDEN_PREFIXES):
            raise ValueError(f"native-only closure requires forbidden framework dependency: {name}")
        seen.add(name)
        if len(seen) > 512:
            raise ValueError("native dependency closure exceeds 512 libraries")
        relative = ROOT_LIB / name
        if not (source / relative).exists():
            relative = ROOT_LIB / "bootstrap" / name
        path = safe_file(source, relative)
        dependencies = needed(path)
        if dependencies is None:
            raise ValueError(f"native library is not an ELF: {relative}")
        files.add(relative)
        pending.extend(dependencies)
    for path in list(files):
        safe_file(source, path)
        dependencies = needed(source / path)
        if dependencies is None:
            raise ValueError(f"native input is not an ELF: {path}")
    files.add(Path("system/etc/ld.config.arm64.txt"))
    safe_file(source, Path("system/etc/ld.config.arm64.txt"))
    return sorted(files)

def build(source, destination):
    source = source.resolve(strict=True)
    descriptor = safe_file(source, Path("pengudroid-arm64-bridge-bundle.json"))
    provenance = json.loads(descriptor.read_text())
    if destination.exists() and (not destination.is_dir() or any(destination.iterdir())):
        raise ValueError("native-only output must be a new or empty directory")
    selected = native_paths(source)
    destination.parent.mkdir(parents=True,exist_ok=True)
    stage = Path(tempfile.mkdtemp(prefix=".pengu-native-",dir=destination.parent))
    try:
        files = {}
        for relative in selected:
            original = source / relative
            known = provenance.get("files",{}).get(relative.as_posix(),{}).get("sha256")
            if known and digest(original) != known:
                raise ValueError(f"source native-bridge file changed: {relative}")
            if relative.parent == Path("system/bin/arm64"):
                target = Path("native/bin") / relative.name
            elif relative.suffix == ".txt":
                target = Path("native") / relative.name
            else:
                target = Path("native/lib64") / relative.name
            if (stage / target).exists():
                raise ValueError(f"duplicate native soname: {target.name}")
            (stage / target).parent.mkdir(parents=True,exist_ok=True)
            shutil.copy2(original,stage / target)
            files[target.as_posix()]={"sha256":digest(stage / target),"bytes":(stage/target).stat().st_size}
        runtime = {"schema":1,"target":"android17-x86_64","profile":"pengu",
            "components":{"arm64-guest":"native"},
            "properties":{"ro.build.version.sdk":"37","ro.build.version.release":"17"},
            "mounts":[{"guest":"/system/bin/arm64","host":"native/bin"},
                {"guest":"/system/lib64/arm64","host":"native/lib64"},
                {"guest":"/system/etc/ld.config.arm64.txt","host":"native/ld.config.arm64.txt"}]}
        (stage/"pengudroid-runtime.json").write_text(json.dumps(runtime,indent=2)+"\n")
        (stage/"pengudroid-arm64-bridge-bundle.json").write_text(json.dumps({
            "schema":1,"profile":"pengu","tool":"PenguDroid native-only ARM64 subset",
            "source_bundle_sha256":digest(descriptor),"files":files,
            "note":"Native linker/libc/proxy ABI support only; no ART/framework or Android system tree."},indent=2)+"\n")
        if destination.exists():
            destination.rmdir()
        # mkdtemp starts private; the published bundle must be readable by any
        # user extracting the AppImage, not just its builder.
        stage.chmod(0o755)
        stage.rename(destination)
    finally:
        if stage.exists():
            shutil.rmtree(stage)
    return len(selected)

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source",type=Path)
    parser.add_argument("destination",type=Path)
    args=parser.parse_args()
    print(f"native-only ARM64 bundle: {build(args.source,args.destination)} files -> {args.destination}")

if __name__=="__main__":main()
