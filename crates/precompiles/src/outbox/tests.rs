use super::*;

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Bytes, address};
use alloy_sol_types::{SolCall, SolInterface};
use revm::precompile::PrecompileResult;
use tempo_precompiles::{Precompile as _, tip20::ISSUER_ROLE};
use tempo_zone_contracts::portal_token_config_slot;

use crate::{
    L1StorageReader, execution,
    test_utils::{
        MockL1Reader, TestContext, call_precompile, test_context, test_l1_env,
        test_storage_provider,
    },
    tx_context,
};

const ANCHOR: u64 = 7;
const GAS: u64 = 10_000_000;
const TX_HASH: B256 = B256::repeat_byte(0x42);
const ALICE: Address = address!("0x00000000000000000000000000000000000000a1");
const BOB: Address = address!("0x00000000000000000000000000000000000000b2");
const SEQUENCER: Address = address!("0x00000000000000000000000000000000000000c3");

struct Harness {
    ctx: TestContext,
    l1: MockL1Reader,
    precompile: DynPrecompile,
    token: Address,
}

impl Harness {
    fn new() -> eyre::Result<Self> {
        let mut ctx = test_context();
        let l1 = MockL1Reader::allow_all();
        let portal = l1.portal_address();
        let token = tempo_precompiles::PATH_USD_ADDRESS;

        l1.set_u256(
            portal,
            U256::from_be_bytes(PORTAL_SEQUENCER_SLOT.0),
            ANCHOR,
            U256::from_be_slice(SEQUENCER.as_slice()),
        );
        l1.set_u256(
            portal,
            U256::from_be_bytes(portal_token_config_slot(token).0),
            ANCHOR,
            U256::ONE,
        );
        l1.seed_transfer_policy_id(token, ANCHOR);

        {
            let mut storage = test_storage_provider(&mut ctx, u64::MAX, false);
            StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
                StorageCtx::default().sstore(
                    zone_primitives::constants::TEMPO_STATE_ADDRESS,
                    crate::tempo_state::slots::TEMPO_BLOCK_NUMBER,
                    U256::from(ANCHOR),
                )?;

                ZoneOutbox::new().initialize()?;
                let mut token_contract =
                    TIP20Token::from_address(token).expect("PATH_USD is a valid TIP20 address");
                token_contract.initialize(
                    ALICE,
                    "Zone USD",
                    "zUSD",
                    "USD",
                    Address::ZERO,
                    ALICE,
                )?;
                token_contract.grant_role_internal(ALICE, *ISSUER_ROLE)?;
                token_contract.grant_role_internal(ZONE_OUTBOX_ADDRESS, *ISSUER_ROLE)?;
                token_contract.mint(
                    ALICE,
                    ITIP20::mintCall {
                        to: ALICE,
                        amount: U256::from(1_000_000u64),
                    },
                )?;
                token_contract.approve(
                    ALICE,
                    ITIP20::approveCall {
                        spender: ZONE_OUTBOX_ADDRESS,
                        amount: U256::MAX,
                    },
                )?;
                Ok(())
            })?;
        }

        let env = test_l1_env(&ctx, l1.clone());
        let precompile = execution::create_l1_backed_precompile(
            "ZoneOutboxTest",
            env,
            ZoneOutboxRules::new(portal),
            |data, caller| ZoneOutbox::new().call(data, caller),
        );

        Ok(Self {
            ctx,
            l1,
            precompile,
            token,
        })
    }

    fn call(&mut self, caller: Address, data: impl AsRef<[u8]>) -> PrecompileResult {
        let _guard = tx_context::set_current_tx_hash(TX_HASH);
        call_precompile(
            &mut self.ctx,
            &self.precompile,
            caller,
            data.as_ref(),
            GAS,
            false,
            ZONE_OUTBOX_ADDRESS,
            ZONE_OUTBOX_ADDRESS,
        )
    }

    fn call_without_hash(&mut self, caller: Address, data: impl AsRef<[u8]>) -> PrecompileResult {
        call_precompile(
            &mut self.ctx,
            &self.precompile,
            caller,
            data.as_ref(),
            GAS,
            false,
            ZONE_OUTBOX_ADDRESS,
            ZONE_OUTBOX_ADDRESS,
        )
    }

    fn pending(&mut self) -> eyre::Result<Vec<ZoneOutboxAbi::PendingWithdrawal>> {
        let output = self.call(
            Address::ZERO,
            ZoneOutboxAbi::getPendingWithdrawalsCall {}.abi_encode(),
        )?;
        Ok(ZoneOutboxAbi::getPendingWithdrawalsCall::abi_decode_returns(&output.bytes)?)
    }

    fn request(&mut self, amount: u128, to: Address, memo: B256) -> PrecompileResult {
        let token = self.token;
        self.call(
            ALICE,
            ZoneOutboxAbi::requestWithdrawalCall {
                token,
                to,
                amount,
                memo,
                gasLimit: 0,
                fallbackRecipient: ALICE,
                data: Bytes::new(),
                revealTo: Bytes::new(),
            }
            .abi_encode(),
        )
    }

    fn finalize(&mut self, count: usize) -> PrecompileResult {
        self.call(
            SEQUENCER,
            ZoneOutboxAbi::finalizeWithdrawalBatchCall {
                count: U256::from(count),
                blockNumber: 0,
                encryptedSenders: vec![Bytes::new(); count],
            }
            .abi_encode(),
        )
    }
}

