#!/bin/sh
# Build the PenguDroid AppImage: one file that installs, lists, launches, stops
# and removes Android packages.
#
# The image carries the desktop shell plus the `pengudroid` CLI it drives, the
# two host libraries the CLI needs at runtime (`libpengudroid_shim.so` for the
# Bionic surface, `libpengudroid_gleshim.so` for EGL/GLES), an icon and a
# desktop entry. The Android sysroot is far too large to embed, so the AppImage
# finds one the same way the CLI does: PENGUDROID_SYSROOT, the working
# directory's `.toolchain/sysroot-android17`, or the per-user install path.
#
# Usage: tools/build-appimage.sh [--skip-build] [--with-bridge] [--with-runtime]
#                                [--bridge-dir DIR] [--output PATH]
#
#   --skip-build    reuse whatever is already in target/release
#   --with-bridge   also embed the ARM64 translator, its proxies and the host
#                   GL forwarders (from --bridge-dir, default
#                   .toolchain/nativebridge-stage), so ARM-only games need no
#                   separately prepared native-bridge directory
#   --with-runtime  also embed the Android 17 runtime (.toolchain/sysroot-android17)
#                   and the ARM64 guest root (.toolchain/guest-arm64-bridge):
#                   the image then needs nothing but a Wayland desktop
#   --bridge-dir    where the translator and proxies are
#   --output PATH   write the AppImage here (default: target/PenguDroid-x86_64.AppImage)
#   --no-image      assemble the AppDir and stop (no appimagetool needed)
#
# Environment:
#   PENGUDROID_APPIMAGETOOL  appimagetool binary (default: cache, then PATH)
#   PENGUDROID_APPIMAGE_COMP squashfs compression (default: zstd)
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TARGET="$ROOT/target"
OUT="$TARGET/PenguDroid-x86_64.AppImage"
SKIP_BUILD=0
WITH_BRIDGE=0
WITH_RUNTIME=0
BRIDGE_DIR="$ROOT/.toolchain/nativebridge-stage"
NO_IMAGE=0
PORTABLE=0

while [ $# -gt 0 ]; do
    case "$1" in
        --skip-build) SKIP_BUILD=1 ;;
        --with-bridge) WITH_BRIDGE=1 ;;
        --with-runtime) WITH_RUNTIME=1 ;;
        --portable) PORTABLE=1 ;;
        --bridge-dir) shift; BRIDGE_DIR=${1:?--bridge-dir needs a path} ;;
        --output) shift; OUT=${1:?--output needs a path} ;;
        --no-image) NO_IMAGE=1 ;;
        *) echo "error: unknown option $1" >&2; exit 2 ;;
    esac
    shift
done

if [ "$SKIP_BUILD" -eq 0 ]; then
    echo "building release binaries"
    cargo build --release --offline \
        -p pengudroid-cli -p pengudroid-gui \
        -p pengudroid-shim -p pengudroid-gleshim -p pengudroid-daemon
fi

missing=""
for f in pengudroid pengudroid-gui pengudroid-daemon libpengudroid_shim.so libpengudroid_gleshim.so; do
    [ -f "$TARGET/release/$f" ] || missing="$missing $f"
done
if [ -n "$missing" ]; then
    echo "error: missing release artifacts:$missing" >&2
    echo "hint: run without --skip-build" >&2
    exit 1
fi

APPDIR="$TARGET/appimage/PenguDroid.AppDir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/256x256/apps" "$APPDIR/usr/share/pengudroid"

install -m 0755 "$TARGET/release/pengudroid-gui" "$APPDIR/usr/bin/pengudroid-gui"
install -m 0755 "$TARGET/release/pengudroid" "$APPDIR/usr/bin/pengudroid"
install -m 0755 "$TARGET/release/pengudroid-daemon" "$APPDIR/usr/bin/pengudroid-daemon"
install -m 0755 "$TARGET/release/libpengudroid_shim.so" "$APPDIR/usr/bin/libpengudroid_shim.so"
install -m 0755 "$TARGET/release/libpengudroid_gleshim.so" "$APPDIR/usr/bin/libpengudroid_gleshim.so"
install -m 0644 "$ROOT/logo.png" "$APPDIR/usr/share/icons/hicolor/256x256/apps/pengudroid.png"
cp "$APPDIR/usr/share/icons/hicolor/256x256/apps/pengudroid.png" "$APPDIR/pengudroid.png"

