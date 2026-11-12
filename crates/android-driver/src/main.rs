use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use android_driver_core::cancel::CancelToken;
use android_driver_core::endpoint::AdbEndpoint;
use android_driver_core::error::{DriverError, ErrorCode, invalid_argument};
use android_driver_core::lines::{LineRead, read_line_bounded};
use android_driver_core::protocol::{
    self, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, Op, ParsedRequest, error_frame, ok_frame,
    parse_request, startup_frame,
};
use android_driver_core::session::{
    ActivityInfoResult, Driver, DriverConfig, LaunchResult, OpenRequest, ScreenshotResult,
    SessionInfo,
};
use serde_json::{Value, json};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_QUEUED_REQUESTS: usize = 32;

fn main() {
    let code = match real_main() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("android-driver: {message}");
            2
        }
    };
    std::process::exit(code);
}

enum CommandLine {
    Run(Args),
    Help,
    Version,
}

struct Args {
    adb: OsString,
    endpoint: AdbEndpoint,
    helper: Option<PathBuf>,
    lock_dir: Option<PathBuf>,
    adb_timeout: Duration,
    helper_timeout: Duration,
    cleanup_timeout: Duration,
}

fn real_main() -> Result<i32, String> {
    match parse_command_line(std::env::args_os().skip(1).collect())? {
        CommandLine::Help => {
            print!("{}", usage());
            Ok(0)
        }
        CommandLine::Version => {
            println!(
                "android-driver {VERSION} (protocol {})",
                protocol::PROTOCOL_VERSION
            );
            Ok(0)
        }
        CommandLine::Run(args) => run(args),
    }
}

fn run(args: Args) -> Result<i32, String> {
    let cancel = CancelToken::new();
    let config = DriverConfig {
        adb_path: args.adb,
        endpoint: args.endpoint,
        helper_path: args.helper,
        lock_dir: args.lock_dir,
        adb_timeout: args.adb_timeout,
        helper_timeout: args.helper_timeout,
        cleanup_timeout: args.cleanup_timeout,
        cancel: cancel.clone(),
    };
    let mut driver = Driver::new(config);
    let hub = InputHub::start(cancel.clone());
    let mut output = BufWriter::new(io::stdout().lock());

    if let Err(error) = emit(&mut output, None, &startup_frame(VERSION)) {
        eprintln!("android-driver: cannot write the startup frame: {error}");
        log_cleanup(driver.shutdown());
        return Ok(1);
    }

    let mut last_id = 0u64;
    loop {
        match hub.next() {
            Next::Eof => {
                let errors = driver.shutdown();
                let code = if errors.is_empty() { 0 } else { 1 };
                log_cleanup(errors);
                return Ok(code);
            }
            Next::Overflow => {
                let error = DriverError::new(
                    ErrorCode::Protocol,
                    "more than 32 requests were queued while a command was running; commands are sequential and excess input was dropped",
                );
                if let Err(write_error) = emit(&mut output, None, &error_frame(None, &error)) {
                    eprintln!("android-driver: cannot write a protocol error: {write_error}");
                    log_cleanup(driver.shutdown());
                    return Ok(1);
                }
            }
            Next::TooLong => {
                let error = invalid_argument(format!(
                    "request line exceeds the {MAX_REQUEST_BYTES} byte limit"
                ));
                if let Err(write_error) = emit(&mut output, None, &error_frame(None, &error)) {
                    eprintln!("android-driver: cannot write a protocol error: {write_error}");
                    log_cleanup(driver.shutdown());
                    return Ok(1);
                }
            }
            Next::Line(line) => {
                let request = match parse_request(&line) {
                    Ok(request) => request,
                    Err(parse_error) => {
                        let error_id = parse_error.id;
                        let error = parse_error.into_driver_error();
                        if let Err(write_error) =
                            emit(&mut output, error_id, &error_frame(error_id, &error))
                        {
                            eprintln!("android-driver: cannot write a parse error: {write_error}");
                            log_cleanup(driver.shutdown());
                            return Ok(1);
                        }
                        continue;
                    }
                };
                if request.id <= last_id {
                    let error = invalid_argument(format!(
                        "request id {} must be greater than the previous id {last_id}",
                        request.id
                    ));
                    if let Err(write_error) = emit(
                        &mut output,
                        Some(request.id),
                        &error_frame(Some(request.id), &error),
                    ) {
                        eprintln!("android-driver: cannot write a protocol error: {write_error}");
                        log_cleanup(driver.shutdown());
                        return Ok(1);
                    }
                    continue;
                }
                last_id = request.id;

                let frame = match dispatch(&mut driver, &request) {
                    Ok(result) => ok_frame(request.id, result),
                    Err(error) => error_frame(Some(request.id), &error),
                };
                if let Err(write_error) = emit(&mut output, Some(request.id), &frame) {
                    eprintln!("android-driver: cannot write a response: {write_error}");
                    log_cleanup(driver.shutdown());
                    return Ok(1);
                }

                if cancel.is_cancelled() {
                    let errors = driver.shutdown();
                    let code = if errors.is_empty() { 0 } else { 1 };
                    log_cleanup(errors);
                    return Ok(code);
                }
            }
        }
    }
}

