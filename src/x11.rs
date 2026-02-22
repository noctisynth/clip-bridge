// ============================================================================
// X11 State
// ============================================================================

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

use x11rb::connection::Connection as X11Connection;
use x11rb::protocol::xfixes::{ConnectionExt as XFixesConnectionExt, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt, CreateWindowAux, EventMask, PropertyNotifyEvent,
    SelectionClearEvent, SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
    SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;

use crate::{
    ClipboardContent, ClipboardType, SyncEvent, CLIPBOARD_ATOM, CURRENT_TIME, INCR_ATOM,
    MULTIPLE_ATOM, PRIMARY_ATOM, STRING_ATOM, TARGETS_ATOM, TEXT_ATOM, TEXT_PLAIN_ATOM,
    TEXT_PLAIN_UTF8_ATOM, UTF8_STRING_ATOM,
};

pub struct X11State {
    conn: x11rb::rust_connection::RustConnection,
    _screen_num: usize,
    atoms: HashMap<String, Atom>,
    pub window: Window,
    sync_tx: mpsc::UnboundedSender<SyncEvent>,
    clipboard_content: Arc<Mutex<Option<String>>>,
    primary_content: Arc<Mutex<Option<String>>>,
    set_clipboard_rx: mpsc::UnboundedReceiver<(String, ClipboardType)>,
}

impl X11State {
    pub fn new(
        conn: x11rb::rust_connection::RustConnection,
        screen_num: usize,
        sync_tx: mpsc::UnboundedSender<SyncEvent>,
        set_clipboard_rx: mpsc::UnboundedReceiver<(String, ClipboardType)>,
    ) -> Result<Self, String> {
        let screen = &conn.setup().roots[screen_num];
        let window = conn
            .generate_id()
            .map_err(|e| format!("Failed to generate window ID: {}", e))?;

        info!("[X11] Creating window: {}", window);

        // Create a window
        conn.create_window(
            screen.root_depth,
            window,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::COPY_FROM_PARENT,
            screen.root_visual,
            &CreateWindowAux::new()
                .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
        )
        .map_err(|e| format!("Failed to create window: {}", e))?;
        conn.flush()
            .map_err(|e| format!("Failed to flush connection: {}", e))?;

        // Initialize XFixes extension
        let xfixes_query = conn
            .xfixes_query_version(5, 0)
            .map_err(|e| format!("Failed to query XFixes version: {}", e))?;
        let xfixes_reply = xfixes_query
            .reply()
            .map_err(|e| format!("Failed to get XFixes version reply: {}", e))?;
        info!(
            "[X11] XFixes version: {}.{}",
            xfixes_reply.major_version, xfixes_reply.minor_version
        );

        // Intern atoms
        let mut atoms = HashMap::new();
        let atom_names = vec![
            CLIPBOARD_ATOM,
            PRIMARY_ATOM,
            TARGETS_ATOM,
            MULTIPLE_ATOM,
            INCR_ATOM,
            UTF8_STRING_ATOM,
            TEXT_ATOM,
            STRING_ATOM,
            TEXT_PLAIN_UTF8_ATOM,
            TEXT_PLAIN_ATOM,
        ];

        for name in &atom_names {
            let atom = conn
                .intern_atom(false, name.as_bytes())
                .map_err(|e| format!("Failed to intern atom {}: {}", name, e))?;
            let reply = atom
                .reply()
                .map_err(|e| format!("Failed to get atom reply for {}: {}", name, e))?;
            atoms.insert(name.to_string(), reply.atom);
            debug!("[X11] Interned atom: {} = {}", name, reply.atom);
        }

        // Set up XFixes selection event mask for CLIPBOARD
        if let Some(clipboard_atom) = atoms.get(CLIPBOARD_ATOM) {
            conn.xfixes_select_selection_input(
                window,
                *clipboard_atom,
                SelectionEventMask::SET_SELECTION_OWNER
                    | SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | SelectionEventMask::SELECTION_CLIENT_CLOSE,
            )
            .map_err(|e| format!("Failed to select XFixes clipboard input: {}", e))?;
            info!("[X11] XFixes selection monitoring enabled for CLIPBOARD");
        }

        // Set up XFixes selection event mask for PRIMARY
        conn.xfixes_select_selection_input(
            window,
            AtomEnum::PRIMARY.into(),
            SelectionEventMask::SET_SELECTION_OWNER
                | SelectionEventMask::SELECTION_WINDOW_DESTROY
                | SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )
        .map_err(|e| format!("Failed to select XFixes primary input: {}", e))?;
        info!("[X11] XFixes selection monitoring enabled for PRIMARY");

        conn.flush()
            .map_err(|e| format!("Failed to flush connection: {}", e))?;

        Ok(Self {
            conn,
            _screen_num: screen_num,
            atoms,
            window,
            sync_tx,
            clipboard_content: Arc::new(Mutex::new(None)),
            primary_content: Arc::new(Mutex::new(None)),
            set_clipboard_rx,
        })
    }

    pub fn get_atom(&self, name: &str) -> Option<Atom> {
        self.atoms.get(name).copied()
    }

    pub fn set_clipboard_content(
        &self,
        content: String,
        clipboard_type: ClipboardType,
    ) -> Result<(), String> {
        info!(
            "[X11] Setting clipboard content: type={:?}, len={}",
            clipboard_type,
            content.len()
        );

        let selection_atom = match clipboard_type {
            ClipboardType::Clipboard => self.get_atom(CLIPBOARD_ATOM).unwrap(),
            ClipboardType::Primary => AtomEnum::PRIMARY.into(),
        };

        // Store content
        let utf8_string = self.get_atom(UTF8_STRING_ATOM).unwrap();
        let content_bytes = content.as_bytes();

        // Set property on our window
        self.conn
            .change_property8(
                x11rb::protocol::xproto::PropMode::REPLACE,
                self.window,
                utf8_string,
                utf8_string,
                content_bytes,
            )
            .map_err(|e| format!("Failed to change property: {}", e))?;
        self.conn
            .flush()
            .map_err(|e| format!("Failed to flush connection: {}", e))?;

        // Claim selection ownership
        let timestamp = CURRENT_TIME;
        self.conn
            .set_selection_owner(self.window, selection_atom, timestamp)
            .map_err(|e| format!("Failed to set selection owner: {}", e))?;
        self.conn
            .flush()
            .map_err(|e| format!("Failed to flush connection: {}", e))?;

        match clipboard_type {
            ClipboardType::Clipboard => {
                *self.clipboard_content.blocking_lock() = Some(content.clone());
            }
            ClipboardType::Primary => {
                *self.primary_content.blocking_lock() = Some(content.clone());
            }
        }

        info!("[X11] Clipboard content set successfully");
        Ok(())
    }

    pub fn request_clipboard_content(&self, clipboard_type: ClipboardType) -> Result<(), String> {
        debug!("[X11] Requesting clipboard content: {:?}", clipboard_type);

        let selection_atom = match clipboard_type {
            ClipboardType::Clipboard => self.get_atom(CLIPBOARD_ATOM).unwrap(),
            ClipboardType::Primary => AtomEnum::PRIMARY.into(),
        };

        let utf8_string = self.get_atom(UTF8_STRING_ATOM).unwrap();
        let text_plain = self.get_atom(TEXT_PLAIN_ATOM).unwrap();
        let string_atom = self.get_atom(STRING_ATOM).unwrap();

        // First check if we own the selection
        let owner = self
            .conn
            .get_selection_owner(selection_atom)
            .map_err(|e| format!("Failed to get selection owner: {}", e))?
            .reply()
            .map_err(|e| format!("Failed to get selection owner reply: {}", e))?;

        if owner.owner == self.window {
            debug!("[X11] We own the selection, using cached content");
            // We own it, use our cached content
            return Ok(());
        }

        if owner.owner == 0 {
            debug!("[X11] No selection owner");
            return Ok(());
        }

        debug!("[X11] Requesting selection from owner: {}", owner.owner);

        // Try multiple targets in order of preference
        let targets = [utf8_string, text_plain, string_atom];
        for (i, target) in targets.iter().enumerate() {
            let property = match self.get_atom(&format!("CLIP_TEMP_{}", i)) {
                Some(atom) => atom,
                None => {
                    // Create a temporary atom if needed
                    let atom = self
                        .conn
                        .intern_atom(false, format!("CLIP_TEMP_{}", i).as_bytes())
                        .unwrap();
                    atom.reply().unwrap().atom
                }
            };

            debug!("[X11] Trying target {} with property {}", target, property);

            // Request selection
            self.conn
                .convert_selection(self.window, selection_atom, *target, property, CURRENT_TIME)
                .map_err(|e| format!("Failed to convert selection: {}", e))?;
            self.conn
                .flush()
                .map_err(|e| format!("Failed to flush connection: {}", e))?;

            // Wait longer for response - some apps take time to respond
            for _ in 0..10 {
                std::thread::sleep(Duration::from_millis(20));

                // Check if we got a response
                match self.conn.poll_for_event() {
                    Ok(Some(Event::SelectionNotify(notify))) => {
                        if notify.property != AtomEnum::NONE.into() {
                            debug!("[X11] Got selection notify for target {}", target);
                            // Read the property content
                            let prop = self
                                .conn
                                .get_property::<u32, u32>(
                                    false,
                                    self.window,
                                    notify.property,
                                    AtomEnum::ANY.into(),
                                    0,
                                    u32::MAX,
                                )
                                .map_err(|e| format!("Failed to get property: {}", e))?
                                .reply()
                                .map_err(|e| format!("Failed to get property reply: {}", e))?;

                            debug!(
                                "[X11] Property read: type={}, format={}, bytes={}",
                                prop.type_,
                                prop.format,
                                prop.value.len()
                            );

                            // Check if property is empty or invalid
                            if prop.type_ == 0 || prop.value.is_empty() {
                                warn!("[X11] Property is empty or invalid");
                                self.conn
                                    .delete_property(self.window, notify.property)
                                    .map_err(|e| format!("Failed to delete property: {}", e))?;
                                self.conn
                                    .flush()
                                    .map_err(|e| format!("Failed to flush connection: {}", e))?;
                                break;
                            }

                            // Try to decode based on property type
                            let content = if prop.type_ == utf8_string || prop.type_ == text_plain {
                                String::from_utf8(prop.value.clone())
                                    .map_err(|e| format!("Failed to convert to UTF-8: {}", e))?
                            } else if prop.type_ == string_atom {
                                // STRING is typically Latin-1
                                prop.value.iter().map(|&b| b as char).collect::<String>()
                            } else {
                                warn!(
                                    "[X11] Unsupported property type: {} (expected UTF8_STRING={}, STRING={}, TEXT_PLAIN={})",
                                    prop.type_, utf8_string, string_atom, text_plain
                                );
                                self.conn
                                    .delete_property(self.window, notify.property)
                                    .map_err(|e| format!("Failed to delete property: {}", e))?;
                                self.conn
                                    .flush()
                                    .map_err(|e| format!("Failed to flush connection: {}", e))?;
                                break;
                            };

                            info!(
                                "[X11] Received clipboard content: type={:?}, len={}",
                                clipboard_type,
                                content.len()
                            );

                            match clipboard_type {
                                ClipboardType::Clipboard => {
                                    *self.clipboard_content.blocking_lock() = Some(content.clone());
                                }
                                ClipboardType::Primary => {
                                    *self.primary_content.blocking_lock() = Some(content.clone());
                                }
                            }

                            // Send sync event
                            debug!(
                                "[X11] Sending sync event to Wayland: type={:?}, len={}",
                                clipboard_type,
                                content.len()
                            );
                            match self.sync_tx.send(SyncEvent::X11ToWayland {
                                content: ClipboardContent::Text(content),
                                clipboard_type,
                            }) {
                                Ok(_) => debug!("[X11] Sync event sent successfully"),
                                Err(e) => error!("[X11] Failed to send sync event: {}", e),
                            }

                            // Delete property
                            self.conn
                                .delete_property(self.window, notify.property)
                                .map_err(|e| format!("Failed to delete property: {}", e))?;
                            self.conn
                                .flush()
                                .map_err(|e| format!("Failed to flush connection: {}", e))?;

                            // Success, don't try other targets
                            return Ok(());
                        } else {
                            debug!(
                                "[X11] Selection notify with NONE property for target {}",
                                target
                            );
                            break;
                        }
                    }
                    Ok(Some(Event::PropertyNotify(_))) => {
                        // Property changed, might be our data
                        continue;
                    }
                    Ok(Some(_)) => {
                        // Other event, continue waiting
                    }
                    Ok(None) => {
                        // No event yet, continue waiting
                    }
                    Err(e) => {
                        debug!("[X11] Poll error: {}", e);
                    }
                }
            }

            debug!("[X11] No valid response for target {}", target);
        }

        Ok(())
    }

    pub fn handle_selection_request(&self, event: SelectionRequestEvent) -> Result<(), String> {
        debug!("[X11] Selection request: {:?}", event);

        let utf8_string = self.get_atom(UTF8_STRING_ATOM).unwrap();
        let targets = self.get_atom(TARGETS_ATOM).unwrap();
        let multiple = self.get_atom(MULTIPLE_ATOM).unwrap();

        let target = event.target;
        let mut property = event.property;

        // Handle TARGETS request
        if target == targets {
            debug!("[X11] Handling TARGETS request");
            let target_atoms = vec![
                utf8_string,
                self.get_atom(STRING_ATOM).unwrap(),
                self.get_atom(TEXT_ATOM).unwrap(),
                targets,
            ];
            self.conn
                .change_property32(
                    x11rb::protocol::xproto::PropMode::REPLACE,
                    event.requestor,
                    property,
                    AtomEnum::ATOM,
                    &target_atoms,
                )
                .map_err(|e| format!("Failed to change property32: {}", e))?;
        }
        // Handle MULTIPLE request
        else if target == multiple {
            debug!("[X11] Handling MULTIPLE request");
            // Read the property and handle each atom pair
            let prop = self
                .conn
                .get_property(false, event.requestor, property, AtomEnum::ATOM, 0, 1024)
                .map_err(|e| format!("Failed to get property: {}", e))?
                .reply()
                .map_err(|e| format!("Failed to get property reply: {}", e))?;

            let atoms = prop.value32().into_iter().flatten().collect::<Vec<_>>();
            for chunk in atoms.chunks(2) {
                if chunk.len() == 2 {
                    // Handle each pair (target, property)
                    // For simplicity, we just set the property to empty
                    self.conn
                        .change_property8(
                            x11rb::protocol::xproto::PropMode::REPLACE,
                            event.requestor,
                            chunk[1],
                            AtomEnum::STRING,
                            &[],
                        )
                        .map_err(|e| format!("Failed to change property8: {}", e))?;
                }
            }
        }
        // Handle text requests
        else if target == utf8_string
            || target == self.get_atom(STRING_ATOM).unwrap()
            || target == self.get_atom(TEXT_ATOM).unwrap()
        {
            debug!("[X11] Handling text request for target: {}", target);
            let content = match event.selection {
                s if s == self.get_atom(CLIPBOARD_ATOM).unwrap() => {
                    self.clipboard_content.blocking_lock().clone()
                }
                s if s == AtomEnum::PRIMARY.into() => self.primary_content.blocking_lock().clone(),
                _ => None,
            };

            if let Some(text) = content {
                debug!("[X11] Sending text content: {} chars", text.len());
                self.conn
                    .change_property8(
                        x11rb::protocol::xproto::PropMode::REPLACE,
                        event.requestor,
                        property,
                        utf8_string,
                        text.as_bytes(),
                    )
                    .map_err(|e| format!("Failed to change property8: {}", e))?;
            } else {
                warn!("[X11] No content available for request");
                property = AtomEnum::NONE.into();
            }
        } else {
            debug!("[X11] Unsupported target: {}", target);
            property = AtomEnum::NONE.into();
        }

        // Send notification
        self.conn
            .send_event(
                false,
                event.requestor,
                EventMask::NO_EVENT,
                SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: event.time,
                    requestor: event.requestor,
                    selection: event.selection,
                    target: event.target,
                    property,
                },
            )
            .map_err(|e| format!("Failed to send event: {}", e))?;
        self.conn
            .flush()
            .map_err(|e| format!("Failed to flush connection: {}", e))?;

        Ok(())
    }

    pub fn handle_selection_notify(&self, event: SelectionNotifyEvent) -> Result<(), String> {
        debug!("[X11] Selection notify: {:?}", event);

        if event.property == AtomEnum::NONE.into() {
            // Selection request failed
            warn!("[X11] Selection request failed (property is NONE)");
            return Ok(());
        }

        let utf8_string = self.get_atom(UTF8_STRING_ATOM).unwrap();
        let string_atom = self.get_atom(STRING_ATOM).unwrap();
        let text_plain = self.get_atom(TEXT_PLAIN_ATOM).unwrap();

        // Read the property - try different types
        let prop = self
            .conn
            .get_property::<u32, u32>(
                false,
                self.window,
                event.property,
                AtomEnum::ANY.into(),
                0,
                u32::MAX,
            )
            .map_err(|e| format!("Failed to get property: {}", e))?
            .reply()
            .map_err(|e| format!("Failed to get property reply: {}", e))?;

        debug!(
            "[X11] Property read: type={}, format={}, bytes={}",
            prop.type_,
            prop.format,
            prop.value.len()
        );

        // Check if property is empty or invalid
        if prop.type_ == 0 || prop.value.is_empty() {
            warn!("[X11] Property is empty or invalid");
            // Delete the property and return
            self.conn
                .delete_property(self.window, event.property)
                .map_err(|e| format!("Failed to delete property: {}", e))?;
            self.conn
                .flush()
                .map_err(|e| format!("Failed to flush connection: {}", e))?;
            return Ok(());
        }

        // Try to decode based on property type
        let content = if prop.type_ == utf8_string || prop.type_ == text_plain {
            String::from_utf8(prop.value.clone())
                .map_err(|e| format!("Failed to convert to UTF-8: {}", e))?
        } else if prop.type_ == string_atom {
            // STRING is typically Latin-1
            prop.value.iter().map(|&b| b as char).collect::<String>()
        } else {
            warn!(
                "[X11] Unsupported property type: {} (expected UTF8_STRING={}, STRING={}, TEXT_PLAIN={})",
                prop.type_, utf8_string, string_atom, text_plain
            );
            // Delete the property and return
            self.conn
                .delete_property(self.window, event.property)
                .map_err(|e| format!("Failed to delete property: {}", e))?;
            self.conn
                .flush()
                .map_err(|e| format!("Failed to flush connection: {}", e))?;
            return Ok(());
        };

        let clipboard_type = if event.selection == self.get_atom(CLIPBOARD_ATOM).unwrap() {
            ClipboardType::Clipboard
        } else {
            ClipboardType::Primary
        };

        info!(
            "[X11] Received clipboard content: type={:?}, len={}",
            clipboard_type,
            content.len()
        );

        match clipboard_type {
            ClipboardType::Clipboard => {
                *self.clipboard_content.blocking_lock() = Some(content.clone());
            }
            ClipboardType::Primary => {
                *self.primary_content.blocking_lock() = Some(content.clone());
            }
        }

        // Send sync event
        let _ = self.sync_tx.send(SyncEvent::X11ToWayland {
            content: ClipboardContent::Text(content),
            clipboard_type,
        });

        // Delete the property
        self.conn
            .delete_property(self.window, event.property)
            .map_err(|e| format!("Failed to delete property: {}", e))?;
        self.conn
            .flush()
            .map_err(|e| format!("Failed to flush connection: {}", e))?;

        Ok(())
    }

    pub fn handle_selection_clear(&self, event: SelectionClearEvent) -> Result<(), String> {
        debug!("[X11] Selection clear: {:?}", event);

        let clipboard_type = if event.selection == self.get_atom(CLIPBOARD_ATOM).unwrap() {
            ClipboardType::Clipboard
        } else {
            ClipboardType::Primary
        };

        info!("[X11] Lost ownership of selection: {:?}", clipboard_type);

        match clipboard_type {
            ClipboardType::Clipboard => {
                *self.clipboard_content.blocking_lock() = None;
            }
            ClipboardType::Primary => {
                *self.primary_content.blocking_lock() = None;
            }
        }

        Ok(())
    }

    pub fn handle_property_notify(&self, event: PropertyNotifyEvent) -> Result<(), String> {
        debug!(
            "[X11] Property notify: atom={}, state={:?}",
            event.atom, event.state
        );
        Ok(())
    }

    pub fn run_event_loop(&mut self) -> Result<(), String> {
        info!("[X11] Starting event loop");

        loop {
            // Check for set clipboard requests
            if let Ok((content, clipboard_type)) = self.set_clipboard_rx.try_recv() {
                let _ = self.set_clipboard_content(content, clipboard_type);
            }

            // Process X11 events
            match self.conn.poll_for_event() {
                Ok(Some(event)) => match event {
                    Event::SelectionRequest(e) => self.handle_selection_request(e)?,
                    Event::SelectionNotify(e) => self.handle_selection_notify(e)?,
                    Event::SelectionClear(e) => self.handle_selection_clear(e)?,
                    Event::PropertyNotify(e) => self.handle_property_notify(e)?,
                    Event::XfixesSelectionNotify(e) => self.handle_xfixes_selection_notify(e)?,
                    _ => {
                        debug!("[X11] Unhandled event: {:?}", event);
                    }
                },
                Ok(None) => {
                    // No events, continue
                }
                Err(e) => {
                    debug!("[X11] Poll error: {}", e);
                }
            }

            // Flush any pending requests
            let _ = self.conn.flush();

            // Sleep to avoid busy waiting
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn handle_xfixes_selection_notify(
        &self,
        event: x11rb::protocol::xfixes::SelectionNotifyEvent,
    ) -> Result<(), String> {
        debug!("[X11] XFixes selection notify: {:?}", event);

        let clipboard_type = if event.selection == self.get_atom(CLIPBOARD_ATOM).unwrap() {
            ClipboardType::Clipboard
        } else {
            ClipboardType::Primary
        };

        // Check if we own the selection
        if event.owner == self.window {
            debug!("[X11] We own the selection, ignoring");
            return Ok(());
        }

        // If there's a new owner (not none), request content
        if event.owner != 0 {
            info!(
                "[X11] Selection changed via XFixes: type={:?}, owner={}",
                clipboard_type, event.owner
            );
            let _ = self.request_clipboard_content(clipboard_type);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;

    #[test]
    fn test_x11_state_initialization() {
        let (conn, screen_num) = x11rb::connect(None).unwrap();
        let (sync_tx, _sync_rx) = unbounded_channel();
        let (_set_clipboard_tx, set_clipboard_rx) = unbounded_channel();

        let x11_state = X11State::new(conn, screen_num, sync_tx, set_clipboard_rx);
        assert!(x11_state.is_ok(), "Failed to initialize X11State");
    }

    #[test]
    fn test_atom_interning() {
        let (conn, screen_num) = x11rb::connect(None).unwrap();
        let (sync_tx, _sync_rx) = unbounded_channel();
        let (_set_clipboard_tx, set_clipboard_rx) = unbounded_channel();

        let x11_state = X11State::new(conn, screen_num, sync_tx, set_clipboard_rx).unwrap();

        // Test that all required atoms are interned
        let required_atoms = vec![
            CLIPBOARD_ATOM,
            PRIMARY_ATOM,
            TARGETS_ATOM,
            MULTIPLE_ATOM,
            INCR_ATOM,
            UTF8_STRING_ATOM,
            TEXT_ATOM,
            STRING_ATOM,
            TEXT_PLAIN_UTF8_ATOM,
            TEXT_PLAIN_ATOM,
        ];

        for atom_name in required_atoms {
            assert!(
                x11_state.get_atom(atom_name).is_some(),
                "Atom {} not interned",
                atom_name
            );
        }
    }
}
