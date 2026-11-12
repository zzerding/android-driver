# Android Driver

A reusable Android 14 virtual-display driver with a Rust library and a JSON-lines stdio process interface. It owns device control and session cleanup; callers own application-specific navigation and state machines.

## Scope

The driver creates one Android 14 (API 34) virtual display through `adb shell` and a small `app_process` helper. It exposes device listing, open/close, status, screenshots, component metadata, explicit component launch, tap, swipe, and a restricted keycode set.

It does not provide UI trees, OCR, vision models, app workflows, text entry, CDP, token capture, or a physical-display fallback. The virtual display isolates display/input routing only. It does **not** isolate application processes, tasks, or state. Closing the display can affect the app task and does not migrate it to the physical display.

The first implementation targets an unlocked Android 14 device and adb shell UID 2000. Other Android versions are unsupported. No real-device compatibility claim is made by the offline tests.

## Build

Rust 1.95 or newer and `adb` are required. Build the device helper with Java 11+ and the Android 14 SDK platform and build-tools 34:

```sh
ANDROID_JAR=/path/to/platforms/android-34/android.jar \
D8_JAR=/path/to/build-tools/34.0.0/lib/d8.jar \
  bash helper/build.sh

cargo check --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets
```

The helper artifact is written under `helper/build/virtual-display-helper.zip`. The build script compiles the checked-in Java source; it does not package the old app-specific helper. Supply the artifact explicitly:

```sh
cargo run -p android-driver -- --helper helper/build/virtual-display-helper.zip
```

No device is contacted until a `open` request is sent. This first release supports Android 14 only; there is no automatic downgrade to the physical display.

## Protocol

The process writes a `ready` frame to stdout first. Requests and responses are UTF-8 JSON, one object per line. Requests are processed in order, IDs must be positive and strictly increasing, unknown fields are rejected, and each request is bounded to 1 MiB. Standard output is protocol-only; diagnostics go to stderr.

```json
{"id":1,"op":"list_devices"}
{"id":2,"op":"open","serial":"DEVICE_SERIAL","user":0}
{"id":3,"op":"launch","component":"com.example.app/.Main"}
{"id":4,"op":"screenshot"}
{"id":5,"op":"tap","width":1080,"height":2400,"x":540,"y":1200}
{"id":6,"op":"close"}
```

A successful response has `{"type":"response","id":N,"ok":true,"result":...}`. Errors have `ok:false` and a structured `error.code` and `error.message`. The ready frame advertises protocol version, operations, limits, and capabilities. `text_input` and `unicode_text_input` are false.

Coordinates use the exact screenshot pixel dimensions returned by the session. They are zero-based and half-open: `0 <= x < width`, `0 <= y < height`. The caller must use the same dimensions as its screenshot. Key actions accept only the advertised safe keycode allowlist.

`open` locks one device identified by canonical adb server IP/port and serial, regardless of Android user. The default per-user lock directory is `~/.android-driver/locks`; lock files persist and kernel locks release on process exit. This is host-local advisory ownership: other machines, containers with different lock namespaces, and tools that ignore this driver's lock are not coordinated. Remote adb server addresses must be IP literals or `localhost` and must be reachable. The optional `--lock-dir` exists for test harnesses and explicit host-local coordination; all clients sharing the device must use the same directory.

The process owns the session until `close`, stdin EOF, or an output pipe failure. EOF cancels pending work and triggers bounded best-effort cleanup. A timeout on launch dispatch, input dispatch, or post-launch verification includes `error.details.result_unknown: true`; it is not retried and the session becomes unusable. A forced process kill or a lost adb connection can still leave device-side state behind. Cleanup is verified before the lock is released where the device remains reachable; uncertain dumpsys output is not proof of removal.

Before launch and each input, the driver requires a confirmed unlocked keyguard and refuses to interfere when the same package is resumed on physical display 0 for the session user. Components with launch modes that may reuse a task are refused. Launch is accepted only after the target package is observed resumed on the virtual display and user. These checks are conservative heuristics, not a security boundary.

## Rust API

The core library can be embedded directly when the host is Rust. The standalone [session example](crates/android-driver/examples/session.rs) is compiled by `cargo check --workspace --all-targets`:

