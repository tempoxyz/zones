use super::{barrier::BarrierInclusionProof, *};

use std::collections::{BTreeMap, BTreeSet};

use alloy_consensus::{Header, Sealable, TxReceipt};
use alloy_eips::eip2718::Encodable2718;
use alloy_evm::{EvmInternals, precompiles::DynPrecompile};
use alloy_primitives::{Address, B256, Bytes, Log, LogData, U256, address, keccak256};
use alloy_sol_types::{SolCall, SolValue};
use alloy_trie::{HashBuilder, Nibbles, proof::ProofRetainer};
use k256::ecdsa::SigningKey;
use revm::precompile::PrecompileResult;
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_contracts::precompiles::{FAST_PROOF_MODE_OPERATOR_ATTESTED, FAST_PROTOCOL_NATIVE_PIN};
use tempo_precompiles::{
    storage::{Handler, StorageCtx},
    test_util::TIP20Setup,
    tip20::{ITIP20, TIP20Token},
    tip403_registry::ALLOW_ALL_POLICY_ID,
    zone_factory::{PortalFastEpochConfig, PortalFastPeerBarrier, ZonePortalStorage},
};
use tempo_primitives::{TempoHeader, TempoReceipt, TempoTxType};
use tempo_zone_contracts::{FAST_TRANSFER_ADDRESS, IFastTransfer};
use zone_fast_transfer::{
    EpochRoster, QuorumVerifier,
    drain::{BarrierInventory, CommittedSourceLock},
};
use zone_primitives::{
    constants::ZONE_OUTBOX_ADDRESS,
    fast_transfer::{
        AssetId, CanonicalEncode, CertificateBody, ExposureRetirementEvidence, HeaderAncestryProof,
        OutcomeCertificate, QuoteCertificate, ReceiptInclusionProof, RejectionReason,
        SignatureBytes, StandingQuote, TransferIntent, TransferOutcome, ZoneDomain,
    },
};

use crate::{
    TempoState,
    test_utils::{
        MockL1Reader, TestContext, call_precompile, test_context_with_hardfork, test_env,
        test_storage_provider,
    },
};

const GAS: u64 = 30_000_000;
const ANCHOR: u64 = 14;
const L1_CHAIN_ID: u64 = 4242;
const PROTOCOL_VERSION: u16 = 1;
const EPOCH: u64 = 14;
const VALID_DESTINATION_EXPIRY_HEIGHT: u64 = ANCHOR + 100;

const SOURCE_PORTAL: Address = address!("0x0000000000000000000000000000000000000a01");
const DESTINATION_PORTAL: Address = address!("0x0000000000000000000000000000000000000b01");
const TOKEN: Address = tempo_precompiles::PATH_USD_ADDRESS;
const SENDER: Address = address!("0x00000000000000000000000000000000000000a1");
const RECIPIENT: Address = address!("0x00000000000000000000000000000000000000b2");
const NEXT_RECIPIENT: Address = address!("0x00000000000000000000000000000000000000c3");
const POOL_OPERATOR: Address = address!("0x00000000000000000000000000000000000000d4");
const REIMBURSEMENT: Address = address!("0x00000000000000000000000000000000000000e5");

fn verifier_code_hash() -> B256 {
    keccak256("t14 acceptance verifier code")
}

fn verifier_config_hash() -> B256 {
    keccak256("t14 acceptance verifier config")
}

fn peer_portals(portal: Address) -> [Address; 9] {
    let mut candidates = vec![SOURCE_PORTAL, DESTINATION_PORTAL];
    candidates.extend((2u8..=9).map(Address::repeat_byte));
    candidates
        .into_iter()
        .filter(|candidate| *candidate != portal)
        .collect::<Vec<_>>()
        .try_into()
        .expect("the ten-Zone fixture has exactly nine remote portals")
}

#[derive(Clone)]
struct Authority {
    domain: ZoneDomain,
    keys: [SigningKey; 3],
    verifier: QuorumVerifier,
    peers: [Address; 9],
}

impl Authority {
    fn new(zone_id: u32, chain_id: u64, portal: Address, key_offset: u8) -> Self {
        assert_eq!(FAST_PROTOCOL_NATIVE_PIN, EXPECTED_FAST_PROTOCOL_NATIVE_PIN);
        let keys = [
            signing_key(key_offset),
            signing_key(key_offset + 1),
            signing_key(key_offset + 2),
        ];
        let members = keys.each_ref().map(member_address);
        let peers = peer_portals(portal);
        let mut domain = ZoneDomain {
            l1_chain_id: L1_CHAIN_ID,
            zone_id,
            chain_id,
            portal,
            authority_epoch: EPOCH,
            roster_hash: B256::ZERO,
            protocol_version: PROTOCOL_VERSION,
        };
        domain.roster_hash = keccak256(
            (
                keccak256("TEMPO_ZONE_FAST_ROSTER_T14_V1"),
                portal,
                EPOCH,
                u32::from(PROTOCOL_VERSION),
                U256::from(2),
                U256::from(FAST_PROOF_MODE_OPERATOR_ATTESTED),
                verifier_code_hash(),
                verifier_config_hash(),
                members.to_vec(),
                peers.to_vec(),
            )
                .abi_encode(),
        );
        let roster = EpochRoster::from_finalized_registry(domain, members)
            .expect("valid deterministic published-Tempo roster");
        Self {
            domain,
            keys,
            verifier: QuorumVerifier::new(roster),
            peers,
        }
    }

    fn sign_quote(&self, quote: StandingQuote) -> QuoteCertificate {
        let mut certificate = QuoteCertificate {
            quote,
            signatures: [SignatureBytes([0; 65]); 2],
        };
        let digest = self.verifier.quote_digest(&certificate);
        certificate.signatures = [sign(&self.keys[0], digest), sign(&self.keys[1], digest)];
        certificate
    }

    fn sign_outcome(&self, body: CertificateBody) -> OutcomeCertificate {
        let mut certificate = OutcomeCertificate {
            body,
            signatures: [SignatureBytes([0; 65]); 2],
        };
        let digest = self.verifier.outcome_digest(&certificate);
        certificate.signatures = [sign(&self.keys[0], digest), sign(&self.keys[1], digest)];
        certificate
    }
}

struct Harness {
    ctx: Box<TestContext>,
    l1: MockL1Reader,
    portal: Address,
    precompile: DynPrecompile,
}

