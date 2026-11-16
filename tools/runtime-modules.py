#!/usr/bin/env python3
"""Package API-37 runtime modules and assemble a source-independent runtime.

No image download or mounts are needed. The desktop profile retains the hosted
ART/framework payload and its ELF dependencies, excluding the Android OS apps.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import posixpath
import shutil
import struct
import tarfile
import tempfile

FORMAT = 2
TARGET = "android17-x86_64"


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def relative(name):
    path = PurePosixPath(name)
    if not name or path.is_absolute() or any(p in ("", ".", "..") for p in name.split("/")):
        raise ValueError(f"unsafe runtime path: {name!r}")
    return path


def link_target(name, target):
    if not target or target.startswith("/"):
        raise ValueError(f"non-relative runtime link: {name} -> {target}")
    resolved = posixpath.normpath(posixpath.join(posixpath.dirname(name), target))
    relative(resolved)


def identity(root):
    props = (root / "system/build.prop").read_text()
    sdk = [line.split("=", 1)[1].strip() for line in props.splitlines()
           if line.split("=", 1)[0].strip() == "ro.build.version.sdk" and "=" in line]
    if sdk != ["37"]:
        raise ValueError("runtime modules require Android 17 / API 37")
    with (root / "system/apex/com.android.art/lib64/libart.so").open("rb") as stream:
        header = stream.read(20)
    if len(header) != 20 or header[:6] != b"\x7fELF\x02\x01" or header[18:20] != b"\x3e\x00":
        raise ValueError("runtime modules require x86_64 ART")


def module_for(name):
    parts = PurePosixPath(name).parts
    if len(parts) > 2 and parts[1] == "apex":
        return "apex-" + parts[2]
    if len(parts) > 1 and parts[1] in ("framework", "lib64", "fonts", "etc"):
        return {"lib64": "native", "etc": "config"}.get(parts[1], parts[1])
    return "platform"


# Runtime directories loaded by Layout::android, in the same precedence order.
def library_dirs(source):
    apex = source / "system/apex"
    return [source / "system/lib64", apex / "com.android.art/lib64"] + [
        p / "lib64" for p in sorted(apex.iterdir())
        if p.is_dir() and p.name != "com.android.art" and (p / "lib64").is_dir()
    ]


def elf_needed(path):
    """Read DT_NEEDED from ELF64 program headers; section tables are unnecessary."""
    with path.open("rb") as stream:
        size = os.fstat(stream.fileno()).st_size

        def read_at(offset, length):
            if offset < 0 or length < 0 or offset + length > size:
                raise ValueError(f"truncated ELF: {path.name}")
            stream.seek(offset)
            data = stream.read(length)
            if len(data) != length:
                raise ValueError(f"truncated ELF: {path.name}")
            return data

        header = stream.read(64)
        if header[:4] != b"\x7fELF":
            return None
        if len(header) != 64 or header[4:6] != b"\x02\x01" or header[18:20] != b"\x3e\x00":
            raise ValueError(f"desktop runtime requires ELF64 x86_64: {path.name}")
        phoff = struct.unpack_from("<Q", header, 32)[0]
        phsize, phnum = struct.unpack_from("<HH", header, 54)
        if phnum > 4096 or phnum and phsize != 56:
            raise ValueError(f"invalid ELF program headers: {path.name}")
        loads, dynamic = [], None
        for index in range(phnum):
            kind, _, offset, address, _, filesz, _, _ = struct.unpack("<II6Q", read_at(phoff + index * phsize, 56))
            if kind == 1:
                loads.append((address, offset, filesz))
            elif kind == 2:
                dynamic = offset, filesz
        if dynamic is None:
            return []
        offset, length = dynamic
        if length % 16 or length > 16 * 1024 * 1024:
            raise ValueError(f"invalid ELF dynamic table: {path.name}")
        tags, terminated = {}, False
        for tag, value in struct.iter_unpack("<qQ", read_at(offset, length)):
            if tag == 0:
                terminated = True
                break
            tags.setdefault(tag, []).append(value)
        if not terminated:
            raise ValueError(f"unterminated ELF dynamic table: {path.name}")
        if 1 not in tags:
            return []
        if len(tags.get(5, [])) != 1 or len(tags.get(10, [])) != 1:
            raise ValueError(f"missing ELF string table: {path.name}")
        address, length = tags[5][0], tags[10][0]
        if length > 64 * 1024 * 1024:
            raise ValueError(f"oversized ELF string table: {path.name}")
        mappings = [offset + address - base for base, offset, filesz in loads
                    if base <= address and address + length <= base + filesz]
        if len(mappings) != 1:
            raise ValueError(f"unmapped ELF string table: {path.name}")
        strings = read_at(mappings[0], length)
        needed = []
        for index in tags[1]:
            end = strings.find(b"\0", index)
            if index >= len(strings) or end < 0:
                raise ValueError(f"invalid ELF dependency string: {path.name}")
            name = strings[index:end].decode("utf-8")
            if len(relative(name).parts) != 1:
                raise ValueError(f"invalid ELF dependency name: {name}")
            needed.append(name)
        return needed


def inventory(source):
    paths = {}
    for directory, dirs, files in os.walk(source / "system", followlinks=False):
        for entry in sorted(dirs + files):
            path = Path(directory) / entry
            paths[path.relative_to(source).as_posix()] = path
    return paths


def desktop_paths(source, paths):
    """Conservative dlopen coverage plus native dependency closure.

    Keep all top-level public/private platform libraries because APKs and JNI
    registration load them dynamically. APEX modules enter through boot
    classpath fragments, visible native sonames and their DT_NEEDED edges.
    """
    apexes = {"com.android.art", "com.android.runtime", "com.android.i18n",
              "com.android.conscrypt", "com.android.tzdata",
              # Loaded by ApexCodecsLazy via dlopen, absent from DT_NEEDED.
              "com.android.media", "com.android.media.swcodec"}
    apexes.update(p.parent.parent.parent.name for p in
                  (source / "system/apex").glob("*/etc/classpaths/bootclasspath.pb"))
    selected = set()
    for name, path in paths.items():
        parts = PurePosixPath(name).parts
        if (name == "system/build.prop" or parts[:2] in (("system", "etc"), ("system", "fonts"))
                or parts[:3] in (("system", "system_ext", "etc"), ("system", "product", "etc"))):
            selected.add(name)
        elif parts[:2] == ("system", "framework"):
            # Keep jars and native x86_64 boot images; discard 32-bit/OAT caches.
            framework_file = len(parts) == 3 and (
                path.suffix in (".jar", ".vdex") or path.name == "framework-res.apk")
            if len(parts) == 2 or framework_file or parts[2:3] == ("x86_64",):
                selected.add(name)
        elif parts[:2] == ("system", "lib64") and len(parts) == 3 and ".so" in path.name:
            selected.add(name)
    providers = {}
    for directory in library_dirs(source):
        for path in sorted(directory.iterdir()) if directory.is_dir() else []:
            if ".so" in path.name and path.is_file():
                providers.setdefault(path.name, path.relative_to(source).as_posix())
    # APKs may dlopen any platform soname, including those exported only by an
    # APEX (e.g. libneuralnetworks.so). Seed the loader's whole visible surface;
    # DT_NEEDED alone cannot express these API contracts.
    for name in providers.values():
        selected.add(name)
        parts = PurePosixPath(name).parts
        if parts[:2] == ("system", "apex"):
            apexes.add(parts[2])
    scanned, dependencies, link_dependencies = set(), {}, {}
    while True:
        before = len(selected)
        for name in paths:
            parts = PurePosixPath(name).parts
            if len(parts) >= 3 and parts[:2] == ("system", "apex") and parts[2] in apexes:
                # No APEX apps, 32-bit libs, precompiled oat caches or executables.
                if len(parts) == 3:
                    selected.add(name)
                    continue
                payload = parts[3] in ("lib64", "etc", "javalib") and "oat" not in parts[4:]
                metadata = len(parts) == 4 and parts[3] in (
                    "apex_manifest.pb", "apex_manifest.json", "NOTICE", "NOTICE.html.gz")
                if payload or metadata:
                    selected.add(name)
        # Normalize guest-absolute links without ever following host /apex.
        for name in sorted(selected):
            path = paths[name]
            if path.is_symlink():
                target = os.readlink(path)
                if target.startswith("/"):
                    target = target.lstrip("/")
                else:
                    target = posixpath.normpath(posixpath.join(posixpath.dirname(name), target))
                if target.startswith("apex/"):
                    target = "system/" + target
                relative(target)
                if target in paths:
                    selected.add(target)
                    link_dependencies.setdefault(name, set()).add(target)
            elif path.is_file() and ".so" in path.name and name not in scanned:
                scanned.add(name)
                needed = elf_needed(path)
                if needed is not None:
                    for dependency in needed:
                        provider = providers.get(dependency)
                        if provider is None:
                            raise ValueError(f"missing ELF dependency {dependency} required by {name}")
                        dependencies.setdefault(name, set()).add(provider)
                        selected.add(provider)
                        parts = PurePosixPath(provider).parts
                        if parts[:2] == ("system", "apex"):
                            apexes.add(parts[2])
        if len(selected) == before:
            break
    # Include every parent explicitly; archives never depend on extraction order.
    for name in list(selected):
        selected.update(str(parent) for parent in PurePosixPath(name).parents
                        if str(parent) in paths)
    return {name: paths[name] for name in sorted(selected)}, dependencies, link_dependencies


def validate_manifest(manifest):
    if manifest.get("format") not in (1, FORMAT) or manifest.get("target") != TARGET:
        raise ValueError("unsupported runtime module format or target")
    if not isinstance(manifest.get("modules"), list) or not manifest["modules"]:
        raise ValueError("runtime module list is empty")
    if manifest.get("format") == FORMAT and manifest.get("profile") not in ("desktop", "full"):
        raise ValueError("unsupported runtime module profile")
    names = [module["name"] for module in manifest["modules"]]
    if len(names) != len(set(names)):
        raise ValueError("duplicate runtime module name")
    available = set(names)
    if not isinstance(manifest.get("arm64_guest", False), bool):
        raise ValueError("invalid ARM64 provider capability")
    if manifest.get("arm64_guest", False) != ("arm64-guest" in available):
        raise ValueError("ARM64 provider capability does not match module list")
    for module in manifest["modules"]:
        for dependency in module.get("requires", []):
            if dependency not in available:
                raise ValueError(f"missing required module {dependency} for {module['name']}")
    for name in manifest.get("roots", []):
        if name not in available:
            raise ValueError(f"missing runtime root module: {name}")


def publish(destination, build):
    destination = Path(destination).absolute()
    if os.path.lexists(destination):
        raise ValueError(f"destination already exists: {destination}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".runtime-modules-", dir=destination.parent) as work:
        stage = Path(work) / "payload"
        stage.mkdir()
        build(stage)
        # No existing runtime is modified if verification or extraction fails.
        if os.path.lexists(destination):
            raise ValueError(f"destination already exists: {destination}")
        stage.rename(destination)
    return destination


def pack(source, destination, profile="desktop"):
    source = Path(source).resolve(strict=True)
    identity(source)
    if Path(destination).resolve().is_relative_to(source):
        raise ValueError("module store must be outside the source runtime")

    if profile not in ("full", "desktop"):
        raise ValueError(f"unknown runtime profile: {profile}")
    paths = inventory(source)
    source_bytes = sum(path.stat().st_size for path in paths.values() if not path.is_symlink() and path.is_file())
    dependencies, link_dependencies = {}, {}
    if profile == "desktop":
        paths, dependencies, link_dependencies = desktop_paths(source, paths)

    def build(stage):
        groups = {}
        for name, path in paths.items():
            groups.setdefault(module_for(name), []).append((name, path))
        modules = []
        for module, module_paths in sorted(groups.items()):
            archive = stage / f"{module}.tar"
            with tarfile.open(archive, "w", format=tarfile.PAX_FORMAT) as output:
                for name, path in sorted(module_paths):
                    info = output.gettarinfo(str(path), arcname=name)
                    info.uid = info.gid = info.mtime = 0
                    info.uname = info.gname = ""
                    info.mode &= 0o777
                    if info.issym():
                        # Interpret Android absolute links inside the guest root.
                        if info.linkname.startswith("/"):
                            info.linkname = posixpath.relpath(info.linkname.lstrip("/"), posixpath.dirname(name))
                        link_target(name, info.linkname)
                    elif not (info.isdir() or info.isfile()):
                        raise ValueError(f"unsupported runtime entry: {name}")
                    if info.isfile():
                        with path.open("rb") as stream:
                            output.addfile(info, stream)
                    else:
                        output.addfile(info)
            requires = {module_for(provider) for name, _ in module_paths
                        for provider in dependencies.get(name, set()) | link_dependencies.get(name, set())} - {module}
            modules.append({"name": module, "archive": archive.name,
                            "sha256": digest(archive), "bytes": archive.stat().st_size,
                            "requires": sorted(requires)})
        manifest = {"format": FORMAT, "target": TARGET, "profile": profile,
                    "roots": sorted(groups), "modules": modules,
                    "source_bytes": source_bytes,
                    "payload_bytes": sum(path.stat().st_size for path in paths.values()
                                         if not path.is_symlink() and path.is_file()),
                    "elf_edges": sum(len(edges) for edges in dependencies.values()),
                    "native_dependencies": {name: sorted(edges) for name, edges in sorted(dependencies.items())},
                    "symlink_dependencies": {name: sorted(edges) for name, edges in sorted(link_dependencies.items())}}
        (stage / "runtime-modules.json").write_text(json.dumps(manifest, indent=2) + "\n")
    return publish(destination, build)


def arm64_name(name):
    return (name == "system/etc/ld.config.arm64.txt" or name == "system/etc/pengudroid-arm64-bundle.json"
            or name == "system/bin/arm64" or name.startswith("system/bin/arm64/")
            or name == "system/lib64/arm64" or name.startswith("system/lib64/arm64/"))


def component_path(name, arm64=False):
    """Map transport archive paths to component payloads, never to a sysroot."""
    relative(name)
    if name == "apex" or name == "system/apex":
        return "components"
    if name.startswith("apex/"):
        name = "system/" + name
    if arm64:
        if name == "system/etc/pengudroid-arm64-bundle.json":
            return "components/arm64/bundle.json"
        if name == "system/etc/ld.config.arm64.txt":
            return "components/arm64/ld.config.arm64.txt"
        for prefix, host in (("system/bin/arm64", "components/arm64/bin"),
                             ("system/lib64/arm64", "components/arm64/lib")):
            if name == prefix or name.startswith(prefix + "/"):
                return host + name[len(prefix):]
    if name.startswith("system/apex/"):
        return "components/" + name[len("system/apex/"):]
    for prefix, host in (("system/lib64", "components/native"),
                         ("system/framework", "components/framework"),
                         ("system/etc", "components/config"), ("system/fonts", "components/fonts")):
        if name == prefix or name.startswith(prefix + "/"):
            return host + name[len(prefix):]
    if name == "system" or name.startswith("system/"):
        return "components/platform" + name[len("system"):]
    # Non-system links may intentionally point at absent optional partitions.
    return "state/" + name


def write_component_descriptor(stage, manifest):
    properties = {}
    props = stage / "components/platform/build.prop"
    for line in props.read_text().splitlines():
        if "=" not in line or line.lstrip().startswith("#"):
            continue
        key, value = map(str.strip, line.split("=", 1))
        if key.startswith("ro.build.version."):
            if key in properties:
                raise ValueError(f"duplicate runtime property: {key}")
            properties[key] = value
    if properties.get("ro.build.version.sdk") != "37":
        raise ValueError("component runtime requires API 37")
    art = stage / "components/com.android.art/lib64/libart.so"
    with art.open("rb") as stream:
        header = stream.read(20)
    if len(header) != 20 or header[:6] != b"\x7fELF\x02\x01" or header[18:20] != b"\x3e\x00":
        raise ValueError("component runtime requires x86_64 ART")
    components = {}
    for module in manifest["modules"]:
        name = module["name"]
        directory = "arm64" if name == "arm64-guest" else name.removeprefix("apex-")
        components[name] = "components/" + directory
        (stage / components[name]).mkdir(parents=True, exist_ok=True)
    mounts = {
        "/system": "components/platform", "/apex": "components", "/system/apex": "components",
        "/system/lib64": "components/native", "/system/framework": "components/framework",
        "/system/etc": "components/config", "/system/fonts": "components/fonts",
        "/product": "components/platform/product", "/system/product": "components/platform/product",
        "/system_ext": "components/platform/system_ext", "/system/system_ext": "components/platform/system_ext",
    }
    for name in components:
        if name.startswith("apex-"):
            apex = name.removeprefix("apex-")
            mounts["/apex/" + apex] = components[name]
            mounts["/system/apex/" + apex] = components[name]
    for name in ("data", "vendor", "odm", "mnt", "linkerconfig", "vendor_dlkm", "odm_dlkm"):
        mounts["/" + name] = "state/" + name
    for target in mounts.values():
        (stage / target).mkdir(parents=True, exist_ok=True)
    if manifest.get("arm64_guest"):
        mounts.update({"/system/bin/arm64": "components/arm64/bin",
                       "/system/lib64/arm64": "components/arm64/lib",
                       "/system/etc/ld.config.arm64.txt": "components/arm64/ld.config.arm64.txt"})
    descriptor = {"schema": 1, "target": TARGET, "components": components,
                  "properties": properties, "mounts": [{"guest": guest, "host": host}
                    for guest, host in sorted(mounts.items())]}
    (stage / "pengudroid-runtime.json").write_text(json.dumps(descriptor, indent=2) + "\n")
    # Runtime identity comes from its descriptor, not a GSI build.prop file.
    for legacy_props in (stage / "components").rglob("build.prop"):
        legacy_props.unlink(missing_ok=True)


def add_guest(store, guest_root, destination):
    store, guest_root = Path(store).resolve(strict=True), Path(guest_root).resolve(strict=True)
    manifest = json.loads((store / "runtime-modules.json").read_bytes())
    validate_manifest(manifest)
    if manifest.get("arm64_guest"):
        raise ValueError("module store already contains an ARM64 guest")
    if (guest_root / "pengudroid-arm64-guest.json").exists():
        raise ValueError("stock/staged-only ARM64 trees are not executable guest providers")
    provenance = guest_root / "pengudroid-arm64-bridge-bundle.json"
    if not provenance.is_file():
        raise ValueError("ARM64 provider requires a built native-bridge bundle manifest")

    def build(stage):
        for module in manifest["modules"]:
            archive = relative(module["archive"])
            if len(archive.parts) != 1:
                raise ValueError("invalid module archive")
            shutil.copy2(store / archive, stage / archive)
            if digest(stage / archive) != module["sha256"]:
                raise ValueError(f"module checksum mismatch: {archive}")
        archive = stage / "arm64-guest.tar"
        with tarfile.open(archive, "w", format=tarfile.PAX_FORMAT) as output:
            for prefix in ("system/bin/arm64", "system/lib64/arm64"):
                directory = guest_root / prefix
                if not directory.is_dir():
                    raise ValueError(f"missing ARM64 provider directory: {prefix}")
                for path in sorted(directory.rglob("*")):
                    name = path.relative_to(guest_root).as_posix()
                    if path.is_file():
                        # Materialize guest links; no build-tree dependency survives.
                        info = output.gettarinfo(str(path.resolve()), arcname=name)
                        info.uid = info.gid = info.mtime = 0
                        info.uname = info.gname = ""
                        info.mode &= 0o777
                        with path.open("rb") as stream:
                            output.addfile(info, stream)
            for source, name in ((guest_root / "system/etc/ld.config.arm64.txt", "system/etc/ld.config.arm64.txt"),
                                 (provenance, "system/etc/pengudroid-arm64-bundle.json")):
                info = output.gettarinfo(str(source), arcname=name)
                info.uid = info.gid = info.mtime = 0
                info.uname = info.gname = ""
                with source.open("rb") as stream:
                    output.addfile(info, stream)
        manifest["arm64_guest"] = True
        manifest["modules"].append({"name": "arm64-guest", "archive": archive.name,
            "sha256": digest(archive), "bytes": archive.stat().st_size,
            "requires": ["native", "apex-com.android.art"]})
        manifest.setdefault("roots", []).append("arm64-guest")
        (stage / "runtime-modules.json").write_text(json.dumps(manifest, indent=2) + "\n")
    return publish(destination, build)


def assemble(store, destination, layout="components"):
    store = Path(store).resolve(strict=True)
    manifest_bytes = (store / "runtime-modules.json").read_bytes()
    manifest = json.loads(manifest_bytes)
    validate_manifest(manifest)
    if layout not in ("components", "legacy"):
        raise ValueError(f"unsupported runtime layout: {layout}")
    flat = layout == "components"
    arm64 = manifest.get("arm64_guest", False)

    def build(stage):
        entries = {}
        archives = set()
        links = []
        for module in manifest["modules"]:
            archive_name = module["archive"]
            if len(relative(archive_name).parts) != 1 or archive_name in archives:
                raise ValueError(f"invalid or duplicate module archive: {archive_name}")
            archives.add(archive_name)
            archive = store / archive_name
            # Pin the open file through verification and extraction.
            with archive.open("rb") as stream:
                if os.fstat(stream.fileno()).st_size != module["bytes"] or hashlib.file_digest(stream, "sha256").hexdigest() != module["sha256"]:
                    raise ValueError(f"module checksum mismatch: {archive_name}")
                stream.seek(0)
                with tarfile.open(fileobj=stream, mode="r:") as payload:
                    for member in payload:
                        name = member.name
                        path = relative(name)
                        if path.parts[0] != "system":
                            raise ValueError(f"module entry outside system: {name}")
                        if arm64 and module["name"] != "arm64-guest" and arm64_name(name):
                            continue
                        if name in entries:
                            raise ValueError(f"module path collision: {name}")
                        if not (member.isdir() or member.isfile() or member.issym()):
                            raise ValueError(f"unsupported module entry: {name}")
                        entries[name] = "dir" if member.isdir() else "leaf"
                        output = stage / (component_path(name, arm64) if flat else name)
                        # Links are created only after all ordinary files.
                        output.parent.mkdir(parents=True, exist_ok=True)
                        if member.isdir():
                            output.mkdir(exist_ok=True)
                        elif member.issym():
                            link_target(name, member.linkname)
                            target = member.linkname
                            if flat:
                                virtual = posixpath.normpath(posixpath.join(posixpath.dirname(name), target))
                                if virtual.startswith("apex/"):
                                    virtual = "system/" + virtual
                                target = posixpath.relpath(stage / component_path(virtual, arm64), output.parent)
                            links.append((output, target))
                        else:
                            with payload.extractfile(member) as src, output.open("xb") as dst:
                                shutil.copyfileobj(src, dst)
                            output.chmod(member.mode & 0o777)
        # Also reject aliases used as parents, including across archives.
        for name in entries:
            for parent in PurePosixPath(name).parents:
                if entries.get(str(parent)) == "leaf":
                    raise ValueError(f"module entry has a non-directory parent: {name}")
        for output, target in links:
            output.symlink_to(target)
        if not flat:
            for name, target in {"apex": "system/apex", "bin": "system/bin", "etc": "system/etc",
                                 "product": "system/product", "system_ext": "system/system_ext"}.items():
                (stage / name).symlink_to(target)
        for output, _ in links:
            try:
                confined = output.resolve().is_relative_to(stage)
            except RuntimeError as error:
                raise ValueError(f"cyclic runtime link: {output.relative_to(stage)}") from error
            if not confined:
                raise ValueError(f"runtime link escapes root: {output.relative_to(stage)}")
        if flat:
            write_component_descriptor(stage, manifest)
        else:
            identity(stage)
        (stage / "pengudroid-modular-runtime.json").write_bytes(manifest_bytes)
    return publish(destination, build)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    for action in ("pack", "assemble"):
        command = sub.add_parser(action)
        command.add_argument("source", type=Path)
        command.add_argument("--output", type=Path, required=True)
        if action == "assemble":
            command.add_argument("--layout", choices=("components", "legacy"), default="components")
        if action == "pack":
            command.add_argument("--profile", choices=("full", "desktop"), default="desktop")
    command = sub.add_parser("add-guest")
    command.add_argument("source", type=Path)
    command.add_argument("--guest-root", type=Path, required=True)
    command.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.action == "pack":
            print(pack(args.source, args.output, args.profile))
        elif args.action == "add-guest":
            print(add_guest(args.source, args.guest_root, args.output))
        else:
            print(assemble(args.source, args.output, args.layout))
    except (OSError, ValueError, KeyError, TypeError, tarfile.TarError) as error:
        parser.exit(1, f"runtime modules: {error}\n")


if __name__ == "__main__":
    main()
