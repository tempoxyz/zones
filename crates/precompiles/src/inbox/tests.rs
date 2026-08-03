use super::*;

use alloy_evm::EvmInternals;
use alloy_primitives::{Bytes, address, keccak256};
use alloy_rlp::Encodable as _;
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::PrecompileResult;
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_precompiles::{
    PATH_USD_ADDRESS, RECEIVE_POLICY_GUARD_ADDRESS, TIP403_REGISTRY_ADDRESS,
    receive_policy_guard::ReceivePolicyGuard,
    storage::{ContractStorage, Handler, StorageCtx},
    test_util::TIP20Setup,
    tip20::{ITIP20, TIP20Token},
    tip403_registry::{ALLOW_ALL_POLICY_ID, ITIP403Registry, REJECT_ALL_POLICY_ID, TIP403Registry},
    zone_factory::{ZonePortalStorage, zone_portal_slots},
};
use tempo_primitives::TempoHeader;
use zone_primitives::constants::ZONE_OUTBOX_ADDRESS;

use crate::test_utils::{
    EncryptedDepositFixture, MockL1Reader, TestContext, build_plaintext, call_precompile,
    compressed_x_and_parity, encrypt_plaintext, test_context, test_env, test_storage_provider,
};

const GAS: u64 = 30_000_000;
const PORTAL: Address = address!("0x4242424242424242424242424242424242424242");
const SEQUENCER: Address = address!("0x00000000000000000000000000000000000000a1");
const ALICE: Address = address!("0x00000000000000000000000000000000000000a2");
const BOB: Address = address!("0x00000000000000000000000000000000000000b0");

fn encode_header(header: &TempoHeader) -> Bytes {
    let mut encoded = Vec::new();
    header.encode(&mut encoded);
    encoded.into()
}

struct Harness {
    ctx: TestContext,
    l1: MockL1Reader,
    l1_state: L1State<MockL1Reader>,
    precompile: DynPrecompile,
    outbox_precompile: DynPrecompile,
    genesis_hash: B256,
}

impl Harness {
    fn new() -> eyre::Result<Self> {
        Self::with_l1(MockL1Reader::default())
    }

