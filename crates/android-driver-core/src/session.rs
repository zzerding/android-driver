use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::adb::AdbRunner;
use crate::cancel::CancelToken;
use crate::device::{Device, parse_devices};
use crate::display::{
    DisplayFocus, Keyguard, display_focus, keyguard_state, parse_wm_density, parse_wm_size,
};
use crate::endpoint::AdbEndpoint;
use crate::error::{DriverError, ErrorCode, Result, blocked, internal, invalid_argument};
use crate::helper::{ANDROID_SDK, EXTERNAL_HELPER_UID, Helper, HelperSpec};
use crate::lock::{DeviceLock, default_lock_dir};
use crate::png::{base64, validate_png};
use crate::validate::{
    canonical_component, package_of, validate_coordinates, validate_display_size,
    validate_duration, validate_keycode, validate_serial, validate_user,
};

const LAUNCH_VERIFY_TIMEOUT: Duration = Duration::from_secs(10);
const LAUNCH_POLL_INTERVAL: Duration = Duration::from_millis(250);
const FRAME_RETRY_INTERVAL: Duration = Duration::from_millis(200);

pub struct DriverConfig {
    pub adb_path: OsString,
    pub endpoint: AdbEndpoint,
    pub helper_path: Option<PathBuf>,
    pub lock_dir: Option<PathBuf>,
    pub adb_timeout: Duration,
    pub helper_timeout: Duration,
    pub cleanup_timeout: Duration,
    pub cancel: CancelToken,
}

pub struct OpenRequest {
    pub serial: String,
    pub user: u32,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub serial: String,
    pub user: u32,
    pub endpoint: String,
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
}

#[derive(Debug, Clone)]
pub struct ScreenshotResult {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
    pub timestamp: i64,
    pub png_base64: String,
}

#[derive(Debug, Clone)]
pub struct ActivityInfoResult {
    pub user: u32,
    pub launch_mode: u32,
    pub task_affinity: Option<String>,
    pub exported: bool,
}

#[derive(Debug, Clone)]
pub struct LaunchResult {
    pub display_id: u32,
    pub component: String,
    pub package: String,
    pub user: u32,
}

#[derive(Debug, Clone)]
pub struct InputResult {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
}

enum State {
    Idle,
    Active(Box<ActiveSession>),
    Failed(DriverError),
}

struct ActiveSession {
    serial: String,
    user: u32,
    lock: DeviceLock,
    display: DisplaySpec,
    helper: Helper,
    jar_path: String,
    target: Option<Target>,
}

struct DisplaySpec {
    id: u32,
    width: u32,
    height: u32,
    dpi: u32,
}

struct Target {
    component: String,
    package: String,
}

#[derive(Clone)]
struct SessionSnapshot {
    serial: String,
    user: u32,
    display: DisplaySpecSnapshot,
    target: Option<TargetSnapshot>,
}

#[derive(Clone)]
struct DisplaySpecSnapshot {
    id: u32,
    width: u32,
    height: u32,
}

#[derive(Clone)]
struct TargetSnapshot {
    package: String,
}

pub struct Driver {
    config: DriverConfig,
    adb: AdbRunner,
    cleanup_adb: AdbRunner,
    state: State,
}

impl Driver {
    pub fn new(config: DriverConfig) -> Self {
        let adb = AdbRunner::new(
            config.adb_path.clone(),
            config.endpoint.clone(),
            config.adb_timeout,
            config.cancel.clone(),
        );
        let cleanup_adb = AdbRunner::new(
            config.adb_path.clone(),
            config.endpoint.clone(),
            config.cleanup_timeout,
            CancelToken::new(),
        );
        Self {
            config,
            adb,
            cleanup_adb,
            state: State::Idle,
        }
    }

    pub fn list_devices(&self) -> Result<Vec<Device>> {
        list_devices(&self.adb)
    }

