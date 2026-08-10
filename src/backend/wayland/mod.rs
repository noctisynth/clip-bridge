mod offer;
mod provider;
mod source;
#[cfg(test)]
mod test_server;

use std::{
    collections::HashMap,
    os::fd::{AsFd, BorrowedFd},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc,
    },
    thread,
};

use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::poll::{PollFd, PollFlags, poll};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    backend::ObjectId,
    event_created_child,
    protocol::{wl_registry, wl_seat},
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::{self, ExtDataControlManagerV1},
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::{self, ZwlrDataControlManagerV1},
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

use crate::{
    backend::{BackendCommand, BackendEvent},
    domain::{
        BackendCapabilities, BackendEpoch, BackendId, CommandId, OfferToken, ProtocolError,
        Revision, SelectionKind, SnapshotOutcome, TextPayload, TransferError, UnavailableReason,
    },
};

use self::{
    offer::{cancellation, choose_mime, read_pipe},
    provider::{ProviderDevice, ProviderManager, ProviderOffer, ProviderSource},
    source::write_pipe,
};

const ACTOR_POLL_TIMEOUT_MS: u16 = 100;
const WORKER_COUNT: usize = 4;
const WORK_QUEUE_CAPACITY: usize = 8;
const COMPLETION_CAPACITY: usize = WORKER_COUNT + WORK_QUEUE_CAPACITY;

fn set_nonblocking(fd: BorrowedFd<'_>, stage: &'static str) -> Result<(), TransferError> {
    let current = fcntl(fd, FcntlArg::F_GETFL).map_err(|error| TransferError::io(stage, error))?;
    let flags = OFlag::from_bits_truncate(current);
    fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))
        .map_err(|error| TransferError::io(stage, error))?;
    Ok(())
}

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
                backend: BackendId::Wayland,
                error: error.clone(),
            })
            .is_err()
    {
        debug!("coordinator unavailable while reporting fatal Wayland error");
    }
    result
}

fn run_actor(
    event_tx: mpsc::Sender<BackendEvent>,
    clipboard_commands: watch::Receiver<Option<BackendCommand>>,
    primary_commands: watch::Receiver<Option<BackendCommand>>,
) -> Result<(), ProtocolError> {
    let connection = Connection::connect_to_env()
        .map_err(|error| ProtocolError::operation("wayland-connect", error))?;
    run_connection(connection, event_tx, clipboard_commands, primary_commands)
}

fn run_connection(
    connection: Connection,
    event_tx: mpsc::Sender<BackendEvent>,
    clipboard_commands: watch::Receiver<Option<BackendCommand>>,
    primary_commands: watch::Receiver<Option<BackendCommand>>,
) -> Result<(), ProtocolError> {
    run_connection_with_provider(
        connection,
        event_tx,
        clipboard_commands,
        primary_commands,
        choose_provider,
    )
}

fn run_connection_with_provider(
    connection: Connection,
    event_tx: mpsc::Sender<BackendEvent>,
    clipboard_commands: watch::Receiver<Option<BackendCommand>>,
    primary_commands: watch::Receiver<Option<BackendCommand>>,
    provider_choice: fn(&Globals) -> Result<ProviderChoice, ProtocolError>,
) -> Result<(), ProtocolError> {
    let display = connection.display();
    let mut event_queue = connection.new_event_queue();
    let queue = event_queue.handle();
    let registry = display.get_registry(&queue, RegistryData);
    let mut state = WaylandState::new(
        queue.clone(),
        event_tx,
        clipboard_commands,
        primary_commands,
    )?;

    event_queue
        .roundtrip(&mut state)
        .map_err(|error| ProtocolError::operation("wayland-discover-globals", error))?;
    state.bind_provider(&registry, provider_choice)?;
    state.emit_ready()?;
    event_queue
        .roundtrip(&mut state)
        .map_err(|error| ProtocolError::operation("wayland-initial-snapshot", error))?;
    state.check_fatal()?;

    loop {
        state.process_completions()?;
        if !state.process_commands()? {
            state.cancel_transfers();
            return Ok(());
        }
        let flushed = flush_event_queue(&event_queue)?;
        if flushed {
            state.commit_prepared_sources()?;
        }
        event_queue
            .dispatch_pending(&mut state)
            .map_err(|error| ProtocolError::operation("wayland-dispatch", error))?;
        state.check_fatal()?;

        let Some(read_guard) = event_queue.prepare_read() else {
            continue;
        };
        let ready = {
            let flags = if flushed {
                PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP
            } else {
                PollFlags::POLLIN | PollFlags::POLLOUT | PollFlags::POLLERR | PollFlags::POLLHUP
            };
            let mut poll_fds = [PollFd::new(read_guard.connection_fd(), flags)];
            let count = poll(&mut poll_fds, ACTOR_POLL_TIMEOUT_MS)
                .map_err(|error| ProtocolError::operation("wayland-poll", error))?;
            if poll_fds[0]
                .revents()
                .is_some_and(|events| events.intersects(PollFlags::POLLERR | PollFlags::POLLHUP))
            {
                return Err(ProtocolError::disconnected("Wayland socket closed"));
            }
            count > 0
                && poll_fds[0]
                    .revents()
                    .is_some_and(|events| events.contains(PollFlags::POLLIN))
        };
        if ready {
            read_guard.read().map_err(ProtocolError::disconnected)?;
        }
    }
}