    fn with_l1(l1: MockL1Reader) -> eyre::Result<Self> {
        let mut ctx = test_context();
        ctx.cfg.spec = TempoHardfork::T9;
        let genesis_rlp = encode_header(&TempoHeader::default());
        let genesis_hash = keccak256(&genesis_rlp);
        {
            let mut storage = test_storage_provider(&mut ctx, u64::MAX, false);
            StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
                TempoState::new().initialize(&genesis_rlp)?;
                ZoneInbox::new().initialize()?;
                ZoneOutbox::new().initialize()?;
                ReceivePolicyGuard::new().initialize()?;
                TIP20Setup::path_usd(ALICE)
                    .with_issuer(ALICE)
                    .with_issuer(ZONE_INBOX_ADDRESS)
                    .with_issuer(ZONE_OUTBOX_ADDRESS)
                    .apply()?;
                Ok(())
            })?;
        }

        l1.seed_active_sequencer(PORTAL, 1, SEQUENCER);
        let l1_state = L1State::new(l1.clone(), PORTAL);
        let env = test_env(&ctx);
        let precompile = ZoneInbox::create(l1_state.clone(), &env);
        let outbox_precompile = crate::create_outbox_precompile(l1_state.clone(), &env);
        Ok(Self {
            ctx,
            l1,
            l1_state,
            precompile,
            outbox_precompile,
            genesis_hash,
        })
    }

    fn child_header(&self) -> TempoHeader {
        TempoHeader {
            inner: alloy_consensus::Header {
                parent_hash: self.genesis_hash,
                number: 1,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn set_queue_hash(&self, hash: B256) {
        self.l1
            .with_storage(1, || {
                ZonePortalStorage::new(PORTAL)
                    .current_deposit_queue_hash
                    .write(hash)
            })
            .unwrap();
    }

    fn advance_call(
        &self,
        deposits: Vec<QueuedDeposit>,
        decryptions: Vec<DecryptionData>,
    ) -> IZoneInbox::advanceTempoCall {
        IZoneInbox::advanceTempoCall {
            header: encode_header(&self.child_header()),
            deposits,
            decryptions,
            enabledTokens: Vec::new(),
        }
    }

    fn call(&mut self, caller: Address, calldata: impl AsRef<[u8]>) -> PrecompileResult {
        self.call_with_gas(caller, calldata, GAS)
    }

    fn call_with_gas(
        &mut self,
        caller: Address,
        calldata: impl AsRef<[u8]>,
        gas: u64,
    ) -> PrecompileResult {
        call_precompile(
            &mut self.ctx,
            &self.precompile,
            caller,
            calldata.as_ref(),
            gas,
            false,
            ZONE_INBOX_ADDRESS,
            ZONE_INBOX_ADDRESS,
        )
    }

    fn call_atomic(&mut self, caller: Address, calldata: impl AsRef<[u8]>) -> PrecompileResult {
        let checkpoint = EvmInternals::from_context(&mut self.ctx).checkpoint();
        let result = self.call(caller, calldata);
        let success = result.as_ref().is_ok_and(|output| output.is_success());
        let mut internals = EvmInternals::from_context(&mut self.ctx);
        if success {
            internals.checkpoint_commit();
        } else {
            internals.checkpoint_revert(checkpoint);
        }
        result
    }

    fn balance(&mut self, token: Address, owner: Address) -> eyre::Result<U256> {
        let mut storage = test_storage_provider(&mut self.ctx, u64::MAX, false);
        StorageCtx::enter(&mut storage, || {
            Ok(TIP20Token::from_address(token)?
                .balance_of(ITIP20::balanceOfCall { account: owner })?)
        })
    }

    fn pending_withdrawals(&mut self) -> eyre::Result<Vec<IZoneOutbox::PendingWithdrawal>> {
        let calldata = IZoneOutbox::getPendingWithdrawalsCall {}.abi_encode();
        let output = call_precompile(
            &mut self.ctx,
            &self.outbox_precompile,
            Address::ZERO,
            &calldata,
            GAS,
            true,
            ZONE_OUTBOX_ADDRESS,
            ZONE_OUTBOX_ADDRESS,
        )?;
        Ok(IZoneOutbox::getPendingWithdrawalsCall::abi_decode_returns(
            &output.bytes,
        )?)
    }

    fn seed_fallback_recipient(&mut self, nonce: u64, recipient: Address) -> eyre::Result<()> {
        let mut storage = test_storage_provider(&mut self.ctx, u64::MAX, false);
        StorageCtx::enter(&mut storage, || {
            ZoneOutbox::new().seed_fallback_recipient(nonce, recipient)?;
            Ok(())
        })
    }

    fn fallback_recipient(&mut self, nonce: u64) -> eyre::Result<Address> {
        let mut storage = test_storage_provider(&mut self.ctx, u64::MAX, false);
        StorageCtx::enter(&mut storage, || {
            Ok(ZoneOutbox::new().fallback_recipient(nonce)?)
        })
    }

    fn assert_single_bounce_back(
        &mut self,
        token: Address,
        amount: u128,
        recipient: Address,
    ) -> eyre::Result<()> {
        let pending = self.pending_withdrawals()?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].token, token);
        assert_eq!(pending[0].amount, amount);
        assert_eq!(pending[0].to, recipient);
        Ok(())
    }
}

