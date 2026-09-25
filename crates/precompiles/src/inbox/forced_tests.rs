use super::*;
use crate::{ecies, forced_exit_storage::ForcedExitPortalStorage};
use exithatch::ForcedExitAuthorization;
use k256::ecdsa::SigningKey;
use revm::context_interface::JournalTr;

const ROOT: Address = address!("7e5f4552091a69125d5dfcb7b8c2659029395bdf");
const PARENT_CHAIN: u64 = 42431;

fn set_spec(h: &mut Harness, spec: TempoHardfork) {
    h.ctx.cfg.spec = spec;
    let env = test_env(&h.ctx);
    h.precompile = ZoneInbox::create(h.l1_state.clone(), &env);
    h.outbox_precompile = crate::create_outbox_precompile(h.l1_state.clone(), &env);
}

fn active_harness() -> eyre::Result<Harness> {
    let mut h = Harness::new()?;
    set_spec(&mut h, TempoHardfork::T13);
    h.l1.with_storage(1, || {
        ForcedExitPortalStorage::new(PORTAL)
            .forced_exit_version
            .write(1)
    })?;
    Ok(h)
}

fn auth(nonce: u64) -> ForcedExitAuthorization {
    ForcedExitAuthorization {
        account: ROOT,
        zoneChainId: U256::from(
            zone_primitives::constants::zone_chain_id(PARENT_CHAIN, 1).unwrap(),
        ),
        token: PATH_USD_ADDRESS,
        recipient: BOB,
        nonce: U256::from(nonce),
        admitBefore: 1,
    }
}

fn signed_payload(auth: &ForcedExitAuthorization) -> Vec<u8> {
    let signer = SigningKey::from_slice(&U256::from(1).to_be_bytes::<32>()).unwrap();
    let digest = exithatch::authorization_digest(auth, U256::from(PARENT_CHAIN), PORTAL);
    let (sig, recovery) = signer.sign_prehash_recoverable(&digest.0).unwrap();
    let mut bytes = sig.to_bytes().to_vec();
    bytes.push(recovery.to_byte());
    exithatch::encode_payload(auth, &bytes)
}

fn request(
    h: &mut Harness,
    id: u64,
    number: u64,
    plaintext: &[u8],
) -> eyre::Result<(QueuedDeposit, DecryptionData)> {
    h.ctx.cfg.chain_id = zone_primitives::constants::zone_chain_id(PARENT_CHAIN, 1).unwrap();
    let secret = k256::SecretKey::from_slice(&[0x33; 32])?;
    let (x, parity) = ecies::compressed_x_and_parity(secret.public_key().as_affine());
    let encrypted = ecies::encrypt_payload(
        &x,
        parity,
        plaintext,
        ecies::EncryptionContext {
            portal: PORTAL,
            sender: ALICE,
            key_index: U256::ZERO,
        },
        &mut k256::elliptic_curve::rand_core::OsRng,
        Some("forced-exit-v1"),
    )
    .unwrap();
    let proof = ecies::compute_ecdh_proof(
        &secret,
        &encrypted.ephemeralPubkeyX,
        encrypted.ephemeralPubkeyYParity,
    )
    .unwrap();
    h.l1.with_storage(1, || {
        let mut portal = ZonePortalStorage::new(PORTAL);
        portal.encryption_keys[0].x.write(x)?;
        portal.encryption_keys[0].y_parity.write(parity)?;
        portal.token_configs[PATH_USD_ADDRESS].enabled.write(true)?;
        let mut forced = ForcedExitPortalStorage::new(PORTAL);
        forced.forced_exit_requests[id]
            .token
            .write(PATH_USD_ADDRESS)?;
        forced.forced_exit_requests[id].deposit_number.write(number)
    })?;
    let entry = ForcedExit {
        requestId: id,
        token: PATH_USD_ADDRESS,
        feePayer: ALICE,
        keyIndex: U256::ZERO,
        requestedAtBlock: 1,
        requestedAtTime: 0,
        encrypted,
    };
    Ok((
        QueuedDeposit {
            depositType: DepositType::ForcedExit,
            depositData: entry.abi_encode().into(),
            rejected: true,
        },
        DecryptionData {
            sharedSecret: proof.shared_secret,
            sharedSecretYParity: proof.shared_secret_y_parity,
            cpProof: proof.cp_proof,
        },
    ))
}

