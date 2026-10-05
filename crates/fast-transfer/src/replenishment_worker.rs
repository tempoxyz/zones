//! Durable driver for the two canonical bridge legs of pool replenishment.
//!
//! The driver owns ordering and persistence, while implementations of [`ReplenishmentBridge`]
//! own ZoneOutbox transaction construction, Tempo L1 outcome reads, encrypted deposits, and
//! destination-pool balance observations. Bridge implementations must return only finalized,
//! canonical observations; an RPC acceptance string is always [`LegObservation::Pending`].

use std::{future::Future, pin::Pin, sync::Arc};

use alloy_primitives::{B256, U256};

use crate::{
    journal::DurableJournal,
    replenishment::{
        DepositRecord, ReplenishmentError, ReplenishmentJob, ReplenishmentStage, TokenAmount,
        TransactionCost, WithdrawalRecord,
    },
};

/// Heap-owned future used by bridge and durable-store implementations.
pub type WorkerFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ReplenishmentWorkerError>> + Send + 'a>>;

/// Canonical observation of a submitted bridge leg.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LegObservation {
    /// Submission is absent, pending, or ambiguous. Known identities are persisted monotonically.
    Pending {
        transaction_hash: Option<B256>,
        queue_index: Option<u64>,
        accepted_batch_hash: Option<B256>,
    },
    /// A source submission candidate or canonical atomic allocation receipt was observed. The
    /// actual fallback identity is absent until `canonical_transaction_hash` is present.
    WithdrawalPending {
        submission_hash: Option<B256>,
        canonical_transaction_hash: Option<B256>,
        fallback_nonce: Option<u64>,
        withdrawal_index: Option<u64>,
        sender_tag: Option<B256>,
        source_transaction_cost: Option<TransactionCost>,
        withdrawal_fee: Option<TokenAmount>,
        queue_index: Option<u64>,
        accepted_batch_hash: Option<B256>,
    },
    /// A destination submission candidate or canonical DepositMade receipt was observed. Pool
    /// availability still requires the committed destination native credit below.
    DepositPending {
        submission_hash: Option<B256>,
        canonical_transaction_hash: Option<B256>,
        transaction_cost: Option<TransactionCost>,
        deposit_fee: Option<TokenAmount>,
        queue_index: Option<u64>,
    },
    /// The source withdrawal finalized and produced this actual L1 treasury credit.
    TreasuryCredited {
        transaction_hash: B256,
        queue_index: u64,
        accepted_batch_hash: B256,
        net_amount: U256,
        fallback_nonce: u64,
        withdrawal_index: u64,
        sender_tag: B256,
        source_transaction_cost: TransactionCost,
        withdrawal_fee: TokenAmount,
        l1_transaction_cost: TransactionCost,
    },
    /// The withdrawal was canonically consumed as a bounce, not a treasury payment.
    WithdrawalBounced {
        transaction_hash: B256,
        queue_index: u64,
        accepted_batch_hash: B256,
        fallback_nonce: u64,
        withdrawal_index: u64,
        sender_tag: B256,
        source_transaction_cost: TransactionCost,
        withdrawal_fee: TokenAmount,
        l1_transaction_cost: TransactionCost,
    },
    /// The destination deposit finalized and the pool received this actual net amount.
    PoolCredited {
        transaction_hash: B256,
        queue_index: u64,
        net_amount: U256,
        transaction_cost: TransactionCost,
        deposit_fee: TokenAmount,
    },
    /// The destination deposit failed and its operator refund is pending reconciliation.
    DepositRefundPending {
        transaction_hash: B256,
        queue_index: u64,
        transaction_cost: TransactionCost,
        deposit_fee: TokenAmount,
    },
    /// Bounced source inventory is canonically restored to the operator account.
    InventoryRestored { amount: U256 },
    /// Failed-deposit funds are canonically restored to the operator treasury.
    TreasuryRefunded { amount: U256 },
}

/// Crash-durable storage for replenishment jobs.
pub trait ReplenishmentStore: Send + Sync {
    /// Load the latest job image.
    fn load(&self, job_id: B256) -> Result<Option<ReplenishmentJob>, ReplenishmentWorkerError>;

    /// Persist and fsync a monotonic job image before the worker performs external I/O.
    fn persist(&self, job: &ReplenishmentJob) -> Result<(), ReplenishmentWorkerError>;
}