if [ "$WITH_BRIDGE" -eq 1 ]; then
    bridge_src="$BRIDGE_DIR"
    if [ ! -f "$bridge_src/libberberis_arm64.so" ]; then
        echo "error: --with-bridge needs $bridge_src/libberberis_arm64.so" >&2
        echo "hint: build the translator and proxies first (tools/build-nativebridge.py)" >&2
        exit 1
    fi
    mkdir -p "$APPDIR/usr/lib/pengudroid/bridge"
    # Only the bridge libraries: the directory is de-bionic'd per run, so it must
    # not contain the interpreter archives that also live beside the translator.
    for lib in "$bridge_src"/*.so; do
        install -m 0755 "$lib" "$APPDIR/usr/lib/pengudroid/bridge/"
    done
    echo "embedded $(ls "$APPDIR/usr/lib/pengudroid/bridge" | wc -l) bridge libraries"
    guest_src="$ROOT/.toolchain/guest-arm64-bridge"
    test -f "$guest_src/pengudroid-arm64-bridge-bundle.json" || { echo "missing built ARM64 guest bundle" >&2; exit 1; }
    mkdir -p "$APPDIR/usr/share/pengudroid/guest-arm64"
    cp -a "$guest_src"/. "$APPDIR/usr/share/pengudroid/guest-arm64/"
fi

if [ "$WITH_RUNTIME" -eq 1 ]; then
    runtime_src="$ROOT/.toolchain/sysroot-android17"
    guest_src="$ROOT/.toolchain/guest-arm64-bridge"
    for d in "$runtime_src/system" "$guest_src/system"; do
        if [ ! -d "$d" ]; then
            echo "error: --with-runtime needs $d" >&2
            echo "hint: tools/prepare-android17.py prepares the runtime" >&2
            exit 1
        fi
    done
    # Copies, not links: the AppDir is packed as is. The runtime's `data` is
    # left out; ART gets a writable per-user ANDROID_DATA instead.
    mkdir -p "$APPDIR/usr/share/pengudroid/sysroot" "$APPDIR/usr/share/pengudroid/guest-arm64"
    for entry in "$runtime_src"/*; do
        [ "$(basename "$entry")" = data ] && continue
        cp -a "$entry" "$APPDIR/usr/share/pengudroid/sysroot/"
    done
    mkdir -p "$APPDIR/usr/share/pengudroid/sysroot/data/dalvik-cache/x86_64"
    cp -a "$guest_src"/. "$APPDIR/usr/share/pengudroid/guest-arm64/"
    echo "embedded the Android runtime ($(du -sh "$APPDIR/usr/share/pengudroid/sysroot" | cut -f1)) and ARM64 guest root ($(du -sh "$APPDIR/usr/share/pengudroid/guest-arm64" | cut -f1))"
fi

cat > "$APPDIR/usr/share/applications/pengudroid.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=PenguDroid
GenericName=Android App Manager
Comment=Install, launch and manage Android apps on Linux
Exec=pengudroid-gui %f
Icon=pengudroid
Terminal=false
Categories=Utility;System;
Keywords=android;apk;emulator;apps;
MimeType=application/vnd.android.package-archive;
StartupWMClass=pengudroid-gui
DESKTOP
install -m 0644 "$APPDIR/usr/share/applications/pengudroid.desktop" "$APPDIR/pengudroid.desktop"

cat > "$APPDIR/AppRun" <<'APPRUN'
#!/bin/sh
# AppRun: make the bundled pieces discoverable, then start the shell.
# `AppRun --cli <args>` runs the `pengudroid` CLI instead (no window needed).
HERE=$(CDPATH= cd -- "${0%/*}" && pwd)

if [ -f "$HERE/build-id" ] && [ ! -f "$HERE/.complete" ]; then
    export PYTHONHOME="$HERE/usr/python"
    export PYTHONNOUSERSITE=1
    unset PYTHONPATH
    exec "$HERE/usr/portable/loader" --library-path "$HERE/usr/portable/lib" \
        "$HERE/usr/python/bin/python3" -B "$HERE/usr/share/pengudroid/bootstrap.py" "$HERE" "$@"
fi

# The CLI and the shell both look for their host libraries next to the binary.
export PATH="$HERE/usr/bin:$PATH"
export LD_LIBRARY_PATH="$HERE/usr/bin${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

# Bundled ARM64 native bridge (built --with-bridge): the translator, its
# proxies and the host GL forwarders. Explicit settings always win so a user
# can test a differently prepared translator.
if [ -d "$HERE/usr/lib/pengudroid/bridge" ]; then
    : "${PENGUDROID_NATIVE_BRIDGE_LIB:=$HERE/usr/lib/pengudroid/bridge}"
    : "${PENGUDROID_GL_FORWARDERS:=$HERE/usr/lib/pengudroid/bridge}"
    : "${PENGUDROID_BRIDGE_TLS:=1}"
    : "${BERBERIS_FLAGS:=glibc-host-thread-id-handoff}"
    : "${BERBERIS_MODE:=lite-translate-or-interpret}"
    export PENGUDROID_NATIVE_BRIDGE_LIB PENGUDROID_GL_FORWARDERS PENGUDROID_BRIDGE_TLS BERBERIS_FLAGS BERBERIS_MODE
fi
if [ -z "${PENGUDROID_ARM64_GUEST_ROOT:-}" ] && [ -d "$HERE/usr/share/pengudroid/guest-arm64/system" ]; then
    export PENGUDROID_ARM64_GUEST_ROOT="$HERE/usr/share/pengudroid/guest-arm64"
fi

# The Android runtime: the bundled copy (built --with-runtime) first, then the
# per-user install location.
if [ -z "${PENGUDROID_SYSROOT:-}" ] && [ -d "$HERE/usr/share/pengudroid/sysroot" ]; then
    export PENGUDROID_SYSROOT="$HERE/usr/share/pengudroid/sysroot"
fi
if [ -z "${PENGUDROID_SYSROOT:-}" ] && [ -d "${XDG_DATA_HOME:-$HOME/.local/share}/pengudroid/sysroot" ]; then
    export PENGUDROID_SYSROOT="${XDG_DATA_HOME:-$HOME/.local/share}/pengudroid/sysroot"
fi

# Android WebView (logins, captchas, in-app pages) through the runtime's own
# provider, in-process, with Chromium's queued work on the framework looper.
: "${PENGUDROID_WEBVIEW:=1}"
: "${PENGUDROID_WEBVIEW_SINGLE_PROCESS:=1}"
: "${PENGUDROID_FRAMEWORK_LOOPER:=1}"
export PENGUDROID_WEBVIEW PENGUDROID_WEBVIEW_SINGLE_PROCESS PENGUDROID_FRAMEWORK_LOOPER
: "${PENGUDROID_AUTO_RUNTIME:=1}"
export PENGUDROID_AUTO_RUNTIME

if [ "${1:-}" = "--cli" ]; then
    shift
    exec "$HERE/usr/bin/pengudroid" "$@"
fi
exec "$HERE/usr/bin/pengudroid-gui" "$@"
APPRUN
chmod 0755 "$APPDIR/AppRun"

if [ "$PORTABLE" -eq 1 ]; then
    python3 -B "$ROOT/tools/package-portable.py" "$APPDIR"
fi

if [ "$NO_IMAGE" -eq 1 ]; then
    echo "assembled $APPDIR"
    exit 0
fi

# appimagetool is not packaged by every distribution; use a cached copy when the
# caller did not pin one. When building a portable image with uruntime, fall
# back to mksquashfs directly if appimagetool is absent.
tool=${PENGUDROID_APPIMAGETOOL:-}
if [ -z "$tool" ]; then
    for candidate in "$HOME/.cache/pengudroid/appimagetool.AppImage" "$(command -v appimagetool || true)"; do
        if [ -n "$candidate" ] && [ -x "$candidate" ]; then tool=$candidate; break; fi
    done
fi

# AppImages need FUSE to self-mount; extraction is enough to run the tool.
run_tool() {
    case "$tool" in
        *AppImage) "$tool" --appimage-extract-and-run "$@" ;;
        *) "$tool" "$@" ;;
    esac
}

rm -f "$OUT" "$OUT.sqfs"
# zstd keeps a runtime-carrying image well under GitHub's 2 GiB asset limit
# and decompresses fast at launch; PENGUDROID_APPIMAGE_COMP=xz is smaller.
if [ -n "$tool" ]; then
    if [ "$PORTABLE" -eq 1 ]; then
        runtime=$(python3 -B "$ROOT/tools/prepare-appimage-runtime.py")
        ARCH=x86_64 run_tool --runtime-file "$runtime" --comp "${PENGUDROID_APPIMAGE_COMP:-zstd}" "$APPDIR" "$OUT"
    else
        ARCH=x86_64 run_tool --comp "${PENGUDROID_APPIMAGE_COMP:-zstd}" "$APPDIR" "$OUT"
    fi
elif [ "$PORTABLE" -eq 1 ] && command -v mksquashfs >/dev/null 2>&1; then
    runtime=$(python3 -B "$ROOT/tools/prepare-appimage-runtime.py")
    mksquashfs "$APPDIR" "$OUT.sqfs" -root-owned -noappend -comp "${PENGUDROID_APPIMAGE_COMP:-zstd}" -mkfs-time 0
    cat "$runtime" "$OUT.sqfs" > "$OUT"
    rm -f "$OUT.sqfs"
else
    echo "error: appimagetool not found" >&2
    echo "hint: download appimagetool-x86_64.AppImage to ~/.cache/pengudroid/appimagetool.AppImage" >&2
    echo "      or set PENGUDROID_APPIMAGETOOL to its path" >&2
    exit 1
fi

if [ ! -s "$OUT" ]; then
    echo "error: appimagetool did not produce $OUT" >&2
    exit 1
fi
chmod +x "$OUT"
echo "built $OUT ($(du -h "$OUT" | cut -f1))"
