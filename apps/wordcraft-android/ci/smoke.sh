#!/usr/bin/env bash
# Emulator smoke test of the debug APK, run by .github/workflows/android.yml inside
# android-emulator-runner (which runs each script line in its own `sh -c`, so everything is here).
#
# It installs the APK and starts it with the sample document, once per graphics backend (Vulkan with the
# OpenGL ES fallback, then OpenGL ES alone). Each run must draw its first frame, stay alive and in front,
# draw something other than a blank window, and log no crash. Then a deliberate panic (debug builds only)
# must reach logcat with a backtrace that names functions. Screenshots and logs go to OUTDIR.
#
# Usage: smoke.sh APK OUTDIR
set -uo pipefail

apk=$1
out=$2
pkg=site.lutfi.foliowriter
activity=$pkg/.MainActivity
mkdir -p "$out"
status=0

fail() {
  echo "::error::$*"
  status=1
}

# wait_for_log PATTERN SECONDS: waits until logcat has a line matching PATTERN. The log goes through a
# file: with pipefail, `adb logcat | grep -q` fails whenever grep stops reading before adb has written all.
wait_for_log() {
  local i
  for ((i = 0; i < $2; i++)); do
    adb logcat -d -v brief >"$out/.wait.txt" 2>/dev/null || true
    grep -qE "$1" "$out/.wait.txt" && return 0
    sleep 1
  done
  return 1
}

# start ARGS: starts the activity afresh with the app's launch flags.
start() {
  adb logcat -b all -c || true
  adb shell am start -W -S -n "$activity" --es args "'$1'"
}

# drew_something RAW: whether the middle of a raw screencap (clear of the system bars) has more than a
# handful of colours, i.e. the app drew its interface and not just a background.
drew_something() {
  python3 - "$1" <<'PY'
import struct, sys
data = open(sys.argv[1], "rb").read()
width, height, fmt = struct.unpack_from("<III", data, 0)
# Android 9+ adds a colour-space word to the header.
header = 16 if len(data) >= 16 + width * height * 4 else 12
pixels = data[header:]
colours = set()
for y in range(height * 15 // 100, height * 85 // 100, 7):
    row = pixels[y * width * 4:(y + 1) * width * 4]
    for x in range(0, len(row) - 3, 4 * 5):
        colours.add(row[x:x + 4])
print(f"screen {width}x{height} (format {fmt}): {len(colours)} colours in the middle")
sys.exit(0 if len(colours) >= 16 else 1)
PY
}

# run NAME ARGS: one start with launch flags ARGS, checked; artefacts are named NAME.
run() {
  local name=$1 args=$2 pid
  echo "::group::$name ($args)"
  start "$args" >"$out/$name-start.txt" 2>&1
  cat "$out/$name-start.txt"
  if ! wait_for_log 'FolioWriter.*first frame after' 90; then
    fail "$name: no first frame within 90 s"
  fi
  # Let it settle: the app's first frames, the caret, any late failure.
  sleep 8
  pid=$(adb shell pidof "$pkg" | tr -d '\r')
  [ -n "$pid" ] || fail "$name: $pkg is not running"
  adb shell dumpsys window | grep -E 'mCurrentFocus|mFocusedApp' >"$out/$name-focus.txt" || true
  grep -q "$pkg" "$out/$name-focus.txt" || fail "$name: $pkg is not in front"
  adb exec-out screencap -p >"$out/$name.png" || fail "$name: no screenshot"
  adb exec-out screencap >"$out/$name.raw" || fail "$name: no raw screenshot"
  drew_something "$out/$name.raw" || fail "$name: the window is blank"
  rm -f "$out/$name.raw"
  adb logcat -d -b all -v threadtime >"$out/$name-logcat.txt"
  grep -E 'FolioWriter' "$out/$name-logcat.txt" | head -n 40
  # This app's crashes only: a Java crash names the process on the line after FATAL EXCEPTION.
  if grep -E "Process: $pkg|Fatal signal.*(foliowriter|android_main)|>>> $pkg <<<|RustPanic|panic at |panicked at|stopped after a panic|ANR in $pkg" "$out/$name-logcat.txt" >"$out/$name-crash.txt"; then
    cat "$out/$name-crash.txt"
    fail "$name: crash in logcat"
  fi
  echo "::endgroup::"
}

adb wait-for-device
{
  adb shell getprop ro.build.fingerprint
  adb shell getconf PAGE_SIZE
  adb shell wm size
  adb shell wm density
} >"$out/device.txt" 2>&1
cat "$out/device.txt"

if ! adb install -r -t -g "$apk" >"$out/install.txt" 2>&1; then
  cat "$out/install.txt"
  echo "::error::install failed"
  exit 1
fi

run vulkan-or-gl "--sample"
run gl "--sample --backend gl"

echo "::group::panic test"
start "--sample --panic-test" >"$out/panic-start.txt" 2>&1
if wait_for_log 'panic at .*--panic-test' 90; then
  sleep 3
  adb logcat -d -b all -v threadtime >"$out/panic-logcat.txt"
  grep -E 'FolioWriter' "$out/panic-logcat.txt" | head -n 60
  grep -q 'android::panic_test' "$out/panic-logcat.txt" || fail "panic test: the backtrace names no functions"
else
  adb logcat -d -b all -v threadtime >"$out/panic-logcat.txt"
  fail "panic test: the panic didn't reach logcat"
fi
echo "::endgroup::"

adb logcat -d -b crash -v threadtime >"$out/crash-buffer.txt" 2>&1 || true
rm -f "$out/.wait.txt"
exit "$status"
