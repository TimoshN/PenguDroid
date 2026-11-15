# PenguDroid 0.2.0 validation

Validated on 2026-11-15 on Ubuntu 26.04.1, Intel Core i3-12100F,
NVIDIA GeForce RTX 3080 Founders Edition (driver 595.99.02), Wayland.

## Included changes

- Genshin Impact 7.2.0 gameplay screenshot supplied from the working local build;
  README rebuilt around application evidence and practical setup instructions.
- Pinned static uruntime 0.8.3 starts in extraction mode by default, without FUSE or fusermount.
- Portable AppImage includes a private ELF loader/libc and dependency closure,
  Python, image extraction tools, PipeWire helpers, native bridge and ARM64 guest
  bundle. Android GSI is downloaded separately from Google's official server.
- First GUI launch prepares the runtime in a background worker with progress.
- ART launches automatically start the packaged daemon, including clean installs.
- Host helper subprocesses retain the private library search path. A persistent,
  content-addressed cache supports self-exec and survives AppImage unmounting.

## Completed checks

| Check | Result |
| --- | --- |
| Offline Rust workspace tests | 475 passed |
| Python tool tests, including ELF interpreter relocation | 258 passed |
| Release workspace build | Passed |
| Official GSI download into an empty cache | Downloaded, SHA-256 verified, extracted and prepared |
| Actual compressed AppImage, FUSE-free mode, unrelated working directory | Passed |
| Normal AppImage CLI startup with `PATH=/nonexistent`, no FUSE helper or developer environment | Passed |
| Runtime discovery without an explicit sysroot | Passed |
| GUI with an empty runtime cache and a previously verified download archive | Automatically prepared runtime; GUI remained alive (36 s) |
| NewPipe 0.29.1, fresh app data | ART/WebView/network activity; alive for 40 s, then stopped by harness |
| Genshin 7.2.0, fresh app data | ARM64 native bridge/activity startup; alive for 75 s, then stopped by harness |
| Beach Buggy Racing 2 2026.10.26 ARM64 XAPK, fresh app data | ARM64 activity/input; alive for 60 s, then stopped by harness |

The application smoke checks used the release AppImage with `PATH=/nonexistent`,
separate XDG data directories and separate daemon sockets. Their runtime came
from the verified first-download test. They did not reuse the developer sysroot
or user game accounts. The GUI preparation test reused only the download archive,
not an installed runtime. Gameplay screenshots document the prior working local
runs; the release smoke checks do not constitute a new end-to-end account login,
full game download, gameplay or video playback benchmark.

## Limits

The older BBR2 2026.10.05 APKM fails in native startup with a pending
`VuAgeHelper.isKidsProfile()` JNI error; it is not the supported test package.
A working x86_64 Linux kernel, Wayland desktop and GPU drivers remain required.
This machine's tests do not establish compatibility with every Linux distribution.
No commercial APK, game data or account credential is included.

Build: `tools/build-appimage.sh --portable --with-bridge` after preparing the
native bridge build inputs. Verify the downloaded asset with `sha256sum -c SHA256SUMS`.
