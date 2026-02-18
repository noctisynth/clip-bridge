use clip_brige::x11::X11State;
use tokio::sync::mpsc::unbounded_channel;
use tracing_subscriber;
use x11rb::connect;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing subscriber for logging
    tracing_subscriber::fmt::init();

    // Connect to X11 server
    let (conn, screen_num) =
        connect(None).map_err(|e| format!("Failed to connect to X11: {}", e))?;

    // Create channels for sync events and clipboard set requests
    let (sync_tx, mut sync_rx) = unbounded_channel();
    let (_set_clipboard_tx, set_clipboard_rx) = unbounded_channel();

    // Create X11State
    let mut x11_state = X11State::new(conn, screen_num, sync_tx, set_clipboard_rx)?;

    println!("Starting X11 clipboard listener. Copy something to clipboard to test...");

    let handle = tokio::task::spawn_blocking(move || {
        if let Err(e) = x11_state.run_event_loop() {
            eprintln!("X11 listener error: {}", e);
        }
    });

    // Run event loop (blocking)
    while let Some(event) = sync_rx.recv().await {
        println!("Received sync event: {:?}", event);
    }

    handle.await?;

    Ok(())
}