fn failed_encrypted_deposit_gas(deposits: usize) -> eyre::Result<u64> {
    let mut harness = Harness::new()?;
    {
        let mut storage = test_storage_provider(&mut harness.ctx, u64::MAX, false);
        StorageCtx::enter(&mut storage, || {
            TIP20Token::from_address(PATH_USD_ADDRESS)?.change_transfer_policy_id(
                ALICE,
                ITIP20::changeTransferPolicyIdCall {
                    newPolicyId: REJECT_ALL_POLICY_ID,
                },
            )
        })?;
    }

    let fixture = EncryptedDepositFixture::new();
    let decrypted = fixture.decrypt().expect("fixture decrypts");
    let info = crate::ecies::hkdf_info(&PORTAL, &fixture.key_index, &fixture.eph_pub_x);
    let key = crate::ecies::hkdf_sha256(&decrypted.proof.shared_secret.0, b"ecies-aes-key", &info);
    let plaintext = build_plaintext(&BOB, &fixture.memo);
    let (ciphertext, nonce, tag) = encrypt_plaintext(&key, &plaintext);
    let (sequencer_x, sequencer_y_parity) = compressed_x_and_parity(&fixture.seq_pub);
    let base: U256 = keccak256(B256::from(zone_portal_slots::ENCRYPTION_KEYS)).into();
    let slot_x = base + fixture.key_index * U256::from(2);
    harness
        .l1
        .insert(PORTAL, slot_x, 1, U256::from_be_bytes(sequencer_x.0));
    harness.l1.insert(
        PORTAL,
        slot_x + U256::ONE,
        1,
        U256::from(sequencer_y_parity),
    );

    let deposit = EncryptedDeposit {
        token: PATH_USD_ADDRESS,
        sender: ALICE,
        amount: 1,
        tempoRefundRecipient: ALICE,
        keyIndex: fixture.key_index,
        encrypted: tempo_zone_contracts::EncryptedDepositPayload {
            ephemeralPubkeyX: fixture.eph_pub_x,
            ephemeralPubkeyYParity: fixture.eph_pub_y_parity,
            ciphertext: ciphertext.into(),
            nonce: nonce.into(),
            tag: tag.into(),
        },
    };
    let decryption = DecryptionData {
        sharedSecret: decrypted.proof.shared_secret,
        sharedSecretYParity: decrypted.proof.shared_secret_y_parity,
        cpProof: tempo_zone_contracts::ChaumPedersenProof {
            s: decrypted.proof.cp_proof_s,
            c: decrypted.proof.cp_proof_c,
        },
    };

    let mut queued_deposits = Vec::with_capacity(deposits);
    let mut decryptions = Vec::with_capacity(deposits);
    let mut head = B256::ZERO;
    for _ in 0..deposits {
        head = keccak256((DepositType::Encrypted, deposit.clone(), head).abi_encode_params());
        queued_deposits.push(QueuedDeposit {
            depositType: DepositType::Encrypted,
            depositData: deposit.abi_encode().into(),
        });
        decryptions.push(decryption.clone());
    }

    harness.set_queue_hash(head);
    let calldata = harness
        .advance_call(queued_deposits, decryptions)
        .abi_encode();
    let output = harness.call_with_gas(Address::ZERO, calldata, u64::MAX)?;
    assert!(output.is_success(), "deposit block failed: {output:?}");
    Ok(output.gas_used)
}

#[test]
fn max_portal_deposit_block_fits_system_gas_budget() -> eyre::Result<()> {
    const BUFFERED_GAS_LIMIT: u64 = 200_000_000;
    const MAX_DEPOSITS_PER_TEMPO_BLOCK: usize = 230;

    for deposits in [640, MAX_DEPOSITS_PER_TEMPO_BLOCK] {
        let should_fit = deposits <= MAX_DEPOSITS_PER_TEMPO_BLOCK;
        let gas_used = failed_encrypted_deposit_gas(deposits)?;
        eprintln!("{deposits} portal deposit block: {gas_used} gas");
        assert_eq!(
            gas_used <= BUFFERED_GAS_LIMIT,
            should_fit,
            "{deposits} deposit block used {gas_used} gas"
        );
    }
    Ok(())
}

#[test]
fn system_advance_selects_child_anchor_and_reads_queue() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    harness.set_queue_hash(B256::ZERO);

    harness.call(
        Address::ZERO,
        harness.advance_call(Vec::new(), Vec::new()).abi_encode(),
    )?;

    assert_eq!(harness.l1_state.get_anchor(), Some(1));
    assert!(harness.l1.requested(
        1,
        &ZonePortalStorage::new(PORTAL).current_deposit_queue_hash,
    ));
    Ok(())
}

#[test]
fn non_system_advance_reverts_before_selecting_or_reading_l1() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let output = harness.call(
        SEQUENCER,
        harness.advance_call(Vec::new(), Vec::new()).abi_encode(),
    )?;

    assert!(output.is_revert());
    assert_eq!(output.bytes, IZoneInbox::OnlySequencer {}.abi_encode());
    assert_eq!(harness.l1_state.get_anchor(), None);
    assert!(harness.l1.storage_requests().is_empty());
    Ok(())
}

#[test]
fn static_advance_and_delegate_call_revert_before_l1_reads() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let calldata = harness.advance_call(Vec::new(), Vec::new()).abi_encode();
    let output = call_precompile(
        &mut harness.ctx,
        &harness.precompile,
        Address::ZERO,
        &calldata,
        GAS,
        true,
        ZONE_INBOX_ADDRESS,
        ZONE_INBOX_ADDRESS,
    )?;
    assert!(output.is_revert());
    assert!(output.bytes.is_empty());
    assert!(harness.l1.storage_requests().is_empty());

    let output = call_precompile(
        &mut harness.ctx,
        &harness.precompile,
        Address::ZERO,
        &calldata,
        GAS,
        false,
        ZONE_INBOX_ADDRESS,
        Address::repeat_byte(0x44),
    )?;
    assert!(output.is_revert());
    assert_eq!(
        output.bytes,
        tempo_precompiles::DelegateCallNotAllowed {}.abi_encode()
    );
    assert!(harness.l1.storage_requests().is_empty());
    Ok(())
}

