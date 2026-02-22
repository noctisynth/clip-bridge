use clip_bridge::{
    wayland::{GlobalData, WaylandState},
    ClipboardType,
};
use tokio::sync::mpsc;
use tracing_subscriber;
use wayland_client::Connection;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    // Create channels for sync events and clipboard set requests
    let (sync_tx, mut sync_rx) = mpsc::unbounded_channel();
    let (set_clipboard_tx, _set_clipboard_rx) =
        mpsc::unbounded_channel::<(String, ClipboardType)>();

    // Connect to Wayland server
    let wayland_conn = Connection::connect_to_env()?;
    let display = wayland_conn.display();
    let mut event_queue = wayland_conn.new_event_queue();
    let qh = event_queue.handle();

    // Create WaylandState
    let mut wayland_state = WaylandState::new(qh.clone(), sync_tx, set_clipboard_tx);

    // Acquire global data
    display.get_registry(&qh, GlobalData);

    // Initial roundtrip to get globals
    event_queue.roundtrip(&mut wayland_state)?;

    println!("Starting Wayland clipboard listener. Copy something to clipboard to test...");

    // Run event loop in a separate blocking task
    let handle = tokio::task::spawn_blocking(move || loop {
        if let Err(e) = event_queue.blocking_dispatch(&mut wayland_state) {
            eprintln!("Wayland dispatch error: {}", e);
            break;
        }
    });

    // Process received events
    while let Some(event) = sync_rx.recv().await {
        println!("Received Wayland clipboard event: {:?}", event);
    }

    handle.await?;

    Ok(())
}
