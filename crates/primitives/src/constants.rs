//! Zone protocol constants shared between host and guest.

use alloy_primitives::{Address, B256, U256, address};

/// Sentinel value for empty withdrawal queue slots.
pub const EMPTY_SENTINEL: B256 = B256::new([0xff; 32]);

/// Sentinel emitted as `BatchSubmitted.withdrawalQueueIndex` when a batch carried no
/// withdrawals and therefore consumed no queue index (`NO_QUEUE_INDEX` in Solidity).
pub const NO_QUEUE_INDEX: U256 = U256::MAX;

/// Maximum callback gas a withdrawal may request.
///
/// The L1 processor adds fixed overhead, so this value keeps the outer
/// keeps the outer `processWithdrawals` transaction well below a 30M gas block.
pub const MAX_WITHDRAWAL_GAS_LIMIT: u64 = 10_000_000;

/// Maximum RLP-encoded block size.
///
/// This follows EIP-7934's `MAX_BLOCK_SIZE - SAFETY_MARGIN` and matches
/// `reth_consensus_common::validation::MAX_RLP_BLOCK_SIZE`.
pub const MAX_RLP_BLOCK_SIZE: usize = 8_388_608;

/// TempoState predeploy address on Zone L2.
pub const TEMPO_STATE_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000000");

/// ZoneInbox predeploy address on Zone L2.
pub const ZONE_INBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000001");

/// ZoneOutbox predeploy address on Zone L2.
pub const ZONE_OUTBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000002");

/// Protocol-level contract deployers permitted to create contracts on Zones.
///
/// WARNING: Updating this list is a consensus change.
pub const CONTRACT_DEPLOYER_ALLOWLIST: &[Address] = &[];

/// ZoneTxContext precompile address on Zone L2.
pub const ZONE_TX_CONTEXT_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000005");

/// Chaum-Pedersen verification precompile address.
pub const CHAUM_PEDERSEN_VERIFY_ADDRESS: Address =
    address!("0x1C00000000000000000000000000000000000100");

/// AES-GCM decryption precompile address.
pub const AES_GCM_DECRYPT_ADDRESS: Address = address!("0x1C00000000000000000000000000000000000101");

/// Zone-native fee manager precompile address.
///
/// This is adjacent to, but distinct from, Tempo L1's fee manager at `0xfeec...0000`.
pub const ZONE_FEE_MANAGER_ADDRESS: Address =
    address!("0xfeec000000000000000000000000000000000001");

/// Default zone token address (pathUSD TIP-20).
pub const ZONE_TOKEN_ADDRESS: Address = address!("0x20C0000000000000000000000000000000000000");

/// ZonePortal storage slot 0: `admin` (address).
pub const PORTAL_ADMIN_SLOT: B256 = B256::ZERO;

/// ZonePortal storage slot 3: `currentDepositQueueHash` (bytes32).
pub const PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT: B256 = B256::with_last_byte(3);

/// ZonePortal storage slot 5: `_encryptionKeys` dynamic array.
pub const PORTAL_ENCRYPTION_KEYS_SLOT: B256 = B256::with_last_byte(5);

/// ZonePortal storage slot 6: `_tokenConfigs` mapping.
pub const PORTAL_TOKEN_CONFIGS_SLOT: B256 = B256::with_last_byte(6);

/// ZonePortal storage slot 19: `isSequencer` (mapping(address => bool)).
pub const PORTAL_IS_SEQUENCER_SLOT: B256 = B256::with_last_byte(19);

/// ZonePortal storage slot immediately following Tempo's exported `isSequencer` slot:
/// `role` (mapping(address => Role)).
pub const PORTAL_ROLE_SLOT: B256 = B256::with_last_byte(20);

/// ZonePortal slot following `role`: packed account and gateway enforcement booleans.
pub const PORTAL_ENFORCEMENT_MODES_SLOT: B256 = B256::with_last_byte(21);

