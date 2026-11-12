#!/usr/bin/env bash
# Builds the device-side helper with the official Android SDK jars.
set -euo pipefail

cd "$(dirname "$0")"

: "${ANDROID_JAR:?set ANDROID_JAR to platforms/android-34/android.jar}"
: "${D8_JAR:?set D8_JAR to build-tools/34.0.0/lib/d8.jar}"

OUT="${1:-build}"
SOURCE="src/io/github/zzerding/androiddriver/VirtualDisplayServer.java"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/classes" "$WORK/dex" "$OUT"

# HACK: D8 8.2.2-dev fails on anonymous classes without MethodParameters, so
# -parameters is required even though this source does not otherwise need it.
javac --release 11 -parameters -proc:none -Xlint:all -Werror \
  -cp "$ANDROID_JAR" -d "$WORK/classes" "$SOURCE"

jar --create --file "$WORK/input.jar" -C "$WORK/classes" .
java -cp "$D8_JAR" com.android.tools.r8.D8 \
  --release --min-api 34 --lib "$ANDROID_JAR" \
  --output "$WORK/dex/helper.zip" "$WORK/input.jar"

cp "$WORK/dex/helper.zip" "$OUT/virtual-display-helper.zip"
if ! jar tf "$OUT/virtual-display-helper.zip" | grep -qx 'classes.dex'; then
  echo "error: dex output does not contain classes.dex" >&2
  exit 1
fi

echo "$OUT/virtual-display-helper.zip"