use alloy_consensus::{Header, Sealable as _, SignableTransaction as _, Signed, TxLegacy};
use alloy_primitives::{Address, B256, Bloom, Bytes, Log, LogData, Signature, U256, b256};
use alloy_rlp::Encodable as _;
use alloy_sol_types::SolEvent as _;
use reth_primitives_traits::{RecoveredBlock, SealedBlock};
use tempo_primitives::{
    Block, BlockBody, TempoHeader, TempoReceipt, TempoTxEnvelope, TempoTxType,
    transaction::envelope::TEMPO_SYSTEM_TX_SIGNATURE,
};
use tempo_zone_contracts::{
    IZoneInbox, IZoneOutbox, TEMPO_STATE_ADDRESS, TempoState, ZONE_INBOX_ADDRESS,
    ZONE_OUTBOX_ADDRESS,
};

use super::*;
use crate::observe::{
    error::{
        AcquisitionError, AcquisitionSource, AuthenticatedDataEvidence, AuthenticatedTransaction,
        DataSource, EnvelopeRule, ObservationError, ProtocolChain,
    },
    events::{Inbox, L2ProtocolEvent, Outbox},
};

const ZONE_NUMBER: u64 = 9;
const ZONE_PARENT_HASH: B256 = B256::repeat_byte(0x19);