fn flush_event_queue<State>(
    event_queue: &wayland_client::EventQueue<State>,
) -> Result<bool, ProtocolError> {
    match event_queue.flush() {
        Ok(()) => Ok(true),
        Err(wayland_client::backend::WaylandError::Io(error))
            if error.kind() == std::io::ErrorKind::WouldBlock =>
        {
            Ok(false)
        }
        Err(error) => Err(ProtocolError::disconnected(error)),
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct RegistryData;

#[derive(Default)]
struct Globals {
    ext_manager: Option<(u32, u32)>,
    wlr_manager: Option<(u32, u32)>,
    seats: Vec<(u32, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderChoice {
    Ext { name: u32, version: u32 },
    Wlr { name: u32, version: u32 },
}

fn choose_provider(globals: &Globals) -> Result<ProviderChoice, ProtocolError> {
    if let Some((name, version)) = globals.ext_manager {
        Ok(ProviderChoice::Ext {
            name,
            version: version.min(1),
        })
    } else if let Some((name, version)) = globals.wlr_manager {
        Ok(ProviderChoice::Wlr {
            name,
            version: version.min(2),
        })
    } else {
        Err(ProtocolError::invalid_state(
            "wayland-registry",
            "compositor advertises neither ext-data-control-v1 nor wlr-data-control-v1",
        ))
    }
}

pub(super) struct WaylandState {
    queue: QueueHandle<Self>,
    event_tx: mpsc::Sender<BackendEvent>,
    clipboard_commands: watch::Receiver<Option<BackendCommand>>,
    primary_commands: watch::Receiver<Option<BackendCommand>>,
    globals: Globals,
    provider: Option<ProviderManager>,
    device: Option<ProviderDevice>,
    provider_global: Option<u32>,
    seat_global: Option<u32>,
    supports_primary: bool,
    offers: HashMap<ObjectId, OfferRecord>,
    clipboard: SelectionSlot,
    primary: SelectionSlot,
    prepared_sources: Vec<PreparedSource>,
    workers: WorkerPool,
    fatal: Option<ProtocolError>,
}

impl WaylandState {
    fn new(
        queue: QueueHandle<Self>,
        event_tx: mpsc::Sender<BackendEvent>,
        clipboard_commands: watch::Receiver<Option<BackendCommand>>,
        primary_commands: watch::Receiver<Option<BackendCommand>>,
    ) -> Result<Self, ProtocolError> {
        Ok(Self {
            queue,
            event_tx,
            clipboard_commands,
            primary_commands,
            globals: Globals::default(),
            provider: None,
            device: None,
            provider_global: None,
            seat_global: None,
            supports_primary: false,
            offers: HashMap::new(),
            clipboard: SelectionSlot::new(),
            primary: SelectionSlot::new(),
            prepared_sources: Vec::new(),
            workers: WorkerPool::new()?,
            fatal: None,
        })
    }

    fn bind_provider(
        &mut self,
        registry: &wl_registry::WlRegistry,
        provider_choice: fn(&Globals) -> Result<ProviderChoice, ProtocolError>,
    ) -> Result<(), ProtocolError> {
        let (seat_name, seat_version) = *self.globals.seats.first().ok_or_else(|| {
            ProtocolError::invalid_state("wayland-registry", "no wl_seat global is available")
        })?;
        if self.globals.seats.len() > 1 {
            warn!(
                seats = self.globals.seats.len(),
                selected_global = seat_name,
                "multiple Wayland seats found; using the first advertised seat"
            );
        }
        let seat =
            registry.bind::<wl_seat::WlSeat, _, _>(seat_name, seat_version.min(9), &self.queue, ());

        let (provider, provider_global) = match provider_choice(&self.globals)? {
            ProviderChoice::Ext { name, version } => {
                let manager =
                    registry.bind::<ExtDataControlManagerV1, _, _>(name, version, &self.queue, ());
                (ProviderManager::Ext(manager), name)
            }
            ProviderChoice::Wlr { name, version } => {
                let manager =
                    registry.bind::<ZwlrDataControlManagerV1, _, _>(name, version, &self.queue, ());
                (ProviderManager::Wlr { manager, version }, name)
            }
        };

        self.supports_primary = provider.supports_primary();
        self.device = Some(provider.get_device(&seat, &self.queue));
        self.provider = Some(provider);
        self.provider_global = Some(provider_global);
        self.seat_global = Some(seat_name);
        Ok(())
    }

    fn emit_ready(&self) -> Result<(), ProtocolError> {
        self.emit(BackendEvent::Ready {
            backend: BackendId::Wayland,
            capabilities: BackendCapabilities {
                clipboard: true,
                primary: self.supports_primary,
            },
        })
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
                } => self.prepare_source(
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

    fn prepare_source(
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
        if selection == SelectionKind::Primary && !self.supports_primary {
            return self.emit_ownership_failed(
                selection,
                command_id,
                revision,
                "Wayland provider does not support Primary",
            );
        }
        if current_epoch != expected_target_epoch {
            return self.emit_ownership_failed(
                selection,
                command_id,
                revision,
                format!(
                    "stale target epoch {}; current epoch is {}",
                    expected_target_epoch.value(),
                    current_epoch.value()
                ),
            );
        }
        let provider = self.provider.as_ref().ok_or_else(|| {
            ProtocolError::invalid_state("wayland-set-selection", "provider is not initialized")
        })?;
        let device = self.device.as_ref().ok_or_else(|| {
            ProtocolError::invalid_state("wayland-set-selection", "device is not initialized")
        })?;
        let cancelled = cancellation();
        let data = SourceData {
            selection,
            revision,
            payload,
            cancelled: cancelled.clone(),
        };
        let source = provider.create_source(&self.queue, data);
        source.ensure_matches(provider)?;
        source.offer_text();
        device.set_selection(selection, &source)?;
        self.prepared_sources.push(PreparedSource {
            command_id,
            selection,
            revision,
            source,
            cancelled,
        });
        Ok(())
    }

    fn commit_prepared_sources(&mut self) -> Result<(), ProtocolError> {
        for prepared in std::mem::take(&mut self.prepared_sources) {
            let old = self
                .slot_mut(prepared.selection)
                .owned
                .replace(OwnedSource {
                    source: prepared.source,
                    revision: prepared.revision,
                    cancelled: prepared.cancelled,
                });
            if let Some(old) = old {
                old.cancelled.store(true, Ordering::Release);
                old.source.destroy();
            }
            self.emit(BackendEvent::OwnershipApplied {
                backend: BackendId::Wayland,
                selection: prepared.selection,
                command_id: prepared.command_id,
                revision: prepared.revision,
            })?;
        }
        Ok(())
    }

    fn offer_created(&mut self, offer: ProviderOffer) {
        self.offers.insert(
            offer.id(),
            OfferRecord {
                offer,
                mime_types: Vec::new(),
            },
        );
    }

    fn offer_mime(&mut self, id: ObjectId, mime_type: String) {
        if let Some(offer) = self.offers.get_mut(&id) {
            offer.mime_types.push(mime_type);
        }
    }

    fn selection_event(
        &mut self,
        selection: SelectionKind,
        offer: Option<ProviderOffer>,
    ) -> Result<(), ProtocolError> {
        if selection == SelectionKind::Primary && !self.supports_primary {
            return Ok(());
        }
        let initial = self.slot(selection).initial_pending;
        let epoch = self.next_epoch(selection)?;
        let token = self.next_token(selection)?;
        if let Some(cancelled) = self.slot_mut(selection).read_cancel.take() {
            cancelled.store(true, Ordering::Release);
        }

        let new_id = offer.as_ref().map(ProviderOffer::id);
        if let Some(old_id) = self.slot_mut(selection).active_offer.take()
            && Some(old_id.clone()) != new_id
            && let Some(old) = self.offers.remove(&old_id)
        {
            old.offer.destroy();
        }
        self.slot_mut(selection).active_offer = new_id.clone();
        self.slot_mut(selection).initial_pending = false;
        if !initial {
            self.emit(BackendEvent::SelectionChanged {
                backend: BackendId::Wayland,
                selection,
                epoch,
            })?;
        }

        let Some(offer) = offer else {
            return if initial {
                self.emit(BackendEvent::InitialSnapshot {
                    backend: BackendId::Wayland,
                    selection,
                    epoch,
                    token,
                    outcome: SnapshotOutcome::Empty,
                })
            } else {
                self.emit(BackendEvent::SelectionUnavailable {
                    backend: BackendId::Wayland,
                    selection,
                    epoch,
                    token,
                    reason: UnavailableReason::Cleared,
                })
            };
        };
        let id = offer.id();
        let Some(record) = self.offers.get(&id) else {
            return self.finish_without_worker(
                selection,
                epoch,
                token,
                initial,
                Err(TransferError::Unsupported),
            );
        };
        let Some(mime_type) = choose_mime(&record.mime_types) else {
            return self.finish_without_worker(
                selection,
                epoch,
                token,
                initial,
                Err(TransferError::Unsupported),
            );
        };
        let (read_fd, write_fd) = nix::unistd::pipe()
            .map_err(|error| ProtocolError::operation("wayland-create-receive-pipe", error))?;
        offer.receive(mime_type, write_fd.as_fd());
        drop(write_fd);
        let cancelled = cancellation();
        self.slot_mut(selection).read_cancel = Some(cancelled.clone());
        let job = Work::Read {
            selection,
            epoch,
            token,
            initial,
            fd: read_fd,
            cancelled,
        };
        if let Err(job) = self.workers.submit(job) {
            drop(job);
            return self.finish_without_worker(
                selection,
                epoch,
                token,
                initial,
                Err(TransferError::io(
                    "queue-read-worker",
                    "worker queue is full",
                )),
            );
        }
        Ok(())
    }

    fn finish_without_worker(
        &mut self,
        selection: SelectionKind,
        epoch: BackendEpoch,
        token: OfferToken,
        initial: bool,
        result: Result<TextPayload, TransferError>,
    ) -> Result<(), ProtocolError> {
        self.finish_read(selection, epoch, token, initial, result)
    }

    fn process_completions(&mut self) -> Result<(), ProtocolError> {
        while let Some(completion) = self.workers.try_completion()? {
            match completion {
                Completion::Read {
                    selection,
                    epoch,
                    token,
                    initial,
                    result,
                } => self.finish_read(selection, epoch, token, initial, result)?,
                Completion::Write {
                    selection,
                    result: Err(error),
                } => self.emit(BackendEvent::RecoverableError {
                    backend: BackendId::Wayland,
                    selection: Some(selection),
                    stage: "serve-selection",
                    error,
                })?,
                Completion::Write { result: Ok(()), .. } => {}
            }
        }
        Ok(())
    }

    fn finish_read(
        &mut self,
        selection: SelectionKind,
        epoch: BackendEpoch,
        token: OfferToken,
        initial: bool,
        result: Result<TextPayload, TransferError>,
    ) -> Result<(), ProtocolError> {
        if !self.slot(selection).transfer_is_current(epoch, token) {
            return Ok(());
        }
        self.slot_mut(selection).read_cancel = None;
        match result {
            Ok(payload) if initial => self.emit(BackendEvent::InitialSnapshot {
                backend: BackendId::Wayland,
                selection,
                epoch,
                token,
                outcome: SnapshotOutcome::Text(payload),
            }),
            Ok(payload) => self.emit(BackendEvent::ObservedText {
                backend: BackendId::Wayland,
                selection,
                epoch,
                token,
                payload,
            }),
            Err(error) if initial => {
                let outcome = match error {
                    TransferError::Empty => SnapshotOutcome::Empty,
                    TransferError::Unsupported => SnapshotOutcome::Unsupported,
                    _ => SnapshotOutcome::Failed,
                };
                self.emit(BackendEvent::InitialSnapshot {
                    backend: BackendId::Wayland,
                    selection,
                    epoch,
                    token,
                    outcome,
                })?;
                if !matches!(error, TransferError::Empty | TransferError::Unsupported) {
                    self.emit(BackendEvent::RecoverableError {
                        backend: BackendId::Wayland,
                        selection: Some(selection),
                        stage: "receive-selection",
                        error,
                    })?;
                }
                Ok(())
            }
            Err(error) => {
                self.emit(BackendEvent::SelectionUnavailable {
                    backend: BackendId::Wayland,
                    selection,
                    epoch,
                    token,
                    reason: unavailable_reason(&error),
                })?;
                if !matches!(error, TransferError::Empty | TransferError::Unsupported) {
                    self.emit(BackendEvent::RecoverableError {
                        backend: BackendId::Wayland,
                        selection: Some(selection),
                        stage: "receive-selection",
                        error,
                    })?;
                }
                Ok(())
            }
        }
    }

    fn source_send(
        &mut self,
        data: &SourceData,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
    ) -> Result<(), ProtocolError> {
        if mime_type != "text/plain;charset=utf-8" && mime_type != "text/plain" {
            return Ok(());
        }
        let job = Work::Write {
            selection: data.selection,
            payload: data.payload.clone(),
            fd,
            cancelled: data.cancelled.clone(),
        };
        if let Err(job) = self.workers.submit(job) {
            drop(job);
            self.emit(BackendEvent::RecoverableError {
                backend: BackendId::Wayland,
                selection: Some(data.selection),
                stage: "queue-serve-worker",
                error: TransferError::io("queue-write-worker", "worker queue is full"),
            })?;
        }
        Ok(())
    }

    fn source_cancelled(
        &mut self,
        source_id: ObjectId,
        data: &SourceData,
    ) -> Result<(), ProtocolError> {
        data.cancelled.store(true, Ordering::Release);
        let current_matches = self
            .slot(data.selection)
            .owned
            .as_ref()
            .is_some_and(|owned| owned.source.id() == source_id && owned.revision == data.revision);
        if !current_matches {
            return Ok(());
        }
        self.slot_mut(data.selection).owned = None;
        self.emit(BackendEvent::OwnershipLost {
            backend: BackendId::Wayland,
            selection: data.selection,
            revision: data.revision,
        })
    }

    fn cancel_transfers(&mut self) {
        for selection in [SelectionKind::Clipboard, SelectionKind::Primary] {
            if let Some(cancelled) = self.slot_mut(selection).read_cancel.take() {
                cancelled.store(true, Ordering::Release);
            }
            if let Some(owned) = self.slot_mut(selection).owned.take() {
                owned.cancelled.store(true, Ordering::Release);
            }
        }
        self.workers.cancel_all();
    }

    fn next_epoch(&mut self, selection: SelectionKind) -> Result<BackendEpoch, ProtocolError> {
        let next =
            self.slot(selection).epoch.checked_next().ok_or_else(|| {
                ProtocolError::invalid_state("wayland-selection", "epoch overflow")
            })?;
        self.slot_mut(selection).epoch = next;
        Ok(next)
    }

    fn next_token(&mut self, selection: SelectionKind) -> Result<OfferToken, ProtocolError> {
        let next =
            self.slot(selection).token.checked_next().ok_or_else(|| {
                ProtocolError::invalid_state("wayland-selection", "token overflow")
            })?;
        self.slot_mut(selection).token = next;
        Ok(next)
    }

    fn emit_ownership_failed(
        &self,
        selection: SelectionKind,
        command_id: CommandId,
        revision: Revision,
        detail: impl Into<String>,
    ) -> Result<(), ProtocolError> {
        self.emit(BackendEvent::OwnershipFailed {
            backend: BackendId::Wayland,
            selection,
            command_id,
            revision,
            error: ProtocolError::invalid_state("wayland-set-selection", detail),
        })
    }

    fn emit(&self, event: BackendEvent) -> Result<(), ProtocolError> {
        self.event_tx
            .blocking_send(event)
            .map_err(|_| ProtocolError::disconnected("coordinator event receiver closed"))
    }

    fn check_fatal(&mut self) -> Result<(), ProtocolError> {
        match self.fatal.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn record(&mut self, result: Result<(), ProtocolError>) {
        if let Err(error) = result {
            self.fatal.get_or_insert(error);
        }
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
    initial_pending: bool,
    active_offer: Option<ObjectId>,
    read_cancel: Option<Arc<AtomicBool>>,
    owned: Option<OwnedSource>,
    last_command_id: CommandId,
}

impl SelectionSlot {
    fn new() -> Self {
        Self {
            initial_pending: true,
            ..Self::default()
        }
    }

    fn transfer_is_current(&self, epoch: BackendEpoch, token: OfferToken) -> bool {
        self.epoch == epoch && self.token == token
    }
}

struct OfferRecord {
    offer: ProviderOffer,
    mime_types: Vec<String>,
}

struct OwnedSource {
    source: ProviderSource,
    revision: Revision,
    cancelled: Arc<AtomicBool>,
}

struct PreparedSource {
    command_id: CommandId,
    selection: SelectionKind,
    revision: Revision,
    source: ProviderSource,
    cancelled: Arc<AtomicBool>,
}

#[derive(Clone)]
pub(super) struct SourceData {
    selection: SelectionKind,
    revision: Revision,
    payload: TextPayload,
    cancelled: Arc<AtomicBool>,
}

enum Work {
    Read {
        selection: SelectionKind,
        epoch: BackendEpoch,
        token: OfferToken,
        initial: bool,
        fd: std::os::fd::OwnedFd,
        cancelled: Arc<AtomicBool>,
    },
    Write {
        selection: SelectionKind,
        payload: TextPayload,
        fd: std::os::fd::OwnedFd,
        cancelled: Arc<AtomicBool>,
    },
}

enum Completion {
    Read {
        selection: SelectionKind,
        epoch: BackendEpoch,
        token: OfferToken,
        initial: bool,
        result: Result<TextPayload, TransferError>,
    },
    Write {
        selection: SelectionKind,
        result: Result<(), TransferError>,
    },
}

struct WorkerPool {
    jobs: Option<std_mpsc::SyncSender<Work>>,
    completions: std_mpsc::Receiver<Completion>,
    shutdown: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
}

impl WorkerPool {
    fn new() -> Result<Self, ProtocolError> {
        let (job_tx, job_rx) = std_mpsc::sync_channel(WORK_QUEUE_CAPACITY);
        let (completion_tx, completion_rx) = std_mpsc::sync_channel(COMPLETION_CAPACITY);
        let shared_rx = Arc::new(Mutex::new(job_rx));
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::with_capacity(WORKER_COUNT);
        for index in 0..WORKER_COUNT {
            let jobs = shared_rx.clone();
            let completions = completion_tx.clone();
            let stopping = shutdown.clone();
            match thread::Builder::new()
                .name(format!("clip-bridge-pipe-{index}"))
                .spawn(move || worker_loop(jobs, completions, stopping))
            {
                Ok(worker) => threads.push(worker),
                Err(error) => {
                    shutdown.store(true, Ordering::Release);
                    drop(job_tx);
                    for worker in threads {
                        if worker.join().is_err() {
                            warn!("Wayland worker panicked while initialization was unwinding");
                        }
                    }
                    return Err(ProtocolError::operation(
                        "wayland-create-worker-thread",
                        error,
                    ));
                }
            }
        }
        Ok(Self {
            jobs: Some(job_tx),
            completions: completion_rx,
            shutdown,
            threads,
        })
    }

    fn submit(&self, work: Work) -> Result<(), Work> {
        let Some(jobs) = &self.jobs else {
            return Err(work);
        };
        jobs.try_send(work).map_err(|error| match error {
            std_mpsc::TrySendError::Full(work) | std_mpsc::TrySendError::Disconnected(work) => work,
        })
    }

    fn try_completion(&self) -> Result<Option<Completion>, ProtocolError> {
        match self.completions.try_recv() {
            Ok(completion) => Ok(Some(completion)),
            Err(std_mpsc::TryRecvError::Empty) => Ok(None),
            Err(std_mpsc::TryRecvError::Disconnected) => Err(ProtocolError::disconnected(
                "all Wayland transfer workers exited",
            )),
        }
    }

    fn cancel_all(&self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.jobs.take();
        for worker in self.threads.drain(..) {
            if worker.join().is_err() {
                warn!("Wayland worker panicked during shutdown");
            }
        }
    }
}

fn worker_loop(
    jobs: Arc<Mutex<std_mpsc::Receiver<Work>>>,
    completions: std_mpsc::SyncSender<Completion>,
    shutdown: Arc<AtomicBool>,
) {
    loop {
        let job = match jobs.lock() {
            Ok(receiver) => receiver.recv(),
            Err(_) => return,
        };
        let Ok(job) = job else {
            return;
        };
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        let completion = match job {
            Work::Read {
                selection,
                epoch,
                token,
                initial,
                fd,
                cancelled,
            } => Completion::Read {
                selection,
                epoch,
                token,
                initial,
                result: read_pipe(fd, &cancelled),
            },
            Work::Write {
                selection,
                payload,
                fd,
                cancelled,
            } => Completion::Write {
                selection,
                result: write_pipe(fd, &payload, &cancelled),
            },
        };
        match completions.try_send(completion) {
            Ok(()) => {}
            Err(std_mpsc::TrySendError::Disconnected(_)) => return,
            Err(std_mpsc::TrySendError::Full(_)) => {
                // Capacity covers every active and queued job, so this can only occur if
                // the worker accounting invariant is broken. End the worker without panic.
                return;
            }
        }
    }
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

impl Dispatch<wl_registry::WlRegistry, RegistryData> for WaylandState {
    fn event(
        state: &mut Self,
        _registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &RegistryData,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "ext_data_control_manager_v1" if state.globals.ext_manager.is_none() => {
                    state.globals.ext_manager = Some((name, version));
                }
                "zwlr_data_control_manager_v1" if state.globals.wlr_manager.is_none() => {
                    state.globals.wlr_manager = Some((name, version));
                }
                "wl_seat" => state.globals.seats.push((name, version)),
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name }
                if state.provider_global == Some(name) || state.seat_global == Some(name) =>
            {
                state.fatal = Some(ProtocolError::disconnected(format!(
                    "required Wayland global {name} was removed"
                )));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Name { name } = event {
            info!(seat = name, "selected Wayland seat");
        }
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtDataControlManagerV1,
        _event: ext_data_control_manager_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlManagerV1,
        _event: zwlr_data_control_manager_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _device: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        let result = match event {
            ext_data_control_device_v1::Event::DataOffer { id } => {
                state.offer_created(ProviderOffer::Ext(id));
                Ok(())
            }
            ext_data_control_device_v1::Event::Selection { id } => {
                state.selection_event(SelectionKind::Clipboard, id.map(ProviderOffer::Ext))
            }
            ext_data_control_device_v1::Event::PrimarySelection { id } => {
                state.selection_event(SelectionKind::Primary, id.map(ProviderOffer::Ext))
            }
            ext_data_control_device_v1::Event::Finished => Err(ProtocolError::disconnected(
                "ext data-control device was finished",
            )),
            _ => Ok(()),
        };
        state.record(result);
    }

    event_created_child!(WaylandState, ExtDataControlDeviceV1, [
        0 => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _device: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        let result = match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                state.offer_created(ProviderOffer::Wlr(id));
                Ok(())
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                state.selection_event(SelectionKind::Clipboard, id.map(ProviderOffer::Wlr))
            }
            zwlr_data_control_device_v1::Event::PrimarySelection { id } => {
                state.selection_event(SelectionKind::Primary, id.map(ProviderOffer::Wlr))
            }
            zwlr_data_control_device_v1::Event::Finished => Err(ProtocolError::disconnected(
                "wlr data-control device was finished",
            )),
            _ => Ok(()),
        };
        state.record(result);
    }

    event_created_child!(WaylandState, ZwlrDataControlDeviceV1, [
        0 => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offer_mime(offer.id(), mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offer_mime(offer.id(), mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, SourceData> for WaylandState {
    fn event(
        state: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        data: &SourceData,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        let result = match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                state.source_send(data, mime_type, fd)
            }
            ext_data_control_source_v1::Event::Cancelled => {
                let result = state.source_cancelled(source.id(), data);
                source.destroy();
                result
            }
            _ => Ok(()),
        };
        state.record(result);
    }
}

impl Dispatch<ZwlrDataControlSourceV1, SourceData> for WaylandState {
    fn event(
        state: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        data: &SourceData,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        let result = match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                state.source_send(data, mime_type, fd)
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                let result = state.source_cancelled(source.id(), data);
                source.destroy();
                result
            }
            _ => Ok(()),
        };
        state.record(result);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        process::Command,
        thread,
        time::{Duration, Instant},
    };

    use super::*;

    #[test]
    fn selection_slots_start_in_snapshot_mode() {
        let clipboard = SelectionSlot::new();
        let primary = SelectionSlot::new();
        assert!(clipboard.initial_pending);
        assert!(primary.initial_pending);
    }

    #[test]
    fn selection_slot_rejects_stale_worker_completions() {
        let mut slot = SelectionSlot::new();
        slot.epoch = BackendEpoch::new(3);
        slot.token = OfferToken::new(5);

        assert!(slot.transfer_is_current(BackendEpoch::new(3), OfferToken::new(5)));
        assert!(!slot.transfer_is_current(BackendEpoch::new(2), OfferToken::new(5)));
        assert!(!slot.transfer_is_current(BackendEpoch::new(3), OfferToken::new(4)));
    }

    #[test]
    fn worker_pool_bounds_active_and_queued_transfers() {
        let pool = WorkerPool::new().expect("create bounded worker pool");
        let total_capacity = WORKER_COUNT + WORK_QUEUE_CAPACITY;
        assert_eq!(COMPLETION_CAPACITY, total_capacity);

        let mut writers = Vec::with_capacity(total_capacity + 1);
        let mut cancellations = Vec::with_capacity(total_capacity + 1);
        let deadline = Instant::now() + Duration::from_secs(1);
        for sequence in 0..total_capacity {
            let (read_fd, write_fd) = nix::unistd::pipe().expect("create blocking worker pipe");
            let cancelled = cancellation();
            let mut pending = Some(Work::Read {
                selection: if sequence % 2 == 0 {
                    SelectionKind::Clipboard
                } else {
                    SelectionKind::Primary
                },
                epoch: BackendEpoch::new(1),
                token: OfferToken::new(sequence as u64 + 1),
                initial: false,
                fd: read_fd,
                cancelled: cancelled.clone(),
            });
            loop {
                let work = pending.take().expect("rejected work is retained for retry");
                match pool.submit(work) {
                    Ok(()) => break,
                    Err(work) if Instant::now() < deadline => {
                        pending = Some(work);
                        thread::yield_now();
                    }
                    Err(_) => panic!("workers did not consume their bounded active slots"),
                }
            }
            writers.push(write_fd);
            cancellations.push(cancelled);
        }

        let (extra_read, extra_write) = nix::unistd::pipe().expect("create overflow worker pipe");
        let extra_cancelled = cancellation();
        assert!(
            pool.submit(Work::Read {
                selection: SelectionKind::Clipboard,
                epoch: BackendEpoch::new(1),
                token: OfferToken::new(total_capacity as u64 + 1),
                initial: false,
                fd: extra_read,
                cancelled: extra_cancelled.clone(),
            })
            .is_err()
        );

        cancellations.push(extra_cancelled);
        for cancelled in cancellations {
            cancelled.store(true, Ordering::Release);
        }
        writers.push(extra_write);
        drop(writers);
        pool.cancel_all();
    }

    #[test]
    fn wlr_primary_capability_depends_on_bound_version() {
        fn capability(version: u32) -> bool {
            version >= 2
        }
        assert!(!capability(1));
        assert!(capability(2));
    }

    #[test]
    fn ext_provider_wins_independent_of_registry_order() {
        let globals = Globals {
            ext_manager: Some((8, 4)),
            wlr_manager: Some((2, 2)),
            seats: vec![],
        };
        assert_eq!(
            choose_provider(&globals).expect("a provider is advertised"),
            ProviderChoice::Ext {
                name: 8,
                version: 1,
            }
        );
    }

    #[test]
    fn provider_choice_falls_back_to_bounded_wlr_version() {
        let globals = Globals {
            ext_manager: None,
            wlr_manager: Some((3, 9)),
            seats: vec![],
        };
        assert_eq!(
            choose_provider(&globals).expect("wlr fallback is advertised"),
            ProviderChoice::Wlr {
                name: 3,
                version: 2,
            }
        );
    }

    #[test]
    fn missing_provider_is_a_startup_error() {
        assert!(choose_provider(&Globals::default()).is_err());
    }

    #[test]
    #[ignore = "run through scripts/test-wayland-kwin.sh"]
    fn kwin_ext_actor_receives_and_serves_both_selections() {
        compositor_actor_receives_and_serves_both_selections(false);
    }

    #[test]
    #[ignore = "run through scripts/test-wayland-niri-wlr.sh"]
    fn wlr_session_actor_receives_and_serves_both_selections() {
        compositor_actor_receives_and_serves_both_selections(true);
    }

    fn compositor_actor_receives_and_serves_both_selections(force_wlr: bool) {
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (clipboard_tx, clipboard_rx) = watch::channel(None);
        let (primary_tx, primary_rx) = watch::channel(None);
        let actor = thread::spawn(move || {
            if force_wlr {
                run_forced_wlr(event_tx, clipboard_rx, primary_rx)
            } else {
                run(event_tx, clipboard_rx, primary_rx)
            }
        });

        let mut snapshots = 0;
        while snapshots < 2 {
            match recv_wire_event(&mut event_rx) {
                BackendEvent::Ready {
                    backend: BackendId::Wayland,
                    capabilities,
                } => {
                    assert!(capabilities.clipboard);
                    assert!(capabilities.primary);
                }
                BackendEvent::InitialSnapshot {
                    backend: BackendId::Wayland,
                    epoch,
                    ..
                } => {
                    assert_eq!(epoch, BackendEpoch::new(1));
                    snapshots += 1;
                }
                event => panic!("unexpected Wayland startup event: {event:?}"),
            }
        }

        exercise_selection(
            &mut event_rx,
            &clipboard_tx,
            SelectionKind::Clipboard,
            "external Clipboard 雪",
            "bridge Clipboard 桥",
            1,
        );
        exercise_selection(
            &mut event_rx,
            &primary_tx,
            SelectionKind::Primary,
            "external Primary 甲",
            "bridge Primary 乙",
            2,
        );

        clipboard_tx.send_replace(Some(BackendCommand::Shutdown));
        primary_tx.send_replace(Some(BackendCommand::Shutdown));
        actor
            .join()
            .expect("Wayland actor thread does not panic")
            .expect("Wayland actor shuts down cleanly");
    }

    fn run_forced_wlr(
        event_tx: mpsc::Sender<BackendEvent>,
        clipboard_commands: watch::Receiver<Option<BackendCommand>>,
        primary_commands: watch::Receiver<Option<BackendCommand>>,
    ) -> Result<(), ProtocolError> {
        let fatal_tx = event_tx.clone();
        let result = Connection::connect_to_env()
            .map_err(|error| ProtocolError::operation("wayland-connect", error))
            .and_then(|connection| {
                run_connection_with_provider(
                    connection,
                    event_tx,
                    clipboard_commands,
                    primary_commands,
                    choose_wlr_for_wire_test,
                )
            });
        if let Err(error) = &result {
            let _ = fatal_tx.blocking_send(BackendEvent::FatalError {
                backend: BackendId::Wayland,
                error: error.clone(),
            });
        }
        result
    }

    fn choose_wlr_for_wire_test(globals: &Globals) -> Result<ProviderChoice, ProtocolError> {
        let (name, advertised_version) = globals.wlr_manager.ok_or_else(|| {
            ProtocolError::invalid_state(
                "wayland-registry",
                "wire test compositor does not advertise wlr-data-control-v1",
            )
        })?;
        Ok(ProviderChoice::Wlr {
            name,
            version: advertised_version.min(2),
        })
    }

    fn exercise_selection(
        events: &mut mpsc::Receiver<BackendEvent>,
        commands: &watch::Sender<Option<BackendCommand>>,
        selection: SelectionKind,
        external_text: &str,
        bridge_text: &str,
        command_id: u64,
    ) {
        wl_copy(selection, external_text);
        let epoch = wait_for_observed(events, selection, external_text);

        let payload =
            TextPayload::from_string(bridge_text.to_owned()).expect("wire test text is valid");
        commands.send_replace(Some(BackendCommand::SetText {
            command_id: CommandId::new(command_id),
            selection,
            revision: Revision::new(command_id),
            expected_target_epoch: epoch,
            payload,
        }));
        loop {
            if matches!(
                recv_wire_event(events),
                BackendEvent::OwnershipApplied {
                    backend: BackendId::Wayland,
                    selection: applied_selection,
                    command_id: applied_command,
                    ..
                } if applied_selection == selection && applied_command == CommandId::new(command_id)
            ) {
                break;
            }
        }
        let echo_epoch = wait_for_observed(events, selection, bridge_text);

        let second_command_id = command_id + 10;
        let second_text = format!("{bridge_text} replacement");
        commands.send_replace(Some(BackendCommand::SetText {
            command_id: CommandId::new(second_command_id),
            selection,
            revision: Revision::new(second_command_id),
            expected_target_epoch: echo_epoch,
            payload: TextPayload::from_string(second_text.clone())
                .expect("replacement wire test text is valid"),
        }));
        loop {
            match recv_wire_event(events) {
                BackendEvent::OwnershipApplied {
                    backend: BackendId::Wayland,
                    selection: applied_selection,
                    command_id: applied_command,
                    ..
                } if applied_selection == selection
                    && applied_command == CommandId::new(second_command_id) =>
                {
                    break;
                }
                BackendEvent::OwnershipLost {
                    selection: lost_selection,
                    revision,
                    ..
                } if lost_selection == selection && revision == Revision::new(command_id) => {
                    panic!("late cancellation of the old source cleared replacement ownership");
                }
                _ => {}
            }
        }
        assert_eq!(wl_paste(selection), second_text.as_bytes());

        let replacement = format!("{external_text} replacement");
        wl_copy(selection, &replacement);
        let mut ownership_lost = false;
        let mut replacement_seen = false;
        while !ownership_lost || !replacement_seen {
            match recv_wire_event(events) {
                BackendEvent::OwnershipLost {
                    backend: BackendId::Wayland,
                    selection: lost_selection,
                    revision,
                } if lost_selection == selection
                    && revision == Revision::new(second_command_id) =>
                {
                    ownership_lost = true;
                }
                BackendEvent::ObservedText {
                    backend: BackendId::Wayland,
                    selection: observed_selection,
                    payload,
                    ..
                } if observed_selection == selection && payload.as_str() == replacement => {
                    replacement_seen = true;
                }
                _ => {}
            }
        }

        wl_clear(selection);
        loop {
            if matches!(
                recv_wire_event(events),
                BackendEvent::SelectionUnavailable {
                    backend: BackendId::Wayland,
                    selection: cleared_selection,
                    reason: UnavailableReason::Cleared,
                    ..
                } if cleared_selection == selection
            ) {
                break;
            }
        }
    }

    fn wait_for_observed(
        events: &mut mpsc::Receiver<BackendEvent>,
        selection: SelectionKind,
        expected: &str,
    ) -> BackendEpoch {
        loop {
            if let BackendEvent::ObservedText {
                backend: BackendId::Wayland,
                selection: observed_selection,
                epoch,
                payload,
                ..
            } = recv_wire_event(events)
                && observed_selection == selection
                && payload.as_str() == expected
            {
                return epoch;
            }
        }
    }

    fn wl_copy(selection: SelectionKind, text: &str) {
        let mut command = Command::new("wl-copy");
        if selection == SelectionKind::Primary {
            command.arg("--primary");
        }
        let status = command
            .args(["--type", "text/plain;charset=utf-8", text])
            .status()
            .expect("start wl-copy in isolated KWin session");
        assert!(status.success());
    }

    fn wl_paste(selection: SelectionKind) -> Vec<u8> {
        let mut command = Command::new("wl-paste");
        if selection == SelectionKind::Primary {
            command.arg("--primary");
        }
        let output = command
            .args(["--no-newline", "--type", "text/plain"])
            .output()
            .expect("start wl-paste in isolated KWin session");
        assert!(output.status.success());
        output.stdout
    }

    fn wl_clear(selection: SelectionKind) {
        let mut command = Command::new("wl-copy");
        if selection == SelectionKind::Primary {
            command.arg("--primary");
        }
        let status = command
            .arg("--clear")
            .status()
            .expect("clear selection in isolated KWin session");
        assert!(status.success());
    }

    fn recv_wire_event(receiver: &mut mpsc::Receiver<BackendEvent>) -> BackendEvent {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match receiver.try_recv() {
                Ok(event) => return event,
                Err(mpsc::error::TryRecvError::Empty) if std::time::Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("timed out waiting for Wayland backend event: {error}"),
            }
        }
    }
}