/// ZonePortal storage slot 22: `maxTempoGasRate` (uint128).
pub const PORTAL_MAX_TEMPO_GAS_RATE_SLOT: B256 = B256::with_last_byte(22);

/// Alias used by consumers reading account allowlist enforcement.
pub const PORTAL_ACCESS_MODE_SLOT: B256 = PORTAL_ENFORCEMENT_MODES_SLOT;

/// Alias used by consumers reading callback gateway enforcement.
pub const PORTAL_GATEWAY_MODE_SLOT: B256 = PORTAL_ENFORCEMENT_MODES_SLOT;

// ---------------------------------------------------------------------------
//  Storage slot constants for the proof system
// ---------------------------------------------------------------------------

/// ZoneInbox storage slot 0: `processedDepositQueueHash` (bytes32).
pub const ZONE_INBOX_PROCESSED_HASH_SLOT: U256 = U256::ZERO;

/// ZoneInbox storage slot 1: `processedDepositNumber` (uint64, lower 8 bytes).
pub const ZONE_INBOX_PROCESSED_NUMBER_SLOT: U256 = {
    let mut le = [0u8; 32];
    le[0] = 1;
    U256::from_le_bytes(le)
};

/// ZoneOutbox storage slot 1: `_withdrawalQueueHash` (bytes32).
///
/// Slot 0 is packed `(tempoGasRate, nextWithdrawalIndex)`.
pub const ZONE_OUTBOX_LAST_BATCH_HASH_SLOT: U256 = {
    let mut le = [0u8; 32];
    le[0] = 1;
    U256::from_le_bytes(le)
};

/// ZoneOutbox storage slot 2: `_withdrawalBatchIndex` (uint64, lower 8 bytes).
pub const ZONE_OUTBOX_LAST_BATCH_INDEX_SLOT: U256 = {
    let mut le = [0u8; 32];
    le[0] = 2;
    U256::from_le_bytes(le)
};

/// Tempo mainnet parent chain ID.
pub const TEMPO_MAINNET_CHAIN_ID: u64 = 4_217;

/// Tempo Moderato parent chain ID.
pub const TEMPO_MODERATO_CHAIN_ID: u64 = 42_431;

/// Base offset for deriving **mainnet** zone chain IDs.
///
/// # Range safety
///
/// EIP-2294 and ENSIP-11 reserve bit 31 (`0x8000_0000`) for coin-type flags,
/// making chain IDs ≥ 2^31 (2,147,483,648) unsafe in parts of the ecosystem
/// (ENS multi-chain address resolution, some JavaScript tooling that uses
/// 32-bit integers, etc.).
///
/// The ranges are chosen so that both mainnet and testnet zones stay well below
/// that limit while remaining non-overlapping:
///
/// | Network  | Base            | Range size        | Chain ID span                         |
/// |----------|-----------------|-------------------|---------------------------------------|
/// | Mainnet  | `421_700_000`   | `1_002_610_000`   | `421_700_000 ..  1_424_310_000`       |
/// | Testnet  | `1_424_310_000` | `723_173_648`     | `1_424_310_000 .. 2_147_483_648`      |
///
pub const ZONE_CHAIN_ID_BASE: u64 = 421_700_000;

/// Number of distinct mainnet zone chain IDs.
///
/// Equal to `ZONE_CHAIN_ID_BASE_TESTNET - ZONE_CHAIN_ID_BASE`, keeping the
/// mainnet range strictly below the testnet range.
pub const ZONE_CHAIN_ID_RANGE: u64 = 1_002_610_000;

/// Base offset for deriving **testnet** (Moderato) zone chain IDs.
///
/// See [`ZONE_CHAIN_ID_BASE`] for range-safety rationale.
pub const ZONE_CHAIN_ID_BASE_TESTNET: u64 = 1_424_310_000;

