use super::*;

use alloc::borrow::Cow;
use alloy_evm::precompiles::PrecompilesMap;
use alloy_primitives::address;
use core::cell::RefCell;
use revm::precompile::Precompiles;
use std::rc::Rc;
use tempo_precompiles::{
    RECEIVE_POLICY_GUARD_ADDRESS,
    storage::{Handler, StorageCtx, StorageKey, actions::StorageActions},
    storage_credits::NonCreditableSlots,
    test_util::TIP20Setup,
    tip20::{ITIP20, TIP20Token},
    tip403_registry::{ALLOW_ALL_POLICY_ID, ITIP403Registry, REJECT_ALL_POLICY_ID, TIP403Registry},
};
use zone_primitives::fast_transfer::{AssetId, ZoneDomain};

use crate::{
    TempoState,
    test_utils::{MockL1Reader, test_context, test_context_with_hardfork, test_storage_provider},
};

const SENDER: Address = address!("0x00000000000000000000000000000000000000a1");
const RECIPIENT: Address = address!("0x00000000000000000000000000000000000000b2");
const OPERATOR: Address = address!("0x00000000000000000000000000000000000000c3");
const PORTAL: Address = address!("0x00000000000000000000000000000000000000d4");
const TOKEN: Address = address!("0x20c0000000000000000000000000000000000000");

fn intent(nonce: u64) -> TransferIntent {
    TransferIntent {
        source: ZoneDomain {
            l1_chain_id: 1,
            zone_id: 1,
            chain_id: 101,
            portal: PORTAL,
            authority_epoch: 7,
            roster_hash: B256::repeat_byte(0x11),
            protocol_version: 1,
        },
        destination: ZoneDomain {
            l1_chain_id: 1,
            zone_id: 2,
            chain_id: 102,
            portal: Address::repeat_byte(0x44),
            authority_epoch: 8,
            roster_hash: B256::repeat_byte(0x22),
            protocol_version: 1,
        },
        asset: AssetId {
            l1_token: TOKEN,
            source_token: TOKEN,
            destination_token: TOKEN,
            decimals: 6,
        },
        sender: SENDER,
        recipient: RECIPIENT,
        refund_account: SENDER,
        destination_pool: OPERATOR,
        reimbursement_account: OPERATOR,
        principal: U256::from(90),
        fee: U256::from(10),
        quote_id: B256::repeat_byte(0x55),
        destination_expiry_height: 100,
        transfer_nonce: nonce,
    }
}

#[test]
fn transfer_identity_and_intent_hash_bind_every_body() {
    let original = intent(9);
    let mut changed = original.clone();
    changed.recipient = Address::repeat_byte(0xee);

    assert_eq!(
        original.transfer_id(),
        changed.transfer_id(),
        "the dedicated nonce defines retry identity"
    );
    assert_ne!(
        original.intent_hash(),
        changed.intent_hash(),
        "the separately stored body hash detects ID reuse"
    );
}

#[test]
fn intent_rejects_cross_asset_mapping_before_any_token_effect() {
    let mut mismatched = intent(8);
    mismatched.asset.destination_token = Address::repeat_byte(0x77);
    assert!(FastTransfer::validate_intent(&mismatched).is_err());

    mismatched.asset.destination_token = TOKEN;
    mismatched.asset.source_token = Address::repeat_byte(0x88);
    assert!(FastTransfer::validate_intent(&mismatched).is_err());
}

#[test]
fn remembered_outcome_is_immutable_and_idempotent() -> eyre::Result<()> {
    let mut ctx = test_context();
    let intent = intent(10);
    let hash = intent.intent_hash();
    let id = intent.transfer_id();
    let mut storage = test_storage_provider(&mut ctx, u64::MAX, false);
    StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
        let mut fast = FastTransfer::new();
        fast.initialize()?;
        fast.write_intent(&intent, hash, TOKEN, 100)?;
        fast.states[id].write(STATE_LOCKED)?;

        assert_eq!(
            fast.record_outcome_verified(id, hash, OUTCOME_PAID)?,
            STATE_PAID_AWAITING_RELEASE
        );
        assert_eq!(
            fast.record_outcome_verified(id, hash, OUTCOME_PAID)?,
            STATE_PAID_AWAITING_RELEASE
        );
        assert!(
            fast.record_outcome_verified(id, hash, OUTCOME_REJECTED)
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
    let hash = intent.intent_hash();
    let id = intent.transfer_id();
    let source = intent.source.domain_hash();
    let mut storage = test_storage_provider(&mut ctx, u64::MAX, false);
    StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
        let mut fast = FastTransfer::new();
        fast.initialize()?;
        fast.write_intent(&intent, hash, TOKEN, 90)?;
        fast.tokens[id].write(TOKEN)?;
        fast.states[id].write(STATE_PAID)?;
        fast.unsettled_exposure[TOKEN][source].write(90)?;

        assert!(
            fast.retire_exposure_verified(id, hash, TOKEN, Address::repeat_byte(0xff), 90,)
                .is_err()
        );
        fast.retire_exposure_verified(id, hash, TOKEN, OPERATOR, 90)?;
        fast.retire_exposure_verified(id, hash, TOKEN, OPERATOR, 90)?;
        assert_eq!(fast.unsettled_exposure[TOKEN][source].read()?, 0);
        assert!(fast.exposure_retired[id].read()?);
        Ok(())
    })
}

