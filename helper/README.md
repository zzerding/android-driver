# Virtual display helper

`VirtualDisplayServer` is the device-side half of the driver. It runs through
`app_process` as adb shell (uid 2000) on Android 14 (SDK 34), creates one
independent virtual display, and speaks a small JSON-lines protocol over its
own stdin/stdout.

The helper is not distributed as a prebuilt binary in this repository. Build it
from source so the device artifact matches the reviewed Java source.

## Requirements

- Java 11 or newer (`javac`, `jar`).
- Android API 34 `android.jar`.
- Android build-tools 34 `lib/d8.jar`.
- No Gradle, NDK, JNI, or signing is required.

Official packages and their published SHA-1 values:

| Package | Download | SHA-1 |
| --- | --- | --- |
| API 34 | <https://dl.google.com/android/repository/platform-34-ext12_r01.zip> | `ba80ccbcc29b29f25ac926a08c0b2777f0bce842` |
| build-tools 34 (macOS) | <https://dl.google.com/android/repository/build-tools_r34-macosx.zip> | `00e1301387028f33753e38274401694e1a27c62f` |

Use the build-tools archive for the host platform; only `lib/d8.jar` is used.

## Build

```bash
export ANDROID_JAR=/path/to/platforms/android-34/android.jar
export D8_JAR=/path/to/build-tools/34.0.0/lib/d8.jar
helper/build.sh
```

The script prints the produced path:

```text
helper/build/virtual-display-helper.zip
```

Pass that path to the driver with `--helper`. The driver pushes the jar to
`/data/local/tmp/android-driver-<token>.jar` for the duration of one session and
removes it during cleanup.

The D8 step keeps `-parameters` because D8 8.2.2-dev can fail on anonymous
classes without `MethodParameters`; the source does not otherwise depend on it.

## Helper protocol

The driver writes one JSON object per line to the helper's stdin:

```json
{"id": 1, "command": "activity_info", "component": "com.example.app/com.example.app.Main", "user": 0}
```

The helper writes one reply per line:

```json
{"id": 1, "ok": true, "result": {"launch_mode": 0, "task_affinity": "com.example.app", "exported": true, "user": 0}}
```

An `id` of `0` is the startup reply. Its result must contain a positive
`display_id`, `own_focus: true`, the requested `width`/`height`/`dpi`, and
`uid: 2000`; otherwise the driver refuses to use the display.

| Command | Arguments | Result |
| --- | --- | --- |
| `info` | none | display id, flags, own focus, dimensions, pid, uid |
| `snapshot` | none | latest frame as base64 PNG plus timestamp |
| `activity_info` | `component`, `user` | launchMode, taskAffinity, exported |
| `close` | none | empty result, then the process exits |

`snapshot` returns `No virtual display frame yet` until the first frame
arrives; the driver retries briefly.

The helper's stdin EOF handler and `close` command attempt
`VirtualDisplay.release()`. Host-side `SIGKILL`, a host crash, or an adb
disconnection does not guarantee that the helper receives EOF or runs this
handler. Device-side helper/display recovery in those cases is unverified on
real hardware; host kernel lock release does not prove device cleanup. The
display is created with
`VIRTUAL_DISPLAY_FLAG_DESTROY_CONTENT_ON_REMOVAL`, so content is not moved back
to the physical display when the helper exits.

## Boundaries

- Android 14 / SDK 34 only, adb shell uid 2000 only.
- A display without independent focus is an error, never a fallback to the
  physical display.
- `activity_info` is parameterized by component and Android user; it contains
  no application-specific constants.