```rust,no_run
use std::ffi::OsString;
use std::time::Duration;

use android_driver_core::{AdbEndpoint, CancelToken, Driver, DriverConfig, OpenRequest};

fn main() -> android_driver_core::Result<()> {
    let mut driver = Driver::new(DriverConfig {
        adb_path: OsString::from("adb"),
        endpoint: AdbEndpoint::parse("localhost:5037")?,
        helper_path: Some("helper/build/virtual-display-helper.zip".into()),
        lock_dir: None,
        adb_timeout: Duration::from_secs(15),
        helper_timeout: Duration::from_secs(8),
        cleanup_timeout: Duration::from_secs(5),
        cancel: CancelToken::new(),
    });
    let result = (|| {
        let session = driver.open(OpenRequest {
            serial: "DEVICE_SERIAL".into(),
            user: 0,
        })?;
        driver.launch("com.example.app/.Main")?;
        driver.screenshot()?;
        driver.close()?;
        println!("virtual display {} closed", session.display_id);
        Ok(())
    })();
    for error in driver.shutdown() {
        eprintln!("cleanup: {error}");
    }
    result
}
```

Call `shutdown()` during host shutdown and report its returned cleanup errors. Do not retry a timed-out action automatically.

## Node.js subprocess example

Run the standalone [demo](examples/node.mjs) from the repository root with `node examples/node.mjs`, after building the binary and adding it to `PATH`. Replace the placeholder serial/component first. Its [small client](examples/node-client.mjs) uses only Node.js standard modules, waits for protocol-1 ready, correlates response IDs, and rejects and clears all pending promises on malformed output, pipe failure, spawn failure, or stdout EOF. Errors retain the driver's structured response.

```js
import { spawn } from "node:child_process";
import { createClient } from "./node-client.mjs";

const child = spawn("android-driver", ["--helper", "helper/build/virtual-display-helper.zip"], {
  stdio: ["pipe", "pipe", "inherit"],
});
const client = createClient(child);

function checked(response) {
  if (!response.ok) {
    throw Object.assign(new Error(response.error.message), { driverError: response.error });
  }
  return response.result;
}

try {
  checked(await client.request("open", { serial: "DEVICE_SERIAL", user: 0 }));
  checked(await client.request("launch", { component: "com.example.app/.Main" }));
  const screenshot = checked(await client.request("screenshot"));
  console.log(`captured ${screenshot.width}x${screenshot.height}`);
  checked(await client.request("close"));
} catch (error) {
  console.error(error);
  process.exitCode = 1;
} finally {
  try {
    await client.end();
  } catch (error) {
    console.error("driver shutdown:", error);
    process.exitCode = 1;
  }
}
```

The outer `finally` ends stdin and waits for child `close` even if open or another operation fails; stdin EOF requests driver cleanup. Transport failures terminate the child and reject pending requests. The demo does not automatically replay operations. Production clients must additionally impose request and exit deadlines and enforce message/queue bounds; this small example does not guarantee completion if a child remains alive but never responds or exits. Check `response.error?.details?.result_unknown === true`: stop automatic replay, reconcile the application outcome independently, and establish a fresh session before further actions. The flag is a JSON boolean; read-only helper timeouts and policy rejections do not receive it, and its absence is not proof that an action had no effect. A timed-out side-effecting request must not be resent.

## Unicode input

There is intentionally no text operation. `adb shell input text` does not provide a reliable arbitrary-Unicode contract, especially for CJK text. A zero-install delivery mechanism has not been verified. Future support requires a separate experiment that checks Unicode scalar coverage, spaces and control characters, Android user selection, IME restoration, and behavior on a generic app; until then the protocol reports both text capabilities as false.

## Offline verification

`cargo check --workspace --all-targets` compiles the Rust example without running it. With Node.js 24, run `node --test examples/node-client.test.mjs` to validate the Node client against local fake JSON-lines peers and check that both README snippets exactly match their standalone files. Node.js is only needed for this separate example check, not for Cargo. These checks do not execute either example against a device; their real-device execution remains unverified.

Integration tests launch a fake `adb` executable and a fake helper. They cover request bounds, device locking, session lifecycle, EOF cancellation, launch/input guards, helper death, timeouts, and cleanup errors without contacting an Android device. These tests establish the local protocol contract only. Android 14 real-device behavior, crash cleanup, cross-host ownership, and Unicode input remain unverified.

## Design boundary

The device transport, display session, and operation primitives belong here. Application page definitions, navigation policy, retries, perception, and application success criteria belong to callers. A successful driver response means the requested device operation was accepted and verified where the operation supports verification; it does not mean the caller's goal page was reached.
