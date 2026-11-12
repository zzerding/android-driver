mod common;

use std::time::Duration;

use serde_json::json;

use android_driver_core::png::{base64, validate_png};

use common::{
    CANONICAL_COMPONENT, COMPONENT, HEIGHT, SERIAL, TestEnv, WIDTH, error_code, error_message,
    launch_request, ok_result, open_and_launch, open_request, tap_fields,
};

#[test]
fn session_round_trip_routes_commands_and_input_to_the_virtual_display() {
    let env = TestEnv::new();
    let mut driver = env.spawn();
    assert_eq!(driver.startup["capabilities"]["text_input"], false);
    assert_eq!(driver.startup["capabilities"]["unicode_text_input"], false);

    let status = ok_result(&driver.request("status", json!({})));
    assert_eq!(status["state"], "idle");

    let devices = ok_result(&driver.request("list_devices", json!({})));
    assert_eq!(devices["devices"][0]["serial"], SERIAL);
    assert_eq!(devices["devices"][0]["model"], "Fake_Phone");

    let opened = ok_result(&driver.request("open", open_request()));
    assert_eq!(opened["state"], "active");
    assert_eq!(opened["display_id"], 7);
    assert_eq!(opened["width"], WIDTH);
    assert_eq!(opened["height"], HEIGHT);
    assert_eq!(opened["user"], 0);

    let active = ok_result(&driver.request("status", json!({})));
    assert_eq!(active["state"], "active");
    assert_eq!(active["serial"], SERIAL);

    let shot = ok_result(&driver.request("screenshot", json!({})));
    assert_eq!(shot["display_id"], 7);
    assert_eq!(shot["width"], WIDTH);
    assert_eq!(shot["height"], HEIGHT);
    let png = base64::decode(shot["png_base64"].as_str().unwrap()).unwrap();
    validate_png(&png, WIDTH, HEIGHT).unwrap();

    let info =
        ok_result(&driver.request("activity_info", json!({"component": COMPONENT, "user": 0})));
    assert_eq!(info["launch_mode"], 0);
    assert_eq!(info["task_affinity"], "com.example.app");
    assert_eq!(info["exported"], true);

    let launched = ok_result(&driver.request("launch", launch_request()));
    assert_eq!(launched["display_id"], 7);
    assert_eq!(launched["component"], CANONICAL_COMPONENT);
    assert_eq!(launched["package"], "com.example.app");
    assert_eq!(launched["user"], 0);

    ok_result(&driver.request("tap", tap_fields(10, 20)));
    ok_result(&driver.request(
        "swipe",
        json!({
            "width": WIDTH,
            "height": HEIGHT,
            "x1": 1,
            "y1": 2,
            "x2": 3,
            "y2": 4,
            "duration_ms": 250,
        }),
    ));
    ok_result(&driver.request("key", json!({"keycode": 4})));

    let after_input = ok_result(&driver.request("status", json!({})));
    assert_eq!(after_input["target_component"], CANONICAL_COMPONENT);

    let closed = ok_result(&driver.request("close", json!({})));
    assert_eq!(closed["state"], "idle");
    assert_eq!(env.lock_file_count(), 1, "the lock file must persist");

    let after_close = ok_result(&driver.request("status", json!({})));
    assert_eq!(after_close["state"], "idle");
    ok_result(&driver.request("list_devices", json!({})));

    let helper_call = env
        .adb_log()
        .into_iter()
        .find(|line| line.iter().any(|argument| argument == "app_process"))
        .expect("helper invocation");
    assert!(
        helper_call
            .iter()
            .any(|argument| argument.starts_with("CLASSPATH=/data/local/tmp/android-driver-"))
    );
    assert!(
        helper_call
            .iter()
            .any(|argument| argument == "io.github.zzerding.androiddriver.VirtualDisplayServer")
    );
    assert!(helper_call.iter().any(|argument| argument == "360"));
    assert!(helper_call.iter().any(|argument| argument == "640"));
    assert!(helper_call.iter().any(|argument| argument == "420"));

    assert!(env.adb_contains(&[
        "shell",
        "am",
        "start-activity",
        "--user",
        "0",
        "--display",
        "7",
        "-f",
        "0x18000000",
        "-n",
        CANONICAL_COMPONENT,
    ]));
    assert!(env.adb_contains(&["shell", "input", "-d", "7", "tap", "10", "20"]));
    assert!(env.adb_contains(&[
        "shell", "input", "-d", "7", "swipe", "1", "2", "3", "4", "250"
    ]));
    assert!(env.adb_contains(&["shell", "input", "-d", "7", "keyevent", "4"]));
    assert!(env.adb_contains(&["shell", "rm", "-f"]));
    assert!(env.helper_log_contains("\"command\":\"close\""));

    let jar = env
        .adb_argument_starting_with("/data/local/tmp/android-driver-")
        .expect("pushed helper jar path");
    assert!(
        env.adb_log().iter().any(|line| {
            line.iter().any(|argument| argument == "push")
                && line.iter().any(|argument| argument == &jar)
        }),
        "adb log must show the helper jar push: {jar}"
    );

    driver.close_stdin();
    assert_eq!(driver.wait_exit(Duration::from_secs(5)), 0);
}

