//! Persistent, role-aware batch submission actor.
//!
//! The actor owns submission control flow while its backend owns bounded portal operations. It is
//! deliberately independent from the node role controller so state transitions can be tested
//! without an L1 RPC server.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::{Address, B256};
use alloy_provider::DynProvider;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{ContractError, SolInterface as _};
use eyre::{Result, WrapErr as _};
use futures::future::BoxFuture;
use tempo_alloy::TempoNetwork;
use tokio::sync::{Notify, watch};
use tracing::{debug, info, warn};

use crate::{
    AttestationStore,
    abi::{self, NO_QUEUE_INDEX, ZonePortal},
    attestation::SettlementCertificate,
    monitor::ZoneMonitorConfig,
    resolve_portal_zone_anchor,
    settlement::{
        BatchData, BatchSubmitError, BatchSubmitter, PreparedBatchSubmission, WithdrawalPage,
        ZoneBlockSnapshot, read_zone_block_snapshot,
    },
    withdrawals::SharedWithdrawalStore,
};

const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(200);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
const PORTAL_POLL_INTERVAL: Duration = Duration::from_secs(1);

fn next_retry_delay(delay: Duration) -> Duration {
    delay.saturating_mul(2).min(MAX_RETRY_DELAY)
}

/// Role state relevant to persistent batch submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchSubmissionRole {
    pub generation: u64,
    pub is_leader: bool,
}

impl BatchSubmissionRole {
    pub const fn inactive(generation: u64) -> Self {
        Self {
            generation,
            is_leader: false,
        }
    }

    pub const fn leader(generation: u64) -> Self {
        Self {
            generation,
            is_leader: true,
        }
    }
}

/// Complete portal-confirmed anchor required to construct the next batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmissionAnchor {
    pub zone_height: u64,
    pub zone_block_hash: B256,
    pub processed_deposit_hash: B256,
    pub processed_deposit_number: u64,
}

/// One ordered boundary produced by Zone candidate discovery.
#[derive(Debug, Clone)]
pub struct BatchCandidate {
    pub from: u64,
    pub to: u64,
    pub batch: BatchData,
    pub withdrawals: Vec<abi::Withdrawal>,
}

/// Provider-backed candidate discovery polled by the submission actor.
pub(crate) trait BatchCandidateSource: Send + Sync + 'static {
    /// Wait for the next canonical batch boundary extending an authoritative Portal anchor.
    ///
    /// Each call is an independent discovery operation. Dropping the returned future cancels it;
    /// the source itself has no leadership-generation lifecycle.
    fn next_candidate(&self, anchor: SubmissionAnchor) -> BoxFuture<'_, Result<BatchCandidate>>;
}

/// Candidate work tagged with the leadership generation that accepted it.
struct ActiveBatchCandidate {
    generation: u64,
    candidate: BatchCandidate,
}

impl std::ops::Deref for ActiveBatchCandidate {
    type Target = BatchCandidate;

    fn deref(&self) -> &Self::Target {
        &self.candidate
    }
}

/// Result of rebuilding actor state from authoritative portal and canonical Zone state.
#[derive(Debug)]
pub struct BatchResync {
    pub anchor: SubmissionAnchor,
    pub pending_withdrawals: WithdrawalPage,
}

/// Receipt fields needed by the actor after a confirmed submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfirmedBatchSubmission {
    pub withdrawal_batch_index: u64,
    pub withdrawal_queue_index: Option<u64>,
}

/// Bounded portal operations used by [`BatchSubmissionActor`].
///
/// Implementations must not wait for role changes. `send` is the externally visible mutation
/// boundary: once called, the returned future must be driven to completion by the actor.
pub trait BatchSubmissionBackend: Send + Sync + 'static {
    type Prepared: Send + 'static;

    fn resync(&self) -> BoxFuture<'_, Result<BatchResync>>;
    fn portal_block_hash(&self) -> BoxFuture<'_, Result<B256>>;
    fn settlement_threshold<'a>(&'a self, batch: &'a BatchData) -> BoxFuture<'a, Result<usize>>;
    fn prepare<'a>(
        &'a self,
        batch: &'a BatchData,
        certificate: Option<SettlementCertificate>,
    ) -> BoxFuture<'a, Result<Self::Prepared>>;

    fn send(&self, prepared: Self::Prepared) -> BoxFuture<'_, Result<ConfirmedBatchSubmission>>;
}

/// Production backend for portal submission and authoritative resynchronization.
pub struct PortalBatchSubmissionAdapter<P: crate::ZoneSequencerProvider> {
    metrics: crate::metrics::ZoneMonitorMetrics,
    provider: P,
    portal_address: Address,
    inbox_address: Address,
    outbox_address: Address,
    submitter: BatchSubmitter,
}

impl<P: crate::ZoneSequencerProvider> PortalBatchSubmissionAdapter<P> {
    pub fn new(
        config: &ZoneMonitorConfig,
        provider: P,
        l1_provider: DynProvider<TempoNetwork>,
        signer: PrivateKeySigner,
    ) -> Self {
        let mut submitter = BatchSubmitter::with_signer_and_anchor_config(
            config.portal_address,
            l1_provider,
            signer,
            config.batch_anchor_config,
        );
        submitter.set_attestation_store(config.attestation_store.clone());
        Self {
            metrics: crate::metrics::ZoneMonitorMetrics::default(),
            provider,
            portal_address: config.portal_address,
            inbox_address: config.inbox_address,
            outbox_address: config.outbox_address,
            submitter,
        }
    }

    fn snapshot_at_or_genesis(&self, height: u64) -> Result<ZoneBlockSnapshot> {
        if height == 0 {
            return Ok(ZoneBlockSnapshot {
                tempo_block_number: 0,
                processed_deposit_hash: B256::ZERO,
                processed_deposit_number: 0,
                block_hash: B256::ZERO,
            });
        }
        read_zone_block_snapshot(&self.provider, self.inbox_address, height)
    }
}

impl<P: crate::ZoneSequencerProvider> BatchSubmissionBackend for PortalBatchSubmissionAdapter<P> {
    type Prepared = PreparedBatchSubmission;