impl Harness {
    fn new(
        hardfork: TempoHardfork,
        portal: Address,
        source: &Authority,
        destination: &Authority,
    ) -> eyre::Result<Self> {
        let mut ctx = Box::new(test_context_with_hardfork(hardfork));
        let l1 = MockL1Reader::default();
        seed_epoch(&l1, source);
        seed_epoch(&l1, destination);
        seed_enabled_asset(&l1, source.domain.portal, TOKEN);
        seed_enabled_asset(&l1, destination.domain.portal, TOKEN);
        seed_access(
            &l1,
            portal,
            true,
            &[
                SENDER,
                RECIPIENT,
                NEXT_RECIPIENT,
                POOL_OPERATOR,
                REIMBURSEMENT,
            ],
        );

        {
            let mut storage = test_storage_provider(&mut ctx, u64::MAX, false);
            StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
                TempoState::new().tempo_block_number.write(ANCHOR)?;
                FastTransfer::new().initialize()?;
                let token = TIP20Setup::path_usd(SENDER)
                    .with_issuer(SENDER)
                    .with_issuer(POOL_OPERATOR)
                    .with_issuer(ZONE_OUTBOX_ADDRESS)
                    .with_mint(SENDER, U256::from(1_000u64))
                    .with_mint(POOL_OPERATOR, U256::from(1_000u64))
                    .with_approval(SENDER, FAST_TRANSFER_ADDRESS, U256::MAX)
                    .with_approval(POOL_OPERATOR, FAST_TRANSFER_ADDRESS, U256::MAX)
                    .apply()?;
                assert_eq!(
                    token.decimals()?,
                    6,
                    "fixture asset metadata must match the route"
                );
                assert_eq!(
                    token.transfer_policy_id()?,
                    ALLOW_ALL_POLICY_ID,
                    "positive fixtures use an explicitly initialized TIP-403 policy"
                );
                Ok(())
            })?;
        }

        let env = test_env(&ctx);
        let precompile = FastTransfer::create(L1State::new(l1.clone(), portal), &env);
        Ok(Self {
            ctx,
            l1,
            portal,
            precompile,
        })
    }

    fn call(&mut self, caller: Address, call: impl SolCall) -> PrecompileResult {
        self.call_raw(caller, &call.abi_encode())
    }

    fn call_raw(&mut self, caller: Address, calldata: &[u8]) -> PrecompileResult {
        let checkpoint = EvmInternals::from_context(&mut self.ctx).checkpoint();
        let result = call_precompile(
            &mut self.ctx,
            &self.precompile,
            caller,
            calldata,
            GAS,
            false,
            FAST_TRANSFER_ADDRESS,
            FAST_TRANSFER_ADDRESS,
        );
        let success = result.as_ref().is_ok_and(|output| output.is_success());
        let mut internals = EvmInternals::from_context(&mut self.ctx);
        if success {
            internals.checkpoint_commit();
        } else {
            internals.checkpoint_revert(checkpoint);
        }
        result
    }

    fn balance(&mut self, account: Address) -> eyre::Result<U256> {
        let mut storage = test_storage_provider(&mut self.ctx, u64::MAX, false);
        StorageCtx::enter(&mut storage, || {
            Ok(TIP20Token::from_address(TOKEN)?.balance_of(ITIP20::balanceOfCall { account })?)
        })
    }

    fn supply(&mut self) -> eyre::Result<U256> {
        let mut storage = test_storage_provider(&mut self.ctx, u64::MAX, false);
        StorageCtx::enter(&mut storage, || {
            Ok(TIP20Token::from_address(TOKEN)?.total_supply()?)
        })
    }

    fn fund_pool(&mut self, amount: u128, reserve: u128) -> PrecompileResult {
        self.call(
            POOL_OPERATOR,
            IFastTransfer::fundPoolCall {
                token: TOKEN,
                amount,
                minimumReserve: reserve,
            },
        )
    }

    fn lock(&mut self, intent: &TransferIntent, quote: &QuoteCertificate) -> PrecompileResult {
        self.call(
            SENDER,
            IFastTransfer::lockCall {
                canonicalIntent: intent.canonical_bytes().into(),
                quoteCertificate: quote.canonical_bytes().into(),
            },
        )
    }

    fn resolve(&mut self, intent: &TransferIntent, lock: &OutcomeCertificate) -> PrecompileResult {
        self.resolve_with_barrier(intent, lock, Bytes::new())
    }

    fn resolve_with_barrier(
        &mut self,
        intent: &TransferIntent,
        lock: &OutcomeCertificate,
        barrier_proof: Bytes,
    ) -> PrecompileResult {
        self.call(
            POOL_OPERATOR,
            IFastTransfer::resolveCall {
                canonicalIntent: intent.canonical_bytes().into(),
                lockCertificate: lock.canonical_bytes().into(),
                cancellation: Bytes::new(),
                barrierProof: barrier_proof,
            },
        )
    }

    fn record_outcome(
        &mut self,
        intent: &TransferIntent,
        outcome: &OutcomeCertificate,
    ) -> PrecompileResult {
        self.call(
            REIMBURSEMENT,
            IFastTransfer::recordOutcomeCall {
                canonicalIntent: intent.canonical_bytes().into(),
                outcomeCertificate: outcome.canonical_bytes().into(),
            },
        )
    }

    fn dispose(&mut self, transfer_id: B256) -> PrecompileResult {
        self.call(
            REIMBURSEMENT,
            IFastTransfer::disposeEscrowCall {
                transferId: transfer_id,
            },
        )
    }

    fn status(&mut self, transfer_id: B256) -> eyre::Result<FastTransferStatus> {
        let output = self.call(
            Address::ZERO,
            IFastTransfer::statusCall {
                transferId: transfer_id,
            },
        )?;
        Ok(IFastTransfer::statusCall::abi_decode_returns(
            &output.bytes,
        )?)
    }

    fn pool_state(&mut self) -> eyre::Result<PoolState> {
        let output = self.call(Address::ZERO, IFastTransfer::poolStateCall { token: TOKEN })?;
        Ok(IFastTransfer::poolStateCall::abi_decode_returns(
            &output.bytes,
        )?)
    }

    fn exposure(&mut self, source_zone: B256) -> eyre::Result<(u128, u128)> {
        let output = self.call(
            Address::ZERO,
            IFastTransfer::exposureCall {
                token: TOKEN,
                sourceZone: source_zone,
            },
        )?;
        let decoded = IFastTransfer::exposureCall::abi_decode_returns(&output.bytes)?;
        Ok((decoded.unsettled, decoded.limit))
    }

    fn set_exposure_limit(&mut self, source_zone: B256, limit: u128) -> PrecompileResult {
        self.call(
            POOL_OPERATOR,
            IFastTransfer::setExposureLimitCall {
                token: TOKEN,
                sourceZone: source_zone,
                limit,
            },
        )
    }

    fn withdraw_pool(&mut self, amount: u128) -> PrecompileResult {
        self.call(
            POOL_OPERATOR,
            IFastTransfer::withdrawPoolCall {
                token: TOKEN,
                recipient: POOL_OPERATOR,
                amount,
            },
        )
    }

    fn ordinary_transfer(&mut self, from: Address, to: Address, amount: u64) -> PrecompileResult {
        let env = test_env(&self.ctx);
        let precompile =
            crate::create_tip20_precompile(TOKEN, &env, L1State::new(self.l1.clone(), self.portal));
        call_precompile(
            &mut self.ctx,
            &precompile,
            from,
            &ITIP20::transferCall {
                to,
                amount: U256::from(amount),
            }
            .abi_encode(),
            GAS,
            false,
            TOKEN,
            TOKEN,
        )
    }

    fn retire_exposure(&mut self, evidence: &ExposureRetirementEvidence) -> PrecompileResult {
        self.call(
            POOL_OPERATOR,
            IFastTransfer::retireExposureCall {
                canonicalEvidence: evidence.canonical_bytes().into(),
            },
        )
    }
}

