use std::{
    convert::Infallible,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_primitives::{B256, Bytes};
use openraft::{CommittedLeaderId, Entry, EntryPayload, LogId, storage::RaftStateMachine};
use zone_node::{
    fast_quorum::{CommittedBlock, FastRaftConfig, ReplicatedBlockInput},
    fast_raft_state_machine::{
        AppliedBlock, CommittedTransferRecord, DurableRaftStateMachine,
        DurableStateMachineExecution,
    },
};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "zone-fast-state-machine-{}-{nonce}",
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

#[derive(Default)]
struct Executor {
    restored: Mutex<Vec<AppliedBlock>>,
}

impl DurableStateMachineExecution for Executor {
    type Error = Infallible;

    fn apply_committed<'a>(
        &'a self,
        log_id: LogId<u64>,
        input: &'a ReplicatedBlockInput,
    ) -> Pin<Box<dyn Future<Output = Result<CommittedBlock, Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            Ok(CommittedBlock {
                input_digest: input.digest(),
                block_height: log_id.index,
                block_hash: B256::with_last_byte(log_id.index as u8),
                state_root: B256::repeat_byte(2),
                receipts_root: B256::repeat_byte(3),
            })
        })
    }

    fn restore_committed<'a>(
        &'a self,
        blocks: &'a [AppliedBlock],
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            *self.restored.lock().unwrap() = blocks.to_vec();
            Ok(())
        })
    }

    fn committed_transfer(
        &self,
        _transfer_id: B256,
    ) -> Result<Option<CommittedTransferRecord>, Self::Error> {
        Ok(None)
    }
}

fn input() -> ReplicatedBlockInput {
    ReplicatedBlockInput {
        epoch: 7,
        parent_hash: B256::repeat_byte(1),
        block_input: Bytes::from_static(b"attributes"),
        transactions: vec![
            Bytes::from_static(b"opening"),
            Bytes::from_static(b"payment"),
        ],
        l1_inputs: Bytes::from_static(b"anchor"),
        replay_witness: Bytes::from_static(b"witness"),
    }
}

#[tokio::test]
async fn committed_state_machine_restores_exact_applied_prefix_before_serving() {
    let directory = TestDirectory::new();
    let first_executor = Arc::new(Executor::default());
    let mut state_machine = DurableRaftStateMachine::open(&directory.0, first_executor.clone())
        .await
        .unwrap();
    let log_id = LogId::new(CommittedLeaderId::new(3, 1), 8);
    let value = input();
    let responses = state_machine
        .apply([Entry::<FastRaftConfig> {
            log_id,
            payload: EntryPayload::Normal(value.clone()),
        }])
        .await
        .unwrap();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].input_digest, value.digest());
    drop(state_machine);

    let recovered_executor = Arc::new(Executor::default());
    let mut recovered = DurableRaftStateMachine::open(&directory.0, recovered_executor.clone())
        .await
        .unwrap();
    let restored = recovered_executor.restored.lock().unwrap().clone();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].log_id, log_id);
    assert_eq!(restored[0].input, value);
    assert_eq!(restored[0].output, responses[0]);

    let (last_applied, _) = recovered.applied_state().await.unwrap();
    assert_eq!(last_applied, Some(log_id));
}
