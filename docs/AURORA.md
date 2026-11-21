# Experimental Aurora OS / AArch64 port

This tree contains an experimental Aurora OS / AArch64 port. The ARM64 ART VM,
NewPipe's Application, a minimal View Activity, and a Jetpack Compose Activity
initialize under QEMU. Both fixtures render through HWUI/EGL and present through
shared memory in the headless test. Compose button clicks and text input also
update the rendered UI. On a physical Dialog-S the same Compose Activity renders
into a visible Lipstick `wl_shell` window, and real Wayland/audb pointer input
changes its button counter. Vulkan has not been tested.

## Implemented in this stage

- The complete Rust workspace, including the Slint GUI, cross-checks for
  `aarch64-unknown-linux-gnu`.
- Release binaries and shim libraries cross-link against an Aurora AArch64
  sysroot. The resulting CLI runs under QEMU with that sysroot.
- `tools/prepare-android17.py --arch arm64` downloads/validates the pinned ARM64
  Android 17 GSI and prepares `android17-arm64`; x86_64 remains the default.
- Runtime inspection recognizes `framework/arm64` and AArch64 ART ELF files.
  Cross-architecture execution is rejected rather than attempted accidentally.
- APK native-library selection uses the host ABI (`arm64-v8a` on AArch64), and
  Android CPU properties match the host runtime architecture.
- `tools/build-aurora-rpm.sh` creates an unsigned AArch64 RPM without AppImage,
  FUSE, a bundled Android image, Python, or Berberis.
- GUI, CLI, and daemon share one multi-call executable. Its CLI mode and daemon
  start/status/stop lifecycle have also been exercised under QEMU.
- An AArch64 NativeActivity fixture completes its lifecycle and render loop
  under QEMU, including EGL initialization and shared-memory presentation.
- A pinned Android 17 ARM64 GSI has been prepared offline. ARM64 `libart.so`
  loads, `JNI_CreateJavaVM` returns `JNI_OK`, and the framework native groups
  register successfully.
- AArch64 Bionic jump buffers, 16-byte `sem_t`, and 40-byte `pthread_mutex_t`
  are translated without writing glibc's larger objects into guest memory.
  Condition waits use the corresponding host mutex, including recursive and
  error-checking static initializers. This fixed the former heap corruption.
- NewPipe now completes `handleBindApplication`: its `App` class, ACRA,
  Room/WorkManager, resources, and main Looper initialize. Its MainActivity
  reaches creation but currently stops in the native skia/png path; this is a
  separate failure from the completed framework bind.
- `fixtures/simple-activity` builds a dependency-free Java APK with one
  `TextView`. On AArch64 the framework's `performLaunchActivity` is used
  directly because the full `handleLaunchActivity` prefix stalls. The fixture
  reaches `onCreate`, content-view installation, `onResume`, and
  `onAttachedToWindow`, creates a valid BLAST surface, completes the first
  `ViewRootImpl` traversal, and presents EGL frames with `PresentShm`. Three
  timed text/background updates produce four swaps and distinct rendered frames
  in both 720x1280 and 1280x720. `PENGUDROID_DUMP_FRAME_AT=0` or
  `PENGUDROID_DUMP_FRAME_EVERY=1` verifies the actual pixels.
- `fixtures/compose-activity` builds a multi-dex AndroidX/Jetpack Compose APK
  with a Material 3 text field and button. Under AArch64 QEMU it completes the
  Activity lifecycle and renders through EGL. A synthetic button tap changes its
  counter, and synthetic key input updates both the field and its reflected
  text; the resulting pixels have been checked.
- A physical Dialog-S (Aurora 5.2.1.200, AArch64) was exercised through audb.
  A separately signed test package with package-owned prepared ART libraries
  reaches `JNI_OK`, native registration, Compose `onCreate/onResume`, BLAST and
  the main Looper. The Hybris loader recursion is fixed by loading host EGL and
  GLES into one `dlmopen` namespace; generated AArch64 tail-call forwarders
  expose the host GLES API to HWUI. Hosted display/timestamp/vsync entry points
  avoid mixing the two Android binder stacks on RenderThread.
- Lipstick does not provide `xdg_wm_base`; the daemon now falls back to its
  `wl_shell` v1. Compose is visibly rendered at 1080x2460 through `PresentShm`,
  survives continuous frames, and an `audb tap` increments the button counter.
  Verified screenshots are `target/aurora/device-compose-wl-shell.png`,
  `target/aurora/device-compose-audb-input.png`, and the pixel dump
  `target/aurora/device-compose-interactive.png`.
- MAX 26.40.1 (`ru.oneme.app`, `one.me.android.MainActivity`) also runs on the
  phone from the original, unmodified APK. Its vendor manifest explicitly sets
  `android:enabled="false"` on DpsInitProvider; the manifest parser now honours
  that flag instead of incorrectly invoking the disabled provider. The earlier
  no-DPS diagnostic copy is no longer needed for startup.
  `PENGUDROID_NATIVE_LIB_DIR` supports an exec-capable package-owned library
  directory, and a typed `InstallSourceInfo` reply fixed its PackageManager
  failure. Bionic condition variables now use aligned host objects: their size
  already matched glibc, but Bionic's 4-byte alignment caused BUS_ADRALN in
  RenderScript helpers on AArch64. Host libpng16's 312-byte jmpbuf capacity is
  adapted from the guest's 256-byte PNG size while non-local jumps still use
  the Bionic longjmp boundary. MAX renders its login screen through EGL/wl_shell,
  survives thousands of presents, and an `audb tap` on the country flag opens
  the interactive country selector. Screenshots: `max-visible-fixed.png` and
  `max-country-selector.png` under `target/aurora/`.
- Host and guest pthread ABIs are separated by ELF symbol version on AArch64.
  Unversioned/default-`LIBC` Android calls use Bionic translation, while host
  Mesa/GLVND/LLVM `GLIBC_2.17`/`2.30`/`2.34` calls go directly to glibc. This fixes the
  former RenderThread deadlock in host LLVM during `eglInitialize`.

## Build

Requirements on the build host:

- Rust toolchain 1.93 with the `aarch64-unknown-linux-gnu` standard library;
- `aarch64-linux-gnu-gcc` and `aarch64-linux-gnu-ar`;
- `rpmbuild`;
- an Aurora Platform SDK AArch64 target (automatically discovered below
  `~/AuroraPlatformSDK-*`), or an explicit `--sysroot`;
- `ffmpeg` is optional and only generates the four launcher icon sizes.

```sh
rustup target add --toolchain 1.93 aarch64-unknown-linux-gnu
tools/build-aurora-rpm.sh
```

To select a particular SDK target:

```sh
tools/build-aurora-rpm.sh \
  --sysroot "$HOME/AuroraPlatformSDK-5.2.0.180/targets/AuroraOS-5.2.0.180-base-aarch64"
```

The RPM is written under:

```text
target/aurora/rpm/RPMS/aarch64/
```