#[test]
fn activation_is_exactly_t14() {
    assert!(!fast_transfer_active(TempoHardfork::T13));
    assert!(fast_transfer_active(TempoHardfork::T14));
}

#[test]
fn reserved_address_stays_registered_before_t14_for_explicit_revert() {
    let ctx = test_context_with_hardfork(TempoHardfork::T13);
    let mut precompiles = PrecompilesMap::new(Cow::Owned(Precompiles::default()));
    crate::extend_zone_precompiles(
        &mut precompiles,
        &ctx.cfg,
        L1State::new(MockL1Reader::default(), PORTAL),
        StorageActions::disabled(),
        Rc::new(RefCell::new(NonCreditableSlots::empty())),
    );
    assert!(precompiles.get(&FAST_TRANSFER_ADDRESS).is_some());
}

#[test]
fn asset_validation_binds_local_decimals_and_both_finalized_portal_enablements() -> eyre::Result<()>
{
    const ANCHOR: u64 = 9;

    let mut ctx = test_context_with_hardfork(TempoHardfork::T14);
    let l1 = MockL1Reader::default();
    let transfer = intent(13);
    let source = ZonePortalStorage::new(transfer.source.portal);
    let destination = ZonePortalStorage::new(transfer.destination.portal);
    let source_token_config = source.token_configs[TOKEN].enabled.base_slot();
    let destination_token_config = destination.token_configs[TOKEN].enabled.base_slot();
    for (portal, slot) in [
        (transfer.source.portal, source_token_config),
        (transfer.destination.portal, destination_token_config),
    ] {
        l1.insert(portal, slot, ANCHOR, U256::ONE);
    }
    let l1_state = L1State::new(l1.clone(), transfer.destination.portal);

    let mut storage = test_storage_provider(&mut ctx, u64::MAX, false);
    StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
        TempoState::new().tempo_block_number.write(ANCHOR)?;
        TIP20Setup::path_usd(OPERATOR)
            .with_issuer(OPERATOR)
            .apply()?;

        FastTransfer::validate_asset(&l1_state, &transfer, TOKEN)?;
        let mut wrong_decimals = transfer.clone();
        wrong_decimals.asset.decimals = 18;
        assert!(FastTransfer::validate_asset(&l1_state, &wrong_decimals, TOKEN).is_err());
        Ok(())
    })?;
    drop(storage);

    for (portal, disabled_slot, other_portal, enabled_slot) in [
        (
            transfer.source.portal,
            source_token_config,
            transfer.destination.portal,
            destination_token_config,
        ),
        (
            transfer.destination.portal,
            destination_token_config,
            transfer.source.portal,
            source_token_config,
        ),
    ] {
        l1.insert(other_portal, enabled_slot, ANCHOR, U256::ONE);
        l1.insert(portal, disabled_slot, ANCHOR, U256::ZERO);
        let mut storage = test_storage_provider(&mut ctx, u64::MAX, false);
        StorageCtx::enter(&mut storage, || {
            assert!(FastTransfer::validate_asset(&l1_state, &transfer, TOKEN).is_err());
        });
    }
    Ok(())
}