    fn resync(&self) -> BoxFuture<'_, Result<BatchResync>> {
        Box::pin(async move {
            let portal_anchor = resolve_portal_zone_anchor(
                &self.provider,
                self.portal_address,
                self.submitter.l1_provider(),
            )
            .await
            .wrap_err("failed to resolve portal-confirmed zone block during resync")?;
            let previous_snapshot = self
                .snapshot_at_or_genesis(portal_anchor.block_number)
                .wrap_err("failed to read portal-confirmed zone commitments")?;
            let pending_withdrawals = self
                .submitter
                .fetch_pending_withdrawals(&self.provider, self.outbox_address)
                .await
                .inspect_err(|_| {
                    self.metrics
                        .withdrawal_store_restore_failure_total
                        .increment(1);
                })?;
            let confirmed = resolve_portal_zone_anchor(
                &self.provider,
                self.portal_address,
                self.submitter.l1_provider(),
            )
            .await
            .wrap_err("failed to confirm portal-confirmed zone block during resync")?;
            eyre::ensure!(
                confirmed == portal_anchor,
                "portal anchor changed while building resync snapshot: initial={portal_anchor:?}, confirmed={confirmed:?}"
            );
            Ok(BatchResync {
                anchor: SubmissionAnchor {
                    zone_height: portal_anchor.block_number,
                    zone_block_hash: portal_anchor.block_hash,
                    processed_deposit_hash: previous_snapshot.processed_deposit_hash,
                    processed_deposit_number: previous_snapshot.processed_deposit_number,
                },
                pending_withdrawals,
            })
        })
    }

    fn portal_block_hash(&self) -> BoxFuture<'_, Result<B256>> {
        Box::pin(self.submitter.read_portal_block_hash())
    }

    fn settlement_threshold<'a>(&'a self, batch: &'a BatchData) -> BoxFuture<'a, Result<usize>> {
        Box::pin(self.submitter.settlement_threshold(batch))
    }

    fn prepare<'a>(
        &'a self,
        batch: &'a BatchData,
        certificate: Option<SettlementCertificate>,
    ) -> BoxFuture<'a, Result<Self::Prepared>> {
        Box::pin(async move {
            self.submitter
                .prepare_submission(batch, certificate)
                .await
                .map_err(batch_submit_error)
        })
    }

    fn send(&self, prepared: Self::Prepared) -> BoxFuture<'_, Result<ConfirmedBatchSubmission>> {
        Box::pin(async move {
            let event = self
                .submitter
                .send_prepared(prepared)
                .await
                .map_err(batch_submit_error)?;
            let withdrawal_queue_index = if event.withdrawalQueueIndex == NO_QUEUE_INDEX {
                None
            } else {
                Some(
                    event
                        .withdrawalQueueIndex
                        .try_into()
                        .map_err(|_| eyre::eyre!("withdrawal queue index overflow"))?,
                )
            };
            Ok(ConfirmedBatchSubmission {
                withdrawal_batch_index: event.withdrawalBatchIndex,
                withdrawal_queue_index,
            })
        })
    }
}

fn batch_submit_error(error: BatchSubmitError) -> eyre::Report {
    match error {
        BatchSubmitError::Other(error) => error,
    }
}

/// Try to decode a ZonePortal revert reason from an eyre error chain.
fn decode_portal_revert(error: &eyre::Report) -> Option<String> {
    let message = format!("{error}");
    let start = message.find("data: \"0x")? + "data: \"".len();
    let end = message[start..].find('"')? + start;
    let bytes = alloy_primitives::hex::decode(&message[start..end]).ok()?;
    let error = ContractError::<ZonePortal::ZonePortalErrors>::abi_decode(&bytes).ok()?;
    Some(error.to_string())
}

#[derive(Debug, Default)]
struct AdmissionFence {
    state: Mutex<AdmissionState>,
}

#[derive(Debug, Default)]
enum AdmissionState {
    #[default]
    Closed,
    Open(u64),
    Sending(u64),
}

impl AdmissionFence {
    fn close(&self) {
        *self.state.lock().expect("admission fence lock poisoned") = AdmissionState::Closed;
    }

    fn open(&self, generation: u64) {
        *self.state.lock().expect("admission fence lock poisoned") =
            AdmissionState::Open(generation);
    }

    /// Atomically reserve the L1 mutation boundary for `generation`.
    fn claim_send(&self, generation: u64) -> bool {
        let mut state = self.state.lock().expect("admission fence lock poisoned");
        if matches!(*state, AdmissionState::Open(open_generation) if open_generation == generation)
        {
            *state = AdmissionState::Sending(generation);
            true
        } else {
            false
        }
    }

    /// Release a completed send while preserving a fence a demotion has closed.
    fn finish_send(&self, generation: u64) {
        let mut state = self.state.lock().expect("admission fence lock poisoned");
        if matches!(*state, AdmissionState::Sending(sending_generation) if sending_generation == generation)
        {
            *state = AdmissionState::Open(generation);
        }
    }
}

/// Node-lifetime control and observation handle for the actor.
#[derive(Clone)]
pub struct BatchSubmissionHandle {
    control_tx: watch::Sender<BatchSubmissionControl>,
    applied_role_rx: watch::Receiver<BatchSubmissionRole>,
    progress_rx: watch::Receiver<Option<SubmissionAnchor>>,
    admission: Arc<AdmissionFence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BatchSubmissionControl {
    role: BatchSubmissionRole,
    stopping: bool,
}

impl BatchSubmissionHandle {
    /// Publish a role transition and wait until the actor has applied it.
    ///
    /// The admission fence closes synchronously before a non-leader update is published. If a
    /// send has already claimed the boundary, this waits until that send drains before returning.
    pub async fn set_role(&mut self, role: BatchSubmissionRole) -> Result<()> {
        if !role.is_leader {
            self.admission.close();
        }
        self.control_tx.send_modify(|control| {
            if !control.stopping {
                control.role = role;
            }
        });
        let control = *self.control_tx.borrow();
        if control.stopping || control.role != role {
            eyre::bail!("batch submission actor is shutting down");
        }
        while *self.applied_role_rx.borrow_and_update() != role {
            self.applied_role_rx
                .changed()
                .await
                .map_err(|_| eyre::eyre!("batch submission actor stopped applying role updates"))?;
        }
        Ok(())
    }

    /// Stop admitting work and ask the node-lifetime actor to exit.
    ///
    /// A submission that already claimed the L1 mutation boundary is drained before exit.
    pub fn shutdown(&self) {
        self.admission.close();
        self.control_tx
            .send_modify(|control| control.stopping = true);
    }

    pub fn subscribe_progress(&self) -> watch::Receiver<Option<SubmissionAnchor>> {
        self.progress_rx.clone()
    }