#[test]
fn advance_rejects_a_preselected_anchor_before_child_selection() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    harness
        .l1_state
        .read_l1_storage(Address::ZERO, B256::ZERO, 0)?;
    let request_count = harness.l1.storage_requests().len();

    let result = harness.call(
        Address::ZERO,
        harness.advance_call(Vec::new(), Vec::new()).abi_encode(),
    );

    assert!(result.is_err());
    assert_eq!(harness.l1.storage_requests().len(), request_count);
    Ok(())
}

#[test]
fn child_anchor_storage_failure_is_fatal_and_rolls_back_checkpoint() -> eyre::Result<()> {
    let mut harness = Harness::with_l1(MockL1Reader::failing_storage())?;
    let result = harness.call_atomic(
        Address::ZERO,
        harness.advance_call(Vec::new(), Vec::new()).abi_encode(),
    );
    assert!(result.is_err(), "L1 storage failure must remain fatal");

    let mut storage = test_storage_provider(&mut harness.ctx, u64::MAX, false);
    StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
        assert_eq!(TempoState::new().tempo_block_number()?, 0);
        Ok(())
    })?;
    Ok(())
}

#[test]
fn queue_head_mismatch_reverts_and_rolls_back() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let first = Deposit {
        token: PATH_USD_ADDRESS,
        sender: ALICE,
        to: BOB,
        amount: 100,
        tempoRefundRecipient: ALICE,
        memo: B256::repeat_byte(0x11),
    };
    let first_hash =
        keccak256((DepositType::Regular, first.clone(), B256::ZERO).abi_encode_params());
    let first_data = first.abi_encode();
    let second = Deposit {
        amount: 200,
        memo: B256::repeat_byte(0x22),
        ..first
    };
    let tempo_head = keccak256((DepositType::Regular, second, first_hash).abi_encode_params());
    harness.set_queue_hash(tempo_head);

    let output = harness.call_atomic(
        Address::ZERO,
        harness
            .advance_call(
                vec![QueuedDeposit {
                    depositType: DepositType::Regular,
                    depositData: first_data.into(),
                }],
                Vec::new(),
            )
            .abi_encode(),
    )?;
    assert!(output.is_revert());
    assert_eq!(
        output.bytes,
        IZoneInbox::InvalidDepositQueueHash {}.abi_encode()
    );

    assert_eq!(harness.balance(PATH_USD_ADDRESS, BOB)?, U256::ZERO);
    let mut storage = test_storage_provider(&mut harness.ctx, u64::MAX, false);
    StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
        assert_eq!(TempoState::new().tempo_block_number()?, 0);
        assert_eq!(
            ZoneInbox::new().processed_deposit_queue_hash.read()?,
            B256::ZERO
        );
        assert_eq!(ZoneInbox::new().processed_deposit_number.read()?, 0);
        Ok(())
    })?;
    Ok(())
}

#[test]
fn regular_deposit_mints_and_updates_hash_and_number() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let deposit = Deposit {
        token: PATH_USD_ADDRESS,
        sender: ALICE,
        to: BOB,
        amount: 500,
        tempoRefundRecipient: ALICE,
        memo: B256::repeat_byte(0x11),
    };
    let expected_hash =
        keccak256((DepositType::Regular, deposit.clone(), B256::ZERO).abi_encode_params());
    harness.set_queue_hash(expected_hash);
    let queued = QueuedDeposit {
        depositType: DepositType::Regular,
        depositData: deposit.abi_encode().into(),
    };

    harness.call(
        Address::ZERO,
        harness.advance_call(vec![queued], Vec::new()).abi_encode(),
    )?;

    assert_eq!(harness.balance(PATH_USD_ADDRESS, BOB)?, U256::from(500));
    assert!(harness.pending_withdrawals()?.is_empty());
    let mut storage = test_storage_provider(&mut harness.ctx, u64::MAX, false);
    StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
        assert_eq!(
            ZoneInbox::new().processed_deposit_queue_hash.read()?,
            expected_hash
        );
        assert_eq!(ZoneInbox::new().processed_deposit_number.read()?, 1);
        assert_eq!(
            StorageCtx::default().sload(ZONE_INBOX_ADDRESS, U256::ZERO)?,
            U256::from_be_bytes(expected_hash.0)
        );
        assert_eq!(
            StorageCtx::default().sload(ZONE_INBOX_ADDRESS, U256::ONE)?,
            U256::ONE
        );
        Ok(())
    })?;
    Ok(())
}

