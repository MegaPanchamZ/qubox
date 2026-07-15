use crate::types::DisplayId;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("display backend not supported on this platform/session: {0}")]
    NotSupported(&'static str),
    #[error("display {0:?} not found")]
    DisplayNotFound(DisplayId),
    #[error("X11 error: {0}")]
    X11(String),
    #[error("DXGI error: {0}")]
    Dxgi(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("timeout")]
    Timeout,
    #[error("other: {0}")]
    Other(String),
}

impl From<Box<dyn std::error::Error>> for CaptureError {
    fn from(err: Box<dyn std::error::Error>) -> Self {
        CaptureError::Other(err.to_string())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DisplayError {
    #[error("display backend not supported on this platform/session: {0}")]
    NotSupported(&'static str),
    #[error("display {0:?} not found")]
    DisplayNotFound(DisplayId),
    #[error("virtual display creation failed: {0}")]
    VirtualDisplayFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("other: {0}")]
    Other(String),
}

impl From<Box<dyn std::error::Error>> for DisplayError {
    fn from(err: Box<dyn std::error::Error>) -> Self {
        DisplayError::Other(err.to_string())
    }
}
