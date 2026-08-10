use std::{
    collections::HashMap,
    env,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use thiserror::Error;
use tokio::{
    sync::{mpsc, watch},
    task::{Id, JoinHandle, JoinSet},
};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::{
    backend::{BackendCommand, BackendEvent},
    coordinator::{BridgeState, CoordinatorEffect, CoordinatorError},
    domain::{BackendId, ProtocolError, SelectionKind, ShutdownError, StartupError},
};

const BACKEND_EVENT_CAPACITY: usize = 32;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(35);

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error(transparent)]
    Startup(#[from] StartupError),
    #[error(transparent)]
    Coordinator(#[from] CoordinatorError),
    #[error("{backend} backend failed: {source}")]
    Backend {
        backend: BackendId,
        #[source]
        source: ProtocolError,
    },
    #[error("{backend} backend exited without a shutdown request")]
    UnexpectedBackendExit { backend: BackendId },
    #[error("{backend} actor panicked: {detail}")]
    ActorJoin { backend: BackendId, detail: String },
    #[error("coordinator task panicked: {detail}")]
    CoordinatorJoin { detail: String },
    #[error("coordinator exited while backend event senders were still active")]
    UnexpectedCoordinatorExit,
    #[error("failed to install shutdown signal handler: {detail}")]
    Signal { detail: String },
    #[error(transparent)]
    Shutdown(#[from] ShutdownError),
}

type ActorResult = (BackendId, Result<(), ProtocolError>);

pub async fn run() -> Result<(), BridgeError> {
    init_tracing()?;
    info!("Starting X11 <-> Wayland clipboard bridge");

    let (backend_event_tx, backend_event_rx) = mpsc::channel(BACKEND_EVENT_CAPACITY);
    let (mailboxes, receivers) = CommandMailboxes::new();
    let startup_complete = Arc::new(AtomicBool::new(false));
    let mut actors = JoinSet::new();
    let mut actor_ids = HashMap::new();

    let x11_event_tx = backend_event_tx.clone();
    let x11_abort = actors.spawn_blocking(move || {
        (
            BackendId::X11,
            crate::backend::x11::run(x11_event_tx, receivers.x11_clipboard, receivers.x11_primary),
        )
    });
    actor_ids.insert(x11_abort.id(), BackendId::X11);

    let wayland_abort = actors.spawn_blocking(move || {
        (
            BackendId::Wayland,
            crate::backend::wayland::run(
                backend_event_tx,
                receivers.wayland_clipboard,
                receivers.wayland_primary,
            ),
        )
    });
    actor_ids.insert(wayland_abort.id(), BackendId::Wayland);

    let coordinator = tokio::spawn(run_coordinator(
        backend_event_rx,
        mailboxes.clone(),
        startup_complete.clone(),
    ));
    supervise(
        actors,
        actor_ids,
        coordinator,
        mailboxes,
        startup_complete,
        shutdown_signal(),
        SHUTDOWN_TIMEOUT,
    )
    .await
}

fn init_tracing() -> Result<(), StartupError> {
    let filter = match env::var("RUST_LOG") {
        Ok(value) => EnvFilter::try_new(value).map_err(|error| StartupError::LoggingFilter {
            detail: error.to_string(),
        })?,
        Err(env::VarError::NotPresent) => EnvFilter::new("info"),
        Err(env::VarError::NotUnicode(_)) => {
            return Err(StartupError::LoggingFilter {
                detail: "RUST_LOG is not valid Unicode".to_owned(),
            });
        }
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .try_init()
        .map_err(|error| StartupError::LoggingFilter {
            detail: error.to_string(),
        })
}

async fn supervise<S>(
    mut actors: JoinSet<ActorResult>,
    mut actor_ids: HashMap<Id, BackendId>,
    mut coordinator: JoinHandle<Result<(), BridgeError>>,
    mailboxes: CommandMailboxes,
    startup_complete: Arc<AtomicBool>,
    signal: S,
    shutdown_timeout: Duration,
) -> Result<(), BridgeError>
where
    S: Future<Output = Result<(), BridgeError>>,
{
    tokio::pin!(signal);

    let root_result = tokio::select! {
        signal_result = &mut signal => signal_result,
        actor_result = actors.join_next() => {
            classify_actor_exit(
                actor_result,
                &mut actor_ids,
                startup_complete.load(Ordering::Acquire),
            )
        }
        coordinator_result = &mut coordinator => {
            classify_coordinator_exit(coordinator_result)
        }
    };

    mailboxes.shutdown();

    let shutdown_result = tokio::time::timeout(shutdown_timeout, async {
        while let Some(result) = actors.join_next().await {
            classify_shutdown_actor_exit(result, &mut actor_ids)?;
        }
        if !coordinator.is_finished() {
            coordinator
                .await
                .map_err(|join_error| BridgeError::CoordinatorJoin {
                    detail: join_error.to_string(),
                })??;
        }
        Ok::<(), BridgeError>(())
    })
    .await;

    let shutdown_result = match shutdown_result {
        Ok(result) => result,
        Err(_) => {
            let backend = actor_ids.values().copied().next().unwrap_or(BackendId::X11);
            Err(BridgeError::Shutdown(ShutdownError::Timeout { backend }))
        }
    };

    match root_result {
        Err(root_error) => {
            if let Err(shutdown_error) = shutdown_result {
                error!(error = %shutdown_error, "bounded shutdown also failed");
            }
            Err(root_error)
        }
        Ok(()) => shutdown_result,
    }
}

fn classify_actor_exit(
    result: Option<Result<ActorResult, tokio::task::JoinError>>,
    actor_ids: &mut HashMap<Id, BackendId>,
    startup_complete: bool,
) -> Result<(), BridgeError> {
    let Some(result) = result else {
        return Err(BridgeError::UnexpectedCoordinatorExit);
    };
    match result {
        Ok((backend, Ok(()))) => {
            actor_ids.retain(|_, candidate| *candidate != backend);
            if startup_complete {
                Err(BridgeError::UnexpectedBackendExit { backend })
            } else {
                Err(BridgeError::Startup(StartupError::Backend {
                    backend,
                    stage: "backend-startup",
                    detail: "actor exited before startup snapshots completed".to_owned(),
                }))
            }
        }
        Ok((backend, Err(source))) => {
            actor_ids.retain(|_, candidate| *candidate != backend);
            if startup_complete {
                Err(BridgeError::Backend { backend, source })
            } else {
                Err(startup_backend_error(backend, &source))
            }
        }
        Err(join_error) => {
            let backend = actor_ids.remove(&join_error.id()).unwrap_or(BackendId::X11);
            Err(BridgeError::ActorJoin {
                backend,
                detail: join_error.to_string(),
            })
        }
    }
}

fn startup_backend_error(backend: BackendId, source: &ProtocolError) -> BridgeError {
    let stage = match source {
        ProtocolError::Operation { stage, .. } | ProtocolError::InvalidState { stage, .. } => stage,
        ProtocolError::Disconnected { .. } => "backend-connection",
    };
    BridgeError::Startup(StartupError::Backend {
        backend,
        stage,
        detail: source.to_string(),
    })
}

fn classify_shutdown_actor_exit(
    result: Result<ActorResult, tokio::task::JoinError>,
    actor_ids: &mut HashMap<Id, BackendId>,
) -> Result<(), BridgeError> {
    match result {
        Ok((backend, result)) => {
            actor_ids.retain(|_, candidate| *candidate != backend);
            result.map_err(|source| BridgeError::Backend { backend, source })
        }
        Err(join_error) => {
            let backend = actor_ids.remove(&join_error.id()).unwrap_or(BackendId::X11);
            Err(BridgeError::ActorJoin {
                backend,
                detail: join_error.to_string(),
            })
        }
    }
}

fn classify_coordinator_exit(
    result: Result<Result<(), BridgeError>, tokio::task::JoinError>,
) -> Result<(), BridgeError> {
    match result {
        Ok(Ok(())) => Err(BridgeError::UnexpectedCoordinatorExit),
        Ok(Err(error)) => Err(error),
        Err(error) => Err(BridgeError::CoordinatorJoin {
            detail: error.to_string(),
        }),
    }
}

async fn shutdown_signal() -> Result<(), BridgeError> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate =
            signal(SignalKind::terminate()).map_err(|error| BridgeError::Signal {
                detail: error.to_string(),
            })?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map_err(|error| BridgeError::Signal {
                detail: error.to_string(),
            }),
            _ = terminate.recv() => Ok(()),
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .map_err(|error| BridgeError::Signal {
                detail: error.to_string(),
            })
    }
}

#[derive(Clone)]
struct CommandMailboxes {
    x11_clipboard: watch::Sender<Option<BackendCommand>>,
    x11_primary: watch::Sender<Option<BackendCommand>>,
    wayland_clipboard: watch::Sender<Option<BackendCommand>>,
    wayland_primary: watch::Sender<Option<BackendCommand>>,
}

struct CommandReceivers {
    x11_clipboard: watch::Receiver<Option<BackendCommand>>,
    x11_primary: watch::Receiver<Option<BackendCommand>>,
    wayland_clipboard: watch::Receiver<Option<BackendCommand>>,
    wayland_primary: watch::Receiver<Option<BackendCommand>>,
}

impl CommandMailboxes {
    fn new() -> (Self, CommandReceivers) {
        let (x11_clipboard, x11_clipboard_rx) = watch::channel(None);
        let (x11_primary, x11_primary_rx) = watch::channel(None);
        let (wayland_clipboard, wayland_clipboard_rx) = watch::channel(None);
        let (wayland_primary, wayland_primary_rx) = watch::channel(None);
        (
            Self {
                x11_clipboard,
                x11_primary,
                wayland_clipboard,
                wayland_primary,
            },
            CommandReceivers {
                x11_clipboard: x11_clipboard_rx,
                x11_primary: x11_primary_rx,
                wayland_clipboard: wayland_clipboard_rx,
                wayland_primary: wayland_primary_rx,
            },
        )
    }

    fn publish(&self, backend: BackendId, command: BackendCommand) {
        let selection = match &command {
            BackendCommand::SetText { selection, .. } => *selection,
            BackendCommand::Shutdown => {
                self.shutdown();
                return;
            }
        };
        self.sender(backend, selection).send_replace(Some(command));
    }

    fn shutdown(&self) {
        for sender in [
            &self.x11_clipboard,
            &self.x11_primary,
            &self.wayland_clipboard,
            &self.wayland_primary,
        ] {
            sender.send_replace(Some(BackendCommand::Shutdown));
        }
    }

    fn sender(
        &self,
        backend: BackendId,
        selection: SelectionKind,
    ) -> &watch::Sender<Option<BackendCommand>> {
        match (backend, selection) {
            (BackendId::X11, SelectionKind::Clipboard) => &self.x11_clipboard,
            (BackendId::X11, SelectionKind::Primary) => &self.x11_primary,
            (BackendId::Wayland, SelectionKind::Clipboard) => &self.wayland_clipboard,
            (BackendId::Wayland, SelectionKind::Primary) => &self.wayland_primary,
        }
    }
}

async fn run_coordinator(
    mut backend_event_rx: mpsc::Receiver<BackendEvent>,
    mailboxes: CommandMailboxes,
    startup_complete: Arc<AtomicBool>,
) -> Result<(), BridgeError> {
    let mut state = BridgeState::new();
    let startup_deadline = tokio::time::Instant::now() + STARTUP_SNAPSHOT_TIMEOUT;
    info!("[Sync] Starting coordinator with native backend events");

    loop {
        let event = if state.startup_complete() {
            backend_event_rx.recv().await
        } else {
            tokio::select! {
                event = backend_event_rx.recv() => event,
                _ = tokio::time::sleep_until(startup_deadline) => {
                    if !state.backends_ready() {
                        return Err(BridgeError::Startup(StartupError::BackendReadyTimeout));
                    }
                    for selection in state.expire_startup() {
                        warn!(%selection, "startup snapshot timed out; no startup overwrite will occur");
                    }
                    if state.startup_complete() {
                        startup_complete.store(true, Ordering::Release);
                    }
                    continue;
                }
            }
        };
        let Some(event) = event else {
            break;
        };
        if let BackendEvent::Ready {
            backend,
            capabilities,
        } = &event
        {
            if !capabilities.clipboard {
                return Err(BridgeError::Startup(
                    StartupError::MissingClipboardCapability { backend: *backend },
                ));
            }
            if !capabilities.primary {
                warn!(%backend, "Primary selection is unavailable for this backend");
            }
        }

        let effects = state.reduce(event)?;
        if state.startup_complete() {
            startup_complete.store(true, Ordering::Release);
        }
        for effect in effects {
            match effect {
                CoordinatorEffect::SendCommand { backend, command } => {
                    if let BackendCommand::SetText {
                        selection,
                        revision,
                        command_id,
                        payload,
                        expected_target_epoch: _,
                    } = &command
                    {
                        info!(
                            %backend,
                            %selection,
                            revision = revision.value(),
                            command_id = command_id.value(),
                            bytes = payload.len(),
                            "forwarding clipboard text"
                        );
                    }
                    mailboxes.publish(backend, command);
                }
                CoordinatorEffect::StartupConflict { selection } => {
                    warn!(%selection, "startup selection conflict");
                }
                CoordinatorEffect::ReportRecoverable {
                    backend,
                    selection,
                    stage,
                    error,
                } => {
                    warn!(%backend, ?selection, stage, error = %error, "recoverable backend event");
                }
                CoordinatorEffect::Stop { backend, error } => {
                    return if startup_complete.load(Ordering::Acquire) {
                        Err(BridgeError::Backend {
                            backend,
                            source: error,
                        })
                    } else {
                        Err(startup_backend_error(backend, &error))
                    };
                }
            }
        }
    }

    debug!("all backend event senders closed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    fn actor_ids(actors: &mut JoinSet<ActorResult>) -> HashMap<Id, BackendId> {
        let mut ids = HashMap::new();
        for backend in [BackendId::X11, BackendId::Wayland] {
            let abort = actors.spawn(async move {
                std::future::pending::<()>().await;
                (backend, Ok(()))
            });
            ids.insert(abort.id(), backend);
        }
        ids
    }

    fn completed_startup() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(true))
    }

    #[test]
    fn backend_event_channel_enforces_its_capacity() {
        let (sender, _receiver) = mpsc::channel(BACKEND_EVENT_CAPACITY);
        let event = BackendEvent::Ready {
            backend: BackendId::X11,
            capabilities: crate::domain::BackendCapabilities::text_bridge(),
        };
        for _ in 0..BACKEND_EVENT_CAPACITY {
            sender
                .try_send(event.clone())
                .expect("events up to the declared capacity are accepted");
        }
        assert!(matches!(
            sender.try_send(event),
            Err(mpsc::error::TrySendError::Full(_))
        ));
    }

    #[test]
    fn command_mailbox_coalesces_a_large_burst_to_the_latest_value() {
        let (mailboxes, mut receivers) = CommandMailboxes::new();
        let payload = crate::domain::TextPayload::from_string("bounded".to_owned())
            .expect("test payload is valid");
        for sequence in 1..=10_000 {
            mailboxes.publish(
                BackendId::Wayland,
                BackendCommand::SetText {
                    command_id: crate::domain::CommandId::new(sequence),
                    selection: SelectionKind::Clipboard,
                    revision: crate::domain::Revision::new(sequence),
                    expected_target_epoch: crate::domain::BackendEpoch::new(1),
                    payload: payload.clone(),
                },
            );
        }
        assert!(matches!(
            receivers.wayland_clipboard.borrow_and_update().as_ref(),
            Some(BackendCommand::SetText {
                command_id,
                revision,
                ..
            }) if *command_id == crate::domain::CommandId::new(10_000)
                && *revision == crate::domain::Revision::new(10_000)
        ));
    }

    #[tokio::test]
    async fn signal_requests_shutdown_for_every_mailbox() {
        let (mailboxes, mut receivers) = CommandMailboxes::new();
        let mut actors = JoinSet::new();
        let mut ids = HashMap::new();
        let pairs = [
            (BackendId::X11, receivers.x11_clipboard.clone()),
            (BackendId::Wayland, receivers.wayland_clipboard.clone()),
        ];
        for (backend, mut receiver) in pairs {
            let abort = actors.spawn(async move {
                if let Err(error) = receiver.changed().await {
                    return (
                        backend,
                        Err(ProtocolError::Operation {
                            stage: "test-mailbox",
                            detail: error.to_string(),
                        }),
                    );
                }
                assert!(matches!(*receiver.borrow(), Some(BackendCommand::Shutdown)));
                (backend, Ok(()))
            });
            ids.insert(abort.id(), backend);
        }
        let coordinator = tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok(())
        });
        let result = supervise(
            actors,
            ids,
            coordinator,
            mailboxes,
            completed_startup(),
            async { Ok(()) },
            Duration::from_secs(1),
        )
        .await;
        assert!(result.is_ok());
        assert!(matches!(
            receivers.x11_primary.borrow_and_update().as_ref(),
            Some(BackendCommand::Shutdown)
        ));
        assert!(matches!(
            receivers.wayland_primary.borrow_and_update().as_ref(),
            Some(BackendCommand::Shutdown)
        ));
    }

    #[tokio::test]
    async fn backend_failure_is_the_root_error() {
        let (mailboxes, _receivers) = CommandMailboxes::new();
        let mut actors = JoinSet::new();
        let failure = ProtocolError::Disconnected {
            detail: "test disconnect".to_owned(),
        };
        let first = failure.clone();
        let abort = actors.spawn(async move { (BackendId::X11, Err(first)) });
        let mut ids = HashMap::from([(abort.id(), BackendId::X11)]);
        let other = actors.spawn(async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            (BackendId::Wayland, Ok(()))
        });
        ids.insert(other.id(), BackendId::Wayland);
        let coordinator =
            tokio::spawn(async { std::future::pending::<Result<(), BridgeError>>().await });

        let result = supervise(
            actors,
            ids,
            coordinator,
            mailboxes,
            completed_startup(),
            std::future::pending(),
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(
            result,
            Err(BridgeError::Backend {
                backend: BackendId::X11,
                source,
            }) if source == failure
        ));
    }

    #[tokio::test]
    async fn join_panic_stops_the_runtime() {
        let (mailboxes, _receivers) = CommandMailboxes::new();
        let mut actors = JoinSet::new();
        let abort = actors.spawn(async move {
            panic!("test actor panic");
            #[allow(unreachable_code)]
            (BackendId::X11, Ok(()))
        });
        let ids = HashMap::from([(abort.id(), BackendId::X11)]);
        let coordinator =
            tokio::spawn(async { std::future::pending::<Result<(), BridgeError>>().await });

        let result = supervise(
            actors,
            ids,
            coordinator,
            mailboxes,
            completed_startup(),
            std::future::pending(),
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(
            result,
            Err(BridgeError::ActorJoin {
                backend: BackendId::X11,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn shutdown_timeout_is_reported() {
        let (mailboxes, _receivers) = CommandMailboxes::new();
        let mut actors = JoinSet::new();
        let ids = actor_ids(&mut actors);
        let coordinator = tokio::spawn(async { std::future::pending().await });
        let (signal_tx, signal_rx) = oneshot::channel();
        signal_tx.send(()).expect("test signal receiver is active");

        let result = supervise(
            actors,
            ids,
            coordinator,
            mailboxes,
            completed_startup(),
            async move {
                signal_rx.await.map_err(|error| BridgeError::Signal {
                    detail: error.to_string(),
                })
            },
            Duration::from_millis(20),
        )
        .await;
        assert!(matches!(
            result,
            Err(BridgeError::Shutdown(ShutdownError::Timeout { .. }))
        ));
    }

    #[tokio::test]
    async fn backend_failure_before_snapshots_is_a_startup_error() {
        let (mailboxes, _receivers) = CommandMailboxes::new();
        let mut actors = JoinSet::new();
        let failure = ProtocolError::invalid_state("wayland-registry", "missing provider");
        let actor_failure = failure.clone();
        let abort = actors.spawn(async move { (BackendId::Wayland, Err(actor_failure)) });
        let ids = HashMap::from([(abort.id(), BackendId::Wayland)]);
        let coordinator =
            tokio::spawn(async { std::future::pending::<Result<(), BridgeError>>().await });

        let result = supervise(
            actors,
            ids,
            coordinator,
            mailboxes,
            Arc::new(AtomicBool::new(false)),
            std::future::pending(),
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(
            result,
            Err(BridgeError::Startup(StartupError::Backend {
                backend: BackendId::Wayland,
                stage: "wayland-registry",
                ..
            }))
        ));
    }
}