#[test]
fn destination_height_expiry_commits_precise_rejection_without_pool_movement() -> eyre::Result<()> {
    const ANCHOR: u64 = 9;

    let mut ctx = test_context_with_hardfork(TempoHardfork::T14);
    ctx.block.number = U256::from(100);
    let l1 = MockL1Reader::default();
    let transfer = intent(14);
    for portal_address in [transfer.source.portal, transfer.destination.portal] {
        let portal = ZonePortalStorage::new(portal_address);
        l1.insert(
            portal_address,
            portal.token_configs[TOKEN].enabled.base_slot(),
            ANCHOR,
            U256::ONE,
        );
    }
    let l1 = L1State::new(l1, transfer.destination.portal);

    let mut storage = test_storage_provider(&mut ctx, u64::MAX, false);
    StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
        TempoState::new().tempo_block_number.write(ANCHOR)?;
        let token = TIP20Setup::path_usd(OPERATOR)
            .with_issuer(OPERATOR)
            .with_mint(FAST_TRANSFER_ADDRESS, U256::from(100))
            .apply()?;
        let supply = token.total_supply()?;
        let mut fast = FastTransfer::new();
        fast.initialize()?;
        fast.pool_operators[TOKEN].write(OPERATOR)?;
        fast.pool_balances[TOKEN].write(100)?;
        fast.exposure_limits[TOKEN][transfer.source.domain_hash()].write(100)?;

        assert_eq!(
            fast.resolve_verified(&l1, transfer.clone(), None)?,
            STATE_REJECTED
        );
        assert_eq!(
            fast.rejection_reasons[transfer.transfer_id()].read()?,
            RejectionReason::QuoteExpired as u8
        );
        assert_eq!(fast.pool_balances[TOKEN].read()?, 100);
        assert_eq!(
            token.balance_of(ITIP20::balanceOfCall {
                account: FAST_TRANSFER_ADDRESS,
            })?,
            U256::from(100)
        );
        assert_eq!(token.total_supply()?, supply);
        Ok(())
    })
}

#[test]
fn redirected_destination_credit_is_rolled_back_before_policy_tombstone() -> eyre::Result<()> {
    const ANCHOR: u64 = 9;

    let mut ctx = test_context_with_hardfork(TempoHardfork::T14);
    ctx.block.number = U256::from(1);
    let l1 = MockL1Reader::default();
    let transfer = intent(12);
    for portal_address in [transfer.source.portal, transfer.destination.portal] {
        let portal = ZonePortalStorage::new(portal_address);
        l1.insert(
            portal_address,
            portal.token_configs[TOKEN].enabled.base_slot(),
            ANCHOR,
            U256::ONE,
        );
    }
    let l1 = L1State::new(l1, transfer.destination.portal);

    let mut storage = test_storage_provider(&mut ctx, u64::MAX, false);
    StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
        TempoState::new().tempo_block_number.write(ANCHOR)?;
        TIP20Setup::path_usd(OPERATOR)
            .with_issuer(OPERATOR)
            .with_mint(FAST_TRANSFER_ADDRESS, U256::from(100))
            .apply()?;
        TIP403Registry::new().set_receive_policy(
            RECIPIENT,
            ITIP403Registry::setReceivePolicyCall {
                senderPolicyId: REJECT_ALL_POLICY_ID,
                tokenFilterId: ALLOW_ALL_POLICY_ID,
                recoveryAuthority: RECIPIENT,
            },
        )?;

        let token = TIP20Token::from_address(TOKEN)?;
        let supply = token.total_supply()?;
        let mut fast = FastTransfer::new();
        fast.initialize()?;
        fast.pool_operators[TOKEN].write(OPERATOR)?;
        fast.pool_balances[TOKEN].write(100)?;
        fast.exposure_limits[TOKEN][transfer.source.domain_hash()].write(100)?;

        assert_eq!(
            fast.resolve_verified(&l1, transfer.clone(), None)?,
            STATE_REJECTED
        );
        assert_eq!(
            fast.rejection_reasons[transfer.transfer_id()].read()?,
            RejectionReason::PolicyDenied as u8
        );
        assert_eq!(
            token.balance_of(ITIP20::balanceOfCall { account: RECIPIENT })?,
            U256::ZERO
        );
        assert_eq!(
            token.balance_of(ITIP20::balanceOfCall {
                account: RECEIVE_POLICY_GUARD_ADDRESS,
            })?,
            U256::ZERO,
            "the inner checkpoint must remove the redirected guard claim"
        );
        assert_eq!(
            token.balance_of(ITIP20::balanceOfCall {
                account: FAST_TRANSFER_ADDRESS,
            })?,
            U256::from(100)
        );
        assert_eq!(token.total_supply()?, supply);
        assert_eq!(fast.pool_balances[TOKEN].read()?, 100);
        assert_eq!(
            fast.unsettled_exposure[TOKEN][transfer.source.domain_hash()].read()?,
            0
        );
        Ok(())
    })
}