    pub fn open(&mut self, request: OpenRequest) -> Result<SessionInfo> {
        if !matches!(self.state, State::Idle) {
            return Err(self.invalid_state("open"));
        }
        validate_serial(&request.serial)?;
        validate_user(request.user)?;
        let helper_path = self.configured_helper_path()?;
        let lock_dir = self
            .config
            .lock_dir
            .clone()
            .map_or_else(default_lock_dir, Ok)?;
        let lock = DeviceLock::acquire(&lock_dir, &self.config.endpoint, &request.serial)?;

        match self.open_session(lock, &request, &helper_path) {
            Ok(session) => {
                let info = SessionInfo {
                    serial: session.serial.clone(),
                    user: session.user,
                    endpoint: self.config.endpoint.canonical(),
                    display_id: session.display.id,
                    width: session.display.width,
                    height: session.display.height,
                    dpi: session.display.dpi,
                };
                self.state = State::Active(Box::new(session));
                Ok(info)
            }
            Err((error, cleanup_failed)) => {
                if cleanup_failed {
                    self.state = State::Failed(error.clone());
                }
                Err(error)
            }
        }
    }

    pub fn status(&self) -> Value {
        match &self.state {
            State::Idle => json!({"state": "idle"}),
            State::Failed(error) => json!({
                "state": "failed",
                "error": error.to_json(),
            }),
            State::Active(session) => json!({
                "state": "active",
                "serial": session.serial,
                "user": session.user,
                "endpoint": self.config.endpoint.canonical(),
                "display_id": session.display.id,
                "width": session.display.width,
                "height": session.display.height,
                "dpi": session.display.dpi,
                "target_component": session.target.as_ref().map(|target| target.component.clone()),
                "lock_file": session.lock.path().display().to_string(),
            }),
        }
    }

