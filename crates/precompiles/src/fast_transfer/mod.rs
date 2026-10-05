//! Deterministic native escrow and funded-pool state for instant Zone transfers.
//!
//! The public entrypoint is installed only under the T14 execution spec. Certificate-bearing
//! operations additionally remain fail closed until their finalized epoch/roster evidence is
//! verified by the dispatch boundary.

mod barrier;
mod dispatch;
mod imported_barrier;
mod inventory;

#[cfg(test)]
mod t14_acceptance_tests;

#[cfg(test)]
mod tests;

use alloc::vec::Vec;
use alloy_consensus::{BlockHeader, ReceiptWithBloom, Sealable, TxReceipt};
use alloy_eips::eip2718::Decodable2718;
use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, B256, Bytes, U256, b256, keccak256};
use alloy_rlp::Decodable;
use alloy_sol_types::SolValue;
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_contracts::precompiles::{
    FAST_PROOF_MODE_OPERATOR_ATTESTED, FAST_PROOF_MODE_REQUIRED, FAST_PROTOCOL_NATIVE_PIN,
};
use tempo_precompiles::{
    error::TempoPrecompileError,
    storage::{ContractStorage, Handler, Mapping, Slot},
    tip20::{ITIP20, TIP20Error, TIP20Token},
    zone_factory::{PortalFastEpochConfig, ZonePortalStorage},
};
use tempo_precompiles_macros::contract;
use tempo_zone_contracts::{
    FAST_TRANSFER_ADDRESS, FastTransferError, FastTransferEvent, FastTransferStatus, PoolState,
};
use zone_fast_transfer::{EpochRoster, QuorumVerifier};
use zone_primitives::fast_transfer::{
    ExposureRetirementEvidence, HeaderAncestryProof, ReceiptInclusionProof, RejectionReason,
    TransferIntent,
};

use crate::{
    ZoneResult,
    execution::{CallCheck, CallRules},
    storage::{L1State, L1StorageReader},
};

/// Maximum accepted encoded certificate size. The fixed certificate body plus two signatures is
/// comfortably below the protocol's 4 KiB transport limit.
pub const MAX_CERTIFICATE_BYTES: usize = 4 * 1024;
/// Maximum combined receipt-inclusion and header-ancestry proof size.
pub const MAX_RETIREMENT_PROOF_BYTES: usize = 512 * 1024;
/// Maximum headers accepted by one ancestry verification chunk.
pub const MAX_RETIREMENT_HEADERS: usize = 256;
/// Maximum encoded closed-epoch barrier inclusion proof.
pub const MAX_BARRIER_PROOF_BYTES: usize = 1024;
/// Protocol-wide maximum number of locks committed by one source barrier.
pub const MAX_DRAIN_LOCKS: usize = 10_000;
/// Maximum Merkle path depth for [`MAX_DRAIN_LOCKS`].
pub const MAX_DRAIN_PROOF_DEPTH: usize = 14;

pub const STATE_NONE: u8 = 0;
pub const STATE_LOCKED: u8 = 1;
pub const STATE_PAID: u8 = 2;
pub const STATE_REJECTED: u8 = 3;
pub const STATE_PAID_AWAITING_RELEASE: u8 = 4;
pub const STATE_REJECTED_AWAITING_REFUND: u8 = 5;
pub const STATE_RELEASED: u8 = 6;
pub const STATE_REFUNDED: u8 = 7;

pub const OUTCOME_PAID: u8 = 2;
pub const OUTCOME_REJECTED: u8 = 3;

const ESCROW_DISPOSED_TOPIC: B256 =
    b256!("3d3e38cad81141003380c51d5870036039d871bd4c1595db429d0a97fed4f1ec");

const EXPECTED_FAST_PROTOCOL_NATIVE_PIN: B256 =
    b256!("8e57521218d8ec9db175a95bca2c3a2061d54f8b60cd443c2c34735741534f8f");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EpochUse {
    Admission,
    Historical,
    DestinationResolution,
}

struct FinalizedEpochRegistry {
    config: PortalFastEpochConfig,
    members: [Address; 3],
    peers: [Address; 9],
}

