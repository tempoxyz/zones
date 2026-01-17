//! Types for the Privacy Zone ExEx.

use alloy_primitives::{address, keccak256, Address, B256, U256};
use alloy_sol_types::sol;
use serde::{Deserialize, Serialize};

/// Exit precompile address - users call this to initiate withdrawals.
/// This is a sentinel address that the block builder intercepts.
pub const EXIT_PRECOMPILE: Address = address!("0000000000000000000000000000000000000420");

sol! {
    /// Deposit event emitted by the ZonePortal.
    #[derive(Debug)]
    event DepositEnqueued(
        uint64 indexed zoneId,
        bytes32 indexed newDepositsHash,
        address indexed sender,
        address to,
        uint256 amount,
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

    /// Exit precompile interface for zone transactions.
    /// Users call this precompile to initiate an exit.
    #[derive(Debug)]
    function exit(address recipient, uint256 amount);
}

/// Cursor for tracking position in L1 event stream.
/// Used for proper reorg handling - we need to know exactly where we left off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct L1Cursor {
    /// L1 block number.
    pub block_number: u64,
    /// Log index within the block.
    pub log_index: u64,
}

impl L1Cursor {
    /// Create a new cursor.
    pub const fn new(block_number: u64, log_index: u64) -> Self {
        Self { block_number, log_index }
    }

    /// Check if this cursor is after another.
    pub fn is_after(&self, other: &Self) -> bool {
        (self.block_number, self.log_index) > (other.block_number, other.log_index)
    }
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
}

impl Deposit {
    /// Compute the hash of this deposit for the deposits chain.
    /// Hash structure: keccak256(abi.encode(deposit, prevHash)) - newest outermost.
    pub fn hash(&self, prev_hash: B256) -> B256 {
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(self.l1_block_hash.as_slice());
        buf.extend_from_slice(&self.l1_block_number.to_be_bytes());
        buf.extend_from_slice(&self.l1_timestamp.to_be_bytes());
        buf.extend_from_slice(self.sender.as_slice());
        buf.extend_from_slice(self.to.as_slice());
        buf.extend_from_slice(&self.amount.to_be_bytes::<32>());
        buf.extend_from_slice(prev_hash.as_slice());
        keccak256(&buf)
    }
}

/// Privacy zone configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PzConfig {
    /// Privacy zone ID.
    pub zone_id: u64,
    /// Zone portal address on L1.
    pub portal_address: Address,
    /// Gas token address (the TIP-20 token bridged to this zone).
    pub gas_token: Address,
    /// Permissioned sequencer address.
    pub sequencer: Address,
    /// Genesis state root.
    pub genesis_state_root: B256,
    /// Optional data directory for persistent state.
    /// If None, uses in-memory state only.
    #[serde(default)]
    pub data_dir: Option<std::path::PathBuf>,
}

/// Privacy zone state tracked by the ExEx.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PzState {
    /// Current state root.
    pub state_root: B256,
    /// Hash of all deposits processed so far (chain hash).
    pub processed_deposits_hash: B256,
    /// Hash of all deposits queued on L1 (chain hash).
    pub deposits_hash: B256,
    /// Hash of all exits so far (chain hash).
    pub exits_hash: B256,
    /// Current batch index.
    pub batch_index: u64,
    /// Current zone block height.
    pub zone_block: u64,
    /// Cursor tracking last processed L1 event.
    pub cursor: L1Cursor,
    /// Journal hash for provenance tracking (like Signet).
    /// Each block's journal hash = keccak256(prev_journal_hash || block_data).
    pub journal_hash: B256,
}

