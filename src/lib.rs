pub mod wayland;
pub mod x11;

// ============================================================================
// Shared State
// ============================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum ClipboardContent {
    Text(String),
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardType {
    Clipboard,
    Primary,
}

#[derive(Debug)]
pub enum SyncEvent {
    X11ToWayland {
        content: ClipboardContent,
        clipboard_type: ClipboardType,
    },
    WaylandToX11 {
        content: ClipboardContent,
        clipboard_type: ClipboardType,
    },
}

pub const CURRENT_TIME: u32 = 0;

// ============================================================================
// Configuration
// ============================================================================

pub const CLIPBOARD_ATOM: &str = "CLIPBOARD";
pub const PRIMARY_ATOM: &str = "PRIMARY";
pub const TARGETS_ATOM: &str = "TARGETS";
pub const MULTIPLE_ATOM: &str = "MULTIPLE";
pub const INCR_ATOM: &str = "INCR";
pub const UTF8_STRING_ATOM: &str = "UTF8_STRING";
pub const TEXT_ATOM: &str = "TEXT";
pub const STRING_ATOM: &str = "STRING";
pub const TEXT_PLAIN_UTF8_ATOM: &str = "text/plain;charset=utf-8";
pub const TEXT_PLAIN_ATOM: &str = "text/plain";