fn read_epoch_registry<P: L1StorageReader>(
    l1: &L1State<P>,
    portal_address: Address,
    epoch: u64,
) -> ZoneResult<FinalizedEpochRegistry> {
    let portal = ZonePortalStorage::new(portal_address);
    let handler = portal.fast_epoch_config_at(epoch);
    let config = PortalFastEpochConfig {
        protocol_version: l1.read_l1(&handler.protocol_version)?,
        threshold: l1.read_l1(&handler.threshold)?,
        proof_mode: l1.read_l1(&handler.proof_mode)?,
        closed: l1.read_l1(&handler.closed)?,
        retired: l1.read_l1(&handler.retired)?,
        expected_peer_barriers: l1.read_l1(&handler.expected_peer_barriers)?,
        recorded_peer_barriers: l1.read_l1(&handler.recorded_peer_barriers)?,
        finalized_peer_barriers: l1.read_l1(&handler.finalized_peer_barriers)?,
        activated_at_tempo_block: l1.read_l1(&handler.activated_at_tempo_block)?,
        roster_hash: l1.read_l1(&handler.roster_hash)?,
        peers_hash: l1.read_l1(&handler.peers_hash)?,
        expected_verifier_code_hash: l1.read_l1(&handler.expected_verifier_code_hash)?,
        expected_verifier_config_hash: l1.read_l1(&handler.expected_verifier_config_hash)?,
        closure_hash: l1.read_l1(&handler.closure_hash)?,
        final_settlement_height: l1.read_l1(&handler.final_settlement_height)?,
        final_settlement_block_hash: l1.read_l1(&handler.final_settlement_block_hash)?,
        final_settlement_withdrawal_batch_index: l1
            .read_l1(&handler.final_settlement_withdrawal_batch_index)?,
        barriers_hash: l1.read_l1(&handler.barriers_hash)?,
        final_settlement_hash: l1.read_l1(&handler.final_settlement_hash)?,
        next_epoch: l1.read_l1(&handler.next_epoch)?,
        next_roster_hash: l1.read_l1(&handler.next_roster_hash)?,
        checkpoint_log_term: l1.read_l1(&handler.checkpoint_log_term)?,
        checkpoint_log_index: l1.read_l1(&handler.checkpoint_log_index)?,
        checkpoint_height: l1.read_l1(&handler.checkpoint_height)?,
        checkpoint_block_hash: l1.read_l1(&handler.checkpoint_block_hash)?,
        checkpoint_state_root: l1.read_l1(&handler.checkpoint_state_root)?,
        checkpoint_hash: l1.read_l1(&handler.checkpoint_hash)?,
    };
    let members_handler = portal.fast_epoch_members_at(epoch);
    if l1.read_l1(&Slot::<U256>::new(
        members_handler.len_slot(),
        portal_address,
    ))? != U256::from(3)
    {
        return Err(FastTransferError::invalid_certificate().into());
    }
    let mut members = [Address::ZERO; 3];
    for (index, member) in members.iter_mut().enumerate() {
        *member = l1.read_l1(&Slot::<Address>::new(
            members_handler.data_slot() + U256::from(index),
            portal_address,
        ))?;
        if !l1.read_l1(portal.fast_epoch_member_at(epoch, *member))? {
            return Err(FastTransferError::invalid_certificate().into());
        }
    }
    let peers_handler = portal.fast_epoch_peers_at(epoch);
    if l1.read_l1(&Slot::<U256>::new(peers_handler.len_slot(), portal_address))? != U256::from(9) {
        return Err(FastTransferError::invalid_certificate().into());
    }
    let mut peers = [Address::ZERO; 9];
    for (index, peer) in peers.iter_mut().enumerate() {
        *peer = l1.read_l1(&Slot::<Address>::new(
            peers_handler.data_slot() + U256::from(index),
            portal_address,
        ))?;
        if !l1.read_l1(portal.fast_epoch_peer_at(epoch, *peer))? {
            return Err(FastTransferError::invalid_certificate().into());
        }
    }

    let expected_peers_hash = keccak256(peers.to_vec().abi_encode());
    let expected_roster_hash = keccak256(
        (
            keccak256("TEMPO_ZONE_FAST_ROSTER_T14_V1"),
            portal_address,
            epoch,
            config.protocol_version,
            U256::from(config.threshold),
            U256::from(config.proof_mode),
            config.expected_verifier_code_hash,
            config.expected_verifier_config_hash,
            members.to_vec(),
            peers.to_vec(),
        )
            .abi_encode(),
    );
    if FAST_PROTOCOL_NATIVE_PIN != EXPECTED_FAST_PROTOCOL_NATIVE_PIN
        || config.protocol_version != 1
        || config.threshold != 2
        || !matches!(
            config.proof_mode,
            FAST_PROOF_MODE_OPERATOR_ATTESTED | FAST_PROOF_MODE_REQUIRED
        )
        || config.expected_peer_barriers != 9
        || config.expected_verifier_code_hash.is_zero()
        || config.expected_verifier_config_hash.is_zero()
        || config.peers_hash != expected_peers_hash
        || config.roster_hash != expected_roster_hash
        || members.iter().any(|member| member.is_zero())
        || members[0] == members[1]
        || members[0] == members[2]
        || members[1] == members[2]
        || peers
            .iter()
            .any(|peer| peer.is_zero() || *peer == portal_address)
        || peers
            .iter()
            .enumerate()
            .any(|(index, peer)| peers[..index].contains(peer))
    {
        return Err(FastTransferError::invalid_certificate().into());
    }
    Ok(FinalizedEpochRegistry {
        config,
        members,
        peers,
    })
}

/// Verify the current fast authority at the exact Tempo checkpoint committed by a same-anchor
/// opening. These reads use the execution witness/provider directly; block input never supplies
/// its own authority evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnchoredFastEpoch {
    pub tempo_block_number: u64,
    pub current_epoch: u64,
    pub activated_at_tempo_block: u64,
    pub closed: bool,
    pub retired: bool,
    pub proof_mode: u8,
    pub expected_verifier_code_hash: B256,
    pub expected_verifier_config_hash: B256,
    pub roster_hash: B256,
    pub members: [Address; 3],
    pub peer_portals: [Address; 9],
    pub native_pin: B256,
}

pub fn same_anchor_protocol_epoch<P: L1StorageReader>(
    l1: &L1State<P>,
    tempo_block_number: u64,
    protocol_epoch: u64,
) -> ZoneResult<Option<AnchoredFastEpoch>> {
    if protocol_epoch == 0 {
        return Ok(None);
    }
    let portal = ZonePortalStorage::new(l1.portal());
    let current_epoch = l1.read_l1(portal.fast_epoch_handler())?;
    if current_epoch != protocol_epoch {
        return Ok(None);
    }
    let registry = read_epoch_registry(l1, l1.portal(), protocol_epoch)?;
    if l1.get_anchor() != Some(tempo_block_number)
        || registry.config.activated_at_tempo_block == 0
        || registry.config.activated_at_tempo_block > tempo_block_number
        || registry.config.retired
    {
        return Ok(None);
    }
    Ok(Some(AnchoredFastEpoch {
        tempo_block_number,
        current_epoch,
        activated_at_tempo_block: registry.config.activated_at_tempo_block,
        closed: registry.config.closed,
        retired: registry.config.retired,
        proof_mode: registry.config.proof_mode,
        expected_verifier_code_hash: registry.config.expected_verifier_code_hash,
        expected_verifier_config_hash: registry.config.expected_verifier_config_hash,
        roster_hash: registry.config.roster_hash,
        members: registry.members,
        peer_portals: registry.peers,
        native_pin: FAST_PROTOCOL_NATIVE_PIN,
    }))
}

