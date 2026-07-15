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

    #[rustfmt::skip]
    fn call_inner(&mut self, caller: Address, data: impl AsRef<[u8]>, with_hash: bool, is_static: bool) -> PrecompileResult {
        let mut call = || { call_precompile(
            &mut self.ctx, &self.precompile, caller, data.as_ref(), GAS, is_static, ZONE_OUTBOX_ADDRESS, ZONE_OUTBOX_ADDRESS
        )};

        if with_hash {
            let _guard = tx_context::set_current_tx_hash(TX_HASH);
            call()
        } else {
            call()
        }
    }

    fn call_without_hash(&mut self, caller: Address, data: impl AsRef<[u8]>) -> PrecompileResult {
        self.call_inner(caller, data, false, false)
    }

    fn call(&mut self, caller: Address, data: impl AsRef<[u8]>) -> PrecompileResult {
        self.call_inner(caller, data, true, false)
    }

    fn call_static(&mut self, caller: Address, data: impl AsRef<[u8]>) -> PrecompileResult {
        self.call_inner(caller, data, false, true)
    }

    fn pending(&mut self) -> eyre::Result<Vec<ZoneOutboxAbi::PendingWithdrawal>> {
        let output = self.call(
            Address::ZERO,
            ZoneOutboxAbi::getPendingWithdrawalsCall {}.abi_encode(),
        )?;
        Ok(ZoneOutboxAbi::getPendingWithdrawalsCall::abi_decode_returns(&output.bytes)?)
    }

    fn request(&mut self, amount: u128, to: Address, memo: B256) -> PrecompileResult {
        self.request_custom(ZoneOutboxAbi::requestWithdrawalCall {
            token: self.token,
            to,
            amount,
            memo,
            gasLimit: 0,
            fallbackRecipient: ALICE,
            data: Bytes::new(),
            revealTo: Bytes::new(),
        })
    }

    fn request_custom(&mut self, call: ZoneOutboxAbi::requestWithdrawalCall) -> PrecompileResult {
        self.call(ALICE, call.abi_encode())
    }

    fn set_gas_rate(&mut self, rate: u128) -> PrecompileResult {
        self.call(
            SEQUENCER,
            ZoneOutboxAbi::setTempoGasRateCall {
                _tempoGasRate: rate,
            }
            .abi_encode(),
        )
    }

    fn set_max_withdrawals(&mut self, max: u32) -> PrecompileResult {
        self.call(
            SEQUENCER,
            ZoneOutboxAbi::setMaxWithdrawalsPerBlockCall {
                _maxWithdrawalsPerBlock: max,
            }
            .abi_encode(),
        )
    }

    fn balance_of(&mut self, account: Address) -> eyre::Result<U256> {
        let mut storage = test_storage_provider(&mut self.ctx, u64::MAX, false);
        StorageCtx::enter(&mut storage, || {
            let token = TIP20Token::from_address(self.token).expect("initialized token");
            Ok(token.balance_of(ITIP20::balanceOfCall { account })?)
        })
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

#[test]
fn fee_rate_and_gas_limit_validation_match_reference() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    harness.set_gas_rate(3)?;

    let output = harness.call(
        ALICE,
        ZoneOutboxAbi::calculateWithdrawalFeeCall { gasLimit: 7 }.abi_encode(),
    )?;
    assert_eq!(
        ZoneOutboxAbi::calculateWithdrawalFeeCall::abi_decode_returns(&output.bytes)?,
        u128::from(WITHDRAWAL_BASE_GAS + 7) * 3
    );

    assert_revert(
        harness.call(
            ALICE,
            ZoneOutboxAbi::calculateWithdrawalFeeCall {
                gasLimit: MAX_WITHDRAWAL_GAS_LIMIT + 1,
            }
            .abi_encode(),
        ),
        ZoneOutboxError::gas_limit_too_high(),
    );
    assert_revert(
        harness.set_gas_rate(MAX_GAS_FEE_RATE + 1),
        ZoneOutboxError::gas_fee_rate_too_high(),
    );
    Ok(())
}

