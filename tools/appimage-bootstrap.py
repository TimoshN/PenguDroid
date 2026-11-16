#!/usr/bin/env python3
"""Materialize a relocatable, persistent runtime using its bundled loader."""
import fcntl
import os
from pathlib import Path
import shutil
import struct
import sys
import tempfile


def patch_interpreter(path, loader):
    with path.open("rb") as stream:
        header = stream.read(64)
    if len(header) < 64 or header[:6] != b"\x7fELF\x02\x01" or struct.unpack_from("<H", header, 18)[0] != 62:
        return
    offset = struct.unpack_from("<Q", header, 32)[0]
    size, count = struct.unpack_from("<HH", header, 54)
    with path.open("r+b") as stream:
        for index in range(count):
            at = offset + index * size
            stream.seek(at)
            segment = stream.read(size)
            if struct.unpack_from("<I", segment)[0] != 3:
                continue
            value = os.fsencode(loader) + b"\0"
            if len(value) > 4096:
                raise RuntimeError("cache path is too long for a Linux ELF interpreter")
            stream.seek(0, 2)
            position = stream.tell()
            stream.write(value)
            stream.seek(at + 8)
            stream.write(struct.pack("<Q", position))
            stream.seek(at + 32)
            stream.write(struct.pack("<QQ", len(value), len(value)))
            return


def main():
    source = Path(sys.argv[1]).resolve()
    build = (source / "build-id").read_text().strip()
    base = Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache") / "pengudroid/appimage"
    base.mkdir(parents=True, exist_ok=True)
    destination = base / build
    with (base / "install.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if not (destination / ".complete").is_file():
            temporary = Path(tempfile.mkdtemp(prefix=".install-", dir=base))
            try:
                shutil.copytree(source / "usr", temporary / "usr", symlinks=True)
                shutil.copy2(source / "AppRun", temporary / "AppRun")
                shutil.copy2(source / "build-id", temporary / "build-id")
                loader = destination / "usr/portable/loader"
                for path in (temporary / "usr").rglob("*"):
                    if path.is_file() and not path.is_symlink():
                        patch_interpreter(path, loader)
                (temporary / ".complete").write_text(build)
                if destination.exists():
                    raise RuntimeError(f"incomplete cache exists: {destination}; inspect it before retrying")
                temporary.rename(destination)
            finally:
                if temporary.exists():
                    shutil.rmtree(temporary)
    env = dict(os.environ)
    env["PENGUDROID_PORTABLE_READY"] = "1"
    env["PYTHONHOME"] = str(destination / "usr/python")
    env["PENGUDROID_PYTHON"] = str(destination / "usr/python/bin/python3")
    env["PENGUDROID_PREPARE_SCRIPT"] = str(destination / "usr/share/pengudroid/setup/tools/prepare-android17.py")
    env["PENGUDROID_MODULES_SCRIPT"] = str(destination / "usr/share/pengudroid/setup/tools/runtime-modules.py")
    env["SSL_CERT_FILE"] = str(destination / "usr/portable/ca-certificates.crt")
    env["PIPEWIRE_MODULE_DIR"] = str(destination / "usr/portable/pipewire-0.3")
    env["SPA_PLUGIN_DIR"] = str(destination / "usr/portable/spa-0.2")
    env["LD_LIBRARY_PATH"] = str(destination / "usr/portable/lib")
    env["PENGUDROID_PORTABLE_LIBS"] = env["LD_LIBRARY_PATH"]
    env["GSETTINGS_SCHEMA_DIR"] = str(destination / "usr/share/glib-2.0/schemas")
    env["GSK_RENDERER"] = "cairo"
    env["FONTCONFIG_FILE"] = str(destination / "usr/portable/fonts.conf")
    env["PATH"] = str(destination / "usr/tools") + ":" + env.get("PATH", "/usr/bin:/bin")
    shell = destination / "usr/tools/sh"
    os.execve(shell, [str(shell), str(destination / "AppRun"), *sys.argv[2:]], env)


if __name__ == "__main__":
    main()