fn fund(h: &mut Harness, amount: U256) -> eyre::Result<()> {
    let mut storage = test_storage_provider(&mut h.ctx, u64::MAX, false);
    StorageCtx::enter(&mut storage, || {
        Ok(TIP20Token::from_address(PATH_USD_ADDRESS)?
            .mint(ALICE, ITIP20::mintCall { to: ROOT, amount })?)
    })
}

fn execute(h: &mut Harness, requests: Vec<(QueuedDeposit, DecryptionData)>) -> PrecompileResult {
    let (entries, proofs): (Vec<_>, Vec<_>) = requests.into_iter().unzip();
    let hash = entries.iter().fold(B256::ZERO, |hash, entry| {
        let forced = ForcedExit::abi_decode(&entry.depositData).unwrap();
        exithatch::request_queue_hash(&forced, hash)
    });
    h.set_queue_hash(hash);
    let count = entries.len() as u64;
    let result = h.call(Address::ZERO, h.advance_call(entries, proofs).abi_encode())?;
    if result.is_success() {
        let mut storage = test_storage_provider(&mut h.ctx, u64::MAX, false);
        StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
            assert_eq!(ZoneInbox::new().processed_deposit_number()?, count);
            Ok(())
        })
        .unwrap();
    }
    Ok(result)
}

fn consumed(h: &mut Harness, nonce: u64) -> eyre::Result<bool> {
    let mut storage = test_storage_provider(&mut h.ctx, u64::MAX, false);
    StorageCtx::enter(&mut storage, || {
        Ok(ZoneInbox::new().forced_exit_nonces[ROOT][U256::from(nonce)].read()?)
    })
}

#[test]
fn forced_exit_accepts_earlier_admission_but_rejects_future_height() -> eyre::Result<()> {
    for admission_block in [0, 2] {
        let mut h = active_harness()?;
        fund(&mut h, U256::from(42))?;
        let (mut queued, proof) = request(&mut h, 1, 1, &signed_payload(&auth(1)))?;
        let mut entry = ForcedExit::abi_decode(&queued.depositData)?;
        // The harness imports block 1. Earlier admission models deferred portal work.
        entry.requestedAtBlock = admission_block;
        queued.depositData = entry.abi_encode().into();
        let result = execute(&mut h, vec![(queued, proof)])?;
        if admission_block == 0 {
            assert!(result.is_success(), "{result:?}");
            assert!(consumed(&mut h, 1)?);
            assert_eq!(h.balance(PATH_USD_ADDRESS, ROOT)?, U256::ZERO);
            assert_eq!(h.pending_withdrawals()?.len(), 1);
        } else {
            assert!(result.is_revert());
            assert!(!consumed(&mut h, 1)?);
            assert_eq!(h.balance(PATH_USD_ADDRESS, ROOT)?, U256::from(42));
            assert!(h.pending_withdrawals()?.is_empty());
        }
    }
    Ok(())
}

#[test]
fn l1_token_pause_rejects_admitted_requests_without_blocking_inbox() -> eyre::Result<()> {
    for paused in [true, false] {
        let mut h = active_harness()?;
        fund(&mut h, U256::from(42))?;
        let first = request(&mut h, 1, 1, &signed_payload(&auth(1)))?;
        let second = request(&mut h, 2, 2, &signed_payload(&auth(2)))?;
        // Admission occurred while unpaused; model the final state after a same-block
        // pause (or pause followed by unpause). The Zone-local token remains unpaused.
        let mut pause_slot = tempo_precompiles::storage::Slot::<bool>::new(
            tempo_precompiles::tip20::slots::PAUSED,
            PATH_USD_ADDRESS,
        );
        h.l1.with_storage(0, || pause_slot.write(!paused))?;
        h.l1.with_storage(1, || pause_slot.write(paused))?;

        // execute checks that both entries advance the inbox cursor.
        assert!(execute(&mut h, vec![first, second])?.is_success());
        assert!(h.l1.requested(1, &pause_slot));
        assert!(!h.l1.requested(0, &pause_slot));
        assert!(consumed(&mut h, 1)? && consumed(&mut h, 2)?);
        assert_eq!(
            h.balance(PATH_USD_ADDRESS, ROOT)?,
            if paused { U256::from(42) } else { U256::ZERO }
        );
        let pending = h.pending_withdrawals()?;
        assert_eq!(pending.len(), usize::from(!paused));
        if !paused {
            assert_eq!(pending[0].amount, 42);
            assert_eq!(pending[0].to, BOB);
        }
    }
    Ok(())
}

