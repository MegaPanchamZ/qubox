//! Error types for pen capture and injection.

use thiserror::Error;

/// Errors returned from a [`crate::traits::PenCapture::start`] call
/// or an event pump run.
#[derive(Debug, Error)]
pub enum PenCaptureError {
    /// The platform-specific capture backend reported an I/O error.
    #[error("platform pen capture I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The user has no access to the platform input subsystem
    /// (Linux: not in the `input` group; macOS: TCC `Input Monitoring`
    /// not granted; Windows: pointer-input target not registered).
    #[error("pen capture permission denied; see ADR-010 §13 risks 1 & 5")]
    PermissionDenied,
    /// The requested feature gate is not enabled in this build.
    #[error("pen capture feature '{0}' is not enabled; rebuild with --features qubox-pen/{0}")]
    FeatureDisabled(&'static str),
    /// Generic platform error with a human-readable message.
    #[error("pen capture backend error: {0}")]
    Backend(String),
}

/// Errors returned from a [`crate::traits::PenInjector::inject`] call.
#[derive(Debug, Error)]
pub enum PenInjectError {
    /// I/O error from the virtual device (`/dev/uinput` etc.).
    #[error("platform pen injection I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The user has no write access to the platform injection
    /// subsystem (Linux: not in `uinput` group; macOS: TCC denied).
    #[error("pen injection permission denied; host-agent needs uinput group membership")]
    PermissionDenied,
    /// The requested feature gate is not enabled in this build.
    #[error("pen injection feature '{0}' is not enabled; rebuild with --features qubox-pen/{0}")]
    FeatureDisabled(&'static str),
    /// Generic platform error.
    #[error("pen injection backend error: {0}")]
    Backend(String),
}
