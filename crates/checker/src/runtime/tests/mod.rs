use super::*;
use crate::{
    kernel::{
        ExpectedState, Finding, FindingCategory, FindingLocation, ImportedFacts, PortalIdentity,
        State, TransitionCandidate, ZoneFacts, ZoneOperation, apply_imported, apply_zone,
    },
    metrics::CheckerMetrics,
    persistence::{BlockNumHash, ChainCut, Coverage, Identity, Persistence},
};
use alloy_primitives::{Address, B256};

fn coordinate(number: u64, domain: u8) -> BlockNumHash {
    BlockNumHash {
        number,
        hash: B256::repeat_byte(domain.wrapping_add(number as u8)),
    }
}

fn identity() -> Identity {
    Identity {
        l1_chain_id: 1,
        zone_chain_id: 2,
        zone_id: 3,
        portal: Address::repeat_byte(4),
        creation_block: B256::repeat_byte(5),
        creation_height: 0,
    }
}

fn create() -> (tempfile::TempDir, Persistence, Snapshot) {
    let directory = tempfile::tempdir().unwrap();
    let anchor = ChainCut {
        zone: coordinate(0, 0x10),
        tempo: coordinate(0, 0x40),
    };
    let (store, snapshot) = Persistence::create(
        directory.path(),
        identity(),
        anchor,
        State::awaiting(PortalIdentity {
            portal: identity().portal,
            zone_id: identity().zone_id,
            initial_token: Address::repeat_byte(6),
        }),
    )
    .unwrap();
    (directory, store, snapshot)
}

fn runtime(snapshot: Snapshot) -> Runtime {
    Runtime::new(snapshot, CheckerMetrics::default())
}

fn outputs(candidate: &TransitionCandidate) -> AuthenticatedOutputs {
    AuthenticatedOutputs {
        effects: candidate.expected_effects.to_vec(),
        state: ExpectedState {
            tempo_block_hash: candidate.expected_state.tempo_block_hash,
            tempo_block_number: candidate.expected_state.tempo_block_number,
            processed_deposit_hash: candidate.expected_state.processed_deposit_hash,
            processed_deposit_number: candidate.expected_state.processed_deposit_number,
            withdrawal_queue_hash: candidate.expected_state.withdrawal_queue_hash,
            withdrawal_batch_index: candidate.expected_state.withdrawal_batch_index,
        },
        supplies: candidate
            .expected_accounting
            .iter()
            .map(|(token, accounting)| (*token, accounting.supply))
            .collect(),
    }
}

fn authenticated_block(state: &State, number: u64) -> AuthenticatedBlock {
    let imported = ImportedFacts {
        block_hash: coordinate(number, 0x40).hash,
        block_number: number,
        ..Default::default()
    };
    let zone_facts = ZoneFacts {
        operations: vec![ZoneOperation::UpdateTempoGasRate(u128::from(number))],
        ..Default::default()
    };
    let candidate = apply_zone(apply_imported(state, &imported).unwrap(), &zone_facts).unwrap();
    AuthenticatedBlock {
        zone: coordinate(number, 0x10),
        parent: coordinate(number - 1, 0x10),
        tempo: coordinate(number, 0x40),
        tempo_parent: coordinate(number - 1, 0x40),
        imported,
        zone_facts,
        outputs: outputs(&candidate),
    }
}

