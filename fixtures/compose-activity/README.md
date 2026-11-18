# Jetpack Compose Activity fixture

A real AndroidX/Jetpack Compose application used to test more than a single
platform `TextView`. The screen contains:

- a Material 3 `OutlinedTextField`;
- a Material 3 `Button` with a click counter;
- text reflecting the current field contents.

The APK is multi-dex and includes AndroidX's ARM64 `libandroidx.graphics.path.so`.
It is compiled against stable API 35 and targets/runs on the Android 17 API-37
runtime. No Google services are used.

## Build

Requirements: JDK 17, Gradle 8.9, and an Android SDK. Gradle downloads the pinned
AGP/Kotlin/Compose dependencies and, when absent, SDK build-tools 34 and platform
35. Toolchain and Gradle caches are development-only and are not included in
PenguDroid's RPM.

```sh
export JAVA_HOME=/path/to/jdk17
export ANDROID_HOME=/path/to/android-sdk
export GRADLE=/path/to/gradle-8.9/bin/gradle
./build.sh
```

The result is `ComposeActivity.apk`.

## Run through PenguDroid

```sh
export PENGUDROID_LAUNCH_ACTIVITY=org.pengudroid.composeactivity.MainActivity
pengudroid art-test --sysroot /path/to/android17-arm64 \
  --main android.app.ActivityThread \
  --classpath "$PWD/ComposeActivity.apk" \
  --register-natives libandroid_runtime.so,libhwui.so \
  --bind-app --vm-option -Xint
```

For the existing headless QEMU/EGL test, run the matching daemon with
`--no-wayland` and set `PENGUDROID_FORCE_SHM=1`.
`PENGUDROID_DUMP_FRAME=/tmp/compose.ppm` with `PENGUDROID_DUMP_FRAME_AT=0`
writes the first frame as `/tmp/compose.s0frame0.ppm`.

## Exercise both controls

At the default 720x1280 geometry, the existing synthetic input path can click
the button, focus the field, and type `hi`:

```sh
export PENGUDROID_SYNTH_CLICK='360,337@10000;360,190@2000'
export PENGUDROID_SYNTH_KEYS='14000:35,23'
```

The expected application log markers are:

```text
Compose button clicked: 1
Compose text changed: h
Compose text changed: hi
```

This interaction path and the resulting rendered frame have been validated with
AArch64 ART/HWUI/EGL under QEMU. A physical Aurora device and a real Wayland
compositor have not yet been tested.