struct Scenario {
    source_authority: Authority,
    destination_authority: Authority,
    source: Harness,
    destination: Harness,
}

impl Scenario {
    fn new() -> eyre::Result<Self> {
        let source_authority = Authority::new(1, 10_001, SOURCE_PORTAL, 1);
        let destination_authority = Authority::new(2, 10_002, DESTINATION_PORTAL, 11);
        let source = Harness::new(
            TempoHardfork::T14,
            SOURCE_PORTAL,
            &source_authority,
            &destination_authority,
        )?;
        let destination = Harness::new(
            TempoHardfork::T14,
            DESTINATION_PORTAL,
            &source_authority,
            &destination_authority,
        )?;
        Ok(Self {
            source_authority,
            destination_authority,
            source,
            destination,
        })
    }

    fn intent(&self, nonce: u64, principal: u64, fee: u64) -> TransferIntent {
        TransferIntent {
            source: self.source_authority.domain,
            destination: self.destination_authority.domain,
            asset: AssetId {
                l1_token: TOKEN,
                source_token: TOKEN,
                destination_token: TOKEN,
                decimals: 6,
            },
            sender: SENDER,
            recipient: RECIPIENT,
            refund_account: SENDER,
            destination_pool: POOL_OPERATOR,
            reimbursement_account: REIMBURSEMENT,
            principal: U256::from(principal),
            fee: U256::from(fee),
            quote_id: keccak256(nonce.to_be_bytes()),
            destination_expiry_height: VALID_DESTINATION_EXPIRY_HEIGHT,
            transfer_nonce: nonce,
        }
    }

    fn quote(&self, intent: &TransferIntent) -> QuoteCertificate {
        self.destination_authority.sign_quote(StandingQuote {
            source: intent.source,
            destination: intent.destination,
            asset: intent.asset,
            quote_id: intent.quote_id,
            fee: intent.fee,
            maximum_principal: intent.principal,
            expiry_height: intent.destination_expiry_height,
            destination_pool: intent.destination_pool,
            reimbursement_account: intent.reimbursement_account,
        })
    }

    fn locked(&self, intent: &TransferIntent) -> OutcomeCertificate {
        self.source_authority.sign_outcome(outcome_body(
            intent,
            intent.source,
            TransferOutcome::Locked {
                escrow: FAST_TRANSFER_ADDRESS,
                amount: intent.principal + intent.fee,
            },
        ))
    }

    fn paid(&self, intent: &TransferIntent) -> OutcomeCertificate {
        self.destination_authority.sign_outcome(outcome_body(
            intent,
            intent.destination,
            TransferOutcome::Paid {
                pool: POOL_OPERATOR,
                recipient: RECIPIENT,
                principal: intent.principal,
            },
        ))
    }

    fn rejected(&self, intent: &TransferIntent) -> OutcomeCertificate {
        self.destination_authority.sign_outcome(outcome_body(
            intent,
            intent.destination,
            TransferOutcome::Rejected {
                reason: RejectionReason::InsufficientLiquidity,
            },
        ))
    }
}

fn install_closed_destination_barrier(
    scenario: &Scenario,
    intent: &TransferIntent,
    lock: &OutcomeCertificate,
    retired: bool,
) -> Bytes {
    let closure_hash = keccak256("finalized destination closure");
    let inventory = BarrierInventory::build(
        L1_CHAIN_ID,
        DESTINATION_PORTAL,
        EPOCH,
        closure_hash,
        SOURCE_PORTAL,
        EPOCH,
        ANCHOR,
        keccak256("imported closure anchor"),
        lock.body.log_term,
        lock.body.log_index,
        lock.body.block_height,
        lock.body.block_hash,
        lock.body.state_root,
        vec![CommittedSourceLock {
            intent: intent.clone(),
            lock: lock.clone(),
        }],
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeSet::new(),
    )
    .expect("canonical source barrier inventory");
    let statement = inventory.statement;
    let barrier_hash = statement.registry_digest(L1_CHAIN_ID);
    scenario
        .destination
        .l1
        .with_storage(ANCHOR, || {
            let mut portal = ZonePortalStorage::new(DESTINATION_PORTAL);
            let mut config = portal.fast_epochs[EPOCH].read()?;
            config.closed = true;
            config.retired = retired;
            config.closure_hash = closure_hash;
            config.recorded_peer_barriers = 1;
            portal.fast_epochs[EPOCH].write(config)?;
            portal.fast_peer_barriers[EPOCH][SOURCE_PORTAL].write(PortalFastPeerBarrier {
                recorded: true,
                finalized: false,
                source_epoch: statement.source_epoch,
                imported_anchor_number: statement.imported_anchor_number,
                imported_anchor_hash: statement.imported_anchor_hash,
                log_term: statement.log_term,
                log_index: statement.log_index,
                block_height: statement.block_height,
                block_hash: statement.block_hash,
                state_root: statement.state_root,
                lock_log_watermark: statement.lock_log_watermark,
                complete_lock_root: statement.complete_lock_root,
                unresolved_root: statement.unresolved_root,
                unresolved_count: statement.unresolved_count,
                barrier_hash,
                terminal_root: B256::ZERO,
                disposition_root: B256::ZERO,
                resolved_count: 0,
                remaining_unresolved_root: B256::ZERO,
                remaining_unresolved_count: 0,
                resolution_hash: B256::ZERO,
            })
        })
        .expect("seed typed anchored source barrier");

    let complete_proof = &inventory.locks[0].complete_proof;
    BarrierInclusionProof {
        destination_portal: statement.destination_portal,
        destination_epoch: statement.destination_epoch,
        closure_hash: statement.closure_hash,
        source_portal: statement.source_portal,
        source_epoch: statement.source_epoch,
        imported_anchor_number: statement.imported_anchor_number,
        imported_anchor_hash: statement.imported_anchor_hash,
        barrier_hash,
        lock_log_watermark: statement.lock_log_watermark,
        complete_lock_root: statement.complete_lock_root,
        leaf_index: complete_proof.leaf_index,
        leaf_count: complete_proof.leaf_count,
        siblings: complete_proof.siblings.clone(),
    }
    .encode()
    .into()
}