#[test]
fn receive_policy_blocked_regular_deposit_enqueues_bounce_back() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let mut storage = test_storage_provider(&mut harness.ctx, u64::MAX, false);
    StorageCtx::enter(&mut storage, || {
        TIP403Registry::new().set_receive_policy(
            BOB,
            ITIP403Registry::setReceivePolicyCall {
                senderPolicyId: REJECT_ALL_POLICY_ID,
                tokenFilterId: ALLOW_ALL_POLICY_ID,
                recoveryAuthority: Address::ZERO,
            },
        )
    })?;
    drop(storage);
    let deposit = Deposit {
        token: PATH_USD_ADDRESS,
        sender: ALICE,
        to: BOB,
        amount: 500,
        tempoRefundRecipient: ALICE,
        memo: B256::repeat_byte(0x11),
    };
    let expected_hash =
        keccak256((DepositType::Regular, deposit.clone(), B256::ZERO).abi_encode_params());
    harness.set_queue_hash(expected_hash);

    harness.call(
        Address::ZERO,
        harness
            .advance_call(
                vec![QueuedDeposit {
                    depositType: DepositType::Regular,
                    depositData: deposit.abi_encode().into(),
                }],
                Vec::new(),
            )
            .abi_encode(),
    )?;

    assert_eq!(harness.balance(PATH_USD_ADDRESS, BOB)?, U256::ZERO);
    assert_eq!(
        harness.balance(PATH_USD_ADDRESS, RECEIVE_POLICY_GUARD_ADDRESS)?,
        U256::ZERO
    );
    harness.assert_single_bounce_back(PATH_USD_ADDRESS, 500, ALICE)?;
    Ok(())
}

#[test]
fn failed_regular_mint_enqueues_one_bounce_back() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let uninitialized_token = address!("0x20c0000000000000000000000000000000000099");
    let deposit = Deposit {
        token: uninitialized_token,
        sender: ALICE,
        to: BOB,
        amount: 222,
        tempoRefundRecipient: ALICE,
        memo: B256::ZERO,
    };
    let expected_hash =
        keccak256((DepositType::Regular, deposit.clone(), B256::ZERO).abi_encode_params());
    harness.set_queue_hash(expected_hash);

    harness.call(
        Address::ZERO,
        harness
            .advance_call(
                vec![QueuedDeposit {
                    depositType: DepositType::Regular,
                    depositData: deposit.abi_encode().into(),
                }],
                Vec::new(),
            )
            .abi_encode(),
    )?;

    harness.assert_single_bounce_back(uninitialized_token, 222, ALICE)?;
    Ok(())
}

#[test]
fn enabled_token_is_initialized_before_deposit_processing() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let token = address!("0x20c00000000000000000000000000000000000aa");
    harness.set_queue_hash(B256::ZERO);
    let anchored_policy = U256::from(7) | (U256::ONE << 64);
    let binding_slot = TIP403Registry::new().token_transfer_policies[token].base_slot();
    {
        let mut storage = test_storage_provider(&mut harness.ctx, u64::MAX, false);
        StorageCtx::enter(&mut storage, || {
            StorageCtx.sstore(TIP403_REGISTRY_ADDRESS, binding_slot, anchored_policy)
        })?;
    }
    let mut call = harness.advance_call(Vec::new(), Vec::new());
    call.enabledTokens.push(EnabledToken {
        token,
        name: "Example Dollar".into(),
        symbol: "EXD".into(),
        currency: "USD".into(),
    });

    harness.call(Address::ZERO, call.abi_encode())?;

    let mut storage = test_storage_provider(&mut harness.ctx, u64::MAX, false);
    StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
        let token = TIP20Token::from_address(token)?;
        assert!(token.is_initialized()?);
        assert_eq!(token.name()?, "Example Dollar");
        assert_eq!(token.next_quote_token()?, PATH_USD_ADDRESS);
        assert!(token.has_role_internal(ZONE_INBOX_ADDRESS, *ISSUER_ROLE)?);
        assert!(token.has_role_internal(ZONE_OUTBOX_ADDRESS, *ISSUER_ROLE)?);
        assert_eq!(
            StorageCtx.sload(TIP403_REGISTRY_ADDRESS, binding_slot)?,
            anchored_policy
        );
        Ok(())
    })?;
    Ok(())
}

