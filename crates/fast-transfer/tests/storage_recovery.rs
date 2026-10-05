//! Durable signing and replay-barrier acceptance tests.
//!
//! Fault injection between individual writes/fsyncs still requires the public crash hooks listed
//! in `instant-tests-status.md`. These tests cover the public durable boundary that exists now.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_primitives::{Address, B256};
use zone_fast_transfer::{DurableJournal, SigningRecord, Tombstone, TombstoneOutcome};
use zone_primitives::fast_transfer::SignatureBytes;

fn hash(byte: u8) -> B256 {
    B256::from([byte; 32])
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock is after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "zone-fast-transfer-{name}-{}-{nonce}",
            std::process::id()
        ));
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        if self.0.starts_with(std::env::temp_dir()) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

#[test]
fn signing_record_retains_original_digest_term_index_and_signature_after_reopen() {
    let directory = TestDirectory::new("signing");
    let record = SigningRecord {
        digest: hash(0x11),
        signer: Address::from([0x12; 20]),
        signature: SignatureBytes([0x13; 65]),
        log_term: 7,
        log_index: 19,
    };

    {
        let journal = DurableJournal::open(directory.path()).unwrap();
        assert!(journal.persist_signing_record(record.clone()).unwrap());
        assert!(!journal.persist_signing_record(record.clone()).unwrap());
        assert_eq!(
            journal
                .signing_record(record.digest, record.signer)
                .unwrap(),
            Some(record.clone())
        );

        let mut conflicting = record.clone();
        conflicting.log_index += 1;
        assert!(journal.persist_signing_record(conflicting).is_err());
    }

    let reopened = DurableJournal::open(directory.path()).unwrap();
    assert_eq!(
        reopened
            .signing_record(record.digest, record.signer)
            .unwrap(),
        Some(record)
    );
}

#[test]
fn terminal_id_and_original_intent_binding_survive_snapshot_and_reopen() {
    let directory = TestDirectory::new("tombstone");
    let paid = Tombstone {
        transfer_id: hash(0x21),
        intent_hash: hash(0x22),
        outcome: TombstoneOutcome::Paid,
    };
    let rejected = Tombstone {
        transfer_id: hash(0x23),
        intent_hash: hash(0x24),
        outcome: TombstoneOutcome::Rejected,
    };

    {
        let journal = DurableJournal::open(directory.path()).unwrap();
        assert!(journal.persist_tombstone(paid).unwrap());
        assert!(journal.persist_tombstone(rejected).unwrap());
        assert!(!journal.persist_tombstone(paid).unwrap());
        assert!(
            journal
                .persist_tombstone(Tombstone {
                    intent_hash: hash(0xff),
                    ..paid
                })
                .is_err(),
            "same transfer ID accepted a substituted original intent"
        );
        journal.snapshot().unwrap();
    }

    let reopened = DurableJournal::open(directory.path()).unwrap();
    assert_eq!(reopened.tombstone(paid.transfer_id).unwrap(), Some(paid));
    assert_eq!(
        reopened.tombstone(rejected.transfer_id).unwrap(),
        Some(rejected)
    );
}
