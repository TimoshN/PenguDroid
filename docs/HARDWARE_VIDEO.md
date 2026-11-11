# Host hardware video decoding

## Current scope

PenguDroid has a host-only hardware decoder with two backends behind one C ABI, and a direct Wayland dma-buf import path. **NewPipe still decodes in software**: its picture reaches the window through `c2.android.avc.decoder` and a `wl_shm` snapshot (measured 2026-11-11, 630 frames in a 60 s run). The host decoder is not yet registered as an Android Codec2 component; the daemon's packet broker (`video-decode-test --broker`) is the host end that adapter will talk to.

| Backend | Driver | What reaches the compositor | Validated on |
| --- | --- | --- | --- |
| VA-API | Intel iHD, AMD radeonsi | the decode surface itself: NV12, two planes, driver modifier, no copy | Intel UHD 630 + GNOME/Mutter (earlier run, not repeated on 2026-11-11) |
| Vulkan Video | NVIDIA | one RGBA plane (`XB24`), block-linear modifier, after one GPU compute pass | RTX 3080, driver 595.99, GNOME Shell 50 (2026-11-11) |

VA-API is tried first; a render node whose driver has no VA-API falls through to Vulkan Video. `Decoder::backend()` and the CLI report which one ran. Neither backend substitutes a software decoder.

| Path | Evidence / status |
| --- | --- |
| VA-API discovery | Real VLD profiles queried per DRM render node |
| H.264 compressed input → GPU decode | 128×128 and 1280×720 I_PCM fixtures on both backends; on Vulkan also 60- and 90-frame x264 streams with P and B frames |
| GPU frame → DMA-BUF | VA-API: synchronized, composed NV12 export. Vulkan: fence-waited RGBA export, modifier fitted to the image height |
| DMA-BUF → Wayland | 32 decoded frames imported directly on GNOME on both backends; no CPU scaling or pixel upload |
| Release and backpressure | Four returned decode frames maximum; three outstanding compositor buffers per surface; source held through actual `wl_buffer.release` |
| Flush / EOS / timestamps | Fixture integration test checks flush, EOS drain and microsecond output timestamps |
| Daemon packet broker | `video-decode-test --broker`: compressed input reaches the daemon, is decoded and imported on the GPU (both backends) |
| NewPipe / MediaCodec adapter | Pending |
| AMD | Same VA-API path is available; real hardware validation pending |
| Vulkan | Separate work for *apps*; loading the host Vulkan loader does not provide Android surface integration |

### Why NVIDIA needs a conversion pass

Each of these was measured on the RTX 3080 (595.99) with Mutter, not assumed:

1. **The decode image cannot be exported.** `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` with video-decode usage fails with `VK_ERROR_FORMAT_NOT_SUPPORTED`, and FFmpeg's own Vulkan→DRM mapping (`av_hwframe_map`) crashes inside `libnvidia-glcore`.
2. **A YUV dma-buf cannot be presented.** Mutter imports a multi-plane buffer one plane at a time, as `R8` + `GR88` EGLImages. NVIDIA's EGL lists `RG88` but not `GR88` (`eglQueryDmaBufModifiersEXT(GR88)` fails, `eglCreateImage` returns `EGL_BAD_MATCH`), so the compositor kills the client with "failed to import supplied dmabufs". A linear two-plane NV12 image, the only NV12 layout Vulkan offers, is external-only in EGL and cannot be bound as a texture either. Mutter advertises NV12 regardless.
3. **RGBA works**, and is what the same driver already scans out for an app's own GL frames. So the backend converts NV12 (or P010) to RGBA in one compute pass (`native/video_convert.comp`, BT.601/709/2020, limited or full range from the stream's tags) into an exportable image and hands that over.
4. **The modifier has to be corrected for short images.** NVIDIA lays a short image out with a smaller block than requested (a 128-row image asked for as `…15` is really `…14`) while still reporting the requested modifier, even from `vkGetImageDrmFormatModifierPropertiesEXT`; EGL then rejects the import. The bridge names the real layout. Checked from 16 to 1088 rows by importing the exported buffer in EGL and comparing every byte with the frame.

