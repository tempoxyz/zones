//! Zone batch state transition function for proving.
//!
//! Implements `prove_zone_batch`, a pure function that takes a [`BatchWitness`]
//! containing all data needed to re-execute a batch of zone blocks, and produces
//! a [`BatchOutput`] with the commitments the on-chain verifier checks.
//!
//! This crate is `no_std` compatible so it can run inside SP1 (RISC-V) and
//! TEE (SGX/TDX) environments.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod db;

use alloc::format;
use alloc::vec::Vec;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_sol_types::SolCall;
use revm::{
    Context,
    handler::{ExecuteEvm, MainBuilder, MainContext, SystemCallCommitEvm},
};
use zone_primitives::{
    BatchOutput, BatchWitness, BlockTransition, DecryptionData, DepositQueueTransition,
    DepositType, Error, LastBatchCommitment, QueuedDeposit, ZoneBlock, ZoneHeader,
    constants::{
        ZONE_INBOX_ADDRESS, ZONE_INBOX_PROCESSED_HASH_SLOT, ZONE_OUTBOX_ADDRESS,
        ZONE_OUTBOX_LAST_BATCH_HASH_SLOT, ZONE_OUTBOX_LAST_BATCH_INDEX_SLOT,
    },
};

pub use db::{WitnessDb, WitnessDbError};

// ---------------------------------------------------------------------------
//  ABI types for system call calldata construction
// ---------------------------------------------------------------------------

alloy_sol_types::sol! {
    enum SolDepositType {
        Regular,
        Encrypted,
    }

    struct SolQueuedDeposit {
        SolDepositType depositType;
        bytes depositData;
    }

    struct SolChaumPedersenProof {
        bytes32 s;
        bytes32 c;
    }

    struct SolDecryptionData {
        bytes32 sharedSecret;
        uint8 sharedSecretYParity;
        address to;
        bytes32 memo;
        SolChaumPedersenProof cpProof;
    }

    struct SolEnabledToken {
        address token;
        string name;
        string symbol;
        string currency;
    }

    function advanceTempo(
        bytes calldata header,
        SolQueuedDeposit[] calldata deposits,
        SolDecryptionData[] calldata decryptions,
        SolEnabledToken[] calldata enabledTokens
    ) external;

    function finalizeWithdrawalBatch(
        uint256 count,
        uint64 blockNumber
    ) external returns (bytes32 withdrawalQueueHash);
}

// ---------------------------------------------------------------------------
//  Core STF
// ---------------------------------------------------------------------------