fn assert_revert(result: PrecompileResult, error: ZoneOutboxError) {
    let output = result.expect("precompile error");
    assert!(output.is_revert());
    assert_eq!(output.bytes, error.abi_encode());
}

#[test]
fn request_withdrawal_stores_fields_and_fifo_order() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    harness.request(500, ALICE, B256::repeat_byte(1))?;
    harness.request(300, BOB, B256::repeat_byte(2))?;

    let pending = harness.pending()?;
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].sender, ALICE);
    assert_eq!(pending[0].txHash, TX_HASH);
    assert_eq!(pending[0].to, ALICE);
    assert_eq!(pending[0].amount, 500);
    assert_eq!(pending[1].to, BOB);
    assert_eq!(pending[1].amount, 300);
    Ok(())
}

#[test]
fn request_withdrawal_rejects_disabled_token() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let portal = harness.l1.portal_address();
    harness.l1.set_u256(
        portal,
        U256::from_be_bytes(portal_token_config_slot(harness.token).0),
        ANCHOR,
        U256::ZERO,
    );
    let result = harness.request(1, BOB, B256::ZERO);
    assert_revert(result, ZoneOutboxError::token_not_enabled());
    Ok(())
}

#[test]
fn request_withdrawal_rejects_missing_transaction_hash() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let token = harness.token;
    let result = harness.call_without_hash(
        ALICE,
        ZoneOutboxAbi::requestWithdrawalCall {
            token,
            to: BOB,
            amount: 1,
            memo: B256::ZERO,
            gasLimit: 0,
            fallbackRecipient: ALICE,
            data: Bytes::new(),
            revealTo: Bytes::new(),
        }
        .abi_encode(),
    );
    assert_revert(result, ZoneOutboxError::invalid_current_tx_hash());
    Ok(())
}

#[test]
fn enqueue_bounce_back_is_inbox_only() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let call = ZoneOutboxAbi::enqueueDepositBounceBackCall {
        token: harness.token,
        amount: 100,
        bouncebackRecipient: BOB,
    }
    .abi_encode();

    assert_revert(
        harness.call(ALICE, &call),
        ZoneOutboxError::only_zone_inbox(),
    );
    harness.call(ZONE_INBOX_ADDRESS, call)?;
    let pending = harness.pending()?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].sender, Address::ZERO);
    assert_eq!(pending[0].fee, 0);
    Ok(())
}

#[test]
fn finalize_empty_queue_returns_zero() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let output = harness.finalize(0)?;
    assert_eq!(
        ZoneOutboxAbi::finalizeWithdrawalBatchCall::abi_decode_returns(&output.bytes)?,
        B256::ZERO
    );
    Ok(())
}

#[test]
fn finalize_single_and_multiple_withdrawals_match_canonical_queue_hash() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    harness.request(100, ALICE, B256::repeat_byte(1))?;
    harness.request(200, BOB, B256::repeat_byte(2))?;
    let pending = harness.pending()?;
    let expected: Vec<Withdrawal> = pending
        .iter()
        .map(|pending| Withdrawal {
            token: pending.token,
            senderTag: Withdrawal::sender_tag(pending.sender, pending.txHash),
            to: pending.to,
            amount: pending.amount,
            fee: pending.fee,
            memo: pending.memo,
            gasLimit: pending.gasLimit,
            fallbackRecipient: pending.fallbackRecipient,
            callbackData: pending.callbackData.clone(),
            encryptedSender: Bytes::new(),
        })
        .collect();

    let output = harness.finalize(2)?;
    assert_eq!(
        ZoneOutboxAbi::finalizeWithdrawalBatchCall::abi_decode_returns(&output.bytes)?,
        Withdrawal::queue_hash(&expected)
    );
    assert!(harness.pending()?.is_empty());
    Ok(())
}

#[test]
fn finalize_rejects_wrong_count_and_non_sequencer() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    harness.request(100, ALICE, B256::ZERO)?;
    assert_revert(
        harness.finalize(0),
        ZoneOutboxError::invalid_withdrawal_count(U256::ZERO, U256::ONE),
    );

    let result = harness.call(
        ALICE,
        ZoneOutboxAbi::finalizeWithdrawalBatchCall {
            count: U256::ONE,
            blockNumber: 0,
            encryptedSenders: vec![Bytes::new()],
        }
        .abi_encode(),
    );
    assert_revert(result, ZoneOutboxError::only_sequencer());
    Ok(())
}
