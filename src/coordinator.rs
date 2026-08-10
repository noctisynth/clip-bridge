use std::fmt;

use crate::{
    backend::{BackendCommand, BackendEvent},
    domain::{
        BackendCapabilities, BackendEpoch, BackendId, CommandId, OfferToken, ProtocolError,
        RecoverableError, Revision, SelectionKind, SnapshotOutcome, TextPayload,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordinatorEffect {
    SendCommand {
        backend: BackendId,
        command: BackendCommand,
    },
    StartupConflict {
        selection: SelectionKind,
    },
    ReportRecoverable {
        backend: BackendId,
        selection: Option<SelectionKind>,
        stage: &'static str,
        error: RecoverableError,
    },
    Stop {
        backend: BackendId,
        error: ProtocolError,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordinatorError {
    RevisionOverflow { selection: SelectionKind },
    CommandIdOverflow,
}

impl fmt::Display for CoordinatorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RevisionOverflow { selection } => {
                write!(formatter, "revision overflow for {selection:?}")
            }
            Self::CommandIdOverflow => formatter.write_str("backend command id overflow"),
        }
    }
}

impl std::error::Error for CoordinatorError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeState {
    x11_capabilities: Option<BackendCapabilities>,
    wayland_capabilities: Option<BackendCapabilities>,
    clipboard: SelectionState,
    primary: SelectionState,
    last_command_id: CommandId,
}

impl Default for BridgeState {
    fn default() -> Self {
        Self::new()
    }
}

impl BridgeState {
    pub fn new() -> Self {
        Self {
            x11_capabilities: None,
            wayland_capabilities: None,
            clipboard: SelectionState::default(),
            primary: SelectionState::default(),
            last_command_id: CommandId::default(),
        }
    }

    #[cfg(test)]
    fn running_for_test() -> Self {
        let mut state = Self::new();
        state.x11_capabilities = Some(BackendCapabilities::text_bridge());
        state.wayland_capabilities = Some(BackendCapabilities::text_bridge());
        state.clipboard.startup = StartupPhase::Complete;
        state.primary.startup = StartupPhase::Complete;
        state
    }

    pub fn backends_ready(&self) -> bool {
        self.x11_capabilities.is_some() && self.wayland_capabilities.is_some()
    }

    pub fn startup_complete(&self) -> bool {
        self.clipboard.startup == StartupPhase::Complete
            && self.primary.startup == StartupPhase::Complete
    }

    pub fn expire_startup(&mut self) -> Vec<SelectionKind> {
        let mut expired = Vec::new();
        for selection in [SelectionKind::Clipboard, SelectionKind::Primary] {
            if self.selection(selection).startup == StartupPhase::Awaiting {
                self.selection_mut(selection).startup = StartupPhase::Complete;
                expired.push(selection);
            }
        }
        expired
    }

    pub fn reduce(
        &mut self,
        event: BackendEvent,
    ) -> Result<Vec<CoordinatorEffect>, CoordinatorError> {
        match event {
            BackendEvent::Ready {
                backend,
                capabilities,
            } => {
                self.set_capabilities(backend, capabilities);
                let mut effects = self.resolve_startup(SelectionKind::Clipboard)?;
                effects.extend(self.resolve_startup(SelectionKind::Primary)?);
                Ok(effects)
            }
            BackendEvent::SelectionChanged {
                backend,
                selection,
                epoch,
            } => {
                self.selection_changed(backend, selection, epoch);
                Ok(Vec::new())
            }
            BackendEvent::InitialSnapshot {
                backend,
                selection,
                epoch,
                token,
                outcome,
            } => {
                if !self.accept_snapshot(backend, selection, epoch, token, outcome) {
                    return Ok(Vec::new());
                }
                self.resolve_startup(selection)
            }
            BackendEvent::ObservedText {
                backend,
                selection,
                epoch,
                token,
                payload,
            } => self.observe_text(backend, selection, epoch, token, payload),
            BackendEvent::SelectionUnavailable {
                backend,
                selection,
                epoch,
                token,
                reason: _,
            } => {
                self.mark_unavailable(backend, selection, epoch, token);
                Ok(Vec::new())
            }
            BackendEvent::OwnershipApplied {
                backend,
                selection,
                command_id,
                revision,
            } => {
                self.ownership_applied(backend, selection, command_id, revision);
                Ok(Vec::new())
            }
            BackendEvent::OwnershipFailed {
                backend,
                selection,
                command_id,
                revision,
                error,
            } => {
                self.ownership_failed(backend, selection, command_id, revision);
                Ok(vec![CoordinatorEffect::ReportRecoverable {
                    backend,
                    selection: Some(selection),
                    stage: "set-selection",
                    error: error.into(),
                }])
            }
            BackendEvent::OwnershipLost {
                backend,
                selection,
                revision,
            } => {
                self.ownership_lost(backend, selection, revision);
                Ok(Vec::new())
            }
            BackendEvent::RecoverableError {
                backend,
                selection,
                stage,
                error,
            } => Ok(vec![CoordinatorEffect::ReportRecoverable {
                backend,
                selection,
                stage,
                error: error.into(),
            }]),
            BackendEvent::FatalError { backend, error } => {
                Ok(vec![CoordinatorEffect::Stop { backend, error }])
            }
        }
    }

    fn set_capabilities(&mut self, backend: BackendId, capabilities: BackendCapabilities) {
        match backend {
            BackendId::X11 => self.x11_capabilities = Some(capabilities),
            BackendId::Wayland => self.wayland_capabilities = Some(capabilities),
        }
    }

    fn capabilities(&self, backend: BackendId) -> Option<BackendCapabilities> {
        match backend {
            BackendId::X11 => self.x11_capabilities,
            BackendId::Wayland => self.wayland_capabilities,
        }
    }

    fn supports(&self, backend: BackendId, selection: SelectionKind) -> bool {
        self.capabilities(backend)
            .is_some_and(|capabilities| capabilities.supports(selection))
    }

    fn selection(&self, selection: SelectionKind) -> &SelectionState {
        match selection {
            SelectionKind::Clipboard => &self.clipboard,
            SelectionKind::Primary => &self.primary,
        }
    }

    fn selection_mut(&mut self, selection: SelectionKind) -> &mut SelectionState {
        match selection {
            SelectionKind::Clipboard => &mut self.clipboard,
            SelectionKind::Primary => &mut self.primary,
        }
    }

    fn selection_changed(
        &mut self,
        backend: BackendId,
        selection: SelectionKind,
        epoch: BackendEpoch,
    ) {
        let state = self.selection_mut(selection).backend_mut(backend);
        if epoch <= state.epoch {
            return;
        }

        state.epoch = epoch;
        state.observed = None;
        if state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.expected_target_epoch != epoch)
        {
            state.pending = None;
        }
    }

    fn accept_snapshot(
        &mut self,
        backend: BackendId,
        selection: SelectionKind,
        epoch: BackendEpoch,
        token: OfferToken,
        outcome: SnapshotOutcome,
    ) -> bool {
        let state = self.selection_mut(selection).backend_mut(backend);
        if epoch < state.epoch || token <= state.latest_token {
            return false;
        }

        state.epoch = epoch;
        state.latest_token = token;
        state.observed = match &outcome {
            SnapshotOutcome::Text(payload) => Some(payload.clone()),
            SnapshotOutcome::Empty | SnapshotOutcome::Unsupported | SnapshotOutcome::Failed => None,
        };
        state.initial = Some(outcome);
        true
    }

    fn observe_text(
        &mut self,
        backend: BackendId,
        selection: SelectionKind,
        epoch: BackendEpoch,
        token: OfferToken,
        payload: TextPayload,
    ) -> Result<Vec<CoordinatorEffect>, CoordinatorError> {
        {
            let source = self.selection_mut(selection).backend_mut(backend);
            if epoch != source.epoch || token <= source.latest_token {
                return Ok(Vec::new());
            }
            source.latest_token = token;
            source.observed = Some(payload.clone());
        }

        let target = backend.other();
        if !self.supports(target, selection) {
            return Ok(Vec::new());
        }

        if self
            .selection(selection)
            .backend(target)
            .contains_payload(&payload)
        {
            return Ok(Vec::new());
        }

        self.issue_set_text(target, selection, payload)
            .map(|effect| vec![effect])
    }

    fn mark_unavailable(
        &mut self,
        backend: BackendId,
        selection: SelectionKind,
        epoch: BackendEpoch,
        token: OfferToken,
    ) {
        let state = self.selection_mut(selection).backend_mut(backend);
        if epoch != state.epoch || token <= state.latest_token {
            return;
        }

        state.latest_token = token;
        state.observed = None;
    }

    fn ownership_applied(
        &mut self,
        backend: BackendId,
        selection: SelectionKind,
        command_id: CommandId,
        revision: Revision,
    ) {
        let state = self.selection_mut(selection).backend_mut(backend);
        let Some(pending) = state.pending.as_ref() else {
            return;
        };
        if pending.command_id != command_id || pending.revision != revision {
            return;
        }

        state.owned = Some(OwnedText {
            revision,
            payload: pending.payload.clone(),
        });
        state.pending = None;
    }

    fn ownership_failed(
        &mut self,
        backend: BackendId,
        selection: SelectionKind,
        command_id: CommandId,
        revision: Revision,
    ) {
        let state = self.selection_mut(selection).backend_mut(backend);
        if state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.command_id == command_id && pending.revision == revision)
        {
            state.pending = None;
        }
    }

    fn ownership_lost(&mut self, backend: BackendId, selection: SelectionKind, revision: Revision) {
        let state = self.selection_mut(selection).backend_mut(backend);
        if state
            .owned
            .as_ref()
            .is_some_and(|owned| owned.revision == revision)
        {
            state.owned = None;
        }
    }

    fn resolve_startup(
        &mut self,
        selection: SelectionKind,
    ) -> Result<Vec<CoordinatorEffect>, CoordinatorError> {
        if self.selection(selection).startup == StartupPhase::Complete {
            return Ok(Vec::new());
        }

        let (Some(x11_capabilities), Some(wayland_capabilities)) =
            (self.x11_capabilities, self.wayland_capabilities)
        else {
            return Ok(Vec::new());
        };

        if !x11_capabilities.supports(selection) || !wayland_capabilities.supports(selection) {
            self.selection_mut(selection).startup = StartupPhase::Complete;
            return Ok(Vec::new());
        }

        let x11 = self.selection(selection).x11.initial.clone();
        let wayland = self.selection(selection).wayland.initial.clone();
        let (Some(x11), Some(wayland)) = (x11, wayland) else {
            return Ok(Vec::new());
        };

        self.selection_mut(selection).startup = StartupPhase::Complete;

        match (x11, wayland) {
            (SnapshotOutcome::Text(left), SnapshotOutcome::Text(right)) if left == right => {
                Ok(Vec::new())
            }
            (SnapshotOutcome::Text(_), SnapshotOutcome::Text(_)) => {
                Ok(vec![CoordinatorEffect::StartupConflict { selection }])
            }
            (SnapshotOutcome::Text(payload), SnapshotOutcome::Empty)
            | (SnapshotOutcome::Text(payload), SnapshotOutcome::Unsupported) => self
                .issue_set_text(BackendId::Wayland, selection, payload)
                .map(|effect| vec![effect]),
            (SnapshotOutcome::Empty, SnapshotOutcome::Text(payload))
            | (SnapshotOutcome::Unsupported, SnapshotOutcome::Text(payload)) => self
                .issue_set_text(BackendId::X11, selection, payload)
                .map(|effect| vec![effect]),
            _ => Ok(Vec::new()),
        }
    }

    fn issue_set_text(
        &mut self,
        target: BackendId,
        selection: SelectionKind,
        payload: TextPayload,
    ) -> Result<CoordinatorEffect, CoordinatorError> {
        let revision = self
            .selection(selection)
            .revision
            .checked_next()
            .ok_or(CoordinatorError::RevisionOverflow { selection })?;
        let command_id = self
            .last_command_id
            .checked_next()
            .ok_or(CoordinatorError::CommandIdOverflow)?;
        let expected_target_epoch = self.selection(selection).backend(target).epoch;

        let pending = PendingCommand {
            command_id,
            revision,
            expected_target_epoch,
            payload: payload.clone(),
        };

        self.last_command_id = command_id;
        let selection_state = self.selection_mut(selection);
        selection_state.revision = revision;
        selection_state.backend_mut(target).pending = Some(pending);

        Ok(CoordinatorEffect::SendCommand {
            backend: target,
            command: BackendCommand::SetText {
                command_id,
                selection,
                revision,
                expected_target_epoch,
                payload,
            },
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum StartupPhase {
    #[default]
    Awaiting,
    Complete,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SelectionState {
    revision: Revision,
    x11: BackendSelectionState,
    wayland: BackendSelectionState,
    startup: StartupPhase,
}

impl SelectionState {
    fn backend(&self, backend: BackendId) -> &BackendSelectionState {
        match backend {
            BackendId::X11 => &self.x11,
            BackendId::Wayland => &self.wayland,
        }
    }

    fn backend_mut(&mut self, backend: BackendId) -> &mut BackendSelectionState {
        match backend {
            BackendId::X11 => &mut self.x11,
            BackendId::Wayland => &mut self.wayland,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct BackendSelectionState {
    epoch: BackendEpoch,
    latest_token: OfferToken,
    initial: Option<SnapshotOutcome>,
    observed: Option<TextPayload>,
    owned: Option<OwnedText>,
    pending: Option<PendingCommand>,
}

impl BackendSelectionState {
    fn contains_payload(&self, payload: &TextPayload) -> bool {
        self.observed.as_ref() == Some(payload)
            || self
                .owned
                .as_ref()
                .is_some_and(|owned| &owned.payload == payload)
            || self
                .pending
                .as_ref()
                .is_some_and(|pending| &pending.payload == payload)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnedText {
    revision: Revision,
    payload: TextPayload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingCommand {
    command_id: CommandId,
    revision: Revision,
    expected_target_epoch: BackendEpoch,
    payload: TextPayload,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::UnavailableReason;

    fn payload(text: &str) -> TextPayload {
        TextPayload::from_string(text.to_owned()).expect("test text is valid and non-empty")
    }

    fn ready_state() -> BridgeState {
        let mut state = BridgeState::new();
        for backend in [BackendId::X11, BackendId::Wayland] {
            state
                .reduce(BackendEvent::Ready {
                    backend,
                    capabilities: BackendCapabilities::text_bridge(),
                })
                .expect("ready events cannot overflow coordinator counters");
        }
        state
    }

    fn snapshot(
        state: &mut BridgeState,
        backend: BackendId,
        selection: SelectionKind,
        outcome: SnapshotOutcome,
    ) -> Vec<CoordinatorEffect> {
        state
            .reduce(BackendEvent::InitialSnapshot {
                backend,
                selection,
                epoch: BackendEpoch::new(1),
                token: OfferToken::new(1),
                outcome,
            })
            .expect("snapshot events use small coordinator counters")
    }

    fn observe(
        state: &mut BridgeState,
        backend: BackendId,
        selection: SelectionKind,
        epoch: u64,
        token: u64,
        text: &str,
    ) -> Vec<CoordinatorEffect> {
        state
            .reduce(BackendEvent::SelectionChanged {
                backend,
                selection,
                epoch: BackendEpoch::new(epoch),
            })
            .expect("selection change does not increment coordinator counters");
        state
            .reduce(BackendEvent::ObservedText {
                backend,
                selection,
                epoch: BackendEpoch::new(epoch),
                token: OfferToken::new(token),
                payload: payload(text),
            })
            .expect("test observations use small coordinator counters")
    }

    #[test]
    fn startup_with_one_text_sets_the_empty_backend() {
        let mut state = ready_state();
        assert!(
            snapshot(
                &mut state,
                BackendId::X11,
                SelectionKind::Clipboard,
                SnapshotOutcome::Text(payload("from x11")),
            )
            .is_empty()
        );

        let effects = snapshot(
            &mut state,
            BackendId::Wayland,
            SelectionKind::Clipboard,
            SnapshotOutcome::Empty,
        );
        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0],
            CoordinatorEffect::SendCommand {
                backend: BackendId::Wayland,
                command: BackendCommand::SetText { payload, .. },
            } if payload.as_str() == "from x11"
        ));
    }

    #[test]
    fn startup_with_only_wayland_text_sets_x11() {
        let mut state = ready_state();
        snapshot(
            &mut state,
            BackendId::X11,
            SelectionKind::Clipboard,
            SnapshotOutcome::Unsupported,
        );

        let effects = snapshot(
            &mut state,
            BackendId::Wayland,
            SelectionKind::Clipboard,
            SnapshotOutcome::Text(payload("from wayland")),
        );
        assert!(matches!(
            &effects[0],
            CoordinatorEffect::SendCommand {
                backend: BackendId::X11,
                command: BackendCommand::SetText { payload, .. },
            } if payload.as_str() == "from wayland"
        ));
    }

    #[test]
    fn equal_startup_text_does_not_write_either_backend() {
        let mut state = ready_state();
        snapshot(
            &mut state,
            BackendId::X11,
            SelectionKind::Clipboard,
            SnapshotOutcome::Text(payload("same")),
        );

        let effects = snapshot(
            &mut state,
            BackendId::Wayland,
            SelectionKind::Clipboard,
            SnapshotOutcome::Text(payload("same")),
        );
        assert!(effects.is_empty());
    }

    #[test]
    fn conflicting_startup_text_waits_for_a_real_change() {
        let mut state = ready_state();
        snapshot(
            &mut state,
            BackendId::X11,
            SelectionKind::Clipboard,
            SnapshotOutcome::Text(payload("x11")),
        );

        let effects = snapshot(
            &mut state,
            BackendId::Wayland,
            SelectionKind::Clipboard,
            SnapshotOutcome::Text(payload("wayland")),
        );
        assert_eq!(
            effects,
            vec![CoordinatorEffect::StartupConflict {
                selection: SelectionKind::Clipboard,
            }]
        );
    }

    #[test]
    fn startup_outcome_categories_have_complete_pairwise_coverage() {
        let outcomes = [
            SnapshotOutcome::Text(payload("text")),
            SnapshotOutcome::Empty,
            SnapshotOutcome::Unsupported,
            SnapshotOutcome::Failed,
        ];

        for x11 in &outcomes {
            for wayland in &outcomes {
                let mut state = ready_state();
                snapshot(
                    &mut state,
                    BackendId::X11,
                    SelectionKind::Clipboard,
                    x11.clone(),
                );
                let effects = snapshot(
                    &mut state,
                    BackendId::Wayland,
                    SelectionKind::Clipboard,
                    wayland.clone(),
                );

                let expected_target = match (x11, wayland) {
                    (
                        SnapshotOutcome::Text(_),
                        SnapshotOutcome::Empty | SnapshotOutcome::Unsupported,
                    ) => Some(BackendId::Wayland),
                    (
                        SnapshotOutcome::Empty | SnapshotOutcome::Unsupported,
                        SnapshotOutcome::Text(_),
                    ) => Some(BackendId::X11),
                    _ => None,
                };

                match expected_target {
                    Some(target) => assert!(matches!(
                        effects.as_slice(),
                        [CoordinatorEffect::SendCommand { backend, .. }] if *backend == target
                    )),
                    None => assert!(effects.is_empty()),
                }
            }
        }
    }

    #[test]
    fn failed_snapshot_never_overwrites_the_other_backend() {
        let mut state = ready_state();
        snapshot(
            &mut state,
            BackendId::X11,
            SelectionKind::Clipboard,
            SnapshotOutcome::Text(payload("x11")),
        );

        let effects = snapshot(
            &mut state,
            BackendId::Wayland,
            SelectionKind::Clipboard,
            SnapshotOutcome::Failed,
        );
        assert!(effects.is_empty());
    }

    #[test]
    fn unsupported_primary_capability_finishes_without_a_command() {
        let mut state = BridgeState::new();
        state
            .reduce(BackendEvent::Ready {
                backend: BackendId::X11,
                capabilities: BackendCapabilities::text_bridge(),
            })
            .expect("ready does not increment counters");
        let effects = state
            .reduce(BackendEvent::Ready {
                backend: BackendId::Wayland,
                capabilities: BackendCapabilities {
                    clipboard: true,
                    primary: false,
                },
            })
            .expect("ready does not increment counters");

        assert!(effects.is_empty());
        assert_eq!(state.primary.startup, StartupPhase::Complete);
    }

    #[test]
    fn runtime_observation_routes_in_both_directions() {
        let mut state = BridgeState::running_for_test();
        let to_wayland = observe(
            &mut state,
            BackendId::X11,
            SelectionKind::Clipboard,
            1,
            1,
            "from x11",
        );
        assert!(matches!(
            &to_wayland[0],
            CoordinatorEffect::SendCommand {
                backend: BackendId::Wayland,
                ..
            }
        ));

        let to_x11 = observe(
            &mut state,
            BackendId::Wayland,
            SelectionKind::Primary,
            1,
            1,
            "from wayland",
        );
        assert!(matches!(
            &to_x11[0],
            CoordinatorEffect::SendCommand {
                backend: BackendId::X11,
                ..
            }
        ));
    }

    #[test]
    fn clipboard_and_primary_have_independent_revisions() {
        let mut state = BridgeState::running_for_test();
        observe(
            &mut state,
            BackendId::X11,
            SelectionKind::Clipboard,
            1,
            1,
            "clipboard",
        );
        observe(
            &mut state,
            BackendId::X11,
            SelectionKind::Primary,
            1,
            1,
            "primary",
        );

        assert_eq!(state.clipboard.revision, Revision::new(1));
        assert_eq!(state.primary.revision, Revision::new(1));
    }

    #[test]
    fn matching_target_observation_prevents_echo() {
        let mut state = BridgeState::running_for_test();
        observe(
            &mut state,
            BackendId::X11,
            SelectionKind::Clipboard,
            1,
            1,
            "same",
        );

        let effects = observe(
            &mut state,
            BackendId::Wayland,
            SelectionKind::Clipboard,
            1,
            1,
            "same",
        );
        assert!(effects.is_empty());
    }

    #[test]
    fn identical_pending_payload_is_not_queued_twice() {
        let mut state = BridgeState::running_for_test();
        observe(
            &mut state,
            BackendId::X11,
            SelectionKind::Clipboard,
            1,
            1,
            "same",
        );

        let effects = observe(
            &mut state,
            BackendId::X11,
            SelectionKind::Clipboard,
            2,
            2,
            "same",
        );
        assert!(effects.is_empty());
    }

    #[test]
    fn selection_change_clears_stale_observation_and_pending() {
        let mut state = BridgeState::running_for_test();
        observe(
            &mut state,
            BackendId::X11,
            SelectionKind::Clipboard,
            1,
            1,
            "old",
        );
        assert!(state.clipboard.wayland.pending.is_some());

        state
            .reduce(BackendEvent::SelectionChanged {
                backend: BackendId::Wayland,
                selection: SelectionKind::Clipboard,
                epoch: BackendEpoch::new(1),
            })
            .expect("selection change does not increment counters");
        assert!(state.clipboard.wayland.observed.is_none());
        assert!(state.clipboard.wayland.pending.is_none());
    }

    #[test]
    fn clear_does_not_clear_or_write_the_other_backend() {
        let mut state = BridgeState::running_for_test();
        state
            .reduce(BackendEvent::SelectionChanged {
                backend: BackendId::X11,
                selection: SelectionKind::Clipboard,
                epoch: BackendEpoch::new(1),
            })
            .expect("selection change does not increment counters");
        let effects = state
            .reduce(BackendEvent::SelectionUnavailable {
                backend: BackendId::X11,
                selection: SelectionKind::Clipboard,
                epoch: BackendEpoch::new(1),
                token: OfferToken::new(1),
                reason: UnavailableReason::Cleared,
            })
            .expect("unavailable does not increment counters");

        assert!(effects.is_empty());
        assert!(state.clipboard.wayland.pending.is_none());
    }

    #[test]
    fn every_unavailable_reason_stays_local() {
        let mut state = BridgeState::running_for_test();
        let reasons = [
            UnavailableReason::Cleared,
            UnavailableReason::Empty,
            UnavailableReason::Unsupported,
            UnavailableReason::InvalidUtf8,
            UnavailableReason::TooLarge,
            UnavailableReason::TransferFailed,
        ];

        for (index, reason) in reasons.into_iter().enumerate() {
            let sequence = u64::try_from(index + 1).expect("the test sequence fits in u64");
            state
                .reduce(BackendEvent::SelectionChanged {
                    backend: BackendId::X11,
                    selection: SelectionKind::Clipboard,
                    epoch: BackendEpoch::new(sequence),
                })
                .expect("selection change does not increment counters");
            let effects = state
                .reduce(BackendEvent::SelectionUnavailable {
                    backend: BackendId::X11,
                    selection: SelectionKind::Clipboard,
                    epoch: BackendEpoch::new(sequence),
                    token: OfferToken::new(sequence),
                    reason,
                })
                .expect("unavailable does not increment counters");
            assert!(effects.is_empty());
        }
    }

    #[test]
    fn ownership_lost_allows_same_text_to_be_applied_again() {
        let mut state = BridgeState::running_for_test();
        let first = observe(
            &mut state,
            BackendId::X11,
            SelectionKind::Clipboard,
            1,
            1,
            "same",
        );
        let (command_id, revision) = match &first[0] {
            CoordinatorEffect::SendCommand {
                command:
                    BackendCommand::SetText {
                        command_id,
                        revision,
                        ..
                    },
                ..
            } => (*command_id, *revision),
            other => panic!("unexpected effect: {other:?}"),
        };
        state
            .reduce(BackendEvent::OwnershipApplied {
                backend: BackendId::Wayland,
                selection: SelectionKind::Clipboard,
                command_id,
                revision,
            })
            .expect("ownership result does not increment counters");
        state
            .reduce(BackendEvent::OwnershipLost {
                backend: BackendId::Wayland,
                selection: SelectionKind::Clipboard,
                revision,
            })
            .expect("ownership loss does not increment counters");

        let effects = observe(
            &mut state,
            BackendId::X11,
            SelectionKind::Clipboard,
            2,
            2,
            "same",
        );
        assert_eq!(effects.len(), 1);
    }

    #[test]
    fn stale_observation_is_ignored() {
        let mut state = BridgeState::running_for_test();
        observe(
            &mut state,
            BackendId::X11,
            SelectionKind::Clipboard,
            2,
            2,
            "new",
        );

        let effects = state
            .reduce(BackendEvent::ObservedText {
                backend: BackendId::X11,
                selection: SelectionKind::Clipboard,
                epoch: BackendEpoch::new(1),
                token: OfferToken::new(1),
                payload: payload("stale"),
            })
            .expect("stale observation does not increment counters");
        assert!(effects.is_empty());
        assert_eq!(
            state
                .clipboard
                .x11
                .observed
                .as_ref()
                .map(TextPayload::as_str),
            Some("new")
        );
    }

    #[test]
    fn stale_ownership_result_does_not_replace_new_pending() {
        let mut state = BridgeState::running_for_test();
        let first = observe(
            &mut state,
            BackendId::X11,
            SelectionKind::Clipboard,
            1,
            1,
            "first",
        );
        let (old_id, old_revision) = match &first[0] {
            CoordinatorEffect::SendCommand {
                command:
                    BackendCommand::SetText {
                        command_id,
                        revision,
                        ..
                    },
                ..
            } => (*command_id, *revision),
            other => panic!("unexpected effect: {other:?}"),
        };
        observe(
            &mut state,
            BackendId::X11,
            SelectionKind::Clipboard,
            2,
            2,
            "second",
        );

        state
            .reduce(BackendEvent::OwnershipApplied {
                backend: BackendId::Wayland,
                selection: SelectionKind::Clipboard,
                command_id: old_id,
                revision: old_revision,
            })
            .expect("ownership result does not increment counters");

        assert!(state.clipboard.wayland.owned.is_none());
        assert_eq!(
            state
                .clipboard
                .wayland
                .pending
                .as_ref()
                .map(|pending| pending.payload.as_str()),
            Some("second")
        );
    }

    #[test]
    fn stale_ownership_failure_does_not_clear_new_pending() {
        let mut state = BridgeState::running_for_test();
        let first = observe(
            &mut state,
            BackendId::X11,
            SelectionKind::Clipboard,
            1,
            1,
            "first",
        );
        let (old_id, old_revision) = match &first[0] {
            CoordinatorEffect::SendCommand {
                command:
                    BackendCommand::SetText {
                        command_id,
                        revision,
                        ..
                    },
                ..
            } => (*command_id, *revision),
            other => panic!("unexpected effect: {other:?}"),
        };
        observe(
            &mut state,
            BackendId::X11,
            SelectionKind::Clipboard,
            2,
            2,
            "second",
        );

        state
            .reduce(BackendEvent::OwnershipFailed {
                backend: BackendId::Wayland,
                selection: SelectionKind::Clipboard,
                command_id: old_id,
                revision: old_revision,
                error: ProtocolError::Operation {
                    stage: "set-selection",
                    detail: "stale failure".to_owned(),
                },
            })
            .expect("ownership result does not increment counters");

        assert_eq!(
            state
                .clipboard
                .wayland
                .pending
                .as_ref()
                .map(|pending| pending.payload.as_str()),
            Some("second")
        );
    }

    #[test]
    fn revision_overflow_is_reported() {
        let mut state = BridgeState::running_for_test();
        state.clipboard.revision = Revision::new(u64::MAX);
        state
            .reduce(BackendEvent::SelectionChanged {
                backend: BackendId::X11,
                selection: SelectionKind::Clipboard,
                epoch: BackendEpoch::new(1),
            })
            .expect("selection change does not increment counters");

        let result = state.reduce(BackendEvent::ObservedText {
            backend: BackendId::X11,
            selection: SelectionKind::Clipboard,
            epoch: BackendEpoch::new(1),
            token: OfferToken::new(1),
            payload: payload("overflow"),
        });
        assert_eq!(
            result,
            Err(CoordinatorError::RevisionOverflow {
                selection: SelectionKind::Clipboard,
            })
        );
    }

    #[test]
    fn reducer_is_deterministic() {
        let state = BridgeState::running_for_test();
        let mut left = state.clone();
        let mut right = state;
        let event = BackendEvent::SelectionChanged {
            backend: BackendId::Wayland,
            selection: SelectionKind::Clipboard,
            epoch: BackendEpoch::new(1),
        };

        let left_effects = left.reduce(event.clone());
        let right_effects = right.reduce(event);
        assert_eq!(left, right);
        assert_eq!(left_effects, right_effects);
    }

    #[test]
    fn startup_timeout_prevents_late_snapshot_overwrite() {
        let mut state = ready_state();
        assert_eq!(
            state.expire_startup(),
            vec![SelectionKind::Clipboard, SelectionKind::Primary]
        );
        assert!(state.startup_complete());

        let effects = snapshot(
            &mut state,
            BackendId::X11,
            SelectionKind::Clipboard,
            SnapshotOutcome::Text(payload("late")),
        );
        assert!(effects.is_empty());
        assert!(state.clipboard.wayland.pending.is_none());
    }
}
