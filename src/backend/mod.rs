use crate::domain::{
    BackendCapabilities, BackendEpoch, BackendId, CommandId, OfferToken, ProtocolError, Revision,
    SelectionKind, SnapshotOutcome, TextPayload, TransferError, UnavailableReason,
};

pub(crate) mod wayland;
pub(crate) mod x11;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendEvent {
    Ready {
        backend: BackendId,
        capabilities: BackendCapabilities,
    },
    SelectionChanged {
        backend: BackendId,
        selection: SelectionKind,
        epoch: BackendEpoch,
    },
    InitialSnapshot {
        backend: BackendId,
        selection: SelectionKind,
        epoch: BackendEpoch,
        token: OfferToken,
        outcome: SnapshotOutcome,
    },
    ObservedText {
        backend: BackendId,
        selection: SelectionKind,
        epoch: BackendEpoch,
        token: OfferToken,
        payload: TextPayload,
    },
    SelectionUnavailable {
        backend: BackendId,
        selection: SelectionKind,
        epoch: BackendEpoch,
        token: OfferToken,
        reason: UnavailableReason,
    },
    OwnershipApplied {
        backend: BackendId,
        selection: SelectionKind,
        command_id: CommandId,
        revision: Revision,
    },
    OwnershipFailed {
        backend: BackendId,
        selection: SelectionKind,
        command_id: CommandId,
        revision: Revision,
        error: ProtocolError,
    },
    OwnershipLost {
        backend: BackendId,
        selection: SelectionKind,
        revision: Revision,
    },
    RecoverableError {
        backend: BackendId,
        selection: Option<SelectionKind>,
        stage: &'static str,
        error: TransferError,
    },
    FatalError {
        backend: BackendId,
        error: ProtocolError,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendCommand {
    SetText {
        command_id: CommandId,
        selection: SelectionKind,
        revision: Revision,
        expected_target_epoch: BackendEpoch,
        payload: TextPayload,
    },
    Shutdown,
}
