#!/usr/bin/env python3
"""Bundle the native dependency closure, Python and offline extraction tools."""
import hashlib
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import sysconfig


def package(appdir, repo):
    usr = appdir / "usr"
    libs = usr / "portable/lib"
    libs.mkdir(parents=True, exist_ok=True)
    tools = usr / "tools"
    tools.mkdir(exist_ok=True)
    python = usr / "python"
    (python / "bin").mkdir(parents=True, exist_ok=True)
    shutil.copy2(sys.executable, python / "bin/python3")
    stdlib = Path(sysconfig.get_path("stdlib"))
    shutil.copytree(stdlib, python / "lib" / stdlib.name,
                    ignore=shutil.ignore_patterns("__pycache__", "site-packages", "dist-packages", "test", "tests"), dirs_exist_ok=True)
    erofs = repo / ".toolchain/host-tools/usr/sbin/fsck.erofs.real"
    if not erofs.is_file():
        erofs = Path(shutil.which("fsck.erofs") or "/missing/fsck.erofs")
    for name, source in [("debugfs", Path(shutil.which("debugfs") or "/usr/sbin/debugfs")),
                         ("fsck.erofs", erofs), ("pw-cat", Path(shutil.which("pw-cat") or "/missing/pw-cat")),
                         ("sh", Path("/bin/dash")), ("unzip", Path("/usr/bin/unzip")), ("zenity", Path("/usr/bin/zenity"))]:
        if not source.is_file() or source.read_bytes()[:4] != b"\x7fELF":
            raise RuntimeError(f"missing real ELF tool: {source}")
        shutil.copy2(source, tools / name)
    # GLVND is driver-neutral; vendor GPU drivers stay on the host.
    dynamic = ["libwayland-client.so.0", "libwayland-egl.so.1", "libxkbcommon.so.0",
               "libxkbcommon-x11.so.0", "libEGL.so.1", "libGLESv2.so.2",
               "libGLdispatch.so.0", "libgbm.so.1", "libstdc++.so.6"]
    cache = subprocess.check_output(["/sbin/ldconfig", "-p"], text=True)
    for name in dynamic:
        match = re.search(r"^\s*" + re.escape(name) + r"\s+\([^\n]*x86-64[^\n]*\) => (\S+)", cache, re.M)
        if not match:
            raise RuntimeError(f"missing host build input: {name}")
        shutil.copy2(match[1], libs / name)
    unversioned_xkb_x11 = libs / "libxkbcommon-x11.so"
    if not unversioned_xkb_x11.exists():
        unversioned_xkb_x11.symlink_to("libxkbcommon-x11.so.0")
    for directory in [Path("/usr/lib/x86_64-linux-gnu/pipewire-0.3"), Path("/usr/lib/x86_64-linux-gnu/spa-0.2")]:
        if directory.is_dir():
            shutil.copytree(directory, usr / "portable" / directory.name, dirs_exist_ok=True)
    schemas = usr / "share/glib-2.0/schemas"
    schemas.mkdir(parents=True, exist_ok=True)
    shutil.copy2("/usr/share/glib-2.0/schemas/gschemas.compiled", schemas)
    fonts = usr / "share/fonts"
    fonts.mkdir(parents=True, exist_ok=True)
    shutil.copy2("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", fonts)
    (usr / "portable/fonts.conf").write_text('''<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
<fontconfig><dir prefix="relative">../share/fonts</dir>
<include ignore_missing="yes">/etc/fonts/fonts.conf</include></fontconfig>
''')
    def is_elf(path):
        if not path.is_file():
            return False
        with path.open("rb") as stream:
            return stream.read(4) == b"\x7fELF"
    queue = [p for p in usr.rglob("*") if is_elf(p)]
    seen = set()
    while queue:
        path = queue.pop()
        if path in seen:
            continue
        seen.add(path)
        result = subprocess.run(["ldd", str(path)], capture_output=True, text=True)
        for line in result.stdout.splitlines():
            match = re.search(r"=> (/[^:\s]+)\s+\(", line) or re.match(r"\s*(/[^:\s]+)\s+\(", line)
            if not match:
                if "not found" in line:
                    # ARM64 bridge artifacts retain Android version labels;
                    # they are adapted by the existing per-run loader.
                    if "/bridge/" not in str(path) and "/guest-arm64/" not in str(path):
                        raise RuntimeError(f"unresolved build dependency: {path}: {line}")
                continue
            source = Path(match[1])
            target = libs / source.name
            if not target.exists():
                shutil.copy2(source, target)
                queue.append(target)
    notices = usr / "share/pengudroid/licenses"
    notices.mkdir(parents=True, exist_ok=True)
    shutil.copy2(repo / "LICENSE", notices / "PenguDroid-Apache-2.0.txt")
    shutil.copy2(repo / "tools/licenses/uruntime-MIT.txt", notices / "uruntime-MIT.txt")
    # Retain installed distribution copyright/license notices with the closure.
    for path in Path("/usr/share/doc").glob("*/copyright"):
        if path.is_file():
            shutil.copy2(path, notices / (path.parent.name + ".copyright"))
    loader = next(libs.glob("ld-linux-x86-64.so.*"))
    shutil.copy2(loader, usr / "portable/loader")
    cert = Path("/etc/ssl/certs/ca-certificates.crt")
    shutil.copy2(cert, usr / "portable/ca-certificates.crt")
    setup = usr / "share/pengudroid/setup/tools"
    (setup / "art").mkdir(parents=True, exist_ok=True)
    for name in ["prepare-android17.py", "runtime-modules.py", "art/unpack-apex.py", "art/sysroot-relink.py"]:
        shutil.copy2(repo / "tools" / name, setup / name)
    shutil.copy2(repo / "tools/appimage-bootstrap.py", usr / "share/pengudroid/bootstrap.py")
    digest = hashlib.sha256()
    digest.update((appdir / "AppRun").read_bytes())
    for path in sorted(p for p in usr.rglob("*") if p.is_file()):
        digest.update(str(path.relative_to(usr)).encode())
        with path.open("rb") as stream:
            while chunk := stream.read(1024 * 1024):
                digest.update(chunk)
    (appdir / "build-id").write_text(digest.hexdigest() + "\n")
    print(f"portable dependency closure: {len(list(libs.iterdir()))} libraries")


if __name__ == "__main__":
    package(Path(sys.argv[1]).resolve(), Path(__file__).resolve().parent.parent)
