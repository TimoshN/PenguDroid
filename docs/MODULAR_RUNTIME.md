# Component runtime without an Android system tree

This document describes the **ART-dependent component profile**. Its absence of a
physical system tree does not mean absence of ART/framework binaries. The separate
[Pengu profile](INDEPENDENT_RUNTIME.md) excludes those runtime payloads and has a
much smaller, experimental Android API implementation.


The component installation no longer creates or reads a physical `system/` or
`apex/` tree. ART, framework resources, configuration, fonts, native libraries
and the optional ARM64 guest are separate providers. `pengudroid-runtime` reads
`pengudroid-runtime.json` and resolves Android ABI paths to those providers.
Version properties come from the descriptor; installed `build.prop` files are
removed. Guest processes use `PENGUDROID_RUNTIME_MANIFEST`, with
`PENGUDROID_ROOT` unset. A bundled ARM64 provider also removes the separate
`PENGUDROID_ARM64_GUEST_ROOT` dependency.

Android ART/framework binaries remain components. This change removes the
filesystem/sysroot dependency; it does not claim a new implementation of ART or
of the entire Android Java API. The earlier modular installer reconstructed a
reduced Android tree. That transport layout remains an explicit compatibility
option; it is no longer the default installation.

## Build and install

Create the host component store once from verified API-37 artifacts:

```sh
python3 tools/runtime-modules.py pack /path/to/android17-x86_64 \
  --output .toolchain/runtime-host-store
```

Include the already built native-bridge guest as an independent component:

```sh
python3 tools/runtime-modules.py add-guest .toolchain/runtime-host-store \
  --guest-root .toolchain/guest-arm64-bridge \
  --output .toolchain/runtime-components-store
```

`add-guest` materializes the guest binaries and libraries in a checksummed archive.
It does not retain links to the build tree. Stock/staged-only guest trees are
refused. Execution still requires the actual modified ARM64 linker/libc/vDSO
contracts, validated by the launcher; a JSON capability alone cannot authorize
an incompatible guest.

Install and launch:

```sh
cargo build -p pengudroid-cli -p pengudroid-daemon -p pengudroid-shim \
  -p pengudroid-gleshim -p pengudroid-vkshim --offline
./target/debug/pengudroid runtime --modules .toolchain/runtime-components-store
./target/debug/pengudroid runtime --status
./target/debug/pengudroid launch /path/to/app.apk
```

The store, rather than its source tree, is the installation input. Installation
and launch require no extracted GSI, NDK, build toolchain or external guest
sysroot. Python 3.11+ runs the installer; the portable AppImage includes it.
`--runtime DIR` or `PENGUDROID_RUNTIME=DIR` selects an explicit installation.
Legacy `--sysroot`/sysroot environment overrides remain accepted for comparisons.

To provide an offline store for automatic first-run setup:

```sh
export PENGUDROID_RUNTIME_MODULES=/path/to/runtime-components-store
./target/debug/pengudroid launch /path/to/app.apk
```

A missing runtime is prepared without a terminal prompt or image download.
`PENGUDROID_AUTO_RUNTIME=0` disables automatic launch/GUI setup; explicit
`runtime --modules STORE` remains available. The CLI also discovers a store
bundled beside its executable.

## Component paths and virtual paths

The installed directory contains only `components/`, `state/` and descriptors.
Typical mappings are:

| Android ABI path | Physical provider |
| --- | --- |
| `/system/framework` | `components/framework` |
| `/system/etc` | `components/config` |
| `/system/lib64` | `components/native` |
| `/system/fonts` | `components/fonts` |
| `/apex/com.android.art` | `components/com.android.art` |
| `/apex/com.android.i18n` | `components/com.android.i18n` |
| `/system/bin/arm64` | `components/arm64/bin` |
| `/system/lib64/arm64` | `components/arm64/lib` |
| `/system/etc/ld.config.arm64.txt` | `components/arm64/ld.config.arm64.txt` |
| `/data` | `state/data` |

The descriptor owns these mappings; the paths are not mount points or symlink
aliases recreating an OS tree. The CLI reads its classpath, native library
search directories and ART location from component providers. The preload shim
maps guest open/stat/access/directory operations. Host paths such as `/usr`,
`/proc` and `/etc` retain host behavior. Explicit guest-relative `..` components
are normalized in the virtual coordinate space before selecting a provider.

Linker realpath/readlink results are translated back to guest coordinates. This
is essential for ARM64 namespace selection: the configuration's `dir.system`
and executable must both be virtual paths. `/system` must not acquire an extra
trailing slash during that conversion. SDK properties, platform compatibility
policy, timezone/ICU resources and mutable ART data also use providers.

