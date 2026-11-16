# Local checkpoint — 2026-11-16

User requested stopping tests and committing locally. Do not start more work in
this turn. Nothing is pushed or released. Resume only on the next user request.

## Implemented in this checkpoint

Real OS-backed Java workers; per-worker interpreters sharing immutable DEX,
synchronized heap/statics and once-only class initialization. Owned HandlerThread /
Looper queues, Handler callback routing/delays/removal/quit, main-thread UI Runnable
delivery, blocking latch/nonfair semaphore, interruptible sleep/join, reentrant
DEX/JNI monitors and wait/notify. JNI attachment owns its interpreter. Dynamic
proxy/reflective metadata, primitive Class objects, boxed values and collection
class identity fixes advance the SDK startup. Choreographer uses a one-shot
17 ms software clock, not compositor vsync.

Native library requests from workers are delivered to the main queue so the
owner uses its actual ARM64 bridge. No Rc bridge closure or main Vm pointer is
sent across threads. Bridge owners remain alive until process exit because native
engine threads can outlive the activity. Full shutdown/GC/reference lifetime,
scheduler fairness and Android framework conformance remain incomplete.

The most recent addition starts the manifest Application (constructor,
attachBaseContext, onCreate) before the Activity and shares its identity. It passes
manifest metadata to ApplicationInfo and provides Bundle lookups. Lifecycle
callback registration exists but delivery is not implemented yet. Custom
Application/content-provider initialization still requires follow-up. This last
Application change has a successful release CLI build; the user stopped further
testing. A regression was added immediately before that stop; do not assume it
has passed without reviewing the captured log on resumption.

## Verified results before the stop

Earlier checkpoint: 528 workspace Rust tests passed; 287 Python tool tests passed;
28 release DexVM tests passed. Later owner-thread native loading addition passed
29 debug DexVM tests. These counts predate the final Application regression.
Latest release CLI build succeeded (`target/thread-release-cli-build.log`).

`target/dexvm-audit/real-threads-release-owner-load.log` and its report are from a
completed bounded release run BEFORE Application startup was added. It exited 0
because the requested observation interval completed: that is not game success.
It reaches repeated Unity nativeRender calls, reports 69 missing Java signatures,
no placeholder JNI calls, no worker failures and no forbidden runtime mappings.
There is NO verified visible, responsive ZZZ login or gameplay.

The meaningful SDK failure is:
`ComboForUnity.setUnityVersion` -> `SDKInfo` initializer -> `ConfigCenter.currentConfig`
-> `UninitializedPropertyAccessException: lateinit property currentConfig has not been initialized`.
The launcher previously skipped manifest `com.combosdk.openapi.ComboApplication`.
The final Application-startup addition is intended to address that missing lifecycle;
its actual ZZZ outcome is not tested yet. Do not hardcode currentConfig or bypass
consent. Other gaps include resource/assets/Scanner, builtin reflective metadata,
MethodHandles, Android services, Play asset-pack tasks, View layout/input and WebView.

## Paths for continuation

- Runner: `target/dexvm-audit/run-threads-release.py LABEL` (180 s timeout,
  150 s main-queue audit), and `run.py` (debug).
- ZZZ XAPK: `/home/asus/Downloads/Zenless+Zone+Zero_3.3.0_APKPure.xapk`.
- Private prefix: `target/dexvm-audit/zzz-prefix`.
- Native-only guest: `target/pengu-native-only`.
- Verified host bridge: `target/upstream-v0.2.1/extracted/AppDir/usr/lib/pengudroid/bridge`.
- Host C++ support: `target/dexvm-audit/bridge-support`.
- Five extracted DEX files: `target/dexvm-audit/inspect/*.dex`.
- Diagnostic example source moved out of repository to
  `target/dexvm-audit/inspect_class.rs`; older built inspector remains in target/debug/examples.
- Use `/home/asus/.cargo/bin/cargo` (cargo is not on PATH).

Existing AppImage `target/PenguDroid-Pengu-x86_64.AppImage` is STALE and predates
this threading implementation. Do not publish it as the new working build.
`docs/compatibility/validation.json`, generated index/overview and their artifact
hash remain historical. Catalog source has been updated; regenerate the figures
and source hashes after further implementation/validation. Latest measured dispatch
samples are `target/dexvm-bench-threads.log` (~187–196 ns/call), baseline
`target/dexvm-bench-before-final.log`; neither is a game FPS benchmark.

Three preexisting local files (apphost, GLES shim, Vulkan shim) were preserved
separately from this implementation. Their transparent-window heuristic is
unverified and may hide a real SDK dialog. Do not rely on it or auto-accept terms.
