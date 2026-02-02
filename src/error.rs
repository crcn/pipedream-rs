use std::any::Any;
use std::sync::Arc;

/// Error from a downstream handler in the stream.
///
/// Errors are cloneable (via `Arc`) and can flow both:
/// - Through the stream (subscribable via `subscribe::<RelayError>()`)
/// - Back to senders (via `send().await` returning `Err(SendError::Downstream(...))`)
#[derive(Debug, Clone)]
pub struct RelayError {
    /// The message ID that caused this error (0 if unknown).
    pub msg_id: u64,
    /// The underlying error.
    pub error: Arc<dyn std::error::Error + Send + Sync>,
    /// Coarse provenance indicator for the error source.
    ///
    /// This should be a compile-time constant string like "sink", "tap",
    /// "within", "subscription", or "forwarder". Not intended for user input.
    pub source: &'static str,
}

impl RelayError {
    /// Create a new RelayError.
    pub fn new<E: std::error::Error + Send + Sync + 'static>(
        msg_id: u64,
        error: E,
        source: &'static str,
    ) -> Self {
        Self {
            msg_id,
            error: Arc::new(error),
            source,
        }
    }
}

impl std::fmt::Display for RelayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] msg {}: {}", self.source, self.msg_id, self.error)
    }
}

impl std::error::Error for RelayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// Wrapper for panic payloads, converting them to an Error type.
///
/// Panic payloads are intentionally lossy - we extract the message if it's
/// a string type, otherwise provide a generic indicator. This is the right
/// behavior for async boundaries and cross-task error propagation.
#[derive(Debug, Clone)]
pub struct PanicError {
    message: String,
}

impl PanicError {
    /// Create a PanicError from a panic payload.
    pub fn new(payload: Box<dyn Any + Send>) -> Self {
        let message = if let Some(s) = payload.downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "panic occurred (non-string payload)".to_string()
        };
        Self { message }
    }
}

impl std::fmt::Display for PanicError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "panic: {}", self.message)
    }
}

impl std::error::Error for PanicError {}