#[test]
fn forced_full_balance_replay_and_empty_finalize_with_exact_commitment() -> eyre::Result<()> {
    let mut h = active_harness()?;
    fund(&mut h, U256::from(42))?;
    let requests = [7, 7, 8]
        .into_iter()
        .enumerate()
        .map(|(i, n)| {
            request(
                &mut h,
                i as u64 + 1,
                i as u64 + 1,
                &signed_payload(&auth(n)),
            )
        })
        .collect::<eyre::Result<Vec<_>>>()?;
    let result = execute(&mut h, requests)?;
    assert!(result.is_success(), "{result:?}");
    assert!(consumed(&mut h, 7)? && consumed(&mut h, 8)?);
    assert_eq!(h.balance(PATH_USD_ADDRESS, ROOT)?, U256::ZERO);
    let pending = h.pending_withdrawals()?;
    assert_eq!(pending.len(), 1);
    assert_eq!(h.fallback_recipient(pending[0].fallbackNonce)?, ROOT);
    let withdrawal = tempo_zone_contracts::Withdrawal {
        token: PATH_USD_ADDRESS,
        senderTag: tempo_zone_contracts::Withdrawal::sender_tag(
            ROOT,
            keccak256(signed_payload(&auth(7))),
            pending[0].fallbackNonce,
        ),
        to: BOB,
        amount: 42,
        memo: B256::ZERO,
        gasLimit: 0,
        fallbackNonce: pending[0].fallbackNonce,
        callbackData: Bytes::new(),
        encryptedSender: Bytes::new(),
    };
    let call = IZoneOutbox::finalizeWithdrawalBatchCall {
        blockNumber: h.ctx.block.inner.number.to::<u64>(),
        count: U256::from(1),
        encryptedSenders: vec![Bytes::new()],
    };
    let finalized = call_precompile(
        &mut h.ctx,
        &h.outbox_precompile,
        Address::ZERO,
        &call.abi_encode(),
        GAS,
        false,
        ZONE_OUTBOX_ADDRESS,
        ZONE_OUTBOX_ADDRESS,
    )?;
    assert!(finalized.is_success());
    assert_eq!(
        IZoneOutbox::finalizeWithdrawalBatchCall::abi_decode_returns(&finalized.bytes)?,
        tempo_zone_contracts::Withdrawal::queue_hash(&[withdrawal])
    );
    Ok(())
}

#[test]
fn forced_failure_categories_preserve_nonce_and_principal_boundaries() -> eyre::Result<()> {
    for case in 0..8 {
        let mut h = active_harness()?;
        let mut a = auth(1);
        let balance = if case == 6 {
            U256::ZERO
        } else {
            U256::from(100)
        };
        fund(&mut h, balance)?;
        match case {
            2 => a.admitBefore = 0,
            3 => a.account = BOB,
            4 => a.token = Address::repeat_byte(5),
            _ => {}
        }
        let mut plaintext = signed_payload(&a);
        if case == 1 {
            plaintext[0] = 1;
        }
        if case == 7 {
            plaintext = exithatch::encode_payload(&a, &[3; 65]);
        }
        let mut req = request(&mut h, 1, 1, &plaintext)?;
        if case == 0 {
            let mut entry = ForcedExit::abi_decode(&req.0.depositData)?;
            entry.encrypted.tag[0] ^= 1;
            req.0.depositData = entry.abi_encode().into();
        }
        if case == 5 || case == 6 {
            h.l1.with_storage(1, || {
                ZonePortalStorage::new(PORTAL).token_configs[PATH_USD_ADDRESS]
                    .enabled
                    .write(false)
            })?;
        }
        assert!(execute(&mut h, vec![req])?.is_success(), "case {case}");
        assert_eq!(consumed(&mut h, 1)?, matches!(case, 5 | 6));
        assert_eq!(h.balance(PATH_USD_ADDRESS, ROOT)?, balance);
        assert!(h.pending_withdrawals()?.is_empty());
    }
    Ok(())
}

