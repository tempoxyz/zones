use std::{
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_primitives::B256;
use zone_fast_transfer::{
    DeliveryRecord, DeliveryStore, DeliveryTransition, DurableJournal, JournalError,
    ReplicatedBlockInput, Tombstone, TombstoneOutcome,
};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "zone-fast-transfer-{name}-{}-{nonce}",
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

#[test]
fn transport_acknowledgment_survives_restart_but_never_compacts_unsettled_work() {
    let directory = TestDirectory::new("delivery");
    let record = DeliveryRecord::queued(4, 1, B256::repeat_byte(1), vec![1, 2, 3]);
    {
        let journal = DurableJournal::open(&directory.0).unwrap();
        journal.persist_outgoing(9, &record).unwrap();
        journal
            .transition(9, 4, 1, DeliveryTransition::TransportAcknowledged)
            .unwrap();
    }

    let journal = DurableJournal::open(&directory.0).unwrap();
    let unfinished = journal.load_unfinished(9).unwrap();
    assert_eq!(unfinished.len(), 1);
    assert_eq!(
        unfinished[0].transition,
        DeliveryTransition::TransportAcknowledged
    );
    assert!(!unfinished[0].compactable());

    journal
        .transition(9, 4, 1, DeliveryTransition::TerminalReceived)
        .unwrap();
    drop(journal);
    let journal = DurableJournal::open(&directory.0).unwrap();
    assert_eq!(journal.load_unfinished(9).unwrap().len(), 1);
    journal
        .transition(9, 4, 1, DeliveryTransition::SourceDisposed)
        .unwrap();
    assert!(journal.load_unfinished(9).unwrap().is_empty());
}

#[test]
fn durable_cursor_cannot_skip_a_gap_across_restart() {
    let directory = TestDirectory::new("cursor");
    {
        let journal = DurableJournal::open(&directory.0).unwrap();
        journal.persist_incoming(8, 2, 1, b"one").unwrap();
        journal.persist_incoming(8, 2, 3, b"three").unwrap();
        assert!(matches!(
            journal.advance_contiguous_cursor(8, 2, 3),
            Err(JournalError::CursorGap)
        ));
        journal.advance_contiguous_cursor(8, 2, 1).unwrap();
    }
    let journal = DurableJournal::open(&directory.0).unwrap();
    assert!(matches!(
        journal.advance_contiguous_cursor(8, 2, 3),
        Err(JournalError::CursorGap)
    ));
    journal.persist_incoming(8, 2, 2, b"two").unwrap();
    journal.advance_contiguous_cursor(8, 2, 3).unwrap();
}

#[test]
fn snapshot_retains_terminal_barriers_and_complete_replay_witnesses() {
    let directory = TestDirectory::new("snapshot");
    let tombstone = Tombstone {
        transfer_id: B256::repeat_byte(10),
        intent_hash: B256::repeat_byte(11),
        outcome: TombstoneOutcome::Paid,
    };
    let block = ReplicatedBlockInput {
        log_term: 3,
        log_index: 7,
        block_height: 6,
        block_hash: B256::repeat_byte(12),
        state_root: B256::repeat_byte(13),
        block_input: b"attributes".to_vec(),
        transactions: vec![b"opening".to_vec(), b"payment".to_vec()],
        l1_execution_input: b"anchor".to_vec(),
        witness: b"complete-witness".to_vec(),
    };
    {
        let journal = DurableJournal::open(&directory.0).unwrap();
        assert!(journal.persist_tombstone(tombstone).unwrap());
        assert!(journal.persist_replicated_block(block.clone()).unwrap());
        journal.snapshot().unwrap();
    }
    let journal = DurableJournal::open(&directory.0).unwrap();
    assert_eq!(
        journal.tombstone(tombstone.transfer_id).unwrap(),
        Some(tombstone)
    );
    assert_eq!(journal.replicated_blocks_from(0).unwrap(), vec![block]);

    let conflict = Tombstone {
        intent_hash: B256::repeat_byte(99),
        ..tombstone
    };
    assert!(matches!(
        journal.persist_tombstone(conflict),
        Err(JournalError::Conflict)
    ));
}

#[test]
fn crash_torn_tail_is_truncated_to_last_fsynced_complete_record() {
    let directory = TestDirectory::new("torn-tail");
    let tombstone = Tombstone {
        transfer_id: B256::repeat_byte(20),
        intent_hash: B256::repeat_byte(21),
        outcome: TombstoneOutcome::Refunded,
    };
    {
        let journal = DurableJournal::open(&directory.0).unwrap();
        journal.persist_tombstone(tombstone).unwrap();
    }
    let path = directory.0.join("journal.bin");
    let stable_length = std::fs::metadata(&path).unwrap().len();
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"FTJ1\0\0").unwrap();
    file.sync_all().unwrap();
    drop(file);

    let journal = DurableJournal::open(&directory.0).unwrap();
    assert_eq!(
        journal.tombstone(tombstone.transfer_id).unwrap(),
        Some(tombstone)
    );
    assert_eq!(std::fs::metadata(path).unwrap().len(), stable_length);
}

#[test]
fn retry_schedule_is_bounded_and_replayable() {
    let mut record = DeliveryRecord::queued(1, 1, B256::repeat_byte(30), vec![]);
    let expected_millis = [50, 100, 200, 400, 800, 1_600, 2_000, 2_000];
    for expected in expected_millis {
        assert_eq!(record.record_attempt(0).unwrap().as_millis(), expected);
    }
    assert_eq!(record.transfer_id, B256::repeat_byte(30));
}
