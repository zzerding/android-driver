# Protocol v1

Normative JSON-lines contract for `android-driver`. Protocol messages use UTF-8, one JSON object per line. Standard output is reserved for protocol frames; logs are written to standard error.

## Startup

The first frame is:

```json
{"type":"ready","protocol":1,"version":"0.1.0","capabilities":{"operations":["list_devices","open","status","screenshot","activity_info","launch","tap","swipe","key","close"],"virtual_display":true,"android_sdk":34,"adb_uid":2000,"raw_pixel_input":true,"text_input":false,"unicode_text_input":false}}
```

Clients must reject an unsupported protocol version or a missing capability they require. The ready frame includes request, response, PNG and safe-keycode limits.

## Requests

Every request has a positive integer `id` greater than the previous request ID and a string `op`. Unknown fields and operation names are errors. Maximum encoded request size is 1 MiB. Commands run sequentially.

| Operation | Required fields | Result |
|---|---|---|
| `list_devices` | none | Connected device records |
| `open` | `serial`, `user` | Session identity and display dimensions |
| `status` | none | `idle`, `active`, or `failed` state |
| `screenshot` | none | PNG base64, timestamp, display ID and dimensions |
| `activity_info` | `component`, `user` | Canonical component, launch mode, task affinity and exported flag |
| `launch` | `component` | Verified resumed component, package, user and display |
| `tap` | `width`, `height`, `x`, `y` | Accepted input target and dimensions |
| `swipe` | `width`, `height`, `x1`, `y1`, `x2`, `y2`, optional `duration_ms` | Accepted input target and dimensions |
| `key` | `keycode` | Accepted input target and dimensions |
| `close` | none | `idle` after verified cleanup |

Input coordinates are unsigned, zero-based screenshot pixels. The passed width and height must equal the active session dimensions. Points must be within the half-open bounds. Swipe duration defaults to 300 ms and is limited to 1..=10000 ms. Keycodes must be in the ready frame allowlist.

`open` takes a cross-process, host-local exclusive lock for the adb-server endpoint and serial. Only Android SDK 34 and shell UID 2000 are accepted. An open session binds one serial and user. Input is unavailable until a target is launched successfully. No physical-display fallback occurs.

## Responses

Success:

```json
{"type":"response","id":4,"ok":true,"result":{"display_id":12,"width":1080,"height":2400,"png_base64":"..."}}
```

Failure:

```json
{"type":"response","id":4,"ok":false,"error":{"code":"Blocked","message":"device is locked; refusing to act"}}
```

Codes include `Busy`, `Blocked`, `Unsupported`, `InvalidArgument`, `DeviceDisconnected`, `Timeout`, `CleanupFailed`, `InvalidState`, `Cancelled`, `Protocol`, and `Internal`. A parse failure includes the request ID only if one could be safely extracted.

When launch dispatch, input dispatch (`tap`, `swipe`, or `key`), or post-launch verification times out, the error includes the JSON boolean `details.result_unknown: true`:

```json
{"type":"response","id":5,"ok":false,"error":{"code":"Timeout","message":"adb command timed out after 15000 ms","details":{"result_unknown":true}}}
```

The operation may already have taken effect. The driver never replays it and invalidates the session. Clients must treat `error.details.result_unknown === true` as an unknown outcome: do not automatically resend the action, reconcile the application outcome independently, and establish a fresh session before further actions. Existing diagnostics such as `args`, `stderr`, `last_observation`, or `cleanup_errors` can accompany this flag. Read-only helper timeouts and policy rejections do not receive it; its absence does not prove that an operation had no effect.

After stdin EOF, pending work is cancelled and bounded cleanup starts. `close` succeeds only when helper shutdown, jar removal, and virtual-display removal have all been confirmed. Incomplete or uncertain cleanup is reported and logged; after cleanup attempts the host lock is released.

The protocol does not define Android text input, app navigation, page state, OCR, model requests, CDP, or token handshakes.
