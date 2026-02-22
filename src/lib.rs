pub mod wayland;
pub mod x11;

use std::collections::HashMap;

// ============================================================================
// Shared State
// ============================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum ClipboardContent {
    Text(String),
    Binary(HashMap<String, Vec<u8>>),
    Empty,
}

impl ClipboardContent {
    pub fn new_binary() -> Self {
        ClipboardContent::Binary(HashMap::new())
    }

    pub fn add_mime(&mut self, mime_type: String, data: Vec<u8>) {
        if let ClipboardContent::Binary(map) = self {
            map.insert(mime_type, data);
        }
    }

    pub fn get_mime(&self, mime_type: &str) -> Option<&Vec<u8>> {
        if let ClipboardContent::Binary(map) = self {
            map.get(mime_type)
        } else {
            None
        }
    }

    pub fn get_text(&self) -> Option<&String> {
        if let ClipboardContent::Text(s) = self {
            Some(s)
        } else {
            None
        }
    }

    pub fn has_binary(&self) -> bool {
        matches!(self, ClipboardContent::Binary(_) )
    }

    pub fn mime_types(&self) -> Vec<String> {
        if let ClipboardContent::Binary(map) = self {
            map.keys().cloned().collect()
        } else {
            vec![]
        }
    }
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