#[test]
fn later_bad_proof_rolls_back_earlier_exit_without_an_external_checkpoint() -> eyre::Result<()> {
    let mut h = active_harness()?;
    fund(&mut h, U256::from(50))?;
    let first = request(&mut h, 1, 1, &signed_payload(&auth(1)))?;
    let mut second = request(&mut h, 2, 2, &signed_payload(&auth(2)))?;
    second.1.cpProof.c = B256::ZERO;
    let result = execute(&mut h, vec![first, second])?;
    assert!(result.is_revert());
    assert_eq!(h.balance(PATH_USD_ADDRESS, ROOT)?, U256::from(50));
    assert!(!consumed(&mut h, 1)?);
    assert!(h.pending_withdrawals()?.is_empty());
    let mut storage = test_storage_provider(&mut h.ctx, u64::MAX, false);
    StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
        assert_eq!(ZoneInbox::new().processed_deposit_number()?, 0);
        assert_eq!(TempoState::new().tempo_block_number()?, 0);
        Ok(())
    })
}

#[test]
fn forced_balance_boundaries_and_exhausted_ordinary_capacity() -> eyre::Result<()> {
    for amount in [
        U256::ONE,
        U256::from(u128::MAX),
        U256::from(u128::MAX) + U256::ONE,
    ] {
        let mut h = active_harness()?;
        // The current native mint caps supply at u128::MAX. Seed an oversized liquid
        // balance only to exercise the protocol's defensive overflow branch.
        if amount > U256::from(u128::MAX) {
            let mut storage = test_storage_provider(&mut h.ctx, u64::MAX, false);
            StorageCtx::enter(&mut storage, || {
                let slot = keccak256(
                    (ROOT, tempo_precompiles::tip20::slots::BALANCES).abi_encode_params(),
                );
                StorageCtx.sstore(PATH_USD_ADDRESS, U256::from_be_bytes(slot.0), amount)
            })?;
        } else {
            fund(&mut h, amount)?;
        }
        let req = request(&mut h, 1, 1, &signed_payload(&auth(1)))?;
        assert!(execute(&mut h, vec![req])?.is_success());
        assert!(consumed(&mut h, 1)?);
        if amount > U256::from(u128::MAX) {
            assert_eq!(h.balance(PATH_USD_ADDRESS, ROOT)?, amount);
            assert!(h.pending_withdrawals()?.is_empty());
        } else {
            assert_eq!(h.pending_withdrawals()?.len(), 1);
            assert_eq!(h.pending_withdrawals()?[0].amount, amount.to::<u128>());
            assert_eq!(h.balance(PATH_USD_ADDRESS, ROOT)?, U256::ZERO);
        }
    }
    let mut h = active_harness()?;
    fund(&mut h, U256::from(5))?;
    let req = request(&mut h, 1, 1, &signed_payload(&auth(1)))?;
    let mut storage = test_storage_provider(&mut h.ctx, u64::MAX, false);
    StorageCtx::enter(&mut storage, || {
        let mut outbox = ZoneOutbox::new();
        outbox.max_withdrawals_per_block.write(1)?;
        outbox
            .current_block_number
            .write(StorageCtx.block_number())?;
        outbox.withdrawals_this_block.write(1)
    })?;
    drop(storage);
    assert!(execute(&mut h, vec![req])?.is_success());
    assert!(consumed(&mut h, 1)?);
    assert_eq!(h.balance(PATH_USD_ADDRESS, ROOT)?, U256::ZERO);
    assert_eq!(h.pending_withdrawals()?.len(), 1);
    let mut storage = test_storage_provider(&mut h.ctx, u64::MAX, false);
    StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
        assert_eq!(ZoneOutbox::new().withdrawals_this_block.read()?, 1);
        Ok(())
    })?;
    Ok(())
}

