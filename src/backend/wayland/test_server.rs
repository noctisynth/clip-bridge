use std::{
    fs::File,
    io::{Read, Write},
    os::fd::AsFd,
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use nix::poll::{PollFd, PollFlags, poll};
use tokio::sync::{mpsc, watch};
use wayland_client::Connection;
use wayland_protocols_wlr::data_control::v1::server::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::{self, ZwlrDataControlManagerV1},
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New, Resource,
    protocol::wl_seat::{self, WlSeat},
};

use crate::{
    backend::{BackendCommand, BackendEvent},
    domain::{
        BackendCapabilities, BackendEpoch, BackendId, CommandId, Revision, SelectionKind,
        SnapshotOutcome, TextPayload,
    },
};

use super::run_connection;

struct ServerState {
    initial_clipboard: Option<Arc<[u8]>>,
    served_tx: std_mpsc::Sender<(SelectionKind, Vec<u8>)>,
}

#[derive(Clone)]
struct OfferData {
    bytes: Arc<[u8]>,
}

impl GlobalDispatch<WlSeat, ()> for ServerState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WlSeat>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WlSeat, ()> for ServerState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WlSeat,
        _request: wl_seat::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<ZwlrDataControlManagerV1, ()> for ServerState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrDataControlManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for ServerState {
    fn request(
        _state: &mut Self,
        client: &Client,
        manager: &ZwlrDataControlManagerV1,
        request: zwlr_data_control_manager_v1::Request,
        _data: &(),
        handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_data_control_manager_v1::Request::CreateDataSource { id } => {
                data_init.init(id, ());
            }
            zwlr_data_control_manager_v1::Request::GetDataDevice { id, .. } => {
                let device = data_init.init(id, ());
                if let Some(bytes) = _state.initial_clipboard.clone() {
                    let offer = client
                        .create_resource::<ZwlrDataControlOfferV1, OfferData, ServerState>(
                            handle,
                            1,
                            OfferData { bytes },
                        )
                        .expect("create server-side wlr data offer");
                    device.data_offer(&offer);
                    offer.offer("text/plain;charset=utf-8".to_owned());
                    device.selection(Some(&offer));
                } else {
                    device.selection(None);
                }
                if manager.version() >= 2 {
                    device.primary_selection(None);
                }
            }
            zwlr_data_control_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for ServerState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrDataControlDeviceV1,
        request: zwlr_data_control_device_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_data_control_device_v1::Request::SetSelection { source } => {
                request_source(state, source, SelectionKind::Clipboard);
            }
            zwlr_data_control_device_v1::Request::SetPrimarySelection { source } => {
                request_source(state, source, SelectionKind::Primary);
            }
            zwlr_data_control_device_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for ServerState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwlrDataControlSourceV1,
        _request: zwlr_data_control_source_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlOfferV1, OfferData> for ServerState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwlrDataControlOfferV1,
        request: zwlr_data_control_offer_v1::Request,
        data: &OfferData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwlr_data_control_offer_v1::Request::Receive { fd, .. } = request {
            let mut file = File::from(fd);
            file.write_all(&data.bytes)
                .expect("write wlr test offer payload");
        }
    }
}

fn request_source(
    state: &ServerState,
    source: Option<ZwlrDataControlSourceV1>,
    selection: SelectionKind,
) {
    let Some(source) = source else {
        return;
    };
    let (read_fd, write_fd) = nix::unistd::pipe().expect("create wlr source test pipe");
    source.send("text/plain".to_owned(), write_fd.as_fd());
    drop(write_fd);
    let served_tx = state.served_tx.clone();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        File::from(read_fd)
            .read_to_end(&mut bytes)
            .expect("read complete wlr source payload");
        served_tx
            .send((selection, bytes))
            .expect("wlr source result receiver remains active");
    });
}

struct TestServer {
    connection: Option<Connection>,
    served_rx: std_mpsc::Receiver<(SelectionKind, Vec<u8>)>,
    stopping: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<Result<(), String>>>,
}

