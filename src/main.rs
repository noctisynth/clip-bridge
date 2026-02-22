//! X11 <-> Wayland Clipboard Bridge
//!
//! This program synchronizes clipboard content between X11 and Wayland compositors.

use clip_bridge::{
    ClipboardContent, ClipboardType, SyncEvent,
    wayland::{GlobalData, WaylandState},
    x11::X11State,
};
// ============================================================================
// Main Application
// ============================================================================
//
use tracing::{debug, error, info};
use wayland_client::{Connection, DispatchError};

use tokio::{sync::mpsc, task::JoinHandle};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt().init();

    info!("Starting X11 <-> Wayland Clipboard Bridge");

    // Create channels for sync events
    let (x11_to_wayland_tx, mut x11_to_wayland_rx) = mpsc::unbounded_channel::<SyncEvent>();
    let (wayland_to_x11_tx, mut wayland_to_x11_rx) = mpsc::unbounded_channel::<SyncEvent>();

    // Create channels for setting clipboard
    let (set_x11_clipboard_tx, set_x11_clipboard_rx) =
        mpsc::unbounded_channel::<(ClipboardContent, ClipboardType)>();
    let (set_wayland_clipboard_tx, set_wayland_clipboard_rx) =
        mpsc::unbounded_channel::<(ClipboardContent, ClipboardType)>();

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

        // Run X11 event loop
        // Note: We don't request clipboard content here on startup.
        // Instead, we wait for XFixes selection events which indicate
        // when another application owns the selection. This avoids the
        // race condition where we request content before any app has set it.
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

    info!("[Wayland] Connection established");

    // Main sync loop
    let wayland_handle: JoinHandle<Result<(), DispatchError>> =
        tokio::task::spawn_blocking(move || {
            let mut set_wayland_clipboard_rx = set_wayland_clipboard_rx;
            loop {
                if let Ok((content, clipboard_type)) = set_wayland_clipboard_rx.try_recv() {
                    wayland_state.set_clipboard_content(content, clipboard_type);
                }

                event_queue.roundtrip(&mut wayland_state)?;

                if let Err(e) = event_queue.dispatch_pending(&mut wayland_state) {
                    error!("[Wayland] Dispatch error: {}", e);
                }
            }
        });

    // Handle sync events in main task
    tokio::spawn(async move {
        let mut x11_clipboard: Option<ClipboardContent> = None;
        let mut x11_primary: Option<ClipboardContent> = None;

        info!("[Sync] Starting sync loop");

        fn contents_equal(a: &Option<ClipboardContent>, b: &ClipboardContent) -> bool {
            match (a, b) {
                (Some(ClipboardContent::Text(t1)), ClipboardContent::Text(t2)) => t1 == t2,
                (Some(ClipboardContent::Binary(m1)), ClipboardContent::Binary(m2)) => {
                    m1 == m2
                }
                _ => false,
            }
        }

        loop {
            tokio::select! {
                Some(event) = x11_to_wayland_rx.recv() => {
                    debug!("[Sync] Received event from X11: {:?}", event);
                    match event {
                        SyncEvent::X11ToWayland { content, clipboard_type } => {
                            debug!("[Sync] X11 content: {:?}", content);
                            let current = match clipboard_type {
                                ClipboardType::Clipboard => &x11_clipboard,
                                ClipboardType::Primary => &x11_primary,
                            };
                            if !contents_equal(&current, &content) {
                                match clipboard_type {
                                    ClipboardType::Clipboard => {
                                        info!("[Sync] X11 -> Wayland clipboard: {:?}", content);
                                        x11_clipboard = Some(content.clone());
                                    }
                                    ClipboardType::Primary => {
                                        info!("[Sync] X11 -> Wayland primary: {:?}", content);
                                        x11_primary = Some(content.clone());
                                    }
                                }
                                debug!("[Sync] Sending to Wayland channel");
                                match set_wayland_clipboard_tx.send((content, clipboard_type)) {
                                    Ok(_) => debug!("[Sync] Sent to Wayland channel successfully"),
                                    Err(e) => error!("[Sync] Failed to send to Wayland channel: {}", e),
                                }
                            } else {
                                debug!("[Sync] X11 content unchanged, skipping");
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
                            debug!("[Sync] Wayland content: {:?}", content);
                            let current = match clipboard_type {
                                ClipboardType::Clipboard => &x11_clipboard,
                                ClipboardType::Primary => &x11_primary,
                            };
                            if !contents_equal(&current, &content) {
                                match clipboard_type {
                                    ClipboardType::Clipboard => {
                                        info!("[Sync] Wayland -> X11 clipboard: {:?}", content);
                                        x11_clipboard = Some(content.clone());
                                    }
                                    ClipboardType::Primary => {
                                        info!("[Sync] Wayland -> X11 primary: {:?}", content);
                                        x11_primary = Some(content.clone());
                                    }
                                }
                                debug!("[Sync] Sending to X11 channel");
                                match set_x11_clipboard_tx.send((content, clipboard_type)) {
                                    Ok(_) => debug!("[Sync] Sent to X11 channel successfully"),
                                    Err(e) => error!("[Sync] Failed to send to X11 channel: {}", e),
                                }
                            } else {
                                debug!("[Sync] Wayland content unchanged, skipping");
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
