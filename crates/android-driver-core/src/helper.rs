use std::io::{BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::adb::AdbRunner;
use crate::cancel::CancelToken;
use crate::error::{DriverError, ErrorCode, Result, internal};
use crate::lines::{LineRead, read_line_bounded};

pub const HELPER_CLASS: &str = "io.github.zzerding.androiddriver.VirtualDisplayServer";
pub const EXTERNAL_HELPER_UID: u32 = 2000;
pub const ANDROID_SDK: u32 = 34;

const MAX_HELPER_LINE: usize = 12 * 1024 * 1024;
const MAX_HELPER_STDERR: usize = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(20);

pub struct HelperSpec<'a> {
    pub serial: &'a str,
    pub jar_device_path: &'a str,
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    pub nice_name: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub struct HelperInfo {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    pub uid: u32,
}

pub struct Helper {
    child: Child,
    stdin: Option<ChildStdin>,
    events: Receiver<HelperEvent>,
    stderr: Arc<Mutex<Vec<u8>>>,
    info: HelperInfo,
    next_id: u64,
    cancel: CancelToken,
}

enum HelperEvent {
    Line(Vec<u8>),
    TooLong,
    Closed(String),
}

impl Helper {
    pub fn spawn(
        adb: &AdbRunner,
        spec: &HelperSpec<'_>,
        timeout: Duration,
        cancel: CancelToken,
    ) -> Result<Self> {
        let mut helper = Self::start(adb, spec, cancel)?;
        if let Err(error) = helper.initialize(spec, timeout) {
            let cleanup = helper.shutdown(timeout);
            return Err(if cleanup.is_empty() {
                error
            } else {
                error.detail("cleanup_errors", cleanup)
            });
        }
        Ok(helper)
    }

    pub(crate) fn start(
        adb: &AdbRunner,
        spec: &HelperSpec<'_>,
        cancel: CancelToken,
    ) -> Result<Self> {
        let mut command = adb.command(Some(spec.serial));
        command
            .arg("shell")
            .arg("env")
            .arg(format!("CLASSPATH={}", spec.jar_device_path))
            .arg("app_process")
            .arg("/system/bin")
            .arg(format!("--nice-name={}", spec.nice_name))
            .arg(HELPER_CLASS)
            .arg(spec.width.to_string())
            .arg(spec.height.to_string())
            .arg(spec.dpi.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => DriverError::new(
                ErrorCode::Unsupported,
                format!(
                    "adb executable not found: {}",
                    adb_executable_name(&command)
                ),
            ),
            _ => DriverError::new(
                ErrorCode::Unsupported,
                format!("cannot start the virtual display helper through adb: {error}"),
            ),
        })?;

        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| internal("helper stdout was not captured"))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| internal("helper stderr was not captured"))?;

        let (sender, events) = mpsc::sync_channel(8);
        spawn_stdout_reader(stdout, sender);
        let stderr = spawn_stderr_reader(stderr_pipe);

        Ok(Self {
            child,
            stdin,
            events,
            stderr,
            info: HelperInfo {
                display_id: 0,
                width: spec.width,
                height: spec.height,
                dpi: spec.dpi,
                uid: 0,
            },
            next_id: 1,
            cancel,
        })
    }

    pub(crate) fn initialize(&mut self, spec: &HelperSpec<'_>, timeout: Duration) -> Result<()> {
        let ready = self.wait_reply(0, timeout)?;
        if let Some(display_id) = ready_display_id(&ready) {
            self.info.display_id = display_id;
        }
        self.info = validate_ready(&ready, spec)?;
        Ok(())
    }

    pub(crate) fn observed_display_id(&self) -> Option<u32> {
        (self.info.display_id > 0).then_some(self.info.display_id)
    }

    pub fn info(&self) -> HelperInfo {
        self.info
    }

    pub fn display_id(&self) -> u32 {
        self.info.display_id
    }

    pub fn stderr_tail(&self) -> String {
        let data = self.stderr.lock().expect("helper stderr lock");
        String::from_utf8_lossy(&data).into_owned()
    }

    pub fn exit_status(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().unwrap_or_default()
    }

    pub fn has_exited(&mut self) -> bool {
        self.exit_status().is_some()
    }

    /// Sends `close`, closes stdin (so a helper that never answers still sees
    /// EOF), and waits for the helper to exit. Any deviation from a clean
    /// zero exit is reported as a cleanup problem.
    pub fn shutdown(&mut self, timeout: Duration) -> Vec<String> {
        let mut problems = Vec::new();
        let already_exited = self.exit_status().is_some();
        if !already_exited {
            if let Some(mut stdin) = self.stdin.take() {
                let id = self.next_id;
                self.next_id += 1;
                let frame = json!({"id": id, "command": "close"});
                match serde_json::to_vec(&frame)
                    .map_err(std::io::Error::other)
                    .and_then(|bytes| {
                        stdin.write_all(&bytes)?;
                        stdin.write_all(b"\n")?;
                        stdin.flush()
                    }) {
                    Ok(()) => {}
                    Err(error) => {
                        problems.push(format!("cannot send the helper close command: {error}"));
                    }
                }
                drop(stdin);
            } else {
                problems.push("helper stdin was already closed before shutdown".to_owned());
            }
            if !wait_for_exit(&mut self.child, timeout) {
                let _ = self.child.kill();
                let _ = wait_for_exit(&mut self.child, Duration::from_secs(2));
                problems.push(format!(
                    "virtual display helper did not exit within {} ms and was killed",
                    timeout.as_millis()
                ));
            }
        }
        self.stdin = None;
        match self.exit_status() {
            Some(status) if !status.success() => {
                problems.push(format!("virtual display helper exited with {status}"));
            }
            Some(_) => {}
            None => problems.push("virtual display helper is still running".to_owned()),
        }
        problems
    }

    pub fn request(&mut self, command: &str, extra: Value, timeout: Duration) -> Result<Value> {
        self.cancel.check()?;
        let id = self.next_id;
        self.next_id += 1;
        let mut frame = Map::new();
        frame.insert("id".into(), Value::from(id));
        frame.insert("command".into(), Value::from(command));
        if let Some(extra) = extra.as_object() {
            for (key, value) in extra {
                if key != "id" && key != "command" {
                    frame.insert(key.clone(), value.clone());
                }
            }
        }
        let mut bytes = serde_json::to_vec(&Value::Object(frame))
            .map_err(|error| internal(format!("cannot encode helper request: {error}")))?;
        bytes.push(b'\n');
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| internal("virtual display helper stdin is closed"))?;
        stdin.write_all(&bytes).map_err(|error| {
            DriverError::new(
                ErrorCode::DeviceDisconnected,
                format!("cannot write to the virtual display helper: {error}"),
            )
        })?;
        stdin.flush().map_err(|error| {
            DriverError::new(
                ErrorCode::DeviceDisconnected,
                format!("cannot flush the virtual display helper: {error}"),
            )
        })?;
        self.wait_reply(id, timeout).map_err(|error| {
            if error.code == ErrorCode::Timeout {
                error.detail("helper_command", command)
            } else {
                error
            }
        })
    }

    fn wait_reply(&mut self, expected_id: u64, timeout: Duration) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            self.cancel.check()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(DriverError::new(
                    ErrorCode::Timeout,
                    format!(
                        "virtual display helper did not reply within {} ms",
                        timeout.as_millis()
                    ),
                )
                .detail("stderr", self.stderr_tail()));
            }
            match self.events.recv_timeout(remaining.min(POLL_INTERVAL)) {
                Ok(HelperEvent::Line(line)) => {
                    let value: Value = serde_json::from_slice(&line).map_err(|error| {
                        internal(format!(
                            "virtual display helper emitted invalid JSON: {error}"
                        ))
                    })?;
                    let id = value.get("id").and_then(Value::as_u64).ok_or_else(|| {
                        internal("virtual display helper reply has no numeric id")
                    })?;
                    if id != expected_id {
                        return Err(internal(format!(
                            "virtual display helper replied with id {id} while waiting for {expected_id}"
                        )));
                    }
                    let ok = value.get("ok").and_then(Value::as_bool).ok_or_else(|| {
                        internal("virtual display helper reply has no boolean ok field")
                    })?;
                    let result = value.get("result").cloned().unwrap_or(Value::Null);
                    if ok {
                        return Ok(result);
                    }
                    let message = result
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("virtual display helper returned an unspecified error");
                    return Err(internal(format!("helper error: {message}")));
                }
                Ok(HelperEvent::TooLong) => {
                    return Err(internal(
                        "virtual display helper reply exceeded the line size limit",
                    ));
                }
                Ok(HelperEvent::Closed(reason)) => {
                    return Err(DriverError::new(
                        ErrorCode::DeviceDisconnected,
                        format!(
                            "virtual display helper closed its output ({reason}): {}",
                            self.stderr_tail().trim()
                        ),
                    ));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(DriverError::new(
                        ErrorCode::DeviceDisconnected,
                        format!(
                            "virtual display helper output stream ended: {}",
                            self.stderr_tail().trim()
                        ),
                    ));
                }
            }
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        if self.exit_status().is_none() {
            self.stdin.take();
            if !wait_for_exit(&mut self.child, Duration::from_millis(200)) {
                let _ = self.child.kill();
                let _ = wait_for_exit(&mut self.child, Duration::from_secs(1));
            }
        }
    }
}

