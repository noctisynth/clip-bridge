use clip_bridge::{ClipboardType, wayland::WaylandState};
use tokio::sync::mpsc;
use tracing::info;
use wayland_client::Connection;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    let (sync_tx, _sync_rx) = mpsc::unbounded_channel();
    let (set_clipboard_tx, _set_clipboard_rx) =
        mpsc::unbounded_channel::<(String, ClipboardType)>();

    let wayland_conn = Connection::connect_to_env()?;
    let display = wayland_conn.display();
    let mut event_queue = wayland_conn.new_event_queue();
    let qh = event_queue.handle();

    let mut wayland_state = WaylandState::new(qh.clone(), sync_tx, set_clipboard_tx);

    display.get_registry(&qh, clip_bridge::wayland::GlobalData);

    info!("Before first roundtrip");
    event_queue.roundtrip(&mut wayland_state)?;
    info!("After first roundtrip");

    // Run the rest in spawn_blocking to have a tokio runtime
    tokio::task::spawn_blocking(move || {
        wayland_state.set_clipboard_content("Hello, World!".to_string(), ClipboardType::Clipboard);

        info!("Before second roundtrip");
        event_queue.roundtrip(&mut wayland_state).unwrap();
        info!("After second roundtrip");

        info!("Keeping event loop running...");
        loop {
            if let Err(e) = event_queue.blocking_dispatch(&mut wayland_state) {
                tracing::error!("Dispatch error: {}", e);
                break;
            }
        }
    })
    .await?;

    Ok(())
}
