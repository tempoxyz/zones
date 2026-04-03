//! Zone protocol constants shared between host and guest.

use alloy_primitives::{Address, B256, U256, address};

/// Sentinel value for empty withdrawal queue slots.
pub const EMPTY_SENTINEL: B256 = B256::new([0xff; 32]);

/// TempoState predeploy address on Zone L2.
pub const TEMPO_STATE_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000000");

/// TempoState storage slot for `tempoBlockHash` (slot 0).
pub const TEMPO_BLOCK_HASH_SLOT: B256 = B256::ZERO;

/// TempoState storage slot for packed
/// `(tempoBlockNumber, tempoGasLimit, tempoGasUsed, tempoTimestamp)` (slot 7).
pub const TEMPO_PACKED_SLOT: B256 = {
    let mut bytes = [0u8; 32];
    bytes[31] = 7;
    B256::new(bytes)
};

/// ZoneInbox predeploy address on Zone L2.
pub const ZONE_INBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000001");

/// ZoneOutbox predeploy address on Zone L2.
pub const ZONE_OUTBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000002");

/// ZoneConfig predeploy address on Zone L2.
pub const ZONE_CONFIG_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000003");

/// TempoStateReader precompile address on Zone L2.
pub const TEMPO_STATE_READER_ADDRESS: Address =
    address!("0x1c00000000000000000000000000000000000004");

/// ZoneTxContext precompile address on Zone L2.
pub const ZONE_TX_CONTEXT_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000005");

/// Chaum-Pedersen verification precompile address.
pub const CHAUM_PEDERSEN_VERIFY_ADDRESS: Address =
    address!("0x1C00000000000000000000000000000000000100");

/// AES-GCM decryption precompile address.
pub const AES_GCM_DECRYPT_ADDRESS: Address = address!("0x1C00000000000000000000000000000000000101");

/// TIP-20 zone token factory precompile address.
pub const ZONE_TIP20_FACTORY_ADDRESS: Address =
    address!("0x20Fc000000000000000000000000000000000000");

/// Default zone token address (pathUSD TIP-20).
pub const ZONE_TOKEN_ADDRESS: Address = address!("0x20C0000000000000000000000000000000000000");

/// ZonePortal storage slot 0: `sequencer` (address).
pub const PORTAL_SEQUENCER_SLOT: B256 = B256::ZERO;

/// ZonePortal storage slot 1: `pendingSequencer` (address).
pub const PORTAL_PENDING_SEQUENCER_SLOT: B256 = {
    let mut bytes = [0u8; 32];
    bytes[31] = 1;
    B256::new(bytes)
};

// ---------------------------------------------------------------------------
//  Storage slot constants for the proof system
// ---------------------------------------------------------------------------

/// ZoneInbox storage slot 0: `processedDepositQueueHash` (bytes32).
pub const ZONE_INBOX_PROCESSED_HASH_SLOT: U256 = U256::ZERO;

/// ZoneOutbox storage slot 1: `_lastBatch.withdrawalQueueHash` (bytes32).
///
/// Slot 0 is packed `(tempoGasRate, nextWithdrawalIndex, withdrawalBatchIndex)`.
/// The `_lastBatch` struct starts at slot 1 with `withdrawalQueueHash` occupying the full slot.
pub const ZONE_OUTBOX_LAST_BATCH_HASH_SLOT: U256 = {
    let mut le = [0u8; 32];
    le[0] = 1;
    U256::from_le_bytes(le)
};

/// ZoneOutbox storage slot 2: `_lastBatch.withdrawalBatchIndex` (uint64, lower 8 bytes).
pub const ZONE_OUTBOX_LAST_BATCH_INDEX_SLOT: U256 = {
    let mut le = [0u8; 32];
    le[0] = 2;
    U256::from_le_bytes(le)
};

/// Base offset for deriving **mainnet** zone chain IDs: `421_700_000 + zone_id`.
///
/// Each zone gets a unique EIP-155 chain ID derived from its on-chain zone ID
/// assigned by the `ZoneFactory` contract.
///
/// # Range safety
///
/// EIP-2294 and ENSIP-11 reserve bit 31 (`0x8000_0000`) for coin-type flags,
/// making chain IDs ≥ 2^31 (2,147,483,647) unsafe in parts of the ecosystem
/// (ENS multi-chain address resolution, some JavaScript tooling that uses
/// 32-bit integers, etc.).
///
/// The ranges are chosen so that both mainnet and testnet zones stay well below
/// that limit while remaining non-overlapping:
///
/// | Network  | Base            | Example zone 1     | Ceiling (2^31 − 1)    |
/// |----------|-----------------|--------------------|-----------------------|
/// | Mainnet  | `421_700_000`   | `421_700_001`      | 2,147,483,647         |
/// | Testnet  | `1_424_310_000` | `1_424_310_001`    | 2,147,483,647         |
///
/// This gives ~1 billion mainnet zone IDs (up to zone ~1,002,600,000) before
/// the testnet range is reached, and ~723 million testnet zone IDs before
/// exceeding `2^31 − 1`.
///
/// WARNING: If the `ZoneFactory` ever allows zone IDs large enough to push
/// `base + zone_id` past `2^31 − 1`, a cap should be enforced both in the
/// Solidity contract and here.
pub const ZONE_CHAIN_ID_BASE: u64 = 421_700_000;

/// Base offset for deriving **testnet** (Moderato) zone chain IDs:
/// `1_424_310_000 + zone_id`.
///
/// See [`ZONE_CHAIN_ID_BASE`] for range-safety rationale.
pub const ZONE_CHAIN_ID_BASE_TESTNET: u64 = 1_424_310_000;

/// Derives the EIP-155 chain ID for a **mainnet** zone from its on-chain zone ID.
pub const fn zone_chain_id(zone_id: u32) -> u64 {
    ZONE_CHAIN_ID_BASE + zone_id as u64
}

/// Derives the EIP-155 chain ID for a **testnet** zone from its on-chain zone ID.
pub const fn zone_chain_id_testnet(zone_id: u32) -> u64 {
    ZONE_CHAIN_ID_BASE_TESTNET + zone_id as u64
}
