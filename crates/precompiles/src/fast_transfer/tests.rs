use super::*;

use alloy_primitives::{Bytes, address};
use alloy_sol_types::{SolCall, SolError};
use tempo_precompiles::storage::StorageCtx;
use tempo_zone_contracts::IFastTransfer;

use crate::test_utils::{call_precompile, test_context, test_env, test_storage_provider};

const SENDER: Address = address!("0x00000000000000000000000000000000000000a1");
const RECIPIENT: Address = address!("0x00000000000000000000000000000000000000b2");
const OPERATOR: Address = address!("0x00000000000000000000000000000000000000c3");
const PORTAL: Address = address!("0x00000000000000000000000000000000000000d4");
const TOKEN: Address = address!("0x20c0000000000000000000000000000000000000");

fn intent(nonce: u64) -> Intent {
    let mut value = Intent {
        transferId: B256::ZERO,
        sourceZone: B256::repeat_byte(0x11),
        destinationZone: B256::repeat_byte(0x22),
        sourceChainId: 1,
        destinationChainId: 2,
        sourcePortal: PORTAL,
        destinationPortal: Address::repeat_byte(0x44),
        sourceEpoch: 7,
        destinationEpoch: 8,
        protocolVersion: 1,
        sourceToken: TOKEN,
        destinationToken: TOKEN,
        sender: SENDER,
        recipient: RECIPIENT,
        refundAccount: SENDER,
        pool: OPERATOR,
        reimbursementAccount: OPERATOR,
        principal: 90,
        fee: 10,
        quoteId: B256::repeat_byte(0x55),
        destinationExpiryHeight: 100,
        transferNonce: nonce,
    };
    value.transferId = FastTransfer::expected_transfer_id(&value);
    value
}

#[test]
fn transfer_identity_and_intent_hash_bind_every_body() {
    let original = intent(9);
    let mut changed = original.clone();
    changed.recipient = Address::repeat_byte(0xee);

    assert_eq!(
        FastTransfer::expected_transfer_id(&original),
        FastTransfer::expected_transfer_id(&changed),
        "the dedicated nonce defines retry identity"
    );
    assert_ne!(
        FastTransfer::intent_hash(&original),
        FastTransfer::intent_hash(&changed),
        "the separately stored body hash detects ID reuse"
    );
}

#[test]
fn remembered_outcome_is_immutable_and_idempotent() -> eyre::Result<()> {
    let mut ctx = test_context();
    let intent = intent(10);
    let hash = FastTransfer::intent_hash(&intent);
    let mut storage = test_storage_provider(&mut ctx, u64::MAX, false);
    StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
        let mut fast = FastTransfer::new();
        fast.initialize()?;
        fast.write_intent(&intent, hash, 100)?;
        fast.states[intent.transferId].write(STATE_LOCKED)?;

        assert_eq!(
            fast.record_outcome_verified(intent.transferId, hash, OUTCOME_PAID)?,
            STATE_PAID_AWAITING_RELEASE
        );
        assert_eq!(
            fast.record_outcome_verified(intent.transferId, hash, OUTCOME_PAID)?,
            STATE_PAID_AWAITING_RELEASE
        );
        assert!(
            fast.record_outcome_verified(intent.transferId, hash, OUTCOME_REJECTED)
                .is_err(),
            "a retry cannot turn a payment into a refund"
        );
        Ok(())
    })
}

#[test]
fn exposure_retirement_is_body_bound_and_exactly_once() -> eyre::Result<()> {
    let mut ctx = test_context();
    let intent = intent(11);
    let hash = FastTransfer::intent_hash(&intent);
    let mut storage = test_storage_provider(&mut ctx, u64::MAX, false);
    StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
        let mut fast = FastTransfer::new();
        fast.initialize()?;
        fast.write_intent(&intent, hash, intent.principal)?;
        fast.tokens[intent.transferId].write(TOKEN)?;
        fast.states[intent.transferId].write(STATE_PAID)?;
        fast.unsettled_exposure[TOKEN][intent.sourceZone].write(intent.principal)?;

        assert!(
            fast.retire_exposure_verified(
                intent.transferId,
                hash,
                TOKEN,
                Address::repeat_byte(0xff),
                intent.principal,
            )
            .is_err()
        );
        fast.retire_exposure_verified(intent.transferId, hash, TOKEN, OPERATOR, intent.principal)?;
        fast.retire_exposure_verified(intent.transferId, hash, TOKEN, OPERATOR, intent.principal)?;
        assert_eq!(fast.unsettled_exposure[TOKEN][intent.sourceZone].read()?, 0);
        assert!(fast.exposure_retired[intent.transferId].read()?);
        Ok(())
    })
}

#[test]
fn public_state_transition_is_dormant_without_consensus_fork() -> eyre::Result<()> {
    let mut ctx = test_context();
    {
        let mut storage = test_storage_provider(&mut ctx, u64::MAX, false);
        StorageCtx::enter(&mut storage, || FastTransfer::new().initialize())?;
    }
    let env = test_env(&ctx);
    let precompile = FastTransfer::create(
        L1State::new(crate::test_utils::MockL1Reader::default(), PORTAL),
        &env,
    );
    let value = intent(12);
    let data = IFastTransfer::lockCall {
        intentHash: FastTransfer::intent_hash(&value),
        intent: value,
    }
    .abi_encode();
    let output = call_precompile(
        &mut ctx,
        &precompile,
        SENDER,
        &data,
        10_000_000,
        false,
        FAST_TRANSFER_ADDRESS,
        FAST_TRANSFER_ADDRESS,
    )?;
    assert!(output.is_revert());
    assert_eq!(
        output.bytes,
        Bytes::from(IFastTransfer::FastTransferNotActive {}.abi_encode())
    );
    Ok(())
}