impl ReplenishmentStore for DurableJournal {
    fn load(&self, job_id: B256) -> Result<Option<ReplenishmentJob>, ReplenishmentWorkerError> {
        self.replenishment_job(job_id)
            .map_err(|error| ReplenishmentWorkerError::Storage(error.to_string()))
    }

    fn persist(&self, job: &ReplenishmentJob) -> Result<(), ReplenishmentWorkerError> {
        self.persist_replenishment_job(job.clone())
            .map(|_| ())
            .map_err(|error| ReplenishmentWorkerError::Storage(error.to_string()))
    }
}

impl<T> ReplenishmentStore for Arc<T>
where
    T: ReplenishmentStore + ?Sized,
{
    fn load(&self, job_id: B256) -> Result<Option<ReplenishmentJob>, ReplenishmentWorkerError> {
        (**self).load(job_id)
    }

    fn persist(&self, job: &ReplenishmentJob) -> Result<(), ReplenishmentWorkerError> {
        (**self).persist(job)
    }
}

/// Adapter over the real ZoneOutbox, Tempo L1 portal, and encrypted-deposit mechanisms.
pub trait ReplenishmentBridge: Send + Sync {
    /// Construct the exact withdrawal action and signer nonce without submitting it.
    fn prepare_withdrawal(
        &self,
        job: &ReplenishmentJob,
    ) -> Result<WithdrawalRecord, ReplenishmentWorkerError>;

