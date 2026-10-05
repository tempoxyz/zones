use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, B256};
use reth_chainspec::EthChainSpec as _;
use tempo_chainspec::{hardfork::TempoHardfork, spec::DEV};
use zone_chainspec::{ZoneChainSpec, test_utils::set_tempo_fork};
use zone_evm::same_anchor::{
    SAME_ANCHOR_MAX_AGE_MILLIS, SameAnchorError, SameAnchorOpening, is_same_anchor_opening,
};
use zone_node::fast_quorum::{
    ActivationError, FAST_PROTOCOL_VERSION, FastActivation, FastAdmissionClock, FinalizedFastEpoch,
    FinalizedT14Capability, FreshnessError, MAX_ANCHOR_STALENESS, t14_fast_protocol_native_pin,
};
use zone_primitives::constants::zone_chain_id;

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .try_into()
        .unwrap()
}

#[test]
fn same_anchor_opening_is_canonical_and_timestamp_bounded() {
    let opening = SameAnchorOpening::v1(17, B256::repeat_byte(0x42), 1_000, 250, 9);
    let encoded = opening.encode_calldata().unwrap();
    assert!(is_same_anchor_opening(&encoded));
    assert_eq!(SameAnchorOpening::decode_calldata(&encoded), Ok(opening));

    for length in [0, 3, 4, encoded.len() - 1] {
        assert_eq!(
            SameAnchorOpening::decode_calldata(&encoded[..length]),
            Err(SameAnchorError::InvalidCalldata)
        );
    }
    let mut trailing = encoded.to_vec();
    trailing.push(0);
    assert_eq!(
        SameAnchorOpening::decode_calldata(&trailing),
        Err(SameAnchorError::InvalidCalldata)
    );

    let anchor_millis = 1_000_250;
    assert_eq!(
        opening.validate_block_timestamp(anchor_millis, 1_000, 251),
        Ok(())
    );
    assert_eq!(
        opening.validate_block_timestamp(anchor_millis, 1_002, 250),
        Ok(())
    );
    assert_eq!(
        opening.validate_block_timestamp(anchor_millis, 1_002, 251),
        Err(SameAnchorError::AnchorExpired {
            maximum: anchor_millis + SAME_ANCHOR_MAX_AGE_MILLIS,
            actual: anchor_millis + SAME_ANCHOR_MAX_AGE_MILLIS + 1,
        })
    );
    assert_eq!(
        opening.validate_block_timestamp(anchor_millis, 1_000, 250),
        Err(SameAnchorError::TimestampNotIncreasing {
            parent: anchor_millis,
            actual: anchor_millis,
        })
    );
}

#[test]
fn same_anchor_activation_is_exactly_t14_and_uses_the_finalized_anchor_time() {
    let mut genesis = DEV.genesis().clone();
    genesis.config.chain_id = zone_chain_id(DEV.chain_id(), 14).unwrap();
    set_tempo_fork(&mut genesis, TempoHardfork::T14, 10_000);
    let zone = ZoneChainSpec::from_genesis(genesis).unwrap();

    assert!(!zone.supports_same_anchor_at(9_999));
    assert!(zone.supports_same_anchor_at(10_000));
}

#[test]
fn repeated_finalized_header_does_not_refresh_live_admission() {
    let clock = FastAdmissionClock::default();
    let hash = B256::repeat_byte(1);
    clock.observe_finalized(10, hash).unwrap();
    clock.admit_live(now_millis()).unwrap();

    std::thread::sleep(MAX_ANCHOR_STALENESS + Duration::from_millis(100));
    clock.observe_finalized(10, hash).unwrap();
    assert!(matches!(
        clock.admit_live(now_millis()),
        Err(FreshnessError::StaleAnchor(_))
    ));

    clock.observe_finalized(11, B256::repeat_byte(2)).unwrap();
    clock.admit_live(now_millis()).unwrap();
}

#[test]
fn finalized_anchor_regression_conflict_and_live_clock_skew_fail_closed() {
    let clock = FastAdmissionClock::default();
    assert_eq!(
        clock.admit_live(now_millis()),
        Err(FreshnessError::NoFinalizedAnchor)
    );
    clock.observe_finalized(20, B256::repeat_byte(3)).unwrap();
    assert_eq!(
        clock.observe_finalized(19, B256::repeat_byte(4)),
        Err(FreshnessError::Regressed {
            current: 20,
            observed: 19,
        })
    );
    assert_eq!(
        clock.observe_finalized(20, B256::repeat_byte(4)),
        Err(FreshnessError::ConflictingFinalizedHeader { number: 20 })
    );
    assert!(matches!(
        clock.admit_live(now_millis() - 1_000),
        Err(FreshnessError::ClockSkew(_))
    ));
}

#[test]
fn fast_activation_cannot_be_enabled_by_local_configuration_alone() {
    let epoch = FinalizedFastEpoch {
        l1_chain_id: 1,
        portal: Address::repeat_byte(1),
        zone_id: 1,
        zone_chain_id: 101,
        epoch: 1,
        protocol_version: FAST_PROTOCOL_VERSION,
        threshold: 2,
        proof_mode: 1,
        expected_verifier_code_hash: B256::repeat_byte(21),
        expected_verifier_config_hash: B256::repeat_byte(22),
        members: [
            Address::repeat_byte(2),
            Address::repeat_byte(3),
            Address::repeat_byte(4),
        ],
        peer_portals: std::array::from_fn(|index| Address::with_last_byte(20 + index as u8)),
        roster_hash: B256::ZERO,
        finalized_l1_block: 1,
    };
    assert_eq!(
        FastActivation::from_finalized_epoch(FinalizedT14Capability {
            epoch,
            native_pin: B256::ZERO,
            t14_active_at_anchor: true,
            current_epoch: 1,
            activated_at_l1_block: 1,
            closed: false,
            retired: false,
        })
        .unwrap_err(),
        ActivationError::NativePin {
            expected: t14_fast_protocol_native_pin(),
            actual: B256::ZERO,
        }
    );
}
