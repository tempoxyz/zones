//! Portal storage-layout conformance tests.

use alloy_primitives::{B256, U256};
use zone_primitives::constants::{
    PORTAL_ADMIN_SLOT, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, PORTAL_ENCRYPTION_KEYS_SLOT,
    PORTAL_MAX_TEMPO_GAS_RATE_SLOT, PORTAL_ROLE_SLOT, PORTAL_TOKEN_ENABLEMENT_HASH_SLOT,
};

/// Pins the Rust portal storage-slot constants to the ZonePortal storage layout.
#[test]
fn zone_portal_storage_slot_constants_match_solidity() {
    assert_eq!(PORTAL_ADMIN_SLOT, B256::ZERO, "admin is slot 0");
    assert_eq!(
        PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
        B256::from(U256::from(3)),
        "currentDepositQueueHash is slot 3"
    );
    assert_eq!(
        PORTAL_ENCRYPTION_KEYS_SLOT,
        B256::from(U256::from(5)),
        "_encryptionKeys is slot 5"
    );
    assert_eq!(
        PORTAL_ROLE_SLOT,
        B256::from(U256::from(20)),
        "role is slot 20"
    );
    assert_eq!(
        PORTAL_MAX_TEMPO_GAS_RATE_SLOT,
        B256::from(U256::from(22)),
        "maxTempoGasRate is slot 22"
    );
    assert_eq!(
        PORTAL_TOKEN_ENABLEMENT_HASH_SLOT,
        B256::from(U256::from(26)),
        "tokenEnablementHash is slot 26"
    );
}