fn closed_epoch_resolve_requires_exact_anchored_barrier_proof_impl() -> eyre::Result<()> {
    let mut scenario = Scenario::new()?;
    let intent = scenario.intent(70, 25, 0);
    let lock = scenario.locked(&intent);
    assert_success(scenario.destination.fund_pool(100, 0));
    assert_success(
        scenario
            .destination
            .set_exposure_limit(intent.source.domain_hash(), 25),
    );
    let proof = install_closed_destination_barrier(&scenario, &intent, &lock, false);

    assert_revert(scenario.destination.resolve(&intent, &lock));
    let mut mutated = proof.to_vec();
    *mutated.last_mut().expect("proof is nonempty") ^= 1;
    assert_revert(
        scenario
            .destination
            .resolve_with_barrier(&intent, &lock, mutated.into()),
    );
    assert_eq!(scenario.destination.balance(RECIPIENT)?, U256::ZERO);
    assert_eq!(scenario.destination.pool_state()?.fundedBalance, 100);

    assert_success(
        scenario
            .destination
            .resolve_with_barrier(&intent, &lock, proof),
    );
    assert_eq!(scenario.destination.balance(RECIPIENT)?, U256::from(25));
    assert_eq!(scenario.destination.pool_state()?.fundedBalance, 75);

    let mut retired = Scenario::new()?;
    let retired_intent = retired.intent(71, 25, 0);
    let retired_lock = retired.locked(&retired_intent);
    assert_success(retired.destination.fund_pool(100, 0));
    assert_success(
        retired
            .destination
            .set_exposure_limit(retired_intent.source.domain_hash(), 25),
    );
    let retired_proof =
        install_closed_destination_barrier(&retired, &retired_intent, &retired_lock, true);
    assert_revert(retired.destination.resolve_with_barrier(
        &retired_intent,
        &retired_lock,
        retired_proof,
    ));
    assert_eq!(retired.destination.balance(RECIPIENT)?, U256::ZERO);
    assert_eq!(retired.destination.pool_state()?.fundedBalance, 100);
    Ok(())
}

fn published_tempo_config_fields_are_consensus_pinned_impl() -> eyre::Result<()> {
    let mut scenario = Scenario::new()?;
    let intent = scenario.intent(72, 25, 0);
    let lock = scenario.locked(&intent);
    assert_success(scenario.destination.fund_pool(100, 0));
    assert_success(
        scenario
            .destination
            .set_exposure_limit(intent.source.domain_hash(), 25),
    );

    for mutation in 0..4 {
        scenario
            .destination
            .l1
            .with_storage(ANCHOR, || {
                let mut portal = ZonePortalStorage::new(DESTINATION_PORTAL);
                let mut config = portal.fast_epochs[EPOCH].read()?;
                match mutation {
                    0 => config.proof_mode = 0,
                    1 => config.expected_verifier_code_hash = keccak256("wrong verifier code"),
                    2 => config.expected_verifier_config_hash = keccak256("wrong verifier config"),
                    3 => config.roster_hash = keccak256("wrong registry roster"),
                    _ => unreachable!(),
                }
                portal.fast_epochs[EPOCH].write(config)
            })
            .expect("mutate one anchored registry field");
        assert_revert(scenario.destination.resolve(&intent, &lock));
        assert_eq!(scenario.destination.balance(RECIPIENT)?, U256::ZERO);
        assert_eq!(scenario.destination.pool_state()?.fundedBalance, 100);
        seed_epoch(&scenario.destination.l1, &scenario.destination_authority);
    }

    assert_success(scenario.destination.resolve(&intent, &lock));
    assert_eq!(scenario.destination.balance(RECIPIENT)?, U256::from(25));
    assert_eq!(scenario.destination.pool_state()?.fundedBalance, 75);
    Ok(())
}

fn actual_escrow_payment_release_and_t14_spend_preserve_supply_impl() -> eyre::Result<()> {
    let mut scenario = Scenario::new()?;
    let intent = scenario.intent(1, 100, 7);
    let quote = scenario.quote(&intent);
    let source_supply = scenario.source.supply()?;
    let destination_supply = scenario.destination.supply()?;

    assert_success(scenario.destination.fund_pool(500, 50));
    assert_success(
        scenario
            .destination
            .set_exposure_limit(intent.source.domain_hash(), u128::MAX),
    );
    assert_success(scenario.source.lock(&intent, &quote));
    assert_eq!(scenario.source.balance(SENDER)?, U256::from(893u64));
    assert_eq!(
        scenario.source.balance(FAST_TRANSFER_ADDRESS)?,
        U256::from(107u64)
    );

    assert_success(
        scenario
            .destination
            .resolve(&intent, &scenario.locked(&intent)),
    );
    assert_eq!(scenario.destination.balance(RECIPIENT)?, U256::from(100u64));
    assert_eq!(scenario.destination.pool_state()?.fundedBalance, 400);
    assert_eq!(
        scenario.destination.balance(FAST_TRANSFER_ADDRESS)?,
        U256::from(400u64)
    );
    assert_eq!(
        scenario.destination.exposure(intent.source.domain_hash())?,
        (100, u128::MAX)
    );

    assert_success(
        scenario
            .destination
            .ordinary_transfer(RECIPIENT, NEXT_RECIPIENT, 40),
    );
    assert_eq!(scenario.destination.balance(RECIPIENT)?, U256::from(60u64));
    assert_eq!(
        scenario.destination.balance(NEXT_RECIPIENT)?,
        U256::from(40u64)
    );

    assert_success(
        scenario
            .source
            .record_outcome(&intent, &scenario.paid(&intent)),
    );
    assert_success(scenario.source.dispose(intent.transfer_id()));
    assert_eq!(scenario.source.balance(REIMBURSEMENT)?, U256::from(107u64));
    assert_eq!(scenario.source.balance(FAST_TRANSFER_ADDRESS)?, U256::ZERO);
    assert_eq!(scenario.source.supply()?, source_supply);
    assert_eq!(scenario.destination.supply()?, destination_supply);
    Ok(())
}

fn rejection_refund_and_source_business_nonce_retries_have_literal_deltas_impl() -> eyre::Result<()>
{
    let mut scenario = Scenario::new()?;
    let intent = scenario.intent(9, 80, 5);
    let quote = scenario.quote(&intent);
    let supply = scenario.source.supply()?;

    assert_success(scenario.source.lock(&intent, &quote));
    assert_success(scenario.source.lock(&intent, &quote));
    assert_eq!(scenario.source.balance(SENDER)?, U256::from(915u64));
    assert_eq!(
        scenario.source.balance(FAST_TRANSFER_ADDRESS)?,
        U256::from(85u64)
    );

    let mut changed = intent.clone();
    changed.recipient = NEXT_RECIPIENT;
    assert_revert(scenario.source.lock(&changed, &scenario.quote(&changed)));
    assert_eq!(scenario.source.balance(SENDER)?, U256::from(915u64));
    assert_eq!(
        scenario.source.balance(FAST_TRANSFER_ADDRESS)?,
        U256::from(85u64)
    );

    assert_success(
        scenario
            .source
            .record_outcome(&intent, &scenario.rejected(&intent)),
    );
    assert_success(scenario.source.dispose(intent.transfer_id()));
    assert_success(scenario.source.dispose(intent.transfer_id()));
    assert_eq!(scenario.source.balance(SENDER)?, U256::from(1_000u64));
    assert_eq!(scenario.source.balance(REIMBURSEMENT)?, U256::ZERO);
    assert_eq!(scenario.source.balance(FAST_TRANSFER_ADDRESS)?, U256::ZERO);
    assert_eq!(scenario.source.supply()?, supply);
    Ok(())
}