It is unsigned. Use Aurora's normal signing flow before installation. Basic
QEMU smoke testing on the build host:

```sh
SYSROOT="$HOME/AuroraPlatformSDK-5.2.0.180/targets/AuroraOS-5.2.0.180-base-aarch64"
qemu-aarch64-static -L "$SYSROOT" \
  target/aarch64-unknown-linux-gnu/release/pengudroid --version
```

## Prepare the ARM64 Android runtime

Preparation currently stays on the development host because it needs Python,
`debugfs`, `fsck.erofs`, and at least 12 GiB free space. Review Google's GSI
license before obtaining the image; the RPM does not redistribute it.

```sh
python3 tools/prepare-android17.py --arch arm64 --download --cache
```

The verified tree is placed at
`$XDG_CACHE_HOME/pengudroid/runtime/android17-arm64`, or under
`~/.cache/pengudroid/runtime/android17-arm64`. For an on-device experiment it
must be transferred to the equivalent application-accessible location or
selected explicitly with `PENGUDROID_SYSROOT`. Native on-device provisioning
without the Python helper remains to be implemented.

Inspect a prepared tree with an AArch64 build:

```sh
pengudroid sysroot /path/to/sysroot-android17-arm64
```

## NativeActivity and graphics

The existing `fixtures/native-game` has been built for AArch64 and run through
the multi-call executable under QEMU. Its activity creation, EGL setup, frame
production, and clean shutdown pass. Set `PENGUDROID_FORCE_SHM=1` on drivers
where dma-buf export is broken or unavailable: the GLES shim then reads the
current FBO and uses the daemon's `PresentShm` protocol instead of
`PresentDmabuf`. This is an explicit CPU fallback; the default zero-copy path
is unchanged.

QEMU's test-only Mesa 23.2 surfaceless driver crashes inside
`eglExportDMABUFImageMESA`, so SHM was required for this smoke test. The test
Mesa packages live under `target/` and are **not included in the RPM**. Hardware
EGL/GBM/dma-buf behavior still has to be checked on each real device.

## RPM validation status

Run:

```sh
aurora-cli psdk --validate "$PWD/target/aurora/rpm/RPMS/aarch64/io.github.denizomer10.PenguDroid-0.2.1-1.aurora.aarch64.rpm"
```

The PSDK 5.2.0.180 validator was run directly in a `bwrap` PSDK/target mount
namespace (the `aurora-cli` wrapper can return zero even when `sb2` rejects its
target). Its current `harbour` suite still returns **FAILED** for the regular
package: the pre-existing package name, `1.aurora` release and APK MimeType
`Provides` do not match that suite. The combined development package also
reports the bundled Android libraries' link/location errors. The desktop entry
itself passes the relevant sandbox check; in particular, a bare
`launch MaxNext.apk` removed the former hardcoded `/home/defaultuser` error.
These suite results must not be reported as successful validation. The signed
combined test RPM is nevertheless accepted by the device's APM profile.
Full logs are kept under `/tmp/maxnext-*-rpmvalidation.log`.

The single executable accepts ordinary CLI arguments, `--cli ...`, and the
hidden `__daemon` entry point. Only private `.so` files remain in its library
directory. The launcher currently declares `X-Nemo-Application-Type=silica-qt5`
for Lipstick compatibility even though the manager itself is not QML/Silica.

## Device provisioning notes

Aurora denies executable mappings for pushed user libraries, and its home mount
is `noexec`. `PENGUDROID_ART_LIBS=/absolute/package/lib` lets `art-test` reuse a
read-only, already de-bionic'd library directory containing `libart.so` instead
of copying libraries into an unexecutable scratch directory. For an APK's own
native code, `PENGUDROID_NATIVE_LIB_DIR=/absolute/package/lib` similarly selects
an exec-capable directory; its ARM64 libraries still need de-bionic/version
processing. APK files used as
DEX classpaths must be read-only (`chmod 0444`), otherwise ART aborts with
`Writable dex file ... is not allowed`.

`PENGUDROID_EGL_ONLY=1` omits vkshim and host Vulkan from `LD_PRELOAD`.
`PENGUDROID_LAZY_HOST_GL=1` and `PENGUDROID_HOST_GL_NAMESPACE=1` remain explicit
diagnostic switches. On Aurora they are no longer required: Hybris detection
automatically selects isolated host GL and lazy loading. SurfaceFlinger's SDR
properties are set through libhybris-common, and HWUI's optional disk shader
cache is disabled on this path. The usual non-isolated QEMU/Mesa render still
passes.

The regular application RPM still contains neither GSI nor the test Mesa stack.
The combined package used for the phone experiment is generated under `target/`
and is not the regular distributable artifact.

## MaxNext desktop launcher on the phone

The desktop entry now executes the multi-call binary as:

```ini
Exec=io.github.denizomer10.MaxNext --cli launch MaxNext.apk --backend art --width 1080 --height 2460 --signature-apk /opt/app/io.github.denizomer10.MaxNext/current/data/apps/MaxOriginal.apk --log MaxNext.launch.log
```

A bare APK name is resolved from the package's read-only
`/opt/app/io.github.denizomer10.PenguDroid/current/data/apps/` directory. The
combined development package supplies the original `MaxNext.apk`, prepared
ART libraries and MAX's processed ARM64 libraries; `~/max_app.apk` is not
changed. PackageManager replies contain the real certificate chain obtained
through Android's ApkSignatureVerifier instead of an empty SigningInfo. An
optional `--signature-apk` remains available for explicitly diagnostic,
modified APK copies, but is not needed for the original MAX package. This also
fixed CallsSdkInitializer.initializeSessionSeed's GetArrayLength(null) abort.
The original APK now logs `skipping disabled provider
ru.trace_flow.dps.internal.DpsInitProvider`, verifies one scheme-3 certificate,
and reaches the interactive login screen without a DPS patch. Evidence:
`target/aurora/maxnext-dps-final.log` and
`target/aurora/maxnext-dps-final.png`. Server acceptance or completion
of phone authentication is not implied. Runtime files for this device test live in
`~/Documents/PenguDroid/runtime/android17-arm64`, accessible through the
existing `Documents` permission. Private prefixes use Aurora's
`~/.local/share/io.github.denizomer10/PenguDroid` and
`~/.cache/io.github.denizomer10/PenguDroid` directories. The launcher log is
written to `/srv/shared/io.github.denizomer10/PenguDroid/cache/MaxNext.launch.log`.

On Hybris/AArch64 the GLES shim loads `libhybris-common.so.1` first and sets
`ro.surface_flinger.has_wide_color_display=false`,
`ro.surface_flinger.has_HDR_display=false`, and
`ro.surface_flinger.use_context_priority=false` **before** loading host
EGL/GLES. This is required because libEGL's initializer can query configstore
immediately; setting the properties after `dlopen(libEGL)` was too late and
caused `Process may not make oneway calls` on RenderThread. The launcher also
calls `/usr/bin/setprop` directly when that executable is visible (manual SSH
launch). Sailjail's private-bin hides it from desktop launches, so cold-boot
property provisioning inside the sandbox still needs hardening; the manual
three-property command remains the verified recovery if they are empty after
a reboot. The ordinary non-Hybris/QEMU path is unchanged.