fn validate_ready(result: &Value, spec: &HelperSpec<'_>) -> Result<HelperInfo> {
    let object = result
        .as_object()
        .ok_or_else(|| internal("virtual display helper ready reply is not an object"))?;
    let display_id = ready_display_id(result)
        .ok_or_else(|| internal("virtual display helper returned an invalid display id"))?;
    if object.get("own_focus").and_then(Value::as_bool) != Some(true) {
        return Err(DriverError::new(
            ErrorCode::Unsupported,
            "virtual display was created without independent focus; refusing to continue",
        ));
    }
    if object.get("uid").and_then(Value::as_u64) != Some(u64::from(EXTERNAL_HELPER_UID)) {
        return Err(DriverError::new(
            ErrorCode::Unsupported,
            "virtual display helper is not running as adb shell uid 2000",
        ));
    }
    for (field, expected) in [
        ("width", spec.width),
        ("height", spec.height),
        ("dpi", spec.dpi),
    ] {
        if object.get(field).and_then(Value::as_u64) != Some(u64::from(expected)) {
            return Err(internal(format!(
                "virtual display helper reported {field} that does not match {expected}"
            )));
        }
    }
    Ok(HelperInfo {
        display_id,
        width: spec.width,
        height: spec.height,
        dpi: spec.dpi,
        uid: EXTERNAL_HELPER_UID,
    })
}

