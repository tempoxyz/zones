//! Programmatic conformance tests for Solidity and native Rust storage layouts.

use super::artifact;
use tempo_precompiles::{
    test_util::storage_conformance::{RustStorageSlot, assert_foundry_slots},
    zone_factory::portal,
};

#[test]
fn zone_portal_slot_constants_match_solidity() {
    let fields = [
        ("admin", portal::slots::ADMIN),
        (
            "currentDepositQueueHash",
            portal::slots::CURRENT_DEPOSIT_QUEUE_HASH,
        ),
        ("_encryptionKeys", portal::slots::ENCRYPTION_KEYS),
        ("_tokenConfigs", portal::slots::TOKEN_CONFIGS),
        ("role", portal::slots::ROLE),
        ("_isAccessEnforced", portal::slots::IS_ACCESS_ENFORCED),
        ("_isGatewayEnforced", portal::slots::IS_GATEWAY_ENFORCED),
        ("maxTempoGasRate", portal::slots::MAX_TEMPO_GAS_RATE),
        ("pauseExpiry", portal::slots::PAUSE_EXPIRY),
        ("tokenEnablementHash", portal::slots::TOKEN_ENABLEMENT_HASH),
        (
            "abdicationEffectiveAt",
            portal::slots::ABDICATION_EFFECTIVE_AT,
        ),
    ]
    .map(|(name, slot)| RustStorageSlot::new(name, slot));
    assert_foundry_slots(&artifact("ZonePortal"), &fields);
}
