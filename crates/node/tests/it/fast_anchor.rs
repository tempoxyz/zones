//! Same-anchor acceptance boundaries derived from the instant-transfer specification.

use alloy_primitives::B256;
use zone_evm::same_anchor::{SameAnchorOpening, is_same_anchor_opening};

#[test]
fn same_anchor_encoding_is_exact_and_rejects_truncation_trailing_and_wrong_format() {
    let opening = SameAnchorOpening::v1(17, B256::from([0x42; 32]), 1_000, 250, 9);
    let encoded = opening
        .encode_calldata()
        .expect("valid same-anchor opening");
    assert!(is_same_anchor_opening(&encoded));
    assert_eq!(
        SameAnchorOpening::decode_calldata(&encoded).unwrap(),
        opening
    );

    for length in 0..encoded.len() {
        assert!(SameAnchorOpening::decode_calldata(&encoded[..length]).is_err());
    }
    let mut trailing = encoded;
    trailing.push(0);
    assert!(SameAnchorOpening::decode_calldata(&trailing).is_err());

    let mut wrong_format = opening;
    wrong_format.format = wrong_format.format.saturating_add(1);
    assert!(wrong_format.validate().is_err());
}

#[test]
fn same_anchor_timestamp_is_monotonic_and_bounded_by_anchor_plus_two_seconds() {
    let opening = SameAnchorOpening::v1(17, B256::from([0x42; 32]), 1_000, 250, 9);
    let anchor_millis = 1_000_250_u64;

    assert!(
        opening
            .validate_block_timestamp(anchor_millis, 1_000, 251)
            .is_ok(),
        "first millisecond after the parent must be admissible"
    );
    assert!(
        opening
            .validate_block_timestamp(anchor_millis, 1_002, 250)
            .is_ok(),
        "exactly anchor plus two seconds must be admissible"
    );
    assert!(
        opening
            .validate_block_timestamp(anchor_millis, 1_000, 250)
            .is_err(),
        "same millisecond as parent was admitted"
    );
    assert!(
        opening
            .validate_block_timestamp(anchor_millis, 1_002, 251)
            .is_err(),
        "timestamp beyond anchor plus two seconds was admitted"
    );
    assert!(
        opening
            .validate_block_timestamp(anchor_millis, 1_001, 1_000)
            .is_err(),
        "invalid millisecond component was admitted"
    );
}
