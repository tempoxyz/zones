use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, U256};

use crate::{
    CheckerBlockedReason,
    failure::{Failure, FailureClass},
    kernel::{Effect, ExpectedState, ImportedFacts, ZoneFacts},
    metrics,
    persistence::{
        BlockNumHash, Coverage, Identity, JournalEntry, Persistence, PersistenceError, Snapshot,
        make_finding,
    },
};

mod logging;
mod verification;

/// Receipt- and state-derived output, constructed independently from the kernel result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthenticatedOutputs {
    pub effects: Vec<Effect>,
    pub state: ExpectedState,
    /// Exact token supplies read at this block, keyed by token address.
    pub supplies: BTreeMap<Address, U256>,
}

/// One authenticated Zone block and its imported Tempo transition.
#[derive(Debug, Clone)]
pub(crate) struct AuthenticatedBlock {
    pub zone: BlockNumHash,
    pub parent: BlockNumHash,
    pub tempo: BlockNumHash,
    pub tempo_parent: BlockNumHash,
    pub imported: ImportedFacts,
    pub zone_facts: ZoneFacts,
    pub outputs: AuthenticatedOutputs,
}

/// A failed authentication with its canonical Zone coordinate when available.
pub(crate) struct AuthenticationFailure {
    failure: Box<Failure>,
    coordinate: Option<(BlockNumHash, BlockNumHash)>,
}

impl AuthenticationFailure {
    /// Retain a failure that occurred before a Zone block was acquired.
    pub(crate) fn unlocated(failure: Failure) -> Self {
        Self {
            failure: Box::new(failure),
            coordinate: None,
        }
    }

    /// Bind a failure to the acquired Zone block and its parent.
    pub(crate) fn at(zone: BlockNumHash, parent: BlockNumHash, failure: Failure) -> Self {
        Self {
            failure: Box::new(failure),
            coordinate: Some((zone, parent)),
        }
    }

    /// Return the acquired Zone coordinate, if acquisition reached one.
    pub(crate) const fn coordinate(&self) -> Option<BlockNumHash> {
        match self.coordinate {
            Some((zone, _)) => Some(zone),
            None => None,
        }
    }
}

struct RetryState {
    attempts: u32,
    next_attempt: Instant,
}

/// The one block the runtime currently needs from canonical local history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AuthenticationRequest {
    height: u64,
}

impl AuthenticationRequest {
    /// Canonical Zone height to acquire.
    pub(crate) const fn height(self) -> u64 {
        self.height
    }
}

/// Work the ExEx must perform after one runtime step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeAction {
    Authenticate(AuthenticationRequest),
    AwaitNotification,
    RetryAt(Instant),
}

/// Sequentially verifies local canonical history from the durable checkpoint.
pub(crate) struct Runtime {
    snapshot: Snapshot,
    retry: Option<RetryState>,
}

impl Runtime {
    /// Start from the latest durable checker snapshot.
    pub(crate) fn new(snapshot: Snapshot) -> Self {
        metrics::set_snapshot(&snapshot);
        Self {
            snapshot,
            retry: None,
        }
    }

    /// Return the latest durable snapshot.
    pub(crate) fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Persist a blocked state without advancing verified progress.
    pub(crate) fn block(
        &mut self,
        store: &Persistence,
        reason: CheckerBlockedReason,
    ) -> Result<(), PersistenceError> {
        self.snapshot = store.record_blocked_current(reason)?;
        metrics::set_blocked(Some(reason));
        self.retry = None;
        Ok(())
    }

