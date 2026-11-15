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
merge; the added Vulkan input integration test also passes. A real ZZZ 3.3.0
run is under verification; reaching startup is not yet a gameplay claim.
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

### 1. Touch / Mouse Input Not Reaching Vulkan Games (High Priority)
- **Root cause already identified**:
  - `apphost.rs` (`spawn_input_pump`) polls `pengudroid_input_poll` (resolved via `dlsym(RTLD_DEFAULT, ...)` from `libpengudroid_gleshim.so`) and injects `MotionEvent` into `ViewRootImpl.dispatchInputEvent`.
  - However, `subscribe_input(surface_id)` in `crates/pengudroid-gleshim/src/lib.rs` (line 1208) is only called when **gleshim** creates a window surface!
  - In Vulkan apps like ZZZ, **vkshim** (`ensure_daemon_surface_id` in `crates/pengudroid-vkshim/src/lib.rs`) creates the daemon surface (`Message::CreateSurface`), while `gleshim` never creates a window surface — so `Message::SubscribeInput { surface_id }` is never sent and `INPUT_QUEUE` stays empty.
- **Fix direction**:
  - Export/call a shared input subscription hook (e.g. `pengudroid_subscribe_input(surface_id)` in `gleshim`, or move/mirror `SubscribeInput` into `vkshim` feeding `pengudroid_input_inject_*` / a shared input queue, and also set `PENGUDROID_LAST_SURFACE_ID` from `vkshim::ensure_daemon_surface_id` for resize-following).
  - Verify `PENGUDROID_TRACE_INPUT=1` shows `PointerDown` / `PointerUp` / `PointerMotion` reaching `ViewRootImpl.dispatchInputEvent`.

### 2. Slow Cold-Start Time
- **Observation**: Berberis (`libberberis_arm64.so`) translates ~350k–550k ARM64 basic blocks on cold start (`libil2cpp.so` `dlopen` ~17 s, total ~100–480 s to continuous rendering).
- **Investigation targets**:
  - Check `BERBERIS_MODE` / Berberis translation cache / AOT or profile-guided options.
  - Check if `DEBUG berberis: berberis: trans#...` stderr logging or `BERBERIS_FLAGS` impacts startup speed.

### 3. GPU Frame Pacing (`vkQueuePresentKHR`)
- **Observation**: `vkQueuePresentKHR` in `crates/pengudroid-vkshim/src/lib.rs` currently calls `vkQueueWaitIdle` / `vkDeviceWaitIdle` and then does a synchronous `present_dmabuf_to_daemon` (`send_message` + `recv_message` Ack) on every frame.
- **Investigation targets**:
  - Replace full `vkQueueWaitIdle` with fence/semaphore-backed synchronization per swapchain image (or async/pipelined present) so the CPU/translator thread does not stall waiting for both full GPU idle and daemon round-trip on every frame.