    /// Submit or reconcile the already-persisted withdrawal economic action.
    fn submit_or_reconcile_withdrawal<'a>(
        &'a self,
        job: &'a ReplenishmentJob,
        record: &'a WithdrawalRecord,
    ) -> WorkerFuture<'a, LegObservation>;

    /// Observe the source fallback account after a canonical withdrawal bounce.
    fn reconcile_inventory_restoration<'a>(
        &'a self,
        job: &'a ReplenishmentJob,
    ) -> WorkerFuture<'a, LegObservation>;

    /// Construct the exact encrypted-deposit action and treasury signer nonce without submitting.
    fn prepare_deposit(
        &self,
        job: &ReplenishmentJob,
        treasury_credit: U256,
    ) -> Result<DepositRecord, ReplenishmentWorkerError>;

    /// Submit or reconcile the already-persisted encrypted-deposit economic action.
    fn submit_or_reconcile_deposit<'a>(
        &'a self,
        job: &'a ReplenishmentJob,
        record: &'a DepositRecord,
    ) -> WorkerFuture<'a, LegObservation>;

    /// Observe the refund recipient or claimable-refund state after deposit failure.
    fn reconcile_treasury_refund<'a>(
        &'a self,
        job: &'a ReplenishmentJob,
    ) -> WorkerFuture<'a, LegObservation>;
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        sync::{Arc, Mutex},
    };

    use alloy_primitives::Address;

    use super::*;
    use crate::replenishment::InventoryContribution;

    struct FakeBridge {
        store: Arc<DurableJournal>,
        observations: Mutex<VecDeque<LegObservation>>,
    }

    impl FakeBridge {
        fn withdrawal_record() -> WithdrawalRecord {
            WithdrawalRecord {
                transaction_intent_hash: B256::repeat_byte(8),
                signer_nonce: 41,
                fallback_nonce: None,
                withdrawal_index: None,
                sender_tag: None,
                submission_hashes: Vec::new(),
                transaction_hash: None,
                accepted_batch_hash: None,
                queue_index: None,
                source_transaction_cost: None,
                withdrawal_fee: None,
                l1_transaction_cost: None,
            }
        }

        fn deposit_record() -> DepositRecord {
            DepositRecord {
                transaction_intent_hash: B256::repeat_byte(9),
                signer_nonce: 42,
                queue_index: None,
                submission_hashes: Vec::new(),
                transaction_hash: None,
                transaction_cost: None,
                deposit_fee: None,
            }
        }
    }

    impl ReplenishmentBridge for FakeBridge {
        fn prepare_withdrawal(
            &self,
            _: &ReplenishmentJob,
        ) -> Result<WithdrawalRecord, ReplenishmentWorkerError> {
            Ok(Self::withdrawal_record())
        }

        fn submit_or_reconcile_withdrawal<'a>(
            &'a self,
            job: &'a ReplenishmentJob,
            record: &'a WithdrawalRecord,
        ) -> WorkerFuture<'a, LegObservation> {
            Box::pin(async move {
                assert_eq!(record.signer_nonce, Self::withdrawal_record().signer_nonce);
                assert_eq!(
                    record.transaction_intent_hash,
                    Self::withdrawal_record().transaction_intent_hash
                );
                assert_eq!(
                    self.store
                        .replenishment_job(job.job_id)
                        .unwrap()
                        .unwrap()
                        .withdrawal,
                    Some(record.clone()),
                    "withdrawal intent must be durable before submission"
                );
                Ok(self.observations.lock().unwrap().pop_front().unwrap())
            })
        }

        fn reconcile_inventory_restoration<'a>(
            &'a self,
            _: &'a ReplenishmentJob,
        ) -> WorkerFuture<'a, LegObservation> {
            Box::pin(async move { Ok(self.observations.lock().unwrap().pop_front().unwrap()) })
        }

        fn prepare_deposit(
            &self,
            _: &ReplenishmentJob,
            treasury_credit: U256,
        ) -> Result<DepositRecord, ReplenishmentWorkerError> {
            assert_eq!(treasury_credit, U256::from(100));
            Ok(Self::deposit_record())
        }

        fn submit_or_reconcile_deposit<'a>(
            &'a self,
            job: &'a ReplenishmentJob,
            record: &'a DepositRecord,
        ) -> WorkerFuture<'a, LegObservation> {
            Box::pin(async move {
                assert_eq!(record, &Self::deposit_record());
                assert_eq!(
                    self.store
                        .replenishment_job(job.job_id)
                        .unwrap()
                        .unwrap()
                        .deposit,
                    Some(record.clone()),
                    "deposit intent must be durable before submission"
                );
                Ok(self.observations.lock().unwrap().pop_front().unwrap())
            })
        }

        fn reconcile_treasury_refund<'a>(
            &'a self,
            _: &'a ReplenishmentJob,
        ) -> WorkerFuture<'a, LegObservation> {
            Box::pin(async move { Ok(self.observations.lock().unwrap().pop_front().unwrap()) })
        }
    }

    #[test]
    fn ambiguous_timeouts_keep_same_nonce_history_and_credit_only_observed_net() {
        futures::executor::block_on(async {
            let directory = std::env::temp_dir().join(format!(
                "zone-replenishment-worker-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let store = Arc::new(DurableJournal::open(&directory).unwrap());
            let job = ReplenishmentJob::allocate(
                B256::repeat_byte(1),
                Address::repeat_byte(2),
                Address::repeat_byte(3),
                Address::repeat_byte(4),
                Address::repeat_byte(5),
                Address::repeat_byte(6),
                vec![InventoryContribution {
                    transfer_id: B256::repeat_byte(7),
                    amount: U256::from(100),
                }],
            )
            .unwrap();
            store.persist_replenishment_job(job.clone()).unwrap();
            let bridge = FakeBridge {
                store: store.clone(),
                observations: Mutex::new(VecDeque::from([
                    LegObservation::WithdrawalPending {
                        submission_hash: Some(B256::repeat_byte(20)),
                        canonical_transaction_hash: None,
                        fallback_nonce: None,
                        withdrawal_index: None,
                        sender_tag: None,
                        source_transaction_cost: None,
                        withdrawal_fee: None,
                        queue_index: None,
                        accepted_batch_hash: None,
                    },
                    LegObservation::WithdrawalPending {
                        submission_hash: Some(B256::repeat_byte(21)),
                        canonical_transaction_hash: None,
                        fallback_nonce: None,
                        withdrawal_index: None,
                        sender_tag: None,
                        source_transaction_cost: None,
                        withdrawal_fee: None,
                        queue_index: None,
                        accepted_batch_hash: None,
                    },
                    LegObservation::TreasuryCredited {
                        transaction_hash: B256::repeat_byte(10),
                        queue_index: 3,
                        accepted_batch_hash: B256::repeat_byte(11),
                        net_amount: U256::from(100),
                        fallback_nonce: 9,
                        withdrawal_index: 2,
                        sender_tag: B256::repeat_byte(13),
                        source_transaction_cost: TransactionCost {
                            transaction_hash: B256::repeat_byte(10),
                            payer: Address::repeat_byte(2),
                            token: Some(Address::repeat_byte(14)),
                            amount: U256::from(4),
                        },
                        withdrawal_fee: TokenAmount {
                            token: Address::repeat_byte(15),
                            amount: U256::from(2),
                        },
                        l1_transaction_cost: TransactionCost {
                            transaction_hash: B256::repeat_byte(16),
                            payer: Address::repeat_byte(17),
                            token: None,
                            amount: U256::ZERO,
                        },
                    },
                    LegObservation::PoolCredited {
                        transaction_hash: B256::repeat_byte(12),
                        queue_index: 5,
                        net_amount: U256::from(97),
                        transaction_cost: TransactionCost {
                            transaction_hash: B256::repeat_byte(12),
                            payer: Address::repeat_byte(4),
                            token: Some(Address::repeat_byte(18)),
                            amount: U256::from(6),
                        },
                        deposit_fee: TokenAmount {
                            token: Address::repeat_byte(15),
                            amount: U256::from(3),
                        },
                    },
                ])),
            };
            let worker = ReplenishmentWorker::new(store.clone(), bridge);

            assert_eq!(
                worker.step(job.job_id).await.unwrap(),
                WorkerProgress::Advanced(ReplenishmentStage::WithdrawalRequested)
            );
            assert_eq!(
                worker.step(job.job_id).await.unwrap(),
                WorkerProgress::Advanced(ReplenishmentStage::WithdrawalRequested)
            );
            assert_eq!(
                worker.step(job.job_id).await.unwrap(),
                WorkerProgress::Advanced(ReplenishmentStage::WithdrawalRequested)
            );
            assert_eq!(
                worker.step(job.job_id).await.unwrap(),
                WorkerProgress::Advanced(ReplenishmentStage::TreasuryFunded)
            );
            assert_eq!(
                worker.step(job.job_id).await.unwrap(),
                WorkerProgress::Advanced(ReplenishmentStage::DepositSubmitted)
            );
            assert_eq!(
                worker.step(job.job_id).await.unwrap(),
                WorkerProgress::Complete(ReplenishmentStage::PoolCredited)
            );

            let completed = store.replenishment_job(job.job_id).unwrap().unwrap();
            assert_eq!(completed.pool_credit, Some(U256::from(97)));
            assert_eq!(completed.withdrawal_fee(), Some(U256::from(2)));
            assert_eq!(completed.deposit_fee(), Some(U256::from(3)));
            assert_eq!(
                completed.withdrawal.unwrap().submission_hashes,
                [
                    B256::repeat_byte(20),
                    B256::repeat_byte(21),
                    B256::repeat_byte(10)
                ]
            );
            drop(worker);
            drop(store);
            fs::remove_dir_all(directory).unwrap();
        });
    }
}

/// Result of one bounded worker step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerProgress {
    /// Intent or canonical evidence was persisted and another step can now run.
    Advanced(ReplenishmentStage),
    /// No new canonical evidence exists; retry the same action later.
    Pending(ReplenishmentStage),
    /// The pool credit or an operator-owned bounce/refund branch completed.
    Complete(ReplenishmentStage),
}

