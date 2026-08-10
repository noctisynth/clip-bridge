mod atoms;
mod receive;
mod serve;

use std::{
    collections::HashMap,
    os::fd::AsFd,
    time::{Duration, Instant},
};

use nix::poll::{PollFd, PollFlags, poll};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info};
use x11rb::{
    CURRENT_TIME,
    connection::{Connection, RequestConnection},
    protocol::{
        Event,
        xfixes::{ConnectionExt as _, SelectionEventMask},
        xproto::{
            Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux,
            EventMask, PropMode, Property, PropertyNotifyEvent, SELECTION_NOTIFY_EVENT,
            SelectionClearEvent, SelectionNotifyEvent, SelectionRequestEvent, Timestamp, Window,
            WindowClass,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

use crate::{
    backend::{BackendCommand, BackendEvent},
    domain::{
        BackendCapabilities, BackendEpoch, BackendId, CommandId, OfferToken, ProtocolError,
        Revision, SelectionKind, SnapshotOutcome, TextPayload, TransferError, UnavailableReason,
    },
};

use self::{
    atoms::Atoms,
    receive::{ChunkAssembler, TextTarget, choose_target, decode},
    serve::{chunk_size, encode_target, request_property},
};

const ACTOR_POLL_TIMEOUT_MS: u16 = 100;
const IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTGOING_TRANSFERS: usize = 8;

pub(crate) fn run(
    event_tx: mpsc::Sender<BackendEvent>,
    clipboard_commands: watch::Receiver<Option<BackendCommand>>,
    primary_commands: watch::Receiver<Option<BackendCommand>>,
) -> Result<(), ProtocolError> {
    let fatal_tx = event_tx.clone();
    let result = run_actor(event_tx, clipboard_commands, primary_commands);
    if let Err(error) = &result
        && fatal_tx
            .blocking_send(BackendEvent::FatalError {
                backend: BackendId::X11,
                error: error.clone(),
            })
            .is_err()
    {
        debug!("coordinator unavailable while reporting fatal X11 error");
    }
    result
}

fn run_actor(
    event_tx: mpsc::Sender<BackendEvent>,
    clipboard_commands: watch::Receiver<Option<BackendCommand>>,
    primary_commands: watch::Receiver<Option<BackendCommand>>,
) -> Result<(), ProtocolError> {
    let (connection, screen_number) =
        x11rb::connect(None).map_err(|error| ProtocolError::operation("x11-connect", error))?;
    let mut actor = Actor::new(
        connection,
        screen_number,
        event_tx,
        clipboard_commands,
        primary_commands,
    )?;
    actor.initialize_snapshots()?;
    actor.run()
}

struct Actor {
    connection: RustConnection,
    window: Window,
    atoms: Atoms,
    event_tx: mpsc::Sender<BackendEvent>,
    clipboard_commands: watch::Receiver<Option<BackendCommand>>,
    primary_commands: watch::Receiver<Option<BackendCommand>>,
    clipboard: SelectionSlot,
    primary: SelectionSlot,
    outgoing: HashMap<(Window, Atom), OutgoingTransfer>,
    transfer_chunk_size: usize,
    last_server_time: Timestamp,
}

impl Actor {
    fn new(
        connection: RustConnection,
        screen_number: usize,
        event_tx: mpsc::Sender<BackendEvent>,
        clipboard_commands: watch::Receiver<Option<BackendCommand>>,
        primary_commands: watch::Receiver<Option<BackendCommand>>,
    ) -> Result<Self, ProtocolError> {
        let screen = connection.setup().roots.get(screen_number).ok_or_else(|| {
            ProtocolError::invalid_state("x11-screen", "connection returned no selected screen")
        })?;
        let window = connection
            .generate_id()
            .map_err(|error| ProtocolError::operation("x11-generate-window-id", error))?;
        connection
            .create_window(
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
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .map_err(|error| ProtocolError::operation("x11-create-owner-window", error))?
            .check()
            .map_err(|error| ProtocolError::operation("x11-create-owner-window-reply", error))?;

        connection
            .xfixes_query_version(5, 0)
            .map_err(|error| ProtocolError::operation("x11-query-xfixes", error))?
            .reply()
            .map_err(|error| ProtocolError::operation("x11-query-xfixes-reply", error))?;
        let atoms = Atoms::intern(&connection)?;
        for selection in [SelectionKind::Clipboard, SelectionKind::Primary] {
            connection
                .xfixes_select_selection_input(
                    window,
                    atoms.selection(selection),
                    SelectionEventMask::SET_SELECTION_OWNER
                        | SelectionEventMask::SELECTION_WINDOW_DESTROY
                        | SelectionEventMask::SELECTION_CLIENT_CLOSE,
                )
                .map_err(|error| ProtocolError::operation("x11-select-selection-input", error))?
                .check()
                .map_err(|error| {
                    ProtocolError::operation("x11-select-selection-input-reply", error)
                })?;
        }
        connection
            .flush()
            .map_err(|error| ProtocolError::operation("x11-initial-flush", error))?;

        let transfer_chunk_size = chunk_size(connection.maximum_request_bytes());
        info!(window, transfer_chunk_size, "X11 actor initialized");
        Ok(Self {
            connection,
            window,
            atoms,
            event_tx,
            clipboard_commands,
            primary_commands,
            clipboard: SelectionSlot::default(),
            primary: SelectionSlot::default(),
            outgoing: HashMap::new(),
            transfer_chunk_size,
            last_server_time: CURRENT_TIME,
        })
    }

    fn initialize_snapshots(&mut self) -> Result<(), ProtocolError> {
        self.emit(BackendEvent::Ready {
            backend: BackendId::X11,
            capabilities: BackendCapabilities::text_bridge(),
        })?;
        for selection in [SelectionKind::Clipboard, SelectionKind::Primary] {
            let epoch = self.next_epoch(selection)?;
            let token = self.next_token(selection)?;
            let owner = self
                .connection
                .get_selection_owner(self.atoms.selection(selection))
                .map_err(|error| ProtocolError::operation("x11-get-initial-owner", error))?
                .reply()
                .map_err(|error| ProtocolError::operation("x11-get-initial-owner-reply", error))?
                .owner;
            self.slot_mut(selection).owner = owner;
            if owner == x11rb::NONE {
                self.emit(BackendEvent::InitialSnapshot {
                    backend: BackendId::X11,
                    selection,
                    epoch,
                    token,
                    outcome: SnapshotOutcome::Empty,
                })?;
            } else {
                self.begin_receive(selection, epoch, token, true)?;
            }
        }
        self.connection
            .flush()
            .map_err(|error| ProtocolError::operation("x11-snapshot-flush", error))
    }

    fn run(&mut self) -> Result<(), ProtocolError> {
        loop {
            if !self.process_commands()? {
                return Ok(());
            }

            while let Some(event) = self
                .connection
                .poll_for_event()
                .map_err(ProtocolError::disconnected)?
            {
                self.handle_event(event)?;
            }
            self.expire_transfers()?;
            self.connection
                .flush()
                .map_err(ProtocolError::disconnected)?;

            let mut poll_fds = [PollFd::new(
                self.connection.stream().as_fd(),
                PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP,
            )];
            poll(&mut poll_fds, ACTOR_POLL_TIMEOUT_MS)
                .map_err(|error| ProtocolError::operation("x11-poll", error))?;
            if poll_fds[0]
                .revents()
                .is_some_and(|events| events.intersects(PollFlags::POLLERR | PollFlags::POLLHUP))
            {
                return Err(ProtocolError::disconnected("X11 socket closed"));
            }
        }
    }

    fn process_commands(&mut self) -> Result<bool, ProtocolError> {
        let clipboard = take_command(&mut self.clipboard_commands, SelectionKind::Clipboard)?;
        let primary = take_command(&mut self.primary_commands, SelectionKind::Primary)?;
        for command in [clipboard, primary].into_iter().flatten() {
            match command {
                BackendCommand::SetText {
                    command_id,
                    selection,
                    revision,
                    expected_target_epoch,
                    payload,
                } => self.set_text(
                    command_id,
                    selection,
                    revision,
                    expected_target_epoch,
                    payload,
                )?,
                BackendCommand::Shutdown => return Ok(false),
            }
        }
        Ok(true)
    }

    fn set_text(
        &mut self,
        command_id: CommandId,
        selection: SelectionKind,
        revision: Revision,
        expected_target_epoch: BackendEpoch,
        payload: TextPayload,
    ) -> Result<(), ProtocolError> {
        let current_epoch = {
            let slot = self.slot_mut(selection);
            if command_id <= slot.last_command_id {
                return Ok(());
            }
            slot.last_command_id = command_id;
            slot.epoch
        };
        if expected_target_epoch != current_epoch {
            return self.emit(BackendEvent::OwnershipFailed {
                backend: BackendId::X11,
                selection,
                command_id,
                revision,
                error: ProtocolError::invalid_state(
                    "x11-set-selection",
                    format!(
                        "stale target epoch {}; current epoch is {}",
                        expected_target_epoch.value(),
                        current_epoch.value()
                    ),
                ),
            });
        }

        let observed_owner = self
            .connection
            .get_selection_owner(self.atoms.selection(selection))
            .map_err(|error| ProtocolError::operation("x11-check-target-owner", error))?
            .reply()
            .map_err(|error| ProtocolError::operation("x11-check-target-owner-reply", error))?
            .owner;
        if observed_owner != self.slot(selection).owner {
            return self.emit(BackendEvent::OwnershipFailed {
                backend: BackendId::X11,
                selection,
                command_id,
                revision,
                error: ProtocolError::invalid_state(
                    "x11-set-selection",
                    "selection owner changed before queued command could execute",
                ),
            });
        }

        let timestamp = self.last_server_time;
        self.connection
            .set_selection_owner(self.window, self.atoms.selection(selection), timestamp)
            .map_err(|error| ProtocolError::operation("x11-set-selection-owner", error))?
            .check()
            .map_err(|error| ProtocolError::operation("x11-set-selection-owner-reply", error))?;
        self.connection
            .flush()
            .map_err(|error| ProtocolError::operation("x11-set-selection-flush", error))?;
        let owner = self
            .connection
            .get_selection_owner(self.atoms.selection(selection))
            .map_err(|error| ProtocolError::operation("x11-confirm-selection-owner", error))?
            .reply()
            .map_err(|error| ProtocolError::operation("x11-confirm-selection-owner-reply", error))?
            .owner;
        if owner != self.window {
            return self.emit(BackendEvent::OwnershipFailed {
                backend: BackendId::X11,
                selection,
                command_id,
                revision,
                error: ProtocolError::invalid_state(
                    "x11-confirm-selection-owner",
                    "X server did not assign ownership to bridge window",
                ),
            });
        }

        self.slot_mut(selection).owned = Some(OwnedSelection {
            revision,
            payload,
            timestamp,
        });
        self.slot_mut(selection).owner = self.window;
        self.emit(BackendEvent::OwnershipApplied {
            backend: BackendId::X11,
            selection,
            command_id,
            revision,
        })
    }

    fn handle_event(&mut self, event: Event) -> Result<(), ProtocolError> {
        match event {
            Event::XfixesSelectionNotify(event) => self.selection_changed(event),
            Event::SelectionNotify(event) => self.selection_notify(event),
            Event::PropertyNotify(event) => self.property_notify(event),
            Event::SelectionRequest(event) => self.selection_request(event),
            Event::SelectionClear(event) => self.selection_clear(event),
            other => {
                debug!(event = ?other, "ignoring unrelated X11 event");
                Ok(())
            }
        }
    }

    fn selection_changed(
        &mut self,
        event: x11rb::protocol::xfixes::SelectionNotifyEvent,
    ) -> Result<(), ProtocolError> {
        let Some(selection) = self.atoms.selection_kind(event.selection) else {
            return Ok(());
        };
        self.last_server_time = event.timestamp;
        if event.owner == self.window {
            self.slot_mut(selection).owner = self.window;
            return Ok(());
        }

        let lost_revision = self
            .slot_mut(selection)
            .owned
            .take()
            .map(|owned| owned.revision);
        if let Some(revision) = lost_revision {
            self.emit(BackendEvent::OwnershipLost {
                backend: BackendId::X11,
                selection,
                revision,
            })?;
        }

        let epoch = self.next_epoch(selection)?;
        let token = self.next_token(selection)?;
        let slot = self.slot_mut(selection);
        slot.owner = event.owner;
        slot.receive = None;
        self.emit(BackendEvent::SelectionChanged {
            backend: BackendId::X11,
            selection,
            epoch,
        })?;
        if event.owner == x11rb::NONE {
            self.emit(BackendEvent::SelectionUnavailable {
                backend: BackendId::X11,
                selection,
                epoch,
                token,
                reason: UnavailableReason::Cleared,
            })
        } else {
            self.begin_receive(selection, epoch, token, false)
        }
    }

    fn begin_receive(
        &mut self,
        selection: SelectionKind,
        epoch: BackendEpoch,
        token: OfferToken,
        initial: bool,
    ) -> Result<(), ProtocolError> {
        let property = self.atoms.transfer_property(selection);
        self.connection
            .delete_property(self.window, property)
            .map_err(|error| ProtocolError::operation("x11-clear-transfer-property", error))?
            .check()
            .map_err(|error| {
                ProtocolError::operation("x11-clear-transfer-property-reply", error)
            })?;
        let request_sequence = self
            .connection
            .convert_selection(
                self.window,
                self.atoms.selection(selection),
                self.atoms.targets,
                property,
                self.last_server_time,
            )
            .map_err(|error| ProtocolError::operation("x11-request-targets", error))?
            .sequence_number() as u16;
        let now = Instant::now();
        self.slot_mut(selection).receive = Some(ReceiveTransfer {
            epoch,
            token,
            initial,
            started: now,
            last_progress: now,
            phase: ReceivePhase::Targets { request_sequence },
        });
        Ok(())
    }

    fn selection_notify(&mut self, event: SelectionNotifyEvent) -> Result<(), ProtocolError> {
        if event.requestor != self.window {
            return Ok(());
        }
        let Some(selection) = self.atoms.selection_kind(event.selection) else {
            return Ok(());
        };
        let Some(transfer) = self.slot_mut(selection).receive.take() else {
            return Ok(());
        };
        let expected_target = match &transfer.phase {
            ReceivePhase::Targets { request_sequence } => {
                if *request_sequence != event.sequence {
                    self.slot_mut(selection).receive = Some(transfer);
                    return Ok(());
                }
                self.atoms.targets
            }
            ReceivePhase::Data {
                target,
                request_sequence,
            } => {
                if *request_sequence != event.sequence {
                    self.slot_mut(selection).receive = Some(transfer);
                    return Ok(());
                }
                target.atom(&self.atoms)
            }
            ReceivePhase::Incr { .. } => {
                self.slot_mut(selection).receive = Some(transfer);
                return Ok(());
            }
        };
        if event.target != expected_target {
            self.slot_mut(selection).receive = Some(transfer);
            return Ok(());
        }
        if event.property == AtomEnum::NONE.into() {
            return self.finish_transfer_error(
                selection,
                transfer.context(),
                TransferError::Unsupported,
            );
        }

        match transfer.phase {
            ReceivePhase::Targets { .. } => {
                self.handle_targets(selection, transfer, event.property)
            }
            ReceivePhase::Data { target, .. } => {
                self.handle_data(selection, transfer, target, event.property)
            }
            ReceivePhase::Incr { .. } => Ok(()),
        }
    }

    fn handle_targets(
        &mut self,
        selection: SelectionKind,
        mut transfer: ReceiveTransfer,
        property: Atom,
    ) -> Result<(), ProtocolError> {
        let reply = self
            .connection
            .get_property(false, self.window, property, AtomEnum::ATOM, 0, u32::MAX)
            .map_err(|error| ProtocolError::operation("x11-read-targets", error))?
            .reply()
            .map_err(|error| ProtocolError::operation("x11-read-targets-reply", error))?;
        let offered: Vec<Atom> = reply.value32().into_iter().flatten().collect();
        self.delete_transfer_property(property)?;
        let Some(target) = choose_target(&offered, &self.atoms) else {
            return self.finish_transfer_error(
                selection,
                transfer.context(),
                TransferError::Unsupported,
            );
        };
        let request_sequence = self
            .connection
            .convert_selection(
                self.window,
                self.atoms.selection(selection),
                target.atom(&self.atoms),
                property,
                self.last_server_time,
            )
            .map_err(|error| ProtocolError::operation("x11-request-selection-data", error))?
            .sequence_number() as u16;
        transfer.last_progress = Instant::now();
        transfer.phase = ReceivePhase::Data {
            target,
            request_sequence,
        };
        self.slot_mut(selection).receive = Some(transfer);
        Ok(())
    }

    fn handle_data(
        &mut self,
        selection: SelectionKind,
        mut transfer: ReceiveTransfer,
        target: TextTarget,
        property: Atom,
    ) -> Result<(), ProtocolError> {
        let reply = self
            .connection
            .get_property(false, self.window, property, AtomEnum::ANY, 0, u32::MAX)
            .map_err(|error| ProtocolError::operation("x11-read-selection-data", error))?
            .reply()
            .map_err(|error| ProtocolError::operation("x11-read-selection-data-reply", error))?;
        if reply.type_ == self.atoms.incr {
            self.delete_transfer_property(property)?;
            transfer.last_progress = Instant::now();
            transfer.phase = ReceivePhase::Incr {
                target,
                property_type: None,
                assembler: ChunkAssembler::default(),
            };
            self.slot_mut(selection).receive = Some(transfer);
            return Ok(());
        }

        self.delete_transfer_property(property)?;
        match decode(target, reply.type_, reply.value, &self.atoms) {
            Ok(payload) => self.finish_transfer_text(selection, transfer.context(), payload),
            Err(error) => self.finish_transfer_error(selection, transfer.context(), error),
        }
    }

    fn property_notify(&mut self, event: PropertyNotifyEvent) -> Result<(), ProtocolError> {
        self.last_server_time = event.time;
        if event.window == self.window && event.state == Property::NEW_VALUE {
            for selection in [SelectionKind::Clipboard, SelectionKind::Primary] {
                if event.atom == self.atoms.transfer_property(selection) {
                    return self.receive_incr_chunk(selection, event.atom);
                }
            }
            return Ok(());
        }
        if event.state == Property::DELETE {
            self.send_incr_chunk(event.window, event.atom)?;
        }
        Ok(())
    }

    fn receive_incr_chunk(
        &mut self,
        selection: SelectionKind,
        property: Atom,
    ) -> Result<(), ProtocolError> {
        let Some(mut transfer) = self.slot_mut(selection).receive.take() else {
            return Ok(());
        };
        let context = transfer.context();
        let ReceivePhase::Incr {
            target,
            mut property_type,
            mut assembler,
        } = transfer.phase
        else {
            self.slot_mut(selection).receive = Some(transfer);
            return Ok(());
        };
        let reply = self
            .connection
            .get_property(true, self.window, property, AtomEnum::ANY, 0, u32::MAX)
            .map_err(|error| ProtocolError::operation("x11-read-incr-chunk", error))?
            .reply()
            .map_err(|error| ProtocolError::operation("x11-read-incr-chunk-reply", error))?;
        transfer.last_progress = Instant::now();
        if reply.value.is_empty() {
            let result = property_type
                .ok_or(TransferError::Empty)
                .and_then(|type_| decode(target, type_, assembler.finish(), &self.atoms));
            return match result {
                Ok(payload) => self.finish_transfer_text(selection, context, payload),
                Err(error) => self.finish_transfer_error(selection, context, error),
            };
        }
        if let Some(expected) = property_type {
            if expected != reply.type_ {
                return self.finish_transfer_error(selection, context, TransferError::Unsupported);
            }
        } else {
            property_type = Some(reply.type_);
        }
        if let Err(error) = assembler.push(&reply.value) {
            return self.finish_transfer_error(selection, context, error);
        }
        transfer.phase = ReceivePhase::Incr {
            target,
            property_type,
            assembler,
        };
        self.slot_mut(selection).receive = Some(transfer);
        Ok(())
    }

    fn finish_transfer_text(
        &mut self,
        selection: SelectionKind,
        transfer: TransferContext,
        payload: TextPayload,
    ) -> Result<(), ProtocolError> {
        if !self.transfer_is_current(selection, transfer) {
            return Ok(());
        }
        if transfer.initial {
            self.emit(BackendEvent::InitialSnapshot {
                backend: BackendId::X11,
                selection,
                epoch: transfer.epoch,
                token: transfer.token,
                outcome: SnapshotOutcome::Text(payload),
            })
        } else {
            self.emit(BackendEvent::ObservedText {
                backend: BackendId::X11,
                selection,
                epoch: transfer.epoch,
                token: transfer.token,
                payload,
            })
        }
    }

    fn finish_transfer_error(
        &mut self,
        selection: SelectionKind,
        transfer: TransferContext,
        error: TransferError,
    ) -> Result<(), ProtocolError> {
        if !self.transfer_is_current(selection, transfer) {
            return Ok(());
        }
        if transfer.initial {
            let outcome = match error {
                TransferError::Unsupported => SnapshotOutcome::Unsupported,
                TransferError::Empty => SnapshotOutcome::Empty,
                _ => SnapshotOutcome::Failed,
            };
            self.emit(BackendEvent::InitialSnapshot {
                backend: BackendId::X11,
                selection,
                epoch: transfer.epoch,
                token: transfer.token,
                outcome,
            })?;
        } else {
            self.emit(BackendEvent::SelectionUnavailable {
                backend: BackendId::X11,
                selection,
                epoch: transfer.epoch,
                token: transfer.token,
                reason: unavailable_reason(&error),
            })?;
        }
        self.emit(BackendEvent::RecoverableError {
            backend: BackendId::X11,
            selection: Some(selection),
            stage: "receive-selection",
            error,
        })
    }

    fn transfer_is_current(&self, selection: SelectionKind, transfer: TransferContext) -> bool {
        let slot = self.slot(selection);
        transfer.epoch == slot.epoch && transfer.token == slot.token
    }

    fn selection_request(&mut self, event: SelectionRequestEvent) -> Result<(), ProtocolError> {
        let Some(selection) = self.atoms.selection_kind(event.selection) else {
            return self.notify_request(event, AtomEnum::NONE.into());
        };
        let property = request_property(event.property, event.target);
        let Some(owned) = self.slot(selection).owned.clone() else {
            return self.notify_request(event, AtomEnum::NONE.into());
        };

        if event.target == self.atoms.targets {
            let mut targets = vec![
                self.atoms.targets,
                self.atoms.timestamp,
                self.atoms.utf8_string,
                self.atoms.text_plain_utf8,
                self.atoms.text_plain,
                self.atoms.text,
            ];
            if encode_target(self.atoms.string, &owned.payload, &self.atoms).is_some() {
                targets.push(self.atoms.string);
            }
            self.connection
                .change_property32(
                    PropMode::REPLACE,
                    event.requestor,
                    property,
                    AtomEnum::ATOM,
                    &targets,
                )
                .map_err(|error| ProtocolError::operation("x11-serve-targets", error))?
                .check()
                .map_err(|error| ProtocolError::operation("x11-serve-targets-reply", error))?;
            return self.notify_request(event, property);
        }
        if event.target == self.atoms.timestamp {
            self.connection
                .change_property32(
                    PropMode::REPLACE,
                    event.requestor,
                    property,
                    AtomEnum::INTEGER,
                    &[owned.timestamp],
                )
                .map_err(|error| ProtocolError::operation("x11-serve-timestamp", error))?
                .check()
                .map_err(|error| ProtocolError::operation("x11-serve-timestamp-reply", error))?;
            return self.notify_request(event, property);
        }
        if event.target == self.atoms.multiple {
            return self.notify_request(event, AtomEnum::NONE.into());
        }
        let Some(encoded) = encode_target(event.target, &owned.payload, &self.atoms) else {
            return self.notify_request(event, AtomEnum::NONE.into());
        };

        if encoded.bytes.len() <= self.transfer_chunk_size {
            self.connection
                .change_property8(
                    PropMode::REPLACE,
                    event.requestor,
                    property,
                    encoded.property_type,
                    &encoded.bytes,
                )
                .map_err(|error| ProtocolError::operation("x11-serve-direct", error))?
                .check()
                .map_err(|error| ProtocolError::operation("x11-serve-direct-reply", error))?;
            return self.notify_request(event, property);
        }

        if self.outgoing.len() >= MAX_OUTGOING_TRANSFERS {
            return self.notify_request(event, AtomEnum::NONE.into());
        }
        self.connection
            .change_window_attributes(
                event.requestor,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .map_err(|error| ProtocolError::operation("x11-watch-incr-requester", error))?
            .check()
            .map_err(|error| ProtocolError::operation("x11-watch-incr-requester-reply", error))?;
        self.connection
            .change_property32(
                PropMode::REPLACE,
                event.requestor,
                property,
                self.atoms.incr,
                &[u32::try_from(encoded.bytes.len()).map_err(|_| {
                    ProtocolError::invalid_state("x11-serve-incr", "payload length exceeds u32")
                })?],
            )
            .map_err(|error| ProtocolError::operation("x11-announce-incr", error))?
            .check()
            .map_err(|error| ProtocolError::operation("x11-announce-incr-reply", error))?;
        self.outgoing.insert(
            (event.requestor, property),
            OutgoingTransfer {
                property_type: encoded.property_type,
                bytes: encoded.bytes,
                offset: 0,
                last_progress: Instant::now(),
            },
        );
        self.notify_request(event, property)
    }

    fn send_incr_chunk(&mut self, requestor: Window, property: Atom) -> Result<(), ProtocolError> {
        let key = (requestor, property);
        let Some(transfer) = self.outgoing.get_mut(&key) else {
            return Ok(());
        };
        if transfer.offset == transfer.bytes.len() {
            self.connection
                .change_property8(
                    PropMode::REPLACE,
                    requestor,
                    property,
                    transfer.property_type,
                    &[],
                )
                .map_err(|error| ProtocolError::operation("x11-finish-incr", error))?
                .check()
                .map_err(|error| ProtocolError::operation("x11-finish-incr-reply", error))?;
            self.outgoing.remove(&key);
            return Ok(());
        }
        let end = transfer
            .offset
            .saturating_add(self.transfer_chunk_size)
            .min(transfer.bytes.len());
        self.connection
            .change_property8(
                PropMode::REPLACE,
                requestor,
                property,
                transfer.property_type,
                &transfer.bytes[transfer.offset..end],
            )
            .map_err(|error| ProtocolError::operation("x11-send-incr-chunk", error))?
            .check()
            .map_err(|error| ProtocolError::operation("x11-send-incr-chunk-reply", error))?;
        transfer.offset = end;
        transfer.last_progress = Instant::now();
        Ok(())
    }

    fn notify_request(
        &self,
        event: SelectionRequestEvent,
        property: Atom,
    ) -> Result<(), ProtocolError> {
        self.connection
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
            .map_err(|error| ProtocolError::operation("x11-send-selection-notify", error))?
            .check()
            .map_err(|error| ProtocolError::operation("x11-send-selection-notify-reply", error))?;
        self.connection
            .flush()
            .map_err(|error| ProtocolError::operation("x11-serve-flush", error))
    }

    fn selection_clear(&mut self, event: SelectionClearEvent) -> Result<(), ProtocolError> {
        self.last_server_time = event.time;
        let Some(selection) = self.atoms.selection_kind(event.selection) else {
            return Ok(());
        };
        if let Some(owned) = self.slot_mut(selection).owned.take() {
            self.emit(BackendEvent::OwnershipLost {
                backend: BackendId::X11,
                selection,
                revision: owned.revision,
            })?;
        }
        Ok(())
    }

    fn expire_transfers(&mut self) -> Result<(), ProtocolError> {
        let now = Instant::now();
        for selection in [SelectionKind::Clipboard, SelectionKind::Primary] {
            let expired = self
                .slot(selection)
                .receive
                .as_ref()
                .is_some_and(|transfer| {
                    now.duration_since(transfer.last_progress) >= IDLE_TIMEOUT
                        || now.duration_since(transfer.started) >= TOTAL_TIMEOUT
                });
            if expired {
                let transfer = self.slot_mut(selection).receive.take().ok_or_else(|| {
                    ProtocolError::invalid_state(
                        "x11-expire-transfer",
                        "expired receive disappeared before cleanup",
                    )
                })?;
                let error = if now.duration_since(transfer.started) >= TOTAL_TIMEOUT {
                    TransferError::TotalTimeout
                } else {
                    TransferError::IdleTimeout
                };
                self.finish_transfer_error(selection, transfer.context(), error)?;
            }
        }
        self.outgoing
            .retain(|_, transfer| now.duration_since(transfer.last_progress) < TOTAL_TIMEOUT);
        Ok(())
    }

    fn delete_transfer_property(&self, property: Atom) -> Result<(), ProtocolError> {
        self.connection
            .delete_property(self.window, property)
            .map_err(|error| ProtocolError::operation("x11-delete-transfer-property", error))?
            .check()
            .map_err(|error| ProtocolError::operation("x11-delete-transfer-property-reply", error))
    }

    fn next_epoch(&mut self, selection: SelectionKind) -> Result<BackendEpoch, ProtocolError> {
        let next = self.slot(selection).epoch.checked_next().ok_or_else(|| {
            ProtocolError::invalid_state("x11-selection-change", "epoch overflow")
        })?;
        self.slot_mut(selection).epoch = next;
        Ok(next)
    }

    fn next_token(&mut self, selection: SelectionKind) -> Result<OfferToken, ProtocolError> {
        let next = self.slot(selection).token.checked_next().ok_or_else(|| {
            ProtocolError::invalid_state("x11-selection-change", "token overflow")
        })?;
        self.slot_mut(selection).token = next;
        Ok(next)
    }

    fn emit(&self, event: BackendEvent) -> Result<(), ProtocolError> {
        self.event_tx
            .blocking_send(event)
            .map_err(|_| ProtocolError::disconnected("coordinator event receiver closed"))
    }

    fn slot(&self, selection: SelectionKind) -> &SelectionSlot {
        match selection {
            SelectionKind::Clipboard => &self.clipboard,
            SelectionKind::Primary => &self.primary,
        }
    }

    fn slot_mut(&mut self, selection: SelectionKind) -> &mut SelectionSlot {
        match selection {
            SelectionKind::Clipboard => &mut self.clipboard,
            SelectionKind::Primary => &mut self.primary,
        }
    }
}

#[derive(Default)]
struct SelectionSlot {
    epoch: BackendEpoch,
    token: OfferToken,
    owner: Window,
    receive: Option<ReceiveTransfer>,
    owned: Option<OwnedSelection>,
    last_command_id: CommandId,
}

struct ReceiveTransfer {
    epoch: BackendEpoch,
    token: OfferToken,
    initial: bool,
    started: Instant,
    last_progress: Instant,
    phase: ReceivePhase,
}

impl ReceiveTransfer {
    const fn context(&self) -> TransferContext {
        TransferContext {
            epoch: self.epoch,
            token: self.token,
            initial: self.initial,
        }
    }
}

#[derive(Clone, Copy)]
struct TransferContext {
    epoch: BackendEpoch,
    token: OfferToken,
    initial: bool,
}

enum ReceivePhase {
    Targets {
        request_sequence: u16,
    },
    Data {
        target: TextTarget,
        request_sequence: u16,
    },
    Incr {
        target: TextTarget,
        property_type: Option<Atom>,
        assembler: ChunkAssembler,
    },
}

#[derive(Clone)]
struct OwnedSelection {
    revision: Revision,
    payload: TextPayload,
    timestamp: Timestamp,
}

struct OutgoingTransfer {
    property_type: Atom,
    bytes: Vec<u8>,
    offset: usize,
    last_progress: Instant,
}

fn take_command(
    receiver: &mut watch::Receiver<Option<BackendCommand>>,
    selection: SelectionKind,
) -> Result<Option<BackendCommand>, ProtocolError> {
    match receiver.has_changed() {
        Ok(true) => Ok(receiver.borrow_and_update().clone()),
        Ok(false) => Ok(None),
        Err(_) => Err(ProtocolError::disconnected(format!(
            "{selection} command mailbox closed"
        ))),
    }
}

fn unavailable_reason(error: &TransferError) -> UnavailableReason {
    match error {
        TransferError::Unsupported => UnavailableReason::Unsupported,
        TransferError::Empty => UnavailableReason::Empty,
        TransferError::InvalidUtf8 => UnavailableReason::InvalidUtf8,
        TransferError::TooLarge { .. } => UnavailableReason::TooLarge,
        TransferError::IdleTimeout
        | TransferError::TotalTimeout
        | TransferError::Cancelled
        | TransferError::Io { .. } => UnavailableReason::TransferFailed,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Write},
        process::{Child, Command, Stdio},
        thread,
    };

    use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux};

    use super::*;

    struct Xvfb {
        child: Child,
        display: String,
    }

    impl Xvfb {
        fn start() -> Option<Self> {
            let mut child = match Command::new("Xvfb")
                .args([
                    "-displayfd",
                    "1",
                    "-screen",
                    "0",
                    "800x600x24",
                    "-maxbigreqsize",
                    "65536",
                    "-nolisten",
                    "tcp",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
                Err(error) => panic!("failed to start Xvfb: {error}"),
            };
            let stdout = child.stdout.take().expect("Xvfb stdout was configured");
            let mut line = String::new();
            BufReader::new(stdout)
                .read_line(&mut line)
                .expect("Xvfb reports its display number");
            let number = line.trim();
            if number.is_empty() {
                let _ = child.wait();
                return None;
            }
            Some(Self {
                child,
                display: format!(":{number}"),
            })
        }
    }

    impl Drop for Xvfb {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    fn actor_serves_utf8_and_honors_none_property_fallback() {
        let Some(server) = Xvfb::start() else {
            eprintln!("skipping X11 wire test because Xvfb is not installed");
            return;
        };
        let (mut event_rx, clipboard_tx, primary_tx, actor_thread) = start_actor(&server, None);
        receive_empty_startup(&mut event_rx);

        let payload =
            TextPayload::from_string("hello from bridge ☃".to_owned()).expect("test text is valid");
        clipboard_tx.send_replace(Some(BackendCommand::SetText {
            command_id: CommandId::new(1),
            selection: SelectionKind::Clipboard,
            revision: Revision::new(1),
            expected_target_epoch: BackendEpoch::new(1),
            payload: payload.clone(),
        }));
        assert!(matches!(
            recv_event(&mut event_rx),
            BackendEvent::OwnershipApplied {
                backend: BackendId::X11,
                selection: SelectionKind::Clipboard,
                command_id,
                revision,
            } if command_id == CommandId::new(1) && revision == Revision::new(1)
        ));

        let (requester, requester_screen) =
            x11rb::connect(Some(&server.display)).expect("connect requester to isolated Xvfb");
        let root = &requester.setup().roots[requester_screen];
        let window = requester.generate_id().expect("allocate requester window");
        requester
            .create_window(
                root.root_depth,
                window,
                root.root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::COPY_FROM_PARENT,
                root.root_visual,
                &CreateWindowAux::new(),
            )
            .expect("create requester window")
            .check()
            .expect("requester window is valid");
        let clipboard = intern(&requester, "CLIPBOARD");
        let utf8 = intern(&requester, "UTF8_STRING");
        requester
            .convert_selection(window, clipboard, utf8, AtomEnum::NONE, CURRENT_TIME)
            .expect("request bridge selection");
        requester.flush().expect("flush selection request");

        let notify = wait_for_selection_notify(&requester);
        assert_eq!(notify.property, utf8);
        let property = requester
            .get_property(false, window, utf8, AtomEnum::ANY, 0, u32::MAX)
            .expect("read bridge property")
            .reply()
            .expect("bridge property reply");
        assert_eq!(property.type_, utf8);
        assert_eq!(property.value, payload.as_str().as_bytes());

        let targets = intern(&requester, "TARGETS");
        let timestamp = intern(&requester, "TIMESTAMP");
        let string = intern(&requester, "STRING");
        let text = intern(&requester, "TEXT");
        let text_plain = intern(&requester, "text/plain");
        let text_plain_utf8 = intern(&requester, "text/plain;charset=utf-8");
        let multiple = intern(&requester, "MULTIPLE");
        let test_property = intern(&requester, "CLIP_BRIDGE_TEST_PROPERTY");

        requester
            .convert_selection(window, clipboard, targets, test_property, CURRENT_TIME)
            .expect("request bridge TARGETS");
        requester.flush().expect("flush TARGETS request");
        assert_eq!(
            wait_for_selection_notify(&requester).property,
            test_property
        );
        let targets_reply = requester
            .get_property(false, window, test_property, AtomEnum::ATOM, 0, u32::MAX)
            .expect("read TARGETS property")
            .reply()
            .expect("TARGETS property reply");
        let offered: Vec<Atom> = targets_reply
            .value32()
            .expect("TARGETS has 32-bit atom values")
            .collect();
        for required in [targets, timestamp, utf8, text_plain_utf8, text_plain, text] {
            assert!(offered.contains(&required));
        }
        assert!(!offered.contains(&string));

        requester
            .convert_selection(window, clipboard, timestamp, test_property, CURRENT_TIME)
            .expect("request bridge TIMESTAMP");
        requester.flush().expect("flush TIMESTAMP request");
        assert_eq!(
            wait_for_selection_notify(&requester).property,
            test_property
        );
        let timestamp_reply = requester
            .get_property(false, window, test_property, AtomEnum::INTEGER, 0, 1)
            .expect("read TIMESTAMP property")
            .reply()
            .expect("TIMESTAMP property reply");
        assert_eq!(timestamp_reply.format, 32);
        assert_eq!(timestamp_reply.value_len, 1);

        requester
            .convert_selection(window, clipboard, string, test_property, CURRENT_TIME)
            .expect("request a non-representable STRING");
        requester.flush().expect("flush STRING request");
        assert_eq!(
            wait_for_selection_notify(&requester).property,
            AtomEnum::NONE.into()
        );

        requester
            .convert_selection(window, clipboard, multiple, test_property, CURRENT_TIME)
            .expect("request unsupported MULTIPLE");
        requester.flush().expect("flush MULTIPLE request");
        assert_eq!(
            wait_for_selection_notify(&requester).property,
            AtomEnum::NONE.into()
        );

        let latin1 = TextPayload::from_string("A£ÿ".to_owned()).expect("Latin-1 text is valid");
        clipboard_tx.send_replace(Some(BackendCommand::SetText {
            command_id: CommandId::new(2),
            selection: SelectionKind::Clipboard,
            revision: Revision::new(2),
            expected_target_epoch: BackendEpoch::new(1),
            payload: latin1,
        }));
        assert!(matches!(
            recv_event(&mut event_rx),
            BackendEvent::OwnershipApplied {
                command_id,
                revision,
                ..
            } if command_id == CommandId::new(2) && revision == Revision::new(2)
        ));
        requester
            .convert_selection(window, clipboard, string, test_property, CURRENT_TIME)
            .expect("request representable STRING");
        requester
            .flush()
            .expect("flush representable STRING request");
        assert_eq!(
            wait_for_selection_notify(&requester).property,
            test_property
        );
        let string_reply = requester
            .get_property(false, window, test_property, AtomEnum::STRING, 0, u32::MAX)
            .expect("read STRING property")
            .reply()
            .expect("STRING property reply");
        assert_eq!(string_reply.value, [0x41, 0xa3, 0xff]);

        shutdown_actor(clipboard_tx, primary_tx, actor_thread);
    }

    #[test]
    fn actor_receives_utf8_from_external_owner() {
        if !xclip_available() {
            eprintln!("skipping X11 receive wire test because xclip is not installed");
            return;
        }
        let Some(server) = Xvfb::start() else {
            eprintln!("skipping X11 receive wire test because Xvfb is unavailable");
            return;
        };
        let (mut event_rx, clipboard_tx, primary_tx, actor_thread) = start_actor(&server, None);
        receive_empty_startup(&mut event_rx);

        let expected = "external owner payload 雪";
        let mut owner = Command::new("xclip")
            .env("DISPLAY", &server.display)
            .args(["-selection", "clipboard", "-in", "-loops", "2", "-silent"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start isolated xclip owner");
        owner
            .stdin
            .take()
            .expect("xclip stdin is piped")
            .write_all(expected.as_bytes())
            .expect("write xclip selection");

        assert!(matches!(
            recv_event(&mut event_rx),
            BackendEvent::SelectionChanged {
                backend: BackendId::X11,
                selection: SelectionKind::Clipboard,
                epoch,
            } if epoch == BackendEpoch::new(2)
        ));
        assert!(matches!(
            recv_event(&mut event_rx),
            BackendEvent::ObservedText {
                backend: BackendId::X11,
                selection: SelectionKind::Clipboard,
                epoch,
                payload,
                ..
            } if epoch == BackendEpoch::new(2) && payload.as_str() == expected
        ));
        assert!(owner.wait().expect("wait for xclip owner").success());
        shutdown_actor(clipboard_tx, primary_tx, actor_thread);
    }

    #[test]
    fn actor_receives_selection_over_incr() {
        if !xclip_available() {
            eprintln!("skipping X11 INCR receive test because xclip is not installed");
            return;
        }
        let Some(server) = Xvfb::start() else {
            eprintln!("skipping X11 INCR receive test because Xvfb is unavailable");
            return;
        };
        let (mut event_rx, clipboard_tx, primary_tx, actor_thread) = start_actor(&server, None);
        receive_empty_startup(&mut event_rx);

        let expected = "external INCR 雪\n".repeat(16_384);
        let mut owner = Command::new("xclip")
            .env("DISPLAY", &server.display)
            .args(["-selection", "clipboard", "-in", "-loops", "2", "-silent"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start INCR xclip owner");
        owner
            .stdin
            .take()
            .expect("xclip stdin is piped")
            .write_all(expected.as_bytes())
            .expect("write large xclip selection");

        assert!(matches!(
            recv_event(&mut event_rx),
            BackendEvent::SelectionChanged {
                selection: SelectionKind::Clipboard,
                ..
            }
        ));
        assert!(matches!(
            recv_event(&mut event_rx),
            BackendEvent::ObservedText {
                selection: SelectionKind::Clipboard,
                payload,
                ..
            } if payload.as_str() == expected
        ));
        assert!(owner.wait().expect("wait for INCR xclip owner").success());
        shutdown_actor(clipboard_tx, primary_tx, actor_thread);
    }

    #[test]
    fn actor_serves_selection_over_incr() {
        if !xclip_available() {
            eprintln!("skipping X11 INCR wire test because xclip is not installed");
            return;
        }
        let Some(server) = Xvfb::start() else {
            eprintln!("skipping X11 INCR wire test because Xvfb is unavailable");
            return;
        };
        let (mut event_rx, clipboard_tx, primary_tx, actor_thread) =
            start_actor(&server, Some(1024));
        receive_empty_startup(&mut event_rx);

        let text = "INCR payload 雪\n".repeat(2048);
        let payload = TextPayload::from_string(text.clone()).expect("test text is valid");
        clipboard_tx.send_replace(Some(BackendCommand::SetText {
            command_id: CommandId::new(1),
            selection: SelectionKind::Clipboard,
            revision: Revision::new(1),
            expected_target_epoch: BackendEpoch::new(1),
            payload,
        }));
        assert!(matches!(
            recv_event(&mut event_rx),
            BackendEvent::OwnershipApplied {
                backend: BackendId::X11,
                selection: SelectionKind::Clipboard,
                ..
            }
        ));

        let output = Command::new("xclip")
            .env("DISPLAY", &server.display)
            .args(["-selection", "clipboard", "-out"])
            .output()
            .expect("read selection through xclip");
        assert!(output.status.success());
        assert_eq!(output.stdout, text.as_bytes());
        shutdown_actor(clipboard_tx, primary_tx, actor_thread);
    }

    #[test]
    fn actor_serves_incr_to_a_slow_requester() {
        let Some(server) = Xvfb::start() else {
            eprintln!("skipping X11 slow requester test because Xvfb is unavailable");
            return;
        };
        let (mut event_rx, clipboard_tx, primary_tx, actor_thread) =
            start_actor(&server, Some(1024));
        receive_empty_startup(&mut event_rx);

        let text = "slow INCR requester 雪\n".repeat(2048);
        clipboard_tx.send_replace(Some(BackendCommand::SetText {
            command_id: CommandId::new(1),
            selection: SelectionKind::Clipboard,
            revision: Revision::new(1),
            expected_target_epoch: BackendEpoch::new(1),
            payload: TextPayload::from_string(text.clone()).expect("test text is valid"),
        }));
        assert!(matches!(
            recv_event(&mut event_rx),
            BackendEvent::OwnershipApplied {
                selection: SelectionKind::Clipboard,
                ..
            }
        ));

        let (requester, requester_screen) =
            x11rb::connect(Some(&server.display)).expect("connect slow requester to Xvfb");
        let root = &requester.setup().roots[requester_screen];
        let window = requester
            .generate_id()
            .expect("allocate slow requester window");
        requester
            .create_window(
                root.root_depth,
                window,
                root.root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::COPY_FROM_PARENT,
                root.root_visual,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .expect("create slow requester window")
            .check()
            .expect("slow requester window is valid");
        let clipboard = intern(&requester, "CLIPBOARD");
        let utf8 = intern(&requester, "UTF8_STRING");
        let incr = intern(&requester, "INCR");
        let property = intern(&requester, "CLIP_BRIDGE_SLOW_INCR");
        requester
            .convert_selection(window, clipboard, utf8, property, CURRENT_TIME)
            .expect("request slow INCR transfer");
        requester.flush().expect("flush slow INCR request");

        assert_eq!(wait_for_selection_notify(&requester).property, property);
        let announcement = requester
            .get_property(false, window, property, AtomEnum::ANY, 0, 1)
            .expect("read INCR announcement")
            .reply()
            .expect("INCR announcement reply");
        assert_eq!(announcement.type_, incr);
        requester
            .delete_property(window, property)
            .expect("acknowledge INCR announcement")
            .check()
            .expect("delete INCR announcement property");
        requester
            .flush()
            .expect("flush initial INCR acknowledgement");

        let mut received = Vec::new();
        loop {
            wait_for_property_value(&requester, window, property);
            let chunk = requester
                .get_property(true, window, property, AtomEnum::ANY, 0, u32::MAX)
                .expect("read INCR chunk")
                .reply()
                .expect("INCR chunk reply");
            assert_eq!(chunk.type_, utf8);
            if chunk.value.is_empty() {
                break;
            }
            received.extend_from_slice(&chunk.value);
            requester.flush().expect("flush INCR chunk deletion");
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(received, text.as_bytes());

        shutdown_actor(clipboard_tx, primary_tx, actor_thread);
    }

    #[test]
    fn actor_reports_x_server_disconnect() {
        let Some(server) = Xvfb::start() else {
            eprintln!("skipping X11 disconnect test because Xvfb is unavailable");
            return;
        };
        let (mut event_rx, clipboard_tx, primary_tx, actor_thread) = start_actor(&server, None);
        receive_empty_startup(&mut event_rx);
        drop(server);

        let result = actor_thread.join().expect("X11 actor does not panic");
        assert!(result.is_err());
        drop(clipboard_tx);
        drop(primary_tx);
    }

    #[test]
    fn actor_keeps_clipboard_and_primary_independent() {
        if !xclip_available() {
            eprintln!("skipping X11 selection isolation test because xclip is not installed");
            return;
        }
        let Some(server) = Xvfb::start() else {
            eprintln!("skipping X11 selection isolation test because Xvfb is unavailable");
            return;
        };
        let (mut event_rx, clipboard_tx, primary_tx, actor_thread) = start_actor(&server, None);
        receive_empty_startup(&mut event_rx);

        for (sender, selection, command_id, text) in [
            (
                &clipboard_tx,
                SelectionKind::Clipboard,
                1,
                "isolated Clipboard 雪",
            ),
            (
                &primary_tx,
                SelectionKind::Primary,
                2,
                "isolated Primary 桥",
            ),
        ] {
            sender.send_replace(Some(BackendCommand::SetText {
                command_id: CommandId::new(command_id),
                selection,
                revision: Revision::new(command_id),
                expected_target_epoch: BackendEpoch::new(1),
                payload: TextPayload::from_string(text.to_owned()).expect("test text is valid"),
            }));
        }
        for expected_selection in [SelectionKind::Clipboard, SelectionKind::Primary] {
            assert!(matches!(
                recv_event(&mut event_rx),
                BackendEvent::OwnershipApplied {
                    selection,
                    ..
                } if selection == expected_selection
            ));
        }

        for (selection_name, expected) in [
            ("clipboard", "isolated Clipboard 雪"),
            ("primary", "isolated Primary 桥"),
        ] {
            let output = Command::new("xclip")
                .env("DISPLAY", &server.display)
                .args(["-selection", selection_name, "-out"])
                .output()
                .expect("read isolated X11 selection");
            assert!(output.status.success());
            assert_eq!(output.stdout, expected.as_bytes());
        }

        shutdown_actor(clipboard_tx, primary_tx, actor_thread);
    }

    #[test]
    fn replacing_owner_discards_the_in_flight_transfer() {
        if !xclip_available() {
            eprintln!("skipping X11 owner replacement test because xclip is not installed");
            return;
        }
        let Some(server) = Xvfb::start() else {
            eprintln!("skipping X11 owner replacement test because Xvfb is unavailable");
            return;
        };
        let (mut event_rx, clipboard_tx, primary_tx, actor_thread) = start_actor(&server, None);
        receive_empty_startup(&mut event_rx);

        let first_text = "stale INCR owner\n".repeat(32_768);
        let mut first_owner = spawn_xclip_owner(&server, "0", first_text.as_bytes());
        assert!(matches!(
            recv_event(&mut event_rx),
            BackendEvent::SelectionChanged {
                selection: SelectionKind::Clipboard,
                epoch,
                ..
            } if epoch == BackendEpoch::new(2)
        ));

        let current_text = "current owner 雪";
        let mut current_owner = spawn_xclip_owner(&server, "2", current_text.as_bytes());
        let mut current_epoch = None;
        loop {
            match recv_event(&mut event_rx) {
                BackendEvent::SelectionChanged {
                    selection: SelectionKind::Clipboard,
                    epoch,
                    ..
                } => current_epoch = Some(epoch),
                BackendEvent::ObservedText {
                    selection: SelectionKind::Clipboard,
                    epoch,
                    payload,
                    ..
                } if Some(epoch) == current_epoch => {
                    assert_eq!(payload.as_str(), current_text);
                    break;
                }
                BackendEvent::ObservedText { epoch, .. } if current_epoch.is_some() => {
                    panic!("stale X11 transfer emitted text for epoch {epoch:?}");
                }
                _ => {}
            }
        }

        assert!(
            first_owner
                .wait()
                .expect("wait for replaced xclip owner")
                .success()
        );
        assert!(
            current_owner
                .wait()
                .expect("wait for current xclip owner")
                .success()
        );
        shutdown_actor(clipboard_tx, primary_tx, actor_thread);
    }

    type ActorThread = thread::JoinHandle<Result<(), ProtocolError>>;
    type ActorHarness = (
        mpsc::Receiver<BackendEvent>,
        watch::Sender<Option<BackendCommand>>,
        watch::Sender<Option<BackendCommand>>,
        ActorThread,
    );

    fn start_actor(server: &Xvfb, transfer_chunk_size: Option<usize>) -> ActorHarness {
        let (connection, screen_number) =
            x11rb::connect(Some(&server.display)).expect("connect actor to isolated Xvfb");
        let (event_tx, event_rx) = mpsc::channel(32);
        let (clipboard_tx, clipboard_rx) = watch::channel(None);
        let (primary_tx, primary_rx) = watch::channel(None);
        let actor_thread = thread::spawn(move || {
            let mut actor = Actor::new(
                connection,
                screen_number,
                event_tx,
                clipboard_rx,
                primary_rx,
            )
            .expect("initialize X11 actor");
            if let Some(chunk_size) = transfer_chunk_size {
                actor.transfer_chunk_size = chunk_size;
            }
            actor.initialize_snapshots().expect("query X11 snapshots");
            actor.run()
        });
        (event_rx, clipboard_tx, primary_tx, actor_thread)
    }

    fn receive_empty_startup(receiver: &mut mpsc::Receiver<BackendEvent>) {
        let mut snapshots = 0;
        while snapshots < 2 {
            match recv_event(receiver) {
                BackendEvent::Ready { .. } => {}
                BackendEvent::InitialSnapshot {
                    epoch,
                    outcome: SnapshotOutcome::Empty,
                    ..
                } => {
                    assert_eq!(epoch, BackendEpoch::new(1));
                    snapshots += 1;
                }
                event => panic!("unexpected startup event: {event:?}"),
            }
        }
    }

    fn shutdown_actor(
        clipboard_tx: watch::Sender<Option<BackendCommand>>,
        primary_tx: watch::Sender<Option<BackendCommand>>,
        actor_thread: ActorThread,
    ) {
        clipboard_tx.send_replace(Some(BackendCommand::Shutdown));
        primary_tx.send_replace(Some(BackendCommand::Shutdown));
        actor_thread
            .join()
            .expect("X11 actor thread does not panic")
            .expect("X11 actor shuts down cleanly");
    }

    fn xclip_available() -> bool {
        Command::new("xclip")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    }

    fn spawn_xclip_owner(server: &Xvfb, loops: &str, bytes: &[u8]) -> Child {
        let mut owner = Command::new("xclip")
            .env("DISPLAY", &server.display)
            .args(["-selection", "clipboard", "-in", "-loops", loops, "-silent"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start isolated xclip owner");
        owner
            .stdin
            .take()
            .expect("xclip stdin is piped")
            .write_all(bytes)
            .expect("write xclip selection");
        owner
    }

    fn recv_event(receiver: &mut mpsc::Receiver<BackendEvent>) -> BackendEvent {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match receiver.try_recv() {
                Ok(event) => return event,
                Err(mpsc::error::TryRecvError::Empty) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("timed out waiting for backend event: {error}"),
            }
        }
    }

    fn wait_for_selection_notify(connection: &RustConnection) -> SelectionNotifyEvent {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match connection.poll_for_event() {
                Ok(Some(Event::SelectionNotify(event))) => return event,
                Ok(Some(_)) | Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Ok(_) => panic!("timed out waiting for SelectionNotify"),
                Err(error) => panic!("failed while waiting for SelectionNotify: {error}"),
            }
        }
    }

    fn wait_for_property_value(connection: &RustConnection, window: Window, property: Atom) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match connection.poll_for_event() {
                Ok(Some(Event::PropertyNotify(event)))
                    if event.window == window
                        && event.atom == property
                        && event.state == Property::NEW_VALUE =>
                {
                    return;
                }
                Ok(Some(_)) | Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Ok(_) => panic!("timed out waiting for INCR property value"),
                Err(error) => panic!("failed while waiting for INCR property value: {error}"),
            }
        }
    }

    fn intern(connection: &RustConnection, name: &str) -> Atom {
        connection
            .intern_atom(false, name.as_bytes())
            .expect("intern test atom")
            .reply()
            .expect("test atom reply")
            .atom
    }
}