#[test]
fn maximum_forced_exit_workload_fits_system_gas_budget() -> eyre::Result<()> {
    const REQUESTS: usize = 210 / 14; // ZonePortal.FORCED_EXIT_ADMISSION_WEIGHT
    let mut h = active_harness()?;
    let enabled_tokens = (1..=8).map(maximum_metadata_token).collect::<Vec<_>>();
    h.set_token_enablements(&enabled_tokens);
    let mut requests = Vec::new();
    for id in 1..=REQUESTS {
        let signer = SigningKey::from_slice(&U256::from(id).to_be_bytes::<32>()).unwrap();
        let mut a = auth(id as u64);
        a.account = Address::from_public_key(signer.verifying_key());
        let digest = exithatch::authorization_digest(&a, U256::from(PARENT_CHAIN), PORTAL);
        let (sig, recovery) = signer.sign_prehash_recoverable(&digest.0).unwrap();
        let mut signature = sig.to_bytes().to_vec();
        signature.push(recovery.to_byte());
        let mut storage = test_storage_provider(&mut h.ctx, u64::MAX, false);
        StorageCtx::enter(&mut storage, || {
            TIP20Token::from_address(PATH_USD_ADDRESS)?.mint(
                ALICE,
                ITIP20::mintCall {
                    to: a.account,
                    amount: U256::ONE,
                },
            )
        })?;
        drop(storage);
        requests.push(request(
            &mut h,
            id as u64,
            id as u64,
            &exithatch::encode_payload(&a, &signature),
        )?);
    }
    let (entries, proofs): (Vec<_>, Vec<_>) = requests.into_iter().unzip();
    h.set_queue_hash(entries.iter().fold(B256::ZERO, |hash, e| {
        exithatch::request_queue_hash(&ForcedExit::abi_decode(&e.depositData).unwrap(), hash)
    }));
    let call = h.advance_call_with_tokens(entries, proofs, enabled_tokens);
    let executed = h.call_with_gas(Address::ZERO, call.abi_encode(), u64::MAX)?;
    assert!(executed.is_success(), "{executed:?}");
    assert_eq!(h.pending_withdrawals()?.len(), REQUESTS);
    // Conservatively charge the maximum admitted payload/signature size for every
    // successful request, and add a separate full transition for the 20 reserved
    // refund entries. This overcounts header/cursor work rather than hiding it.
    let maximum_crypto_extra = REQUESTS as u64 * (2368 * 3 + 2048 * 16);
    let gas_bound = executed.gas_used + maximum_crypto_extra + failed_deposit_gas(20, 0)?;
    eprintln!(
        "{REQUESTS} forced exits + 8 token initializations + 20 refund bound: {gas_bound} gas (forced transition {})",
        executed.gas_used
    );
    assert!(
        gas_bound <= 225_000_000,
        "forced inbox workload exceeds system budget: {gas_bound}"
    );
    let finalize = IZoneOutbox::finalizeWithdrawalBatchCall {
        blockNumber: h.ctx.block.inner.number.to::<u64>(),
        count: U256::from(REQUESTS),
        encryptedSenders: vec![Bytes::new(); REQUESTS],
    };
    let result = call_precompile(
        &mut h.ctx,
        &h.outbox_precompile,
        Address::ZERO,
        &finalize.abi_encode(),
        u64::MAX,
        false,
        ZONE_OUTBOX_ADDRESS,
        ZONE_OUTBOX_ADDRESS,
    )?;
    assert!(result.is_success());
    eprintln!(
        "{REQUESTS} ordinary withdrawals finalization: {} gas",
        result.gas_used
    );
    assert!(result.gas_used < 225_000_000);
    Ok(())
}

