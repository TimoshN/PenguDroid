#!/bin/sh
# Build the Jetpack Compose fixture with JDK 17 and Gradle 8.9+.
set -eu
cd "$(dirname "$0")"
GRADLE=${GRADLE:?set GRADLE to a Gradle 8.9+ executable}
"$GRADLE" --no-daemon "$@" assembleDebug
cp app/build/outputs/apk/debug/app-debug.apk ComposeActivity.apk
printf 'built %s\n' "$PWD/ComposeActivity.apk"
