#!/bin/sh
# Build a dependency-free Java Activity APK. Requires JDK 17, R8 and aapt2.
set -eu
cd "$(dirname "$0")"

JAVAC=${JAVAC:-javac}
JAVA=${JAVA:-java}
AAPT2=${AAPT2:-aapt2}
R8_JAR=${R8_JAR:?set R8_JAR to the R8 jar}
ANDROID_JAR=${ANDROID_JAR:?set ANDROID_JAR to an Android platform jar}

rm -rf build SimpleActivity.apk
mkdir -p build/classes build/dex

"$JAVAC" --release 8 -classpath "$ANDROID_JAR" \
    -d build/classes src/org/pengudroid/simpleactivity/MainActivity.java
"$JAVA" -cp "$R8_JAR" com.android.tools.r8.D8 --release --min-api 23 \
    --classpath "$ANDROID_JAR" --output build/dex \
    build/classes/org/pengudroid/simpleactivity/MainActivity*.class
"$AAPT2" link -o SimpleActivity.apk -I "$ANDROID_JAR" \
    --manifest AndroidManifest.xml --min-sdk-version 23 --target-sdk-version 37
zip -q -j SimpleActivity.apk build/dex/classes.dex
printf 'built %s\n' "$PWD/SimpleActivity.apk"
