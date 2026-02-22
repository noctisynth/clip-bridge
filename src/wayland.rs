use std::collections::HashMap;
use std::fs::File;
use std::os::fd::AsFd;
use std::sync::Arc;

use nix::unistd;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use wayland_client::{
    Connection, Dispatch, QueueHandle, event_created_child,
    protocol::{wl_compositor, wl_registry, wl_seat},
};
use wayland_protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1,
    zwp_primary_selection_offer_v1::{self, ZwpPrimarySelectionOfferV1},
    zwp_primary_selection_source_v1::{self, ZwpPrimarySelectionSourceV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

use crate::{ClipboardContent, ClipboardType, SyncEvent};

// ============================================================================
// Wayland State
// ============================================================================

#[derive(Debug, Clone, Copy)]
pub struct GlobalData;

pub struct WaylandState {
    _qh: QueueHandle<Self>,
    sync_tx: mpsc::UnboundedSender<SyncEvent>,
    data_control_manager: Option<ZwlrDataControlManagerV1>,
    data_control_device: Option<ZwlrDataControlDeviceV1>,
    primary_selection_manager: Option<ZwpPrimarySelectionDeviceManagerV1>,
    compositor: Option<wl_compositor::WlCompositor>,
    seat: Option<wl_seat::WlSeat>,
    clipboard_content: Arc<Mutex<Option<ClipboardContent>>>,
    primary_content: Arc<Mutex<Option<ClipboardContent>>>,
    clipboard_source: Option<ZwlrDataControlSourceV1>,
    primary_source: Option<ZwlrDataControlSourceV1>,
    _set_clipboard_tx: mpsc::UnboundedSender<(ClipboardContent, ClipboardType)>,
    pending_primary_content: Arc<Mutex<Option<ClipboardContent>>>,
}

impl WaylandState {
    pub fn new(
        qh: QueueHandle<Self>,
        sync_tx: mpsc::UnboundedSender<SyncEvent>,
        set_clipboard_tx: mpsc::UnboundedSender<(ClipboardContent, ClipboardType)>,
    ) -> Self {
        Self {
            _qh: qh,
            sync_tx,
            data_control_manager: None,
            data_control_device: None,
            primary_selection_manager: None,
            compositor: None,
            seat: None,
            clipboard_content: Arc::new(Mutex::new(None)),
            primary_content: Arc::new(Mutex::new(None)),
            clipboard_source: None,
            primary_source: None,
            _set_clipboard_tx: set_clipboard_tx,
            pending_primary_content: Arc::new(Mutex::new(None)),
        }
    }

    pub fn set_clipboard_content(&mut self, content: ClipboardContent, clipboard_type: ClipboardType) {
        info!(
            "[Wayland] Setting clipboard content: type={:?}, content={:?}",
            clipboard_type,
            content
        );

        let device = if let Some(d) = &self.data_control_device {
            d.clone()
        } else {
            warn!("[Wayland] No data control device available");
            return;
        };

        match clipboard_type {
            ClipboardType::Clipboard => {
                *self.clipboard_content.blocking_lock() = Some(content.clone());

                if let Some(manager) = &self.data_control_manager {
                    let source = manager.create_data_source(&self._qh, ());

                    match &content {
                        ClipboardContent::Text(_) => {
                            source.offer("text/plain;charset=utf-8".into());
                            source.offer("text/plain".into());
                            source.offer("UTF8_STRING".into());
                            source.offer("TEXT".into());
                            source.offer("STRING".into());
                        }
                        ClipboardContent::Binary(mimes) => {
                            for mime in mimes.keys() {
                                source.offer(mime.clone().into());
                            }
                        }
                        ClipboardContent::Empty => {}
                    }

                    debug!("[Wayland] Created clipboard source: {:?}", source);

                    device.set_selection(Some(&source));
                    debug!("[Wayland] Set clipboard selection");

                    if let Some(old_source) = self.clipboard_source.take() {
                        debug!("[Wayland] Destroying old clipboard source");
                        old_source.destroy();
                    }

                    self.clipboard_source = Some(source);
                    info!("[Wayland] Clipboard content set successfully");
                } else {
                    warn!("[Wayland] No data control manager available");
                }
            }
            ClipboardType::Primary => {
                *self.pending_primary_content.blocking_lock() = Some(content.clone());
                *self.primary_content.blocking_lock() = Some(content.clone());

                // Create new source BEFORE destroying old one to avoid gap
                if let Some(manager) = &self.data_control_manager {
                    let source = manager.create_data_source(&self._qh, ());

                    match &content {
                        ClipboardContent::Text(_) => {
                            source.offer("text/plain;charset=utf-8".into());
                            source.offer("text/plain".into());
                            source.offer("UTF8_STRING".into());
                            source.offer("TEXT".into());
                            source.offer("STRING".into());
                        }
                        ClipboardContent::Binary(mimes) => {
                            for mime in mimes.keys() {
                                source.offer(mime.clone().into());
                            }
                        }
                        ClipboardContent::Empty => {}
                    }

                    debug!("[Wayland] Created primary source: {:?}", source);

                    device.set_primary_selection(Some(&source));
                    debug!("[Wayland] Set primary selection");

                    // Now destroy old source after new one is active
                    if let Some(old_source) = self.primary_source.take() {
                        debug!("[Wayland] Destroying old primary source");
                        old_source.destroy();
                    }

                    self.primary_source = Some(source);
                    info!("[Wayland] Primary selection content set successfully");
                }
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalData> for WaylandState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &GlobalData,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                debug!(
                    "[Wayland] Global: {} v{} (name: {})",
                    interface, version, name
                );

                match interface.as_str() {
                    "wl_compositor" => {
                        state.compositor = Some(
                            registry
                                .bind::<wl_compositor::WlCompositor, _, _>(name, 4, qh, GlobalData),
                        );
                    }
                    "wl_seat" => {
                        state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(name, 7, qh, ()));
                        if let Some(manager) = &state.data_control_manager
                            && let Some(seat) = &state.seat
                        {
                            state.data_control_device = Some(manager.get_data_device(seat, qh, ()));
                        }
                    }
                    "zwlr_data_control_manager_v1" => {
                        state.data_control_manager =
                            Some(registry.bind::<ZwlrDataControlManagerV1, _, _>(name, 2, qh, ()));
                        if let Some(seat) = &state.seat {
                            state.data_control_device = Some(
                                state
                                    .data_control_manager
                                    .as_ref()
                                    .unwrap()
                                    .get_data_device(seat, qh, ()),
                            );
                        }
                    }
                    "zwp_primary_selection_device_manager_v1" => {
                        state.primary_selection_manager =
                            Some(registry.bind::<ZwpPrimarySelectionDeviceManagerV1, _, _>(
                                name,
                                1,
                                qh,
                                (),
                            ));
                    }
                    _ => {}
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                debug!("[Wayland] Global removed: {}", name);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, GlobalData> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_compositor::WlCompositor,
        _event: wl_compositor::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for WaylandState {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_seat::Event::Capabilities { capabilities } => {
                debug!("[Wayland] Seat capabilities: {:?}", capabilities);
            }
            wl_seat::Event::Name { name } => {
                debug!("[Wayland] Seat name: {}", name);
            }
            _ => {
                debug!("[Wayland] Seat event: {:?}", event);
            }
        }

        // Bind data control device if manager is available
        // Check every time since manager might be bound after seat
        debug!(
            "[Wayland] Checking data control device binding: device={:?}, manager={:?}",
            state.data_control_device.is_some(),
            state.data_control_manager.is_some()
        );
        if state.data_control_device.is_none() {
            if let Some(manager) = &state.data_control_manager {
                state.data_control_device = Some(manager.get_data_device(seat, qh, ()));
                info!("[Wayland] Data control device bound");
            } else {
                debug!("[Wayland] Data control manager not available yet");
            }
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _device: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                debug!("[Wayland] New data offer: {:?}", id);
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                info!("[Wayland] Selection changed: {:?}", id);
                if let Some(offer) = id {
                    let mime_types = vec![
                        "text/plain;charset=utf-8".to_string(),
                        "text/plain".to_string(),
                        "UTF8_STRING".to_string(),
                        "image/png".to_string(),
                        "image/bmp".to_string(),
                        "image/jpeg".to_string(),
                    ];

                    let sync_tx = state.sync_tx.clone();
                    let offer = offer.clone();

                    std::thread::spawn(move || {
                        let mut all_data: HashMap<String, Vec<u8>> = HashMap::new();
                        let mut has_text = false;

                        for mime in mime_types {
                            match unistd::pipe() {
                                Ok((read_fd, write_fd)) => {
                                    offer.receive(mime.clone(), write_fd.as_fd());
                                    let _ = unistd::close(write_fd);
                                    let mut read_file = File::from(read_fd);
                                    let mut buffer = Vec::new();
                                    let mut chunk = [0u8; 8192];

                                    loop {
                                        match std::io::Read::read(&mut read_file, &mut chunk) {
                                            Ok(0) => break,
                                            Ok(n) => {
                                                buffer.extend_from_slice(&chunk[..n]);
                                            }
                                            Err(e) => {
                                                debug!("[Wayland] Failed to read {}: {}", mime, e);
                                                break;
                                            }
                                        }
                                    }

                                    if !buffer.is_empty() {
                                        debug!("[Wayland] Read {} bytes for MIME: {}", buffer.len(), mime);
                                        if mime.starts_with("text/") || mime == "UTF8_STRING" || mime == "STRING" {
                                            has_text = true;
                                        }
                                        all_data.insert(mime, buffer);
                                    }
                                }
                                Err(e) => {
                                    debug!("[Wayland] Failed to create pipe for {}: {}", mime, e);
                                }
                            }
                        }

                        if !all_data.is_empty() {
                            let content = if has_text {
                                if let Some(text_data) = all_data.get("text/plain;charset=utf-8") {
                                    if let Ok(text) = String::from_utf8(text_data.clone()) {
                                        if !text.is_empty() {
                                            info!("[Wayland] Clipboard content: {} chars text + {} binary types", text.len(), all_data.len());
                                            let _ = sync_tx.send(SyncEvent::WaylandToX11 {
                                                content: ClipboardContent::Text(text.clone()),
                                                clipboard_type: ClipboardType::Clipboard,
                                            });
                                            return;
                                        }
                                    }
                                }
                                if let Some(text_data) = all_data.get("text/plain") {
                                    if let Ok(text) = String::from_utf8(text_data.clone()) {
                                        if !text.is_empty() {
                                            info!("[Wayland] Clipboard content: {} chars text + {} binary types", text.len(), all_data.len());
                                            let _ = sync_tx.send(SyncEvent::WaylandToX11 {
                                                content: ClipboardContent::Text(text),
                                                clipboard_type: ClipboardType::Clipboard,
                                            });
                                            return;
                                        }
                                    }
                                }
                                ClipboardContent::Binary(all_data)
                            } else {
                                info!("[Wayland] Clipboard content: {} binary types", all_data.len());
                                ClipboardContent::Binary(all_data)
                            };

                            let _ = sync_tx.send(SyncEvent::WaylandToX11 {
                                content: content.clone(),
                                clipboard_type: ClipboardType::Clipboard,
                            });
                        } else {
                            warn!("[Wayland] No clipboard content received");
                        }
                    });
                } else {
                    // Selection cleared
                    info!("[Wayland] Selection cleared");
                    let sync_tx = state.sync_tx.clone();
                    std::thread::spawn(move || {
                        let _ = sync_tx.send(SyncEvent::WaylandToX11 {
                            content: ClipboardContent::Empty,
                            clipboard_type: ClipboardType::Clipboard,
                        });
                    });
                }
            }
            zwlr_data_control_device_v1::Event::PrimarySelection { id: _id } => {
                // Primary selection may very large and frequent, this will cause performance issue.
                // So we ignore it for now.

                // info!("[Wayland] Primary selection changed: {:?}", id);
                // if let Some(offer) = id {
                //     match unistd::pipe() {
                //         Ok((read_fd, write_fd)) => {
                //             debug!("[Wayland] Created pipe for reading primary selection data");
                //             offer.receive("text/plain;charset=utf-8".into(), write_fd.as_fd());
                //             let _ = unistd::close(write_fd);
                //             debug!("[Wayland] Closed write_fd for primary selection");
                //             let read_file = File::from(read_fd);
                //             let sync_tx = state.sync_tx.clone();
                //             let content_ref = state.primary_content.clone();
                //             tokio::spawn(async move {
                //                 use tokio::io::AsyncReadExt;
                //                 let mut reader = tokio::fs::File::from_std(read_file);
                //                 let mut buffer = Vec::new();
                //                 match reader.read_to_end(&mut buffer).await {
                //                     Ok(n) => {
                //                         debug!("[Wayland] Read {} bytes from primary pipe", n);
                //                         if let Ok(text) = String::from_utf8(buffer) {
                //                             info!(
                //                                 "[Wayland] Primary selection content received: {} chars",
                //                                 text.len()
                //                             );
                //                             *content_ref.lock().await = Some(text.clone());
                //                             let _ = sync_tx.send(SyncEvent::WaylandToX11 {
                //                                 content: ClipboardContent::Text(text),
                //                                 clipboard_type: ClipboardType::Primary,
                //                             });
                //                         } else {
                //                             warn!("[Wayland] Failed to decode primary as UTF-8");
                //                         }
                //                     }
                //                     Err(e) => {
                //                         error!("[Wayland] Failed to read from pipe: {}", e);
                //                     }
                //                 }
                //             });
                //         }
                //         Err(e) => {
                //             error!("[Wayland] Failed to create pipe: {}", e);
                //         }
                //     }
                // } else {
                //     info!("[Wayland] Primary selection cleared");
                //     let content_ref = state.primary_content.clone();
                //     let sync_tx = state.sync_tx.clone();
                //     tokio::spawn(async move {
                //         *content_ref.lock().await = None;
                //         let _ = sync_tx.send(SyncEvent::WaylandToX11 {
                //             content: ClipboardContent::Empty,
                //             clipboard_type: ClipboardType::Primary,
                //         });
                //     });
                // }
            }
            zwlr_data_control_device_v1::Event::Finished => {
                debug!("[Wayland] Data control device finished");
            }
            _ => {}
        }
    }

    event_created_child!(WaylandState, ZwlrDataControlDeviceV1, [
        0 => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            debug!("[Wayland] Offer mime type: {}", mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        debug!("[Wayland] Source event received: {:?}", event);
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                info!(
                    "[Wayland] Send data for mime type: {} from source: {:?}",
                    mime_type, source
                );

                // Determine which content to send based on source
                let content = if Some(source) == state.clipboard_source.as_ref() {
                    debug!("[Wayland] This is clipboard source");
                    state.clipboard_content.blocking_lock().clone()
                } else if Some(source) == state.primary_source.as_ref() {
                    debug!("[Wayland] This is primary source");
                    state.pending_primary_content.blocking_lock().clone()
                } else {
                    warn!(
                        "[Wayland] Unknown source {:?}, cannot determine content. Current clipboard: {:?}, Primary: {:?}",
                        source, state.clipboard_source, state.primary_source
                    );
                    return;
                };

                if let Some(data) = content {
                    match data {
                        ClipboardContent::Text(text) => {
                            debug!("[Wayland] Writing text: {} chars", text.len());
                            use nix::unistd::write;
                            match write(&fd, text.as_bytes()) {
                                Ok(bytes_written) => {
                                    debug!("[Wayland] Successfully wrote {} bytes", bytes_written);
                                }
                                Err(e) => {
                                    error!("[Wayland] Failed to write text: {}", e);
                                }
                            }
                        }
                        ClipboardContent::Binary(mime_map) => {
                            let mime_str = mime_type.to_string();
                            if let Some(binary_data) = mime_map.get(&mime_str) {
                                debug!("[Wayland] Writing {} bytes for MIME: {}", binary_data.len(), mime_str);
                                use nix::unistd::write;
                                match write(&fd, binary_data) {
                                    Ok(bytes_written) => {
                                        debug!("[Wayland] Successfully wrote {} bytes", bytes_written);
                                    }
                                    Err(e) => {
                                        error!("[Wayland] Failed to write binary: {}", e);
                                    }
                                }
                            } else {
                                warn!("[Wayland] No data for MIME type: {}", mime_str);
                            }
                        }
                        ClipboardContent::Empty => {
                            warn!("[Wayland] Empty content, nothing to send");
                        }
                    }
                } else {
                    warn!("[Wayland] No content available to send");
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                debug!("[Wayland] Data source cancelled");
                source.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpPrimarySelectionDeviceManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpPrimarySelectionDeviceManagerV1,
        _event: <ZwpPrimarySelectionDeviceManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpPrimarySelectionOfferV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _offer: &ZwpPrimarySelectionOfferV1,
        event: zwp_primary_selection_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwp_primary_selection_offer_v1::Event::Offer { mime_type } = event {
            debug!("[Wayland] Primary offer mime type: {}", mime_type);
        }
    }
}

impl Dispatch<ZwpPrimarySelectionSourceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _source: &ZwpPrimarySelectionSourceV1,
        event: zwp_primary_selection_source_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        debug!("[Wayland] Primary source event received: {:?}", event);
        match event {
            zwp_primary_selection_source_v1::Event::Send { mime_type, fd } => {
                info!(
                    "[Wayland] Primary send data for mime type: {}",
                    mime_type
                );

                let content = state.pending_primary_content.blocking_lock().clone();

                if let Some(data) = content {
                    match data {
                        ClipboardContent::Text(text) => {
                            debug!("[Wayland] Writing primary text: {} chars", text.len());
                            use nix::unistd::write;
                            match write(&fd, text.as_bytes()) {
                                Ok(bytes_written) => {
                                    debug!("[Wayland] Successfully wrote {} bytes to primary", bytes_written);
                                }
                                Err(e) => {
                                    error!("[Wayland] Failed to write primary text: {}", e);
                                }
                            }
                        }
                        ClipboardContent::Binary(mime_map) => {
                            let mime_str = mime_type.to_string();
                            if let Some(binary_data) = mime_map.get(&mime_str) {
                                debug!("[Wayland] Writing {} bytes for primary MIME: {}", binary_data.len(), mime_str);
                                use nix::unistd::write;
                                match write(&fd, binary_data) {
                                    Ok(bytes_written) => {
                                        debug!("[Wayland] Successfully wrote {} bytes to primary", bytes_written);
                                    }
                                    Err(e) => {
                                        error!("[Wayland] Failed to write primary binary: {}", e);
                                    }
                                }
                            } else {
                                warn!("[Wayland] No data for primary MIME type: {}", mime_str);
                            }
                        }
                        ClipboardContent::Empty => {
                            warn!("[Wayland] Empty primary content");
                        }
                    }
                } else {
                    warn!("[Wayland] No primary content available to send");
                }
            }
            zwp_primary_selection_source_v1::Event::Cancelled => {
                debug!("[Wayland] Primary data source cancelled");
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlManagerV1,
        _event: wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}
