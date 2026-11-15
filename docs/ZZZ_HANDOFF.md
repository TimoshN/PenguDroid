# Zenless Zone Zero (Vulkan) Handoff — 0.2.1

This document records the exact state of Zenless Zone Zero (`com.HoYoverse.Nap` 3.3.0) on PenguDroid 0.2.1 so the next agent can continue directly from verified findings without re-investigating fixed issues.

## Local merge follow-up — 2026-11-16

The divergent local controller/activity fixes are merged with upstream 0.2.1.
Local touch/key/mouse keymap profiles remain supported; upstream Vulkan WSI,
image orientation, Mesa loading and physical touchscreen readers are retained.
The forced keyboard-to-Xbox preset replacement is deliberately not applied to
existing touch profiles. Physical controllers use the existing hotplug reader
and are exposed through the new virtual Android Xbox One device (ID 2), with
matching event device IDs. `PENGUDROID_GAMEPAD_PROFILE=xbox360` selects the
legacy identity without changing axis/key semantics.

Vulkan surface creation now invokes gleshim's exported
`pengudroid_subscribe_input` hook and publishes the current surface ID. The
`vulkan_input` integration test proves the subscription handshake and delivery
of pointer coordinates into the Android input queue without a GL window.

Validation: 486 Rust workspace tests and 258 Python tool tests passed after the
merge; the added Vulkan input integration test also passes. The Xbox One and Xbox 360 profiles also pass a live ART/InputManager
CheckJNI test. ZZZ 3.3.0 reaches the login screen on the RTX 3080 after the
DRM-modifier allocation fix below; login interaction remains under investigation.
This is not a gameplay claim.
The old local ARM bridge omitted Vulkan. For this run the bridge and ARM guest
bundle come from the SHA-256-verified project v0.2.1 release AppImage, while
host CLI/shims/daemon are built from the merged source. Rebuilding Vulkan
bridge artifacts from the older local dependency slice remains separate work.


## What Is Working (Verified)

1. **Vulkan initialization & presentation (`crates/pengudroid-cli/src/sysroot.rs`, `crates/pengudroid-vkshim/src/lib.rs`)**:
   - `LD_PRELOAD` includes `libpengudroid_vkshim.so` and host `libvulkan.so.1` ahead of Android's driverless `art-libs/libvulkan.so`.
   - `HostLoader` loads `vkDestroyInstance`, `vkEnumerateDeviceExtensionProperties`, `vkCreateDevice`, and `vkDestroyDevice` directly from `libvulkan.so.1` via `dlsym` to avoid `LD_PRELOAD` recursion.
2. **Wwise / OpenSL ES (`crates/pengudroid-shim/src/lib.rs`)**:
   - `SL_IID_*` symbols point to 64-byte `.rodata` `SLInterfaceID_` structs (`SlIidPtr`) instead of `0x5044_00xx` integer sentinels, fixing the `SIGSEGV` at `0x50440030` during Wwise initialization.
3. **Correct image orientation (`pengudroid-proto`, `pengudroid-daemon`, `pengudroid-vkshim`, `pengudroid-gleshim`, `pengudroid-cli`)**:
   - `Message::PresentDmabuf` carries `top_down: bool`.
   - `pengudroid-vkshim` sends `top_down: true` → daemon uses `wl_output::Transform::Normal`.
   - `pengudroid-gleshim` and `pengudroid-cli` offscreen GL send `top_down: false` → daemon keeps `wl_output::Transform::Flipped180`.
4. **AppImage X11 / catalog compatibility (`tools/package-portable.py`)**:
   - Bundled `libxkbcommon-x11.so.0` (plus `libxkbcommon-x11.so` symlink) into `usr/portable/lib` so `pengudroid-gui` (`winit` / `xkbcommon-dl`) does not fail with `Library libxkbcommon-x11.so could not be loaded` on X11-only / minimal test containers.
