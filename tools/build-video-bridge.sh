#!/bin/sh
# Build the optional host decoder plugin using distribution development headers.
# Requires pkg-config, libavcodec-dev and libavutil-dev, plus at least one of:
#   libva-dev      VA-API backend (Intel, AMD): exports the decode surface.
#   libvulkan-dev  Vulkan Video backend (NVIDIA): one GPU colour-conversion pass
#                  per frame, exported as RGBA.
# Both are compiled in when both are present; VA-API is tried first at runtime.
#
# `--shader` regenerates native/video_convert.spv.h from video_convert.comp
# (needs glslangValidator). The generated header is checked in, so an ordinary
# build needs no shader compiler.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
if [ "${1:-}" = "--shader" ]; then
    native="$root/crates/pengudroid-hostgpu/native"
    glslangValidator -V --quiet --vn pd_video_convert_spirv -o "$native/video_convert.spv.h" "$native/video_convert.comp"
    sed -i 's/^const uint32_t pd_video_convert_spirv/static const uint32_t pd_video_convert_spirv/' "$native/video_convert.spv.h"
    printf 'Regenerated %s\n' "$native/video_convert.spv.h"
    exit 0
fi
out=${1:-"$root/target/debug/libpengudroid_video.so"}
mkdir -p "$(dirname -- "$out")"
packages="libavcodec libavutil"
backends=""
if pkg-config --exists libva; then packages="$packages libva"; backends="$backends -DPD_VIDEO_VAAPI"; fi
if pkg-config --exists vulkan; then packages="$packages vulkan"; backends="$backends -DPD_VIDEO_VULKAN"; fi
if [ -z "$backends" ]; then
    echo "build-video-bridge: neither libva nor vulkan development files were found" >&2
    exit 1
fi
video_cflags=$(pkg-config --cflags $packages)
video_libs=$(pkg-config --libs $packages)
# Additional flags allow a locally extracted SDK without modifying the system.
# Deliberate word splitting: compiler/pkg-config flag lists.
${CC:-cc} -std=c11 -O2 -fPIC -shared -Wall -Wextra -Werror \
    ${CPPFLAGS:-} $backends $video_cflags \
    "$root/crates/pengudroid-hostgpu/native/video_bridge.c" \
    ${LDFLAGS:-} $video_libs \
    -Wl,-z,defs -Wl,-z,relro,-z,now -o "$out"
printf 'Built %s (%s)\n' "$out" "$(echo $backends | sed 's/-DPD_VIDEO_//g')"