#[test]
fn forced_exit_keeps_nonce_consumed_through_pending_credit_recovery() -> eyre::Result<()> {
    let mut h = active_harness()?;
    fund(&mut h, U256::from(17))?;
    let req = request(&mut h, 1, 1, &signed_payload(&auth(1)))?;
    assert!(execute(&mut h, vec![req])?.is_success());
    let nonce = h.pending_withdrawals()?[0].fallbackNonce;
    let mut encoded_nonce = [0; 20];
    encoded_nonce[12..].copy_from_slice(&nonce.to_be_bytes());
    {
        let mut storage = test_storage_provider(&mut h.ctx, u64::MAX, false);
        StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
            let mut token = TIP20Token::from_address(PATH_USD_ADDRESS)?;
            token.change_transfer_policy_id(
                ALICE,
                ITIP20::changeTransferPolicyIdCall {
                    newPolicyId: REJECT_ALL_POLICY_ID,
                },
            )?;
            let mut inbox = ZoneInbox::new();
            inbox.process_withdrawal_bounce_back(
                &mut ZoneOutbox::new(),
                WithdrawalBounceBackDeposit {
                    token: PATH_USD_ADDRESS,
                    to: Address::from(encoded_nonce),
                    amount: 17,
                },
            )?;
            assert_eq!(
                inbox.withdrawal_bounce_backs[PATH_USD_ADDRESS][ROOT].read()?,
                17
            );
            token.change_transfer_policy_id(
                ALICE,
                ITIP20::changeTransferPolicyIdCall {
                    newPolicyId: ALLOW_ALL_POLICY_ID,
                },
            )?;
            Ok(())
        })?;
    }
    assert_eq!(h.fallback_recipient(nonce)?, Address::ZERO);
    assert_eq!(h.balance(PATH_USD_ADDRESS, ROOT)?, U256::ZERO);
    assert!(
        h.call(
            ROOT,
            IZoneInbox::claimRefundCall {
                token: PATH_USD_ADDRESS
            }
            .abi_encode()
        )?
        .is_success()
    );
    assert_eq!(h.balance(PATH_USD_ADDRESS, ROOT)?, U256::from(17));
    assert!(consumed(&mut h, 1)?);
    Ok(())
}

#[test]
fn forced_authorization_nonce_is_shared_across_tokens() -> eyre::Result<()> {
    let mut h = active_harness()?;
    let first = request(&mut h, 1, 1, &signed_payload(&auth(1)))?;
    let mut other_auth = auth(1);
    other_auth.token = address!("20c0000000000000000000000000000000000002");
    let mut second = request(&mut h, 2, 2, &signed_payload(&other_auth))?;
    let mut entry = ForcedExit::abi_decode(&second.0.depositData)?;
    entry.token = other_auth.token;
    second.0.depositData = entry.abi_encode().into();
    h.l1.with_storage(1, || {
        ForcedExitPortalStorage::new(PORTAL).forced_exit_requests[2]
            .token
            .write(other_auth.token)
    })?;
    assert!(execute(&mut h, vec![first, second])?.is_success());
    assert!(consumed(&mut h, 1)?);
    assert!(h.pending_withdrawals()?.is_empty());
    Ok(())
}

#[test]
fn admitted_forced_workload_preserves_ordinary_capacity() -> eyre::Result<()> {
    const REQUESTS: u64 = 15;
    for used in [0, 1] {
        let mut h = active_harness()?;
        let mut accounts = Vec::new();
        let mut requests = Vec::new();
        for id in 1..=REQUESTS {
            let signer = SigningKey::from_slice(&U256::from(id).to_be_bytes::<32>())?;
            let mut a = auth(id);
            a.account = Address::from_public_key(signer.verifying_key());
            accounts.push(a.account);
            let digest = exithatch::authorization_digest(&a, U256::from(PARENT_CHAIN), PORTAL);
            let (sig, recovery) = signer.sign_prehash_recoverable(&digest.0)?;
            let mut signature = sig.to_bytes().to_vec();
            signature.push(recovery.to_byte());
            {
                let mut storage = test_storage_provider(&mut h.ctx, u64::MAX, false);
                StorageCtx::enter(&mut storage, || {
                    TIP20Token::from_address(PATH_USD_ADDRESS)?.mint(
                        ALICE,
                        ITIP20::mintCall {
                            to: a.account,
                            amount: U256::ONE,
                        },
                    )
                })?;
            }
            requests.push(request(
                &mut h,
                id,
                id,
                &exithatch::encode_payload(&a, &signature),
            )?);
        }
        {
            let mut storage = test_storage_provider(&mut h.ctx, u64::MAX, false);
            StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
                let mut outbox = ZoneOutbox::new();
                outbox.max_withdrawals_per_block.write(1)?;
                outbox
                    .current_block_number
                    .write(StorageCtx.block_number())?;
                outbox.withdrawals_this_block.write(used)?;
                Ok(())
            })?;
        }
        let (entries, proofs): (Vec<_>, Vec<_>) = requests.into_iter().unzip();
        h.set_queue_hash(entries.iter().fold(B256::ZERO, |tail, entry| {
            exithatch::request_queue_hash(
                &ForcedExit::abi_decode(&entry.depositData).unwrap(),
                tail,
            )
        }));
        let call = h.advance_call(entries, proofs).abi_encode();
        let result = h.call_with_gas(Address::ZERO, call, 225_000_000)?;
        assert!(
            result.is_success(),
            "ordinary slots used={used}: {result:?}"
        );
        let pending = h.pending_withdrawals()?;
        assert_eq!(pending.len(), REQUESTS as usize);
        for (i, account) in accounts.iter().enumerate() {
            assert_eq!(h.balance(PATH_USD_ADDRESS, *account)?, U256::ZERO);
            assert_eq!(pending[i].amount, 1);
            assert_eq!(h.fallback_recipient(pending[i].fallbackNonce)?, *account);
        }
        let mut storage = test_storage_provider(&mut h.ctx, u64::MAX, false);
        StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
            let inbox = ZoneInbox::new();
            assert_eq!(inbox.processed_deposit_number()?, REQUESTS);
            assert_eq!(TempoState::new().tempo_block_number()?, 1);
            assert_eq!(ZoneOutbox::new().withdrawals_this_block.read()?, used);
            for (i, account) in accounts.iter().enumerate() {
                assert!(inbox.forced_exit_nonces[*account][U256::from(i + 1)].read()?);
            }
            Ok(())
        })?;
    }
    Ok(())
}