#[test]
fn a_second_process_gets_busy_until_the_first_cleanup_finishes() {
    let env = TestEnv::new();
    let mut first = env.spawn_with(&[], &[("FAKE_ADB_CLEANUP_DELAY_MS", "500")]);
    ok_result(&first.request("open", open_request()));

    let mut second = env.spawn_with(&[], &[]);
    let busy = second.request("open", open_request());
    assert_eq!(error_code(&busy), "Busy");
    assert!(!env.adb_contains(&["push", "/data/local/tmp"]));

    let close_id = first.send_op("close", json!({}));
    std::thread::sleep(Duration::from_millis(80));
    let still_busy = second.request("open", open_request());
    assert_eq!(
        error_code(&still_busy),
        "Busy",
        "the lock must be held until cleanup completes"
    );

    let close_response = first.recv().expect("close response");
    assert_eq!(close_response["id"], close_id);
    ok_result(&close_response);

    let reopened = ok_result(&second.request("open", open_request()));
    assert_eq!(reopened["state"], "active");
    ok_result(&second.request("close", json!({})));
    assert_eq!(
        env.lock_file_count(),
        1,
        "the lock file must not be unlinked"
    );

    first.close_stdin();
    second.close_stdin();
    assert_eq!(first.wait_exit(Duration::from_secs(5)), 0);
    assert_eq!(second.wait_exit(Duration::from_secs(5)), 0);
}

#[test]
fn stdin_eof_cancels_a_pending_operation_and_cleans_up_promptly() {
    let env = TestEnv::new();
    let mut driver = env.spawn_with(&[], &[("FAKE_HELPER_SNAPSHOT_HANG", "1")]);
    ok_result(&driver.request("open", open_request()));

    let _screenshot_id = driver.send_op("screenshot", json!({}));
    env.wait_for_snapshot_request(Duration::from_secs(3));
    driver.close_stdin();

    assert_eq!(
        driver.wait_exit(Duration::from_secs(5)),
        0,
        "EOF shutdown must clean up: {}",
        driver.stderr_text()
    );
    assert!(env.helper_log_contains("\"command\":\"close\""));
    assert!(env.adb_contains(&["shell", "rm", "-f"]));
    assert!(!env.state().join("display-active").exists());

    let mut replacement = env.spawn();
    let opened = ok_result(&replacement.request("open", open_request()));
    assert_eq!(
        opened["state"], "active",
        "the device lock must be released"
    );
    ok_result(&replacement.request("close", json!({})));
    replacement.close_stdin();
    assert_eq!(replacement.wait_exit(Duration::from_secs(5)), 0);
}