/// Consensus activation predicate shared by native registration and source-local tests.
#[inline]
pub const fn fast_transfer_active(spec: TempoHardfork) -> bool {
    spec.is_t14()
}

struct FastTransferCallRules {
    active: bool,
}

impl CallRules for FastTransferCallRules {
    fn admit(&self, _data: &[u8], _caller: Address) -> CallCheck {
        if self.active {
            CallCheck::Continue
        } else {
            CallCheck::Revert(Bytes::new())
        }
    }
}

/// Native state. Each field occupies a stable Solidity-compatible slot in declaration order.
/// Transfer records use parallel mappings so no Rust struct packing becomes consensus-critical.
#[contract(addr = FAST_TRANSFER_ADDRESS)]
pub struct FastTransfer {
    intent_hashes: Mapping<B256, B256>,
    states: Mapping<B256, u8>,
    tokens: Mapping<B256, Address>,
    source_zones: Mapping<B256, B256>,
    senders: Mapping<B256, Address>,
    recipients: Mapping<B256, Address>,
    refund_accounts: Mapping<B256, Address>,
    reimbursement_accounts: Mapping<B256, Address>,
    pools: Mapping<B256, Address>,
    principals: Mapping<B256, u128>,
    escrow_totals: Mapping<B256, u128>,
    rejection_reasons: Mapping<B256, u8>,
    exposure_retired: Mapping<B256, bool>,
    consumed_nonces: Mapping<Address, Mapping<u64, B256>>,
    pool_operators: Mapping<Address, Address>,
    pool_balances: Mapping<Address, u128>,
    pool_minimum_reserves: Mapping<Address, u128>,
    unsettled_exposure: Mapping<Address, Mapping<B256, u128>>,
    exposure_limits: Mapping<Address, Mapping<B256, u128>>,
    /// Source Portal retained at the destination for finalized ancestry verification.
    source_portals: Mapping<B256, Address>,
    /// Header hashes transitively authenticated to a finalized Portal accepted block hash.
    ancestry_checkpoints: Mapping<Address, Mapping<B256, bool>>,
    replenishment_treasury_operators: Mapping<Address, Mapping<Address, Address>>,
    inventory_job_hashes: Mapping<B256, B256>,
    inventory_job_operators: Mapping<B256, Address>,
    inventory_job_tokens: Mapping<B256, Address>,
    inventory_job_treasuries: Mapping<B256, Address>,
    inventory_job_amounts: Mapping<B256, u128>,
    inventory_job_contribution_counts: Mapping<B256, u32>,
    inventory_job_contributions: Mapping<B256, Mapping<u32, B256>>,
    inventory_allocations: Mapping<B256, B256>,
    inventory_job_fallback_nonces: Mapping<B256, u64>,
    inventory_jobs_by_fallback_nonce: Mapping<u64, B256>,
    inventory_job_withdrawal_indexes: Mapping<B256, u64>,
    inventory_job_restored: Mapping<B256, bool>,
    replenishment_credits: Mapping<B256, u128>,
    /// Immutable authenticated inventory hash keyed by destination epoch and source authority.
    imported_barriers: Mapping<B256, B256>,
}

impl FastTransfer {
    pub fn initialize(&mut self) -> tempo_precompiles::Result<()> {
        self.__initialize()
    }

    /// Creates the direct-call-only precompile. Registration is safe before activation because
    /// every stateful and state-reading call is rejected by the consensus predicate.
    pub fn create<P>(l1: L1State<P>, env: &crate::ZonePrecompileEnv) -> DynPrecompile
    where
        P: L1StorageReader,
    {
        crate::execution::create_precompile(
            "FastTransfer",
            env,
            FastTransferCallRules {
                active: fast_transfer_active(env.spec()),
            },
            move |data, caller| {
                #[cfg(feature = "std")]
                let transaction = crate::tx_context::current_transaction();
                #[cfg(not(feature = "std"))]
                let transaction = None;
                let (tx_hash, fee_payer) = transaction.unwrap_or((B256::ZERO, caller));
                Self::new().call(&l1, data, caller, tx_hash, fee_payer)
            },
        )
    }

    /// Load and validate the exact historical authority schema from typed Portal handlers at the
    /// finalized execution anchor.
    fn epoch_verifier<P: L1StorageReader>(
        l1: &L1State<P>,
        domain: zone_primitives::fast_transfer::ZoneDomain,
        use_case: EpochUse,
    ) -> ZoneResult<(QuorumVerifier, PortalFastEpochConfig)> {
        let portal = ZonePortalStorage::new(domain.portal);
        let current_epoch = l1.read_l1(portal.fast_epoch_handler())?;
        let registry = read_epoch_registry(l1, domain.portal, domain.authority_epoch)?;
        if registry.config.protocol_version != u32::from(domain.protocol_version)
            || registry.config.roster_hash != domain.roster_hash
            || registry.config.activated_at_tempo_block == 0
            || match use_case {
                EpochUse::Admission => {
                    current_epoch != domain.authority_epoch
                        || registry.config.closed
                        || registry.config.retired
                }
                EpochUse::Historical => false,
                EpochUse::DestinationResolution => {
                    current_epoch != domain.authority_epoch || registry.config.retired
                }
            }
        {
            return Err(FastTransferError::invalid_certificate().into());
        }
        let roster = EpochRoster::from_finalized_registry(domain, registry.members)
            .map_err(|_| FastTransferError::invalid_certificate())?;
        Ok((QuorumVerifier::new(roster), registry.config))
    }

