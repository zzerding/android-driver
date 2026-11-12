use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    Busy,
    Blocked,
    Unsupported,
    InvalidArgument,
    DeviceDisconnected,
    Timeout,
    CleanupFailed,
    InvalidState,
    Cancelled,
    Protocol,
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Busy => "Busy",
            Self::Blocked => "Blocked",
            Self::Unsupported => "Unsupported",
            Self::InvalidArgument => "InvalidArgument",
            Self::DeviceDisconnected => "DeviceDisconnected",
            Self::Timeout => "Timeout",
            Self::CleanupFailed => "CleanupFailed",
            Self::InvalidState => "InvalidState",
            Self::Cancelled => "Cancelled",
            Self::Protocol => "Protocol",
            Self::Internal => "Internal",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DriverError {
    pub code: ErrorCode,
    pub message: String,
    pub details: Option<Map<String, Value>>,
}

impl DriverError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    pub fn detail(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.details
            .get_or_insert_with(Map::new)
            .insert(key.to_owned(), value.into());
        self
    }

    pub fn merge_details(mut self, details: Map<String, Value>) -> Self {
        if details.is_empty() {
            return self;
        }
        let target = self.details.get_or_insert_with(Map::new);
        for (key, value) in details {
            target.insert(key, value);
        }
        self
    }

    pub fn to_json(&self) -> Value {
        let mut error = Map::new();
        error.insert("code".into(), Value::String(self.code.as_str().into()));
        error.insert("message".into(), Value::String(self.message.clone()));
        if let Some(details) = &self.details {
            error.insert("details".into(), Value::Object(details.clone()));
        }
        Value::Object(error)
    }
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for DriverError {}

pub type Result<T> = std::result::Result<T, DriverError>;

pub fn invalid_argument(message: impl Into<String>) -> DriverError {
    DriverError::new(ErrorCode::InvalidArgument, message)
}

pub fn internal(message: impl Into<String>) -> DriverError {
    DriverError::new(ErrorCode::Internal, message)
}

pub fn blocked(message: impl Into<String>) -> DriverError {
    DriverError::new(ErrorCode::Blocked, message)
}