    pub fn screenshot(&mut self) -> Result<ScreenshotResult> {
        let snapshot = self.snapshot("screenshot")?;
        let requested = json!({});
        let helper_timeout = self.config.helper_timeout;
        let reply = {
            let helper = self.active_helper()?;
            request_frame(helper, helper_timeout, requested)
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(error)
                if error.code == ErrorCode::Cancelled || error.code == ErrorCode::Timeout =>
            {
                return Err(error);
            }
            Err(error) if self.must_invalidate(&error) => {
                return Err(self.invalidate_active(error));
            }
            Err(error) => return Err(error),
        };

        let object = reply
            .as_object()
            .ok_or_else(|| internal("helper snapshot reply is not an object"))?;
        let encoded = object
            .get("png")
            .and_then(Value::as_str)
            .ok_or_else(|| internal("helper snapshot reply has no PNG data"))?;
        let width = object
            .get("width")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| internal("helper snapshot reply has no valid width"))?;
        let height = object
            .get("height")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| internal("helper snapshot reply has no valid height"))?;
        let timestamp = object
            .get("timestamp")
            .and_then(Value::as_i64)
            .ok_or_else(|| internal("helper snapshot reply has no timestamp"))?;
        if width != snapshot.display.width || height != snapshot.display.height {
            return Err(self.invalidate_active(internal(
                "helper snapshot dimensions do not match the virtual display",
            )));
        }
        let bytes = base64::decode(encoded)
            .map_err(|error| internal(format!("helper snapshot base64 is invalid: {error}")))?;
        if let Err(error) = validate_png(&bytes, width, height) {
            return Err(self
                .invalidate_active(internal(format!("helper produced an invalid PNG: {error}"))));
        }
        Ok(ScreenshotResult {
            display_id: snapshot.display.id,
            width,
            height,
            timestamp,
            png_base64: encoded.to_owned(),
        })
    }

    pub fn activity_info(&mut self, component: &str, user: u32) -> Result<ActivityInfoResult> {
        let snapshot = self.snapshot("activity_info")?;
        if user != snapshot.user {
            return Err(invalid_argument(
                "activity_info user must match the session's Android user",
            ));
        }
        let component = canonical_component(component)?;
        let result = self.helper_activity_info(&component, user)?;
        Ok(result)
    }

    pub fn launch(&mut self, component: &str) -> Result<LaunchResult> {
        let snapshot = self.snapshot("launch")?;
        let component = canonical_component(component)?;
        let package = package_of(&component).to_owned();
        self.guard_keyguard_and_physical(&snapshot, &package)?;
        let info = self.helper_activity_info(&component, snapshot.user)?;
        if !matches!(info.launch_mode, 0 | 1) {
            return Err(blocked(format!(
                "target launchMode {} may reuse an existing task; refusing to launch",
                info.launch_mode
            ))
            .detail("launch_mode", info.launch_mode)
            .detail("component", component));
        }

        let display = snapshot.display.id.to_string();
        let flags = "0x18000000";
        let user = snapshot.user.to_string();
        let args = [
            "shell",
            "am",
            "start-activity",
            "--user",
            &user,
            "--display",
            &display,
            "-f",
            flags,
            "-n",
            &component,
        ];
        match self.adb.run(Some(&snapshot.serial), &args, None) {
            Ok(output) => {
                if output_indicates_failure(&output.stdout, &output.stderr) {
                    return Err(self.invalidate_active(internal(format!(
                        "am start-activity reported a failure: {}",
                        first_failure_line(&output.stdout, &output.stderr)
                    ))));
                }
            }
            Err(error) if error.code == ErrorCode::Cancelled => return Err(error),
            Err(error) => return Err(self.invalidate_active(error)),
        }

        self.verify_launched_target(&snapshot, &package)?;
        if let State::Active(session) = &mut self.state {
            session.target = Some(Target {
                component: component.clone(),
                package: package.clone(),
            });
        }
        Ok(LaunchResult {
            display_id: snapshot.display.id,
            component,
            package,
            user: snapshot.user,
        })
    }

    pub fn tap(&mut self, width: u32, height: u32, x: u32, y: u32) -> Result<InputResult> {
        let snapshot = self.snapshot("tap")?;
        self.validate_input_dimensions(&snapshot, width, height)?;
        validate_coordinates(width, height, &[(x, y)])?;
        self.guard_input(&snapshot)?;
        let display = snapshot.display.id.to_string();
        let (x, y) = (x.to_string(), y.to_string());
        let args = ["shell", "input", "-d", &display, "tap", &x, &y];
        self.run_input_command(&snapshot.serial, &args)?;
        Ok(input_result(&snapshot))
    }

    pub fn swipe(
        &mut self,
        width: u32,
        height: u32,
        x1: u32,
        y1: u32,
        x2: u32,
        y2: u32,
        duration_ms: u32,
    ) -> Result<InputResult> {
        let snapshot = self.snapshot("swipe")?;
        self.validate_input_dimensions(&snapshot, width, height)?;
        validate_coordinates(width, height, &[(x1, y1), (x2, y2)])?;
        validate_duration(duration_ms)?;
        self.guard_input(&snapshot)?;
        let display = snapshot.display.id.to_string();
        let x1 = x1.to_string();
        let y1 = y1.to_string();
        let x2 = x2.to_string();
        let y2 = y2.to_string();
        let duration = duration_ms.to_string();
        let args = [
            "shell", "input", "-d", &display, "swipe", &x1, &y1, &x2, &y2, &duration,
        ];
        self.run_input_command(&snapshot.serial, &args)?;
        Ok(input_result(&snapshot))
    }

    pub fn key(&mut self, keycode: u32) -> Result<InputResult> {
        let snapshot = self.snapshot("key")?;
        validate_keycode(keycode)?;
        self.guard_input(&snapshot)?;
        let display = snapshot.display.id.to_string();
        let keycode = keycode.to_string();
        let args = ["shell", "input", "-d", &display, "keyevent", &keycode];
        self.run_input_command(&snapshot.serial, &args)?;
        Ok(input_result(&snapshot))
    }

    pub fn close(&mut self) -> Result<()> {
        let previous = std::mem::replace(&mut self.state, State::Idle);
        match previous {
            State::Active(session) => {
                let cleanup = cleanup_session(&self.config, &self.cleanup_adb, *session);
                if cleanup.is_empty() {
                    Ok(())
                } else {
                    let error = DriverError::new(
                        ErrorCode::CleanupFailed,
                        "session cleanup did not complete cleanly",
                    )
                    .detail("cleanup_errors", cleanup);
                    self.state = State::Failed(error.clone());
                    Err(error)
                }
            }
            State::Idle => Err(self.invalid_state("close")),
            State::Failed(error) => {
                self.state = State::Failed(error.clone());
                Err(DriverError::new(
                    ErrorCode::InvalidState,
                    format!(
                        "session is unusable after a failed cleanup: {}",
                        error.message
                    ),
                ))
            }
        }
    }

    /// Best-effort shutdown used for stdin EOF and stdout failure. The kernel
    /// device lock stays held until every cleanup attempt has run.
    pub fn shutdown(&mut self) -> Vec<String> {
        let previous = std::mem::replace(&mut self.state, State::Idle);
        match previous {
            State::Active(session) => cleanup_session(&self.config, &self.cleanup_adb, *session),
            State::Idle | State::Failed(_) => Vec::new(),
        }
    }

    fn configured_helper_path(&self) -> Result<PathBuf> {
        let Some(path) = &self.config.helper_path else {
            return Err(DriverError::new(
                ErrorCode::Unsupported,
                "no virtual display helper configured; pass --helper PATH to the driver",
            ));
        };
        let metadata = std::fs::metadata(path).map_err(|error| {
            DriverError::new(
                ErrorCode::Unsupported,
                format!(
                    "virtual display helper is missing at {}: {error}; build it with helper/build.sh",
                    path.display()
                ),
            )
        })?;
        if !metadata.is_file() {
            return Err(DriverError::new(
                ErrorCode::Unsupported,
                format!("virtual display helper is not a file: {}", path.display()),
            ));
        }
        Ok(path.clone())
    }

    fn open_session(
        &self,
        lock: DeviceLock,
        request: &OpenRequest,
        helper_path: &Path,
    ) -> std::result::Result<ActiveSession, (DriverError, bool)> {
        let mut helper: Option<Helper> = None;
        let mut jar_path: Option<String> = None;
        let result = (|| -> Result<DisplaySpec> {
            let devices = list_devices(&self.adb)?;
            let device = devices
                .iter()
                .find(|device| device.serial == request.serial)
                .ok_or_else(|| {
                    blocked(format!(
                        "serial {} is not present on the selected adb server",
                        request.serial
                    ))
                })?;
            if device.state != "device" {
                return Err(blocked(format!(
                    "serial {} is in state {} instead of device",
                    request.serial, device.state
                )));
            }

            let sdk = self
                .adb
                .run(
                    Some(&request.serial),
                    &["shell", "getprop", "ro.build.version.sdk"],
                    None,
                )?
                .stdout;
            let sdk = sdk.trim();
            if sdk != "34" {
                return Err(DriverError::new(
                    ErrorCode::Unsupported,
                    format!(
                        "virtual display requires Android SDK {ANDROID_SDK}; the device reports {sdk}"
                    ),
                ));
            }

            let policy = self
                .adb
                .run(
                    Some(&request.serial),
                    &["shell", "dumpsys", "window", "policy"],
                    None,
                )?
                .stdout;
            match keyguard_state(&policy) {
                Keyguard::Unlocked => {}
                Keyguard::Locked => {
                    return Err(blocked(
                        "device is locked; unlock it before opening a session",
                    ));
                }
                Keyguard::Unknown => {
                    return Err(blocked(
                        "cannot determine keyguard state; refusing to open a session",
                    ));
                }
            }

            let size_output = self
                .adb
                .run(Some(&request.serial), &["shell", "wm", "size"], None)?
                .stdout;
            let density_output = self
                .adb
                .run(Some(&request.serial), &["shell", "wm", "density"], None)?
                .stdout;
            let (width, height) = parse_wm_size(&size_output)
                .map_err(|error| blocked(format!("cannot determine the display size: {error}")))?;
            let dpi = parse_wm_density(&density_output).map_err(|error| {
                blocked(format!("cannot determine the display density: {error}"))
            })?;
            validate_display_size(width, height)?;

            let helper_host_path = helper_path.to_str().ok_or_else(|| {
                DriverError::new(
                    ErrorCode::Unsupported,
                    "helper path must be valid UTF-8 to pass through adb",
                )
            })?;
            let token = unique_token();
            let jar = format!("/data/local/tmp/android-driver-{token}.jar");
            self.adb.run(
                Some(&request.serial),
                &["push", helper_host_path, &jar],
                None,
            )?;
            jar_path = Some(jar.clone());

            let nice_name = format!("android-driver-vd-{token}");
            let spec = HelperSpec {
                serial: &request.serial,
                jar_device_path: &jar,
                width,
                height,
                dpi,
                nice_name: &nice_name,
            };
            let spawned = Helper::spawn(
                &self.adb,
                &spec,
                self.config.helper_timeout,
                self.config.cancel.clone(),
            )?;
            let info = spawned.info();
            if info.uid != EXTERNAL_HELPER_UID {
                return Err(DriverError::new(
                    ErrorCode::Unsupported,
                    "virtual display helper is not running as adb shell uid 2000",
                ));
            }
            helper = Some(spawned);
            Ok(DisplaySpec {
                id: info.display_id,
                width: info.width,
                height: info.height,
                dpi: info.dpi,
            })
        })();

        match result {
            Ok(display) => {
                let spawned = helper
                    .take()
                    .expect("helper present after a successful open");
                Ok(ActiveSession {
                    serial: request.serial.clone(),
                    user: request.user,
                    lock,
                    display,
                    helper: spawned,
                    jar_path: jar_path.expect("jar path set before the helper starts"),
                    target: None,
                })
            }
            Err(cause) => {
                let mut cleanup = Vec::new();
                if let Some(mut spawned) = helper {
                    let display_id = spawned.display_id();
                    cleanup.extend(spawned.shutdown(self.config.cleanup_timeout));
                    if let Err(error) = verify_display_removed(
                        &self.cleanup_adb,
                        &request.serial,
                        display_id,
                        self.config.cleanup_timeout,
                    ) {
                        cleanup.push(error);
                    }
                }
                if let Some(jar) = &jar_path
                    && let Err(error) = self.cleanup_adb.run(
                        Some(&request.serial),
                        &["shell", "rm", "-f", jar],
                        Some(self.config.cleanup_timeout),
                    )
                {
                    cleanup.push(format!(
                        "cannot remove the device helper jar {jar}: {error}"
                    ));
                }
                let cleanup_failed = !cleanup.is_empty();
                let cause = if cleanup_failed {
                    cause.detail("cleanup_errors", cleanup)
                } else {
                    cause
                };
                Err((cause, cleanup_failed))
            }
        }
    }

    fn snapshot(&mut self, operation: &str) -> Result<SessionSnapshot> {
        let exited = match &mut self.state {
            State::Active(session) => session.helper.exit_status(),
            State::Idle | State::Failed(_) => None,
        };
        if let Some(status) = exited {
            let error = DriverError::new(
                ErrorCode::DeviceDisconnected,
                format!("virtual display helper exited ({status}) during {operation}"),
            );
            return Err(self.invalidate_active(error));
        }
        match &self.state {
            State::Active(session) => Ok(SessionSnapshot {
                serial: session.serial.clone(),
                user: session.user,
                display: DisplaySpecSnapshot {
                    id: session.display.id,
                    width: session.display.width,
                    height: session.display.height,
                },
                target: session.target.as_ref().map(|target| TargetSnapshot {
                    package: target.package.clone(),
                }),
            }),
            State::Idle => Err(DriverError::new(
                ErrorCode::InvalidState,
                format!("{operation} requires an active session; call open first"),
            )),
            State::Failed(error) => Err(DriverError::new(
                ErrorCode::InvalidState,
                format!(
                    "{operation} is unavailable because the session failed: {}",
                    error.message
                ),
            )),
        }
    }

    fn active_helper(&mut self) -> Result<&mut Helper> {
        match &mut self.state {
            State::Active(session) => Ok(&mut session.helper),
            State::Idle => Err(DriverError::new(
                ErrorCode::InvalidState,
                "no active session",
            )),
            State::Failed(error) => Err(DriverError::new(
                ErrorCode::InvalidState,
                format!("session failed: {}", error.message),
            )),
        }
    }

    fn helper_activity_info(&mut self, component: &str, user: u32) -> Result<ActivityInfoResult> {
        let requested = json!({"component": component, "user": user});
        let helper_timeout = self.config.helper_timeout;
        let reply = {
            let helper = self.active_helper()?;
            helper.request("activity_info", requested, helper_timeout)
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) if error.code == ErrorCode::Cancelled => return Err(error),
            Err(error) if self.must_invalidate(&error) => {
                return Err(self.invalidate_active(error));
            }
            Err(error) => return Err(error),
        };
        let object = reply
            .as_object()
            .ok_or_else(|| internal("helper activity_info reply is not an object"))?;
        if object.get("user").and_then(Value::as_u64) != Some(u64::from(user)) {
            return Err(self
                .invalidate_active(internal("helper activity_info reply has a mismatched user")));
        }
        let launch_mode = object
            .get("launch_mode")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| internal("helper activity_info has no valid launch_mode"))?;
        let exported = object
            .get("exported")
            .and_then(Value::as_bool)
            .ok_or_else(|| internal("helper activity_info has no exported flag"))?;
        let task_affinity = match object.get("task_affinity") {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) => Some(value.clone()),
            Some(_) => {
                return Err(self.invalidate_active(internal(
                    "helper activity_info has an invalid task_affinity",
                )));
            }
        };
        Ok(ActivityInfoResult {
            user,
            launch_mode,
            task_affinity,
            exported,
        })
    }

    fn guard_keyguard_and_physical(
        &mut self,
        snapshot: &SessionSnapshot,
        package: &str,
    ) -> Result<()> {
        let policy =
            self.read_checked(&snapshot.serial, &["shell", "dumpsys", "window", "policy"])?;
        check_keyguard(&policy)?;
        let activities = self.read_checked(
            &snapshot.serial,
            &["shell", "dumpsys", "activity", "activities"],
        )?;
        require_physical_clear(&activities, package, snapshot.user)
    }

    fn guard_input(&mut self, snapshot: &SessionSnapshot) -> Result<()> {
        let target = snapshot.target.as_ref().ok_or_else(|| {
            DriverError::new(
                ErrorCode::InvalidState,
                "input requires a successfully launched target; call launch first",
            )
        })?;
        self.guard_keyguard_and_physical(snapshot, &target.package)?;
        let activities = self.read_checked(
            &snapshot.serial,
            &["shell", "dumpsys", "activity", "activities"],
        )?;
        require_virtual_target(
            &activities,
            snapshot.display.id,
            &target.package,
            snapshot.user,
        )
    }

    fn validate_input_dimensions(
        &self,
        snapshot: &SessionSnapshot,
        width: u32,
        height: u32,
    ) -> Result<()> {
        if width != snapshot.display.width || height != snapshot.display.height {
            return Err(invalid_argument(format!(
                "input dimensions {width}x{height} must match the original screenshot {}x{}",
                snapshot.display.width, snapshot.display.height
            )));
        }
        Ok(())
    }

    fn run_input_command(&mut self, serial: &str, args: &[&str]) -> Result<()> {
        match self.adb.run(Some(serial), args, None) {
            Ok(output) => {
                if output_indicates_failure(&output.stdout, &output.stderr) {
                    return Err(self.invalidate_active(internal(format!(
                        "input command reported a failure: {}",
                        first_failure_line(&output.stdout, &output.stderr)
                    ))));
                }
                Ok(())
            }
            Err(error) if error.code == ErrorCode::Cancelled => Err(error),
            Err(error) => Err(self.invalidate_active(error)),
        }
    }

    fn verify_launched_target(&mut self, snapshot: &SessionSnapshot, package: &str) -> Result<()> {
        let deadline = Instant::now() + LAUNCH_VERIFY_TIMEOUT;
        let mut last = "no activity dump was readable".to_owned();
        while Instant::now() < deadline {
            self.config.cancel.check()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout = self
                .adb
                .timeout()
                .min(remaining.max(Duration::from_millis(200)));
            match self.adb.run(
                Some(&snapshot.serial),
                &["shell", "dumpsys", "activity", "activities"],
                Some(timeout),
            ) {
                Ok(output) => match display_focus(&output.stdout, snapshot.display.id) {
                    DisplayFocus::Present(Some(activity))
                        if activity.package() == package && activity.user == snapshot.user =>
                    {
                        return Ok(());
                    }
                    DisplayFocus::Present(Some(activity)) => {
                        last = format!(
                            "top resumed activity is {} for user {}",
                            activity.component, activity.user
                        );
                    }
                    DisplayFocus::Present(None) => {
                        last = format!("display #{} has no resumed activity", snapshot.display.id);
                    }
                    DisplayFocus::Missing => {
                        last = format!("display #{} is missing from the dump", snapshot.display.id);
                    }
                    DisplayFocus::Uncertain(error) => last = error,
                },
                Err(error) if error.code == ErrorCode::Cancelled => return Err(error),
                Err(error) if error.code == ErrorCode::DeviceDisconnected => {
                    return Err(self.invalidate_active(error));
                }
                Err(error) => last = error.to_string(),
            }
            thread::sleep(LAUNCH_POLL_INTERVAL);
        }
        Err(self.invalidate_active(
            DriverError::new(
                ErrorCode::Timeout,
                format!(
                    "launch on display #{} was not verified within {} ms; the result is unknown and the session was invalidated",
                    snapshot.display.id,
                    LAUNCH_VERIFY_TIMEOUT.as_millis()
                ),
            )
            .detail("last_observation", last),
        ))
    }

    fn read_checked(&mut self, serial: &str, args: &[&str]) -> Result<String> {
        match self.adb.run(Some(serial), args, None) {
            Ok(output) => Ok(output.stdout),
            Err(error) if error.code == ErrorCode::Cancelled => Err(error),
            Err(error) if error.code == ErrorCode::DeviceDisconnected => {
                Err(self.invalidate_active(error))
            }
            Err(error) => Err(error),
        }
    }

    fn invalid_state(&self, operation: &str) -> DriverError {
        match &self.state {
            State::Idle => DriverError::new(
                ErrorCode::InvalidState,
                format!("{operation} requires an active session; call open first"),
            ),
            State::Failed(error) => DriverError::new(
                ErrorCode::InvalidState,
                format!(
                    "{operation} is unavailable because the session failed: {}",
                    error.message
                ),
            ),
            State::Active(_) => DriverError::new(
                ErrorCode::InvalidState,
                format!("{operation} is not allowed while a session is active"),
            ),
        }
    }

    fn invalidate_active(&mut self, cause: DriverError) -> DriverError {
        let previous = std::mem::replace(&mut self.state, State::Idle);
        if let State::Active(session) = previous {
            let cleanup = cleanup_session(&self.config, &self.cleanup_adb, *session);
            let error = if cleanup.is_empty() {
                cause
            } else {
                cause.detail("cleanup_errors", cleanup)
            };
            self.state = State::Failed(error.clone());
            error
        } else {
            cause
        }
    }

    fn must_invalidate(&self, error: &DriverError) -> bool {
        matches!(
            error.code,
            ErrorCode::DeviceDisconnected | ErrorCode::Internal | ErrorCode::CleanupFailed
        )
    }
}