#[test]
fn callback_and_reveal_boundaries_are_enforced() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let token = harness.token;
    let base = |data: Bytes, reveal_to: Bytes| ZoneOutboxAbi::requestWithdrawalCall {
        token,
        to: BOB,
        amount: 1,
        memo: B256::ZERO,
        gasLimit: 0,
        fallbackRecipient: ALICE,
        data,
        revealTo: reveal_to,
    };

    harness.request_custom(base(
        Bytes::from(vec![0; MAX_CALLBACK_DATA_SIZE]),
        Bytes::new(),
    ))?;
    assert_revert(
        harness.request_custom(base(
            Bytes::from(vec![0; MAX_CALLBACK_DATA_SIZE + 1]),
            Bytes::new(),
        )),
        ZoneOutboxError::callback_data_too_large(),
    );
    assert_revert(
        harness.request_custom(base(Bytes::new(), Bytes::from(vec![2; 32]))),
        ZoneOutboxError::invalid_reveal_to(),
    );
    assert_revert(
        harness.request_custom(base(Bytes::new(), Bytes::from(vec![4; 33]))),
        ZoneOutboxError::invalid_reveal_to(),
    );

    let valid = Bytes::copy_from_slice(&alloy_primitives::hex!(
        "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
    ));
    harness.request_custom(base(Bytes::new(), valid))?;
    Ok(())
}

#[test]
fn fallback_recipient_and_zero_amount_semantics_match_reference() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let token = harness.token;
    assert_revert(
        harness.request_custom(ZoneOutboxAbi::requestWithdrawalCall {
            token,
            to: BOB,
            amount: 1,
            memo: B256::ZERO,
            gasLimit: 0,
            fallbackRecipient: Address::ZERO,
            data: Bytes::new(),
            revealTo: Bytes::new(),
        }),
        ZoneOutboxError::invalid_fallback_recipient(),
    );
    harness.request(0, BOB, B256::ZERO)?;
    assert_eq!(harness.pending()?[0].amount, 0);
    Ok(())
}

#[test]
fn request_burns_amount_plus_fee_and_rejects_insufficient_funds() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    harness.set_gas_rate(2)?;
    let before = harness.balance_of(ALICE)?;
    let fee = u128::from(WITHDRAWAL_BASE_GAS) * 2;
    harness.request(100, BOB, B256::ZERO)?;
    assert_eq!(harness.balance_of(ALICE)?, before - U256::from(100 + fee));

    let result = harness.request(u128::MAX, BOB, B256::ZERO);
    assert!(result.expect("precompile result").is_revert());
    Ok(())
}

#[test]
fn encrypted_sender_count_and_length_are_validated() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    harness.request(1, BOB, B256::ZERO)?;
    assert_revert(
        harness.call(
            SEQUENCER,
            ZoneOutboxAbi::finalizeWithdrawalBatchCall {
                count: U256::ONE,
                blockNumber: 0,
                encryptedSenders: Vec::new(),
            }
            .abi_encode(),
        ),
        ZoneOutboxError::invalid_encrypted_sender_count(U256::ZERO, U256::ONE),
    );
    assert_revert(
        harness.call(
            SEQUENCER,
            ZoneOutboxAbi::finalizeWithdrawalBatchCall {
                count: U256::ONE,
                blockNumber: 0,
                encryptedSenders: vec![Bytes::from(vec![1])],
            }
            .abi_encode(),
        ),
        ZoneOutboxError::invalid_encrypted_sender_length(U256::ONE, U256::ZERO),
    );
    Ok(())
}