The isolated Hybris namespace also used to load its own Bionic `libc.so`.
Bionic and glibc independently allocated the same pthread key; on MAX's
`OneVideoRenderT`, Hybris `eglGetError` consequently received Android
libbinder's `IPCThreadState*` as its EGL error record and overwrote `mProcess`
with `EGL_SUCCESS`. A later `flushCommands()` crashed. The development package
now loads `libpengudroid_hybris_pthread.so` before `libhybris-common`: its five
Bionic `LIBC` pthread-key/once exports branch to explicitly versioned
`GLIBC_2.34` implementations, so both graphics and Binder use glibc's single
key allocator. No EGL API is intercepted by this bridge. GDB watchpoint
evidence is `target/aurora/maxnext-video-gdb-object4.out`; the fixed 90-second
run is `target/aurora/maxnext-hybris-pthread-fixed.log`, with live ART/daemon,
continuing presents, and no `OneVideoRenderT` fatal.

Launch the package normally:

```sh
audb launch io.github.denizomer10.PenguDroid
```

Package launches currently disable the unavailable Codec2 service so MAX's
background `MediaCodecList` discovery cannot dereference a null platform store.
The launcher creates one Wayland window immediately and the daemon animates a
loading spinner until ART presents. `PENGUDROID_SURFACE_ID` attaches HWUI to
that surface; attach establishes both present and input channels and transfers
surface ownership from the launcher to the ART process. Real `wl_touch`
down/up events are translated to Android `ACTION_DOWN`/`ACTION_UP`. A tap on the
country control now opens the actual country selector, rather than merely
showing a ripple. A repeated `audb launch` returns `ApplicationAlreadyRunning`
and does not create another daemon/window.

Evidence: `target/aurora/maxnext-loading-early.png` (loading placeholder),
`target/aurora/maxnext-owner-country.png` (real touch navigation), and
`target/aurora/maxnext-owner-input.log` (Wayland touch and Android actions).

## SHM performance and dma-buf status

`PENGUDROID_PROFILE_SHM=1` reports per-second guest readback, copy and IPC
timings. Before optimization a 1080×2460 MAX frame typically took 39–53 ms:
readback 6 ms, guest copy 10–16 ms, daemon/IPC 22–30 ms (about 17–19 FPS).
Full-frame RGBA `PresentShm` now imports the guest memfd directly as a
`wl_buffer` and retains its fd until `wl_buffer.release`; crop, resize and video
conversion still use the snapshot path. This reduced daemon/IPC to roughly
5–8 ms and the measured frame rate to about 20–24 swaps/s. The installed RPM
was launched through audb and touch still opens the country selector:
`target/aurora/maxnext-direct-shm-country-final2.png`.

The gleshim also tries the ABI-compatible `eglExportDMABUFImage*EXT` entry
points after the MESA ones. This device's Mali/Hybris EGL advertises neither:
`dma_query=false`, `dma_export=false`. Lipstick publishes neither
`zwp_linux_dmabuf_v1` nor legacy `wl_drm`, so the usual dma-buf transport is
unavailable on this device.

A separate native path solves this without CPU readback. The Aurora AArch64
package launcher detects Hybris and enables `PENGUDROID_GRALLOC=1` and
`PENGUDROID_GRALLOC_SHM=1` by default. An explicit `PENGUDROID_FORCE_SHM=1`
selects the readback fallback instead, while `PENGUDROID_FORCE_DMABUF=1`
preserves the ordinary dma-buf attempt. Manual non-package launches can still
set the two gralloc variables themselves. QEMU and ordinary Linux behavior is
unchanged.

In this mode gleshim allocates a linear BGRA Hybris gralloc buffer with
SW_READ_OFTEN, wraps it as an EGLImage texture, and renders HWUI into its FBO. It selects the mmap-able native fd, converts the
gralloc stride from pixels to bytes, and presents that fd through `wl_shm`.
Lipstick ignores wl_shell buffer transforms, so a second gralloc buffer and
`glBlitFramebuffer` perform the Y-flip entirely on the GPU. The daemon imports
the fd directly as an `Xrgb8888` `wl_buffer` and retains it until release. No
`glReadPixels` or CPU frame copy occurs in this path.

The audb-launched result has correct orientation and colours, reaches about
42 FPS on the tested MAX login screen, and touch opens the country selector.
Evidence: `target/aurora/maxnext-gralloc-flip.png` and
`target/aurora/maxnext-gralloc-zerocopy-country.png`. The standalone desktop
RPM was also launched without any manual graphics environment: it selected
gralloc automatically, rendered the MAX login screen and sustained about
32–34 FPS (`target/aurora/maxnext-default-gralloc.png` and
`target/aurora/maxnext-default-gralloc.log`). Gralloc remains experimental and
specific to the detected Hybris device path; ordinary dma-buf and explicit
readback fallback remain available. Full resolution and vsync are retained.

`PENGUDROID_PROFILE_GRALLOC=1` adds per-second timing. On the measured MAX
login screen the sustained rate is 32–34 FPS: `glFinish` after HWUI takes about
13.0 ms, GPU Y-flip 3.7 ms, fd present/ACK 1.4 ms, and daemon
pool/buffer/commit 0.3–0.4 ms. Guest `eglSwapBuffers` itself takes about 21 ms;
the remaining inter-frame time is outside that function. One-time
allocation/EGLImage/texture setup varies from 1.4 to 14 ms per surface. Full
output: `target/aurora/maxnext-gralloc-profile.log`.

An alternative full-screen GLSL flip is available through
`PENGUDROID_GRALLOC_SHADER_FLIP=1`. It renders correctly but does not improve
performance: blit is about 3.7 ms versus 3.9 ms for the shader, with the same
32–34 FPS. Shader setup takes about 4.6 ms once. Blit therefore remains the
default and fallback. Comparison log: `target/aurora/maxnext-shader-flip-profile.log`.

## Lipstick extended-window integration

The daemon optionally binds `qt_surface_extension` and creates a
`qt_extended_surface` after the ordinary xdg/wl_shell role. It configures all
orientation bits, zero window flags, `BACKGROUND_VISIBLE=true`,
`WINDOW_OPACITY=1.0` and the Wayland `WINID` using QDataStream/QVariant
big-endian encoding. `Close` enters the existing surface/guest close path;
`OnscreenVisibility` and generic-property events are diagnosed. Extended objects
are dropped before their owning `wl_surface`. `STATUSBAR_VISIBLE=true` is sent
at creation and again when the window becomes visible. After the ordinary
`wl_shell_surface.set_toplevel`/`xdg_surface.get_toplevel` role and the first
configure/attach/commit cycle, the daemon sends `qt_extended_surface.raise` and
flushes it with a roundtrip. This prevents Lipstick from leaving a newly
launched Android app visible only as a small cover. Compositors without the
global keep the existing xdg/wl_shell behavior.