#[test]
fn zero_authorization_fields_reject_without_blocking_next_request() -> eyre::Result<()> {
    for zero_account in [true, false] {
        let mut h = active_harness()?;
        fund(&mut h, U256::from(42))?;
        let mut invalid = auth(1);
        if zero_account {
            invalid.account = Address::ZERO;
        } else {
            invalid.recipient = Address::ZERO;
        }
        let first = request(&mut h, 1, 1, &signed_payload(&invalid))?;
        let second = request(&mut h, 2, 2, &signed_payload(&auth(2)))?;
        assert!(execute(&mut h, vec![first, second])?.is_success());
        assert!(!consumed(&mut h, 1)?);
        assert!(consumed(&mut h, 2)?);
        let pending = h.pending_withdrawals()?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].amount, 42);
        assert_eq!(pending[0].fallbackNonce, 1);
        assert_eq!(pending[0].txHash, keccak256(signed_payload(&auth(2))));
        assert_eq!(h.balance(PATH_USD_ADDRESS, ROOT)?, U256::ZERO);
        let mut storage = test_storage_provider(&mut h.ctx, u64::MAX, false);
        StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
            assert!(!ZoneInbox::new().forced_exit_nonces[Address::ZERO][U256::ONE].read()?);
            Ok(())
        })?;
    }
    Ok(())
}

#[test]
fn policy_rejection_consumes_nonce_without_debit_and_processes_next_request() -> eyre::Result<()> {
    let mut h = active_harness()?;
    fund(&mut h, U256::from(42))?;
    let mut rejected = auth(1);
    rejected.recipient = Address::repeat_byte(0x77);
    let first = request(&mut h, 1, 1, &signed_payload(&rejected))?;
    let second = request(&mut h, 2, 2, &signed_payload(&auth(2)))?;
    h.l1.with_storage(1, || -> tempo_precompiles::Result<()> {
        let mut portal = ZonePortalStorage::new(PORTAL);
        portal.is_access_enforced.write(true)?;
        portal.role[BOB].write(u8::from(tempo_zone_contracts::ZonePortal::Role::Account))?;
        Ok(())
    })?;
    assert!(execute(&mut h, vec![first, second])?.is_success());
    assert!(consumed(&mut h, 1)? && consumed(&mut h, 2)?);
    let pending = h.pending_withdrawals()?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].to, BOB);
    assert_eq!(pending[0].amount, 42);
    assert_eq!(pending[0].fallbackNonce, 1);
    assert_eq!(pending[0].txHash, keccak256(signed_payload(&auth(2))));
    assert_eq!(h.fallback_recipient(1)?, ROOT);
    assert_eq!(h.balance(PATH_USD_ADDRESS, ROOT)?, U256::ZERO);
    assert_eq!(
        h.balance(PATH_USD_ADDRESS, ZONE_OUTBOX_ADDRESS)?,
        U256::ZERO
    );
    let mut storage = test_storage_provider(&mut h.ctx, u64::MAX, false);
    StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
        assert_eq!(
            TIP20Token::from_address(PATH_USD_ADDRESS)?.total_supply()?,
            U256::ZERO
        );
        assert_eq!(ZoneOutbox::new().last_fallback_nonce.read()?, 1);
        Ok(())
    })?;
    Ok(())
}

