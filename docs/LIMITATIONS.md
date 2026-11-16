# Compatibility boundaries

## Independent execution and optional ART are different profiles

The Pengu backend can execute DEX and host JNI without ART or Android framework
JARs. Its Android/Java API implementation is experimental and incomplete.
The independent AppImage excludes ART/framework provisioning. The normal ART
backend still requires those binaries, either as components or an explicit legacy
tree. Removing a physical `system.img` does not remove that backend's dependencies.
See [independent runtime](INDEPENDENT_RUNTIME.md).

## What the compatibility figure measures

[The generated index](compatibility/index.json) counts JNI table wiring and a fixed
foundation scenario checklist. Wired functions may be partial or no-ops; a passing
test covers only its named scenario. Missing calls from a game trace are a lower
bound. These counts are not a percentage of all Android APIs and do not predict
whether an arbitrary APK works.

## Independent VM gaps

- Real concurrent Java execution, HandlerThread/Looper ownership, JNI attachment
  and asynchronous callbacks are incomplete. Synchronous builtin helpers are not
  a replacement for Android's scheduling model.
- Reflection, class/field metadata, resources, View layout and framework services
  have large gaps. Some prototype builtins/defaults remain placeholders.
- Independent WebView/Chromium, Android network/TLS services and many application
  platform APIs are not implemented.
- Heap collection, reference lifetime, monitor semantics, verification and complete
  exception behavior remain open. JNI UTF-16 roundtrips do not prove every
  `java.lang.String` operation preserves every edge case.
- Native calls now preserve typed arguments/returns and the JNI call families have
  real C varargs adapters. This does not establish complete JNI conformance.
- The normalized Xbox InputManager/device path is verified on the ART host.
  Independent DexVM still needs the corresponding device/UI dispatch integration.

Unimplemented methods and JNI slots are logged. `PENGUDROID_VM_STRICT=1` rejects
missing Java calls. A final compatibility report is written only when activity
launch returns; a native hang needs its trace log and thread diagnosis.

## Current application evidence

Genshin, BBR2 and NewPipe screenshots belong to the hosted ART compatibility path.
They must not be relabeled as independent DexVM results. Hardware, APK version and
account flow matter; historical gameplay is not a guarantee for later updates.

ZZZ 3.3.0 reaches native engine initialization without ART/framework on the Pengu
path, then blocks during incomplete worker/handler setup. Independent login,
responsive UI and gameplay are unverified. On the ART path the NVIDIA swapchain
allocation failure is fixed, but a transparent/blank SDK window remains unresolved.
Its content has not been established as a consent dialog; it must not be accepted
or discarded blindly. See [ZZZ handoff](ZZZ_HANDOFF.md).

BBR2 rendering artifacts reported by the user remain unresolved; adding a depth
buffer alone did not remove them. There is no new claim that they are fixed.

## Native support and host requirements

ARM64 translation still needs compatible native linker/libc/proxy support and host
C++ runtime support. That is independent of ART but is still a real dependency.
Missing or incompatible bridge setup produces an explicit error instead of
falling through to host `dlopen` for an ARM64 object.

The verified host target is x86_64 Linux with a Wayland compositor and working
GPU/audio drivers. Portable packaging supplies userspace libraries, not a kernel,
compositor or graphics kernel driver. Every Linux distribution/GPU and arbitrary
Android applications have not been verified. A host-process compatibility layer
also does not provide the isolation of a booted Android virtual machine.
