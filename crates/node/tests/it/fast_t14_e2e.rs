use std::{
    convert::Infallible,
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_primitives::{B256, Bytes};
use tempo_precompiles::PATH_USD_ADDRESS;
use tempo_zone_contracts::{FAST_TRANSFER_ADDRESS, IFastTransfer};
use zone_fast_transfer::DurableJournal;
use zone_node::fast_quorum::{
    ApplyCommittedError, CanonicalReplay, CommittedBlock, CommittedPrefix, CommittedPrefixStore,
    DeterministicBlockExecution, ReplicatedBlockInput, apply_committed_entry,
    reconcile_before_canonical_read,
};

use crate::utils::{DEFAULT_TIMEOUT, start_local_zone_with_fixture};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("zone-fast-quorum-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct DeterministicExecutor {
    mismatched_digest: bool,
}

impl DeterministicBlockExecution for DeterministicExecutor {
    type Error = Infallible;

    fn execute_replicated(
        &self,
        input: &ReplicatedBlockInput,
    ) -> Result<CommittedBlock, Self::Error> {
        Ok(CommittedBlock {
            input_digest: if self.mismatched_digest {
                B256::repeat_byte(0xff)
            } else {
                input.digest()
            },
            block_height: 9,
            block_hash: B256::repeat_byte(2),
            state_root: B256::repeat_byte(3),
            receipts_root: B256::repeat_byte(4),
        })
    }
}

struct ReplayHarness {
    height: AtomicU64,
    replayed: AtomicBool,
}

impl CanonicalReplay for ReplayHarness {
    type Error = Infallible;

    fn canonical_height(&self) -> Result<u64, Self::Error> {
        Ok(self.height.load(Ordering::SeqCst))
    }

    fn replay_committed_through(&self, target: &CommittedPrefix) -> Result<(), Self::Error> {
        self.replayed.store(true, Ordering::SeqCst);
        self.height.store(target.block_height, Ordering::SeqCst);
        Ok(())
    }
}

fn replicated_input() -> ReplicatedBlockInput {
    ReplicatedBlockInput {
        epoch: 4,
        parent_hash: B256::repeat_byte(1),
        block_input: Bytes::from_static(b"same-anchor-attributes"),
        transactions: vec![
            Bytes::from_static(b"opening-system-transaction"),
            Bytes::from_static(b"native-payment-transaction"),
        ],
        l1_inputs: Bytes::from_static(b"finalized-anchor"),
        replay_witness: Bytes::from_static(b"complete-witness"),
    }
}

#[test]
fn committed_execution_is_fsynced_before_canonical_reads_and_survives_restart() {
    let directory = TestDirectory::new();
    let input = replicated_input();
    {
        let journal = DurableJournal::open(&directory.0).unwrap();
        let block = apply_committed_entry(
            &journal,
            &DeterministicExecutor {
                mismatched_digest: false,
            },
            3,
            8,
            &input,
        )
        .unwrap();
        assert_eq!(block.block_height, 9);
    }

    let journal = DurableJournal::open(&directory.0).unwrap();
    let committed = journal.committed_prefix().unwrap().unwrap();
    assert_eq!(committed.term, 3);
    assert_eq!(committed.index, 8);
    assert_eq!(committed.block_height, 9);
    let replay = journal.replicated_blocks_from(0).unwrap();
    assert_eq!(replay.len(), 1);
    assert_eq!(replay[0].transactions.len(), 2);
    assert_eq!(replay[0].witness, b"complete-witness");

    let canonical = ReplayHarness {
        height: AtomicU64::new(7),
        replayed: AtomicBool::new(false),
    };
    assert_eq!(
        reconcile_before_canonical_read(&journal, &canonical).unwrap(),
        Some(committed)
    );
    assert!(canonical.replayed.load(Ordering::SeqCst));
    assert_eq!(canonical.height.load(Ordering::SeqCst), 9);
}

#[test]
fn mismatched_execution_digest_is_not_persisted_as_a_committed_prefix() {
    let directory = TestDirectory::new();
    let journal = DurableJournal::open(&directory.0).unwrap();
    let result = apply_committed_entry(
        &journal,
        &DeterministicExecutor {
            mismatched_digest: true,
        },
        3,
        8,
        &replicated_input(),
    );
    assert!(matches!(
        result,
        Err(ApplyCommittedError::InputDigest { .. })
    ));
    assert_eq!(journal.committed_prefix().unwrap(), None);
    assert!(journal.replicated_blocks_from(0).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn native_fast_transfer_is_unavailable_before_compatible_epoch_activation() -> eyre::Result<()>
{
    reth_tracing::init_test_tracing();
    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;
    fixture.inject_empty_block(zone.deposit_queue());
    zone.wait_for_tempo_block_number(1, DEFAULT_TIMEOUT).await?;

    let fast = IFastTransfer::new(FAST_TRANSFER_ADDRESS, zone.provider());
    let result = fast.poolState(PATH_USD_ADDRESS).call().await;
    assert!(
        result.is_err(),
        "the native fast-transfer API must not become usable before a finalized compatible epoch"
    );
    Ok(())
}