impl TestServer {
    fn start(wlr_version: Option<u32>, initial_clipboard: Option<&str>) -> Self {
        let (server_socket, client_socket) =
            UnixStream::pair().expect("create Wayland test socket pair");
        let mut display = Display::<ServerState>::new().expect("create Wayland test display");
        let mut handle = display.handle();
        handle
            .insert_client(server_socket, Arc::new(()))
            .expect("insert Wayland test client");
        handle.create_global::<ServerState, WlSeat, _>(9, ());
        if let Some(version) = wlr_version {
            handle.create_global::<ServerState, ZwlrDataControlManagerV1, _>(version, ());
        }

        let stopping = Arc::new(AtomicBool::new(false));
        let thread_stopping = stopping.clone();
        let (served_tx, served_rx) = std_mpsc::channel();
        let initial_clipboard = initial_clipboard.map(|text| Arc::<[u8]>::from(text.as_bytes()));
        let server_thread = thread::spawn(move || {
            let mut state = ServerState {
                initial_clipboard,
                served_tx,
            };
            while !thread_stopping.load(Ordering::Acquire) {
                let readable = {
                    let mut poll_fds = [PollFd::new(
                        display.as_fd(),
                        PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP,
                    )];
                    poll(&mut poll_fds, 100_u16)
                        .map_err(|error| format!("poll Wayland test display: {error}"))?;
                    poll_fds[0]
                        .revents()
                        .is_some_and(|events| events.contains(PollFlags::POLLIN))
                };
                if readable {
                    display
                        .dispatch_clients(&mut state)
                        .map_err(|error| format!("dispatch Wayland test client: {error}"))?;
                }
                display
                    .flush_clients()
                    .map_err(|error| format!("flush Wayland test client: {error}"))?;
            }
            Ok(())
        });
        Self {
            connection: Some(
                Connection::from_socket(client_socket).expect("connect to Wayland test display"),
            ),
            served_rx,
            stopping,
            thread: Some(server_thread),
        }
    }

    fn take_connection(&mut self) -> Connection {
        self.connection
            .take()
            .expect("test connection is taken once")
    }

    fn recv_served(&self) -> (SelectionKind, Vec<u8>) {
        self.served_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("receive wlr source payload")
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(server_thread) = self.thread.take() {
            server_thread
                .join()
                .expect("Wayland test server does not panic")
                .expect("Wayland test server exits cleanly");
        }
    }
}

fn recv_event(receiver: &mut mpsc::Receiver<BackendEvent>) -> Result<BackendEvent, String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match receiver.try_recv() {
            Ok(event) => return Ok(event),
            Err(mpsc::error::TryRecvError::Empty) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(format!("timed out waiting for Wayland test event: {error}")),
        }
    }
}

#[test]
fn wlr_wire_versions_gate_primary_capability() {
    for (version, expected_capabilities, expected_snapshots) in [
        (
            1,
            BackendCapabilities {
                clipboard: true,
                primary: false,
            },
            1,
        ),
        (2, BackendCapabilities::text_bridge(), 2),
    ] {
        let mut server = TestServer::start(Some(version), None);
        let connection = server.take_connection();
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (clipboard_tx, clipboard_rx) = watch::channel(None);
        let (primary_tx, primary_rx) = watch::channel(None);
        let actor =
            thread::spawn(move || run_connection(connection, event_tx, clipboard_rx, primary_rx));

        let ready = match recv_event(&mut event_rx) {
            Ok(event) => event,
            Err(receive_error) => {
                let actor_error = actor
                    .join()
                    .expect("Wayland actor does not panic")
                    .expect_err("closed event channel means the actor failed");
                if actor_error.to_string().contains("Operation not permitted") {
                    eprintln!("skipping Wayland socket-pair tests: {actor_error}");
                    return;
                }
                panic!("{receive_error}; actor failed: {actor_error}");
            }
        };
        assert!(matches!(
            ready,
            BackendEvent::Ready {
                backend: BackendId::Wayland,
                capabilities,
            } if capabilities == expected_capabilities
        ));
        for _ in 0..expected_snapshots {
            assert!(matches!(
                recv_event(&mut event_rx).expect("receive initial Wayland snapshot"),
                BackendEvent::InitialSnapshot {
                    backend: BackendId::Wayland,
                    epoch,
                    outcome: SnapshotOutcome::Empty,
                    ..
                } if epoch == BackendEpoch::new(1)
            ));
        }

        clipboard_tx.send_replace(Some(BackendCommand::Shutdown));
        primary_tx.send_replace(Some(BackendCommand::Shutdown));
        actor
            .join()
            .expect("Wayland actor does not panic")
            .expect("Wayland actor shuts down cleanly");
    }
}

#[test]
fn missing_data_control_provider_is_a_startup_error() {
    let mut server = TestServer::start(None, None);
    let connection = server.take_connection();
    let (event_tx, _event_rx) = mpsc::channel(4);
    let (_clipboard_tx, clipboard_rx) = watch::channel(None);
    let (_primary_tx, primary_rx) = watch::channel(None);
    let error = run_connection(connection, event_tx, clipboard_rx, primary_rx)
        .expect_err("missing data-control provider must fail startup");
    if error.to_string().contains("Operation not permitted") {
        eprintln!("skipping Wayland socket-pair tests: {error}");
        return;
    }
    assert!(error.to_string().contains("neither ext-data-control-v1"));
}