fn dispatch(driver: &mut Driver, request: &ParsedRequest) -> android_driver_core::Result<Value> {
    match &request.op {
        Op::ListDevices => {
            let devices = driver.list_devices()?;
            Ok(json!({
                "devices": devices.iter().map(|device| json!({
                    "serial": device.serial,
                    "state": device.state,
                    "model": device.model,
                    "product": device.product,
                    "device": device.device,
                })).collect::<Vec<_>>()
            }))
        }
        Op::Open(args) => {
            let info = driver.open(OpenRequest {
                serial: args.serial.clone(),
                user: args.user,
            })?;
            Ok(session_json(&info))
        }
        Op::Status => Ok(driver.status()),
        Op::Screenshot => {
            let result = driver.screenshot()?;
            Ok(screenshot_json(&result))
        }
        Op::ActivityInfo(args) => {
            let result = driver.activity_info(&args.component, args.user)?;
            Ok(activity_info_json(&result))
        }
        Op::Launch(args) => {
            let result = driver.launch(&args.component)?;
            Ok(launch_json(&result))
        }
        Op::Tap(args) => {
            let result = driver.tap(args.width, args.height, args.x, args.y)?;
            Ok(input_json(&result))
        }
        Op::Swipe(args) => {
            let result = driver.swipe(
                args.width,
                args.height,
                args.x1,
                args.y1,
                args.x2,
                args.y2,
                args.duration_ms,
            )?;
            Ok(input_json(&result))
        }
        Op::Key(args) => {
            let result = driver.key(args.keycode)?;
            Ok(input_json(&result))
        }
        Op::Close => {
            driver.close()?;
            Ok(json!({"state": "idle"}))
        }
    }
}

fn session_json(info: &SessionInfo) -> Value {
    json!({
        "state": "active",
        "serial": info.serial,
        "user": info.user,
        "endpoint": info.endpoint,
        "display_id": info.display_id,
        "width": info.width,
        "height": info.height,
        "dpi": info.dpi,
    })
}

fn screenshot_json(result: &ScreenshotResult) -> Value {
    json!({
        "display_id": result.display_id,
        "width": result.width,
        "height": result.height,
        "timestamp_ns": result.timestamp,
        "pixel_format": "RGBA_8888",
        "png_base64": result.png_base64,
    })
}

fn activity_info_json(result: &ActivityInfoResult) -> Value {
    json!({
        "user": result.user,
        "launch_mode": result.launch_mode,
        "task_affinity": result.task_affinity,
        "exported": result.exported,
    })
}

fn launch_json(result: &LaunchResult) -> Value {
    json!({
        "display_id": result.display_id,
        "component": result.component,
        "package": result.package,
        "user": result.user,
    })
}

fn input_json(result: &android_driver_core::InputResult) -> Value {
    json!({
        "display_id": result.display_id,
        "width": result.width,
        "height": result.height,
    })
}

fn emit(output: &mut impl Write, id: Option<u64>, frame: &Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(frame).map_err(io::Error::other)?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        let fallback = error_frame(
            id,
            &DriverError::new(
                ErrorCode::Internal,
                format!("response would exceed the {MAX_RESPONSE_BYTES} byte limit"),
            ),
        );
        bytes = serde_json::to_vec(&fallback).map_err(io::Error::other)?;
    }
    bytes.push(b'\n');
    output.write_all(&bytes)?;
    output.flush()
}

fn log_cleanup(errors: Vec<String>) {
    for error in errors {
        eprintln!("android-driver: cleanup failed: {error}");
    }
}

enum Next {
    Line(Vec<u8>),
    TooLong,
    Overflow,
    Eof,
}

struct InputHub {
    shared: Arc<Shared>,
}

struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
}

struct Queue {
    events: VecDeque<Inbound>,
    eof: bool,
    overflow: bool,
}

enum Inbound {
    Line(Vec<u8>),
    TooLong,
}

impl InputHub {
    fn start(cancel: CancelToken) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                events: VecDeque::new(),
                eof: false,
                overflow: false,
            }),
            ready: Condvar::new(),
        });
        let thread_shared = Arc::clone(&shared);
        thread::spawn(move || {
            let stdin = io::stdin();
            let mut reader = BufReader::new(stdin.lock());
            loop {
                match read_line_bounded(&mut reader, MAX_REQUEST_BYTES) {
                    Ok(None) => break,
                    Ok(Some(LineRead::Line(line))) => {
                        push(&thread_shared, Inbound::Line(line));
                    }
                    Ok(Some(LineRead::TooLong)) => {
                        push(&thread_shared, Inbound::TooLong);
                    }
                    Err(_) => break,
                }
            }
            let mut queue = thread_shared.queue.lock().expect("input queue lock");
            queue.eof = true;
            cancel.cancel();
            drop(queue);
            thread_shared.ready.notify_all();
        });
        Self { shared }
    }

    fn next(&self) -> Next {
        let mut queue = self.shared.queue.lock().expect("input queue lock");
        loop {
            if queue.eof {
                return Next::Eof;
            }
            if queue.overflow {
                queue.overflow = false;
                return Next::Overflow;
            }
            if let Some(event) = queue.events.pop_front() {
                return match event {
                    Inbound::Line(line) => Next::Line(line),
                    Inbound::TooLong => Next::TooLong,
                };
            }
            queue = self.shared.ready.wait(queue).expect("input queue wait");
        }
    }
}

