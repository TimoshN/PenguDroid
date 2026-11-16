# Independent Pengu runtime

PenguDroid has an independent execution path, not a complete independent Android
implementation. `--backend pengu` executes DEX bytecode with `pengudroid-dexvm` and
uses its own JNI table and Java/Android builtins. It does not load ART or Android
framework JARs. The older `art` backend remains available in ordinary builds for
applications that need the hosted Android framework.

Removing the physical `system/` tree is a different achievement. The published
0.3.0 component runtime still contains ART/framework binaries. Moving those
binaries into components does not make that backend ART independent.

## Execution paths

| Path | ART/framework at runtime | Native translation | Status |
|---|---|---|---|
| `pengu` | Excluded; no automatic ART fallback | Digitalis/Berberis for ARM64 libraries | Experimental; incomplete Android APIs |
| `native` | Not needed for the supported NativeActivity path | Host ABI or supported bridge | Limited native application path |
| `art` | Required, either components or an explicit legacy tree | Digitalis/Berberis for ARM64 libraries | Existing compatibility path; still incomplete |

The independent AppImage sets `PENGUDROID_INDEPENDENT=1`, selects `pengu`, disables
runtime provisioning in both CLI and GUI, and refuses ART provisioning/execution
commands. Its profile marker lives under `usr/share/pengudroid`, so first-start
cache materialization preserves the restriction. It contains no ART/framework module store. Normal builds retain their
existing `auto` selection for compatibility. Set `PENGUDROID_BACKEND=pengu` or pass
`--backend pengu` explicitly when using a normal build.

## Build and inspect

Building the native-call adapter requires a C compiler, `ar` and libffi development
headers/library, in addition to Rust. The portable packager carries libffi in its
host-library closure; users of the AppImage do not need development packages.

```sh
cargo build --release --offline --workspace
./target/release/pengudroid compat-index
./target/release/pengudroid launch /path/to/application.apk --backend pengu
```

For an independent portable image with ARM64 support:

```sh
sh tools/build-appimage.sh --skip-build --pengu --portable --with-bridge \
  --bridge-dir /path/to/verified/host-bridge \
  --bridge-support /path/to/host-cpp-support \
  --guest-root /path/to/verified/arm64-guest-bundle \
  --output target/PenguDroid-Pengu-x86_64.AppImage
```

`--bridge-support` copies only `libc++.so`, with Android symbol versions stripped,
for the host-side translator. It does not copy a sysroot, ART, or framework
libraries. `tools/pengu-guest.py` derives only named native ABI roots and their
ELF dependency closure from the verified ARM64 input. The tested closure has 21
payload files (about 13 MB), under `native/bin` and `native/lib64`, with no physical
`system/` or `apex/` tree, ART libraries or framework JARs. A `profile: pengu`
descriptor maps only the three native ARM64 ABI locations. Unknown Android paths
resolve to a missing location instead of falling back to a stock runtime.
The guest still supplies its native linker/libc and proxy support; independence
from ART is not independence from native ABI support.
`--pengu` rejects `--with-runtime` and `--with-modules`.

The AppImage provides userspace libraries, not the Linux kernel, compositor or GPU
driver. The tested target remains x86_64 Linux with Wayland and a working driver.

## What was changed and tested

- Indexed class/method lookup replaces repeated scans. Method bytecode is shared
  through `Arc`, rather than copied on every invocation. Loading a later DEX
  preserves first-definition class identity and rejects invalid method indices
  before publishing a partial index.
- Native dispatch respects declared static/private method flags. libffi carries
  integer, pointer, float/double and stack arguments with the declared return
  type. Prepared interfaces are reused by shorty signature.
- JNI ellipsis, `V` and `A` method families use a C adapter for real default
  argument promotions; the old three-integer argument guess is no longer wired.
- JNI UTF-16 / modified UTF-8 handles embedded NUL, supplementary characters and
  lone surrogates in JNI roundtrips, with independent pinned buffers and checked
  regions. This does not establish complete `java.lang.String` conformance.
- Pending JNI exceptions are retained and transferred back to the interpreter.
  VM callback registration is scoped so returning from activity launch does not
  leave a dangling VM pointer in thread-local storage.