    pub fn current_progress(&self) -> Option<SubmissionAnchor> {
        *self.progress_rx.borrow()
    }
}

/// Create the persistent actor and its cloneable control handle.
pub(crate) fn batch_submission_actor<B: BatchSubmissionBackend, S: BatchCandidateSource>(
    backend: B,
    candidate_source: S,
    attestation_store: Option<AttestationStore>,
    withdrawal_store: SharedWithdrawalStore,
    withdrawal_notify: Arc<Notify>,
    repair_notify: Arc<Notify>,
) -> (BatchSubmissionActor<B, S>, BatchSubmissionHandle) {
    let initial_role = BatchSubmissionRole::inactive(0);
    let initial_control = BatchSubmissionControl {
        role: initial_role,
        stopping: false,
    };
    let (control_tx, control_rx) = watch::channel(initial_control);
    let (applied_role_tx, applied_role_rx) = watch::channel(initial_role);
    let (progress_tx, progress_rx) = watch::channel(None);
    let admission = Arc::new(AdmissionFence::default());
    (
        BatchSubmissionActor {
            backend: Arc::new(backend),
            candidate_source,
            metrics: crate::metrics::ZoneMonitorMetrics::default(),
            attestation_store,
            withdrawal_store,
            withdrawal_notify,
            repair_notify,
            control_rx,
            applied_role_tx,
            progress_tx,
            admission: admission.clone(),
            role: initial_role,
            anchor: None,
            recovery_delay: INITIAL_RETRY_DELAY,
        },
        BatchSubmissionHandle {
            control_tx,
            applied_role_rx,
            progress_rx,
            admission,
        },
    )
}

/// Persistent event loop for ordered L1 batch submission.
pub(crate) struct BatchSubmissionActor<B: BatchSubmissionBackend, S: BatchCandidateSource> {
    backend: Arc<B>,
    candidate_source: S,
    metrics: crate::metrics::ZoneMonitorMetrics,
    attestation_store: Option<AttestationStore>,
    withdrawal_store: SharedWithdrawalStore,
    withdrawal_notify: Arc<Notify>,
    repair_notify: Arc<Notify>,
    control_rx: watch::Receiver<BatchSubmissionControl>,
    applied_role_tx: watch::Sender<BatchSubmissionRole>,
    progress_tx: watch::Sender<Option<SubmissionAnchor>>,
    admission: Arc<AdmissionFence>,
    role: BatchSubmissionRole,
    anchor: Option<SubmissionAnchor>,
    recovery_delay: Duration,
}

struct CandidateWork {
    candidate: ActiveBatchCandidate,
    certificate: Option<SettlementCertificate>,
    attempt: u32,
    delay: Duration,
    submit_started: Option<std::time::Instant>,
}

impl CandidateWork {
    fn new(candidate: ActiveBatchCandidate) -> Self {
        Self {
            candidate,
            certificate: None,
            attempt: 1,
            delay: INITIAL_RETRY_DELAY,
            submit_started: None,
        }
    }
}

#[derive(Clone, Copy)]
enum ResumePoint {
    Reconcile { backoff_after_success: bool },
    CheckPortal,
    ReadThreshold,
    Prepare,
}

enum SubmissionState {
    Inactive,
    Reconciling {
        work: Option<CandidateWork>,
        backoff_after_success: bool,
    },
    Discovering,
    CheckingPortal(CandidateWork),
    ReadingThreshold(CandidateWork),
    AwaitingSettlement {
        work: CandidateWork,
        threshold: usize,
        changes: watch::Receiver<u64>,
        changes_open: bool,
        portal_poll_at: tokio::time::Instant,
    },
    CheckingSettlementPortal {
        work: CandidateWork,
        threshold: usize,
        changes: watch::Receiver<u64>,
        changes_open: bool,
    },
    Preparing(CandidateWork),
    BackingOff {
        work: Option<CandidateWork>,
        resume: ResumePoint,
        deadline: tokio::time::Instant,
    },
    Submitting {
        work: CandidateWork,
        operation: BoxFuture<'static, Result<ConfirmedBatchSubmission>>,
        shutdown_observed: bool,
    },
    Stopped,
}

impl<B: BatchSubmissionBackend, S: BatchCandidateSource> BatchSubmissionActor<B, S> {
    pub(crate) async fn run(mut self) -> Result<()> {
        info!("Batch submission actor started");
        let mut state = SubmissionState::Inactive;
        loop {
            state = match state {
                SubmissionState::Inactive => {
                    self.control_rx
                        .changed()
                        .await
                        .map_err(|_| eyre::eyre!("batch submission control channel closed"))?;
                    self.apply_latest_control()
                }
                SubmissionState::Reconciling {
                    work,
                    backoff_after_success,
                } => {
                    self.metrics.resync_from_portal_total.increment(1);
                    let mut operation = self.resync_operation();
                    tokio::select! {
                        biased;
                        changed = self.control_rx.changed() => {
                            changed.map_err(|_| eyre::eyre!("batch submission control channel closed"))?;
                            self.apply_latest_control()
                        }
                        result = &mut operation => {
                            match result {
                                Ok(resync) => {
                                    self.apply_resync(resync)?;
                                    self.recovery_delay = INITIAL_RETRY_DELAY;
                                    if !self.control_matches_role() {
                                        self.apply_latest_control()
                                    } else if let Some(work) = work {
                                        if self.candidate_extends_anchor(&work.candidate) {
                                            self.admission.open(self.role.generation);
                                            if backoff_after_success {
                                                self.backoff(Some(work), ResumePoint::CheckPortal)
                                            } else {
                                                SubmissionState::CheckingPortal(work)
                                            }
                                        } else {
                                            SubmissionState::Discovering
                                        }
                                    } else {
                                        self.admission.open(self.role.generation);
                                        self.applied_role_tx.send_replace(self.role);
                                        SubmissionState::Discovering
                                    }
                                }
                                Err(error) => {
                                    if work.is_some() {
                                        self.metrics.batch_submit_retry_total.increment(1);
                                    }
                                    self.record_actor_failure(&error, "Batch submission reconciliation failed; retrying");
                                    self.backoff(
                                        work,
                                        ResumePoint::Reconcile { backoff_after_success },
                                    )
                                }
                            }
                        }
                    }
                }
                SubmissionState::Discovering => {
                    let anchor = self
                        .anchor
                        .expect("candidate discovery requires a reconciled Portal anchor");
                    tokio::select! {
                        biased;
                        changed = self.control_rx.changed() => {
                            changed.map_err(|_| eyre::eyre!("batch submission control channel closed"))?;
                            self.apply_latest_control()
                        }
                        candidate = self.candidate_source.next_candidate(anchor) => {
                            match candidate {
                                Ok(candidate) => {
                                    self.recovery_delay = INITIAL_RETRY_DELAY;
                                    let candidate = ActiveBatchCandidate {
                                        generation: self.role.generation,
                                        candidate,
                                    };
                                    if self.candidate_extends_anchor(&candidate) {
                                        SubmissionState::CheckingPortal(CandidateWork::new(candidate))
                                    } else {
                                        warn!(generation = candidate.generation, from = candidate.from, to = candidate.to, "Rejected stale or non-contiguous batch candidate");
                                        self.admission.close();
                                        SubmissionState::Reconciling {
                                            work: None,
                                            backoff_after_success: false,
                                        }
                                    }
                                }
                                Err(error) => {
                                    self.record_actor_failure(&error, "Candidate discovery failed; retrying reconciliation");
                                    self.admission.close();
                                    self.backoff(
                                        None,
                                        ResumePoint::Reconcile {
                                            backoff_after_success: false,
                                        },
                                    )
                                }
                            }
                        }
                        () = self.repair_notify.notified() => {
                            self.admission.close();
                            SubmissionState::Reconciling {
                                work: None,
                                backoff_after_success: false,
                            }
                        }
                    }
                }
                SubmissionState::CheckingPortal(work) => {
                    let mut operation = self.portal_hash_operation();
                    tokio::select! {
                        biased;
                        changed = self.control_rx.changed() => {
                            changed.map_err(|_| eyre::eyre!("batch submission control channel closed"))?;
                            self.apply_latest_control()
                        }
                        result = &mut operation => {
                            match result {
                                Ok(hash) if hash == work.candidate.batch.prev_block_hash => {
                                    let mut work = work;
                                    work.submit_started = Some(std::time::Instant::now());
                                    if self.attestation_store.is_some() {
                                        SubmissionState::ReadingThreshold(work)
                                    } else {
                                        SubmissionState::Preparing(work)
                                    }
                                }
                                Ok(_) => {
                                    self.admission.close();
                                    SubmissionState::Reconciling {
                                        work: Some(work),
                                        backoff_after_success: false,
                                    }
                                }
                                Err(error) => {
                                    self.record_candidate_failure(work.attempt, &error, "Failed reading portal state before batch submission");
                                    self.backoff(Some(work), ResumePoint::CheckPortal)
                                }
                            }
                        }
                    }
                }
                SubmissionState::ReadingThreshold(work) => {
                    let mut operation = self.threshold_operation(&work);
                    tokio::select! {
                        biased;
                        changed = self.control_rx.changed() => {
                            changed.map_err(|_| eyre::eyre!("batch submission control channel closed"))?;
                            self.apply_latest_control()
                        }
                        result = &mut operation => {
                            match result {
                                Ok(threshold) => {
                                    let store = self.attestation_store.as_ref().expect("threshold requires attestation store");
                                    SubmissionState::AwaitingSettlement {
                                        work,
                                        threshold,
                                        changes: store.subscribe_settlement_changes(),
                                        changes_open: true,
                                        portal_poll_at: tokio::time::Instant::now(),
                                    }
                                }
                                Err(error) => {
                                    self.record_candidate_failure(work.attempt, &error, "Failed reading settlement threshold; retrying");
                                    self.backoff(Some(work), ResumePoint::ReadThreshold)
                                }
                            }
                        }
                    }
                }
                SubmissionState::AwaitingSettlement {
                    mut work,
                    threshold,
                    mut changes,
                    mut changes_open,
                    portal_poll_at,
                } => {
                    let store = self
                        .attestation_store
                        .as_ref()
                        .expect("settlement wait requires store");
                    if let Some(certificate) = store.settlement_at(work.candidate.to, threshold) {
                        work.certificate = Some(certificate);
                        SubmissionState::Preparing(work)
                    } else {
                        tokio::select! {
                            biased;
                            changed = self.control_rx.changed() => {
                                changed.map_err(|_| eyre::eyre!("batch submission control channel closed"))?;
                                self.apply_latest_control()
                            }
                            changed = changes.changed(), if changes_open => {
                                if changed.is_err() {
                                    changes_open = false;
                                }
                                SubmissionState::AwaitingSettlement {
                                    work,
                                    threshold,
                                    changes,
                                    changes_open,
                                    portal_poll_at,
                                }
                            }
                            () = tokio::time::sleep_until(portal_poll_at) => {
                                SubmissionState::CheckingSettlementPortal {
                                    work,
                                    threshold,
                                    changes,
                                    changes_open,
                                }
                            }
                        }
                    }
                }
                SubmissionState::CheckingSettlementPortal {
                    work,
                    threshold,
                    changes,
                    changes_open,
                } => {
                    let mut operation = self.portal_hash_operation();
                    tokio::select! {
                        biased;
                        changed = self.control_rx.changed() => {
                            changed.map_err(|_| eyre::eyre!("batch submission control channel closed"))?;
                            self.apply_latest_control()
                        }
                        result = &mut operation => {
                            match result {
                                Ok(hash) if hash == work.candidate.batch.prev_block_hash => {
                                    SubmissionState::AwaitingSettlement {
                                        work,
                                        threshold,
                                        changes,
                                        changes_open,
                                        portal_poll_at: tokio::time::Instant::now() + PORTAL_POLL_INTERVAL,
                                    }
                                }
                                Ok(_) => {
                                    self.admission.close();
                                    SubmissionState::Reconciling {
                                        work: Some(work),
                                        backoff_after_success: false,
                                    }
                                }
                                Err(error) => {
                                    warn!(attempt = work.attempt, %error, "Failed polling Portal while awaiting settlement quorum");
                                    self.admission.close();
                                    SubmissionState::Reconciling {
                                        work: Some(work),
                                        backoff_after_success: false,
                                    }
                                }
                            }
                        }
                    }
                }
                SubmissionState::Preparing(work) => {
                    let mut operation = self.prepare_operation(&work);
                    tokio::select! {
                        biased;
                        changed = self.control_rx.changed() => {
                            changed.map_err(|_| eyre::eyre!("batch submission control channel closed"))?;
                            self.apply_latest_control()
                        }
                        result = &mut operation => {
                            match result {
                                Ok(prepared) => {
                                    if self.role_matches(&work.candidate)
                                        && self.admission.claim_send(work.candidate.generation)
                                    {
                                        SubmissionState::Submitting {
                                            work,
                                            operation: self.send_operation(prepared),
                                            shutdown_observed: false,
                                        }
                                    } else {
                                        self.apply_latest_control()
                                    }
                                }
                                Err(error) => {
                                    self.record_candidate_failure(work.attempt, &error, "Batch submission preparation failed; retrying");
                                    self.backoff(Some(work), ResumePoint::Prepare)
                                }
                            }
                        }
                    }
                }
                SubmissionState::BackingOff {
                    work,
                    resume,
                    deadline,
                } => {
                    tokio::select! {
                        biased;
                        changed = self.control_rx.changed() => {
                            changed.map_err(|_| eyre::eyre!("batch submission control channel closed"))?;
                            self.apply_latest_control()
                        }
                        () = tokio::time::sleep_until(deadline) => {
                            match resume {
                                ResumePoint::Reconcile { backoff_after_success } => {
                                    SubmissionState::Reconciling { work, backoff_after_success }
                                }
                                ResumePoint::CheckPortal => SubmissionState::CheckingPortal(work.expect("candidate retry requires work")),
                                ResumePoint::ReadThreshold => SubmissionState::ReadingThreshold(work.expect("threshold retry requires work")),
                                ResumePoint::Prepare => SubmissionState::Preparing(work.expect("preparation retry requires work")),
                            }
                        }
                    }
                }
                SubmissionState::Submitting {
                    work,
                    mut operation,
                    mut shutdown_observed,
                } => {
                    tokio::select! {
                        biased;
                        changed = self.control_rx.changed() => {
                            changed.map_err(|_| eyre::eyre!("batch submission control channel closed"))?;
                            let control = *self.control_rx.borrow_and_update();
                            self.role = control.role;
                            self.admission.close();
                            if control.stopping && !shutdown_observed {
                                warn!(generation = work.candidate.generation, to = work.candidate.to, "Draining submitted transaction during actor shutdown");
                                shutdown_observed = true;
                            }
                            SubmissionState::Submitting { work, operation, shutdown_observed }
                        }
                        result = &mut operation => {
                            self.admission.finish_send(work.candidate.generation);
                            if let Some(started) = work.submit_started {
                                self.metrics.batch_submit_latency_seconds.record(started.elapsed().as_secs_f64());
                            }
                            let control = *self.control_rx.borrow();
                            let keep_leading = !control.stopping
                                && control.role.is_leader
                                && control.role.generation == work.candidate.generation;
                            match result {
                                Ok(confirmed) => {
                                    self.confirm_candidate(work.candidate, confirmed)?;
                                    if keep_leading {
                                        self.role = control.role;
                                        SubmissionState::Discovering
                                    } else {
                                        self.apply_latest_control()
                                    }
                                }
                                Err(error) if keep_leading => {
                                    self.metrics.batch_submit_failure_total.increment(1);
                                    self.metrics.batch_submit_retry_total.increment(1);
                                    let revert_reason = decode_portal_revert(&error);
                                    warn!(attempt = work.attempt, %error, ?revert_reason, "Batch submission failed; resynchronizing before retry");
                                    self.admission.close();
                                    SubmissionState::Reconciling {
                                        work: Some(work),
                                        backoff_after_success: true,
                                    }
                                }
                                Err(_) => self.apply_latest_control(),
                            }
                        }
                    }
                }
                SubmissionState::Stopped => return Ok(()),
            };
        }
    }