#[test]
fn recovery_verifies_observed_history_sequentially() {
    let (_directory, store, snapshot) = create();
    let mut runtime = runtime(snapshot);
    runtime.observe_tip(&store, coordinate(2, 0x10)).unwrap();

    let request = AuthenticationRequest { height: 1 };
    assert_eq!(
        runtime.next_action(Instant::now()),
        RuntimeAction::Authenticate(request)
    );
    let block = authenticated_block(&runtime.snapshot.state, 1);
    assert_eq!(
        runtime
            .complete_authentication(&store, identity(), request, Ok(block), Instant::now(),)
            .unwrap(),
        Some(coordinate(1, 0x10))
    );
    assert_eq!(runtime.snapshot.meta.coverage, Coverage::Recovering);

    let request = AuthenticationRequest { height: 2 };
    let block = authenticated_block(&runtime.snapshot.state, 2);
    runtime
        .complete_authentication(&store, identity(), request, Ok(block), Instant::now())
        .unwrap();
    assert_eq!(runtime.snapshot.meta.verified_zone_tip, coordinate(2, 0x10));
    assert_eq!(runtime.snapshot.meta.coverage, Coverage::Complete);
}

#[test]
fn recovered_block_must_extend_the_verified_parent() {
    let (_directory, store, snapshot) = create();
    let mut runtime = runtime(snapshot);
    runtime.observe_tip(&store, coordinate(1, 0x10)).unwrap();
    let mut block = authenticated_block(&runtime.snapshot.state, 1);
    block.parent = coordinate(0, 0x20);

    assert!(matches!(
        runtime.complete_authentication(
            &store,
            identity(),
            AuthenticationRequest { height: 1 },
            Ok(block),
            Instant::now(),
        ),
        Err(PersistenceError::Invalid(_))
    ));
    assert_eq!(runtime.snapshot.meta.verified_zone_tip, coordinate(0, 0x10));
}

#[test]
fn unavailable_data_retries_without_creating_a_gap() {
    let (_directory, store, snapshot) = create();
    let mut runtime = runtime(snapshot);
    runtime.observe_tip(&store, coordinate(2, 0x10)).unwrap();
    let now = Instant::now();

    runtime
        .complete_authentication(
            &store,
            identity(),
            AuthenticationRequest { height: 1 },
            Err(AuthenticationFailure::unlocated(Failure::retry("offline"))),
            now,
        )
        .unwrap();

    assert!(matches!(
        runtime.next_action(now),
        RuntimeAction::RetryAt(_)
    ));
    assert_eq!(runtime.snapshot.meta.verified_zone_tip, coordinate(0, 0x10));
    assert_eq!(runtime.snapshot.meta.coverage, Coverage::Recovering);
}

#[test]
fn authenticated_acquisition_divergence_records_a_finding() {
    let (_directory, store, snapshot) = create();
    let mut runtime = runtime(snapshot);
    runtime.observe_tip(&store, coordinate(2, 0x10)).unwrap();
    let zone = coordinate(1, 0x10);
    let parent = coordinate(0, 0x10);
    let failure = Failure::authenticated_divergence(
        "bad authenticated input",
        Finding::coded(FindingCategory::Observation, 1, FindingLocation::Block),
    );

    runtime
        .complete_authentication(
            &store,
            identity(),
            AuthenticationRequest { height: 1 },
            Err(AuthenticationFailure::at(zone, parent, failure)),
            Instant::now(),
        )
        .unwrap();

    assert!(runtime.snapshot.meta.active_finding.is_some());
    assert!(matches!(
        runtime.snapshot.meta.coverage,
        Coverage::Gap { .. }
    ));
    assert_eq!(runtime.snapshot.meta.verified_zone_tip, parent);
}

#[test]
fn malformed_authenticated_data_blocks_without_advancing() {
    let (_directory, store, snapshot) = create();
    let mut runtime = runtime(snapshot);
    runtime.observe_tip(&store, coordinate(1, 0x10)).unwrap();

    runtime
        .complete_authentication(
            &store,
            identity(),
            AuthenticationRequest { height: 1 },
            Err(AuthenticationFailure::unlocated(Failure::terminal(
                "malformed",
            ))),
            Instant::now(),
        )
        .unwrap();

    assert_eq!(
        runtime.snapshot.meta.blocked,
        Some(CheckerBlockedReason::InvalidAuthenticatedData)
    );
    assert_eq!(runtime.snapshot.meta.verified_zone_tip, coordinate(0, 0x10));
}