Lipstick draws the system bar, while Android receives a separate top inset via
`WindowInsets.Builder` once per root View. On Dialog-S the dconf statusbar and
cutout heights are both 132 px; `PENGUDROID_STATUSBAR_HEIGHT` overrides this
value and zero disables the inset. Dispatch alone is insufficient for
edge-to-edge Compose: after the app-driven layout, the host checks the real
`android.R.id.content` global top and reserves only the missing space on
DecorView. Waiting for layout avoids double-insetting traditional Views such as
PSB. The real desktop-launch paths for MAX and Sberbank now record
`reserved missing status-bar space after layout: 0px -> 132px`; PSB already lays
out at 132 px and receives no extra DecorView padding. Evidence:
`target/aurora/maxnext-statusbar-after-layout.png`,
`target/aurora/psbmobile-statusbar-after-layout.png`, and
`target/aurora/sberbank-statusbar-after-layout.png`.

When that fallback padding is necessary, the host also clones the app's own
full-width top background drawable onto DecorView before applying the final
padding. This keeps the safe area visually part of the app instead of exposing
a black framework window. Applying the background last was also tested and
rejected because drawable padding could reset the safe-area padding. On MAX's
main UI the old band sampled RGB (0,0,0); the fixed band and adjacent app
background both sample (23,24,28), while controls remain below Lipstick.
Evidence: `target/aurora/maxnext-status-app-background2.png` and `.log`.

The phone logs `qt_surface_extension enabled`, `surface 1 has
qt_extended_surface`, and `onscreen visibility=true`; MAX remains visible and
touch opens the country selector. See
`target/aurora/maxnext-qt-extended-device.png` and
`target/aurora/maxnext-qt-extended-country.png`. The protocol XML retains the
Qt BSD copyright in `crates/pengudroid-daemon/protocols/surface-extension.xml`.

## Focus-driven Maliit keyboard

The input pump checks the focused Android View with `findFocus()` and
`onCheckIsTextEditor()` every 500 ms and after window-focus changes. On an actual
text-focus transition it sends protocol-v8 `SetImeState`; there is no show call
on every redraw. The daemon lazily opens the persistent Maliit P2P connection at
`$XDG_RUNTIME_DIR/maliit-server/dbus-socket`, registers
`com.meego.inputmethod.inputcontext1`, and calls `activateContext`, orientation,
`updateWidgetInformation`, `showInputMethod`; focus loss and surface teardown
call `hideInputMethod`. Orientation is currently portrait angle 0.

The device log records `text input focus=true` and `IME visible=true`, and the
screenshot shows the real system keyboard:
`target/aurora/maxnext-ime-focus.png`. Protocol v9 carries commit, preedit and
finish-composing events without logging their contents. Android delivery uses
the focused View's `InputConnection`: `commitText`, `setComposingText` and
`finishComposingText`. Events are queued by the input pump and applied only on
the main Looper thread, with an explicit Handler wakeup; this avoids a
DynamicLayout race seen in the first direct-thread prototype. Maliit `keyEvent`
uses its actual seven-argument signature, and special keys take precedence over
control text. Actual device tests entered `123` and then verified Backspace
(`12` followed by Backspace produced `1`). Evidence:
`target/aurora/maxnext-textinput-main-123.png` and
`target/aurora/maxnext-textinput-backspace-verified.png`. Preedit is implemented
but was not exercised by this numeric keyboard test.

Maliit visibility also updates Android's real `WindowInsets.Type.ime` bottom
inset and visibility flag. The current portrait panel occupies the lower 3/8
of the Wayland surface (923 px at 2460 px); `PENGUDROID_IME_PANEL_TOP_PX`
overrides its top for other layouts. After app layout, only unconsumed bottom
space is added to DecorView and the host-added amount is removed on hide.
Status-bar dispatch remains root-initial only: repeating it with every IME
update made Compose add a second top inset. Device verification showed MAX
search ending at y=1537 immediately above Maliit without extra top space, then
returning to full-height content after Back. Evidence:
`target/aurora/maxnext-safearea3-ime.png` and
`target/aurora/maxnext-safearea3-hidden2.png`.

Kernel touchscreen slots are mapped to stable Android MotionEvent pointer IDs
1..31 (ID 0 remains the compositor/mouse pointer) and recycled after ACTION_UP.
The previous `100 + slot` mapping produced pointer ID 101 and aborted
VelocityTracker on the SMS-code screen. A subsequent original-APK device run
recorded simultaneous valid pointerId 0 and pointerId 1 events without a fatal
error or guest exit. Regression coverage also checks allocation, stability,
recycling and the 31-pointer hardware bound. Evidence:
`target/aurora/maxnext-pointer-fixed.log`.

Per-motion logging was also removed from the default device path: the previous
run wrote 13,878 VelocityTracker lines (73% of the entire log), which stalled
touch processing. VERBOSE/DEBUG now require `PENGUDROID_LOG_DEBUG=1`, and noisy
VelocityTracker messages below ERROR are suppressed unless that option is set.
The comparable low-latency run wrote 1,307 lines total and zero VelocityTracker
lines; INFO/WARN/ERROR/FATAL from other components remain enabled. Evidence:
`target/aurora/maxnext-low-latency.log`.

CLOCK_MONOTONIC instrumentation then showed that transport plus
dispatchInputEvent was normally only 1–15 ms, but every contact was dispatched
twice: once from evdev and once from wl_touch, with different coordinates and
Android pointer IDs. When a real hardware reader exists, evdev is now the single
multitouch source and compositor touch is deduplicated; the Wayland fallback is
unchanged where no hardware reader exists. The deduplicated device run has one
kernel pointer stream and no duplicate id=0 dispatch, while tap/swipe dispatch
returns in a few milliseconds. Timing logs are opt-in through
`PENGUDROID_TRACE_INPUT_TIMING=1`. Evidence:
`target/aurora/maxnext-dedup.log` and `target/aurora/maxnext-dedup.png`.

For visual touch/latency debugging, `PENGUDROID_TOUCH_DEBUG=1` renders the
latest contact directly into the Android surface FBO before eglSwapBuffers.
The pressed indicator is an orange ring/crosshair; release turns it cyan for
750 ms. It uses the existing state-preserving glScissor/glClear overlay path,
so Android shaders, textures and VAOs are untouched, and normal launches have
no overlay. Device screenshots: `target/aurora/maxnext-touch-debug-pressed.png`
and `target/aurora/maxnext-touch-debug-released.png`.

The touch overlay is frame-driven: its 750 ms release timeout is checked only
on a subsequent eglSwapBuffers. If no new frame is submitted, the last presented
pixels (including the old ring) can remain visible longer than 750 ms; this is
a diagnostic-overlay limitation, not a measurement of touch latency. The pale
filled circle seen beside the ring is not drawn by the overlay implementation
and must not be attributed to MAX versus the compositor without separate proof.