    fn apply_latest_control(&mut self) -> SubmissionState {
        let control = *self.control_rx.borrow_and_update();
        self.admission.close();
        self.role = control.role;
        if control.stopping {
            SubmissionState::Stopped
        } else if control.role.is_leader {
            SubmissionState::Reconciling {
                work: None,
                backoff_after_success: false,
            }
        } else {
            self.applied_role_tx.send_replace(control.role);
            SubmissionState::Inactive
        }
    }

    fn control_matches_role(&self) -> bool {
        let control = *self.control_rx.borrow();
        !control.stopping && control.role == self.role
    }

    fn backoff(&mut self, mut work: Option<CandidateWork>, resume: ResumePoint) -> SubmissionState {
        let delay = if let Some(work) = work.as_mut() {
            let delay = work.delay;
            work.attempt = work.attempt.saturating_add(1);
            work.delay = next_retry_delay(delay);
            delay
        } else {
            let delay = self.recovery_delay;
            self.recovery_delay = next_retry_delay(delay);
            delay
        };
        SubmissionState::BackingOff {
            work,
            resume,
            deadline: tokio::time::Instant::now() + delay,
        }
    }

    fn record_actor_failure(&self, error: &eyre::Report, message: &'static str) {
        self.metrics.batch_submit_failure_total.increment(1);
        warn!(%error, "{message}");
    }

