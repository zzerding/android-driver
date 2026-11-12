use std::ffi::OsString;
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::cancel::CancelToken;
use crate::endpoint::AdbEndpoint;
use crate::error::{DriverError, ErrorCode, Result, internal};

const ADB_STDOUT_LIMIT: usize = 8 * 1024 * 1024;
const ADB_STDERR_LIMIT: usize = 256 * 1024;
const CAPTURE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const POST_KILL_REAP_TIMEOUT: Duration = Duration::from_secs(2);

pub struct AdbOutput {
    pub stdout: String,
    pub stderr: String,
    pub status: ExitStatus,
}

pub struct AdbRunner {
    path: OsString,
    endpoint: AdbEndpoint,
    default_timeout: Duration,
    cancel: CancelToken,
}

impl AdbRunner {
    pub fn new(
        path: OsString,
        endpoint: AdbEndpoint,
        default_timeout: Duration,
        cancel: CancelToken,
    ) -> Self {
        Self {
            path,
            endpoint,
            default_timeout,
            cancel,
        }
    }

    /// Pins the endpoint and optional serial, removing environment overrides so the device lock matches the server used.
    pub fn command(&self, serial: Option<&str>) -> Command {
        let mut command = Command::new(&self.path);
        command
            .arg("-H")
            .arg(self.endpoint.host())
            .arg("-P")
            .arg(self.endpoint.port_text())
            .env_remove("ADB_SERVER_SOCKET")
            .env_remove("ADB_SERVER_PORT")
            .env_remove("ANDROID_ADB_SERVER_PORT")
            .env_remove("ANDROID_SERIAL");
        if let Some(serial) = serial {
            command.arg("-s").arg(serial);
        }
        command
    }

    pub fn timeout(&self) -> Duration {
        self.default_timeout
    }

    pub fn run(
        &self,
        serial: Option<&str>,
        args: &[&str],
        timeout: Option<Duration>,
    ) -> Result<AdbOutput> {
        self.cancel.check()?;
        let timeout = timeout.unwrap_or(self.default_timeout);
        let mut command = self.command(serial);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => DriverError::new(
                ErrorCode::Unsupported,
                format!("adb executable not found: {}", self.path.to_string_lossy()),
            ),
            _ => DriverError::new(
                ErrorCode::Unsupported,
                format!("cannot start adb executable: {error}"),
            ),
        })?;

        let stdout = child
            .stdout
            .take()
            .map(|reader| StreamCapture::spawn(reader, ADB_STDOUT_LIMIT));
        let stderr = child
            .stderr
            .take()
            .map(|reader| StreamCapture::spawn(reader, ADB_STDERR_LIMIT));

        let deadline = Instant::now() + timeout;
        let mut timed_out = false;
        let mut cancelled = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    reap(&mut child);
                    return Err(internal(format!("failed to wait for adb: {error}")));
                }
            }
            if self.cancel.is_cancelled() {
                cancelled = true;
                let _ = child.kill();
                break reap(&mut child);
            }
            if Instant::now() >= deadline {
                timed_out = true;
                let _ = child.kill();
                break reap(&mut child);
            }
            thread::sleep(POLL_INTERVAL);
        };

        let stdout = stdout.map(StreamCapture::finish);
        let stderr = stderr.map(StreamCapture::finish);
        if cancelled {
            return Err(DriverError::new(
                ErrorCode::Cancelled,
                "adb command cancelled because the protocol input closed",
            ));
        }
        let stderr_text = stderr
            .as_ref()
            .map(|capture| capture.text())
            .unwrap_or_default();
        if timed_out {
            return Err(DriverError::new(
                ErrorCode::Timeout,
                format!("adb command timed out after {} ms", timeout.as_millis()),
            )
            .detail("stderr", truncate(&stderr_text, 4000))
            .detail("args", serde_json::json!(args)));
        }
        if stdout.as_ref().is_some_and(|capture| capture.truncated())
            || stderr.as_ref().is_some_and(|capture| capture.truncated())
        {
            return Err(internal("adb output exceeded the capture limit"));
        }
        let status =
            status.ok_or_else(|| internal("adb child disappeared without an exit status"))?;
        let stdout_text = stdout.map(|capture| capture.text()).unwrap_or_default();
        if status.success() {
            return Ok(AdbOutput {
                stdout: stdout_text,
                stderr: stderr_text,
                status,
            });
        }

        let mut error = classify_adb_failure(&stderr_text);
        error
            .details
            .get_or_insert_with(serde_json::Map::new)
            .insert("exit_code".into(), status.code().unwrap_or(-1).into());
        error
            .details
            .get_or_insert_with(serde_json::Map::new)
            .insert("stderr".into(), truncate(&stderr_text, 4000).into());
        if !stdout_text.trim().is_empty() {
            error
                .details
                .get_or_insert_with(serde_json::Map::new)
                .insert("stdout".into(), truncate(stdout_text.trim(), 2000).into());
        }
        Err(error)
    }
}

fn classify_adb_failure(stderr: &str) -> DriverError {
    let lower = stderr.to_ascii_lowercase();
    let disconnected = [
        "device offline",
        "device not found",
        "no devices/emulators found",
        "device unauthorized",
        "device still authorizing",
        "error: closed",
        "connection reset",
        "protocol fault",
    ];
    let message = if disconnected.iter().any(|marker| lower.contains(marker)) {
        return DriverError::new(
            ErrorCode::DeviceDisconnected,
            format!(
                "adb reported the device is unavailable: {}",
                truncate(stderr.trim(), 1000)
            ),
        );
    } else {
        format!("adb command failed: {}", truncate(stderr.trim(), 1000))
    };
    internal(message)
}

fn reap(child: &mut Child) -> Option<ExitStatus> {
    let deadline = Instant::now() + POST_KILL_REAP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            _ => return None,
        }
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

struct StreamCapture {
    data: Arc<Mutex<Vec<u8>>>,
    truncated_flag: Arc<AtomicBool>,
    done: Receiver<()>,
}

impl StreamCapture {
    fn spawn(mut reader: impl Read + Send + 'static, limit: usize) -> Self {
        let data = Arc::new(Mutex::new(Vec::with_capacity(limit.min(64 * 1024))));
        let truncated_flag = Arc::new(AtomicBool::new(false));
        let (sender, done) = mpsc::channel();
        let thread_data = Arc::clone(&data);
        let thread_truncated = Arc::clone(&truncated_flag);
        thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        let mut captured = thread_data.lock().expect("capture lock");
                        let remaining = limit.saturating_sub(captured.len());
                        let take = remaining.min(read);
                        captured.extend_from_slice(&buffer[..take]);
                        if take < read {
                            thread_truncated.store(true, Ordering::Relaxed);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = sender.send(());
        });
        Self {
            data,
            truncated_flag,
            done,
        }
    }

    fn finish(self) -> Self {
        let _ = self.done.recv_timeout(CAPTURE_DRAIN_TIMEOUT);
        self
    }

    fn truncated(&self) -> bool {
        self.truncated_flag.load(Ordering::Relaxed)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.data.lock().expect("capture lock")).into_owned()
    }
}
