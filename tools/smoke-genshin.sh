#!/usr/bin/env bash
# Reproduce the ARM64 Unity launch with the TLS-adapted development bridge.
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
export PENGUDROID_NATIVE_BRIDGE_LIB=${PENGUDROID_NATIVE_BRIDGE_LIB:-"$repo/target/guest-pc-ring/lib/libberberis_arm64.so"}
export PENGUDROID_ARM64_GUEST_ROOT=${PENGUDROID_ARM64_GUEST_ROOT:-"$repo/.toolchain/nativebridge-guest-linker-build/bundle"}
export PENGUDROID_BRIDGE_TLS=1
export BERBERIS_FLAGS=${BERBERIS_FLAGS:-glibc-host-thread-id-handoff}
export BERBERIS_MODE=${BERBERIS_MODE:-lite-translate-or-interpret}
export SMOKE_BACKEND=art
export SMOKE_SECONDS=${SMOKE_SECONDS:-120}
export SMOKE_DIR=${SMOKE_DIR:-"$repo/target/genshin-smoke"}
if [[ $# -eq 0 ]]; then
    echo "Usage: $0 <Genshin .apkm or .apk> [-- launch arguments...]" >&2
    exit 2
fi
if [[ ! -f "$PENGUDROID_NATIVE_BRIDGE_LIB" || ! -d "$PENGUDROID_ARM64_GUEST_ROOT/system" ]]; then
    echo "Missing development bridge or native-bridge guest bundle; see docs/STATUS.md." >&2
    exit 1
fi
exec python3 -B "$repo/tools/smoke_bundle.py" "$@"