HIDL support comes from PenguDroid's in-process dispatcher. API-37 libhidlbase's
specific `access(F_OK)` capability probe no longer requires the GSI's
`hwservicemanager` executable. Read/write/execute requests still use the actual
filesystem, and absent HALs still return typed absence.

## Payload and dependency checks

The desktop host payload contains 38 modules and 774,991,194 transport bytes
(739.1 MiB), versus 2,702,441,079 source payload bytes: a 71.3% reduction. The
optional built ARM64 guest adds its own archive/component, making 39 modules.
The host payload count excludes that separate guest archive and runtime state.

Every native soname visible in the launcher's search directories is retained,
including APEX-only and dynamically loaded APIs. Boot classpath fragments retain
their APEX providers. ELF program headers supply native dependency edges, and
cross-component symlinks supply additional module edges. The verified host
manifest records 8,573 ELF edges. Missing providers/modules fail preparation.

System apps, 32-bit host libraries, executable OS services, packed APEX duplicates
and precompiled OAT caches are omitted. `framework-res.apk`, native codecs,
configuration and fonts remain usable providers. App data and host virtual
filesystems are not packaged.

Archive SHA-256/size are verified before extraction. Traversal, absolute archive
paths, special files, collisions and escaping symlinks are refused. Descriptor
mount targets must be inside the installation and cannot traverse symlinked
mount roots. Target SDK and the actual ART ELF architecture are checked. Local
manifests are trusted input; checksums are not publisher signatures.

## Installation lifecycle

Under `$XDG_CACHE_HOME/pengudroid/runtime`, or `$HOME/.cache/pengudroid/runtime`:

- `android17-x86_64-modular-versions/<generation>` holds an installation.
- `android17-x86_64-modular-current` atomically selects it.

Updates assemble separately, validate the descriptor and real boot classpath,
then switch the active link. Running guests use canonical generation paths.
Failed updates retain the active installation. Identical component installs
reuse their generation; an older tree-based installation is migrated by
reinstalling its store. Old generations remain available for running guests and
explicit rollback. Automatic garbage collection is not implemented.

For an isolated test, set `XDG_CACHE_HOME` and `XDG_DATA_HOME` to scratch
locations. Both version 1 and version 2 transport stores remain readable. The
legacy debug assembler is explicit:

```sh
python3 tools/runtime-modules.py assemble STORE --layout legacy --output NEW_DIR
```

## AppImage

```sh
tools/build-appimage.sh --portable --with-bridge \
  --with-modules .toolchain/runtime-components-store \
  --output target/PenguDroid-components-x86_64.AppImage
```

When the store includes the ARM64 component, `--with-bridge` bundles only the
host translator/proxies. It does not also copy a `guest-arm64/system` tree.
The image contains neither an extracted sysroot nor a separate guest sysroot.
First launch assembles component providers offline in the user's cache.

## Verification on 2026-11-16

- Source-removal and bundled first-run tests run without a GSI downloader.
- Registry tests cover exact mount boundaries, traversal, host-path exclusion,
  symlinked roots, duplicate mounts, SDK/ELF identity and non-UTF-8 file names.
- The seven real CheckJNI framework gates pass on a component installation:
  activities, AudioTrack, BLAST lifecycle, core services, main-thread dispatch,
  media discovery and process identity.
- The ARM64 bridge initialized from `components/arm64` with no external guest
  sysroot; a translated function returned `42` through its real trampoline.
- The 446 MiB release AppImage installed offline with an empty isolated cache,
  unrelated working directory and `PATH=/nonexistent`.
- BBR2 ran from that image with no physical `system/`, `apex/` or `build.prop`.
  Its process environment used the descriptor and neither legacy root variable.
  See [the packaged validation report](COMPONENT_APPIMAGE_TESTS.md).

ART continues to use its existing imageless fallback after a boot-image
read-barrier mismatch. These checks establish specific hosted behavior, not
universal Android compatibility. Genshin is excluded from the current scope at
the user's request. Remote module hosting/signing and a replacement ART/Java
framework implementation are not part of this filesystem migration.

```sh
python3 -m unittest discover -s tools -p test_runtime_modules.py -v
python3 -m unittest discover -s tools -p test_build_appimage.py -v
cargo test -p pengudroid-runtime -p pengudroid-cli --offline --lib --bins --test runtime_modules
cargo test -p pengudroid-shim --lib rootfs::tests --offline
PENGUDROID_TEST_ANDROID17_SYSROOT=/path/to/component-installation \
  cargo test -p pengudroid-cli --test art_framework --offline -- --test-threads=1
```
