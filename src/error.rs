use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidInput,
    NotFound,
    Conflict,
    Policy,
    Unavailable,
    Io,
}

#[derive(Clone, Debug)]
pub struct RuntimeError {
    pub kind: ErrorKind,
    pub message: String,
    pub code: Option<String>,
    pub os_code: Option<i32>,
}

impl RuntimeError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            code: None,
            os_code: None,
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidInput, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, message)
    }

    pub fn policy(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Policy, message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unavailable, message)
    }

    pub fn coded(kind: ErrorKind, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            code: Some(code.into()),
            os_code: None,
        }
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RuntimeError {}

impl From<std::io::Error> for RuntimeError {
    fn from(err: std::io::Error) -> Self {
        let kind = match err.kind() {
            std::io::ErrorKind::NotFound => ErrorKind::NotFound,
            std::io::ErrorKind::AlreadyExists => ErrorKind::Conflict,
            std::io::ErrorKind::PermissionDenied => ErrorKind::Policy,
            _ => ErrorKind::Io,
        };
        Self {
            kind,
            message: err.to_string(),
            code: None,
            os_code: err.raw_os_error(),
        }
    }
}

impl From<serde_json::Error> for RuntimeError {
    fn from(err: serde_json::Error) -> Self {
        Self::invalid(format!("Invalid JSON: {err}"))
    }
}

impl From<serde_yaml::Error> for RuntimeError {
    fn from(err: serde_yaml::Error) -> Self {
        Self::invalid(format!("Invalid YAML: {err}"))
    }
}

impl From<regex::Error> for RuntimeError {
    fn from(err: regex::Error) -> Self {
        Self::invalid(format!("Invalid regex: {err}"))
    }
}

pub type Result<T> = std::result::Result<T, RuntimeError>;