    fn verify_closed_barrier<P: L1StorageReader>(
        l1: &L1State<P>,
        intent: &TransferIntent,
        lock: &zone_primitives::fast_transfer::OutcomeCertificate,
        config: &PortalFastEpochConfig,
        proof: &barrier::BarrierInclusionProof,
    ) -> ZoneResult<()> {
        let destination = ZonePortalStorage::new(intent.destination.portal);
        if config.closure_hash.is_zero()
            || proof.destination_portal != intent.destination.portal
            || proof.destination_epoch != intent.destination.authority_epoch
            || proof.closure_hash != config.closure_hash
            || proof.source_portal != intent.source.portal
            || proof.source_epoch != intent.source.authority_epoch
            || !l1.read_l1(
                destination
                    .fast_epoch_peer_at(intent.destination.authority_epoch, intent.source.portal),
            )?
        {
            return Err(FastTransferError::invalid_certificate().into());
        }

        let barrier = destination
            .fast_peer_barrier_at(intent.destination.authority_epoch, intent.source.portal);
        let recorded = l1.read_l1(&barrier.recorded)?;
        let source_epoch = l1.read_l1(&barrier.source_epoch)?;
        let imported_anchor_number = l1.read_l1(&barrier.imported_anchor_number)?;
        let imported_anchor_hash = l1.read_l1(&barrier.imported_anchor_hash)?;
        let log_index = l1.read_l1(&barrier.log_index)?;
        let block_hash = l1.read_l1(&barrier.block_hash)?;
        let state_root = l1.read_l1(&barrier.state_root)?;
        let lock_log_watermark = l1.read_l1(&barrier.lock_log_watermark)?;
        let complete_lock_root = l1.read_l1(&barrier.complete_lock_root)?;
        let barrier_hash = l1.read_l1(&barrier.barrier_hash)?;
        if !recorded
            || source_epoch != proof.source_epoch
            || imported_anchor_number != proof.imported_anchor_number
            || imported_anchor_hash != proof.imported_anchor_hash
            || barrier_hash != proof.barrier_hash
            || lock_log_watermark != proof.lock_log_watermark
            || complete_lock_root != proof.complete_lock_root
            || log_index < lock_log_watermark
            || block_hash.is_zero()
            || state_root.is_zero()
            || lock.body.log_index > log_index
            || proof
                .verify_lock(intent.transfer_id(), intent.intent_hash(), lock)
                .is_err()
        {
            return Err(FastTransferError::invalid_certificate().into());
        }
        Ok(())
    }

    fn amount_u128(value: U256) -> ZoneResult<u128> {
        value
            .try_into()
            .map_err(|_| FastTransferError::arithmetic_overflow().into())
    }

    fn validate_intent(intent: &TransferIntent) -> ZoneResult<()> {
        intent
            .validate()
            .map_err(|_| FastTransferError::invalid_intent())?;
        let total = intent
            .principal
            .checked_add(intent.fee)
            .ok_or_else(FastTransferError::arithmetic_overflow)?;
        if total.is_zero()
            || intent.sender.is_zero()
            || intent.recipient.is_zero()
            || intent.refund_account != intent.sender
            || intent.reimbursement_account.is_zero()
            || intent.destination_pool.is_zero()
            || intent.asset.l1_token.is_zero()
            || intent.asset.source_token.is_zero()
            || intent.asset.destination_token.is_zero()
            || intent.asset.l1_token != intent.asset.source_token
            || intent.asset.l1_token != intent.asset.destination_token
        {
            return Err(FastTransferError::invalid_intent().into());
        }
        Self::amount_u128(total)?;
        Self::amount_u128(intent.principal)?;
        Ok(())
    }

    fn require_same_intent(&self, id: B256, intent_hash: B256) -> ZoneResult<u8> {
        let stored = self.intent_hashes[id].read()?;
        if !stored.is_zero() && stored != intent_hash {
            return Err(FastTransferError::intent_mismatch().into());
        }
        Ok(self.states[id].read()?)
    }

    fn check_account<P: L1StorageReader>(
        &self,
        l1: &L1State<P>,
        account: Address,
    ) -> ZoneResult<()> {
        if !Self::account_allowed(l1, account)? {
            return Err(FastTransferError::account_not_allowed(account).into());
        }
        Ok(())
    }

    fn account_allowed<P: L1StorageReader>(l1: &L1State<P>, account: Address) -> ZoneResult<bool> {
        use tempo_zone_contracts::ZonePortal::Role;
        Ok(!l1.read_portal(|portal| &portal.is_access_enforced)?
            || l1.has_portal_role(account, Role::Account)?)
    }

    fn validate_asset<P: L1StorageReader>(
        l1: &L1State<P>,
        intent: &TransferIntent,
        local_token: Address,
    ) -> ZoneResult<()> {
        let token = TIP20Token::from_address(local_token)?;
        if !token.is_initialized()? || token.decimals()? != intent.asset.decimals {
            return Err(FastTransferError::invalid_intent().into());
        }
        let source_portal = ZonePortalStorage::new(intent.source.portal);
        let destination_portal = ZonePortalStorage::new(intent.destination.portal);
        if !l1.read_l1(&source_portal.token_configs[intent.asset.l1_token].enabled)?
            || !l1.read_l1(&destination_portal.token_configs[intent.asset.l1_token].enabled)?
        {
            return Err(FastTransferError::invalid_intent().into());
        }
        Ok(())
    }

