use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::error::{DriverError, invalid_argument};
use crate::png::MAX_PNG_BYTES;
use crate::validate::SAFE_KEYCODES;

pub const PROTOCOL_VERSION: u64 = 1;
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub struct ParsedRequest {
    pub id: u64,
    pub op: Op,
}

#[derive(Debug)]
pub enum Op {
    ListDevices,
    Open(OpenArgs),
    Status,
    Screenshot,
    ActivityInfo(ActivityInfoArgs),
    Launch(LaunchArgs),
    Tap(TapArgs),
    Swipe(SwipeArgs),
    Key(KeyArgs),
    Close,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenArgs {
    pub serial: String,
    pub user: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityInfoArgs {
    pub component: String,
    pub user: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchArgs {
    pub component: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TapArgs {
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwipeArgs {
    pub width: u32,
    pub height: u32,
    pub x1: u32,
    pub y1: u32,
    pub x2: u32,
    pub y2: u32,
    #[serde(default = "default_swipe_duration")]
    pub duration_ms: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyArgs {
    pub keycode: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

fn default_swipe_duration() -> u32 {
    300
}

#[derive(Debug)]
pub struct ParseError {
    pub id: Option<u64>,
    pub message: String,
}

impl ParseError {
    pub fn into_driver_error(self) -> DriverError {
        invalid_argument(self.message)
    }
}

pub fn parse_request(line: &[u8]) -> std::result::Result<ParsedRequest, ParseError> {
    if line.len() > MAX_REQUEST_BYTES {
        return Err(ParseError {
            id: None,
            message: format!("request line exceeds the {MAX_REQUEST_BYTES} byte limit"),
        });
    }
    let value: Value = serde_json::from_slice(line).map_err(|error| ParseError {
        id: None,
        message: format!("request is not valid JSON: {error}"),
    })?;
    let Value::Object(mut object) = value else {
        return Err(ParseError {
            id: None,
            message: "request must be a JSON object".to_owned(),
        });
    };
    let id = match object.remove("id") {
        Some(Value::Number(number)) => number.as_u64().filter(|value| *value > 0),
        _ => None,
    };
    let Some(id) = id else {
        return Err(ParseError {
            id: None,
            message: "request id must be a positive integer".to_owned(),
        });
    };
    let Some(Value::String(op)) = object.remove("op") else {
        return Err(ParseError {
            id: Some(id),
            message: "request op must be a string".to_owned(),
        });
    };

    let parsed = match op.as_str() {
        "list_devices" => parse_args::<EmptyArgs>(id, object).map(|_| Op::ListDevices),
        "open" => parse_args(id, object).map(Op::Open),
        "status" => parse_args::<EmptyArgs>(id, object).map(|_| Op::Status),
        "screenshot" => parse_args::<EmptyArgs>(id, object).map(|_| Op::Screenshot),
        "activity_info" => parse_args(id, object).map(Op::ActivityInfo),
        "launch" => parse_args(id, object).map(Op::Launch),
        "tap" => parse_args(id, object).map(Op::Tap),
        "swipe" => parse_args(id, object).map(Op::Swipe),
        "key" => parse_args(id, object).map(Op::Key),
        "close" => parse_args::<EmptyArgs>(id, object).map(|_| Op::Close),
        other => {
            return Err(ParseError {
                id: Some(id),
                message: format!("unknown op `{other}`"),
            });
        }
    }?;
    Ok(ParsedRequest { id, op: parsed })
}

fn parse_args<T: DeserializeOwned>(
    id: u64,
    object: Map<String, Value>,
) -> std::result::Result<T, ParseError> {
    serde_json::from_value(Value::Object(object)).map_err(|error| ParseError {
        id: Some(id),
        message: error.to_string(),
    })
}

pub fn startup_frame(version: &str) -> Value {
    json!({
        "type": "ready",
        "protocol": PROTOCOL_VERSION,
        "version": version,
        "capabilities": {
            "operations": [
                "list_devices", "open", "status", "screenshot", "activity_info",
                "launch", "tap", "swipe", "key", "close"
            ],
            "virtual_display": true,
            "android_sdk": 34,
            "adb_uid": 2000,
            "raw_pixel_input": true,
            "text_input": false,
            "unicode_text_input": false,
            "text_input_note": "adb shell input text cannot deliver arbitrary Unicode; no text op is exposed",
            "display_isolation": "display-and-input-only; app tasks and processes are shared",
            "max_request_bytes": MAX_REQUEST_BYTES,
            "max_png_bytes": MAX_PNG_BYTES,
            "max_response_bytes": MAX_RESPONSE_BYTES,
            "safe_keycodes": SAFE_KEYCODES,
        },
    })
}

pub fn ok_frame(id: u64, result: Value) -> Value {
    json!({"type": "response", "id": id, "ok": true, "result": result})
}

pub fn error_frame(id: Option<u64>, error: &DriverError) -> Value {
    json!({"type": "response", "id": id, "ok": false, "error": error.to_json()})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_operation_shape() {
        let request = parse_request(br#"{"id":1,"op":"open","serial":"S","user":10}"#).unwrap();
        assert_eq!(request.id, 1);
        assert!(matches!(request.op, Op::Open(OpenArgs { ref serial, user: 10 }) if serial == "S"));

        let request = parse_request(
            br#"{"id":2,"op":"swipe","width":10,"height":20,"x1":1,"y1":2,"x2":3,"y2":4}"#,
        )
        .unwrap();
        assert!(matches!(
            request.op,
            Op::Swipe(SwipeArgs {
                duration_ms: 300,
                ..
            })
        ));

        parse_request(br#"{"id":3,"op":"status"}"#).unwrap();
        parse_request(br#"{"id":4,"op":"close"}"#).unwrap();
    }

    #[test]
    fn rejects_unknown_fields_missing_ids_and_unknown_ops() {
        let error = parse_request(br#"{"id":1,"op":"status","extra":true}"#).unwrap_err();
        assert_eq!(error.id, Some(1));
        assert!(error.message.contains("unknown field"));

        let error = parse_request(br#"{"op":"status"}"#).unwrap_err();
        assert_eq!(error.id, None);

        let error = parse_request(br#"{"id":1,"op":"teleport"}"#).unwrap_err();
        assert_eq!(error.id, Some(1));
        assert!(error.message.contains("unknown op"));

        assert!(parse_request(br#"{"id":0,"op":"status"}"#).is_err());
        assert!(parse_request(br#"[]"#).is_err());
        assert!(parse_request(br#"{"id":1,"op":"diagnostics"}"#).is_err());
    }

    #[test]
    fn rejects_oversized_request_without_parsing() {
        let mut line = br#"{"id":1,"op":"status","pad":""#.to_vec();
        line.extend(std::iter::repeat_n(b'a', MAX_REQUEST_BYTES));
        let error = parse_request(&line).unwrap_err();
        assert_eq!(error.id, None);
        assert!(error.message.contains("limit"));
    }
}