`PENGUDROID_TRACE_INPUT_TIMING=1` now also correlates the latest compositor
contact with the next eglSwapBuffers and PresentShm reply, and logs every swap
gap plus glFinish/flip/present costs. A 3718-frame MAX run with tap/swipe had:
swap-gap median 30 ms, maximum 83 ms, no gaps over 100 ms; touch-to-swap median
10 ms, maximum 29 ms; touch-to-PresentShm median 31 ms, maximum 54 ms.
glFinish was about 13 ms median, flip 4 ms, and PresentShm 1 ms. Thus this run
has no multi-second application/render stall; it does not measure physical
scanout and does not rule out a different stall during real-finger use.
Evidence: `target/aurora/maxnext-frame-trace.log` and
`target/aurora/maxnext-frame-trace-summary.txt`.

Swap timing alone does not prove visual responsiveness: unchanged pixels can
be resubmitted at normal FPS. `PENGUDROID_TRACE_CONTENT=1` therefore hashes a
small country-name strip after glFinish. In the open country list, two 1 s
swipes had their first changed-content frame 3172 ms and 2358 ms after down,
after 89 and 106 unchanged frames respectively. The list then flung for
several seconds. This confirms delayed Compose/ART gesture processing rather
than multi-second input transport or presentation. The readback probe is
opt-in and can itself affect timing. Evidence:
`target/aurora/maxnext-list-content.log`,
`target/aurora/maxnext-list-after-swipe1.png`, and
`target/aurora/maxnext-list-after-swipe2.png`.

MAX normally runs with ART `-Xint`; interpreting Kotlin/Compose is a major
performance limitation. `PENGUDROID_ART_JIT=1` omits `-Xint` (including when set
in the app prefix's `pengudroid.env`). The former nterp failure was an implicit
null check at `nterp0_op_invoke_virtual+36`: ART's frame validations pass, but
the fault is not recovered through the generated-code handler. The AArch64
signal boundary now recognizes that exact PC, calls ART's own IsValidMethod and
IsValidReturnPc checks, and only then performs the stock arm64 NPE signal
recovery through art_quick_throw_null_pointer_exception_from_signal. Other
faults retain the normal handler chain. MAX survives repeated recovery and
renders with JIT; the measured Settings → Chats transition improved from about
810 ms to 490–530 ms, so residual route cost remains rather than being claimed
zero. `PENGUDROID_TRACE_NTERP=1` enables recovery diagnostics. Evidence:
`target/aurora/maxnext-nterp-recover5.log`,
`target/aurora/maxnext-jit-fixed.log`, and
`target/aurora/maxnext-jit-fixed-repeat.log`.

A controlled Chats-list scroll comparison used the same four 600 ms swipes on
the device. Interpreter-only execution delivered 30.2 swaps/s with a 34 ms
median frame gap; JIT delivered 33.2 swaps/s with a 30 ms median gap. JIT also
kept median touch-to-PresentShm near 31–35 ms. During scrolling RenderThread
used about 81% of one CPU core, main 56%, and a Mali worker 33%. In the
presentation part of the frame, glFinish took about 12 ms, full-surface Y-flip
3.4–3.6 ms, and PresentShm IPC about 1 ms. Raising the requested vsync rate to
60 Hz, using present-driven compatibility mode, and replacing the blit with
the existing shader flip did not produce a material FPS increase; those
experiments were removed. The clean JIT configuration remains at the normal
request-driven default. Evidence: `target/aurora/maxnext-chat-xint.log`,
`target/aurora/maxnext-chat-vsync60.log`,
`target/aurora/maxnext-chat-present-mode.log`,
`target/aurora/maxnext-chat-shader-flip.log`, and
`target/aurora/maxnext-chat-thread-cpu.raw`. Swap counts are not claimed as
unique visual frames; the opt-in content probe confirmed changing pixels but
its narrow readback strip is not a full-surface visual-FPS meter.

`PENGUDROID_GRALLOC_ASYNC=1` enables an experimental double-buffered gralloc
presentation path. It alternates two independent native output fd/FBO pairs,
issues the full-frame Y-flip without glFinish, creates a real host EGL fence,
and flushes it. A dedicated presenter thread performs eglClientWaitSync and
only then sends that buffer through PresentShm. At most two frames per EGL
surface may be outstanding; the wait for a free slot happens before the third
Y-flip so it cannot overwrite the oldest buffer. The worker receives only
metadata and an owned fd, never a SurfaceRec pointer. The final frame does not
need another swap to wake presentation. Fence/setup failure falls back to a
synchronous glFinish + native present; the async path adds no readback
fallback. The normal synchronous path remains the default.

On MAX Chats the async path delivered 70.6 swaps/s with a 14 ms median swap
gap and 1025/1025 successful fence-to-present operations. Median fence wait
was 12.1 ms off the RenderThread. Taking the first presented frame associated
with each input sequence gave about 30 ms median and 38 ms p90 touch-to-present
latency (later fling frames inherit the old sequence and are not new-input
latency measurements). Settings ↔ Chats and the final static list frame were
pixel-checked without a wake-up tap. Evidence:
`target/aurora/maxnext-async88-scroll.log`,
`target/aurora/maxnext-async88-scroll.png`,
`target/aurora/maxnext-async88-settings-final.png`, and
`target/aurora/maxnext-async88-chats-static.png`.

Follow-up latency checks used Settings specifically because it has no Chats-like
background redraw stream. With opt-in content readback (which itself perturbs
timing), two async/default-30 Hz gestures reached their first changed content
after 103 ms and 60 ms; the synchronous comparison took 103 ms and 188 ms.
Android input dispatch itself remained about 1–3 ms. A 60 Hz request timeline
was slower in this test (184/175 ms), and present-driven mode was decisively
rejected: it produced 1794 DisplayEventDispatcher errors and an 1892 ms
content response. Production therefore retains request-driven 30 Hz timing,
not the 60/120 Hz or present-mode experiments. Evidence:
`target/aurora/maxnext-settings-async30.log`,
`target/aurora/maxnext-settings-sync30.log`,
`target/aurora/maxnext-settings-async60.log`, and
`target/aurora/maxnext-settings-async-present.log`.

The larger scroll-latency root cause was the vsync producer/consumer mismatch:
the broker emitted free-running 60 Hz DisplayEvents while the hosted HWUI
pipeline rendered about 30 FPS. Surplus events left Compose processing stale
frame timelines; the old run accumulated 657 DisplayEventDispatcher warnings.
A pure one-shot experiment stalled MAX (it does not continually issue
requestNextVsync), and SIOCOUTQ cannot inspect AF_UNIX seqpacket queues, so
neither experiment is retained. Instead `PENGUDROID_VSYNC_INTERVAL_US`
configures one consistent interval for the producer, event timeline and latest
vsync reply. Aurora/Hybris packages now default to 33333 us unless explicitly
overridden; QEMU and ordinary Linux keep the previous default.

With explicit 30 Hz vsync, two diagnostic swipes changed content after 14 ms
and 39 ms instead of 3172/2358 ms. With the automatic package default and no
PENGUDROID_VSYNC_INTERVAL_US override, two subsequent swipes changed content
after 281 ms and 290 ms, still eliminating the multi-second stall. The automatic
run had 41 dispatcher warnings over the whole log and no crash. Content hashes
measure pixel changes, not a semantic proof that every change is scroll rather
than a ripple, so the exact sub-frame/ripple components need separate analysis.
Evidence: `target/aurora/maxnext-vsync30.log`,
`target/aurora/maxnext-vsync-auto.log`, and
`target/aurora/maxnext-vsync-auto-after-swipes.png`.

The fixed 30 Hz producer was then replaced by true present-driven
backpressure. `eglSwapBuffers` calls
`__pengudroid_vsync_frame_presented()` only after successful presentation;
`PENGUDROID_VSYNC_ON_PRESENT=1` disables the free-running clock and emits one
following DisplayEvent per presented frame. The initial forced traversal and
requestNextVsync remain bootstrap paths. Explicit requests bypass the
producer's half-interval dedup: dropping one could freeze PIN/editor screens.
Present pulses are not gated on a new request because some MAX window
transitions do not issue one in time; gating them caused a one-frame regression.
Aurora/Hybris enables this mode by default while retaining the 33333 us
timeline interval and explicit overrides.
The explicit test had 3741 present notifications and zero dispatcher warnings.
The final automatic run had 3471 frames, zero dispatcher warnings, no crash,
and changed-content probes at 293/266 ms after down. Unlike the old
free-running modes, idle presentation no longer generates an independent vsync
backlog. Evidence: `target/aurora/maxnext-vsync-present.log` and
`target/aurora/maxnext-present-auto.log`.

The remaining PIN/static-page stall was later traced to a more fundamental
LIBGUI interposition bug: gleshim exported `BpDisplayEventConnection::
requestNextVsync` and `setVsyncRate` as aliases of a Status-only no-op stub.
Because libgui vtables are preemptible, native scheduling returned success
without sending `IDisplayEventConnection` request code 3. The hosted request
implementation now returns a real `binder::Status` and asks the PenguDroid
broker for a one-shot DisplayEvent directly. This avoids entering an
IPCThreadState on RenderThread after Hybris EGL has used the colliding Bionic/
glibc TLS slot. `PENGUDROID_VSYNC_ON_PRESENT=0` is the Aurora default; 1 retains
the previous present-pulse compatibility mode. On the device the hosted run
remained alive, PIN digits appeared without a second tap, and disabling the
very noisy vsync/gleshim traces restored the observed HUD rate from about 3
to 44 FPS. This verifies rendering and scheduling, not completion of MAX
authentication. The related `OneVideoRenderT → flushCommands` crash was the
same TLS collision. Since Hybris' internal DSO is not visible through the
wrapper's dlmopen handle, gleshim now finds the existing mappings through
`/proc/self/maps`, calls the real Bionic pthread functions by their verified
ELF offsets, and moves only libEGL's key to a separate Bionic slot (masked
index >= 16), migrating the current thread value. Android's key is unchanged.
The device accepted key 2 → 0x80000010, reached MAX's main UI and survived a
clean no-trace relaunch with ongoing frames and no new fatal signal.
Evidence: `target/aurora/maxnext-hosted-request.log`,
`target/aurora/maxnext-hosted-page-4s.png`, and
`target/aurora/maxnext-hosted-no-trace.png`.