    fn transfer_in(&self, token: Address, owner: Address, amount: u128) -> ZoneResult<()> {
        let mut token = TIP20Token::from_address(token)?;
        if !token.is_initialized()? {
            return Err(TempoPrecompileError::from(TIP20Error::uninitialized()).into());
        }
        let before = token.balance_of(ITIP20::balanceOfCall {
            account: self.address,
        })?;
        if !token.transfer_from(
            self.address,
            ITIP20::transferFromCall {
                from: owner,
                to: self.address,
                amount: U256::from(amount),
            },
        )? {
            return Err(FastTransferError::transfer_failed().into());
        }
        let after = token.balance_of(ITIP20::balanceOfCall {
            account: self.address,
        })?;
        if after.checked_sub(before) != Some(U256::from(amount)) {
            return Err(FastTransferError::transfer_failed().into());
        }
        Ok(())
    }

    fn transfer_out(&self, token: Address, recipient: Address, amount: u128) -> ZoneResult<()> {
        let mut token = TIP20Token::from_address(token)?;
        let before = token.balance_of(ITIP20::balanceOfCall { account: recipient })?;
        if !token.transfer(
            self.address,
            ITIP20::transferCall {
                to: recipient,
                amount: U256::from(amount),
            },
        )? {
            return Err(FastTransferError::transfer_failed().into());
        }
        let after = token.balance_of(ITIP20::balanceOfCall { account: recipient })?;
        if after.checked_sub(before) != Some(U256::from(amount)) {
            return Err(FastTransferError::transfer_failed().into());
        }
        Ok(())
    }

    /// Attempt a destination payment in an inner journal. TIP-1028 can report a successful
    /// transfer while redirecting the credit to the receive-policy guard. In that case the
    /// recipient delta is not exact, so discard every token-side effect before the caller writes
    /// the permanent `PolicyDenied` tombstone.
    fn transfer_out_direct(
        &mut self,
        token: Address,
        recipient: Address,
        amount: u128,
    ) -> ZoneResult<bool> {
        let checkpoint = self.storage.checkpoint();
        let mut token = TIP20Token::from_address(token)?;
        let before = token.balance_of(ITIP20::balanceOfCall { account: recipient })?;
        if !token.transfer(
            self.address,
            ITIP20::transferCall {
                to: recipient,
                amount: U256::from(amount),
            },
        )? {
            return Err(FastTransferError::transfer_failed().into());
        }
        let after = token.balance_of(ITIP20::balanceOfCall { account: recipient })?;
        if after.checked_sub(before) != Some(U256::from(amount)) {
            return Ok(false);
        }
        checkpoint.commit();
        Ok(true)
    }

    /// Atomic source escrow lock. Identical retries return the existing state without another
    /// token movement; a reused ID or sender business nonce with a different body is rejected.
    fn lock_verified<P: L1StorageReader>(
        &mut self,
        l1: &L1State<P>,
        caller: Address,
        intent: TransferIntent,
    ) -> ZoneResult<u8> {
        Self::validate_intent(&intent)?;
        let transfer_id = intent.transfer_id();
        let intent_hash = intent.intent_hash();
        if caller != intent.sender || intent.source.portal != l1.portal() {
            return Err(FastTransferError::unauthorized().into());
        }
        let state = self.require_same_intent(transfer_id, intent_hash)?;
        if state != STATE_NONE {
            return Ok(state);
        }
        let nonce_body = self.consumed_nonces[caller][intent.transfer_nonce].read()?;
        if !nonce_body.is_zero() && nonce_body != intent_hash {
            return Err(FastTransferError::nonce_already_used().into());
        }
        self.check_account(l1, caller)?;
        self.check_account(l1, intent.refund_account)?;
        self.check_account(l1, intent.reimbursement_account)?;
        Self::validate_asset(l1, &intent, intent.asset.source_token)?;

        let total = intent
            .principal
            .checked_add(intent.fee)
            .ok_or_else(FastTransferError::arithmetic_overflow)?;
        let total = Self::amount_u128(total)?;
        self.transfer_in(intent.asset.source_token, caller, total)?;
        self.write_intent(&intent, intent_hash, intent.asset.source_token, total)?;
        self.consumed_nonces[caller][intent.transfer_nonce].write(intent_hash)?;
        self.states[transfer_id].write(STATE_LOCKED)?;
        self.emit_event(FastTransferEvent::locked(
            transfer_id,
            intent_hash,
            caller,
            intent.asset.source_token,
            total,
        ))?;
        Ok(STATE_LOCKED)
    }

    fn write_intent(
        &mut self,
        intent: &TransferIntent,
        intent_hash: B256,
        local_token: Address,
        total: u128,
    ) -> ZoneResult<()> {
        let id = intent.transfer_id();
        self.intent_hashes[id].write(intent_hash)?;
        self.tokens[id].write(local_token)?;
        self.source_zones[id].write(intent.source.domain_hash())?;
        self.source_portals[id].write(intent.source.portal)?;
        self.senders[id].write(intent.sender)?;
        self.recipients[id].write(intent.recipient)?;
        self.refund_accounts[id].write(intent.refund_account)?;
        self.reimbursement_accounts[id].write(intent.reimbursement_account)?;
        self.pools[id].write(intent.destination_pool)?;
        self.principals[id].write(Self::amount_u128(intent.principal)?)?;
        self.escrow_totals[id].write(total)?;
        Ok(())
    }

