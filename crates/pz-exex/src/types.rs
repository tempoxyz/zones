//! Types for the Privacy Zone ExEx.
//!
//! These mirror the Solidity types from the zone design document.

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::sol;
use serde::{Deserialize, Serialize};

// Generate Solidity bindings for zone portal events and types
sol! {
    /// Exit intent kinds.
    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    enum ExitKind {
        Transfer,
        Swap,
        SwapAndDeposit,
    }

    /// Deposit event emitted by the ZonePortal.
    #[derive(Debug)]
    event DepositEnqueued(
        uint64 indexed zoneId,
        bytes32 indexed newDepositsHash,
        address indexed sender,
        address to,
        uint256 amount,
        bytes32 memo,
        bytes32 l1BlockHash,
        uint64 l1BlockNumber,
        uint64 l1Timestamp
    );

    /// Batch submitted event emitted by the ZonePortal.
    #[derive(Debug)]
    event BatchSubmitted(
        uint64 indexed zoneId,
        uint64 indexed batchIndex,
        bytes32 indexed newStateRoot,
        bytes32 newDepositsHash,
        uint256 exitCount
    );

    /// Transfer exit processed event.
    #[derive(Debug)]
    event TransferExitProcessed(
        uint64 indexed zoneId,
        address indexed recipient,
        uint128 amount
    );

    /// Swap exit processed event.
    #[derive(Debug)]
    event SwapExitProcessed(
        uint64 indexed zoneId,
        address indexed recipient,
        address tokenOut,
        uint128 amountIn,
        uint128 amountOut
    );
}

/// A deposit from L1 to the zone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deposit {
    /// L1 block hash when the deposit was made.
    pub l1_block_hash: B256,
    /// L1 block number when the deposit was made.
    pub l1_block_number: u64,
    /// L1 timestamp when the deposit was made.
    pub l1_timestamp: u64,
    /// Sender address on L1.
    pub sender: Address,
    /// Recipient address on the zone.
    pub to: Address,
    /// Amount of gas token deposited.
    pub amount: U256,
    /// Optional memo.
    pub memo: B256,
}

impl Deposit {
    /// Compute the hash of this deposit for the deposits chain.
    pub fn hash(&self, prev_hash: B256) -> B256 {
        use alloy_primitives::keccak256;
        let mut data = Vec::with_capacity(32 + 8 + 8 + 20 + 20 + 32 + 32);
        data.extend_from_slice(prev_hash.as_slice());
        data.extend_from_slice(self.l1_block_hash.as_slice());
        data.extend_from_slice(&self.l1_block_number.to_be_bytes());
        data.extend_from_slice(&self.l1_timestamp.to_be_bytes());
        data.extend_from_slice(self.sender.as_slice());
        data.extend_from_slice(self.to.as_slice());
        data.extend_from_slice(&self.amount.to_be_bytes::<32>());
        data.extend_from_slice(self.memo.as_slice());
        keccak256(&data)
    }
}

/// Transfer exit intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferExit {
    pub recipient: Address,
    pub amount: u128,
}

/// Swap exit intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapExit {
    pub recipient: Address,
    pub token_out: Address,
    pub amount_in: u128,
    pub min_amount_out: u128,
}

/// Swap and deposit exit intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapAndDepositExit {
    pub token_out: Address,
    pub amount_in: u128,
    pub min_amount_out: u128,
    pub destination_zone_id: u64,
    pub destination_recipient: Address,
}

/// Exit intent from the zone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExitIntent {
    Transfer(TransferExit),
    Swap(SwapExit),
    SwapAndDeposit(SwapAndDepositExit),
}

/// Privacy zone configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PzConfig {
    /// Privacy zone ID.
    pub pz_id: u64,
    /// Zone portal address on L1.
    pub portal_address: Address,
    /// Gas token address (the TIP-20 token bridged to this zone).
    pub gas_token: Address,
    /// Permissioned sequencer address.
    pub sequencer: Address,
    /// Genesis state root.
    pub genesis_state_root: B256,
}

/// Privacy zone state tracked by the ExEx.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PzState {
    /// Current state root.
    pub state_root: B256,
    /// Hash of all deposits processed so far.
    pub processed_deposits_hash: B256,
    /// Hash of all deposits queued on L1.
    pub deposits_hash: B256,
    /// Current batch index.
    pub batch_index: u64,
    /// Last L1 block number processed.
    pub last_l1_block: u64,
}

impl Default for PzState {
    fn default() -> Self {
        Self {
            state_root: B256::ZERO,
            processed_deposits_hash: B256::ZERO,
            deposits_hash: B256::ZERO,
            batch_index: 0,
            last_l1_block: 0,
        }
    }
}

/// Account state in the privacy zone.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PzAccount {
    /// Account balance in the gas token.
    pub balance: U256,
    /// Account nonce.
    pub nonce: u64,
}
