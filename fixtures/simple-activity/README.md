# Simple Java Activity fixture

A single dependency-free Android `Activity` with one hardware-rendered
`TextView`. It logs `onCreate`, content-view installation, `onResume`, and
window attachment so an ART smoke test can distinguish application binding from
actual Activity/HWUI progress. After resume it changes the text and background
three times, exercising repeated EGL invalidation, drawing, and presentation.

The APK contains Java bytecode only, so the same package tests x86_64 and
AArch64 runtimes. Build tools are development dependencies and are not included
in PenguDroid's RPM.

```sh
export JAVAC=/path/to/jdk/bin/javac
export JAVA=/path/to/jdk/bin/java
export AAPT2=/path/to/aapt2
export R8_JAR=/path/to/r8.jar
export ANDROID_JAR=/path/to/android.jar
./build.sh

pengudroid art-test --sysroot /path/to/android17-sysroot \
  --main android.app.ActivityThread \
  --classpath "$PWD/SimpleActivity.apk" \
  --register-natives libandroid_runtime.so,libhwui.so \
  --bind-app --vm-option -Xint
```

Set `PENGUDROID_LAUNCH_ACTIVITY=org.pengudroid.simpleactivity.MainActivity` to
exercise Activity creation and EGL rendering, not only `Application` binding.
For a headless QEMU run, start the matching daemon with `--no-wayland`, set
`PENGUDROID_FORCE_SHM=1`, and use `PENGUDROID_DUMP_FRAME=/tmp/activity.ppm`
with `PENGUDROID_DUMP_FRAME_AT=0` to dump the first real frame. Use
`PENGUDROID_DUMP_FRAME_EVERY=1` instead to dump every update after frame zero.
The resulting files are named like `/tmp/activity.s0frame0.ppm`.
