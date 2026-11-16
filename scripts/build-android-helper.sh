#!/usr/bin/env bash
# Build android-helper/phone-helper.jar: the helper phone-cli pushes to an
# Android phone's /data/local/tmp and runs with app_process (see
# docs/how-it-works.md). The jar is committed and embedded in the `phone`
# binary, so only people changing the Java need this script.
#
#   scripts/build-android-helper.sh          build, then check it builds the same twice
#   scripts/build-android-helper.sh --check  build in a scratch dir and fail if it differs
#                                            from the committed jar
#
# Needs a JDK (javac, release 8 output) and the Android SDK (d8 and
# platforms/android-NN/android.jar). Override with JAVAC, D8, ANDROID_JAR.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
src="$root/android-helper/src"
out_jar="$root/android-helper/phone-helper.jar"
out_sum="$root/android-helper/phone-helper.jar.sha256"
sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
min_api=26

pick_file() { for c in "$@"; do [ -n "$c" ] && [ -f "$c" ] && { echo "$c"; return; }; done; return 1; }
pick() { for c in "$@"; do [ -n "$c" ] && [ -x "$c" ] && { echo "$c"; return; }; done; return 1; }

JAVAC="${JAVAC:-$(pick \
  /opt/homebrew/opt/openjdk@17/bin/javac \
  "/Applications/Android Studio.app/Contents/jbr/Contents/Home/bin/javac" \
  "$(command -v javac || true)")}" || { echo "no javac (set JAVAC)" >&2; exit 1; }
D8="${D8:-$(ls -d "$sdk"/build-tools/*/d8 2>/dev/null | sort -V | tail -1)}"
# android-35 is what the committed jar is built against; else the newest there is.
ANDROID_JAR="${ANDROID_JAR:-$(pick_file "$sdk/platforms/android-35/android.jar" \
  "$(ls -d "$sdk"/platforms/android-*/android.jar 2>/dev/null | sort -V | tail -1)")}"
[ -x "$D8" ] || { echo "no d8 under $sdk/build-tools (set D8)" >&2; exit 1; }
[ -f "$ANDROID_JAR" ] || { echo "no android.jar under $sdk/platforms (set ANDROID_JAR)" >&2; exit 1; }
# d8 is a shell script that wants `java` next to the JDK javac came from.
export JAVA_HOME="${JAVA_HOME:-$(cd "$(dirname "$JAVAC")/.." && pwd)}"
export PATH="$JAVA_HOME/bin:$PATH"

# One build into $1/phone-helper.jar.
build() {
  local dir="$1"
  rm -rf "$dir" && mkdir -p "$dir/classes" "$dir/dex"
  # Sorted sources and no debug info, so class files depend on the code only.
  find "$src" -name '*.java' | LC_ALL=C sort > "$dir/sources.txt"
  "$JAVAC" -nowarn -Xlint:-options -source 8 -target 8 -g:none -encoding UTF-8 \
    -bootclasspath "$ANDROID_JAR" -d "$dir/classes" @"$dir/sources.txt"
  find "$dir/classes" -name '*.class' | LC_ALL=C sort > "$dir/classes.txt"
  "$D8" --release --min-api "$min_api" --lib "$ANDROID_JAR" --output "$dir/dex" $(cat "$dir/classes.txt")
  # A zip with fixed timestamps and order: the same dex gives the same jar.
  python3 - "$dir/dex/classes.dex" "$dir/phone-helper.jar" <<'PY'
import sys, zipfile
dex, jar = sys.argv[1], sys.argv[2]
# Stored, not deflated: no zlib version can change the bytes.
with zipfile.ZipFile(jar, "w", zipfile.ZIP_STORED) as z:
    info = zipfile.ZipInfo("classes.dex", date_time=(1980, 1, 1, 0, 0, 0))
    info.compress_type = zipfile.ZIP_STORED
    info.external_attr = 0o644 << 16
    info.create_system = 3
    z.writestr(info, open(dex, "rb").read())
PY
}

sum() { shasum -a 256 "$1" | cut -d' ' -f1; }

scratch="$(mktemp -d "${TMPDIR:-/tmp}/phone-helper.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT

echo "javac: $JAVAC ($("$JAVAC" -version 2>&1))"
echo "d8:    $D8"
echo "sdk:   $ANDROID_JAR"
build "$scratch/a"
build "$scratch/b"
a="$(sum "$scratch/a/phone-helper.jar")"
b="$(sum "$scratch/b/phone-helper.jar")"
if [ "$a" != "$b" ]; then
  echo "not reproducible: two builds differ ($a vs $b)" >&2
  exit 1
fi

if [ "${1:-}" = "--check" ]; then
  want="$(cut -d' ' -f1 < "$out_sum")"
  have="$(sum "$out_jar")"
  [ "$have" = "$want" ] || { echo "committed jar $have does not match its .sha256 $want" >&2; exit 1; }
  if [ "$a" != "$want" ]; then
    echo "this toolchain builds $a; the committed jar is $want (a different javac/d8 can do that)" >&2
    exit 1
  fi
  echo "ok: the committed jar ($want) is what these sources build"
  exit 0
fi

cp "$scratch/a/phone-helper.jar" "$out_jar"
echo "$a  phone-helper.jar" > "$out_sum"
echo "built $out_jar ($(wc -c < "$out_jar" | tr -d ' ') bytes, sha256 $a, reproducible)"
