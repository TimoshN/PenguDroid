# PenguDroid 0.3.0 — Component runtime

Android 17 / API 37 applications can now launch from independent runtime
components without materializing an Android system tree. First-run AppImage
setup installs the bundled components offline, without a GSI download or a
separate ARM64 guest sysroot.

## Changes

- Shared `pengudroid-runtime` descriptor and virtual Android path resolver.
- Direct ART/classpath/native-library provider discovery; version metadata
  comes from the descriptor rather than installed `build.prop` files.
- ARM64 linker, libc, vDSO and libraries are packaged as a guest component.
  Linker namespace realpaths remain in virtual Android coordinates.
- Checksummed offline stores, dependency checks, atomic activation and retained
  generations. Failed updates keep the active runtime intact.
- Existing tree installs no longer take automatic precedence over supplied
  component stores. Explicit developer overrides remain supported.
- `--runtime DIR` and `PENGUDROID_RUNTIME` select a runtime explicitly.
- AppImage includes private host libraries/loader, Python, the translation
  bridge and component archives. Normal launch requires no FUSE helper.

ART and Android framework binaries remain shipped components. This release
removes the sysroot filesystem requirement; it does not replace the entire
Android framework implementation. An x86_64 Linux kernel, Wayland session and
working host graphics drivers remain required.

## Validation

Workspace Rust tests and the full Python tool suite passed. Optional build-SDK
checks are skipped when their verified build inputs are unavailable. The seven
real CheckJNI framework probes were separately enabled against the component
installation. The ARM64 bridge initialized and a real translated guest function
returned `42` without an external guest root.

The final AppImage kept BBR2 2026.10.26 alive for 160 seconds, rendered its
game/menu scene (5,911 presents), and accepted injected input. A preceding
component development build also captured a live race. Test details, screenshots and the
scope of the final artifact verification are recorded in
[the AppImage validation report](COMPONENT_APPIMAGE_TESTS.md).

Genshin is excluded from this release's validation scope. Historical NVIDIA
screenshots are not a new Genshin compatibility claim. This validation does not
establish compatibility with all Android applications, GPU drivers or Linux
distributions. The existing ART boot-image mismatch uses its imageless fallback.

## Download

Download `PenguDroid-0.3.0-x86_64.AppImage` and `SHA256SUMS` from this release.
Verify the checksum, mark the image executable, and launch it:

```sh
sha256sum -c SHA256SUMS
chmod +x PenguDroid-0.3.0-x86_64.AppImage
./PenguDroid-0.3.0-x86_64.AppImage
```

First setup needs space for extraction, cached userspace files and installed
components; allow at least 5 GiB plus your games' own storage. Commercial APKs,
game downloads and account credentials are not included.