/// Replenishment worker failure.
#[derive(Debug, thiserror::Error)]
pub enum ReplenishmentWorkerError {
    /// The requested job is not in durable storage.
    #[error("unknown replenishment job {0}")]
    UnknownJob(B256),
    /// A bridge adapter returned an outcome that cannot apply to the current leg.
    #[error("bridge observation does not match replenishment stage")]
    InvalidObservation,
    /// A durable job transition failed.
    #[error(transparent)]
    Transition(#[from] ReplenishmentError),
    /// Durable storage failed.
    #[error("durable replenishment storage failed: {0}")]
    Storage(String),
    /// Bridge construction, submission, or canonical reconciliation failed.
    #[error("replenishment bridge failed: {0}")]
    Bridge(String),
}

/// Executes one persist-before-I/O step for one permanent job ID.
pub struct ReplenishmentWorker<S, B> {
    store: S,
    bridge: B,
}

impl<S, B> ReplenishmentWorker<S, B>
where
    S: ReplenishmentStore,
    B: ReplenishmentBridge,
{
    /// Construct a worker over a durable store and real bridge adapter.
    pub const fn new(store: S, bridge: B) -> Self {
        Self { store, bridge }
    }

    /// Advance one job by at most one durable stage.
    pub async fn step(&self, job_id: B256) -> Result<WorkerProgress, ReplenishmentWorkerError> {
        let mut job = self
            .store
            .load(job_id)?
            .ok_or(ReplenishmentWorkerError::UnknownJob(job_id))?;
        match job.stage {
            ReplenishmentStage::InventoryAllocated => {
                let record = self.bridge.prepare_withdrawal(&job)?;
                job.request_withdrawal(record)?;
                self.store.persist(&job)?;
                Ok(WorkerProgress::Advanced(job.stage))
            }
            ReplenishmentStage::WithdrawalRequested => {
                let record = job
                    .withdrawal
                    .clone()
                    .ok_or(ReplenishmentError::OutOfOrder)?;
                let observation = self
                    .bridge
                    .submit_or_reconcile_withdrawal(&job, &record)
                    .await?;
                self.apply_withdrawal_observation(&mut job, observation)
            }
            ReplenishmentStage::WithdrawalBounced => {
                let observation = self.bridge.reconcile_inventory_restoration(&job).await?;
                match observation {
                    LegObservation::InventoryRestored { amount } => {
                        job.record_inventory_restored(amount)?;
                        self.persist_complete(&job)
                    }
                    LegObservation::Pending { .. } => Ok(WorkerProgress::Pending(job.stage)),
                    _ => Err(ReplenishmentWorkerError::InvalidObservation),
                }
            }
            ReplenishmentStage::TreasuryFunded => {
                let treasury_credit = job.treasury_credit.ok_or(ReplenishmentError::OutOfOrder)?;
                let record = self.bridge.prepare_deposit(&job, treasury_credit)?;
                job.submit_deposit(record)?;
                self.store.persist(&job)?;
                Ok(WorkerProgress::Advanced(job.stage))
            }
            ReplenishmentStage::DepositSubmitted => {
                let record = job.deposit.clone().ok_or(ReplenishmentError::OutOfOrder)?;
                let observation = self
                    .bridge
                    .submit_or_reconcile_deposit(&job, &record)
                    .await?;
                self.apply_deposit_observation(&mut job, observation)
            }
            ReplenishmentStage::DepositRefundPending => {
                let observation = self.bridge.reconcile_treasury_refund(&job).await?;
                match observation {
                    LegObservation::TreasuryRefunded { amount } => {
                        job.record_treasury_refund(amount)?;
                        self.persist_complete(&job)
                    }
                    LegObservation::Pending { .. } => Ok(WorkerProgress::Pending(job.stage)),
                    _ => Err(ReplenishmentWorkerError::InvalidObservation),
                }
            }
            ReplenishmentStage::InventoryRestored
            | ReplenishmentStage::TreasuryRefunded
            | ReplenishmentStage::PoolCredited => Ok(WorkerProgress::Complete(job.stage)),
        }
    }

    fn apply_withdrawal_observation(
        &self,
        job: &mut ReplenishmentJob,
        observation: LegObservation,
    ) -> Result<WorkerProgress, ReplenishmentWorkerError> {
        match observation {
            LegObservation::WithdrawalPending {
                submission_hash,
                canonical_transaction_hash,
                fallback_nonce,
                withdrawal_index,
                sender_tag,
                source_transaction_cost,
                withdrawal_fee,
                queue_index,
                accepted_batch_hash,
            } => {
                let before = job.clone();
                if let Some(hash) = submission_hash {
                    job.record_withdrawal_submission(hash)?;
                }
                match (
                    canonical_transaction_hash,
                    fallback_nonce,
                    withdrawal_index,
                    sender_tag,
                    source_transaction_cost,
                    withdrawal_fee,
                ) {
                    (Some(hash), Some(fallback), Some(index), Some(tag), Some(cost), Some(fee)) => {
                        job.bind_canonical_withdrawal(hash, fallback, index, tag, cost, fee)?;
                    }
                    (None, None, None, None, None, None) => {}
                    _ => return Err(ReplenishmentWorkerError::InvalidObservation),
                }
                job.reconcile_withdrawal(None, accepted_batch_hash, queue_index)?;
                if *job == before {
                    Ok(WorkerProgress::Pending(job.stage))
                } else {
                    self.store.persist(job)?;
                    Ok(WorkerProgress::Advanced(job.stage))
                }
            }
            LegObservation::Pending {
                transaction_hash,
                queue_index,
                accepted_batch_hash,
            } => {
                let before = job.clone();
                job.reconcile_withdrawal(transaction_hash, accepted_batch_hash, queue_index)?;
                if *job == before {
                    Ok(WorkerProgress::Pending(job.stage))
                } else {
                    self.store.persist(job)?;
                    Ok(WorkerProgress::Advanced(job.stage))
                }
            }
            LegObservation::TreasuryCredited {
                transaction_hash,
                queue_index,
                accepted_batch_hash,
                net_amount,
                fallback_nonce,
                withdrawal_index,
                sender_tag,
                source_transaction_cost,
                withdrawal_fee,
                l1_transaction_cost,
            } => {
                job.bind_canonical_withdrawal(
                    transaction_hash,
                    fallback_nonce,
                    withdrawal_index,
                    sender_tag,
                    source_transaction_cost,
                    withdrawal_fee,
                )?;
                job.reconcile_withdrawal(None, Some(accepted_batch_hash), Some(queue_index))?;
                job.record_l1_withdrawal_cost(l1_transaction_cost)?;
                job.record_treasury_credit(net_amount)?;
                self.store.persist(job)?;
                Ok(WorkerProgress::Advanced(job.stage))
            }
            LegObservation::WithdrawalBounced {
                transaction_hash,
                queue_index,
                accepted_batch_hash,
                fallback_nonce,
                withdrawal_index,
                sender_tag,
                source_transaction_cost,
                withdrawal_fee,
                l1_transaction_cost,
            } => {
                job.bind_canonical_withdrawal(
                    transaction_hash,
                    fallback_nonce,
                    withdrawal_index,
                    sender_tag,
                    source_transaction_cost,
                    withdrawal_fee,
                )?;
                job.reconcile_withdrawal(None, Some(accepted_batch_hash), Some(queue_index))?;
                job.record_l1_withdrawal_cost(l1_transaction_cost)?;
                job.record_withdrawal_bounce()?;
                self.store.persist(job)?;
                Ok(WorkerProgress::Advanced(job.stage))
            }
            _ => Err(ReplenishmentWorkerError::InvalidObservation),
        }
    }

    fn apply_deposit_observation(
        &self,
        job: &mut ReplenishmentJob,
        observation: LegObservation,
    ) -> Result<WorkerProgress, ReplenishmentWorkerError> {
        match observation {
            LegObservation::DepositPending {
                submission_hash,
                canonical_transaction_hash,
                transaction_cost,
                deposit_fee,
                queue_index,
            } => {
                let before = job.clone();
                if let Some(hash) = submission_hash {
                    job.record_deposit_submission(hash)?;
                }
                match (canonical_transaction_hash, transaction_cost, deposit_fee) {
                    (Some(hash), Some(cost), Some(fee)) => {
                        job.bind_canonical_deposit(hash, cost, fee)?;
                    }
                    (None, None, None) => {}
                    _ => return Err(ReplenishmentWorkerError::InvalidObservation),
                }
                job.reconcile_deposit(None, queue_index)?;
                if *job == before {
                    Ok(WorkerProgress::Pending(job.stage))
                } else {
                    self.store.persist(job)?;
                    Ok(WorkerProgress::Advanced(job.stage))
                }
            }
            LegObservation::Pending {
                transaction_hash,
                queue_index,
                accepted_batch_hash: None,
            } => {
                let before = job.clone();
                job.reconcile_deposit(transaction_hash, queue_index)?;
                if *job == before {
                    Ok(WorkerProgress::Pending(job.stage))
                } else {
                    self.store.persist(job)?;
                    Ok(WorkerProgress::Advanced(job.stage))
                }
            }
            LegObservation::PoolCredited {
                transaction_hash,
                queue_index,
                net_amount,
                transaction_cost,
                deposit_fee,
            } => {
                job.bind_canonical_deposit(transaction_hash, transaction_cost, deposit_fee)?;
                job.reconcile_deposit(None, Some(queue_index))?;
                job.record_pool_credit(net_amount)?;
                self.persist_complete(job)
            }
            LegObservation::DepositRefundPending {
                transaction_hash,
                queue_index,
                transaction_cost,
                deposit_fee,
            } => {
                job.bind_canonical_deposit(transaction_hash, transaction_cost, deposit_fee)?;
                job.reconcile_deposit(None, Some(queue_index))?;
                job.record_deposit_refund_pending()?;
                self.store.persist(job)?;
                Ok(WorkerProgress::Advanced(job.stage))
            }
            _ => Err(ReplenishmentWorkerError::InvalidObservation),
        }
    }

    fn persist_complete(
        &self,
        job: &ReplenishmentJob,
    ) -> Result<WorkerProgress, ReplenishmentWorkerError> {
        self.store.persist(job)?;
        Ok(WorkerProgress::Complete(job.stage))
    }
}