fn closed_account_policy_blocks_liability_without_changing_beneficiary_impl() -> eyre::Result<()> {
    let mut scenario = Scenario::new()?;
    let intent = scenario.intent(3, 50, 2);
    let quote = scenario.quote(&intent);
    assert_success(scenario.source.lock(&intent, &quote));
    assert_success(
        scenario
            .source
            .record_outcome(&intent, &scenario.paid(&intent)),
    );

    seed_access(&scenario.source.l1, SOURCE_PORTAL, true, &[SENDER]);
    assert_revert(scenario.source.dispose(intent.transfer_id()));
    let pending = scenario.source.status(intent.transfer_id())?;
    assert_eq!(pending.state, STATE_PAID_AWAITING_RELEASE);
    assert_eq!(pending.beneficiary, REIMBURSEMENT);
    assert_eq!(
        scenario.source.balance(FAST_TRANSFER_ADDRESS)?,
        U256::from(52u64)
    );
    assert_eq!(scenario.source.balance(SENDER)?, U256::from(948u64));

    seed_access(
        &scenario.source.l1,
        SOURCE_PORTAL,
        true,
        &[SENDER, REIMBURSEMENT],
    );
    assert_success(scenario.source.dispose(intent.transfer_id()));
    assert_eq!(scenario.source.balance(REIMBURSEMENT)?, U256::from(52u64));
    assert_eq!(scenario.source.balance(SENDER)?, U256::from(948u64));

    let mut refund = Scenario::new()?;
    let rejected_intent = refund.intent(4, 30, 1);
    assert_success(
        refund
            .source
            .lock(&rejected_intent, &refund.quote(&rejected_intent)),
    );
    assert_success(
        refund
            .source
            .record_outcome(&rejected_intent, &refund.rejected(&rejected_intent)),
    );
    seed_access(&refund.source.l1, SOURCE_PORTAL, true, &[REIMBURSEMENT]);
    assert_revert(refund.source.dispose(rejected_intent.transfer_id()));
    let pending_refund = refund.source.status(rejected_intent.transfer_id())?;
    assert_eq!(pending_refund.state, STATE_REJECTED_AWAITING_REFUND);
    assert_eq!(pending_refund.beneficiary, SENDER);
    assert_eq!(refund.source.balance(SENDER)?, U256::from(969u64));
    assert_eq!(
        refund.source.balance(FAST_TRANSFER_ADDRESS)?,
        U256::from(31u64)
    );
    seed_access(
        &refund.source.l1,
        SOURCE_PORTAL,
        true,
        &[SENDER, REIMBURSEMENT],
    );
    assert_success(refund.source.dispose(rejected_intent.transfer_id()));
    assert_eq!(refund.source.balance(SENDER)?, U256::from(1_000u64));
    assert_eq!(refund.source.balance(FAST_TRANSFER_ADDRESS)?, U256::ZERO);
    Ok(())
}

fn closed_destination_account_roles_fail_closed_without_pool_debit_impl() -> eyre::Result<()> {
    let mut scenario = Scenario::new()?;
    let intent = scenario.intent(5, 35, 0);
    assert_success(scenario.destination.fund_pool(100, 0));
    assert_success(
        scenario
            .destination
            .set_exposure_limit(intent.source.domain_hash(), 100),
    );
    seed_access(
        &scenario.destination.l1,
        DESTINATION_PORTAL,
        true,
        &[POOL_OPERATOR],
    );

    assert_success(
        scenario
            .destination
            .resolve(&intent, &scenario.locked(&intent)),
    );
    assert_eq!(
        scenario.destination.status(intent.transfer_id())?.state,
        STATE_REJECTED
    );
    assert_eq!(scenario.destination.balance(RECIPIENT)?, U256::ZERO);
    assert_eq!(
        scenario.destination.balance(FAST_TRANSFER_ADDRESS)?,
        U256::from(100u64)
    );
    assert_eq!(scenario.destination.pool_state()?.fundedBalance, 100);

    seed_access(
        &scenario.destination.l1,
        DESTINATION_PORTAL,
        true,
        &[POOL_OPERATOR, RECIPIENT],
    );
    assert_success(
        scenario
            .destination
            .resolve(&intent, &scenario.locked(&intent)),
    );
    assert_eq!(
        scenario.destination.status(intent.transfer_id())?.state,
        STATE_REJECTED
    );
    assert_eq!(scenario.destination.balance(RECIPIENT)?, U256::ZERO);
    Ok(())
}

fn pool_liquidity_exposure_and_operator_withdrawal_are_serialized_impl() -> eyre::Result<()> {
    let mut scenario = Scenario::new()?;
    let first = scenario.intent(20, 10, 0);
    let second = scenario.intent(21, 10, 0);
    assert_success(scenario.destination.fund_pool(10, 0));
    assert_success(
        scenario
            .destination
            .set_exposure_limit(first.source.domain_hash(), 10),
    );

    assert_success(
        scenario
            .destination
            .resolve(&first, &scenario.locked(&first)),
    );
    assert_success(
        scenario
            .destination
            .resolve(&second, &scenario.locked(&second)),
    );
    assert_revert(scenario.destination.withdraw_pool(1));
    assert_eq!(scenario.destination.balance(RECIPIENT)?, U256::from(10u64));
    assert_eq!(scenario.destination.pool_state()?.fundedBalance, 0);
    assert_eq!(
        scenario.destination.exposure(first.source.domain_hash())?,
        (10, 10)
    );
    assert_eq!(
        scenario.destination.status(first.transfer_id())?.state,
        STATE_PAID
    );
    assert_eq!(
        scenario.destination.status(second.transfer_id())?.state,
        STATE_REJECTED
    );

    let mut withdrawal_first = Scenario::new()?;
    let intent = withdrawal_first.intent(22, 10, 0);
    assert_success(withdrawal_first.destination.fund_pool(10, 0));
    assert_success(
        withdrawal_first
            .destination
            .set_exposure_limit(intent.source.domain_hash(), 10),
    );
    assert_success(withdrawal_first.destination.withdraw_pool(10));
    assert_success(
        withdrawal_first
            .destination
            .resolve(&intent, &withdrawal_first.locked(&intent)),
    );
    assert_eq!(withdrawal_first.destination.balance(RECIPIENT)?, U256::ZERO);
    assert_eq!(
        withdrawal_first
            .destination
            .status(intent.transfer_id())?
            .state,
        STATE_REJECTED
    );
    assert_eq!(withdrawal_first.destination.pool_state()?.fundedBalance, 0);
    Ok(())
}

fn pre_t14_dispatch_is_gated_with_zero_token_effects_impl() -> eyre::Result<()> {
    let source_authority = Authority::new(1, 10_001, SOURCE_PORTAL, 1);
    let destination_authority = Authority::new(2, 10_002, DESTINATION_PORTAL, 11);
    let mut harness = Harness::new(
        TempoHardfork::T13,
        DESTINATION_PORTAL,
        &source_authority,
        &destination_authority,
    )?;
    let supply = harness.supply()?;
    for selector in IFastTransfer::IFastTransferCalls::SELECTORS {
        assert_revert(harness.call_raw(POOL_OPERATOR, selector));
        assert_eq!(harness.balance(POOL_OPERATOR)?, U256::from(1_000u64));
        assert_eq!(harness.balance(FAST_TRANSFER_ADDRESS)?, U256::ZERO);
        assert_eq!(harness.supply()?, supply);
    }
    Ok(())
}

