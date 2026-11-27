#!/bin/sh
# Rebuild the development MaxNext package (original APK + prepared ART libs)
# after a source change. `tools/build-aurora-rpm.sh` always starts from a clean
# staging tree, so the extra payload has to be added afterwards every time.
#
# Usage: tools/build-aurora-maxnext-test.sh [--release N]
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"
PKG=io.github.denizomer10.MaxNext
STAGE="$ROOT/target/aurora/package/pengudroid-aurora"
WORK="$ROOT/target/aurora/maxnext-test-rpm"
ARTIFACTS="$ROOT/target/aarch64-unknown-linux-gnu/release"
PAYLOAD="$ROOT/target/aurora/maxnext-rpm/stage/pengudroid-aurora/usr/share/$PKG"
SPEC="$ROOT/rpm/aurora/pengudroid.spec"

if [ "${1:-}" = "--release" ]; then
    RELEASE=$2
    sed -i "s/^Release:        .*/Release:        ${RELEASE}.aurora/" "$SPEC"
    sed -i "s/^\\* .* - 0\\.2\\.1-.*$/* $(date '+%a %b %d %Y') PenguDroid contributors - 0.2.1-${RELEASE}.aurora/" "$SPEC"
fi

# 1. Cross-compile everything (this wipes STAGE).
tools/build-aurora-rpm.sh

# 2. Install only the observed MAX/ART library closure, then overlay fresh
# shims. Keep the app's own native libraries except the optional WebView engine.
LIB="$STAGE/usr/share/$PKG/lib"
rm -rf "$LIB"
mkdir -p "$LIB/ru.oneme.app"
while IFS= read -r library; do
    install -m0644 "$PAYLOAD/lib/$library" "$LIB/$library"
done < "$ROOT/rpm/aurora/maxnext-arm64-minimal-libs.files"
for library in "$PAYLOAD"/lib/ru.oneme.app/*.so; do
    [ -f "$library" ] || continue
    [ "${library##*/}" = libwebviewchromium.so ] && continue
    install -m0644 "$library" "$LIB/ru.oneme.app/${library##*/}"
done
for library in libpengudroid_shim.so libpengudroid_gleshim.so \
               libpengudroid_hybris_pthread.so libpengudroid_vkshim.so; do
    install -m0644 "$ARTIFACTS/$library" "$LIB/$library"
done
install -m0644 "$LIB/libpengudroid_gleshim.so" "$LIB/libEGL.so"

# 3. Original APK plus a direct, read-only runtime. There is intentionally no
# nested runtime.tar.gz and no first-launch extraction.
mkdir -p "$STAGE/usr/share/$PKG/apps" "$STAGE/usr/share/$PKG/runtime"
install -m0444 "$PAYLOAD/apps/MaxNext.apk" "$STAGE/usr/share/$PKG/apps/MaxNext.apk"
tools/prepare-aurora-minimal-runtime.sh
cp -a "$ROOT/target/aurora/android17-arm64-minimal/android17-arm64" \
    "$STAGE/usr/share/$PKG/runtime/"

test -f "$LIB/libart.so"
test -f "$STAGE/usr/share/$PKG/apps/MaxNext.apk"
test -f "$STAGE/usr/share/$PKG/runtime/android17-arm64/system/framework/framework.jar"

# 4. Pack and sign. APM requires a strictly newer Release.
mkdir -p "$WORK/rpm/SOURCES" "$WORK/rpm/BUILD"
install -m0644 "$SPEC" "$WORK/app.spec"
tar --sort=name --mtime='@0' --owner=0 --group=0 --numeric-owner \
    -C "$(dirname "$STAGE")" \
    -czf "$WORK/rpm/SOURCES/pengudroid-aurora.tar.gz" pengudroid-aurora
rpmbuild -bb "$WORK/app.spec" --target aarch64-linux \
    --define "_topdir $WORK/rpm" --define "_builddir $WORK/rpm/BUILD" >/dev/null

RELEASE=$(awk '/^Release:/ {print $2}' "$SPEC")
RPM="$WORK/rpm/RPMS/aarch64/$PKG-0.2.1-$RELEASE.aarch64.rpm"
PSDK_CHROOT=$(echo "$HOME"/AuroraPlatformSDK-*/sdks/aurora_psdk/sdk-chroot | awk '{print $NF}')
"$PSDK_CHROOT" rpmsign-external sign --force \
    --key="$HOME/.aurora-cli/keys/regular_key.pem" \
    --cert="$HOME/.aurora-cli/keys/regular_cert.pem" "$RPM"

echo "$RPM"