    /// Rewind the verified checkpoint to a canonical notification ancestor.
    pub(crate) fn reorg(
        &mut self,
        store: &Persistence,
        ancestor: BlockNumHash,
    ) -> Result<(), PersistenceError> {
        self.snapshot = match store.reorg(&self.snapshot, ancestor) {
            Ok(snapshot) => snapshot,
            Err(PersistenceError::ReorgBeyondRetention { .. }) => {
                self.block(store, CheckerBlockedReason::DeepReorgBeyondRetention)?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        metrics::set_snapshot(&self.snapshot);
        self.retry = None;
        Ok(())
    }

    /// Record the local canonical head as the target for sequential recovery.
    pub(crate) fn observe_tip(
        &mut self,
        store: &Persistence,
        observed: BlockNumHash,
    ) -> Result<(), PersistenceError> {
        self.snapshot.meta = store.record_observed_tip(&self.snapshot, observed)?;
        Ok(())
    }

    /// Select the next unverified canonical height.
    pub(crate) fn next_action(&self, now: Instant) -> RuntimeAction {
        if self.snapshot.meta.blocked.is_some()
            || !matches!(self.snapshot.meta.coverage, Coverage::Recovering)
        {
            return RuntimeAction::AwaitNotification;
        }
        if let Some(retry) = &self.retry
            && now < retry.next_attempt
        {
            return RuntimeAction::RetryAt(retry.next_attempt);
        }
        RuntimeAction::Authenticate(AuthenticationRequest {
            height: self
                .snapshot
                .meta
                .verified_zone_tip
                .number
                .saturating_add(1),
        })
    }

    /// Commit a canonical block or schedule a retry for unavailable acquisition data.
    pub(crate) fn complete_authentication(
        &mut self,
        store: &Persistence,
        identity: Identity,
        request: AuthenticationRequest,
        result: Result<AuthenticatedBlock, AuthenticationFailure>,
        now: Instant,
    ) -> Result<Option<BlockNumHash>, PersistenceError> {
        let verified = self.snapshot.meta.verified_zone_tip;
        match result {
            Ok(block) => {
                if block.zone.number != request.height || block.parent != verified {
                    return Err(PersistenceError::Invalid(
                        "recovered block does not extend the requested parent".into(),
                    ));
                }
                self.retry = None;
                self.process_block(store, identity, &block)?;
            }
            Err(failure) => self.handle_acquisition_failure(store, failure, now)?,
        }
        Ok((self.snapshot.meta.verified_zone_tip != verified)
            .then_some(self.snapshot.meta.verified_zone_tip))
    }

    /// Retry unavailable local or L1 data without converting backpressure into a coverage gap.
    fn handle_acquisition_failure(
        &mut self,
        store: &Persistence,
        failed: AuthenticationFailure,
        now: Instant,
    ) -> Result<(), PersistenceError> {
        let AuthenticationFailure {
            failure,
            coordinate,
        } = failed;
        match failure.class {
            FailureClass::Retry => {
                let retry = self.retry.get_or_insert(RetryState {
                    attempts: 0,
                    next_attempt: now,
                });
                retry.attempts = retry.attempts.saturating_add(1);
                let exponent = retry.attempts.saturating_sub(1).min(5);
                let delay = Duration::from_millis(250).saturating_mul(1u32 << exponent);
                retry.next_attempt = now + delay;
                logging::retry(
                    coordinate.map(|(zone, _)| zone),
                    retry.attempts,
                    delay,
                    &failure.message,
                );
            }
            FailureClass::Divergence => {
                let Some((zone, parent)) = coordinate else {
                    logging::terminal(&failure.message);
                    self.block(store, CheckerBlockedReason::InvalidAuthenticatedData)?;
                    return Ok(());
                };
                self.record_finding(store, zone, parent, None, *failure)?;
            }
            FailureClass::Terminal => {
                logging::terminal(&failure.message);
                self.block(store, CheckerBlockedReason::InvalidAuthenticatedData)?;
            }
        }
        Ok(())
    }

    /// Verify and persist one authenticated canonical block.
    fn process_block(
        &mut self,
        store: &Persistence,
        identity: Identity,
        block: &AuthenticatedBlock,
    ) -> Result<(), PersistenceError> {
        let candidate = match verification::verify_block(identity, &self.snapshot.state, block) {
            Ok(candidate) => candidate,
            Err(failure) if failure.class == FailureClass::Divergence => {
                return self.record_finding(
                    store,
                    block.zone,
                    block.parent,
                    Some((block.tempo, block.tempo_parent)),
                    failure,
                );
            }
            Err(failure) => {
                logging::terminal(&failure.message);
                self.block(store, CheckerBlockedReason::InvalidAuthenticatedData)?;
                return Ok(());
            }
        };
        let observed = self.snapshot.meta.observed_zone_tip;
        let coverage = if block.zone == observed {
            Coverage::Complete
        } else {
            Coverage::Recovering
        };
        self.snapshot = store.apply(
            &self.snapshot,
            JournalEntry {
                zone: block.zone,
                parent: block.parent,
                imported_tempo: block.tempo,
                imported_tempo_parent: block.tempo_parent,
                delta: candidate.delta,
            },
            observed,
            coverage,
        )?;
        logging::verified(block);
        Ok(())
    }

    /// Persist an authenticated divergence and its unchecked canonical suffix.
    fn record_finding(
        &mut self,
        store: &Persistence,
        zone: BlockNumHash,
        parent: BlockNumHash,
        imported: Option<(BlockNumHash, BlockNumHash)>,
        failure: Failure,
    ) -> Result<(), PersistenceError> {
        let finding = failure
            .finding
            .ok_or_else(|| PersistenceError::Invalid("divergence has no finding".into()))?;
        let (key, finding) = make_finding(zone, parent, imported, *finding, failure.message)?;
        let logged_finding = finding.clone();
        self.snapshot = store.record_divergence(
            &self.snapshot,
            key,
            finding,
            self.snapshot.meta.observed_zone_tip,
        )?;
        metrics::record_divergence(logged_finding.details.category);
        logging::finding(&logged_finding);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