#[test]
fn malformed_oversized_duplicate_and_unknown_requests_get_structured_errors() {
    let env = TestEnv::new();
    let mut driver = env.spawn();

    driver.send_raw("this is not json");
    let response = driver.recv().unwrap();
    assert_eq!(response["id"], serde_json::Value::Null);
    assert_eq!(error_code(&response), "InvalidArgument");

    driver.send_raw(r#"{"id":1,"op":"status","extra":true}"#);
    let response = driver.recv().unwrap();
    assert_eq!(response["id"], 1);
    assert_eq!(error_code(&response), "InvalidArgument");
    assert!(error_message(&response).contains("unknown field"));

    driver.send_raw(r#"{"op":"list_devices"}"#);
    let response = driver.recv().unwrap();
    assert_eq!(response["id"], serde_json::Value::Null);
    assert_eq!(error_code(&response), "InvalidArgument");

    let oversized = format!(
        r#"{{"id":2,"op":"status","pad":"{}"}}"#,
        "a".repeat(common::MAX_REQUEST_BYTES_FOR_TESTS)
    );
    driver.send_raw(&oversized);
    let response = driver.recv().unwrap();
    assert_eq!(response["id"], serde_json::Value::Null);
    assert_eq!(error_code(&response), "InvalidArgument");
    assert!(error_message(&response).contains("limit"));

    let listed = ok_result(&driver.request("list_devices", json!({})));
    assert!(listed["devices"].as_array().is_some(), "stream recovered");

    let duplicate_id = driver.send_op("status", json!({}));
    let first = driver.recv().unwrap();
    assert_eq!(first["id"], duplicate_id);
    ok_result(&first);

    driver.send_raw(&format!(r#"{{"id":{duplicate_id},"op":"status"}}"#));
    let duplicate = driver.recv().unwrap();
    assert_eq!(error_code(&duplicate), "InvalidArgument");
    assert!(error_message(&duplicate).contains("greater"));

    driver.send_raw(r#"{"id":90,"op":"teleport"}"#);
    let unknown = driver.recv().unwrap();
    assert_eq!(unknown["id"], 90);
    assert_eq!(error_code(&unknown), "InvalidArgument");

    ok_result(&driver.request("status", json!({})));
    driver.close_stdin();
    assert_eq!(driver.wait_exit(Duration::from_secs(5)), 0);
}

#[test]
fn launch_guards_refuse_locked_screens_and_unsafe_task_reuse() {
    let env = TestEnv::new();
    let mut driver = env.spawn_with(&[], &[("FAKE_HELPER_LAUNCH_MODE", "3")]);
    ok_result(&driver.request("open", open_request()));

    let unsafe_mode = driver.request("launch", launch_request());
    assert_eq!(error_code(&unsafe_mode), "Blocked");
    assert!(
        !env.adb_contains(&["shell", "am", "start-activity"]),
        "unsafe launchMode must be rejected before am runs"
    );

    env.set_window_policy(true);
    let locked = driver.request("launch", launch_request());
    assert_eq!(error_code(&locked), "Blocked");
    assert!(
        !env.adb_contains(&["shell", "am", "start-activity"]),
        "a locked screen must be rejected before am runs"
    );

    env.set_window_policy(false);
    env.set_physical_activity(COMPONENT, 0);
    let occupied = driver.request("launch", launch_request());
    assert_eq!(error_code(&occupied), "Blocked");
    assert!(!env.adb_contains(&["shell", "am", "start-activity"]));

    ok_result(&driver.request("close", json!({})));
    driver.close_stdin();
    assert_eq!(driver.wait_exit(Duration::from_secs(5)), 0);
}

#[test]
fn open_requires_a_confirmed_unlock_and_fails_without_a_helper() {
    let env = TestEnv::new();
    env.set_window_policy(true);
    let mut driver = env.spawn();
    let locked = driver.request("open", open_request());
    assert_eq!(error_code(&locked), "Blocked");
    assert!(!env.adb_contains(&["push", "/data/local/tmp"]));

    env.set_window_policy(false);
    let opened = ok_result(&driver.request("open", open_request()));
    assert_eq!(opened["state"], "active");
    ok_result(&driver.request("close", json!({})));

    std::fs::write(
        env.state().join("window-policy"),
        "unrecognized keyguard dump\n",
    )
    .unwrap();
    let unknown = driver.request("open", open_request());
    assert_eq!(error_code(&unknown), "Blocked");
    driver.close_stdin();
    assert_eq!(driver.wait_exit(Duration::from_secs(5)), 0);

    let missing_env = TestEnv::new();
    let mut missing = missing_env.spawn_without_helper();
    let error = missing.request("open", open_request());
    assert_eq!(error_code(&error), "Unsupported");
    assert!(error_message(&error).contains("helper"));
    assert!(
        !missing_env.adb_contains(&["push", "/data/local/tmp"]),
        "missing helper must never fall back to another display path"
    );
    let state = ok_result(&missing.request("status", json!({})));
    assert_eq!(state["state"], "idle");
    missing.close_stdin();
    assert_eq!(missing.wait_exit(Duration::from_secs(5)), 0);
}

#[test]
fn input_is_blocked_by_lockscreen_physical_use_and_wrong_virtual_target() {
    let env = TestEnv::new();
    let mut driver = open_and_launch(&env, &[], &[]);

    env.set_window_policy(true);
    let locked = driver.request("tap", tap_fields(10, 20));
    assert_eq!(error_code(&locked), "Blocked");
    assert_eq!(env.adb_count(&["shell", "input", "-d", "7", "tap"]), 0);

    env.set_window_policy(false);
    env.set_physical_activity(CANONICAL_COMPONENT, 10);
    ok_result(&driver.request("tap", tap_fields(11, 21)));
    assert_eq!(env.adb_count(&["shell", "input", "-d", "7", "tap"]), 1);

    env.set_physical_activity(CANONICAL_COMPONENT, 0);
    let occupied = driver.request("tap", tap_fields(12, 22));
    assert_eq!(error_code(&occupied), "Blocked");
    assert_eq!(env.adb_count(&["shell", "input", "-d", "7", "tap"]), 1);

    env.set_default_activities();
    env.set_virtual_activity(7, "com.other.app/.Main", 0);
    let wrong_screen = driver.request("tap", tap_fields(13, 23));
    assert_eq!(error_code(&wrong_screen), "Blocked");
    assert_eq!(env.adb_count(&["shell", "input", "-d", "7", "tap"]), 1);

    env.set_virtual_activity(7, CANONICAL_COMPONENT, 0);
    ok_result(&driver.request("tap", tap_fields(14, 24)));
    assert_eq!(env.adb_count(&["shell", "input", "-d", "7", "tap"]), 2);

    ok_result(&driver.request("close", json!({})));
    driver.close_stdin();
    assert_eq!(driver.wait_exit(Duration::from_secs(5)), 0);
}

#[test]
fn invalid_arguments_are_rejected_without_side_effects() {
    let env = TestEnv::new();
    let mut driver = env.spawn();

    let bad_serial = driver.request("open", json!({"serial": "-bad", "user": 0}));
    assert_eq!(error_code(&bad_serial), "InvalidArgument");
    let bad_user = driver.request("open", json!({"serial": SERIAL, "user": 1000}));
    assert_eq!(error_code(&bad_user), "InvalidArgument");

    ok_result(&driver.request("open", open_request()));

    for (op, fields) in [
        ("tap", tap_fields(WIDTH, 0)),
        (
            "tap",
            json!({"width": 100, "height": 100, "x": 10, "y": 10}),
        ),
        (
            "swipe",
            json!({
                "width": WIDTH, "height": HEIGHT,
                "x1": 1, "y1": 2, "x2": 3, "y2": 4, "duration_ms": 0,
            }),
        ),
        ("key", json!({"keycode": 26})),
        (
            "launch",
            json!({"component": "com.example.app/.Main; rm -rf /"}),
        ),
        (
            "activity_info",
            json!({"component": "com.example.app/.Main", "user": 1}),
        ),
        (
            "activity_info",
            json!({"component": "com.example.app/../Other", "user": 0}),
        ),
        ("open", open_request()),
    ] {
        let response = driver.request(op, fields);
        let code = error_code(&response);
        assert!(
            code == "InvalidArgument" || code == "InvalidState",
            "{op} returned {code}: {response}"
        );
    }

    assert!(!env.adb_contains(&["shell", "input"]));
    assert!(!env.adb_contains(&["shell", "am", "start-activity"]));
    let status = ok_result(&driver.request("status", json!({})));
    assert_eq!(status["state"], "active");

    ok_result(&driver.request("close", json!({})));
    driver.close_stdin();
    assert_eq!(driver.wait_exit(Duration::from_secs(5)), 0);
}

#[test]
fn an_input_timeout_reports_timeout_once_and_invalidates_the_session() {
    let env = TestEnv::new();
    let mut driver = env.spawn_with(
        &["--adb-timeout-ms", "2000"],
        &[("FAKE_ADB_INPUT_DELAY_MS", "5000")],
    );
    ok_result(&driver.request("open", open_request()));
    ok_result(&driver.request("launch", launch_request()));

    let timed_out = driver.request("tap", tap_fields(10, 20));
    assert_eq!(error_code(&timed_out), "Timeout");
    assert_eq!(
        env.adb_count(&["shell", "input", "-d", "7", "tap", "10", "20"]),
        1,
        "a timed-out side effect must not be replayed"
    );
    assert!(env.adb_contains(&["shell", "rm", "-f"]));

    let status = ok_result(&driver.request("status", json!({})));
    assert_eq!(status["state"], "failed");
    let after = driver.request("tap", tap_fields(10, 20));
    assert_eq!(error_code(&after), "InvalidState");

    let mut replacement = env.spawn();
    let opened = ok_result(&replacement.request("open", open_request()));
    assert_eq!(
        opened["state"], "active",
        "the device lock must be released"
    );
    ok_result(&replacement.request("close", json!({})));
    replacement.close_stdin();
    assert_eq!(replacement.wait_exit(Duration::from_secs(5)), 0);

    driver.close_stdin();
    assert_eq!(driver.wait_exit(Duration::from_secs(5)), 0);
}

#[test]
fn helper_death_is_reported_as_a_disconnect_and_releases_the_lock() {
    let env = TestEnv::new();
    let mut driver = env.spawn_with(&[], &[("FAKE_HELPER_EXIT_AFTER_READY", "1")]);
    ok_result(&driver.request("open", open_request()));

    let screenshot = driver.request("screenshot", json!({}));
    assert_eq!(error_code(&screenshot), "DeviceDisconnected");
    let status = ok_result(&driver.request("status", json!({})));
    assert_eq!(status["state"], "failed");
    assert!(env.adb_contains(&["shell", "rm", "-f"]));

    let mut replacement = env.spawn();
    let opened = ok_result(&replacement.request("open", open_request()));
    assert_eq!(
        opened["state"], "active",
        "the device lock must be released"
    );
    replacement.close_stdin();
    let _ = replacement.wait_exit(Duration::from_secs(5));

    driver.close_stdin();
    assert_eq!(driver.wait_exit(Duration::from_secs(5)), 0);
}

#[test]
fn cleanup_failures_are_surfaced_and_the_lock_is_released_after_attempts() {
    let env = TestEnv::new();
    let mut driver = env.spawn_with(
        &["--cleanup-timeout-ms", "800"],
        &[("FAKE_HELPER_IGNORE_CLOSE", "1")],
    );
    ok_result(&driver.request("open", open_request()));

    let close = driver.request("close", json!({}));
    assert_eq!(error_code(&close), "CleanupFailed");
    let errors = close["error"]["details"]["cleanup_errors"]
        .as_array()
        .expect("cleanup_errors array");
    assert!(!errors.is_empty());
    assert!(
        env.state().join("display-active").is_file(),
        "the fixture keeps the display alive so cleanup cannot verify removal"
    );
    assert!(env.adb_contains(&["shell", "rm", "-f"]));
    assert_eq!(env.lock_file_count(), 1);

    driver.close_stdin();
    assert_eq!(driver.wait_exit(Duration::from_secs(5)), 0);

    let mut replacement = env.spawn_with(
        &["--cleanup-timeout-ms", "800"],
        &[("FAKE_HELPER_IGNORE_CLOSE", "1")],
    );
    let opened = ok_result(&replacement.request("open", open_request()));
    assert_eq!(
        opened["state"], "active",
        "the lock must be released after all cleanup attempts finish"
    );
    replacement.close_stdin();
    assert_eq!(replacement.wait_exit(Duration::from_secs(5)), 1);
}

#[test]
fn a_launch_failure_reported_by_am_invalidates_the_session() {
    let env = TestEnv::new();
    let mut driver = env.spawn_with(&[], &[("FAKE_ADB_START_FAIL", "1")]);
    ok_result(&driver.request("open", open_request()));

    let launch = driver.request("launch", launch_request());
    assert_eq!(error_code(&launch), "Internal");
    assert!(error_message(&launch).contains("am start-activity"));
    assert_eq!(
        env.adb_count(&["shell", "am", "start-activity"]),
        1,
        "a failed launch must not be replayed"
    );
    assert!(env.adb_contains(&["shell", "rm", "-f"]));

    let status = ok_result(&driver.request("status", json!({})));
    assert_eq!(status["state"], "failed");

    driver.close_stdin();
    assert_eq!(driver.wait_exit(Duration::from_secs(5)), 0);
}
