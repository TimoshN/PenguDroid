#!/bin/sh
# Materialise the Android 17 data-only runtime used by the AArch64 Aurora RPM.
# Extraction is a build-host operation only: the resulting files are installed
# directly by RPM and are never unpacked by the application launcher.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
ARCHIVE="$ROOT/target/aurora/android17-arm64.tar.gz"
FULL="$ROOT/target/aurora/android17-arm64-full/android17-arm64"
OUT="$ROOT/target/aurora/android17-arm64-minimal/android17-arm64"
MANIFEST="$ROOT/rpm/aurora/android17-arm64-minimal.files"

[ -f "$ARCHIVE" ] || { echo "error: missing $ARCHIVE" >&2; exit 1; }
[ -f "$MANIFEST" ] || { echo "error: missing $MANIFEST" >&2; exit 1; }

if [ ! -f "$FULL/system/framework/framework.jar" ]; then
    rm -rf "$ROOT/target/aurora/android17-arm64-full"
    mkdir -p "$ROOT/target/aurora/android17-arm64-full"
    tar -xzf "$ARCHIVE" -C "$ROOT/target/aurora/android17-arm64-full"
fi

rm -rf "$ROOT/target/aurora/android17-arm64-minimal"
mkdir -p "$OUT"
(
    cd "$FULL"
    while IFS= read -r path; do
        [ -n "$path" ] || continue
        [ -f "$path" ] || { echo "error: runtime file is missing: $path" >&2; exit 1; }
        cp -a --parents -- "$path" "$OUT/"
    done < "$MANIFEST"
    # Keep Android's mount-point links (apex -> system/apex, etc), but copy no
    # unlisted regular files. Broken optional links are harmless to the guest.
    find . -type l -exec cp -a --parents -- '{}' "$OUT/" ';'
)
# Preserve required data behind links (notably framework/arm64/boot*.vdex)
# as ordinary files. Links to omitted native binaries remain absent.
find "$OUT" -type l | while IFS= read -r link; do
    if [ -f "$link" ]; then
        cp -L "$link" "$link.pengudroid-copy"
        mv "$link.pengudroid-copy" "$link"
    fi
done
# APM rejects Android's remaining symlink forest. PenguDroid maps /apex,
# /product and /system_ext directly into system/, so no links are required.
find "$OUT" -type l -delete
find "$OUT" -type f -exec chmod 0644 '{}' '+'
find "$OUT" -depth -type d -empty -delete

# A data-only runtime takes executable ART libraries from PENGUDROID_ART_LIBS.
python3 - "$OUT/pengudroid-runtime.json" <<'PY'
import json
import sys
path = sys.argv[1]
with open(path, encoding="utf-8") as stream:
    manifest = json.load(stream)
manifest["data_only"] = True
with open(path, "w", encoding="utf-8") as stream:
    json.dump(manifest, stream, indent=2, sort_keys=True)
    stream.write("\n")
PY

# ART rejects writable DEX inputs. RPM installs these files read-only as well.
find "$OUT" -type f \( -name '*.apk' -o -name '*.jar' -o -name '*.art' \
    -o -name '*.oat' -o -name '*.odex' -o -name '*.vdex' \) -exec chmod a-w '{}' '+'
for required in \
    system/build.prop \
    system/framework/framework.jar \
    system/framework/arm64/boot.art \
    system/apex/com.android.art/javalib/core-oj.jar \
    system/apex/com.android.i18n/javalib/core-icu4j.jar; do
    [ -f "$OUT/$required" ] || { echo "error: incomplete minimal runtime: $required" >&2; exit 1; }
done

du -sh "$OUT"