#[test]
fn malformed_nested_deposit_reverts_before_l1_reads() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let output = harness.call(
        Address::ZERO,
        harness
            .advance_call(
                vec![QueuedDeposit {
                    depositType: DepositType::Regular,
                    depositData: Bytes::from_static(b"malformed"),
                }],
                Vec::new(),
            )
            .abi_encode(),
    )?;

    assert!(output.is_revert());
    assert!(output.bytes.is_empty());
    assert!(harness.l1.storage_requests().is_empty());
    Ok(())
}

#[test]
fn encrypted_deposit_uses_child_anchor_key_and_mints_plaintext_recipient() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let fixture = EncryptedDepositFixture::new();
    let decrypted = fixture.decrypt().expect("fixture decrypts");
    let portal = PORTAL;
    let info = crate::ecies::hkdf_info(&portal, &fixture.key_index, &fixture.eph_pub_x);
    let key = crate::ecies::hkdf_sha256(&decrypted.proof.shared_secret.0, b"ecies-aes-key", &info);
    let plaintext = build_plaintext(&fixture.to, &fixture.memo);
    let (ciphertext, nonce, tag) = encrypt_plaintext(&key, &plaintext);
    let (sequencer_x, sequencer_y_parity) = compressed_x_and_parity(&fixture.seq_pub);

    let base: U256 = keccak256(B256::from(zone_portal_slots::ENCRYPTION_KEYS)).into();
    let slot_x = base + fixture.key_index * U256::from(2);
    harness
        .l1
        .insert(portal, slot_x, 1, U256::from_be_bytes(sequencer_x.0));
    harness.l1.insert(
        portal,
        slot_x + U256::ONE,
        1,
        U256::from(sequencer_y_parity),
    );

    let deposit = EncryptedDeposit {
        token: PATH_USD_ADDRESS,
        sender: ALICE,
        amount: 900,
        tempoRefundRecipient: ALICE,
        keyIndex: fixture.key_index,
        encrypted: tempo_zone_contracts::EncryptedDepositPayload {
            ephemeralPubkeyX: fixture.eph_pub_x,
            ephemeralPubkeyYParity: fixture.eph_pub_y_parity,
            ciphertext: ciphertext.into(),
            nonce: nonce.into(),
            tag: tag.into(),
        },
    };
    let expected_hash =
        keccak256((DepositType::Encrypted, deposit.clone(), B256::ZERO).abi_encode_params());
    harness.set_queue_hash(expected_hash);

    harness.call(
        Address::ZERO,
        harness
            .advance_call(
                vec![QueuedDeposit {
                    depositType: DepositType::Encrypted,
                    depositData: deposit.abi_encode().into(),
                }],
                vec![DecryptionData {
                    sharedSecret: decrypted.proof.shared_secret,
                    sharedSecretYParity: decrypted.proof.shared_secret_y_parity,
                    cpProof: tempo_zone_contracts::ChaumPedersenProof {
                        s: decrypted.proof.cp_proof_s,
                        c: decrypted.proof.cp_proof_c,
                    },
                }],
            )
            .abi_encode(),
    )?;

    assert_eq!(
        harness.balance(PATH_USD_ADDRESS, fixture.to)?,
        U256::from(900)
    );
    assert!(harness.pending_withdrawals()?.is_empty());
    assert!(
        harness
            .l1
            .storage_requests()
            .contains(&(portal, B256::from(slot_x.to_be_bytes()), 1))
    );
    Ok(())
}