impl PzState {
    /// Compute the next journal hash given new block data.
    pub fn next_journal_hash(&self, block_data: &[u8]) -> B256 {
        let mut data = Vec::with_capacity(32 + block_data.len());
        data.extend_from_slice(self.journal_hash.as_slice());
        data.extend_from_slice(block_data);
        keccak256(&data)
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

/// Exit intent from the zone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitIntent {
    /// Sender address on the zone (who initiated the exit).
    pub sender: Address,
    /// Recipient address on L1.
    pub recipient: Address,
    /// Amount to exit.
    pub amount: U256,
    /// Zone block where this exit was included.
    pub zone_block: u64,
    /// Index within the zone block.
    pub exit_index: u64,
}

impl ExitIntent {
    /// Compute the hash of this exit for the exits chain.
    pub fn hash(&self, prev_hash: B256) -> B256 {
        let mut data = Vec::with_capacity(32 + 20 + 20 + 32 + 8 + 8);
        data.extend_from_slice(prev_hash.as_slice());
        data.extend_from_slice(self.sender.as_slice());
        data.extend_from_slice(self.recipient.as_slice());
        data.extend_from_slice(&self.amount.to_be_bytes::<32>());
        data.extend_from_slice(&self.zone_block.to_be_bytes());
        data.extend_from_slice(&self.exit_index.to_be_bytes());
        keccak256(&data)
    }
}

/// A decoded portal event with its position in the L1 chain.
#[derive(Debug, Clone)]
pub struct PortalEvent {
    /// Position of this event in the L1 chain.
    pub cursor: L1Cursor,
    /// The event payload.
    pub kind: PortalEventKind,
}

/// Types of portal events we care about.
#[derive(Debug, Clone)]
pub enum PortalEventKind {
    /// A deposit was enqueued.
    Deposit(Deposit),
    /// A batch was submitted.
    BatchSubmitted {
        batch_index: u64,
        new_state_root: B256,
        new_deposits_hash: B256,
        exit_count: U256,
    },
}

/// Extraction results for a single L1 block.
/// Accumulates all events from one L1 block before processing.
#[derive(Debug, Clone, Default)]
pub struct L1BlockExtracts {
    /// L1 block number.
    pub l1_block_number: u64,
    /// Deposits in this block.
    pub deposits: Vec<Deposit>,
    /// Batch submissions in this block.
    pub batches: Vec<PortalEventKind>,
}

impl L1BlockExtracts {
    /// Create a new extraction result for an L1 block.
    pub fn new(l1_block_number: u64) -> Self {
        Self {
            l1_block_number,
            deposits: Vec::new(),
            batches: Vec::new(),
        }
    }

    /// Check if there are any events to process.
    pub fn is_empty(&self) -> bool {
        self.deposits.is_empty() && self.batches.is_empty()
    }
}

/// A pending transaction in the zone mempool.
///
/// This is the unified queue entry for both deposits and user transactions.
/// Deposits are forced inclusions (priority), user txs come from RPC.
#[derive(Debug, Clone)]
pub enum PendingTx {
    /// A deposit from L1 - forced inclusion, executes deposit logic.
    Deposit {
        /// L1 cursor for ordering and reorg handling.
        cursor: L1Cursor,
        /// Hash of this deposit in the deposits chain.
        deposit_hash: B256,
        /// The deposit data.
        deposit: Deposit,
    },
    /// A user transaction from RPC (to be added later).
    /// For now this is a placeholder.
    UserTx {
        /// Transaction hash.
        tx_hash: B256,
        /// The signed transaction envelope.
        tx: Bytes,
    },
}

impl PendingTx {
    /// Create a new pending deposit.
    pub fn deposit(cursor: L1Cursor, deposit_hash: B256, deposit: Deposit) -> Self {
        Self::Deposit {
            cursor,
            deposit_hash,
            deposit,
        }
    }

    /// Get the L1 cursor if this is a deposit.
    pub fn cursor(&self) -> Option<L1Cursor> {
        match self {
            Self::Deposit { cursor, .. } => Some(*cursor),
            Self::UserTx { .. } => None,
        }
    }

    /// Check if this is a deposit.
    pub fn is_deposit(&self) -> bool {
        matches!(self, Self::Deposit { .. })
    }
}
