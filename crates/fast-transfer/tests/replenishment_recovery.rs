use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_primitives::{Address, B256, U256};
use zone_fast_transfer::{
    DepositRecord, DurableJournal, LegObservation, ReplenishmentBridge, ReplenishmentError,
    ReplenishmentJob, ReplenishmentStage, ReplenishmentWorker, ReplenishmentWorkerError,
    WorkerFuture, WorkerProgress,
    replenishment::{InventoryContribution, TokenAmount, TransactionCost, WithdrawalRecord},
};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "zone-fast-replenishment-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn job() -> ReplenishmentJob {
    ReplenishmentJob::allocate(
        B256::repeat_byte(1),
        Address::repeat_byte(2),
        Address::repeat_byte(3),
        Address::repeat_byte(4),
        Address::repeat_byte(5),
        Address::repeat_byte(6),
        vec![
            InventoryContribution {
                transfer_id: B256::repeat_byte(7),
                amount: U256::from(60),
            },
            InventoryContribution {
                transfer_id: B256::repeat_byte(8),
                amount: U256::from(40),
            },
        ],
    )
    .unwrap()
}

fn withdrawal() -> WithdrawalRecord {
    WithdrawalRecord {
        transaction_intent_hash: B256::repeat_byte(9),
        signer_nonce: 12,
        fallback_nonce: Some(13),
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

struct ScriptedBridge {
    store: Arc<DurableJournal>,
    observations: Mutex<VecDeque<LegObservation>>,
}

impl ReplenishmentBridge for ScriptedBridge {
    fn prepare_withdrawal(
        &self,
        _: &ReplenishmentJob,
    ) -> Result<WithdrawalRecord, ReplenishmentWorkerError> {
        Ok(withdrawal())
    }

    fn submit_or_reconcile_withdrawal<'a>(
        &'a self,
        job: &'a ReplenishmentJob,
        record: &'a WithdrawalRecord,
    ) -> WorkerFuture<'a, LegObservation> {
        Box::pin(async move {
            assert_eq!(
                self.store
                    .replenishment_job(job.job_id)
                    .unwrap()
                    .unwrap()
                    .withdrawal,
                Some(record.clone()),
                "withdrawal action must be durable before bridge I/O"
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
        _: U256,
    ) -> Result<DepositRecord, ReplenishmentWorkerError> {
        Ok(DepositRecord {
            transaction_intent_hash: B256::repeat_byte(12),
            signer_nonce: 44,
            queue_index: None,
            transaction_hash: None,
            submission_hashes: Vec::new(),
            transaction_cost: None,
            deposit_fee: None,
        })
    }

    fn submit_or_reconcile_deposit<'a>(
        &'a self,
        job: &'a ReplenishmentJob,
        record: &'a DepositRecord,
    ) -> WorkerFuture<'a, LegObservation> {
        Box::pin(async move {
            assert_eq!(
                self.store
                    .replenishment_job(job.job_id)
                    .unwrap()
                    .unwrap()
                    .deposit,
                Some(record.clone()),
                "deposit action must be durable before bridge I/O"
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
fn rpc_ambiguity_reuses_one_economic_action_after_restart() {
    let directory = TestDirectory::new();
    let mut value = job();
    assert_eq!(value.request_withdrawal(withdrawal()), Ok(true));
    {
        let journal = DurableJournal::open(&directory.0).unwrap();
        journal.persist_replenishment_job(value.clone()).unwrap();
    }

    let journal = DurableJournal::open(&directory.0).unwrap();
    let mut recovered = journal.unfinished_replenishment_jobs().unwrap().remove(0);
    assert_eq!(recovered.request_withdrawal(withdrawal()), Ok(false));

    let mut replacement = withdrawal();
    replacement.signer_nonce += 1;
    assert_eq!(
        recovered.request_withdrawal(replacement),
        Err(ReplenishmentError::ReplacementConflict)
    );
    recovered
        .reconcile_withdrawal(
            Some(B256::repeat_byte(10)),
            Some(B256::repeat_byte(11)),
            Some(4),
        )
        .unwrap();
    recovered.record_treasury_credit(U256::from(98)).unwrap();
    let deposit = DepositRecord {
        transaction_intent_hash: B256::repeat_byte(12),
        signer_nonce: 44,
        queue_index: None,
        transaction_hash: None,
        submission_hashes: Vec::new(),
        transaction_cost: None,
        deposit_fee: None,
    };
    recovered.submit_deposit(deposit.clone()).unwrap();
    assert_eq!(recovered.submit_deposit(deposit), Ok(false));
    recovered.record_pool_credit(U256::from(97)).unwrap();
    assert_eq!(recovered.stage, ReplenishmentStage::PoolCredited);
    assert_eq!(recovered.gross_amount, U256::from(100));
    assert_eq!(recovered.treasury_credit, Some(U256::from(98)));
    assert_eq!(recovered.pool_credit, Some(U256::from(97)));
}

#[test]
fn bounce_and_refund_branches_never_credit_the_pool_or_user() {
    let mut bounced = job();
    bounced.request_withdrawal(withdrawal()).unwrap();
    assert_eq!(bounced.record_withdrawal_bounce(), Ok(true));
    assert_eq!(bounced.stage, ReplenishmentStage::WithdrawalBounced);
    assert_eq!(
        bounced.record_treasury_credit(U256::from(100)),
        Err(ReplenishmentError::OutOfOrder)
    );
    assert_eq!(bounced.pool_credit, None);

    let mut refunded = job();
    refunded.request_withdrawal(withdrawal()).unwrap();
    refunded.record_treasury_credit(U256::from(99)).unwrap();
    refunded
        .submit_deposit(DepositRecord {
            transaction_intent_hash: B256::repeat_byte(13),
            signer_nonce: 45,
            queue_index: Some(7),
            transaction_hash: Some(B256::repeat_byte(14)),
            submission_hashes: vec![B256::repeat_byte(14)],
            transaction_cost: None,
            deposit_fee: None,
        })
        .unwrap();
    assert_eq!(refunded.record_deposit_refund_pending(), Ok(true));
    assert_eq!(refunded.stage, ReplenishmentStage::DepositRefundPending);
    assert_eq!(
        refunded.record_pool_credit(U256::from(99)),
        Err(ReplenishmentError::OutOfOrder)
    );
    assert_eq!(refunded.pool_credit, None);
}

#[test]
fn fast_recipient_cannot_be_any_background_job_recipient() {
    let recipient = Address::repeat_byte(2);
    assert_eq!(
        ReplenishmentJob::allocate(
            B256::repeat_byte(20),
            recipient,
            Address::repeat_byte(3),
            Address::repeat_byte(4),
            Address::repeat_byte(5),
            recipient,
            vec![InventoryContribution {
                transfer_id: B256::repeat_byte(21),
                amount: U256::from(1),
            }],
        ),
        Err(ReplenishmentError::RecipientReuse)
    );
}

#[test]
fn worker_persists_ambiguous_identities_and_completes_refund_without_pool_credit() {
    futures::executor::block_on(async {
        let directory = TestDirectory::new();
        let store = Arc::new(DurableJournal::open(&directory.0).unwrap());
        let value = job();
        store.persist_replenishment_job(value.clone()).unwrap();
        let bridge = ScriptedBridge {
            store: store.clone(),
            observations: Mutex::new(VecDeque::from([
                LegObservation::Pending {
                    transaction_hash: Some(B256::repeat_byte(30)),
                    queue_index: Some(3),
                    accepted_batch_hash: Some(B256::repeat_byte(31)),
                },
                LegObservation::TreasuryCredited {
                    transaction_hash: B256::repeat_byte(30),
                    queue_index: 3,
                    accepted_batch_hash: B256::repeat_byte(31),
                    net_amount: U256::from(98),
                    fallback_nonce: 13,
                    withdrawal_index: 3,
                    sender_tag: B256::repeat_byte(33),
                    source_transaction_cost: TransactionCost {
                        transaction_hash: B256::repeat_byte(30),
                        payer: value.source_inventory,
                        token: None,
                        amount: U256::ZERO,
                    },
                    withdrawal_fee: TokenAmount {
                        token: Address::repeat_byte(9),
                        amount: U256::from(2),
                    },
                    l1_transaction_cost: TransactionCost {
                        transaction_hash: B256::repeat_byte(31),
                        payer: value.treasury,
                        token: None,
                        amount: U256::ZERO,
                    },
                },
                LegObservation::DepositRefundPending {
                    transaction_hash: B256::repeat_byte(32),
                    queue_index: 5,
                    transaction_cost: TransactionCost {
                        transaction_hash: B256::repeat_byte(32),
                        payer: value.treasury,
                        token: None,
                        amount: U256::ZERO,
                    },
                    deposit_fee: TokenAmount {
                        token: Address::repeat_byte(9),
                        amount: U256::ONE,
                    },
                },
                LegObservation::Pending {
                    transaction_hash: None,
                    queue_index: None,
                    accepted_batch_hash: None,
                },
                LegObservation::TreasuryRefunded {
                    amount: U256::from(97),
                },
            ])),
        };
        let worker = ReplenishmentWorker::new(store.clone(), bridge);

        assert_eq!(
            worker.step(value.job_id).await.unwrap(),
            WorkerProgress::Advanced(ReplenishmentStage::WithdrawalRequested)
        );
        assert_eq!(
            worker.step(value.job_id).await.unwrap(),
            WorkerProgress::Advanced(ReplenishmentStage::WithdrawalRequested)
        );
        assert_eq!(
            worker.step(value.job_id).await.unwrap(),
            WorkerProgress::Advanced(ReplenishmentStage::TreasuryFunded)
        );
        assert_eq!(
            worker.step(value.job_id).await.unwrap(),
            WorkerProgress::Advanced(ReplenishmentStage::DepositSubmitted)
        );
        assert_eq!(
            worker.step(value.job_id).await.unwrap(),
            WorkerProgress::Advanced(ReplenishmentStage::DepositRefundPending)
        );
        assert_eq!(
            worker.step(value.job_id).await.unwrap(),
            WorkerProgress::Pending(ReplenishmentStage::DepositRefundPending)
        );
        assert_eq!(
            worker.step(value.job_id).await.unwrap(),
            WorkerProgress::Complete(ReplenishmentStage::TreasuryRefunded)
        );

        let completed = store.replenishment_job(value.job_id).unwrap().unwrap();
        assert_eq!(completed.pool_credit, None);
        assert_eq!(completed.treasury_refund, Some(U256::from(97)));
        assert_eq!(completed.deposit_refund, completed.treasury);
    });
}