#[test]
fn invalid_encrypted_proof_bounces_without_mint() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let fixture = EncryptedDepositFixture::new();
    let (sequencer_x, sequencer_y_parity) = compressed_x_and_parity(&fixture.seq_pub);
    let portal = PORTAL;
    let base: U256 = keccak256(B256::from(zone_portal_slots::ENCRYPTION_KEYS)).into();
    let slot_x = base + fixture.key_index * U256::from(2);
    harness
        .l1
        .insert(portal, slot_x, 1, U256::from_be_bytes(sequencer_x.0));
    harness.l1.insert(
        portal,
        slot_x + U256::ONE,
        1,
        U256::from(sequencer_y_parity),
    );
    let deposit = EncryptedDeposit {
        token: PATH_USD_ADDRESS,
        sender: ALICE,
        amount: 333,
        tempoRefundRecipient: BOB,
        keyIndex: fixture.key_index,
        encrypted: tempo_zone_contracts::EncryptedDepositPayload {
            ephemeralPubkeyX: fixture.eph_pub_x,
            ephemeralPubkeyYParity: fixture.eph_pub_y_parity,
            ciphertext: fixture.ciphertext.into(),
            nonce: fixture.nonce.into(),
            tag: fixture.tag.into(),
        },
    };
    let expected_hash =
        keccak256((DepositType::Encrypted, deposit.clone(), B256::ZERO).abi_encode_params());
    harness.set_queue_hash(expected_hash);

    harness.call(
        Address::ZERO,
        harness
            .advance_call(
                vec![QueuedDeposit {
                    depositType: DepositType::Encrypted,
                    depositData: deposit.abi_encode().into(),
                }],
                vec![DecryptionData {
                    sharedSecret: B256::ZERO,
                    sharedSecretYParity: 2,
                    cpProof: tempo_zone_contracts::ChaumPedersenProof {
                        s: B256::ZERO,
                        c: B256::ZERO,
                    },
                }],
            )
            .abi_encode(),
    )?;

    assert_eq!(harness.balance(PATH_USD_ADDRESS, fixture.to)?, U256::ZERO);
    harness.assert_single_bounce_back(PATH_USD_ADDRESS, 333, BOB)?;
    Ok(())
}

#[test]
fn missing_and_extra_decryption_data_revert() -> eyre::Result<()> {
    let fixture = EncryptedDepositFixture::new();
    let deposit = EncryptedDeposit {
        token: PATH_USD_ADDRESS,
        sender: ALICE,
        amount: 1,
        tempoRefundRecipient: BOB,
        keyIndex: fixture.key_index,
        encrypted: tempo_zone_contracts::EncryptedDepositPayload {
            ephemeralPubkeyX: fixture.eph_pub_x,
            ephemeralPubkeyYParity: fixture.eph_pub_y_parity,
            ciphertext: fixture.ciphertext.into(),
            nonce: fixture.nonce.into(),
            tag: fixture.tag.into(),
        },
    };
    let expected_hash =
        keccak256((DepositType::Encrypted, deposit.clone(), B256::ZERO).abi_encode_params());
    let mut missing = Harness::new()?;
    missing.set_queue_hash(expected_hash);
    let output = missing.call_atomic(
        Address::ZERO,
        missing
            .advance_call(
                vec![QueuedDeposit {
                    depositType: DepositType::Encrypted,
                    depositData: deposit.abi_encode().into(),
                }],
                Vec::new(),
            )
            .abi_encode(),
    )?;
    assert!(output.is_revert());
    assert_eq!(
        output.bytes,
        IZoneInbox::MissingDecryptionData {}.abi_encode()
    );

    let mut extra = Harness::new()?;
    extra.set_queue_hash(B256::ZERO);
    let output = extra.call_atomic(
        Address::ZERO,
        extra
            .advance_call(
                Vec::new(),
                vec![DecryptionData {
                    sharedSecret: B256::ZERO,
                    sharedSecretYParity: 2,
                    cpProof: tempo_zone_contracts::ChaumPedersenProof {
                        s: B256::ZERO,
                        c: B256::ZERO,
                    },
                }],
            )
            .abi_encode(),
    )?;
    assert!(output.is_revert());
    assert_eq!(
        output.bytes,
        IZoneInbox::ExtraDecryptionData {}.abi_encode()
    );
    Ok(())
}

#[test]
fn refund_reads_are_limited_to_owner_and_active_sequencer() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    harness.l1.seed_active_sequencer(PORTAL, 0, SEQUENCER);
    {
        let mut storage = test_storage_provider(&mut harness.ctx, u64::MAX, false);
        StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
            ZoneInbox::new().withdrawal_bounce_backs[PATH_USD_ADDRESS][BOB].write(444)?;
            Ok(())
        })?;
    }

    let calldata = IZoneInbox::refundsCall {
        token: PATH_USD_ADDRESS,
        owner: BOB,
    }
    .abi_encode();

    let owner_output = harness.call(BOB, &calldata)?;
    assert_eq!(
        IZoneInbox::refundsCall::abi_decode_returns(&owner_output.bytes)?,
        444
    );

    let outsider_output = harness.call(ALICE, &calldata)?;
    assert!(outsider_output.is_revert());
    assert_eq!(
        outsider_output.bytes,
        IZoneInbox::Unauthorized {}.abi_encode()
    );

    let sequencer_output = harness.call(SEQUENCER, &calldata)?;
    assert_eq!(
        IZoneInbox::refundsCall::abi_decode_returns(&sequencer_output.bytes)?,
        444
    );
    Ok(())
}