#[test]
fn forced_requests_require_t13_even_when_no_withdrawal_would_be_created() -> eyre::Result<()> {
    // Success, Empty, and invalid plaintext must all be rejected before activation.
    for (balance, invalid_payload) in [(42, false), (0, false), (42, true)] {
        let mut h = active_harness()?;
        fund(&mut h, U256::from(balance))?;
        let payload = if invalid_payload {
            vec![0; 384]
        } else {
            signed_payload(&auth(1))
        };
        let entry = request(&mut h, 1, 1, &payload)?;
        set_spec(&mut h, TempoHardfork::T12);
        let logs_before = h.ctx.journaled_state.logs().to_vec();
        assert!(execute(&mut h, vec![entry.clone()])?.is_revert());
        assert_eq!(h.ctx.journaled_state.logs(), logs_before.as_slice());
        assert!(!consumed(&mut h, 1)?);
        assert_eq!(h.balance(PATH_USD_ADDRESS, ROOT)?, U256::from(balance));
        assert!(h.pending_withdrawals()?.is_empty());
        let metadata = ForcedExitPortalStorage::new(PORTAL);
        assert!(!h.l1.requested(1, &metadata.forced_exit_requests[1].token));
        let mut storage = test_storage_provider(&mut h.ctx, u64::MAX, false);
        StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
            assert_eq!(ZoneInbox::new().processed_deposit_number()?, 0);
            assert_eq!(ZoneInbox::new().processed_deposit_queue_hash()?, B256::ZERO);
            assert_eq!(TempoState::new().tempo_block_number()?, 0);
            Ok(())
        })?;
        drop(storage);
        set_spec(&mut h, TempoHardfork::T13);
        assert!(execute(&mut h, vec![entry])?.is_success());
        assert_eq!(consumed(&mut h, 1)?, !invalid_payload);
    }
    Ok(())
}

#[test]
fn forced_requests_use_activation_version_at_execution_anchor() -> eyre::Result<()> {
    for version in [0, 1, 2, u64::MAX] {
        for (balance, invalid_payload) in [(42, false), (0, false), (42, true)] {
            let mut h = active_harness()?;
            let mut portal = ForcedExitPortalStorage::new(PORTAL);
            // Parent and later state disagree with the imported block deliberately.
            h.l1.with_storage(0, || {
                portal.forced_exit_version.write(u64::from(version != 1))
            })?;
            h.l1.with_storage(1, || portal.forced_exit_version.write(version))?;
            h.l1.with_storage(2, || {
                portal.forced_exit_version.write(u64::from(version != 1))
            })?;
            fund(&mut h, U256::from(balance))?;
            let payload = if invalid_payload {
                vec![0; 384]
            } else {
                signed_payload(&auth(1))
            };
            let entry = request(&mut h, 1, 1, &payload)?;
            let logs_before = h.ctx.journaled_state.logs().to_vec();
            let result = execute(&mut h, vec![entry])?;
            assert!(h.l1.requested(1, &portal.forced_exit_version));
            assert!(!h.l1.requested(0, &portal.forced_exit_version));
            assert!(!h.l1.requested(2, &portal.forced_exit_version));
            if version == 1 {
                assert!(result.is_success());
                assert_eq!(consumed(&mut h, 1)?, !invalid_payload);
            } else {
                assert!(result.is_revert());
                assert_eq!(h.ctx.journaled_state.logs(), logs_before.as_slice());
                assert!(!consumed(&mut h, 1)?);
                assert_eq!(h.balance(PATH_USD_ADDRESS, ROOT)?, U256::from(balance));
                assert!(h.pending_withdrawals()?.is_empty());
                assert!(!h.l1.requested(1, &portal.forced_exit_requests[1].token));
                let mut storage = test_storage_provider(&mut h.ctx, u64::MAX, false);
                StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
                    assert_eq!(ZoneInbox::new().processed_deposit_number()?, 0);
                    assert_eq!(ZoneInbox::new().processed_deposit_queue_hash()?, B256::ZERO);
                    assert_eq!(TempoState::new().tempo_block_number()?, 0);
                    Ok(())
                })?;
            }
        }
    }
    Ok(())
}