fn push(shared: &Shared, event: Inbound) {
    let mut queue = shared.queue.lock().expect("input queue lock");
    if queue.events.len() >= MAX_QUEUED_REQUESTS {
        queue.overflow = true;
        return;
    }
    queue.events.push_back(event);
    drop(queue);
    shared.ready.notify_one();
}

fn parse_command_line(values: Vec<OsString>) -> Result<CommandLine, String> {
    let mut adb = OsString::from("adb");
    let mut endpoint = AdbEndpoint::parse("127.0.0.1:5037").map_err(|error| error.to_string())?;
    let mut helper = None;
    let mut lock_dir = None;
    let mut adb_timeout = Duration::from_millis(15_000);
    let mut helper_timeout = Duration::from_millis(15_000);
    let mut cleanup_timeout = Duration::from_millis(5_000);

    let mut index = 0;
    while index < values.len() {
        let argument = values[index]
            .to_str()
            .ok_or_else(|| "command-line arguments must be valid UTF-8".to_owned())?;
        let (name, inline) = match argument.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (argument, None),
        };
        match name {
            "--help" | "-h" => return Ok(CommandLine::Help),
            "--version" => return Ok(CommandLine::Version),
            "--adb" => {
                adb = take_value(&values, &mut index, name, inline)?;
            }
            "--adb-server" => {
                let value = take_string(&values, &mut index, name, inline)?;
                endpoint = AdbEndpoint::parse(&value).map_err(|error| error.to_string())?;
            }
            "--helper" => {
                helper = Some(PathBuf::from(take_value(
                    &values, &mut index, name, inline,
                )?));
            }
            "--lock-dir" => {
                lock_dir = Some(PathBuf::from(take_value(
                    &values, &mut index, name, inline,
                )?));
            }
            "--adb-timeout-ms" => {
                adb_timeout = parse_timeout(&values, &mut index, name, inline)?;
            }
            "--helper-timeout-ms" => {
                helper_timeout = parse_timeout(&values, &mut index, name, inline)?;
            }
            "--cleanup-timeout-ms" => {
                cleanup_timeout = parse_timeout(&values, &mut index, name, inline)?;
            }
            other => {
                return Err(format!("unknown argument `{other}`\n\n{}", usage()));
            }
        }
        index += 1;
    }

    Ok(CommandLine::Run(Args {
        adb,
        endpoint,
        helper,
        lock_dir,
        adb_timeout,
        helper_timeout,
        cleanup_timeout,
    }))
}

fn take_value(
    values: &[OsString],
    index: &mut usize,
    name: &str,
    inline: Option<&str>,
) -> Result<OsString, String> {
    if let Some(value) = inline {
        if value.is_empty() {
            return Err(format!("{name} requires a non-empty value"));
        }
        return Ok(OsString::from(value));
    }
    *index += 1;
    let value = values
        .get(*index)
        .ok_or_else(|| format!("{name} requires a value"))?;
    if value.is_empty() {
        return Err(format!("{name} requires a non-empty value"));
    }
    Ok(value.clone())
}

fn take_string(
    values: &[OsString],
    index: &mut usize,
    name: &str,
    inline: Option<&str>,
) -> Result<String, String> {
    let value = take_value(values, index, name, inline)?;
    value
        .into_string()
        .map_err(|_| format!("{name} must be valid UTF-8"))
}

fn parse_timeout(
    values: &[OsString],
    index: &mut usize,
    name: &str,
    inline: Option<&str>,
) -> Result<Duration, String> {
    let text = take_string(values, index, name, inline)?;
    let millis: u64 = text
        .parse()
        .map_err(|_| format!("{name} must be an integer number of milliseconds"))?;
    if !(100..=600_000).contains(&millis) {
        return Err(format!(
            "{name} must be between 100 and 600000 milliseconds"
        ));
    }
    Ok(Duration::from_millis(millis))
}

fn usage() -> String {
    format!(
        "\
android-driver {VERSION} - JSON-lines driver for one Android virtual display session

USAGE:
  android-driver [OPTIONS]

OPTIONS:
  --adb PATH                  adb executable (default: adb)
  --adb-server HOST:PORT      explicitly selected adb server (default: 127.0.0.1:5037)
  --helper PATH               virtual display helper jar built by helper/build.sh
  --lock-dir PATH             host directory for the cross-process device lock
  --adb-timeout-ms N          adb command timeout (default: 15000)
  --helper-timeout-ms N       helper protocol timeout (default: 15000)
  --cleanup-timeout-ms N      per-step cleanup timeout (default: 5000)
  --help, -h                  print this help
  --version                   print the version

PROTOCOL:
  One JSON request per stdin line, one JSON response per stdout line.
  The first stdout line is a ready frame with the protocol version and
  capabilities. Diagnostics are written to stderr only.
"
    )
}