fn imported_header() -> TempoHeader {
    TempoHeader {
        inner: Header {
            number: 100,
            state_root: B256::repeat_byte(0x31),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn encode_header(header: &TempoHeader) -> Bytes {
    let mut encoded = Vec::new();
    header.encode(&mut encoded);
    encoded.into()
}

fn advance_transaction(to: Address) -> TempoTxEnvelope {
    advance_transaction_with_tokens(to, Vec::new())
}

fn advance_transaction_with_tokens(
    to: Address,
    enabled_tokens: Vec<IZoneInbox::EnabledToken>,
) -> TempoTxEnvelope {
    let calldata = IZoneInbox::advanceTempoCall {
        header: encode_header(&imported_header()),
        deposits: Vec::new(),
        decryptions: Vec::new(),
        enabledTokens: enabled_tokens,
    }
    .abi_encode();
    system_transaction(to, calldata.into())
}

fn token_enabled_log(symbol: &str) -> Log {
    Log {
        address: ZONE_INBOX_ADDRESS,
        data: IZoneInbox::TokenEnabled {
            token: Address::repeat_byte(0x71),
            name: "Token".into(),
            symbol: symbol.into(),
            currency: "USD".into(),
        }
        .encode_log_data(),
    }
}

fn finalization_transaction(block_number: u64) -> TempoTxEnvelope {
    let calldata = IZoneOutbox::finalizeWithdrawalBatchCall {
        count: U256::ZERO,
        blockNumber: block_number,
        encryptedSenders: Vec::new(),
    }
    .abi_encode();
    system_transaction(ZONE_OUTBOX_ADDRESS, calldata.into())
}

fn system_transaction(to: Address, input: Bytes) -> TempoTxEnvelope {
    TempoTxEnvelope::Legacy(Signed::new_unhashed(
        TxLegacy {
            chain_id: None,
            nonce: 0,
            gas_price: 0,
            gas_limit: 0,
            to: to.into(),
            value: U256::ZERO,
            input,
        },
        TEMPO_SYSTEM_TX_SIGNATURE,
    ))
}

fn user_transaction(input_tag: u8) -> TempoTxEnvelope {
    TempoTxEnvelope::Legacy(
        TxLegacy {
            to: Address::repeat_byte(input_tag).into(),
            input: Bytes::from(vec![input_tag]),
            ..Default::default()
        }
        .into_signed(Signature::new(U256::from(1), U256::from(2), false)),
    )
}

fn receipt(success: bool, logs: Vec<Log>) -> TempoReceipt<Log> {
    TempoReceipt {
        tx_type: TempoTxType::Legacy,
        success,
        cumulative_gas_used: 0,
        logs,
    }
}

fn advance_logs(hash_override: Option<B256>) -> Vec<Log> {
    let header = imported_header();
    let hash = hash_override.unwrap_or_else(|| header.hash_slow());
    vec![
        Log {
            address: TEMPO_STATE_ADDRESS,
            data: TempoState::TempoBlockFinalized {
                blockHash: header.hash_slow(),
                blockNumber: header.inner.number,
                stateRoot: header.inner.state_root,
            }
            .encode_log_data(),
        },
        Log {
            address: ZONE_INBOX_ADDRESS,
            data: IZoneInbox::TempoAdvanced {
                tempoBlockHash: hash,
                tempoBlockNumber: header.inner.number,
                depositsProcessed: U256::ZERO,
                newProcessedDepositQueueHash: B256::repeat_byte(0x41),
                lastProcessedDepositNumber: 12,
            }
            .encode_log_data(),
        },
    ]
}

fn recovered_block(
    transactions: Vec<TempoTxEnvelope>,
    senders: Vec<Address>,
    receipts: &[TempoReceipt],
) -> RecoveredBlock<Block> {
    let (receipts_root, logs_bloom) = receipt_commitments(receipts);
    let block = Block {
        header: TempoHeader {
            inner: Header {
                number: ZONE_NUMBER,
                parent_hash: ZONE_PARENT_HASH,
                receipts_root,
                logs_bloom,
                ..Default::default()
            },
            ..Default::default()
        },
        body: BlockBody {
            transactions,
            ..Default::default()
        },
    };
    RecoveredBlock::new_sealed(SealedBlock::seal_slow(block), senders)
}

fn reseal_with_receipts(
    block: RecoveredBlock<Block>,
    receipts: &[TempoReceipt],
) -> RecoveredBlock<Block> {
    let (receipts_root, logs_bloom) = receipt_commitments(receipts);
    reseal_with_commitments(block, receipts_root, logs_bloom)
}

fn reseal_with_commitments(
    block: RecoveredBlock<Block>,
    receipts_root: B256,
    logs_bloom: Bloom,
) -> RecoveredBlock<Block> {
    let senders = block.senders().to_vec();
    let mut block = block.into_block();
    block.header.inner.receipts_root = receipts_root;
    block.header.inner.logs_bloom = logs_bloom;
    RecoveredBlock::new_sealed(SealedBlock::seal_slow(block), senders)
}

fn receipt_commitments(receipts: &[TempoReceipt]) -> (B256, Bloom) {
    let receipts_root = TempoReceipt::calculate_receipt_root_no_memo(receipts);
    let logs_bloom = receipts
        .iter()
        .fold(Bloom::ZERO, |bloom, receipt| bloom | receipt.bloom());
    (receipts_root, logs_bloom)
}

fn basic_fixture() -> (RecoveredBlock<Block>, Vec<TempoReceipt<Log>>) {
    let receipts = vec![receipt(true, advance_logs(None))];
    let block = recovered_block(
        vec![advance_transaction(ZONE_INBOX_ADDRESS)],
        vec![Address::ZERO],
        &receipts,
    );
    (block, receipts)
}

fn tempo_gas_rate_updated_log() -> Log {
    Log {
        address: ZONE_OUTBOX_ADDRESS,
        data: IZoneOutbox::TempoGasRateUpdated { tempoGasRate: 7 }.encode_log_data(),
    }
}

fn withdrawal_requested_log() -> Log {
    Log {
        address: ZONE_OUTBOX_ADDRESS,
        data: IZoneOutbox::WithdrawalRequested {
            withdrawalIndex: 4,
            sender: Address::repeat_byte(0x45),
            token: Address::repeat_byte(0x55),
            to: Address::repeat_byte(0x66),
            amount: 100,
            fee: 9,
            memo: B256::ZERO,
            gasLimit: 0,
            fallbackNonce: 3,
            data: Bytes::new(),
            revealTo: Bytes::new(),
        }
        .encode_log_data(),
    }
}

fn observe_user_logs(logs: Vec<Log>) -> (B256, L2BlockObservation) {
    let advance = advance_transaction(ZONE_INBOX_ADDRESS);
    let user = user_transaction(0x77);
    let user_hash = *user.tx_hash();
    let receipts = vec![receipt(true, advance_logs(None)), receipt(true, logs)];
    let block = recovered_block(
        vec![advance, user],
        vec![Address::ZERO, Address::repeat_byte(0x44)],
        &receipts,
    );
    let observation = observe_l2_block(&block, &receipts).unwrap();
    (user_hash, observation)
}

mod authentication;
mod events;
mod finalization;
mod observation;