fn list_devices(adb: &AdbRunner) -> Result<Vec<Device>> {
    let output = adb.run(None, &["devices", "-l"], None)?;
    parse_devices(&output.stdout).map_err(|error| {
        internal(format!("cannot parse adb devices output: {error}")).detail(
            "stdout",
            output.stdout.chars().take(2000).collect::<String>(),
        )
    })
}

fn cleanup_session(
    config: &DriverConfig,
    cleanup_adb: &AdbRunner,
    session: ActiveSession,
) -> Vec<String> {
    let ActiveSession {
        serial,
        lock,
        helper,
        jar_path,
        display,
        ..
    } = session;
    let mut errors = Vec::new();
    let mut helper = helper;
    errors.extend(helper.shutdown(config.cleanup_timeout));
    if let Err(error) = cleanup_adb.run(
        Some(&serial),
        &["shell", "rm", "-f", &jar_path],
        Some(config.cleanup_timeout),
    ) {
        errors.push(format!(
            "cannot remove the device helper jar {jar_path}: {error}"
        ));
    }
    if let Err(error) =
        verify_display_removed(cleanup_adb, &serial, display.id, config.cleanup_timeout)
    {
        errors.push(error);
    }
    drop(lock);
    errors
}

fn verify_display_removed(
    cleanup_adb: &AdbRunner,
    serial: &str,
    display_id: u32,
    timeout: Duration,
) -> std::result::Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let command_timeout = remaining
            .max(Duration::from_millis(200))
            .min(Duration::from_secs(2));
        let last = match cleanup_adb.run(
            Some(serial),
            &["shell", "dumpsys", "activity", "activities"],
            Some(command_timeout),
        ) {
            Ok(output) => match display_focus(&output.stdout, display_id) {
                DisplayFocus::Missing if has_physical_display(&output.stdout) => return Ok(()),
                DisplayFocus::Missing => {
                    "activity dump did not confirm any display section".to_owned()
                }
                DisplayFocus::Present(None) => {
                    format!("display #{display_id} is still present")
                }
                DisplayFocus::Present(Some(activity)) => {
                    format!("display #{display_id} still hosts {}", activity.component)
                }
                DisplayFocus::Uncertain(error) => error,
            },
            Err(error) => error.to_string(),
        };
        if Instant::now() >= deadline {
            return Err(format!(
                "cannot verify removal of virtual display #{display_id} within {} ms: {last}",
                timeout.as_millis()
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn has_physical_display(dump: &str) -> bool {
    dump.lines().any(|line| {
        let line = line.trim();
        line.starts_with("Display #0 ") && line.ends_with("):")
    })
}
fn request_frame(helper: &mut Helper, timeout: Duration, requested: Value) -> Result<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(DriverError::new(
                ErrorCode::Timeout,
                "virtual display produced no frame before the timeout",
            ));
        }
        match helper.request("snapshot", requested.clone(), remaining) {
            Ok(reply) => return Ok(reply),
            Err(error) if error.code == ErrorCode::Cancelled => return Err(error),
            Err(error)
                if error.message.contains("No virtual display frame yet")
                    && Instant::now() < deadline =>
            {
                thread::sleep(FRAME_RETRY_INTERVAL);
            }
            Err(error) => return Err(error),
        }
    }
}