#[test]
fn claim_refund_clears_balance_and_mints_to_caller() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    {
        let mut storage = test_storage_provider(&mut harness.ctx, u64::MAX, false);
        StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
            ZoneInbox::new().withdrawal_bounce_backs[PATH_USD_ADDRESS][BOB].write(444)?;
            Ok(())
        })?;
    }

    let output = harness.call(
        BOB,
        IZoneInbox::claimRefundCall {
            token: PATH_USD_ADDRESS,
        }
        .abi_encode(),
    )?;
    assert_eq!(
        IZoneInbox::claimRefundCall::abi_decode_returns(&output.bytes)?,
        444
    );
    assert_eq!(harness.balance(PATH_USD_ADDRESS, BOB)?, U256::from(444));
    let mut storage = test_storage_provider(&mut harness.ctx, u64::MAX, false);
    StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
        assert_eq!(
            ZoneInbox::new().withdrawal_bounce_backs[PATH_USD_ADDRESS][BOB].read()?,
            0
        );
        Ok(())
    })?;
    Ok(())
}

#[test]
fn failed_withdrawal_bounce_back_parks_refund() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let nonce = 8u64;
    harness.seed_fallback_recipient(nonce, BOB)?;
    let token = address!("0x20c00000000000000000000000000000000000cc");
    let mut encoded_nonce = [0u8; 20];
    encoded_nonce[12..].copy_from_slice(&nonce.to_be_bytes());
    let deposit = Deposit {
        token,
        sender: PORTAL,
        to: Address::from(encoded_nonce),
        amount: 555,
        tempoRefundRecipient: Address::ZERO,
        memo: B256::ZERO,
    };
    let expected_hash =
        keccak256((DepositType::Regular, deposit.clone(), B256::ZERO).abi_encode_params());
    harness.set_queue_hash(expected_hash);

    harness.call(
        Address::ZERO,
        harness
            .advance_call(
                vec![QueuedDeposit {
                    depositType: DepositType::Regular,
                    depositData: deposit.abi_encode().into(),
                }],
                Vec::new(),
            )
            .abi_encode(),
    )?;

    let mut storage = test_storage_provider(&mut harness.ctx, u64::MAX, false);
    StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
        assert_eq!(
            ZoneInbox::new().withdrawal_bounce_backs[token][BOB].read()?,
            555
        );
        Ok(())
    })?;
    drop(storage);
    assert!(harness.pending_withdrawals()?.is_empty());
    Ok(())
}

#[test]
fn withdrawal_bounce_back_consumes_fallback_nonce() -> eyre::Result<()> {
    let mut harness = Harness::new()?;
    let nonce = 7u64;
    harness.seed_fallback_recipient(nonce, BOB)?;
    let mut encoded_nonce = [0u8; 20];
    encoded_nonce[12..].copy_from_slice(&nonce.to_be_bytes());
    let deposit = Deposit {
        token: PATH_USD_ADDRESS,
        sender: PORTAL,
        to: Address::from(encoded_nonce),
        amount: 321,
        tempoRefundRecipient: Address::ZERO,
        memo: B256::ZERO,
    };
    let expected_hash =
        keccak256((DepositType::Regular, deposit.clone(), B256::ZERO).abi_encode_params());
    harness.set_queue_hash(expected_hash);

    harness.call(
        Address::ZERO,
        harness
            .advance_call(
                vec![QueuedDeposit {
                    depositType: DepositType::Regular,
                    depositData: deposit.abi_encode().into(),
                }],
                Vec::new(),
            )
            .abi_encode(),
    )?;

    assert_eq!(harness.balance(PATH_USD_ADDRESS, BOB)?, U256::from(321));
    assert!(harness.pending_withdrawals()?.is_empty());
    assert_eq!(harness.fallback_recipient(nonce)?, Address::ZERO);
    Ok(())
}