    /// Atomic destination decision after the certificate/quote/route hooks have authenticated the
    /// lock. A caller cannot use this helper until the external verifier has pinned the epoch.
    #[allow(
        dead_code,
        reason = "consensus-gated certificate verifier calls this seam after fast-fork activation"
    )]
    fn resolve_verified<P: L1StorageReader>(
        &mut self,
        l1: &L1State<P>,
        intent: TransferIntent,
        reject_reason: Option<u8>,
    ) -> ZoneResult<u8> {
        Self::validate_intent(&intent)?;
        let transfer_id = intent.transfer_id();
        let intent_hash = intent.intent_hash();
        let state = self.require_same_intent(transfer_id, intent_hash)?;
        if state != STATE_NONE {
            return Ok(state);
        }
        Self::validate_asset(l1, &intent, intent.asset.destination_token)?;
        if let Some(reason) = reject_reason {
            return self.record_rejection(&intent, intent_hash, reason);
        }
        if self.storage.block_number() >= intent.destination_expiry_height {
            return self.record_rejection(
                &intent,
                intent_hash,
                RejectionReason::QuoteExpired as u8,
            );
        }
        if !Self::account_allowed(l1, intent.recipient)? {
            return self.record_rejection(
                &intent,
                intent_hash,
                RejectionReason::PolicyDenied as u8,
            );
        }
        let operator = self.pool_operators[intent.asset.destination_token].read()?;
        if operator.is_zero() || operator != intent.destination_pool {
            return self.record_rejection(
                &intent,
                intent_hash,
                RejectionReason::RouteDisabled as u8,
            );
        }
        let balance = self.pool_balances[intent.asset.destination_token].read()?;
        let reserve = self.pool_minimum_reserves[intent.asset.destination_token].read()?;
        let spendable = balance.saturating_sub(reserve);
        let source_zone = intent.source.domain_hash();
        let exposure =
            self.unsettled_exposure[intent.asset.destination_token][source_zone].read()?;
        let limit = self.exposure_limits[intent.asset.destination_token][source_zone].read()?;
        let principal = Self::amount_u128(intent.principal)?;
        let next_exposure = exposure
            .checked_add(principal)
            .ok_or_else(FastTransferError::arithmetic_overflow)?;
        if principal > spendable {
            return self.record_rejection(
                &intent,
                intent_hash,
                RejectionReason::InsufficientLiquidity as u8,
            );
        }
        if limit == 0 || next_exposure > limit {
            return self.record_rejection(
                &intent,
                intent_hash,
                RejectionReason::ExposureLimit as u8,
            );
        }

        if !self.transfer_out_direct(intent.asset.destination_token, intent.recipient, principal)? {
            return self.record_rejection(
                &intent,
                intent_hash,
                RejectionReason::PolicyDenied as u8,
            );
        }
        self.write_intent(
            &intent,
            intent_hash,
            intent.asset.destination_token,
            Self::amount_u128(
                intent
                    .principal
                    .checked_add(intent.fee)
                    .ok_or_else(FastTransferError::arithmetic_overflow)?,
            )?,
        )?;
        self.pool_balances[intent.asset.destination_token].write(balance - principal)?;
        self.unsettled_exposure[intent.asset.destination_token][source_zone]
            .write(next_exposure)?;
        self.states[transfer_id].write(STATE_PAID)?;
        self.emit_event(FastTransferEvent::paid(
            transfer_id,
            intent_hash,
            intent.recipient,
            intent.asset.destination_token,
            principal,
        ))?;
        Ok(STATE_PAID)
    }

    #[allow(
        dead_code,
        reason = "used exclusively by the consensus-gated destination resolution seam"
    )]
    fn record_rejection(
        &mut self,
        intent: &TransferIntent,
        intent_hash: B256,
        reason: u8,
    ) -> ZoneResult<u8> {
        let transfer_id = intent.transfer_id();
        self.write_intent(
            intent,
            intent_hash,
            intent.asset.destination_token,
            Self::amount_u128(
                intent
                    .principal
                    .checked_add(intent.fee)
                    .ok_or_else(FastTransferError::arithmetic_overflow)?,
            )?,
        )?;
        self.rejection_reasons[transfer_id].write(reason)?;
        self.states[transfer_id].write(STATE_REJECTED)?;
        self.emit_event(FastTransferEvent::rejected(
            transfer_id,
            intent_hash,
            reason,
        ))?;
        Ok(STATE_REJECTED)
    }

    /// Records an authenticated destination decision without attempting policy-sensitive escrow
    /// disposal. This boundary keeps a later policy failure from forgetting the terminal result.
    #[allow(
        dead_code,
        reason = "consensus-gated certificate verifier calls this seam after fast-fork activation"
    )]
    fn record_outcome_verified(
        &mut self,
        transfer_id: B256,
        intent_hash: B256,
        outcome: u8,
    ) -> ZoneResult<u8> {
        let current = self.require_same_intent(transfer_id, intent_hash)?;
        match (outcome, current) {
            (OUTCOME_PAID, STATE_PAID_AWAITING_RELEASE | STATE_RELEASED)
            | (OUTCOME_REJECTED, STATE_REJECTED_AWAITING_REFUND | STATE_REFUNDED) => {
                return Ok(current);
            }
            (_, STATE_LOCKED) => {}
            _ => return Err(FastTransferError::invalid_state(current).into()),
        }
        let (state, beneficiary) = match outcome {
            OUTCOME_PAID => (
                STATE_PAID_AWAITING_RELEASE,
                self.reimbursement_accounts[transfer_id].read()?,
            ),
            OUTCOME_REJECTED => (
                STATE_REJECTED_AWAITING_REFUND,
                self.refund_accounts[transfer_id].read()?,
            ),
            _ => return Err(FastTransferError::invalid_certificate().into()),
        };
        self.states[transfer_id].write(state)?;
        self.emit_event(FastTransferEvent::outcome_recorded(
            transfer_id,
            outcome,
            beneficiary,
        ))?;
        Ok(state)
    }

    fn dispose_escrow_verified<P: L1StorageReader>(
        &mut self,
        l1: &L1State<P>,
        transfer_id: B256,
    ) -> ZoneResult<u8> {
        let state = self.states[transfer_id].read()?;
        if matches!(state, STATE_RELEASED | STATE_REFUNDED) {
            return Ok(state);
        }
        let (beneficiary, next) = match state {
            STATE_PAID_AWAITING_RELEASE => (
                self.reimbursement_accounts[transfer_id].read()?,
                STATE_RELEASED,
            ),
            STATE_REJECTED_AWAITING_REFUND => {
                (self.refund_accounts[transfer_id].read()?, STATE_REFUNDED)
            }
            _ => return Err(FastTransferError::invalid_state(state).into()),
        };
        self.check_account(l1, beneficiary)?;
        let token = self.tokens[transfer_id].read()?;
        let amount = self.escrow_totals[transfer_id].read()?;
        self.transfer_out(token, beneficiary, amount)?;
        self.states[transfer_id].write(next)?;
        self.emit_event(FastTransferEvent::escrow_disposed(
            transfer_id,
            if next == STATE_RELEASED {
                OUTCOME_PAID
            } else {
                OUTCOME_REJECTED
            },
            beneficiary,
            amount,
        ))?;
        Ok(next)
    }

    fn fund_pool_verified<P: L1StorageReader>(
        &mut self,
        l1: &L1State<P>,
        caller: Address,
        token: Address,
        amount: u128,
        minimum_reserve: u128,
    ) -> ZoneResult<()> {
        if amount == 0 {
            return Err(FastTransferError::invalid_intent().into());
        }
        self.check_account(l1, caller)?;
        let operator = self.pool_operators[token].read()?;
        if !operator.is_zero() && operator != caller {
            return Err(FastTransferError::unauthorized().into());
        }
        let balance = self.pool_balances[token]
            .read()?
            .checked_add(amount)
            .ok_or_else(FastTransferError::arithmetic_overflow)?;
        if minimum_reserve > balance {
            return Err(FastTransferError::insufficient_pool_liquidity().into());
        }
        self.transfer_in(token, caller, amount)?;
        self.pool_operators[token].write(caller)?;
        self.pool_balances[token].write(balance)?;
        self.pool_minimum_reserves[token].write(minimum_reserve)?;
        self.emit_event(FastTransferEvent::pool_funded(token, caller, amount))?;
        Ok(())
    }

    fn withdraw_pool_verified<P: L1StorageReader>(
        &mut self,
        l1: &L1State<P>,
        caller: Address,
        token: Address,
        recipient: Address,
        amount: u128,
    ) -> ZoneResult<()> {
        if self.pool_operators[token].read()? != caller {
            return Err(FastTransferError::unauthorized().into());
        }
        self.check_account(l1, caller)?;
        self.check_account(l1, recipient)?;
        let balance = self.pool_balances[token].read()?;
        let reserve = self.pool_minimum_reserves[token].read()?;
        if amount > balance.saturating_sub(reserve) {
            return Err(FastTransferError::insufficient_pool_liquidity().into());
        }
        self.transfer_out(token, recipient, amount)?;
        self.pool_balances[token].write(balance - amount)?;
        self.emit_event(FastTransferEvent::pool_withdrawn(
            token, caller, recipient, amount,
        ))?;
        Ok(())
    }

    fn set_exposure_limit_verified(
        &mut self,
        caller: Address,
        token: Address,
        source_zone: B256,
        limit: u128,
    ) -> ZoneResult<()> {
        if self.pool_operators[token].read()? != caller {
            return Err(FastTransferError::unauthorized().into());
        }
        let current = self.unsettled_exposure[token][source_zone].read()?;
        if limit < current {
            return Err(FastTransferError::exposure_limit_exceeded().into());
        }
        self.exposure_limits[token][source_zone].write(limit)?;
        Ok(())
    }

    /// Authenticate a successful source release against a receipt root and the source Portal's
    /// accepted block hash imported at this destination block's finalized L1 anchor.
    fn verify_retirement_evidence<P: L1StorageReader>(
        &self,
        l1: &L1State<P>,
        evidence: &ExposureRetirementEvidence,
    ) -> ZoneResult<()> {
        let invalid = || FastTransferError::invalid_retirement_evidence();
        if self.require_same_intent(evidence.transfer_id, evidence.intent_hash)? != STATE_PAID {
            return Err(invalid().into());
        }
        let principal = Self::amount_u128(evidence.principal)?;
        if self.tokens[evidence.transfer_id].read()? != evidence.destination_token
            || self.reimbursement_accounts[evidence.transfer_id].read()? != evidence.beneficiary
            || self.principals[evidence.transfer_id].read()? != principal
        {
            return Err(invalid().into());
        }

        let source_portal = self.source_portals[evidence.transfer_id].read()?;
        if source_portal.is_zero() {
            return Err(invalid().into());
        }
        let source = ZonePortalStorage::new(source_portal);
        let current_accepted = l1.read_l1(source.accepted_block_hash_handler())?;
        let authenticated_descendant = evidence.accepted_source_block_hash;
        if authenticated_descendant.is_zero()
            || (authenticated_descendant != current_accepted
                && !self.ancestry_checkpoints[source_portal][authenticated_descendant].read()?)
        {
            return Err(invalid().into());
        }

        let ancestry =
            HeaderAncestryProof::decode(&evidence.header_chain).map_err(|_| invalid())?;
        let mut headers = Vec::with_capacity(ancestry.headers.len());
        for encoded in ancestry.headers {
            let mut input = encoded.as_slice();
            let header =
                tempo_primitives::TempoHeader::decode(&mut input).map_err(|_| invalid())?;
            if !input.is_empty() {
                return Err(invalid().into());
            }
            headers.push(header);
        }
        for pair in headers.windows(2) {
            if pair[1].parent_hash() != pair[0].hash_slow()
                || pair[1].number() != pair[0].number().saturating_add(1)
            {
                return Err(invalid().into());
            }
        }
        if headers.last().map(|header| header.hash_slow()) != Some(authenticated_descendant) {
            return Err(invalid().into());
        }

        let proof =
            ReceiptInclusionProof::decode(&evidence.receipt_proof).map_err(|_| invalid())?;
        if keccak256(&proof.receipt) != evidence.release_receipt_hash {
            return Err(invalid().into());
        }
        let nodes: Vec<Bytes> = proof.nodes.into_iter().map(Bytes::from).collect();
        alloy_trie::proof::verify_proof(
            headers[0].receipts_root(),
            alloy_trie::Nibbles::unpack(alloy_rlp::encode(proof.transaction_index)),
            Some(proof.receipt.clone()),
            nodes.iter(),
        )
        .map_err(|_| invalid())?;

        let receipt =
            ReceiptWithBloom::<tempo_primitives::TempoReceipt>::decode_2718_exact(&proof.receipt)
                .map_err(|_| invalid())?;
        if !receipt.status() {
            return Err(invalid().into());
        }
        let total = self.escrow_totals[evidence.transfer_id].read()?;
        let mut beneficiary_topic = [0u8; 32];
        beneficiary_topic[12..].copy_from_slice(evidence.beneficiary.as_slice());
        let matching_release = receipt.logs().iter().any(|log| {
            let topics = log.data.topics();
            if log.address != FAST_TRANSFER_ADDRESS
                || topics.len() != 3
                || topics[0] != ESCROW_DISPOSED_TOPIC
                || topics[1] != evidence.transfer_id
                || topics[2] != B256::from(beneficiary_topic)
                || log.data.data.len() != 64
            {
                return false;
            }
            let data = log.data.data.as_ref();
            data[..31].iter().all(|byte| *byte == 0)
                && data[31] == OUTCOME_PAID
                && U256::from_be_slice(&data[32..]) == U256::from(total)
        });
        if !matching_release {
            return Err(invalid().into());
        }
        Ok(())
    }

    /// Persist a bounded chain of ancestors rooted in either the current finalized Portal tip or
    /// a previously authenticated checkpoint. This lets old releases advance in 256-header chunks.
    fn record_ancestry_checkpoint_verified<P: L1StorageReader>(
        &mut self,
        l1: &L1State<P>,
        source_portal: Address,
        encoded_chain: &[u8],
    ) -> ZoneResult<()> {
        let invalid = || FastTransferError::invalid_retirement_evidence();
        if source_portal.is_zero() {
            return Err(invalid().into());
        }
        let ancestry = HeaderAncestryProof::decode(encoded_chain).map_err(|_| invalid())?;
        let mut headers = Vec::with_capacity(ancestry.headers.len());
        for encoded in ancestry.headers {
            let mut input = encoded.as_slice();
            let header =
                tempo_primitives::TempoHeader::decode(&mut input).map_err(|_| invalid())?;
            if !input.is_empty() {
                return Err(invalid().into());
            }
            headers.push(header);
        }
        for pair in headers.windows(2) {
            if pair[1].parent_hash() != pair[0].hash_slow()
                || pair[1].number() != pair[0].number().saturating_add(1)
            {
                return Err(invalid().into());
            }
        }
        let terminal = headers
            .last()
            .expect("nonempty proof validated")
            .hash_slow();
        let source = ZonePortalStorage::new(source_portal);
        let current_accepted = l1.read_l1(source.accepted_block_hash_handler())?;
        if terminal != current_accepted
            && !self.ancestry_checkpoints[source_portal][terminal].read()?
        {
            return Err(invalid().into());
        }
        for header in headers {
            self.ancestry_checkpoints[source_portal][header.hash_slow()].write(true)?;
        }
        Ok(())
    }

    /// Applies already-verified finalized source receipt and ancestry evidence exactly once.
    fn retire_exposure_verified(
        &mut self,
        transfer_id: B256,
        intent_hash: B256,
        token: Address,
        beneficiary: Address,
        amount: u128,
    ) -> ZoneResult<()> {
        if self.require_same_intent(transfer_id, intent_hash)? != STATE_PAID
            || self.tokens[transfer_id].read()? != token
            || self.reimbursement_accounts[transfer_id].read()? != beneficiary
            || self.principals[transfer_id].read()? != amount
        {
            return Err(FastTransferError::invalid_retirement_evidence().into());
        }
        if self.exposure_retired[transfer_id].read()? {
            return Ok(());
        }
        let source_zone = self.source_zones[transfer_id].read()?;
        let exposure = self.unsettled_exposure[token][source_zone].read()?;
        let next = exposure
            .checked_sub(amount)
            .ok_or_else(FastTransferError::invalid_retirement_evidence)?;
        self.unsettled_exposure[token][source_zone].write(next)?;
        self.exposure_retired[transfer_id].write(true)?;
        self.emit_event(FastTransferEvent::exposure_retired(
            transfer_id,
            source_zone,
            token,
            amount,
        ))?;
        Ok(())
    }

    fn status(&self, transfer_id: B256) -> ZoneResult<FastTransferStatus> {
        let state = self.states[transfer_id].read()?;
        let beneficiary = match state {
            STATE_PAID | STATE_PAID_AWAITING_RELEASE | STATE_RELEASED => {
                self.reimbursement_accounts[transfer_id].read()?
            }
            STATE_REJECTED | STATE_REJECTED_AWAITING_REFUND | STATE_REFUNDED => {
                self.refund_accounts[transfer_id].read()?
            }
            _ => Address::ZERO,
        };
        Ok(FastTransferStatus {
            intentHash: self.intent_hashes[transfer_id].read()?,
            state,
            token: self.tokens[transfer_id].read()?,
            beneficiary,
            principal: self.principals[transfer_id].read()?,
            total: self.escrow_totals[transfer_id].read()?,
            exposureRetired: self.exposure_retired[transfer_id].read()?,
        })
    }

    fn pool_state(&self, token: Address) -> ZoneResult<PoolState> {
        Ok(PoolState {
            operator: self.pool_operators[token].read()?,
            fundedBalance: self.pool_balances[token].read()?,
            minimumReserve: self.pool_minimum_reserves[token].read()?,
        })
    }
}