5. **Live run**:
   - `PENGUDROID_TRACE_VKSHIM=1 ./target/PenguDroid-x86_64.AppImage --cli launch /home/pc/Downloads/Zenless+Zone+Zero_3.3.0_APKPure.xapk` reaches continuous `vkQueuePresentKHR presented 1280x720` (`492` frames by `t=140s`, `failed=0`, no crashes).

## Open Issues to Solve Next

### 1. Login screen stops responding (RTX 3080 follow-up)

The Vulkan input subscription defect is fixed and covered by the socket/input
queue integration test. Live traces show pointer button DOWN/UP arriving at the
Android input pump and dispatching to a root. The user sees the login screen but
reports that clicking does not work. Vulkan presentation stopped after 375
frames in this run; receiving input alone does not establish a responsive UI.
Investigate the stopped render thread / login overlay before claiming success.

The user supplied a screenshot of a transparent separate window over the game
and reports a transient loading indicator there. No consent text/buttons have
been observed, so do not classify it as a terms prompt or dismiss it blindly.
Two host-side defects are addressed in the follow-up: touch targeting now uses
FLAG_NOT_TOUCHABLE independently of keyboard FLAG_NOT_FOCUSABLE; removed/unadded
roots are excluded from input. The applied frame is read from the pinned
framework's actual `ViewRootImpl.mWinFrame` Rect. The old
`mClientWindowFrames` lookup did not exist in that framework and prevented frame
repair. `probe_window_configuration` now checks the actual frame and lifecycle
field contracts under CheckJNI. These fixes pass 149 CLI tests; the transparent
window still requires live confirmation after the restart.

### NVIDIA swapchain allocation — fixed

ZZZ requests SRGB format 43 and image usage `0x93`. The previous unconditional
`VK_IMAGE_TILING_LINEAR` allocation returned `VK_ERROR_FORMAT_NOT_SUPPORTED`
(-11) on RTX 3080 / driver 595.99.02, and Unity immediately called `exit_group(0)`.
The host reports no color-attachment support for linear RGBA/SRGB images.

The shim now enables the supported DRM modifier extension, enumerates one-plane
color-attachment modifiers, validates DMA-BUF export for the exact format/usage,
and creates images with a single validated modifier. The exported plane uses the
real modifier, row pitch and offset. Hosts without the extension retain the linear
path. On this NVIDIA host modifier `216172782120099861` allocates successfully;
ZZZ renders and the user confirms the login screen.

Hardware regression (requires Vulkan DMA-BUF support):

```sh
cargo test -p pengudroid-vkshim gpu_srgb_swapchain_exports_color_attachments -- --ignored --nocapture
```

This allocates and exports all three SRGB color-attachment swapchain images on
the real GPU, without requiring a compositor window. It passes on the RTX 3080.
Both Berberis `two-gear` and `lite-translate-or-interpret` failed before the image
fix; the translator mode was not the cause of that exit.

### 2. Slow Cold-Start Time
- **Observation**: Berberis (`libberberis_arm64.so`) translates ~350k–550k ARM64 basic blocks on cold start (`libil2cpp.so` `dlopen` ~17 s, total ~100–480 s to continuous rendering).
- **Investigation targets**:
  - Check `BERBERIS_MODE` / Berberis translation cache / AOT or profile-guided options.
  - Check if `DEBUG berberis: berberis: trans#...` stderr logging or `BERBERIS_FLAGS` impacts startup speed.

### 3. GPU Frame Pacing (`vkQueuePresentKHR`)
- **Observation**: `vkQueuePresentKHR` in `crates/pengudroid-vkshim/src/lib.rs` currently calls `vkQueueWaitIdle` / `vkDeviceWaitIdle` and then does a synchronous `present_dmabuf_to_daemon` (`send_message` + `recv_message` Ack) on every frame.
- **Investigation targets**:
  - Replace full `vkQueueWaitIdle` with fence/semaphore-backed synchronization per swapchain image (or async/pipelined present) so the CPU/translator thread does not stall waiting for both full GPU idle and daemon round-trip on every frame.
