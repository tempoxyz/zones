//! Persistent, role-aware batch submission actor.
//!
//! The actor owns submission control flow while its backend owns bounded portal operations. It is
//! deliberately independent from the node role controller so state transitions can be tested
//! without an L1 RPC server.

use std::{
    future::pending,
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::B256;
use eyre::Result;
use futures::future::BoxFuture;
use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    AttestationStore, abi,
    attestation::SettlementCertificate,
    settlement::{BatchData, WithdrawalPage},
    withdrawals::SharedWithdrawalStore,
};

const CANDIDATE_CHANNEL_CAPACITY: usize = 1;
const MAX_RETRIES: u32 = 3;
const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(200);
const PORTAL_POLL_INTERVAL: Duration = Duration::from_secs(1);

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

/// One ordered, generation-tagged boundary produced by the zone monitor.
#[derive(Debug, Clone)]
pub struct BatchCandidate {
    pub generation: u64,
    pub from: u64,
    pub to: u64,
    pub batch: BatchData,
    pub withdrawals: Vec<abi::Withdrawal>,
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
    fn settlement_threshold(&self, batch: &BatchData) -> BoxFuture<'_, Result<usize>>;
    fn prepare(
        &self,
        batch: &BatchData,
        certificate: Option<SettlementCertificate>,
    ) -> BoxFuture<'_, Result<Self::Prepared>>;

    fn send(&self, prepared: Self::Prepared) -> BoxFuture<'_, Result<ConfirmedBatchSubmission>>;
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
    role_tx: watch::Sender<BatchSubmissionRole>,
    applied_role_rx: watch::Receiver<BatchSubmissionRole>,
    candidate_tx: mpsc::Sender<BatchCandidate>,
    progress_rx: watch::Receiver<Option<SubmissionAnchor>>,
    admission: Arc<AdmissionFence>,
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
        self.role_tx.send_replace(role);
        while *self.applied_role_rx.borrow_and_update() != role {
            self.applied_role_rx
                .changed()
                .await
                .map_err(|_| eyre::eyre!("batch submission actor stopped applying role updates"))?;
        }
        Ok(())
    }

    pub fn candidate_sender(&self) -> mpsc::Sender<BatchCandidate> {
        self.candidate_tx.clone()
    }

    pub fn subscribe_progress(&self) -> watch::Receiver<Option<SubmissionAnchor>> {
        self.progress_rx.clone()
    }

    pub fn current_progress(&self) -> Option<SubmissionAnchor> {
        *self.progress_rx.borrow()
    }
}

/// Create the persistent actor and its cloneable control handle.
pub fn batch_submission_actor<B: BatchSubmissionBackend>(
    backend: B,
    attestation_store: Option<AttestationStore>,
    withdrawal_store: SharedWithdrawalStore,
    withdrawal_notify: Arc<Notify>,
    repair_notify: Arc<Notify>,
) -> (BatchSubmissionActor<B>, BatchSubmissionHandle) {
    let initial_role = BatchSubmissionRole::inactive(0);
    let (role_tx, role_rx) = watch::channel(initial_role);
    let (applied_role_tx, applied_role_rx) = watch::channel(initial_role);
    let (candidate_tx, candidate_rx) = mpsc::channel(CANDIDATE_CHANNEL_CAPACITY);
    let (progress_tx, progress_rx) = watch::channel(None);
    let admission = Arc::new(AdmissionFence::default());
    (
        BatchSubmissionActor {
            backend: Arc::new(backend),
            attestation_store,
            withdrawal_store,
            withdrawal_notify,
            repair_notify,
            role_rx,
            applied_role_tx,
            candidate_rx,
            progress_tx,
            admission: admission.clone(),
            role: initial_role,
            anchor: None,
        },
        BatchSubmissionHandle {
            role_tx,
            applied_role_rx,
            candidate_tx,
            progress_rx,
            admission,
        },
    )
}