#[test]
fn indices_last_batch_and_timestamp_advance_across_batches() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    harness.ctx.block.timestamp = U256::from(123);
    harness.request(1, BOB, B256::ZERO)?;
    harness.finalize(1)?;
    harness.request(2, BOB, B256::ZERO)?;
    let second_hash = ZoneOutboxAbi::finalizeWithdrawalBatchCall::abi_decode_returns(
        &harness.finalize(1)?.bytes,
    )?;

    let next = harness.call(
        Address::ZERO,
        ZoneOutboxAbi::nextWithdrawalIndexCall {}.abi_encode(),
    )?;
    assert_eq!(
        ZoneOutboxAbi::nextWithdrawalIndexCall::abi_decode_returns(&next.bytes)?,
        2
    );
    let batch = harness.call(Address::ZERO, ZoneOutboxAbi::lastBatchCall {}.abi_encode())?;
    let batch = ZoneOutboxAbi::lastBatchCall::abi_decode_returns(&batch.bytes)?;
    assert_eq!(batch.withdrawalBatchIndex, 2);
    assert_eq!(batch.withdrawalQueueHash, second_hash);
    let timestamp = harness.call(
        Address::ZERO,
        ZoneOutboxAbi::lastFinalizedTimestampCall {}.abi_encode(),
    )?;
    assert_eq!(
        ZoneOutboxAbi::lastFinalizedTimestampCall::abi_decode_returns(&timestamp.bytes)?,
        123
    );
    Ok(())
}

#[test]
fn per_block_cap_is_unlimited_resettable_and_updateable() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    harness.set_max_withdrawals(0)?;
    for _ in 0..3 {
        harness.request(1, BOB, B256::ZERO)?;
    }

    harness.set_max_withdrawals(1)?;
    harness.request(1, BOB, B256::ZERO)?;
    assert_revert(
        harness.request(1, BOB, B256::ZERO),
        ZoneOutboxError::too_many_withdrawals_this_block(),
    );
    harness.ctx.block.number = U256::ONE;
    harness.request(1, BOB, B256::ZERO)?;
    assert_revert(
        harness.request(1, BOB, B256::ZERO),
        ZoneOutboxError::too_many_withdrawals_this_block(),
    );
    harness.set_max_withdrawals(2)?;
    harness.request(1, BOB, B256::ZERO)?;
    Ok(())
}

#[test]
fn many_withdrawals_finalize_and_clear_pending_state() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    for i in 0..20u64 {
        harness.request(1, BOB, B256::from(U256::from(i)))?;
    }
    harness.finalize(20)?;
    assert!(harness.pending()?.is_empty());
    Ok(())
}

#[test]
fn legacy_withdrawal_matches_current_overload_and_defaults_reveal_to() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let token = harness.token;
    let memo = B256::repeat_byte(0x11);
    let data = Bytes::from_static(b"callback");

    harness.request_custom(ZoneOutboxAbi::requestWithdrawalCall {
        token,
        to: BOB,
        amount: 123,
        memo,
        gasLimit: 7,
        fallbackRecipient: ALICE,
        data: data.clone(),
        revealTo: Bytes::new(),
    })?;
    harness.call(
        ALICE,
        ILegacyZoneOutbox::requestWithdrawalCall {
            token,
            to: BOB,
            amount: 123,
            memo,
            gasLimit: 7,
            fallbackRecipient: ALICE,
            data,
        }
        .abi_encode(),
    )?;

    let pending = harness.pending()?;
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0], pending[1]);
    assert!(pending[1].revealTo.is_empty());
    Ok(())
}

#[test]
fn malformed_legacy_withdrawal_reverts_with_empty_data() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let output = harness.call(ALICE, ILegacyZoneOutbox::requestWithdrawalCall::SELECTOR)?;
    assert!(output.is_revert());
    assert!(output.bytes.is_empty());
    assert!(harness.pending()?.is_empty());
    Ok(())
}

#[test]
fn static_mutation_reverts_with_static_call_not_allowed() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let token = harness.token;
    assert_revert(
        harness.call_static(
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
        ),
        ZoneOutboxError::static_call_not_allowed(),
    );
    assert!(harness.pending()?.is_empty());
    Ok(())
}