    fn record_candidate_failure(&self, attempt: u32, error: &eyre::Report, message: &'static str) {
        self.metrics.batch_submit_failure_total.increment(1);
        self.metrics.batch_submit_retry_total.increment(1);
        warn!(attempt, %error, "{message}");
    }

    fn resync_operation(&self) -> BoxFuture<'static, Result<BatchResync>> {
        let backend = self.backend.clone();
        Box::pin(async move { backend.resync().await })
    }

    fn portal_hash_operation(&self) -> BoxFuture<'static, Result<B256>> {
        let backend = self.backend.clone();
        Box::pin(async move { backend.portal_block_hash().await })
    }

    fn threshold_operation(&self, work: &CandidateWork) -> BoxFuture<'static, Result<usize>> {
        let backend = self.backend.clone();
        let batch = work.candidate.batch.clone();
        Box::pin(async move { backend.settlement_threshold(&batch).await })
    }

    fn prepare_operation(&self, work: &CandidateWork) -> BoxFuture<'static, Result<B::Prepared>> {
        let backend = self.backend.clone();
        let batch = work.candidate.batch.clone();
        let certificate = work.certificate.clone();
        Box::pin(async move { backend.prepare(&batch, certificate).await })
    }

    fn send_operation(
        &self,
        prepared: B::Prepared,
    ) -> BoxFuture<'static, Result<ConfirmedBatchSubmission>> {
        let backend = self.backend.clone();
        Box::pin(async move { backend.send(prepared).await })
    }

    fn candidate_extends_anchor(&self, candidate: &ActiveBatchCandidate) -> bool {
        let Some(anchor) = self.anchor else {
            return false;
        };
        candidate.generation == self.role.generation
            && self.role.is_leader
            && candidate.from == anchor.zone_height.saturating_add(1)
            && candidate.to == candidate.batch.zone_height
            && candidate.batch.prev_block_hash == anchor.zone_block_hash
            && candidate.batch.prev_processed_deposit_hash == anchor.processed_deposit_hash
            && candidate.batch.prev_deposit_number == anchor.processed_deposit_number
    }

    fn role_matches(&self, candidate: &ActiveBatchCandidate) -> bool {
        self.role.is_leader && self.role.generation == candidate.generation
    }

    fn confirm_candidate(
        &mut self,
        candidate: ActiveBatchCandidate,
        confirmed: ConfirmedBatchSubmission,
    ) -> Result<()> {
        let candidate = candidate.candidate;
        let blocks = candidate.to.saturating_sub(candidate.from) + 1;
        self.metrics.batch_submit_success_total.increment(1);
        self.metrics.batch_size_blocks.record(blocks as f64);
        self.metrics
            .withdrawals_per_batch
            .record(candidate.withdrawals.len() as f64);
        self.metrics
            .latest_zone_block_submitted_to_l1
            .set(candidate.to as f64);
        info!(
            generation = self.role.generation,
            from = candidate.from,
            to = candidate.to,
            tempo_block_number = candidate.batch.tempo_block_number,
            withdrawal_batch_index = confirmed.withdrawal_batch_index,
            ?confirmed.withdrawal_queue_index,
            withdrawal_queue_hash = %candidate.batch.withdrawal_queue_hash,
            "Batch successfully submitted to L1"
        );
        match confirmed.withdrawal_queue_index {
            Some(portal_index) if !candidate.withdrawals.is_empty() => {
                let count = candidate.withdrawals.len();
                if !self
                    .withdrawal_store
                    .lock()
                    .add_batch(portal_index, candidate.withdrawals)
                {
                    debug!(
                        portal_index,
                        count, "Withdrawal cache full; dropped reconstructible far-tail payload"
                    );
                }
            }
            None if !candidate.batch.withdrawal_queue_hash.is_zero()
                || !candidate.withdrawals.is_empty() =>
            {
                warn!(
                    withdrawal_queue_hash = %candidate.batch.withdrawal_queue_hash,
                    withdrawal_count = candidate.withdrawals.len(),
                    "submitBatch emitted NO_QUEUE_INDEX for a batch that locally had withdrawals"
                );
            }
            _ => {}
        }
        if let Some(store) = &self.attestation_store {
            store.remove_submitted(candidate.to);
        }
        let anchor = SubmissionAnchor {
            zone_height: candidate.to,
            zone_block_hash: candidate.batch.next_block_hash,
            processed_deposit_hash: candidate.batch.next_processed_deposit_hash,
            processed_deposit_number: candidate.batch.next_deposit_number,
        };
        self.publish_anchor(anchor)?;
        self.withdrawal_notify.notify_one();
        Ok(())
    }

    fn apply_resync(&mut self, resync: BatchResync) -> Result<()> {
        self.withdrawal_store
            .lock()
            .replace_page(resync.pending_withdrawals);
        self.withdrawal_notify.notify_one();
        if let Some(store) = &self.attestation_store {
            store.remove_submitted(resync.anchor.zone_height);
        }
        self.publish_anchor(resync.anchor)?;
        self.metrics
            .latest_zone_block_submitted_to_l1
            .set(resync.anchor.zone_height as f64);
        Ok(())
    }

    fn publish_anchor(&mut self, anchor: SubmissionAnchor) -> Result<()> {
        if self
            .anchor
            .is_some_and(|current| anchor.zone_height < current.zone_height)
        {
            warn!(
                from = self.anchor.expect("checked above").zone_height,
                to = anchor.zone_height,
                "Portal submission height regressed; accepting canonical resync"
            );
        }
        self.anchor = Some(anchor);
        self.progress_tx.send_replace(Some(anchor));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ops::{Deref, DerefMut},
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use alloy_primitives::B256;
    use parking_lot::Mutex;
    use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc};

    use super::*;

    struct TestCandidateSource {
        receiver: AsyncMutex<mpsc::Receiver<BatchCandidate>>,
        observed_anchor: watch::Sender<Option<SubmissionAnchor>>,
    }

    impl BatchCandidateSource for TestCandidateSource {
        fn next_candidate(
            &self,
            anchor: SubmissionAnchor,
        ) -> BoxFuture<'_, Result<BatchCandidate>> {
            self.observed_anchor.send_replace(Some(anchor));
            Box::pin(async move {
                self.receiver
                    .lock()
                    .await
                    .recv()
                    .await
                    .ok_or_else(|| eyre::eyre!("test candidate channel closed"))
            })
        }
    }

    struct TestHandle {
        actor: BatchSubmissionHandle,
        candidate_tx: mpsc::Sender<BatchCandidate>,
        observed_anchor: watch::Receiver<Option<SubmissionAnchor>>,
    }

    impl TestHandle {
        fn candidate_sender(&self) -> mpsc::Sender<BatchCandidate> {
            self.candidate_tx.clone()
        }

        async fn next_observed_anchor(&mut self) -> SubmissionAnchor {
            self.observed_anchor
                .changed()
                .await
                .expect("candidate source must remain open");
            self.observed_anchor
                .borrow_and_update()
                .expect("candidate discovery must receive an anchor")
        }
    }

    impl Deref for TestHandle {
        type Target = BatchSubmissionHandle;

        fn deref(&self) -> &Self::Target {
            &self.actor
        }
    }

    impl DerefMut for TestHandle {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.actor
        }
    }

    #[derive(Default)]
    struct MockBackendState {
        anchor: Mutex<Option<SubmissionAnchor>>,
        resyncs: AtomicUsize,
        resync_failures: AtomicUsize,
        send_failures: AtomicUsize,
        threshold_reads: AtomicUsize,
        prepares: AtomicUsize,
        sends: AtomicUsize,
        block_prepare: AtomicBool,
        block_send: AtomicBool,
        block_send_start: AtomicBool,
        prepare_started: Notify,
        prepare_release: Notify,
        send_created: Notify,
        send_poll_started: Notify,
        send_poll_release: Notify,
        send_started: Notify,
        send_release: Notify,
    }

    #[derive(Clone, Default)]
    struct MockBackend(Arc<MockBackendState>);

    impl MockBackend {
        fn with_anchor(anchor: SubmissionAnchor) -> Self {
            let backend = Self::default();
            *backend.0.anchor.lock() = Some(anchor);
            backend
        }

        fn fail_next_resyncs(&self, count: usize) {
            self.0.resync_failures.store(count, Ordering::SeqCst);
        }

        fn fail_next_sends(&self, count: usize) {
            self.0.send_failures.store(count, Ordering::SeqCst);
        }
    }

    impl BatchSubmissionBackend for MockBackend {
        type Prepared = BatchData;

        fn resync(&self) -> BoxFuture<'_, Result<BatchResync>> {
            Box::pin(async move {
                self.0.resyncs.fetch_add(1, Ordering::SeqCst);
                if self
                    .0
                    .resync_failures
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_ok()
                {
                    return Err(eyre::eyre!("mock resync failure"));
                }
                Ok(BatchResync {
                    anchor: self.0.anchor.lock().expect("mock anchor"),
                    pending_withdrawals: WithdrawalPage {
                        head: 0,
                        tail: 0,
                        batches: Default::default(),
                    },
                })
            })
        }

        fn portal_block_hash(&self) -> BoxFuture<'_, Result<B256>> {
            Box::pin(async move { Ok(self.0.anchor.lock().expect("mock anchor").zone_block_hash) })
        }

        fn settlement_threshold<'a>(
            &'a self,
            _batch: &'a BatchData,
        ) -> BoxFuture<'a, Result<usize>> {
            Box::pin(async move {
                self.0.threshold_reads.fetch_add(1, Ordering::SeqCst);
                Ok(1)
            })
        }

        fn prepare<'a>(
            &'a self,
            batch: &'a BatchData,
            _certificate: Option<SettlementCertificate>,
        ) -> BoxFuture<'a, Result<Self::Prepared>> {
            let batch = batch.clone();
            Box::pin(async move {
                self.0.prepares.fetch_add(1, Ordering::SeqCst);
                self.0.prepare_started.notify_one();
                if self.0.block_prepare.load(Ordering::SeqCst) {
                    self.0.prepare_release.notified().await;
                }
                Ok(batch)
            })
        }

        fn send(
            &self,
            _prepared: Self::Prepared,
        ) -> BoxFuture<'_, Result<ConfirmedBatchSubmission>> {
            self.0.send_created.notify_one();
            Box::pin(async move {
                if self.0.block_send_start.load(Ordering::SeqCst) {
                    self.0.send_poll_started.notify_one();
                    self.0.send_poll_release.notified().await;
                }
                self.0.sends.fetch_add(1, Ordering::SeqCst);
                self.0.send_started.notify_one();
                if self
                    .0
                    .send_failures
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_ok()
                {
                    return Err(eyre::eyre!("mock send failure"));
                }
                if self.0.block_send.load(Ordering::SeqCst) {
                    self.0.send_release.notified().await;
                }
                Ok(ConfirmedBatchSubmission {
                    withdrawal_batch_index: 1,
                    withdrawal_queue_index: None,
                })
            })
        }
    }

    fn anchor() -> SubmissionAnchor {
        SubmissionAnchor {
            zone_height: 10,
            zone_block_hash: B256::repeat_byte(1),
            processed_deposit_hash: B256::repeat_byte(2),
            processed_deposit_number: 3,
        }
    }

    fn candidate() -> BatchCandidate {
        let anchor = anchor();
        BatchCandidate {
            from: 11,
            to: 20,
            batch: BatchData {
                zone_height: 20,
                tempo_block_number: 100,
                prev_block_hash: anchor.zone_block_hash,
                next_block_hash: B256::repeat_byte(4),
                prev_processed_deposit_hash: anchor.processed_deposit_hash,
                next_processed_deposit_hash: B256::repeat_byte(5),
                prev_deposit_number: anchor.processed_deposit_number,
                next_deposit_number: 6,
                withdrawal_queue_hash: B256::ZERO,
                withdrawal_batch_index: 1,
            },
            withdrawals: Vec::new(),
        }
    }

    fn spawn_actor(
        backend: MockBackend,
    ) -> (MockBackend, TestHandle, tokio::task::JoinHandle<Result<()>>) {
        spawn_actor_with_store(backend, None)
    }

    fn spawn_actor_with_store(
        backend: MockBackend,
        attestation_store: Option<AttestationStore>,
    ) -> (MockBackend, TestHandle, tokio::task::JoinHandle<Result<()>>) {
        let (candidate_tx, candidate_rx) = mpsc::channel(1);
        let (observed_anchor_tx, observed_anchor_rx) = watch::channel(None);
        let (actor, handle) = batch_submission_actor(
            backend.clone(),
            TestCandidateSource {
                receiver: AsyncMutex::new(candidate_rx),
                observed_anchor: observed_anchor_tx,
            },
            attestation_store,
            SharedWithdrawalStore::new(),
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
        );
        let task = tokio::spawn(actor.run());
        (
            backend,
            TestHandle {
                actor: handle,
                candidate_tx,
                observed_anchor: observed_anchor_rx,
            },
            task,
        )
    }

    async fn stop_actor(handle: &BatchSubmissionHandle, task: tokio::task::JoinHandle<Result<()>>) {
        handle.shutdown();
        task.await.unwrap().unwrap();
    }

    #[test]
    fn retry_backoff_doubles_and_caps_at_one_minute() {
        let mut delay = INITIAL_RETRY_DELAY;
        assert_eq!(next_retry_delay(delay), Duration::from_millis(400));

        for _ in 0..20 {
            delay = next_retry_delay(delay);
        }
        assert_eq!(delay, MAX_RETRY_DELAY);
        assert_eq!(next_retry_delay(delay), MAX_RETRY_DELAY);
    }

    #[tokio::test]
    async fn discovery_restarts_from_confirmed_anchor() {
        let (_, mut handle, task) = spawn_actor(MockBackend::with_anchor(anchor()));
        handle
            .set_role(BatchSubmissionRole::leader(1))
            .await
            .unwrap();

        assert_eq!(handle.next_observed_anchor().await, anchor());
        handle.candidate_sender().send(candidate()).await.unwrap();

        let candidate = candidate();
        let confirmed_anchor = SubmissionAnchor {
            zone_height: candidate.to,
            zone_block_hash: candidate.batch.next_block_hash,
            processed_deposit_hash: candidate.batch.next_processed_deposit_hash,
            processed_deposit_number: candidate.batch.next_deposit_number,
        };
        assert_eq!(handle.next_observed_anchor().await, confirmed_anchor);
        stop_actor(&handle, task).await;
    }

    #[tokio::test]
    async fn retries_failed_promotion_resync_without_stopping_actor() {
        let backend = MockBackend::with_anchor(anchor());
        backend.fail_next_resyncs(1);
        let (backend, mut handle, task) = spawn_actor(backend);

        tokio::time::timeout(
            Duration::from_secs(1),
            handle.set_role(BatchSubmissionRole::leader(1)),
        )
        .await
        .expect("actor must retry promotion resync")
        .unwrap();

        assert!(backend.0.resyncs.load(Ordering::SeqCst) >= 2);
        assert!(!task.is_finished());
        stop_actor(&handle, task).await;
    }

    #[tokio::test]
    async fn shutdown_interrupts_reconciliation_backoff() {
        let backend = MockBackend::with_anchor(anchor());
        backend.fail_next_resyncs(usize::MAX);
        let (backend, handle, task) = spawn_actor(backend);
        let mut promotion_handle = handle.clone();
        let promotion = tokio::spawn(async move {
            promotion_handle
                .set_role(BatchSubmissionRole::leader(1))
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while backend.0.resyncs.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actor must enter reconciliation backoff");

        handle.shutdown();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("shutdown must interrupt reconciliation backoff")
            .unwrap()
            .unwrap();
        assert!(promotion.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn retries_active_candidate_after_transient_resync_failure() {
        let backend = MockBackend::with_anchor(anchor());
        let (backend, mut handle, task) = spawn_actor(backend);
        handle
            .set_role(BatchSubmissionRole::leader(1))
            .await
            .unwrap();
        backend.fail_next_sends(1);
        backend.fail_next_resyncs(2);
        handle.candidate_sender().send(candidate()).await.unwrap();

        tokio::time::timeout(Duration::from_secs(3), async {
            while backend.0.sends.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("candidate must resume after Portal resynchronization recovers");

        assert!(backend.0.resyncs.load(Ordering::SeqCst) >= 4);
        assert_eq!(backend.0.sends.load(Ordering::SeqCst), 2);
        stop_actor(&handle, task).await;
    }

    #[tokio::test]
    async fn accepts_portal_height_regression_during_resync() {
        let (backend, mut handle, task) = spawn_actor(MockBackend::with_anchor(anchor()));
        handle
            .set_role(BatchSubmissionRole::leader(1))
            .await
            .unwrap();
        let mut progress = handle.subscribe_progress();
        progress.borrow_and_update();

        let regressed = SubmissionAnchor {
            zone_height: 9,
            ..anchor()
        };
        *backend.0.anchor.lock() = Some(regressed);
        let mut stale = candidate();
        stale.from = 99;
        handle.candidate_sender().send(stale).await.unwrap();

        tokio::time::timeout(Duration::from_secs(1), progress.changed())
            .await
            .expect("regressed resync must publish progress")
            .unwrap();
        assert_eq!(*progress.borrow(), Some(regressed));
        assert!(!task.is_finished());
        stop_actor(&handle, task).await;
    }

    #[tokio::test]
    async fn demotion_during_preparation_prevents_send() {
        let backend = MockBackend::with_anchor(anchor());
        backend.0.block_prepare.store(true, Ordering::SeqCst);
        let (backend, mut handle, task) = spawn_actor(backend);
        handle
            .set_role(BatchSubmissionRole::leader(1))
            .await
            .unwrap();
        handle.candidate_sender().send(candidate()).await.unwrap();
        backend.0.prepare_started.notified().await;

        handle
            .set_role(BatchSubmissionRole::inactive(2))
            .await
            .unwrap();
        backend.0.prepare_release.notify_waiters();
        tokio::task::yield_now().await;

        assert_eq!(backend.0.sends.load(Ordering::SeqCst), 0);
        stop_actor(&handle, task).await;
    }

    #[tokio::test]
    async fn shutdown_during_preparation_prevents_send_and_exits() {
        let backend = MockBackend::with_anchor(anchor());
        backend.0.block_prepare.store(true, Ordering::SeqCst);
        let (backend, mut handle, task) = spawn_actor(backend);
        handle
            .set_role(BatchSubmissionRole::leader(1))
            .await
            .unwrap();
        handle.candidate_sender().send(candidate()).await.unwrap();
        backend.0.prepare_started.notified().await;

        handle.shutdown();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("shutdown must interrupt unsent preparation")
            .unwrap()
            .unwrap();
        assert_eq!(backend.0.sends.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn shutdown_interrupts_settlement_wait() {
        let (backend, mut handle, task) = spawn_actor_with_store(
            MockBackend::with_anchor(anchor()),
            Some(AttestationStore::default()),
        );
        handle
            .set_role(BatchSubmissionRole::leader(1))
            .await
            .unwrap();
        handle.candidate_sender().send(candidate()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while backend.0.threshold_reads.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actor must reach settlement wait");

        handle.shutdown();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("shutdown must interrupt settlement wait")
            .unwrap()
            .unwrap();
        assert_eq!(backend.0.prepares.load(Ordering::SeqCst), 0);
        assert_eq!(backend.0.sends.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn demotion_after_send_claim_drains_and_publishes_progress() {
        let backend = MockBackend::with_anchor(anchor());
        backend.0.block_send_start.store(true, Ordering::SeqCst);
        let (backend, mut handle, task) = spawn_actor(backend);
        handle
            .set_role(BatchSubmissionRole::leader(1))
            .await
            .unwrap();
        let mut progress = handle.subscribe_progress();
        handle.candidate_sender().send(candidate()).await.unwrap();
        backend.0.send_created.notified().await;
        backend.0.send_poll_started.notified().await;

        let mut demotion_handle = handle.clone();
        let demotion = tokio::spawn(async move {
            demotion_handle
                .set_role(BatchSubmissionRole::inactive(2))
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !matches!(
                *handle.admission.state.lock().expect("admission fence lock"),
                AdmissionState::Closed
            ) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("demotion must close the admission fence");
        assert!(!demotion.is_finished());
        assert_eq!(handle.current_progress(), Some(anchor()));
        assert_eq!(backend.0.sends.load(Ordering::SeqCst), 0);

        backend.0.send_poll_release.notify_waiters();
        demotion.await.unwrap().unwrap();
        while progress
            .borrow()
            .is_none_or(|anchor| anchor.zone_height < 20)
        {
            progress.changed().await.unwrap();
        }

        assert_eq!(backend.0.sends.load(Ordering::SeqCst), 1);
        assert_eq!(progress.borrow().unwrap().zone_height, 20);
        stop_actor(&handle, task).await;
    }

    #[tokio::test]
    async fn shutdown_after_send_drains_and_publishes_progress() {
        let backend = MockBackend::with_anchor(anchor());
        backend.0.block_send.store(true, Ordering::SeqCst);
        let (backend, mut handle, task) = spawn_actor(backend);
        handle
            .set_role(BatchSubmissionRole::leader(1))
            .await
            .unwrap();
        let mut progress = handle.subscribe_progress();
        handle.candidate_sender().send(candidate()).await.unwrap();
        backend.0.send_started.notified().await;

        handle.shutdown();
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        backend.0.send_release.notify_waiters();
        task.await.unwrap().unwrap();
        while progress
            .borrow()
            .is_none_or(|anchor| anchor.zone_height < 20)
        {
            progress.changed().await.unwrap();
        }

        assert_eq!(backend.0.sends.load(Ordering::SeqCst), 1);
        assert_eq!(progress.borrow().unwrap().zone_height, 20);
    }
}