/// Persistent event loop for ordered L1 batch submission.
pub struct BatchSubmissionActor<B: BatchSubmissionBackend> {
    backend: Arc<B>,
    attestation_store: Option<AttestationStore>,
    withdrawal_store: SharedWithdrawalStore,
    withdrawal_notify: Arc<Notify>,
    repair_notify: Arc<Notify>,
    role_rx: watch::Receiver<BatchSubmissionRole>,
    applied_role_tx: watch::Sender<BatchSubmissionRole>,
    candidate_rx: mpsc::Receiver<BatchCandidate>,
    progress_tx: watch::Sender<Option<SubmissionAnchor>>,
    admission: Arc<AdmissionFence>,
    role: BatchSubmissionRole,
    anchor: Option<SubmissionAnchor>,
}

impl<B: BatchSubmissionBackend> BatchSubmissionActor<B> {
    pub async fn run(mut self, shutdown: CancellationToken) -> Result<()> {
        info!("Batch submission actor started");
        loop {
            if !self.role.is_leader {
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => return Ok(()),
                    changed = self.role_rx.changed() => {
                        changed.map_err(|_| eyre::eyre!("batch submission role channel closed"))?;
                        if !self.apply_role_or_retry(&shutdown).await {
                            return Ok(());
                        }
                    }
                    candidate = self.candidate_rx.recv() => {
                        let Some(candidate) = candidate else {
                            return Err(eyre::eyre!("batch candidate channel closed"));
                        };
                        debug!(generation = candidate.generation, to = candidate.to, "Rejected batch candidate while inactive");
                    }
                }
                continue;
            }

            tokio::select! {
                biased;
                () = shutdown.cancelled() => return Ok(()),
                changed = self.role_rx.changed() => {
                    changed.map_err(|_| eyre::eyre!("batch submission role channel closed"))?;
                    if !self.apply_role_or_retry(&shutdown).await {
                        return Ok(());
                    }
                }
                candidate = self.candidate_rx.recv() => {
                    let Some(candidate) = candidate else {
                        return Err(eyre::eyre!("batch candidate channel closed"));
                    };
                    if !self.candidate_extends_anchor(&candidate) {
                        warn!(generation = candidate.generation, from = candidate.from, to = candidate.to, "Rejected stale or non-contiguous batch candidate");
                        if let Err(error) = self.resync().await
                            && !self.retry_after_error(error, &shutdown).await
                        {
                            return Ok(());
                        }
                        continue;
                    }
                    if let Err(error) = self.process_candidate(candidate, &shutdown).await
                        && !self.retry_after_error(error, &shutdown).await
                    {
                        return Ok(());
                    }
                }
                () = self.repair_notify.notified() => {
                    if let Err(error) = self.resync().await
                        && !self.retry_after_error(error, &shutdown).await
                    {
                        return Ok(());
                    }
                }
            }
        }
    }

    async fn apply_role(&mut self) -> Result<()> {
        let next = *self.role_rx.borrow_and_update();
        self.role = next;
        if next.is_leader {
            self.admission.close();
            self.resync().await?;
            // A newer role may have arrived while resynchronization was in flight.
            let current = *self.role_rx.borrow();
            if current == next {
                self.admission.open(next.generation);
            } else {
                self.role = current;
                self.admission.close();
            }
        } else {
            self.admission.close();
        }
        self.applied_role_tx.send_replace(self.role);
        Ok(())
    }

    /// Reconcile the current role after a transient portal failure without dropping the
    /// node-lifetime actor or its control channels.
    async fn apply_role_or_retry(&mut self, shutdown: &CancellationToken) -> bool {
        match self.apply_role().await {
            Ok(()) => true,
            Err(error) => self.retry_after_error(error, shutdown).await,
        }
    }

    /// Keep the actor alive across recoverable backend failures. A failed reconciliation leaves
    /// the admission fence closed until a later resync succeeds.
    async fn retry_after_error(
        &mut self,
        error: eyre::Report,
        shutdown: &CancellationToken,
    ) -> bool {
        self.admission.close();
        warn!(%error, "Batch submission actor operation failed; retrying reconciliation");

        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => return false,
                () = tokio::time::sleep(INITIAL_RETRY_DELAY) => {}
            }

            match self.apply_role().await {
                Ok(()) => return true,
                Err(error) => {
                    warn!(%error, "Batch submission actor reconciliation retry failed");
                }
            }
        }
    }

    fn candidate_extends_anchor(&self, candidate: &BatchCandidate) -> bool {
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

    async fn process_candidate(
        &mut self,
        candidate: BatchCandidate,
        shutdown: &CancellationToken,
    ) -> Result<()> {
        let mut attempt = 1;
        let mut delay = INITIAL_RETRY_DELAY;
        loop {
            if !self.role_matches(&candidate) {
                return Ok(());
            }
            let backend = self.backend.clone();
            let portal_hash = match self
                .await_unsent(backend.portal_block_hash(), &candidate, shutdown)
                .await?
            {
                Some(Ok(hash)) => hash,
                None => return Ok(()),
                Some(Err(error)) if attempt < MAX_RETRIES => {
                    warn!(attempt, %error, "Failed reading portal state before batch submission");
                    if !self.wait_backoff(delay, &candidate, shutdown).await? {
                        return Ok(());
                    }
                    attempt += 1;
                    delay *= 2;
                    continue;
                }
                Some(Err(error)) => {
                    warn!(attempt, %error, "Portal preflight failed; resynchronizing");
                    self.resync().await?;
                    return Err(error);
                }
            };
            if portal_hash != candidate.batch.prev_block_hash {
                self.resync().await?;
                return Ok(());
            }

            let certificate = if let Some(store) = self.attestation_store.clone() {
                let backend = self.backend.clone();
                let threshold = match self
                    .await_unsent(
                        backend.settlement_threshold(&candidate.batch),
                        &candidate,
                        shutdown,
                    )
                    .await?
                {
                    Some(threshold) => threshold?,
                    None => return Ok(()),
                };
                match self
                    .wait_for_certificate(&store, &candidate, threshold, shutdown)
                    .await?
                {
                    Some(certificate) => Some(certificate),
                    None => return Ok(()),
                }
            } else {
                None
            };

            let backend = self.backend.clone();
            let prepared = match self
                .await_unsent(
                    backend.prepare(&candidate.batch, certificate),
                    &candidate,
                    shutdown,
                )
                .await?
            {
                Some(prepared) => prepared?,
                None => return Ok(()),
            };
            if !self.role_matches(&candidate) || !self.admission.claim_send(candidate.generation) {
                return Ok(());
            }

            let backend = self.backend.clone();
            let result = self
                .drive_sent(backend.send(prepared), &candidate, shutdown)
                .await;
            match result {
                Ok(confirmed) => {
                    self.confirm_candidate(candidate, confirmed)?;
                    return Ok(());
                }
                Err(error) if self.role_matches(&candidate) && attempt < MAX_RETRIES => {
                    warn!(attempt, %error, "Batch submission failed; resynchronizing before retry");
                    self.resync().await?;
                    if self
                        .anchor
                        .is_some_and(|anchor| anchor.zone_height >= candidate.to)
                    {
                        return Ok(());
                    }
                    if !self.candidate_extends_anchor(&candidate) {
                        return Ok(());
                    }
                    if !self.wait_backoff(delay, &candidate, shutdown).await? {
                        return Ok(());
                    }
                    attempt += 1;
                    delay *= 2;
                }
                Err(error) => {
                    warn!(attempt, %error, "Batch submission failed; resynchronizing");
                    self.resync().await?;
                    if self.role_matches(&candidate) {
                        return Err(error);
                    }
                    return Ok(());
                }
            }
        }
    }

    async fn wait_for_certificate(
        &mut self,
        store: &AttestationStore,
        candidate: &BatchCandidate,
        threshold: usize,
        shutdown: &CancellationToken,
    ) -> Result<Option<SettlementCertificate>> {
        let mut changes = store.subscribe_settlement_changes();
        let mut portal_poll = tokio::time::interval(PORTAL_POLL_INTERVAL);
        portal_poll.tick().await;
        loop {
            if let Some(certificate) = store.settlement_at(candidate.to, threshold) {
                return Ok(Some(certificate));
            }
            tokio::select! {
                biased;
                () = shutdown.cancelled() => return Ok(None),
                changed = self.role_rx.changed() => {
                    changed.map_err(|_| eyre::eyre!("batch submission role channel closed"))?;
                    self.apply_role().await?;
                    if !self.role_matches(candidate) {
                        return Ok(None);
                    }
                }
                queued = self.candidate_rx.recv() => {
                    if let Some(queued) = queued {
                        debug!(generation = queued.generation, to = queued.to, "Rejected candidate while a boundary is active");
                    }
                }
                changed = changes.changed() => {
                    if changed.is_err() {
                        pending::<()>().await;
                    }
                }
                _ = portal_poll.tick() => {
                    let portal_hash = self.backend.portal_block_hash().await?;
                    if portal_hash != candidate.batch.prev_block_hash {
                        self.resync().await?;
                        return Ok(None);
                    }
                }
            }
        }
    }

    async fn await_unsent<T>(
        &mut self,
        operation: BoxFuture<'_, Result<T>>,
        candidate: &BatchCandidate,
        shutdown: &CancellationToken,
    ) -> Result<Option<Result<T>>> {
        tokio::pin!(operation);
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => return Ok(None),
                changed = self.role_rx.changed() => {
                    changed.map_err(|_| eyre::eyre!("batch submission role channel closed"))?;
                    self.apply_role().await?;
                    if !self.role_matches(candidate) {
                        return Ok(None);
                    }
                }
                queued = self.candidate_rx.recv() => {
                    if let Some(queued) = queued {
                        debug!(generation = queued.generation, to = queued.to, "Rejected candidate while a boundary is active");
                    }
                }
                result = &mut operation => return Ok(Some(result)),
            }
        }
    }

    async fn drive_sent(
        &mut self,
        operation: BoxFuture<'_, Result<ConfirmedBatchSubmission>>,
        candidate: &BatchCandidate,
        shutdown: &CancellationToken,
    ) -> Result<ConfirmedBatchSubmission> {
        tokio::pin!(operation);
        let mut shutdown_observed = false;
        let mut role_changed_while_sending = false;
        loop {
            tokio::select! {
                biased;
                changed = self.role_rx.changed() => {
                    changed.map_err(|_| eyre::eyre!("batch submission role channel closed"))?;
                    // The send claim is already linearized. Defer applying and acknowledging the
                    // role change until this mutation has drained.
                    self.role = *self.role_rx.borrow_and_update();
                    self.admission.close();
                    role_changed_while_sending = true;
                }
                queued = self.candidate_rx.recv() => {
                    if let Some(queued) = queued {
                        debug!(generation = queued.generation, to = queued.to, "Rejected candidate while a transaction is active");
                    }
                }
                () = shutdown.cancelled(), if !shutdown_observed => {
                    shutdown_observed = true;
                    warn!(generation = candidate.generation, to = candidate.to, "Draining submitted transaction during actor shutdown");
                }
                result = &mut operation => {
                    self.admission.finish_send(candidate.generation);
                    if role_changed_while_sending {
                        self.apply_role().await?;
                    }
                    return result;
                }
            }
        }
    }

    async fn wait_backoff(
        &mut self,
        delay: Duration,
        candidate: &BatchCandidate,
        shutdown: &CancellationToken,
    ) -> Result<bool> {
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => return Ok(false),
                changed = self.role_rx.changed() => {
                    changed.map_err(|_| eyre::eyre!("batch submission role channel closed"))?;
                    self.apply_role().await?;
                    if !self.role_matches(candidate) {
                        return Ok(false);
                    }
                }
                queued = self.candidate_rx.recv() => {
                    if let Some(queued) = queued {
                        debug!(generation = queued.generation, to = queued.to, "Rejected candidate during retry backoff");
                    }
                }
                () = &mut sleep => return Ok(true),
            }
        }
    }

    fn role_matches(&self, candidate: &BatchCandidate) -> bool {
        self.role.is_leader && self.role.generation == candidate.generation
    }

    fn confirm_candidate(
        &mut self,
        candidate: BatchCandidate,
        confirmed: ConfirmedBatchSubmission,
    ) -> Result<()> {
        if let Some(portal_index) = confirmed.withdrawal_queue_index
            && !candidate.withdrawals.is_empty()
        {
            self.withdrawal_store
                .lock()
                .add_batch(portal_index, candidate.withdrawals);
        }
        if let Some(store) = &self.attestation_store {
            store.remove_submitted(candidate.to);
        }
        self.publish_anchor(SubmissionAnchor {
            zone_height: candidate.to,
            zone_block_hash: candidate.batch.next_block_hash,
            processed_deposit_hash: candidate.batch.next_processed_deposit_hash,
            processed_deposit_number: candidate.batch.next_deposit_number,
        })?;
        self.withdrawal_notify.notify_one();
        Ok(())
    }

    async fn resync(&mut self) -> Result<()> {
        let resync = self.backend.resync().await?;
        self.withdrawal_store
            .lock()
            .replace_page(resync.pending_withdrawals);
        if let Some(store) = &self.attestation_store {
            store.remove_submitted(resync.anchor.zone_height);
        }
        self.publish_anchor(resync.anchor)?;
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use alloy_primitives::B256;
    use parking_lot::Mutex;
    use tokio::sync::Notify;

    use super::*;

    #[derive(Default)]
    struct MockBackendState {
        anchor: Mutex<Option<SubmissionAnchor>>,
        resyncs: AtomicUsize,
        resync_failures: AtomicUsize,
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

        fn settlement_threshold(&self, _batch: &BatchData) -> BoxFuture<'_, Result<usize>> {
            Box::pin(async { Ok(1) })
        }

        fn prepare(
            &self,
            batch: &BatchData,
            _certificate: Option<SettlementCertificate>,
        ) -> BoxFuture<'_, Result<Self::Prepared>> {
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

    fn candidate(generation: u64) -> BatchCandidate {
        let anchor = anchor();
        BatchCandidate {
            generation,
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
    ) -> (
        MockBackend,
        BatchSubmissionHandle,
        CancellationToken,
        tokio::task::JoinHandle<Result<()>>,
    ) {
        let (actor, handle) = batch_submission_actor(
            backend.clone(),
            None,
            SharedWithdrawalStore::new(),
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
        );
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(actor.run(shutdown.clone()));
        (backend, handle, shutdown, task)
    }

    #[tokio::test]
    async fn inactive_actor_drains_stale_candidate_before_promotion() {
        let (backend, mut handle, shutdown, task) = spawn_actor(MockBackend::with_anchor(anchor()));
        handle.candidate_sender().send(candidate(0)).await.unwrap();
        tokio::task::yield_now().await;
        handle
            .set_role(BatchSubmissionRole::leader(1))
            .await
            .unwrap();

        assert_eq!(backend.0.sends.load(Ordering::SeqCst), 0);
        assert_eq!(handle.current_progress(), Some(anchor()));
        shutdown.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn retries_failed_promotion_resync_without_stopping_actor() {
        let backend = MockBackend::with_anchor(anchor());
        backend.fail_next_resyncs(1);
        let (backend, mut handle, shutdown, task) = spawn_actor(backend);

        tokio::time::timeout(
            Duration::from_secs(1),
            handle.set_role(BatchSubmissionRole::leader(1)),
        )
        .await
        .expect("actor must retry promotion resync")
        .unwrap();

        assert!(backend.0.resyncs.load(Ordering::SeqCst) >= 2);
        assert!(!task.is_finished());
        shutdown.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn accepts_portal_height_regression_during_resync() {
        let (backend, mut handle, shutdown, task) = spawn_actor(MockBackend::with_anchor(anchor()));
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
        let mut stale = candidate(1);
        stale.from = 99;
        handle.candidate_sender().send(stale).await.unwrap();

        tokio::time::timeout(Duration::from_secs(1), progress.changed())
            .await
            .expect("regressed resync must publish progress")
            .unwrap();
        assert_eq!(*progress.borrow(), Some(regressed));
        assert!(!task.is_finished());
        shutdown.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn demotion_during_preparation_prevents_send() {
        let backend = MockBackend::with_anchor(anchor());
        backend.0.block_prepare.store(true, Ordering::SeqCst);
        let (backend, mut handle, shutdown, task) = spawn_actor(backend);
        handle
            .set_role(BatchSubmissionRole::leader(1))
            .await
            .unwrap();
        handle.candidate_sender().send(candidate(1)).await.unwrap();
        backend.0.prepare_started.notified().await;

        handle
            .set_role(BatchSubmissionRole::inactive(2))
            .await
            .unwrap();
        backend.0.prepare_release.notify_waiters();
        tokio::task::yield_now().await;

        assert_eq!(backend.0.sends.load(Ordering::SeqCst), 0);
        shutdown.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn demotion_after_send_drains_and_publishes_progress() {
        let backend = MockBackend::with_anchor(anchor());
        backend.0.block_send.store(true, Ordering::SeqCst);
        let (backend, mut handle, shutdown, task) = spawn_actor(backend);
        handle
            .set_role(BatchSubmissionRole::leader(1))
            .await
            .unwrap();
        let mut progress = handle.subscribe_progress();
        handle.candidate_sender().send(candidate(1)).await.unwrap();
        backend.0.send_started.notified().await;

        let mut demotion_handle = handle.clone();
        let demotion = tokio::spawn(async move {
            demotion_handle
                .set_role(BatchSubmissionRole::inactive(2))
                .await
        });
        tokio::task::yield_now().await;
        assert!(!demotion.is_finished());
        assert_eq!(handle.current_progress(), Some(anchor()));
        backend.0.send_release.notify_waiters();
        demotion.await.unwrap().unwrap();
        while progress
            .borrow()
            .is_none_or(|anchor| anchor.zone_height < 20)
        {
            progress.changed().await.unwrap();
        }

        assert_eq!(backend.0.sends.load(Ordering::SeqCst), 1);
        assert_eq!(progress.borrow().unwrap().zone_height, 20);
        shutdown.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn demotion_waits_for_a_claimed_send_before_acknowledging() {
        let backend = MockBackend::with_anchor(anchor());
        backend.0.block_send_start.store(true, Ordering::SeqCst);
        let (backend, mut handle, shutdown, task) = spawn_actor(backend);
        handle
            .set_role(BatchSubmissionRole::leader(1))
            .await
            .unwrap();
        handle.candidate_sender().send(candidate(1)).await.unwrap();
        backend.0.send_created.notified().await;
        backend.0.send_poll_started.notified().await;

        let mut demotion_handle = handle.clone();
        let demotion = tokio::spawn(async move {
            demotion_handle
                .set_role(BatchSubmissionRole::inactive(2))
                .await
        });
        tokio::task::yield_now().await;

        assert!(!demotion.is_finished());
        assert_eq!(backend.0.sends.load(Ordering::SeqCst), 0);
        backend.0.send_poll_release.notify_waiters();
        demotion.await.unwrap().unwrap();
        assert_eq!(backend.0.sends.load(Ordering::SeqCst), 1);

        shutdown.cancel();
        task.await.unwrap().unwrap();
    }
}
