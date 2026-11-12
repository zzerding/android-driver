# Agent Instructions

## Workspace

This repository is a Rust workspace with an Android Java helper. Keep application-specific flows out of the driver. Never use a real Android device as an implicit test fixture.

## Verification

After each change:

1. `cargo check --workspace --all-targets`
2. `cargo fmt --all -- --check`
3. For behavior changes, run the smallest relevant test, then `cargo test --workspace` when cross-crate behavior is affected.
4. Run `cargo clippy --workspace --all-targets` when lint-relevant code changes.

Build the helper only when Java source/build configuration changes and SDK inputs are available:

```sh
ANDROID_JAR=/path/to/android-34/android.jar D8_JAR=/path/to/34/lib/d8.jar bash helper/build.sh
```

`cargo build` is for an executable artifact, not a type check. Do not enable Cargo incremental compilation. Never run device commands against a real serial in automated tests.

## Contracts

- Keep stdout protocol-only and error results structured.
- Side-effecting operation timeouts are unknown outcomes: no replay and invalidate the session.
- Preserve host-local device-lock ownership through cleanup attempts.
- Unknown keyguard/display state must fail closed.
- No physical-screen fallback, app-specific navigation, text input, perception, CDP, or application-specific success criteria.
- Do not claim real-device, post-crash cleanup, cross-host lock, or Unicode guarantees without corresponding verification.