fn every_signed_outcome_field_mutation_has_zero_token_effects_impl() -> eyre::Result<()> {
    let mut scenario = Scenario::new()?;
    let intent = scenario.intent(30, 25, 1);
    assert_success(scenario.destination.fund_pool(100, 0));
    assert_success(
        scenario
            .destination
            .set_exposure_limit(intent.source.domain_hash(), 25),
    );
    let valid = scenario.locked(&intent);
    let destination_before = scenario.destination.balance(RECIPIENT)?;
    let pool_before = scenario.destination.balance(FAST_TRANSFER_ADDRESS)?;
    let supply_before = scenario.destination.supply()?;

    for mutation in 0..20 {
        let mut certificate = valid.clone();
        mutate_signed_field(&mut certificate, mutation);
        assert_revert(scenario.destination.resolve(&intent, &certificate));
        assert_eq!(scenario.destination.balance(RECIPIENT)?, destination_before);
        assert_eq!(
            scenario.destination.balance(FAST_TRANSFER_ADDRESS)?,
            pool_before
        );
        assert_eq!(scenario.destination.supply()?, supply_before);
        assert_eq!(
            scenario.destination.status(intent.transfer_id())?.state,
            STATE_NONE
        );
    }

    assert_success(scenario.destination.resolve(&intent, &valid));
    assert_success(scenario.destination.resolve(&intent, &valid));
    assert_eq!(scenario.destination.balance(RECIPIENT)?, U256::from(25u64));
    assert_eq!(
        scenario.destination.balance(FAST_TRANSFER_ADDRESS)?,
        U256::from(75u64)
    );
    assert_eq!(scenario.destination.supply()?, supply_before);
    Ok(())
}

fn receipt_trie_and_header_ancestry_retire_exposure_once_impl() -> eyre::Result<()> {
    let mut scenario = paid_destination_scenario(40, 40)?;
    let intent = scenario.intent(40, 40, 3);
    let evidence = retirement_evidence(&intent);
    seed_accepted_source_hash(
        &scenario.destination.l1,
        evidence.accepted_source_block_hash,
    );

    assert_success(scenario.destination.retire_exposure(&evidence));
    assert_eq!(
        scenario.destination.exposure(intent.source.domain_hash())?,
        (0, u128::MAX)
    );
    assert!(
        scenario
            .destination
            .status(intent.transfer_id())?
            .exposureRetired
    );

    assert_success(scenario.destination.retire_exposure(&evidence));
    assert_eq!(
        scenario.destination.exposure(intent.source.domain_hash())?,
        (0, u128::MAX)
    );
    assert_eq!(scenario.destination.balance(RECIPIENT)?, U256::from(40u64));
    assert_eq!(scenario.destination.supply()?, U256::from(2_000u64));
    Ok(())
}

fn forged_receipt_wrong_beneficiary_wrong_root_and_ancestry_do_not_retire_impl() -> eyre::Result<()>
{
    let mut scenario = paid_destination_scenario(41, 40)?;
    let intent = scenario.intent(41, 40, 3);
    let valid = retirement_evidence(&intent);
    seed_accepted_source_hash(&scenario.destination.l1, valid.accepted_source_block_hash);

    let mut forged_receipt = valid.clone();
    forged_receipt.receipt_proof[8] ^= 1;
    assert_revert(scenario.destination.retire_exposure(&forged_receipt));

    let mut wrong_beneficiary = valid.clone();
    wrong_beneficiary.beneficiary = SENDER;
    assert_revert(scenario.destination.retire_exposure(&wrong_beneficiary));

    let mut wrong_root = valid.clone();
    wrong_root.accepted_source_block_hash = B256::repeat_byte(0x44);
    assert_revert(scenario.destination.retire_exposure(&wrong_root));

    let mut wrong_ancestry = valid.clone();
    let last = wrong_ancestry.header_chain.len() - 1;
    wrong_ancestry.header_chain[last] ^= 1;
    assert_revert(scenario.destination.retire_exposure(&wrong_ancestry));

    let preceding_settlement = TempoHeader {
        inner: Header {
            number: 76,
            ..Default::default()
        },
        ..Default::default()
    }
    .hash_slow();
    seed_accepted_source_hash(&scenario.destination.l1, preceding_settlement);
    let mut settled_before_release = valid.clone();
    settled_before_release.accepted_source_block_hash = preceding_settlement;
    assert_revert(
        scenario
            .destination
            .retire_exposure(&settled_before_release),
    );
    seed_accepted_source_hash(&scenario.destination.l1, valid.accepted_source_block_hash);

    assert_eq!(
        scenario.destination.exposure(intent.source.domain_hash())?,
        (40, u128::MAX)
    );
    assert!(
        !scenario
            .destination
            .status(intent.transfer_id())?
            .exposureRetired
    );
    assert_eq!(scenario.destination.balance(RECIPIENT)?, U256::from(40u64));
    assert_eq!(scenario.destination.supply()?, U256::from(2_000u64));
    Ok(())
}

fn seeded_randomized_schedule_matches_literal_native_balances_and_supply_impl() -> eyre::Result<()>
{
    const TRANSFERS: usize = 4;
    const PRINCIPAL: u64 = 10;
    const SEED: u64 = 0x005e_ed14;

    let mut scenario = Scenario::new()?;
    let intents = (0..TRANSFERS)
        .map(|index| scenario.intent(100 + index as u64, PRINCIPAL, 0))
        .collect::<Vec<_>>();
    assert_success(scenario.destination.fund_pool(20, 0));
    assert_success(
        scenario
            .destination
            .set_exposure_limit(intents[0].source.domain_hash(), 20),
    );
    for intent in &intents {
        assert_success(scenario.source.lock(intent, &scenario.quote(intent)));
    }

    let source_supply = scenario.source.supply()?;
    let destination_supply = scenario.destination.supply()?;
    let mut resolved = [None; TRANSFERS];
    let mut disposed = [false; TRANSFERS];
    let mut remaining_pool = 20u64;
    let mut recipient = 0u64;
    let mut sender = 1_000u64 - PRINCIPAL * TRANSFERS as u64;
    let mut reimbursement = 0u64;
    let mut rng = SEED;

    for _ in 0..128 {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        let index = ((rng >> 32) as usize) % TRANSFERS;
        if rng & 1 == 0 {
            let intent = &intents[index];
            assert_success(
                scenario
                    .destination
                    .resolve(intent, &scenario.locked(intent)),
            );
            if resolved[index].is_none() {
                let paid = remaining_pool >= PRINCIPAL;
                resolved[index] = Some(paid);
                if paid {
                    remaining_pool -= PRINCIPAL;
                    recipient += PRINCIPAL;
                }
            }
        } else if let Some(paid) = resolved[index] {
            let intent = &intents[index];
            let outcome = if paid {
                scenario.paid(intent)
            } else {
                scenario.rejected(intent)
            };
            assert_success(scenario.source.record_outcome(intent, &outcome));
            assert_success(scenario.source.dispose(intent.transfer_id()));
            if !disposed[index] {
                disposed[index] = true;
                if paid {
                    reimbursement += PRINCIPAL;
                } else {
                    sender += PRINCIPAL;
                }
            }
        }

        assert_eq!(
            scenario.destination.balance(RECIPIENT)?,
            U256::from(recipient)
        );
        assert_eq!(
            scenario.destination.balance(FAST_TRANSFER_ADDRESS)?,
            U256::from(remaining_pool)
        );
        assert_eq!(scenario.source.balance(SENDER)?, U256::from(sender));
        assert_eq!(
            scenario.source.balance(REIMBURSEMENT)?,
            U256::from(reimbursement)
        );
        assert_eq!(scenario.source.supply()?, source_supply);
        assert_eq!(scenario.destination.supply()?, destination_supply);
    }

    for (index, intent) in intents.iter().enumerate() {
        if resolved[index].is_none() {
            assert_success(
                scenario
                    .destination
                    .resolve(intent, &scenario.locked(intent)),
            );
            let paid = remaining_pool >= PRINCIPAL;
            resolved[index] = Some(paid);
            if paid {
                remaining_pool -= PRINCIPAL;
                recipient += PRINCIPAL;
            }
        }
        if !disposed[index] {
            let outcome = if resolved[index] == Some(true) {
                scenario.paid(intent)
            } else {
                scenario.rejected(intent)
            };
            assert_success(scenario.source.record_outcome(intent, &outcome));
            assert_success(scenario.source.dispose(intent.transfer_id()));
            if resolved[index] == Some(true) {
                reimbursement += PRINCIPAL;
            } else {
                sender += PRINCIPAL;
            }
        }
    }

    assert_eq!(recipient, 20);
    assert_eq!(remaining_pool, 0);
    assert_eq!(sender, 980);
    assert_eq!(reimbursement, 20);
    assert_eq!(scenario.destination.balance(RECIPIENT)?, U256::from(20u64));
    assert_eq!(scenario.source.balance(SENDER)?, U256::from(980u64));
    assert_eq!(scenario.source.balance(REIMBURSEMENT)?, U256::from(20u64));
    assert_eq!(scenario.source.balance(FAST_TRANSFER_ADDRESS)?, U256::ZERO);
    assert_eq!(scenario.source.supply()?, source_supply);
    assert_eq!(scenario.destination.supply()?, destination_supply);
    Ok(())
}

