use std::{fmt, sync::Arc};

use thiserror::Error;

pub const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendId {
    X11,
    Wayland,
}

impl BackendId {
    pub const fn other(self) -> Self {
        match self {
            Self::X11 => Self::Wayland,
            Self::Wayland => Self::X11,
        }
    }
}

impl fmt::Display for BackendId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::X11 => formatter.write_str("x11"),
            Self::Wayland => formatter.write_str("wayland"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
    Clipboard,
    Primary,
}

impl fmt::Display for SelectionKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Clipboard => formatter.write_str("clipboard"),
            Self::Primary => formatter.write_str("primary"),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Revision(u64);

impl Revision {
    #[cfg(test)]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }

    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BackendEpoch(u64);

impl BackendEpoch {
    #[cfg(test)]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }

    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct OfferToken(u64);

impl OfferToken {
    #[cfg(test)]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CommandId(u64);

impl CommandId {
    #[cfg(test)]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }

    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct TextPayload {
    text: Arc<str>,
}

impl TextPayload {
    pub fn from_string(text: String) -> Result<Self, TextPayloadError> {
        Self::validate(&text)?;
        Ok(Self {
            text: Arc::from(text),
        })
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, TextPayloadError> {
        let text = String::from_utf8(bytes).map_err(|_| TextPayloadError::InvalidUtf8)?;
        Self::from_string(text)
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    fn validate(text: &str) -> Result<(), TextPayloadError> {
        if text.is_empty() {
            return Err(TextPayloadError::Empty);
        }

        if text.len() > MAX_TEXT_BYTES {
            return Err(TextPayloadError::TooLarge {
                size: text.len(),
                max: MAX_TEXT_BYTES,
            });
        }

        Ok(())
    }
}

impl fmt::Debug for TextPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TextPayload")
            .field("bytes", &self.len())
            .finish()
    }
}

impl TryFrom<String> for TextPayload {
    type Error = TextPayloadError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::from_string(value)
    }
}

impl TryFrom<Vec<u8>> for TextPayload {
    type Error = TextPayloadError;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        Self::from_bytes(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextPayloadError {
    Empty,
    InvalidUtf8,
    TooLarge { size: usize, max: usize },
}

impl fmt::Display for TextPayloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("clipboard text is empty"),
            Self::InvalidUtf8 => formatter.write_str("clipboard text is not valid UTF-8"),
            Self::TooLarge { size, max } => {
                write!(formatter, "clipboard text is {size} bytes; limit is {max}")
            }
        }
    }
}

impl std::error::Error for TextPayloadError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCapabilities {
    pub clipboard: bool,
    pub primary: bool,
}

impl BackendCapabilities {
    pub const fn text_bridge() -> Self {
        Self {
            clipboard: true,
            primary: true,
        }
    }

    pub const fn supports(self, selection: SelectionKind) -> bool {
        match selection {
            SelectionKind::Clipboard => self.clipboard,
            SelectionKind::Primary => self.primary,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotOutcome {
    Text(TextPayload),
    Empty,
    Unsupported,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableReason {
    Cleared,
    Empty,
    Unsupported,
    InvalidUtf8,
    TooLarge,
    TransferFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StartupError {
    #[error("failed to initialize {backend} backend during {stage}: {detail}")]
    Backend {
        backend: BackendId,
        stage: &'static str,
        detail: String,
    },
    #[error("{backend} backend does not provide the required clipboard capability")]
    MissingClipboardCapability { backend: BackendId },
    #[error("startup snapshots did not complete before the deadline")]
    SnapshotTimeout,
    #[error("backend Ready handshake did not complete before the deadline")]
    BackendReadyTimeout,
    #[error("invalid logging filter: {detail}")]
    LoggingFilter { detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TransferError {
    #[error("selection format is unsupported")]
    Unsupported,
    #[error("selection is empty")]
    Empty,
    #[error("selection is not valid UTF-8")]
    InvalidUtf8,
    #[error("selection exceeds the {max}-byte limit ({size} bytes)")]
    TooLarge { size: usize, max: usize },
    #[error("transfer was idle for too long")]
    IdleTimeout,
    #[error("transfer exceeded its total deadline")]
    TotalTimeout,
    #[error("transfer was cancelled")]
    Cancelled,
    #[error("{operation} failed: {detail}")]
    Io {
        operation: &'static str,
        detail: String,
    },
}

impl TransferError {
    pub fn io(operation: &'static str, error: impl fmt::Display) -> Self {
        Self::Io {
            operation,
            detail: error.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProtocolError {
    #[error("{stage} failed: {detail}")]
    Operation { stage: &'static str, detail: String },
    #[error("invalid protocol state during {stage}: {detail}")]
    InvalidState { stage: &'static str, detail: String },
    #[error("protocol connection disconnected: {detail}")]
    Disconnected { detail: String },
}

impl ProtocolError {
    pub fn operation(stage: &'static str, error: impl fmt::Display) -> Self {
        Self::Operation {
            stage,
            detail: error.to_string(),
        }
    }

    pub fn invalid_state(stage: &'static str, detail: impl Into<String>) -> Self {
        Self::InvalidState {
            stage,
            detail: detail.into(),
        }
    }

    pub fn disconnected(error: impl fmt::Display) -> Self {
        Self::Disconnected {
            detail: error.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RecoverableError {
    #[error(transparent)]
    Transfer(#[from] TransferError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ShutdownError {
    #[error("{backend} actor did not stop before the shutdown deadline")]
    Timeout { backend: BackendId },
    #[error("{backend} actor panicked while shutting down: {detail}")]
    Join { backend: BackendId, detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_payload_rejects_empty_text() {
        assert_eq!(
            TextPayload::from_string(String::new()),
            Err(TextPayloadError::Empty)
        );
    }

    #[test]
    fn text_payload_rejects_invalid_utf8() {
        assert_eq!(
            TextPayload::from_bytes(vec![0xff]),
            Err(TextPayloadError::InvalidUtf8)
        );
    }

    #[test]
    fn text_payload_accepts_limit_and_rejects_larger_text() {
        let at_limit = "a".repeat(MAX_TEXT_BYTES);
        assert!(TextPayload::from_string(at_limit).is_ok());

        let above_limit = "a".repeat(MAX_TEXT_BYTES + 1);
        assert_eq!(
            TextPayload::from_string(above_limit),
            Err(TextPayloadError::TooLarge {
                size: MAX_TEXT_BYTES + 1,
                max: MAX_TEXT_BYTES,
            })
        );
    }

    #[test]
    fn text_payload_preserves_newlines_unicode_and_nul() {
        let text = "first line\n雪\0last line";
        let payload = TextPayload::from_bytes(text.as_bytes().to_vec())
            .expect("embedded NUL is valid UTF-8 text");
        assert_eq!(payload.as_str().as_bytes(), text.as_bytes());
    }

    #[test]
    fn debug_does_not_expose_clipboard_text() {
        let payload = TextPayload::from_string("sensitive clipboard text".to_owned())
            .expect("the test payload is valid non-empty UTF-8");

        let debug = format!("{payload:?}");
        assert_eq!(debug, "TextPayload { bytes: 24 }");
        assert!(!debug.contains("sensitive"));
    }
}