A later Lipstick remap exposed a recovery case: the process stayed alive but
the last frame was old and the screen became black because pure on-present
had no producer to restart the loop. A bounded
`__pengudroid_vsync_input_wakeup()` now emits at most one pulse per 250 ms
when a real down arrives after at least 100 ms without a present. It does not
restart a free-running clock. Home/app-cover/resume was verified visibly;
`target/aurora/maxnext-wakeup-resumed.png` shows the restored UI.

MAX also has no root outside-click handler to clear editor focus. Optional
compatibility `PENGUDROID_CLEAR_FOCUS_ON_OUTSIDE=1` reads the focused editor's
global bounds on ACTION_DOWN, and defers View.clearFocus until after ACTION_UP
on the main Looper so the focus change cannot cancel the in-flight click. Aurora/Hybris enables it by default; an
explicit value of 0/false disables it. Verification:
`apphost: cleared text focus after outside tap`, `IME visible=false`, and the
full-height layout in `target/aurora/maxnext-outside-fix-after.png`.
The current MAX package default disables outside-tap hiding
(`PENGUDROID_CLEAR_FOCUS_ON_OUTSIDE=0`). Tapping an already focused editor
explicitly requests Maliit show; if host-initiated hiding left no focused
editor, a real editor hit-test is followed by `requestFocus()` on the main
Looper before showing IME. Statusbar dispatch is deduplicated by a global JNI
reference and `is_same_object`, not by the address of a temporary local
reference. Consequently repeated keyboard show requests do not repeatedly
apply the 132-px WindowInsets or DecorView fallback. Evidence:
`target/aurora/maxnext-statusbar-ime-final.log` contains one initial inset and
multiple subsequent IME show requests without another inset.

## Wayland output geometry and theme-derived Android density

Aurora package launches no longer hardcode 1080x2460 in their desktop entries.
Without explicit `--width/--height`, `CreateSurface(0,0)` selects the current
physical `wl_output.mode` with rotation applied. The launcher then reads the
actual `SurfaceGeometry` after the initial configure and exports those pixels
to Android. Explicit dimensions and `PENGUDROID_DISPLAY` remain overrides;
non-Aurora defaults are unchanged.

Independently, `dconf read /desktop/sailfish/silica/theme_pixel_ratio` is read
without modifying it, and Android gets
`densityDpi = round(theme_pixel_ratio * 1.5 * 160)` unless
`PENGUDROID_DENSITY_DPI` or `--density` is explicit. On Dialog-S the verified
values for MaxNext, PSB no-DGIS and Sberbank are Wayland 1080x2460, theme 1.75,
densityDpi 420 and DisplayMetrics.density/scaledDensity 2.625 (about 411x937
dp). Evidence: `target/aurora/maxnext-wayland-density.log`,
`target/aurora/psbmobile-wayland-density-nodgis2.log`, and
`target/aurora/sberbank-wayland-density.log`.

The same 1080x2460 / 420 dpi / 2.625 values are verified through the actual
Sailjail desktop-launch path for all three packages. Sailjail's `--private-bin`
hides the `dconf` executable, so the launcher uses `libdconf.so.1` and
`dconf_client_read()` in process instead of spawning `dconf`; the system setting
is still read-only. Evidence: `target/aurora/maxnext-sailjail-libdconf.log`,
`target/aurora/psbmobile-sailjail-libdconf.log`, and
`target/aurora/sberbank-sailjail-libdconf.log`.