fn run_acceptance_test(test: fn() -> eyre::Result<()>) -> eyre::Result<()> {
    std::thread::Builder::new()
        .name("t14-native-acceptance".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(test)?
        .join()
        .map_err(|_| eyre::eyre!("T14 acceptance worker panicked"))?
}

macro_rules! acceptance_tests {
    ($($name:ident => $implementation:ident),+ $(,)?) => {
        $(
            #[test]
            fn $name() -> eyre::Result<()> {
                run_acceptance_test($implementation)
            }
        )+
    };
}

acceptance_tests! {
    published_tempo_config_fields_are_consensus_pinned =>
        published_tempo_config_fields_are_consensus_pinned_impl,
    closed_epoch_resolve_requires_exact_anchored_barrier_proof =>
        closed_epoch_resolve_requires_exact_anchored_barrier_proof_impl,
    actual_escrow_payment_release_and_t14_spend_preserve_supply =>
        actual_escrow_payment_release_and_t14_spend_preserve_supply_impl,
    rejection_refund_and_source_business_nonce_retries_have_literal_deltas =>
        rejection_refund_and_source_business_nonce_retries_have_literal_deltas_impl,
    closed_account_policy_blocks_liability_without_changing_beneficiary =>
        closed_account_policy_blocks_liability_without_changing_beneficiary_impl,
    closed_destination_account_roles_fail_closed_without_pool_debit =>
        closed_destination_account_roles_fail_closed_without_pool_debit_impl,
    pool_liquidity_exposure_and_operator_withdrawal_are_serialized =>
        pool_liquidity_exposure_and_operator_withdrawal_are_serialized_impl,
    pre_t14_dispatch_is_gated_with_zero_token_effects =>
        pre_t14_dispatch_is_gated_with_zero_token_effects_impl,
    every_signed_outcome_field_mutation_has_zero_token_effects =>
        every_signed_outcome_field_mutation_has_zero_token_effects_impl,
    receipt_trie_and_header_ancestry_retire_exposure_once =>
        receipt_trie_and_header_ancestry_retire_exposure_once_impl,
    forged_receipt_wrong_beneficiary_wrong_root_and_ancestry_do_not_retire =>
        forged_receipt_wrong_beneficiary_wrong_root_and_ancestry_do_not_retire_impl,
    seeded_randomized_schedule_matches_literal_native_balances_and_supply =>
        seeded_randomized_schedule_matches_literal_native_balances_and_supply_impl,
}

fn paid_destination_scenario(nonce: u64, principal: u64) -> eyre::Result<Scenario> {
    let mut scenario = Scenario::new()?;
    let intent = scenario.intent(nonce, principal, 3);
    assert_success(scenario.destination.fund_pool(100, 0));
    assert_success(
        scenario
            .destination
            .set_exposure_limit(intent.source.domain_hash(), u128::MAX),
    );
    assert_success(
        scenario
            .destination
            .resolve(&intent, &scenario.locked(&intent)),
    );
    Ok(scenario)
}

fn retirement_evidence(intent: &TransferIntent) -> ExposureRetirementEvidence {
    let event_signature = keccak256("EscrowDisposed(bytes32,uint8,address,uint128)");
    let beneficiary_topic = B256::left_padding_from(REIMBURSEMENT.as_slice());
    let mut event_data = [0u8; 64];
    event_data[31] = OUTCOME_PAID;
    event_data[48..].copy_from_slice(
        &u128::try_from(intent.principal + intent.fee)
            .unwrap()
            .to_be_bytes(),
    );
    let log = Log {
        address: FAST_TRANSFER_ADDRESS,
        data: LogData::new_unchecked(
            vec![event_signature, intent.transfer_id(), beneficiary_topic],
            event_data.into(),
        ),
    };
    let receipt = TempoReceipt {
        tx_type: TempoTxType::Legacy,
        success: true,
        cumulative_gas_used: 100_000,
        logs: vec![log],
    }
    .into_with_bloom();
    let receipt_bytes = receipt.encoded_2718();

    let key = alloy_rlp::encode(0u64);
    let nibbles = Nibbles::unpack(&key);
    let mut builder: HashBuilder =
        HashBuilder::default().with_proof_retainer(ProofRetainer::new(vec![nibbles]));
    builder.add_leaf(nibbles, &receipt_bytes);
    let receipts_root = builder.root();
    let nodes = builder
        .take_proof_nodes()
        .into_nodes_sorted()
        .into_iter()
        .map(|(_, node)| node.to_vec())
        .collect::<Vec<_>>();

    let header = TempoHeader {
        inner: Header {
            receipts_root,
            number: 77,
            ..Default::default()
        },
        ..Default::default()
    };
    let accepted_source_block_hash = header.hash_slow();
    let receipt_proof = ReceiptInclusionProof {
        transaction_index: 0,
        receipt: receipt_bytes.clone(),
        nodes,
    }
    .canonical_bytes();
    let header_chain = HeaderAncestryProof {
        headers: vec![alloy_rlp::encode(header)],
    }
    .canonical_bytes();

    ExposureRetirementEvidence {
        transfer_id: intent.transfer_id(),
        intent_hash: intent.intent_hash(),
        accepted_source_block_hash,
        release_receipt_hash: keccak256(&receipt_bytes),
        destination_token: TOKEN,
        beneficiary: REIMBURSEMENT,
        principal: intent.principal,
        receipt_proof,
        header_chain,
    }
}

fn seed_accepted_source_hash(l1: &MockL1Reader, hash: B256) {
    l1.with_storage(ANCHOR, || {
        ZonePortalStorage::new(SOURCE_PORTAL).block_hash.write(hash)
    })
    .expect("seed finalized accepted source hash");
}

fn seed_enabled_asset(l1: &MockL1Reader, portal_address: Address, token: Address) {
    l1.with_storage(ANCHOR, || {
        let mut storage = ZonePortalStorage::new(portal_address);
        storage.token_configs[token].enabled.write(true)?;
        storage.token_configs[token].deposits_active.write(true)
    })
    .expect("seed finalized enabled asset");
}

fn signing_key(seed: u8) -> SigningKey {
    let mut bytes = [0u8; 32];
    bytes[31] = seed.max(1);
    SigningKey::from_bytes((&bytes).into()).expect("valid deterministic key")
}

fn member_address(key: &SigningKey) -> Address {
    Address::from_public_key(key.verifying_key())
}

fn sign(key: &SigningKey, digest: B256) -> SignatureBytes {
    let (signature, recovery_id) = key
        .sign_prehash_recoverable(digest.as_slice())
        .expect("prehash is exactly 32 bytes");
    let mut bytes = [0u8; 65];
    bytes[..64].copy_from_slice(&signature.to_bytes());
    bytes[64] = recovery_id.to_byte();
    SignatureBytes(bytes)
}

fn seed_epoch(l1: &MockL1Reader, authority: &Authority) {
    let portal = authority.domain.portal;
    let epoch = authority.domain.authority_epoch;
    l1.with_storage(ANCHOR, || {
        let mut storage = ZonePortalStorage::new(portal);
        storage.fast_epoch.write(epoch)?;
        storage.fast_epochs[epoch].write(PortalFastEpochConfig {
            protocol_version: u32::from(authority.domain.protocol_version),
            threshold: 2,
            proof_mode: FAST_PROOF_MODE_OPERATOR_ATTESTED,
            closed: false,
            retired: false,
            expected_peer_barriers: 9,
            recorded_peer_barriers: 0,
            finalized_peer_barriers: 0,
            activated_at_tempo_block: ANCHOR,
            roster_hash: authority.domain.roster_hash,
            peers_hash: keccak256(authority.peers.to_vec().abi_encode()),
            expected_verifier_code_hash: verifier_code_hash(),
            expected_verifier_config_hash: verifier_config_hash(),
            closure_hash: B256::ZERO,
            final_settlement_height: U256::ZERO,
            final_settlement_block_hash: B256::ZERO,
            final_settlement_withdrawal_batch_index: 0,
            barriers_hash: B256::ZERO,
            final_settlement_hash: B256::ZERO,
            next_epoch: 0,
            next_roster_hash: B256::ZERO,
            checkpoint_log_term: 0,
            checkpoint_log_index: 0,
            checkpoint_height: U256::ZERO,
            checkpoint_block_hash: B256::ZERO,
            checkpoint_state_root: B256::ZERO,
            checkpoint_hash: B256::ZERO,
        })?;
        storage.fast_epoch_members[epoch].write(authority.verifier.roster().members.to_vec())?;
        for member in authority.verifier.roster().members {
            storage.is_fast_epoch_member[epoch][member].write(true)?;
        }
        storage.fast_epoch_peers[epoch].write(authority.peers.to_vec())?;
        for peer in authority.peers {
            storage.is_fast_epoch_peer[epoch][peer].write(true)?;
        }
        Ok(())
    })
    .expect("seed finalized published-Tempo fast epoch");
}

fn seed_access(l1: &MockL1Reader, portal: Address, enforced: bool, accounts: &[Address]) {
    use tempo_precompiles::zone_factory::ZonePortalStorage;
    use tempo_zone_contracts::ZonePortal::Role;

    l1.with_storage(ANCHOR, || {
        let mut storage = ZonePortalStorage::new(portal);
        storage.is_access_enforced.write(enforced)?;
        for account in [
            SENDER,
            RECIPIENT,
            NEXT_RECIPIENT,
            POOL_OPERATOR,
            REIMBURSEMENT,
        ] {
            storage.role[account].write(u8::from(Role::None))?;
        }
        for account in accounts {
            storage.role[*account].write(u8::from(Role::Account))?;
        }
        Ok(())
    })
    .expect("seed finalized portal access state");
}

fn outcome_body(
    intent: &TransferIntent,
    zone: ZoneDomain,
    outcome: TransferOutcome,
) -> CertificateBody {
    CertificateBody {
        transfer_id: intent.transfer_id(),
        intent_hash: intent.intent_hash(),
        zone,
        log_term: 7,
        log_index: 9,
        block_height: 11,
        block_hash: keccak256("committed block"),
        state_root: keccak256("committed state"),
        transaction_hash: keccak256("committed transaction"),
        outcome,
    }
}

fn mutate_signed_field(certificate: &mut OutcomeCertificate, mutation: usize) {
    let body = &mut certificate.body;
    match mutation {
        0 => body.transfer_id = B256::repeat_byte(1),
        1 => body.intent_hash = B256::repeat_byte(2),
        2 => body.zone.l1_chain_id += 1,
        3 => body.zone.zone_id += 1,
        4 => body.zone.chain_id += 1,
        5 => body.zone.portal = Address::repeat_byte(5),
        6 => body.zone.authority_epoch += 1,
        7 => body.zone.roster_hash = B256::repeat_byte(7),
        8 => body.zone.protocol_version += 1,
        9 => body.log_term += 1,
        10 => body.log_index += 1,
        11 => body.block_height += 1,
        12 => body.block_hash = B256::repeat_byte(12),
        13 => body.state_root = B256::repeat_byte(13),
        14 => body.transaction_hash = B256::repeat_byte(14),
        15 => {
            body.outcome = TransferOutcome::Locked {
                escrow: Address::repeat_byte(15),
                amount: U256::from(26),
            }
        }
        16 => {
            body.outcome = TransferOutcome::Locked {
                escrow: FAST_TRANSFER_ADDRESS,
                amount: U256::from(27),
            }
        }
        17 => certificate.signatures[0].0[0] ^= 1,
        18 => certificate.signatures[1] = certificate.signatures[0],
        19 => {
            body.outcome = TransferOutcome::Rejected {
                reason: RejectionReason::PolicyDenied,
            }
        }
        _ => unreachable!(),
    }
}

fn assert_success(result: PrecompileResult) {
    let output = result.expect("native precompile call should return an EVM result");
    assert!(output.is_success(), "expected success, got {output:?}");
}

fn assert_revert(result: PrecompileResult) {
    let output = result.expect("native precompile call should return an EVM result");
    assert!(output.is_revert(), "expected revert, got {output:?}");
}