Colour accuracy against FFmpeg's software conversion (`scale=…:flags=accurate_rnd+full_chroma_int`): at most one code of difference, on the two fixtures and on every frame of a 1280×720 60-frame and an 854×480 90-frame x264 stream (2 B-frames, GOP 30). P010 goes through the same pass but **has not been run**, and its output is 8-bit, so HDR is not preserved.

The normal decode/export path never calls `av_hwframe_transfer_data`. Explicit fixture diagnostics (`test_luma` for a YUV frame, `test_rgba` for an RGBA one) can download a frame to verify pixel values; that diagnostic is not part of playback. If hardware format negotiation, device initialization or export fails, the decoder reports an error rather than selecting a software decoder.

## Native bridge and build

The optional `libpengudroid_video.so` is a small C bridge compiled against the host's public FFmpeg headers and libva and/or Vulkan headers. Its versioned C ABI exposes no FFmpeg structure layouts to Rust or Android. `pengudroid-hostgpu::video::Decoder` loads it at runtime. It belongs in the **host/broker process**, not the Bionic guest's linker namespace.

Dependencies are a C11 compiler, pkg-config and development packages for libavcodec and libavutil, plus libva (VA-API backend) and/or Vulkan (Vulkan Video backend); on Debian: `libavcodec-dev libavutil-dev` and `libva-dev` and/or `libvulkan-dev`. The script compiles in every backend whose headers it finds and prints which. The conversion shader is checked in as SPIR-V (`native/video_convert.spv.h`); `tools/build-video-bridge.sh --shader` regenerates it with `glslangValidator`. Ordinary Cargo builds remain usable without these development packages. Build the optional plugin after building the CLI:

```sh
cargo build --workspace
tools/build-video-bridge.sh
```

The default output is `target/debug/libpengudroid_video.so`. Pass an output path for another layout, such as `target/release/libpengudroid_video.so`. At runtime the Rust API searches beside the executable, or uses the explicit `PENGUDROID_VIDEO_BRIDGE` path. The plugin links to the host FFmpeg/libva/Vulkan SONAMEs; rebuild it when changing those ABI versions. The Intel validation used locally extracted Debian development headers, without changing installed system packages.

## Reproduce the real hardware tests

```sh
python3 tools/make_h264.py /tmp/pd-gradient.h264 8 8
target/debug/pengudroid video-decode-test /tmp/pd-gradient.h264 --verify-gradient

export PENGUDROID_TEST_VIDEO_BRIDGE="$PWD/target/debug/libpengudroid_video.so"
export PENGUDROID_TEST_VIDEO_FIXTURE=/tmp/pd-gradient.h264
cargo test -p pengudroid-hostgpu --test hardware_video -- --nocapture
cargo test -p pengudroid-daemon --lib hardware_video_import -- --nocapture
```

Set `PENGUDROID_TEST_VIDEO_DEVICE` for a render node other than `/dev/dri/renderD128`. The CLI also accepts `--device`. These tests require real hardware; they do not substitute software decoding. Without the explicit plugin/fixture environment, the hardware integration cases skip and print/return accordingly; a passing ordinary workspace run alone does not prove hardware decode.

The CLI probe accepts **one complete Annex-B H.264 access unit**, including SPS/PPS. It is not a container demuxer or a complete video player. `--verify-gradient` expects the fixture's first block (Y=64, Cb=128, Cr=192): luma 64 on VA-API, or its RGB value on Vulkan — (158, 4, 56) below 720 lines, (171, 22, 56) from 720 up. The generator now correctly marks its stream as constrained baseline and selects a level for its macroblock dimensions, matching its actual coding tools and the driver's supported H.264 profile.

