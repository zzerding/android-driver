#![allow(dead_code)]

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

pub const SERIAL: &str = "FAKE0001";
pub const COMPONENT: &str = "com.example.app/.Main";
pub const CANONICAL_COMPONENT: &str = "com.example.app/com.example.app.Main";
pub const WIDTH: u32 = 360;
pub const HEIGHT: u32 = 640;
pub const DPI: u32 = 420;
pub const MAX_REQUEST_BYTES_FOR_TESTS: usize = android_driver_core::protocol::MAX_REQUEST_BYTES;

static UNIQUE: AtomicU64 = AtomicU64::new(0);

pub struct TestEnv {
    root: PathBuf,
    state: PathBuf,
    lock_dir: PathBuf,
    helper: PathBuf,
    fake_adb: PathBuf,
}

impl TestEnv {
    pub fn new() -> Self {
        let unique = UNIQUE.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "android-driver-test-{}-{unique}-{nanos}",
            std::process::id()
        ));
        let state = root.join("state");
        let lock_dir = root.join("locks");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&lock_dir).unwrap();
        write_test_png(&state.join("snapshot.png"), WIDTH, HEIGHT);
        let fake_adb = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("fake-adb.sh");
        assert!(fake_adb.is_file(), "missing fixture {}", fake_adb.display());
        let helper = root.join("helper.jar");
        fs::write(&helper, b"test helper placeholder").unwrap();
        let env = Self {
            root,
            state,
            lock_dir,
            helper,
            fake_adb,
        };
        env.set_window_policy(false);
        env.set_default_activities();
        env
    }

    pub fn state(&self) -> &Path {
        &self.state
    }

    pub fn lock_file_count(&self) -> usize {
        fs::read_dir(&self.lock_dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_name().to_string_lossy().ends_with(".lock"))
                    .count()
            })
            .unwrap_or(0)
    }

    pub fn set_window_policy(&self, locked: bool) {
        let value = if locked { "true" } else { "false" };
        fs::write(
            self.state.join("window-policy"),
            format!(
                "WINDOW MANAGER POLICY STATE (dumpsys window policy)\nKeyguardServiceDelegate\n  showing={value}\n  mShowingLockscreen={value}\n"
            ),
        )
        .unwrap();
    }

    pub fn set_default_activities(&self) {
        fs::write(
            self.state.join("activities"),
            "ACTIVITY MANAGER ACTIVITIES (dumpsys activity activities)\n\
Display #0 (activities from top to bottom):\n  \
mResumedActivity: ActivityRecord{fake0 u0 com.android.launcher3/.Launcher t1}\n",
        )
        .unwrap();
    }

    pub fn set_physical_activity(&self, component: &str, user: u32) {
        fs::write(
            self.state.join("activities"),
            format!(
                "ACTIVITY MANAGER ACTIVITIES (dumpsys activity activities)\n\
Display #0 (activities from top to bottom):\n  \
mResumedActivity: ActivityRecord{{fake0 u{user} {component} t1}}\n"
            ),
        )
        .unwrap();
    }

    pub fn set_virtual_activity(&self, display_id: u32, component: &str, user: u32) {
        fs::write(
            self.state.join("virtual-activities"),
            format!("  mResumedActivity: ActivityRecord{{fake1 u{user} {component} t99}}\n"),
        )
        .unwrap();
        fs::write(self.state.join("display-id"), display_id.to_string()).unwrap();
    }

    pub fn helper_log_contains(&self, line: &str) -> bool {
        fs::read_to_string(self.state.join("helper-log"))
            .map(|contents| contents.lines().any(|entry| entry.contains(line)))
            .unwrap_or(false)
    }

    pub fn adb_log(&self) -> Vec<Vec<String>> {
        let Ok(contents) = fs::read_to_string(self.state.join("log")) else {
            return Vec::new();
        };
        contents
            .lines()
            .map(|line| {
                line.split('\t')
                    .filter(|part| !part.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .filter(|line: &Vec<String>| !line.is_empty())
            .collect()
    }

    pub fn adb_contains(&self, needle: &[&str]) -> bool {
        self.adb_log()
            .iter()
            .any(|line| contains_subsequence(line, needle))
    }

    pub fn adb_count(&self, needle: &[&str]) -> usize {
        self.adb_log()
            .iter()
            .filter(|line| contains_subsequence(line, needle))
            .count()
    }

    pub fn adb_argument_starting_with(&self, prefix: &str) -> Option<String> {
        self.adb_log()
            .into_iter()
            .flatten()
            .find(|argument| argument.starts_with(prefix))
    }

    pub fn wait_for_snapshot_request(&self, timeout: Duration) {
        wait_until(timeout, || self.state.join("snapshot-seen").is_file());
    }

    pub fn spawn(&self) -> DriverProcess {
        self.spawn_with(&[], &[])
    }

    pub fn spawn_with(&self, extra_args: &[&str], envs: &[(&str, &str)]) -> DriverProcess {
        self.spawn_inner(extra_args, envs, true)
    }

    pub fn spawn_without_helper(&self) -> DriverProcess {
        self.spawn_inner(&[], &[], false)
    }

    fn spawn_inner(
        &self,
        extra_args: &[&str],
        envs: &[(&str, &str)],
        include_helper: bool,
    ) -> DriverProcess {
        let mut command = Command::new(env!("CARGO_BIN_EXE_android-driver"));
        command
            .arg("--adb")
            .arg(&self.fake_adb)
            .arg("--adb-server")
            .arg("127.0.0.1:5037")
            .arg("--lock-dir")
            .arg(&self.lock_dir)
            .arg("--adb-timeout-ms")
            .arg("5000")
            .arg("--helper-timeout-ms")
            .arg("5000")
            .arg("--cleanup-timeout-ms")
            .arg("2000");
        if include_helper {
            command.arg("--helper").arg(&self.helper);
        }
        for argument in extra_args {
            command.arg(argument);
        }
        command
            .env("FAKE_ADB_STATE", &self.state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in envs {
            command.env(key, value);
        }
        let mut process = DriverProcess::start(command);
        let ready = process
            .recv_optional(Duration::from_secs(10))
            .expect("driver did not print a startup frame");
        assert_eq!(ready["type"], "ready", "{ready}");
        assert_eq!(ready["protocol"], 1, "{ready}");
        process.startup = ready;
        process
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub struct DriverProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    close_stdout_at_id: Arc<AtomicU64>,
    stdout_closed: Receiver<()>,
    stderr: Arc<Mutex<Vec<u8>>>,
    status: Option<ExitStatus>,
    next_id: u64,
    pub startup: Value,
}

impl DriverProcess {
    fn start(mut command: Command) -> Self {
        let mut child = command.spawn().expect("cannot start android-driver");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout");
        let stderr_pipe = child.stderr.take().expect("stderr");
        let (sender, lines) = mpsc::channel();
        let close_stdout_at_id = Arc::new(AtomicU64::new(0));
        let thread_close_stdout_at_id = Arc::clone(&close_stdout_at_id);
        let (stdout_closed_sender, stdout_closed) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            for line in (&mut reader).lines() {
                match line {
                    Ok(line) => {
                        let close_id = thread_close_stdout_at_id.load(Ordering::Acquire);
                        let close = close_id != 0
                            && serde_json::from_str::<Value>(&line)
                                .is_ok_and(|frame| frame["id"].as_u64() == Some(close_id));
                        if sender.send(line).is_err() {
                            break;
                        }
                        if close {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            drop(reader);
            let _ = stdout_closed_sender.send(());
        });
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let thread_stderr = Arc::clone(&stderr);
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr_pipe);
            let mut buffer = [0u8; 2048];
            loop {
                use std::io::Read;
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => thread_stderr
                        .lock()
                        .expect("stderr lock")
                        .extend_from_slice(&buffer[..read]),
                    Err(_) => break,
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
            close_stdout_at_id,
            stdout_closed,
            stderr,
            status: None,
            next_id: 1,
            startup: Value::Null,
        }
    }

    pub fn send_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin is closed");
        stdin.write_all(line.as_bytes()).expect("write request");
        stdin.write_all(b"\n").expect("write newline");
        stdin.flush().expect("flush request");
    }

    pub fn send_op(&mut self, op: &str, extra: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let mut object = Map::new();
        object.insert("id".into(), Value::from(id));
        object.insert("op".into(), Value::from(op));
        if let Some(fields) = extra.as_object() {
            for (key, value) in fields {
                object.insert(key.clone(), value.clone());
            }
        }
        self.send_raw(&serde_json::to_string(&Value::Object(object)).unwrap());
        id
    }

    pub fn request(&mut self, op: &str, extra: Value) -> Value {
        let id = self.send_op(op, extra);
        let response = self
            .recv()
            .unwrap_or_else(|| panic!("no response for {op}; stderr: {}", self.stderr_text()));
        assert_eq!(response["id"], id, "response id mismatch: {response}");
        response
    }

    pub fn recv(&mut self) -> Option<Value> {
        self.recv_optional(Duration::from_secs(15))
    }

    pub fn recv_optional(&mut self, timeout: Duration) -> Option<Value> {
        match self.lines.recv_timeout(timeout) {
            Ok(line) => Some(
                serde_json::from_str(&line)
                    .unwrap_or_else(|error| panic!("invalid JSON from driver: {error}: {line}")),
            ),
            Err(mpsc::RecvTimeoutError::Timeout) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                None
            }
        }
    }

    pub fn close_stdin(&mut self) {
        self.stdin.take();
    }

    pub fn close_stdout(&mut self) {
        self.close_stdout_at_id
            .store(self.next_id, Ordering::Release);
        ok_result(&self.request("status", json!({})));
        self.stdout_closed
            .recv_timeout(Duration::from_secs(5))
            .expect("stdout reader did not close");
    }

    pub fn wait_exit(&mut self, timeout: Duration) -> i32 {
        if let Some(status) = self.status {
            return status.code().unwrap_or(1);
        }
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait().expect("try_wait") {
                Some(status) => {
                    self.status = Some(status);
                    return status.code().unwrap_or(1);
                }
                None if Instant::now() >= deadline => {
                    panic!(
                        "driver did not exit within {:?}; stderr: {}",
                        timeout,
                        self.stderr_text()
                    );
                }
                None => thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().expect("stderr lock")).into_owned()
    }
}

impl Drop for DriverProcess {
    fn drop(&mut self) {
        if self.status.is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub fn ok_result(response: &Value) -> Value {
    assert_eq!(response["ok"], true, "expected success, got {response}");
    response["result"].clone()
}

pub fn error_code(response: &Value) -> String {
    assert_eq!(response["ok"], false, "expected an error, got {response}");
    response["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("error has no code: {response}"))
        .to_owned()
}

pub fn error_message(response: &Value) -> String {
    response["error"]["message"]
        .as_str()
        .unwrap_or("")
        .to_owned()
}

pub fn wait_until(timeout: Duration, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("condition was not met within {timeout:?}");
}

fn contains_subsequence(line: &[String], needle: &[&str]) -> bool {
    if needle.is_empty() {
        return true;
    }
    line.windows(needle.len())
        .any(|window| window.iter().zip(needle).all(|(left, right)| left == right))
}

pub fn write_test_png(path: &Path, width: u32, height: u32) {
    let row_bytes = 1 + width as usize * 4;
    let mut raw = Vec::with_capacity(row_bytes * height as usize);
    for _ in 0..height {
        raw.push(0);
        raw.extend(std::iter::repeat_n(0u8, width as usize * 4));
    }

    let mut zlib = vec![0x78, 0x01];
    let mut position = 0;
    while position < raw.len() {
        let remaining = raw.len() - position;
        let chunk = remaining.min(65_535);
        let last = position + chunk == raw.len();
        zlib.push(if last { 1 } else { 0 });
        zlib.extend_from_slice(&(chunk as u16).to_le_bytes());
        let complement = !(chunk as u16);
        zlib.extend_from_slice(&complement.to_le_bytes());
        zlib.extend_from_slice(&raw[position..position + chunk]);
        position += chunk;
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    png.extend_from_slice(&png_chunk(b"IHDR", &header));
    png.extend_from_slice(&png_chunk(b"IDAT", &zlib));
    png.extend_from_slice(&png_chunk(b"IEND", &[]));
    fs::write(path, png).unwrap();
}

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
    bytes.extend_from_slice(kind);
    bytes.extend_from_slice(data);
    let mut crc_input = Vec::from(*kind);
    crc_input.extend_from_slice(data);
    bytes.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    bytes
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut b = 0u32;
    for byte in data {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

pub fn json_object(fields: Value) -> Value {
    fields
}

pub fn tap_fields(x: u32, y: u32) -> Value {
    json!({"width": WIDTH, "height": HEIGHT, "x": x, "y": y})
}

pub fn open_request() -> Value {
    json!({"serial": SERIAL, "user": 0})
}

pub fn launch_request() -> Value {
    json!({"component": COMPONENT})
}

pub fn open_and_launch(env: &TestEnv, extra_args: &[&str], envs: &[(&str, &str)]) -> DriverProcess {
    let mut driver = env.spawn_with(extra_args, envs);
    let opened = ok_result(&driver.request("open", open_request()));
    assert_eq!(opened["state"], "active");
    let launched = ok_result(&driver.request("launch", launch_request()));
    assert_eq!(launched["display_id"], 7);
    driver
}

pub fn queue_is_empty(lines: &[Vec<String>]) -> bool {
    lines.is_empty()
}

pub fn drain_stdout(process: &mut DriverProcess) -> VecDeque<Value> {
    let mut frames = VecDeque::new();
    while let Some(frame) = process.recv_optional(Duration::from_millis(10)) {
        frames.push_back(frame);
    }
    frames
}