fn check_keyguard(policy: &str) -> Result<()> {
    match keyguard_state(policy) {
        Keyguard::Unlocked => Ok(()),
        Keyguard::Locked => Err(blocked("device is locked; refusing to act")),
        Keyguard::Unknown => Err(blocked("cannot determine keyguard state; refusing to act")),
    }
}

fn require_physical_clear(dump: &str, package: &str, user: u32) -> Result<()> {
    match display_focus(dump, 0) {
        DisplayFocus::Present(Some(activity))
            if activity.package() == package && activity.user == user =>
        {
            Err(blocked(format!(
                "{package} is already resumed on the physical display for user {user}; refusing to act"
            ))
            .detail("physical_component", activity.component)
            .detail("physical_user", activity.user))
        }
        DisplayFocus::Present(Some(_)) => Ok(()),
        DisplayFocus::Present(None) => Err(blocked(
            "cannot determine the physical display's resumed activity; refusing to act",
        )),
        DisplayFocus::Missing => Err(blocked(
            "the physical display section is missing from dumpsys; refusing to act",
        )),
        DisplayFocus::Uncertain(error) => Err(blocked(format!(
            "cannot determine the physical display state ({error}); refusing to act"
        ))),
    }
}

fn require_virtual_target(dump: &str, display_id: u32, package: &str, user: u32) -> Result<()> {
    match display_focus(dump, display_id) {
        DisplayFocus::Present(Some(activity))
            if activity.package() == package && activity.user == user =>
        {
            Ok(())
        }
        DisplayFocus::Present(Some(activity)) => Err(blocked(format!(
            "the virtual display is showing {} for user {}, not {package} for user {user}",
            activity.component, activity.user
        ))),
        DisplayFocus::Present(None) => Err(blocked(format!(
            "display #{display_id} has no resumed target activity"
        ))),
        DisplayFocus::Missing => Err(blocked(format!(
            "display #{display_id} is missing from dumpsys; refusing to send input"
        ))),
        DisplayFocus::Uncertain(error) => Err(blocked(format!(
            "cannot determine display #{display_id} state ({error}); refusing to send input"
        ))),
    }
}