/// Number of distinct Moderato zone chain IDs.
///
/// Equal to `2^31 - ZONE_CHAIN_ID_BASE_TESTNET`, keeping the testnet range
/// strictly below the EIP-2294 safe ceiling.
pub const ZONE_CHAIN_ID_RANGE_TESTNET: u64 = 723_173_648;

/// Failure to derive a unique zone chain ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ZoneChainIdError {
    /// Production zone IDs must fit in the reserved range.
    #[error("zone ID {zone_id} exhausts the reserved range for parent chain {parent_chain_id}")]
    ZoneIdOutOfRange { parent_chain_id: u64, zone_id: u32 },
    /// Generic parent chain IDs must be nonzero and fit in 32 bits.
    #[error("generic parent chain ID must be in 1..=u32::MAX, got {0}")]
    InvalidParentChainId(u64),
}

/// Derives a zone EIP-155 chain ID from its parent Tempo chain and ZoneFactory ID.
///
/// The production branches remain below 2^31 for ecosystem compatibility. Other
/// parents use the high 32 bits, making the mapping injective for accepted inputs.
pub const fn zone_chain_id(parent_chain_id: u64, zone_id: u32) -> Result<u64, ZoneChainIdError> {
    if parent_chain_id == 0 || parent_chain_id > u32::MAX as u64 {
        return Err(ZoneChainIdError::InvalidParentChainId(parent_chain_id));
    }
    match parent_chain_id {
        TEMPO_MAINNET_CHAIN_ID => {
            if (zone_id as u64) < ZONE_CHAIN_ID_RANGE {
                Ok(ZONE_CHAIN_ID_BASE + zone_id as u64)
            } else {
                Err(ZoneChainIdError::ZoneIdOutOfRange {
                    parent_chain_id,
                    zone_id,
                })
            }
        }
        TEMPO_MODERATO_CHAIN_ID => {
            if (zone_id as u64) < ZONE_CHAIN_ID_RANGE_TESTNET {
                Ok(ZONE_CHAIN_ID_BASE_TESTNET + zone_id as u64)
            } else {
                Err(ZoneChainIdError::ZoneIdOutOfRange {
                    parent_chain_id,
                    zone_id,
                })
            }
        }
        _ => Ok((parent_chain_id << 32) | zone_id as u64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_domain_separated_chain_ids() {
        assert_eq!(zone_chain_id(TEMPO_MAINNET_CHAIN_ID, 7), Ok(421_700_007));
        assert_eq!(zone_chain_id(TEMPO_MODERATO_CHAIN_ID, 7), Ok(1_424_310_007));
        assert_ne!(zone_chain_id(1_337, 7), zone_chain_id(1_338, 7));

        let production = [
            zone_chain_id(TEMPO_MAINNET_CHAIN_ID, 42).unwrap(),
            zone_chain_id(TEMPO_MODERATO_CHAIN_ID, 42).unwrap(),
        ];
        let generic = zone_chain_id(1, 42).unwrap();
        assert!(production.into_iter().all(|id| id < 1 << 31));
        assert!(generic >= 1 << 32);
        assert!(!production.contains(&generic));
    }

    #[test]
    fn rejects_exhausted_ranges_and_invalid_generic_parents() {
        assert!(matches!(
            zone_chain_id(TEMPO_MAINNET_CHAIN_ID, ZONE_CHAIN_ID_RANGE as u32),
            Err(ZoneChainIdError::ZoneIdOutOfRange { .. })
        ));
        assert!(matches!(
            zone_chain_id(TEMPO_MODERATO_CHAIN_ID, ZONE_CHAIN_ID_RANGE_TESTNET as u32),
            Err(ZoneChainIdError::ZoneIdOutOfRange { .. })
        ));
        assert_eq!(
            zone_chain_id(0, 1),
            Err(ZoneChainIdError::InvalidParentChainId(0))
        );
        assert!(matches!(
            zone_chain_id(u32::MAX as u64 + 1, 1),
            Err(ZoneChainIdError::InvalidParentChainId(_))
        ));
    }
}