The Aurora packaging script selects each package's own desktop file and source
icon (`maxnext.png`, `psbmobile.png`, or `sberbank.png`) before generating the
launcher sizes. A previous unconditional `maxnext.png` input was the reason all
three installed icons looked identical. `target/aurora/icons-home-after-fix.png`
shows the distinct MAX, PSB and Sberbank icons on their application covers.

MAX now uses the same theme-derived density as PSB and Sberbank; the temporary
`--density 320` override is removed. The former 420-dpi country route was created
correctly (visible, laid-out country views with alpha 1 and zero translation),
but its pixels remained stale only in the gralloc path. HWUI leaves its dirty
rectangle scissor enabled on some frames, and `glBlitFramebuffer` honors it.
The GPU Y-flip therefore copied only part of the frame. The flip now disables
`GL_SCISSOR_TEST` for its full-surface blit and restores the prior state; no CPU
readback is used. A real 420-dpi flag tap opens the populated country list with
correct flags and the 132-px statusbar/923-px IME safe areas. Evidence:
`target/aurora/maxnext-route420-realtap-open.png` and
`target/aurora/maxnext-route420-gralloc-fixed.log`.

Hardware evdev contacts which start inside the visible Maliit panel (the lower
3/8 of the display, with `PENGUDROID_IME_PANEL_TOP_PX` as an explicit override)
are not dispatched to Android. Gestures beginning above the panel retain their
move/up sequence. This prevents touches passing through the keyboard while
Maliit still receives the key. On the real country-list screen, a tap on `q`
produced exactly one IME commit and the expected Iraq/Qatar search results
without selecting an underlying country row:
`target/aurora/maxnext-final320-keyboard-filter.png`.

## Wayland clipboard

Plain-text clipboard support connects Android `IClipboard` to Lipstick through
`wl_data_device`. Copy creates a `wl_data_source` offering `text/plain`,
`text/plain;charset=utf-8`, `UTF8_STRING` and `pid/<pid>`; selection serials are
tracked from real touch, pointer-button and keyboard presses. Source `send`
uses bounded writes, while paste selects a supported offer, calls `receive` with
a pipe, closes the local write end and reads with a 500 ms timeout. Selection
changes invalidate the read cache. Empty selections, invalid UTF-8 and oversized
payloads are handled without logging clipboard contents.

Android 17 operations include set/clear/get primary clip, description, has/hasText
and listener registration. The bounds-checked Parcel codec handles the native
interface header, typed-object presence, ClipDescription and plain CharSequence
items. Exact request ClipData bytes are retained for a subsequent get, preserving
label/timestamp/item layout byte-for-byte. Maliit Copy/Cut/Paste are explicit
protocol-v11 actions applied on the main Android Looper.

The opt-in `PENGUDROID_TEST_CLIPBOARD=1` smoke test uses the real Android
ClipboardManager set/get API. On the phone it reported a successful roundtrip,
selected the Wayland source, and Maliit Paste inserted `123` into MAX. Final
screenshot: `target/aurora/maxnext-clipboard-final.png`; log:
`target/aurora/maxnext-clipboard-final.log`. Status-bar/inset and IME changes
are deferred to the main thread, with no wrong-thread or DynamicLayout errors in
that run. Only plain text is currently supported; images, arbitrary MIME data
and primary-selection protocol remain future work.

## Android network smoke test

`PENGUDROID_TEST_NETWORK=1` starts a one-shot background diagnostic after the
first Android window appears. It uses the app's real Context and Android APIs to
check three distinct layers: `ConnectivityManager` active-network capabilities,
`InetAddress` DNS resolution, and an HTTPS GET to the fixed public
`https://www.google.com/generate_204` endpoint. Only capabilities, DNS success
and HTTP status are logged; application traffic is not captured. The UI thread
is not blocked. The flag is opt-in and has no effect during ordinary launches.

On Dialog-S the Android runtime reported INTERNET and VALIDATED capabilities,
successful DNS, and HTTPS status 204. This proves working TCP/TLS/HTTP, not just
the synthetic connectivity reply. The diagnostic also exposed and fixed Bionic
`getnameinfo` routing: numeric address/service formatting now uses local
`inet_ntop` formatting instead of an unnecessary reverse-DNS lookup. Result log:
`target/aurora/maxnext-network-final.log`.

## Sberbank Online device probe

The user APK is `~/SberbankOnline.apk` (there is no `~/SberOnline.apk`), package
`ru.sberbankmobile` 17.15.0, with a launcher alias to `MainContainerActivity`,
15 DEX files and 32 ARM64 libraries. The original APK remained unchanged; a
read-only copy was pushed to the phone and de-bionic/version-stripped libraries
were added only to the development combined RPM under `target/`.

This probe fixed four generic launch issues: `<queries><provider>` entries no
longer require a component `android:name`; early ART helper provisioning now
honours `PENGUDROID_NATIVE_LIB_DIR` instead of always selecting noexec `/tmp`;
direct SSH launch falls back to a runtime-dir lock only when the privileged
shared-lock directory cannot be created; and Bionic `__fgets_chk` now uses its
correct `(buf, size, stream, buf_size)` argument order. The last fix removes a
libjpeg `/proc/cpuinfo` SIGSEGV caused by passing 0x400 as a glibc `FILE*`.

After these fixes `SbolApplication` and `MainContainerActivity` bound, launched,
resumed and rendered the visible Sber welcome/login screen. Screenshot:
`target/aurora/sberbank-after-fgets.png`; log:
`target/aurora/sberbank-after-fgets.log`. One run later called `System.exit(1)`
and closed its window (`sberbank-login-tap.png`); this was not proven to be a
button action. DEX analysis identifies the direct status-1 caller as
`nl/r.uncaughtException`: the Crashlytics branch calls System.exit only when the
default uncaught-exception handler is null. PenguDroid lacked RuntimeInit's
normal handler, so the original Throwable was not printed. The opt-in
`PENGUDROID_TRACE_EXIT=1` diagnostic installs RuntimeInit's logging/kill
handler to expose early application exceptions. The original Throwable has
not recurred in subsequent runs, so the underlying
exception remains unknown. SQLCipher also logs a missing `nativeOpen` JNI
method.

A separate signed `io.github.denizomer10.Sberbank` RPM now contains its own
executable, desktop entry, icons, original APK and prepared native libraries.
The currently installed development variant excludes four
`libcrashlytics*.so` files because their Bionic dependencies otherwise attempt
executable mappings from the noexec Documents runtime. The APK itself remains
unchanged; the full-native package is preserved separately under
`target/aurora/sberbank-rpm/`. With the three Hybris display properties set,
the clean standalone desktop launch renders the welcome/login screen through
automatic gralloc and remains alive. Evidence:
`target/aurora/sberbank-separate-clean-relaunch.png` and its `.log`. This is
still a development bundle, not a claim of full banking compatibility. 115 CLI
tests, the fortified-fgets regression and `git diff --check` passed.