#[test]
fn wlr_wire_reads_offer_and_serves_both_sources() {
    let external_text = "wlr external offer 雪";
    let mut server = TestServer::start(Some(2), Some(external_text));
    let connection = server.take_connection();
    let (event_tx, mut event_rx) = mpsc::channel(32);
    let (clipboard_tx, clipboard_rx) = watch::channel(None);
    let (primary_tx, primary_rx) = watch::channel(None);
    let actor =
        thread::spawn(move || run_connection(connection, event_tx, clipboard_rx, primary_rx));

    let ready = match recv_event(&mut event_rx) {
        Ok(event) => event,
        Err(receive_error) => {
            let actor_error = actor
                .join()
                .expect("Wayland actor does not panic")
                .expect_err("closed event channel means the actor failed");
            if actor_error.to_string().contains("Operation not permitted") {
                eprintln!("skipping Wayland socket-pair tests: {actor_error}");
                return;
            }
            panic!("{receive_error}; actor failed: {actor_error}");
        }
    };
    assert!(matches!(
        ready,
        BackendEvent::Ready {
            capabilities,
            ..
        } if capabilities == BackendCapabilities::text_bridge()
    ));

    let mut clipboard_snapshot = false;
    let mut primary_snapshot = false;
    while !clipboard_snapshot || !primary_snapshot {
        match recv_event(&mut event_rx).expect("receive wlr initial snapshot") {
            BackendEvent::InitialSnapshot {
                selection: SelectionKind::Clipboard,
                epoch,
                outcome: SnapshotOutcome::Text(payload),
                ..
            } => {
                assert_eq!(epoch, BackendEpoch::new(1));
                assert_eq!(payload.as_str(), external_text);
                clipboard_snapshot = true;
            }
            BackendEvent::InitialSnapshot {
                selection: SelectionKind::Primary,
                outcome: SnapshotOutcome::Empty,
                ..
            } => primary_snapshot = true,
            event => panic!("unexpected wlr startup event: {event:?}"),
        }
    }

    for (selection, command_id, text) in [
        (SelectionKind::Clipboard, 1, "wlr Clipboard source 桥"),
        (SelectionKind::Primary, 2, "wlr Primary source 甲"),
    ] {
        let sender = match selection {
            SelectionKind::Clipboard => &clipboard_tx,
            SelectionKind::Primary => &primary_tx,
        };
        sender.send_replace(Some(BackendCommand::SetText {
            command_id: CommandId::new(command_id),
            selection,
            revision: Revision::new(command_id),
            expected_target_epoch: BackendEpoch::new(1),
            payload: TextPayload::from_string(text.to_owned()).expect("test payload is valid"),
        }));
        loop {
            if matches!(
                recv_event(&mut event_rx).expect("receive wlr ownership event"),
                BackendEvent::OwnershipApplied {
                    selection: applied,
                    command_id: applied_id,
                    ..
                } if applied == selection && applied_id == CommandId::new(command_id)
            ) {
                break;
            }
        }
        let (served_selection, bytes) = server.recv_served();
        assert_eq!(served_selection, selection);
        assert_eq!(bytes, text.as_bytes());
    }

    clipboard_tx.send_replace(Some(BackendCommand::Shutdown));
    primary_tx.send_replace(Some(BackendCommand::Shutdown));
    actor
        .join()
        .expect("Wayland actor does not panic")
        .expect("Wayland actor shuts down cleanly");
}

#[test]
fn actor_reports_wayland_server_disconnect() {
    let mut server = TestServer::start(Some(2), None);
    let connection = server.take_connection();
    let (event_tx, mut event_rx) = mpsc::channel(8);
    let (_clipboard_tx, clipboard_rx) = watch::channel(None);
    let (_primary_tx, primary_rx) = watch::channel(None);
    let actor =
        thread::spawn(move || run_connection(connection, event_tx, clipboard_rx, primary_rx));

    let ready = match recv_event(&mut event_rx) {
        Ok(event) => event,
        Err(receive_error) => {
            let actor_error = actor
                .join()
                .expect("Wayland actor does not panic")
                .expect_err("closed event channel means the actor failed");
            if actor_error.to_string().contains("Operation not permitted") {
                eprintln!("skipping Wayland socket-pair tests: {actor_error}");
                return;
            }
            panic!("{receive_error}; actor failed: {actor_error}");
        }
    };
    assert!(matches!(ready, BackendEvent::Ready { .. }));
    recv_event(&mut event_rx).expect("receive Clipboard snapshot");
    recv_event(&mut event_rx).expect("receive Primary snapshot");

    drop(server);
    let error = actor
        .join()
        .expect("Wayland actor does not panic")
        .expect_err("server disconnect must stop the Wayland actor");
    assert!(matches!(
        error,
        crate::domain::ProtocolError::Disconnected { .. }
    ));
}