- File/app-directory and storage-capacity calls use the actual host filesystem.
  Atomic integer/long/reference operations use the serialized heap and preserve
  Java overflow and reference identity in tested cases. Handler `obtainMessage`
  preserves its target and payload.
- Bridge callback tables have static lifetime, enforced by the native-bridge API
  and a compile-fail regression. This fixes the optimized ZZZ crash after
  `GetMethodID(Class.getClassLoader)`: the translator retained a pointer to a
  stack table that had already gone out of scope. Later threading work builds on that bridge lifetime fix.
- The launcher respects an explicit bridge, fails explicitly when ARM64 bridge
  setup fails, publishes APK split paths in `ApplicationInfo`, and establishes
  prefix paths before preload. Re-execution happens before unpacking the bundle,
  avoiding materializing the large APK payload twice.

The dispatch example is a focused lookup microbenchmark, not a game benchmark:

```sh
cargo run --release --offline -p pengudroid-dexvm --example dispatch_bench
```

Its workload and raw before/after samples are recorded in
[the generated index](compatibility/index.json). The baseline is upstream
`0be8e05`. Do not translate its speedup into an FPS claim.

## Dependency and compatibility audit

```sh
./target/release/pengudroid compat-index > target/pengu-inventory.json
PENGUDROID_COMPAT_REPORT=target/app-compat.json \
  ./target/release/pengudroid launch /path/to/application.xapk --backend pengu
```

`compat-index` reports effective JNI table wiring and checks current process
mappings. The launch path checks for forbidden ART/framework mappings before and
after activity execution. `PENGUDROID_COMPAT_REPORT` writes the launch result,
missing Java signatures, placeholder JNI calls and observed mappings **when
activity launch returns**. A native hang or forced termination can prevent this
initial report; collect the trace log as well. Bounded main-loop audits refresh
the report with worker failures and aggregate counts while the queue runs. Missing methods and JNI slots are
logged even without verbose VM tracing. `PENGUDROID_VM_STRICT=1` stops unresolved
Java calls instead of returning a descriptor default.

For external verification use `strace -f -e trace=openat` and inspect successful
opens, plus `/proc/<pid>/maps`. Merely finding no `DT_NEEDED: libart.so` is
insufficient because a backend can load libraries dynamically.

The generator takes the binary's inventory and runtime logs:

```sh
python3 -B tools/compat-index.py --inventory-json target/pengu-inventory.json \
  --trace-log target/app-trace.log --output docs/compatibility
```

The figure counts 229 JNI function slots (excluding four reserved entries) and a
fixed checklist of foundation scenarios. A wired entry may still be partial or
a no-op. Observed missing calls are a lower bound. There is no defensible global
“Android compatibility percentage” in this audit.

## ZZZ 3.3.0 result and remaining work

With the flat native-only ARM64 bundle, verified bridge and isolated host C++ support, the independent VM
loads five DEX files and reaches `libmain.so` / `libil2cpp.so` initialization without
opening ART/framework libraries in the recorded `openat` trace. This is dependency
isolation evidence, not a working-game result.

The current source implements real worker interpreters, owned HandlerThread/Looper
queues, shared statics/class initialization and DEX/JNI monitors. SDK type identity,
primitive Class fields, dynamic proxies and reflection corrections advance the
local development-prefix run to repeated Unity `nativeRender` calls. Additional
resource/UI, Android service, builtin reflection, asset-pack and MethodHandle calls
remain unresolved. No independent ZZZ responsive login or gameplay is verified.
See [the threading implementation](DEXVM_THREADING.md) and [validation records](compatibility/validation.json).

The older standalone AppImage validation in that record predates this threading
work. It must not be used as validation of the new source. Its clean-prefix test
was bounded at 90 seconds and remained before a verified UI.

The ART path separately reached the game's login background on RTX 3080 after the
DRM-modifier swapchain fix, but its transparent/blank SDK window remains unresolved.
The user has not observed readable consent text or buttons there. It must not be
silently dismissed or treated as accepted terms.

The remaining architecture work includes resources/View layout, platform services,
an independent WebView/network implementation, heap collection, JNI reference
lifetime and complete application/native-engine teardown.