fn ready_display_id(result: &Value) -> Option<u32> {
    result
        .get("display_id")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0 && *value <= i64::from(i32::MAX))
        .map(|value| u32::try_from(value).expect("positive i32"))
}

fn spawn_stdout_reader(stdout: impl Read + Send + 'static, sender: SyncSender<HelperEvent>) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_line_bounded(&mut reader, MAX_HELPER_LINE) {
                Ok(None) => {
                    let _ = sender.send(HelperEvent::Closed("stdout reached EOF".to_owned()));
                    break;
                }
                Ok(Some(LineRead::Line(line))) => {
                    if sender.send(HelperEvent::Line(line)).is_err() {
                        break;
                    }
                }
                Ok(Some(LineRead::TooLong)) => {
                    let _ = sender.send(HelperEvent::TooLong);
                    break;
                }
                Err(error) => {
                    let _ = sender.send(HelperEvent::Closed(error.to_string()));
                    break;
                }
            }
        }
    });
}

fn spawn_stderr_reader(stderr: impl Read + Send + 'static) -> Arc<Mutex<Vec<u8>>> {
    let data = Arc::new(Mutex::new(Vec::new()));
    let thread_data = Arc::clone(&data);
    thread::spawn(move || {
        let mut reader = stderr;
        let mut buffer = [0u8; 2048];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let mut captured = thread_data.lock().expect("helper stderr lock");
                    let remaining = MAX_HELPER_STDERR.saturating_sub(captured.len());
                    let take = remaining.min(read);
                    captured.extend_from_slice(&buffer[..take]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
    data
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            _ => return false,
        }
    }
}

fn adb_executable_name(command: &Command) -> String {
    command.get_program().to_string_lossy().into_owned()
}