/// Pure state transition function for zone batch proving.
///
/// Given a complete [`BatchWitness`], re-executes all zone blocks using revm
/// and produces a [`BatchOutput`] containing the commitments that the
/// on-chain verifier will check.
pub fn prove_zone_batch(witness: BatchWitness) -> Result<BatchOutput, Error> {
    let public_inputs = &witness.public_inputs;

    // ------------------------------------------------------------------
    // Verify previous block header
    // ------------------------------------------------------------------

    let prev_header_hash = witness.prev_block_header.hash();
    if prev_header_hash != public_inputs.prev_block_hash {
        return Err(Error::InconsistentState);
    }

    if witness.initial_zone_state.state_root != witness.prev_block_header.state_root {
        return Err(Error::InconsistentState);
    }

    // ------------------------------------------------------------------
    // Initialize zone state
    // ------------------------------------------------------------------

    let db = WitnessDb::from_witness(&witness.initial_zone_state);

    // Capture the deposit queue hash at the start of the batch.
    let deposit_prev = B256::from(
        db.read_storage(ZONE_INBOX_ADDRESS, ZONE_INBOX_PROCESSED_HASH_SLOT)
            .to_be_bytes::<32>(),
    );

    // Create the EVM. We reuse a single instance across all blocks — only the
    // block environment changes between blocks.
    let ctx = Context::mainnet()
        .modify_cfg_chained(|cfg| {
            cfg.set_spec_and_mainnet_gas_params(revm::primitives::hardfork::SpecId::PRAGUE);
        })
        .with_db(db);
    let mut evm = ctx.build_mainnet();

    // ------------------------------------------------------------------
    // Execute zone blocks
    // ------------------------------------------------------------------

    let mut prev_block_hash = public_inputs.prev_block_hash;
    let mut prev_number = witness.prev_block_header.number;
    let num_blocks = witness.zone_blocks.len();

    for (idx, block) in witness.zone_blocks.iter().enumerate() {
        let is_last = idx + 1 == num_blocks;

        // --- Block property validation ---

        if block.parent_hash != prev_block_hash {
            return Err(Error::InconsistentState);
        }
        if block.number != prev_number + 1 {
            return Err(Error::InconsistentState);
        }
        if block.beneficiary != public_inputs.sequencer {
            return Err(Error::InconsistentState);
        }
        if is_last {
            if block.finalize_withdrawal_batch_count.is_none() {
                return Err(Error::InconsistentState);
            }
        } else if block.finalize_withdrawal_batch_count.is_some() {
            return Err(Error::InconsistentState);
        }

        // --- Set block environment ---

        evm.set_block(block_env(block));

        // --- System tx: advanceTempo ---

        if let Some(ref tempo_header_rlp) = block.tempo_header_rlp {
            let calldata =
                advance_tempo_calldata(tempo_header_rlp, &block.deposits, &block.decryptions);
            evm.system_call_with_caller_commit(
                Address::ZERO,
                ZONE_INBOX_ADDRESS,
                calldata.into(),
            )
            .map_err(|e| Error::ExecutionError(format!("{e:?}")))?;
        } else if !block.deposits.is_empty() || !block.decryptions.is_empty() {
            return Err(Error::InconsistentState);
        }

        // --- User transactions ---

        // TODO: Decode serialized transactions from `block.transactions`,
        // build TxEnv for each, and execute via `transact_commit`.

        // --- System tx: finalizeWithdrawalBatch (final block only) ---

        if let Some(count) = block.finalize_withdrawal_batch_count {
            let calldata = finalize_withdrawal_batch_calldata(count, block.number);
            evm.system_call_with_caller_commit(
                Address::ZERO,
                ZONE_OUTBOX_ADDRESS,
                calldata.into(),
            )
            .map_err(|e| Error::ExecutionError(format!("{e:?}")))?;
        }

        // --- Compute zone block hash ---

        // TODO: The state root should be recomputed from committed changes
        // via MPT update. For now we use the initial state root which is only
        // correct for the first block before any state changes.
        let state_root = evm.ctx.journaled_state.database.state_root();

        // TODO: Compute proper transactions_root and receipts_root from the
        // ordered trie of all transactions / receipts in this block.
        let header = ZoneHeader {
            parent_hash: prev_block_hash,
            beneficiary: block.beneficiary,
            state_root,
            transactions_root: B256::ZERO,
            receipts_root: B256::ZERO,
            number: block.number,
            timestamp: block.timestamp,
        };
        prev_block_hash = header.hash();
        prev_number = block.number;
    }

    // ------------------------------------------------------------------
    // Extract output commitments
    // ------------------------------------------------------------------

    let db = &evm.ctx.journaled_state.database;

    // Read processedDepositQueueHash after all blocks executed.
    let deposit_next = B256::from(
        db.read_storage(ZONE_INBOX_ADDRESS, ZONE_INBOX_PROCESSED_HASH_SLOT)
            .to_be_bytes::<32>(),
    );

    // Read _lastBatch from ZoneOutbox state.
    let withdrawal_queue_hash = B256::from(
        db.read_storage(ZONE_OUTBOX_ADDRESS, ZONE_OUTBOX_LAST_BATCH_HASH_SLOT)
            .to_be_bytes::<32>(),
    );
    let withdrawal_batch_index: u64 = db
        .read_storage(ZONE_OUTBOX_ADDRESS, ZONE_OUTBOX_LAST_BATCH_INDEX_SLOT)
        .to::<u64>();

    // TODO: Verify Tempo state binding
    // - Read tempoBlockNumber from TempoState storage
    // - Verify it matches public_inputs.tempo_block_number
    // - Verify anchor block hash (direct or ancestry mode)

    Ok(BatchOutput {
        block_transition: BlockTransition {
            prev_block_hash: public_inputs.prev_block_hash,
            next_block_hash: prev_block_hash,
        },
        deposit_queue_transition: DepositQueueTransition {
            prev_processed_hash: deposit_prev,
            next_processed_hash: deposit_next,
        },
        withdrawal_queue_hash,
        last_batch: LastBatchCommitment {
            withdrawal_batch_index,
        },
    })
}

// ---------------------------------------------------------------------------
//  Helpers
// ---------------------------------------------------------------------------

/// Build a [`revm::context::BlockEnv`] from a [`ZoneBlock`].
fn block_env(block: &ZoneBlock) -> revm::context::BlockEnv {
    revm::context::BlockEnv {
        number: U256::from(block.number),
        beneficiary: block.beneficiary,
        timestamp: U256::from(block.timestamp),
        gas_limit: 30_000_000,
        basefee: 0,
        difficulty: U256::ZERO,
        prevrandao: Some(B256::ZERO),
        blob_excess_gas_and_price: None,
    }
}

/// ABI-encode the `advanceTempo(header, deposits, decryptions, enabledTokens)` calldata.
fn advance_tempo_calldata(
    tempo_header_rlp: &[u8],
    deposits: &[QueuedDeposit],
    decryptions: &[DecryptionData],
) -> Vec<u8> {
    let sol_deposits: Vec<SolQueuedDeposit> = deposits
        .iter()
        .map(|d| SolQueuedDeposit {
            depositType: match d.deposit_type {
                DepositType::Regular => SolDepositType::Regular,
                DepositType::Encrypted => SolDepositType::Encrypted,
            },
            depositData: Bytes::copy_from_slice(&d.deposit_data),
        })
        .collect();

    let sol_decryptions: Vec<SolDecryptionData> = decryptions
        .iter()
        .map(|d| SolDecryptionData {
            sharedSecret: d.shared_secret,
            sharedSecretYParity: d.shared_secret_y_parity,
            to: d.to,
            memo: d.memo,
            cpProof: SolChaumPedersenProof {
                s: d.cp_proof.s,
                c: d.cp_proof.c,
            },
        })
        .collect();

    advanceTempoCall {
        header: Bytes::copy_from_slice(tempo_header_rlp),
        deposits: sol_deposits,
        decryptions: sol_decryptions,
        enabledTokens: Vec::new(),
    }
    .abi_encode()
}

/// ABI-encode the `finalizeWithdrawalBatch(count, blockNumber)` calldata.
fn finalize_withdrawal_batch_calldata(count: U256, block_number: u64) -> Vec<u8> {
    finalizeWithdrawalBatchCall {
        count,
        blockNumber: block_number,
    }
    .abi_encode()
}