fn output_indicates_failure(stdout: &str, stderr: &str) -> bool {
    let combined = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    [
        "error:",
        "exception",
        "activity not started",
        "unable to start",
    ]
    .iter()
    .any(|marker| combined.contains(marker))
}

fn first_failure_line(stdout: &str, stderr: &str) -> String {
    stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .find(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("error") || lower.contains("exception") || lower.contains("not started")
        })
        .unwrap_or("no diagnostic line")
        .chars()
        .take(500)
        .collect()
}

fn input_result(snapshot: &SessionSnapshot) -> InputResult {
    InputResult {
        display_id: snapshot.display.id,
        width: snapshot.display.width,
        height: snapshot.display.height,
    }
}

fn unique_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:x}{:x}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_guard_only_blocks_the_same_package_and_user() {
        let dump = "\
Display #0 (activities from top to bottom):
  mResumedActivity: ActivityRecord{a u0 com.example.app/.Main t1}
Display #7 (activities from top to bottom):
  mResumedActivity: ActivityRecord{b u10 com.example.app/.Main t2}
";
        assert!(require_physical_clear(dump, "com.example.app", 0).is_err());
        require_physical_clear(dump, "com.example.app", 10).unwrap();
        require_physical_clear(dump, "com.other.app", 0).unwrap();
        assert!(require_virtual_target(dump, 7, "com.example.app", 10).is_ok());
        assert!(require_virtual_target(dump, 7, "com.example.app", 0).is_err());
    }

    #[test]
    fn missing_physical_state_is_never_treated_as_safe() {
        assert!(require_physical_clear("", "com.example.app", 0).is_err());
        assert!(
            require_physical_clear(
                "Display #0 (activities from top to bottom):\n  * Task{x}\n",
                "com.example.app",
                0
            )
            .is_err()
        );
    }
}
