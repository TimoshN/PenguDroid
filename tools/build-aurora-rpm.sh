#!/bin/sh
# Cross-build PenguDroid for AArch64 and assemble an Aurora OS RPM.
#
# The Android runtime is deliberately not embedded. Prepare it separately:
#   python3 tools/prepare-android17.py --arch arm64 --download --cache
#
# Usage: tools/build-aurora-rpm.sh [--skip-build] [--sysroot DIR] [--no-rpm]
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TARGET=${CARGO_TARGET_DIR:-$ROOT/target}
TRIPLE=aarch64-unknown-linux-gnu
TOOLCHAIN=${PENGUDROID_RUST_TOOLCHAIN:-1.93}
PACKAGE=${PENGUDROID_AURORA_PACKAGE:-io.github.denizomer10.MaxNext}
SYSROOT=${PENGUDROID_AURORA_SYSROOT:-}
SKIP_BUILD=0
NO_RPM=0

while [ $# -gt 0 ]; do
    case "$1" in
        --skip-build) SKIP_BUILD=1 ;;
        --no-rpm) NO_RPM=1 ;;
        --sysroot) shift; SYSROOT=${1:?--sysroot needs a directory} ;;
        *) echo "error: unknown option $1" >&2; exit 2 ;;
    esac
    shift
done

if [ -z "$SYSROOT" ]; then
    for candidate in "$HOME"/AuroraPlatformSDK-*/targets/*-aarch64; do
        if [ -f "$candidate/usr/lib/pkgconfig/fontconfig.pc" ]; then
            SYSROOT=$candidate
        fi
    done
fi
if [ -n "$SYSROOT" ] && [ ! -d "$SYSROOT/usr" ]; then
    echo "error: Aurora AArch64 sysroot not found: $SYSROOT" >&2
    exit 1
fi

CC=${PENGUDROID_AARCH64_CC:-aarch64-linux-gnu-gcc}
AR=${PENGUDROID_AARCH64_AR:-aarch64-linux-gnu-ar}
command -v "$CC" >/dev/null || { echo "error: missing $CC" >&2; exit 1; }
command -v "$AR" >/dev/null || { echo "error: missing $AR" >&2; exit 1; }

export CC AR
export PENGUDROID_AURORA_CLI="/usr/bin/$PACKAGE"
export PENGUDROID_AURORA_LIBDIR="/usr/share/$PACKAGE/lib"
export PENGUDROID_AURORA_UNIFIED=1
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="$CC"
export PKG_CONFIG_ALLOW_CROSS=1

# rustc's anonymous export map and the shim's Android symbol-version map must
# be combined by LLD; older GNU ld rejects that combination.
RUST_SYSROOT=$(rustc "+$TOOLCHAIN" --print sysroot)
RUST_HOST=$(rustc "+$TOOLCHAIN" -vV | awk '/^host:/ {print $2}')
LLD_DIR="$RUST_SYSROOT/lib/rustlib/$RUST_HOST/bin/gcc-ld"
LLD_FLAGS=""
if [ -f "$LLD_DIR/ld.lld" ]; then
    LLD_FLAGS="-C link-arg=-B$LLD_DIR -C link-arg=-fuse-ld=lld"
fi
RPATH_FLAGS="-C link-arg=-Wl,-rpath,/usr/share/$PACKAGE/lib"

if [ -n "$SYSROOT" ]; then
    export PKG_CONFIG_SYSROOT_DIR="$SYSROOT"
    export PKG_CONFIG_PATH="$SYSROOT/usr/lib/pkgconfig:$SYSROOT/usr/share/pkgconfig"
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS:-} $LLD_FLAGS $RPATH_FLAGS -C link-arg=--sysroot=$SYSROOT"
    echo "using Aurora sysroot: $SYSROOT"
else
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS:-} $LLD_FLAGS $RPATH_FLAGS"
    echo "warning: no Aurora sysroot; producing a generic AArch64 Linux build" >&2
fi

if [ "$SKIP_BUILD" -eq 0 ]; then
    cargo "+$TOOLCHAIN" build --release --offline --target "$TRIPLE" \
        -p pengudroid-cli -p pengudroid-gui -p pengudroid-daemon \
        -p pengudroid-shim -p pengudroid-gleshim -p pengudroid-vkshim
fi

ARTIFACTS="$TARGET/$TRIPLE/release"
missing=""
for file in pengudroid pengudroid-gui pengudroid-daemon \
            libpengudroid_shim.so libpengudroid_gleshim.so libpengudroid_vkshim.so; do
    [ -f "$ARTIFACTS/$file" ] || missing="$missing $file"
done
if [ -n "$missing" ]; then
    echo "error: missing release artifacts:$missing" >&2
    exit 1
fi

WORK="$TARGET/aurora"
STAGE="$WORK/package/pengudroid-aurora"
PRIVATE="$STAGE/usr/share/$PACKAGE/lib"
rm -rf "$STAGE"
mkdir -p "$PRIVATE" "$STAGE/usr/bin" "$STAGE/usr/share/applications" \
    "$STAGE/usr/share/icons/hicolor/256x256/apps"

install -m 0755 "$ARTIFACTS/pengudroid-gui" "$STAGE/usr/bin/$PACKAGE"
for file in libpengudroid_shim.so libpengudroid_gleshim.so libpengudroid_vkshim.so; do
    install -m 0644 "$ARTIFACTS/$file" "$PRIVATE/$file"
done
install -m 0644 "$ROOT/rpm/aurora/pengudroid.desktop" \
    "$STAGE/usr/share/applications/$PACKAGE.desktop"
install -m 0644 "$ROOT/rpm/aurora/maxnext.png" "$STAGE/usr/share/icons/hicolor/256x256/apps/$PACKAGE.png"
install -m 0644 "$ROOT/LICENSE" "$STAGE/usr/share/$PACKAGE/LICENSE"

# Aurora's launcher uses these sizes. The original remains as a generic fallback.
if command -v ffmpeg >/dev/null 2>&1; then
    for size in 86 108 128 172; do
        dir="$STAGE/usr/share/icons/hicolor/${size}x${size}/apps"
        mkdir -p "$dir"
        ffmpeg -loglevel error -y -i "$ROOT/rpm/aurora/maxnext.png" -vf "scale=$size:$size" \
            -frames:v 1 "$dir/$PACKAGE.png"
    done
fi

printf '\nAArch64 package tree: %s\n' "$STAGE"
if [ "$NO_RPM" -eq 1 ]; then
    exit 0
fi
command -v rpmbuild >/dev/null || { echo "error: rpmbuild is required (or use --no-rpm)" >&2; exit 1; }

mkdir -p "$WORK/rpm/SOURCES"
TAR="$WORK/rpm/SOURCES/pengudroid-aurora.tar.gz"
tar --sort=name --mtime='@0' --owner=0 --group=0 --numeric-owner \
    -C "$WORK/package" -czf "$TAR" pengudroid-aurora
rpmbuild -bb "$ROOT/rpm/aurora/pengudroid.spec" \
    --target aarch64-linux \
    --define "_topdir $WORK/rpm" \
    --define "_builddir $WORK/rpm/BUILD"

printf '\nBuilt RPM:\n'
find "$WORK/rpm/RPMS" -type f -name "$PACKAGE-*.rpm" -print