## PSB Mobile device probe

`~/psbmobile.apk` is PSB Mobile 3.110.0 (versionCode 623), package
`logo.com.mbanking`, with launcher
`psb.com.mbanking.common.preloader.PreLoaderActivity`, 18 DEX files and 21
ARM64 libraries. Its original SHA-256 is
`a8757ee624455dc970590aebc22deea84ab3ae23eeb7dcc129d4a279118b6d50`.
The APK remains unchanged.

A separate signed `io.github.denizomer10.PsbMobile` development RPM contains
its own executable, `ПСБ` desktop entry, the APK's original icon, the original
APK and all 21 version-stripped ARM64 libraries. APM accepted and installed it
on Dialog-S. The first desktop launch reached application binding and provider
initialization, then died at PC 0 while loading CryptoPro native code.
`LD_DEBUG=files,reloc` isolated this to `libcspjni.so`: its 105-entry
`DT_INIT_ARRAY` has 104 relocated constructors followed by one unrelocated null
sentinel. Bionic skips that entry; glibc calls it. Native preprocessing now
trims trailing unrelocated null init-array entries while preserving relocated
zero entries. The device-verified `libcspjni.so` subsequently passed direct
`dlopen`, and the full app loaded both `libsupport.so` and `libcspjni.so` and
registered CryptoPro native methods. The ELF crate has regression coverage for
both cases (12 tests).

PenguDroid also performs the normal
`android.security.keystore2.AndroidKeyStoreProvider.install()` registration
from Zygote's JCA warmup before application binding. A real software Keystore2
AIDL backend now implements service/security-level/operation handles, app-scoped
AES-128/192/256 GCM key generation/import/get/list/delete, purpose checks,
nonce generation, AAD/update/finish/abort and service-specific errors. It uses
host OpenSSL EVP rather than a handwritten cipher and reports
`SECURITY_LEVEL_SOFTWARE`, never TEE/StrongBox. Persistent records use an atomic
app-private file below `$PENGUDROID_PREFIX/keystore2`; the device directory is
0700 and the file is 0600. This is OS-file-backed software key storage, not a
claim of hardware-backed banking security. Key/plaintext/ciphertext contents
are not logged.

The opt-in `PENGUDROID_TEST_KEYSTORE=1` JCA test passed the real Android
KeyGenerator/KeyStore/Cipher generate/get/encrypt/decrypt/delete path on the
phone. Unit tests cover descriptor/parameter Parcel roundtrips, the NIST
AES-256-GCM vector and modified-tag rejection. PSB then progressed through
AndroidX EncryptedSharedPreferences and Tink validation. Evidence:
`target/aurora/psbmobile-keystore2-keyid.log`.

The next independent startup blocker is DGis native code. A missing
`pthread_atfork` export was added and forwarded; DGis then entered native
`DGis.initialize` and crashed at `libdgismap-2.3.0.so+0x1147c0c`, in a loop
indexing the `_ctype_` table through a corrupted pointer. This is outside the
Keystore2 backend. Evidence: `target/aurora/psbmobile-atfork.log` and the
earlier native diagnostics `target/aurora/psbmobile-lddebug.log`.

DGis is used by the offices/ATMs map and is not required to reach the first
unauthenticated Activity. The target-only diagnostic
`target/aurora/psbmobile-nodgis.apk` changes only
`DgisInteractor.b(): void` in `classes5.dex` to `return-void`; the original APK
and regular RPM are unchanged. On the phone this copy completed `AppContainer`,
launched `PreLoaderActivity` and then `MainNoAuthActivity`, rendered through
gralloc and remained alive. A visible PSB splash/loading screen with its
spinner is captured in `target/aurora/psbmobile-nodgis.png` and
`target/aurora/psbmobile-nodgis-later.png`; lifecycle and rendering details are
in `target/aurora/psbmobile-nodgis.log`. With the subsequent automatic
`qt_extended_surface.raise`, the same diagnostic copy opened as a full-screen
window and showed the PSB welcome screen with Login and bank-onboarding
buttons: `target/aurora/psbmobile-raise.png` and
`target/aurora/psbmobile-raise.log`. This does not make the map feature usable
and does not prove the login flow. The no-DGIS copy is development evidence,
not a production banking package.

Network startup exposed three separate compatibility details. The real
`android:networkSecurityConfig` and `usesCleartextTraffic` values are now copied
from the manifest into `ApplicationInfo`; other applications do not inherit a
global cleartext exception. PSB's own config does not permit HTTP to
`nuc-cdp.voskhod.ru`, so that secondary request is still rejected correctly.
The visible offline banner had a different cause: its monitor starts in
DISCONNECT and waits for `CONNECTIVITY_CHANGE`. PenguDroid now supplies sticky
replies for both Android-17 registerReceiver transactions, an initial
`IMessenger` `CALLBACK_AVAILABLE` for modern NetworkCallbacks, and main-thread
sticky delivery through real LoadedApk ReceiverDispatchers whose
`getExpectedAction()` matches. The receiver then calls the published
`getActiveNetworkInfo().isConnected()` itself.

The red "Нет связи с сетью" banner is gone and the full bottom navigation is
visible in `target/aurora/psbmobile-network-direct.png`; the corresponding log
records delivery after both PreLoaderActivity and MainNoAuthActivity resumes,
without Binder/parcel failures. This is a verified connectivity-monitor fix,
not a claim that every bank endpoint or login succeeds. See
`target/aurora/psbmobile-network-direct.log`.

Opening the login pager then exposed a missing platform NFC initializer:
`NfcAdapter.getDefaultAdapter()` threw because `NfcServiceManager` itself was
null. PenguDroid now installs the ordinary framework NfcServiceManager but does
not publish an `nfc` Binder service or emulate hardware. Consequently the
adapter query returns null and Android logs `this device does not have NFC
support`, rather than throwing UnsupportedOperationException. The Login button
now reaches the card/login form and the process remains alive; see
`target/aurora/psbmobile-nfc-absent-login3.png` and
`target/aurora/psbmobile-nfc-absent.log`. No credentials or authenticated
banking operation have been tested.

## Runtime work still required

1. Run a native ARM64 `NativeActivity` fixture on the device as well; verify
   EGL/GBM/dma-buf and Firejail. The validated device Compose path uses SHM.
2. Audit the remaining AArch64 Bionic/glibc ABI boundaries on device. Successful
   cross-compilation does not prove those layouts or ART compatibility.
3. Replace the `pw-cat`/`aplay` audio assumptions with PulseAudio/libpulse.
4. Re-test NewPipe through the fixed cond/PNG path. Add real text-input support;
   audb pointer input works, while `audb text` currently does not reach Compose.
   Vulkan remains outside this stage.
5. Implement native runtime provisioning and reduce its disk footprint.
6. Complete sandbox lifecycle and application-data tests. Developer signing,
   audb installation and the visible Compose lifecycle have passed only with
   the separate combined test package.

Do not distribute this package as a working Android compatibility layer yet.
