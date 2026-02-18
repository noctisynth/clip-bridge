//! X11 <-> Wayland Clipboard Bridge
//!
//! This program synchronizes clipboard content between X11 and Wayland compositors.

use clip_brige::{
    wayland::{GlobalData, WaylandState},
    x11::X11State,
    ClipboardContent, ClipboardType, SyncEvent,
};
// ============================================================================
// Main Application
// ============================================================================
//
use tracing::{debug, error, info};
use wayland_client::Connection;

use std::time::Duration;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    info!("Starting X11 <-> Wayland Clipboard Bridge");

    // Create channels for sync events
    let (x11_to_wayland_tx, mut x11_to_wayland_rx) = mpsc::unbounded_channel::<SyncEvent>();
    let (wayland_to_x11_tx, mut wayland_to_x11_rx) = mpsc::unbounded_channel::<SyncEvent>();

    // Create channels for setting clipboard
    let (set_x11_clipboard_tx, set_x11_clipboard_rx) =
        mpsc::unbounded_channel::<(String, ClipboardType)>();
    let (set_wayland_clipboard_tx, set_wayland_clipboard_rx) =
        mpsc::unbounded_channel::<(String, ClipboardType)>();

    // Clone for X11 thread
    let x11_sync_tx = x11_to_wayland_tx.clone();
    let wayland_sync_tx = wayland_to_x11_tx.clone();

    // Spawn X11 thread
    let x11_handle = tokio::task::spawn_blocking(move || {
        info!("[X11] Initializing X11 connection");

        let (conn, screen_num) =
            x11rb::connect(None).map_err(|e| format!("Failed to connect to X11: {}", e))?;
        let mut x11_state = X11State::new(conn, screen_num, x11_sync_tx, set_x11_clipboard_rx)
            .map_err(|e| format!("Failed to create X11 state: {}", e))?;

        info!("[X11] Connection established, window: {}", x11_state.window);

        // Request initial clipboard content
        info!("[X11] Requesting initial clipboard content");
        let _ = x11_state.request_clipboard_content(ClipboardType::Clipboard);
        let _ = x11_state.request_clipboard_content(ClipboardType::Primary);

        // Run X11 event loop
        if let Err(e) = x11_state.run_event_loop() {
            error!("[X11] Event loop error: {}", e);
        }

        Ok::<(), String>(())
    });

    // Initialize Wayland
    info!("[Wayland] Initializing Wayland connection");

    let wayland_conn = Connection::connect_to_env()?;
    let display = wayland_conn.display();
    let mut event_queue = wayland_conn.new_event_queue();
    let qh = event_queue.handle();

    let mut wayland_state = WaylandState::new(
        qh.clone(),
        wayland_sync_tx,
        set_wayland_clipboard_tx.clone(),
    );

    // Get registry
    display.get_registry(&qh, GlobalData);

    // Roundtrip to initialize globals
    event_queue.roundtrip(&mut wayland_state)?;

    // Additional roundtrip to ensure seat events are processed
    event_queue.roundtrip(&mut wayland_state)?;

    info!("[Wayland] Connection established");

    // Request initial Wayland clipboard content after everything is set up
    info!("[Wayland] Connection established, waiting for initial clipboard events");

    // Main sync loop
    let wayland_handle = tokio::task::spawn_blocking(move || {
        let mut set_wayland_clipboard_rx = set_wayland_clipboard_rx;
        loop {
            // Check for set clipboard requests
            if let Ok((content, clipboard_type)) = set_wayland_clipboard_rx.try_recv() {
                wayland_state.set_clipboard_content(content, clipboard_type);
            }

            // Process Wayland events - use blocking_dispatch() to wait for events
            if let Err(e) = event_queue.blocking_dispatch(&mut wayland_state) {
                error!("[Wayland] Dispatch error: {}", e);
            }
        }
    });

    // Handle sync events in main task
    tokio::spawn(async move {
        let mut x11_content: Option<String> = None;
        let mut primary_content: Option<String> = None;

        info!("[Sync] Starting sync loop");

        loop {
            tokio::select! {
                Some(event) = x11_to_wayland_rx.recv() => {
                    debug!("[Sync] Received event from X11: {:?}", event);
                    match event {
                        SyncEvent::X11ToWayland { content, clipboard_type } => {
                            debug!("[Sync] Matching content: {:?}", content);
                            match content {
                                ClipboardContent::Text(text) => {
                                    debug!("[Sync] X11 text content: {:?}", text);
                                    debug!("[Sync] Current x11_content: {:?}", x11_content);
                                    match clipboard_type {
                                        ClipboardType::Clipboard => {
                                            if x11_content.as_ref() != Some(&text) {
                                                info!("[Sync] X11 -> Wayland clipboard: {} chars", text.len());
                                                x11_content = Some(text.clone());
                                                debug!("[Sync] Sending to Wayland clipboard channel");
                                                match set_wayland_clipboard_tx.send((text, ClipboardType::Clipboard)) {
                                                    Ok(_) => debug!("[Sync] Sent to Wayland clipboard channel successfully"),
                                                    Err(e) => error!("[Sync] Failed to send to Wayland clipboard channel: {}", e),
                                                }
                                            } else {
                                                debug!("[Sync] X11 clipboard content unchanged, skipping");
                                            }
                                        }
                                        ClipboardType::Primary => {
                                            if primary_content.as_ref() != Some(&text) {
                                                info!("[Sync] X11 -> Wayland primary: {} chars", text.len());
                                                primary_content = Some(text.clone());
                                                debug!("[Sync] Sending to Wayland primary channel");
                                                match set_wayland_clipboard_tx.send((text, ClipboardType::Primary)) {
                                                    Ok(_) => debug!("[Sync] Sent to Wayland primary channel successfully"),
                                                    Err(e) => error!("[Sync] Failed to send to Wayland primary channel: {}", e),
                                                }
                                            } else {
                                                debug!("[Sync] X11 primary content unchanged, skipping");
                                            }
                                        }
                                    }
                                }
                                ClipboardContent::Empty => {
                                    debug!("[Sync] X11 empty content");
                                    match clipboard_type {
                                        ClipboardType::Clipboard => {
                                            x11_content = None;
                                        }
                                        ClipboardType::Primary => {
                                            primary_content = None;
                                        }
                                    }
                                }
                            }
                        }
                        _ => {
                            debug!("[Sync] Unhandled event from X11: {:?}", event);
                        }
                    }
                }
                Some(event) = wayland_to_x11_rx.recv() => {
                    debug!("[Sync] Received event from Wayland: {:?}", event);
                    match event {
                        SyncEvent::WaylandToX11 { content, clipboard_type } => {
                            debug!("[Sync] Matching Wayland content: {:?}", content);
                            match content {
                                ClipboardContent::Text(text) => {
                                    debug!("[Sync] Wayland text content: {:?}", text);
                                    match clipboard_type {
                                        ClipboardType::Clipboard => {
                                            if x11_content.as_ref() != Some(&text) {
                                                info!("[Sync] Wayland -> X11 clipboard: {} chars", text.len());
                                                x11_content = Some(text.clone());
                                                debug!("[Sync] Sending to X11 clipboard channel");
                                                match set_x11_clipboard_tx.send((text, ClipboardType::Clipboard)) {
                                                    Ok(_) => debug!("[Sync] Sent to X11 clipboard channel successfully"),
                                                    Err(e) => error!("[Sync] Failed to send to X11 clipboard channel: {}", e),
                                                }
                                            } else {
                                                debug!("[Sync] Wayland clipboard content unchanged, skipping");
                                            }
                                        }
                                        ClipboardType::Primary => {
                                            if primary_content.as_ref() != Some(&text) {
                                                info!("[Sync] Wayland -> X11 primary: {} chars", text.len());
                                                primary_content = Some(text.clone());
                                                debug!("[Sync] Sending to X11 primary channel");
                                                match set_x11_clipboard_tx.send((text, ClipboardType::Primary)) {
                                                    Ok(_) => debug!("[Sync] Sent to X11 primary channel successfully"),
                                                    Err(e) => error!("[Sync] Failed to send to X11 primary channel: {}", e),
                                                }
                                            } else {
                                                debug!("[Sync] Wayland primary content unchanged, skipping");
                                            }
                                        }
                                    }
                                }
                                ClipboardContent::Empty => {
                                    debug!("[Sync] Wayland empty content");
                                    match clipboard_type {
                                        ClipboardType::Clipboard => {
                                            x11_content = None;
                                        }
                                        ClipboardType::Primary => {
                                            primary_content = None;
                                        }
                                    }
                                }
                            }
                        }
                        _ => {
                            debug!("[Sync] Unhandled event from Wayland: {:?}", event);
                        }
                    }
                }
            }
        }
    });

    // Wait for tasks
    let (x11_result, wayland_result) = tokio::join!(x11_handle, wayland_handle);

    if let Err(e) = x11_result {
        error!("X11 task error: {:?}", e);
    }
    if let Err(e) = wayland_result {
        error!("Wayland task error: {:?}", e);
    }

    info!("Clipboard bridge shutting down");
    Ok(())
}