The hardware lifecycle test performs 2,000 decode/flush/export cycles after warming caches, checks descriptor counts and bounded RSS, holds four GPU surfaces to exercise backpressure, and verifies a frame remains valid after decoder destruction. On the recorded VA-API run, descriptor count stayed 5 → 5 and RSS changed 31,496 → 31,580 KiB; on the Vulkan run (2026-11-11) 85 → 85 and 178,272 → 180,664 KiB. This is an isolated decoder test, not a long-running NewPipe memory result.

The compositor test preserves both NV12 planes (VA-API) or the single RGBA plane (Vulkan), waits for release after replacing a frame, and closes a window with the last GPU frame attached. Its source surface is retained until the compositor releases it. DMA-BUF v3 is bound deliberately for explicit format/modifier advertisements; the v4 feedback protocol is not implemented. Unsupported format/modifier pairs are rejected before import. Ordinary submit acknowledgments are not sufficient to recycle a decoder surface.

## NewPipe component selection and output surface

An earlier captured NewPipe run selected `c2.android.avc.decoder` (`video/avc`), with 1280×720 HAL format 4 (`RGB_565`) buffers reaching BLAST and software `wl_shm`. That is a historical checkpoint, not current API-37 video validation.

The latest API-37 run (`target/newpipe-api37-hidl-discovery/`) no longer aborts in HIDL manager discovery. It finds `__ApexCodecs__` and one real AAC codec, reports 14 positive codec output events / 57,344 output bytes, then fails with `AudioSink$InitializationException` and invalid audio session ID `-1`. It does not establish audible playback or video decode. The existing native HIDL store loader stops at missing `android::hardware::Parcel::setData` export; do not substitute another private signature or fabricate codec entries. The audio registration hook is also bypassed; its old `native_setup` ABI does not match Android 17, so enabling it unchanged is unsafe. See [STATUS.md](STATUS.md) for the next typed JNI boundary and retained contract audit.

A Codec2 replacement has to return the exact work and graphic-buffer ownership the framework expects. Directly presenting a host DMA-BUF as a separate Wayland video layer would still require the Codec2 work to complete, the Android output surface not to cover or discard that frame, and format/flush/reconfigure behavior to remain correct. Returning a YUV frame through the existing RGB565 memfd allocator would reintroduce a CPU copy and conversion. Component advertisement must only happen when that full contract is implemented; a name in the codec list alone is not a working decoder.

## Next integration boundary

The existing shim's `codec2.rs` attempts to build the real Android `C2PlatformComponentStore`, which opens `libcodec2_soft_*` plugins when its ABI requirements are met. That attempted HIDL path is not currently available on the restored API-37 runtime. Advertising a hardware codec name without implementing its component would be incorrect. The next adapter must connect complete compressed access units and codec configuration to the host decoder while preserving Android work completion, timestamps, flush/EOS, surface association, color metadata and ownership. Its packet queues and returned frames must remain bounded. Per-frame presentation pacing/audio synchronization and resolution changes need real playback coverage.

The current decoder can request H.264, HEVC, VP8, VP9 and AV1 from FFmpeg, but only H.264 has been tested with compressed input (FFmpeg has no Vulkan VP8 decoder, so VP8 is VA-API only). Driver profile discovery does not establish end-to-end support for the other codecs. On VA-API, color-space/range metadata and HDR handling are not yet carried into compositor presentation; on Vulkan the matrix and range are applied in the conversion pass and HDR is reduced to 8-bit. Host Vulkan, GLES and multi-GPU selection must be validated independently of media decode.

References: [FFmpeg hardware decode example](https://github.com/FFmpeg/FFmpeg/blob/master/doc/examples/hw_decode.c), [VA-API core synchronization/export](https://intel.github.io/libva/group__api__core.html), [DRM PRIME surface descriptors](https://github.com/intel/libva/blob/master/va/va_drmcommon.h).
