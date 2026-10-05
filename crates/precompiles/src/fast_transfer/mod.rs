//! Deterministic native escrow and funded-pool state for instant Zone transfers.
//!
//! The public entrypoint is deliberately dormant: the pinned Tempo protocol has no compatible
//! fast epoch, same-anchor executor, or finalized peer-roster storage. Keeping the implementation
//! behind [`fast_transfer_active`] prevents a CLI or calldata switch from activating only this
//! fraction of the protocol.

mod dispatch;

#[cfg(test)]
mod tests;

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_sol_types::SolValue;
use tempo_precompiles::{
    error::TempoPrecompileError,
    storage::{ContractStorage, Handler, Mapping},
    tip20::{ITIP20, TIP20Error, TIP20Token},
};
use tempo_precompiles_macros::contract;
use tempo_zone_contracts::{
    FAST_TRANSFER_ADDRESS, FastTransferError, FastTransferEvent, FastTransferStatus, Intent,
    PoolState,
};

use crate::{
    ZoneResult,
    storage::{L1State, L1StorageReader},
};

/// Maximum accepted encoded certificate size. The fixed certificate body plus two signatures is
/// comfortably below the protocol's 4 KiB transport limit.
pub const MAX_CERTIFICATE_BYTES: usize = 4 * 1024;
/// Maximum combined receipt-inclusion and header-ancestry proof size.
pub const MAX_RETIREMENT_PROOF_BYTES: usize = 512 * 1024;
/// Maximum headers accepted by one ancestry verification chunk.
pub const MAX_RETIREMENT_HEADERS: usize = 256;

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

/// No compatible execution fork exists in the pinned upstream `TempoHardfork` enum (T13 is the
/// latest variant). This MUST become a real fork/epoch predicate, not a process configuration.
#[inline]
pub const fn fast_transfer_active() -> bool {
    false
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
            crate::execution::NoCallRules,
            move |data, caller| Self::new().call(&l1, data, caller),
        )
    }

    fn ensure_active(&self) -> ZoneResult<()> {
        if !fast_transfer_active() {
            return Err(FastTransferError::fast_transfer_not_active().into());
        }
        Ok(())
    }

    fn intent_hash(intent: &Intent) -> B256 {
        keccak256(intent.abi_encode())
    }

    fn expected_transfer_id(intent: &Intent) -> B256 {
        // Canonical ABI encoding is length-delimited. The domain contains the L1 chain, source
        // Portal and protocol version as required by the protocol specification.
        keccak256(
            (
                keccak256("TEMPO_ZONE_FAST_TRANSFER"),
                intent.sourceChainId,
                intent.sourcePortal,
                intent.protocolVersion,
                intent.sourceZone,
                intent.sender,
                intent.transferNonce,
            )
                .abi_encode(),
        )
    }

    fn validate_intent(intent: &Intent, supplied_hash: B256) -> ZoneResult<()> {
        let total = intent
            .principal
            .checked_add(intent.fee)
            .ok_or_else(|| FastTransferError::arithmetic_overflow())?;
        if intent.transferId != Self::expected_transfer_id(intent)
            || supplied_hash != Self::intent_hash(intent)
            || intent.principal == 0
            || total == 0
            || intent.sender.is_zero()
            || intent.recipient.is_zero()
            || intent.refundAccount != intent.sender
            || intent.reimbursementAccount.is_zero()
            || intent.pool.is_zero()
            || intent.sourceToken.is_zero()
            || intent.destinationToken.is_zero()
        {
            return Err(FastTransferError::invalid_intent().into());
        }
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
        use tempo_zone_contracts::ZonePortal::Role;
        if l1.read_portal(|portal| &portal.is_access_enforced)?
            && !l1.has_portal_role(account, Role::Account)?
        {
            return Err(FastTransferError::account_not_allowed(account).into());
        }
        Ok(())
    }

    fn transfer_in(&self, token: Address, owner: Address, amount: u128) -> ZoneResult<()> {
        let mut token = TIP20Token::from_address(token)?;
        if !token.is_initialized()? {
            return Err(TempoPrecompileError::from(TIP20Error::uninitialized()).into());
        }
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
        Ok(())
    }

    fn transfer_out(&self, token: Address, recipient: Address, amount: u128) -> ZoneResult<()> {
        let mut token = TIP20Token::from_address(token)?;
        if !token.transfer(
            self.address,
            ITIP20::transferCall {
                to: recipient,
                amount: U256::from(amount),
            },
        )? {
            return Err(FastTransferError::transfer_failed().into());
        }
        Ok(())
    }

    /// Atomic source escrow lock. Identical retries return the existing state without another
    /// token movement; a reused ID or sender business nonce with a different body is rejected.
    fn lock_verified<P: L1StorageReader>(
        &mut self,
        l1: &L1State<P>,
        caller: Address,
        intent: Intent,
        intent_hash: B256,
    ) -> ZoneResult<u8> {
        Self::validate_intent(&intent, intent_hash)?;
        if caller != intent.sender || intent.sourcePortal != l1.portal() {
            return Err(FastTransferError::unauthorized().into());
        }
        let state = self.require_same_intent(intent.transferId, intent_hash)?;
        if state != STATE_NONE {
            return Ok(state);
        }
        let nonce_body = self.consumed_nonces[caller][intent.transferNonce].read()?;
        if !nonce_body.is_zero() && nonce_body != intent_hash {
            return Err(FastTransferError::nonce_already_used().into());
        }
        self.check_account(l1, caller)?;
        self.check_account(l1, intent.refundAccount)?;
        self.check_account(l1, intent.reimbursementAccount)?;

        let total = intent
            .principal
            .checked_add(intent.fee)
            .ok_or_else(|| FastTransferError::arithmetic_overflow())?;
        self.transfer_in(intent.sourceToken, caller, total)?;
        self.write_intent(&intent, intent_hash, total)?;
        self.consumed_nonces[caller][intent.transferNonce].write(intent_hash)?;
        self.states[intent.transferId].write(STATE_LOCKED)?;
        self.emit_event(FastTransferEvent::locked(
            intent.transferId,
            intent_hash,
            caller,
            intent.sourceToken,
            total,
        ))?;
        Ok(STATE_LOCKED)
    }

    fn write_intent(&mut self, intent: &Intent, intent_hash: B256, total: u128) -> ZoneResult<()> {
        let id = intent.transferId;
        self.intent_hashes[id].write(intent_hash)?;
        self.tokens[id].write(intent.sourceToken)?;
        self.source_zones[id].write(intent.sourceZone)?;
        self.senders[id].write(intent.sender)?;
        self.recipients[id].write(intent.recipient)?;
        self.refund_accounts[id].write(intent.refundAccount)?;
        self.reimbursement_accounts[id].write(intent.reimbursementAccount)?;
        self.pools[id].write(intent.pool)?;
        self.principals[id].write(intent.principal)?;
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
        intent: Intent,
        intent_hash: B256,
        reject_reason: Option<u8>,
    ) -> ZoneResult<u8> {
        Self::validate_intent(&intent, intent_hash)?;
        let state = self.require_same_intent(intent.transferId, intent_hash)?;
        if state != STATE_NONE {
            return Ok(state);
        }
        self.check_account(l1, intent.recipient)?;
        let operator = self.pool_operators[intent.destinationToken].read()?;
        if operator.is_zero() || operator != intent.pool {
            return self.record_rejection(&intent, intent_hash, 1);
        }
        if let Some(reason) = reject_reason {
            return self.record_rejection(&intent, intent_hash, reason);
        }

        let balance = self.pool_balances[intent.destinationToken].read()?;
        let reserve = self.pool_minimum_reserves[intent.destinationToken].read()?;
        let spendable = balance.saturating_sub(reserve);
        let exposure =
            self.unsettled_exposure[intent.destinationToken][intent.sourceZone].read()?;
        let limit = self.exposure_limits[intent.destinationToken][intent.sourceZone].read()?;
        let next_exposure = exposure
            .checked_add(intent.principal)
            .ok_or_else(|| FastTransferError::arithmetic_overflow())?;
        if intent.principal > spendable || limit == 0 || next_exposure > limit {
            return self.record_rejection(&intent, intent_hash, 2);
        }

        self.transfer_out(intent.destinationToken, intent.recipient, intent.principal)?;
        self.write_intent(&intent, intent_hash, intent.principal)?;
        self.pool_balances[intent.destinationToken].write(balance - intent.principal)?;
        self.unsettled_exposure[intent.destinationToken][intent.sourceZone].write(next_exposure)?;
        self.states[intent.transferId].write(STATE_PAID)?;
        self.emit_event(FastTransferEvent::paid(
            intent.transferId,
            intent_hash,
            intent.recipient,
            intent.destinationToken,
            intent.principal,
        ))?;
        Ok(STATE_PAID)
    }

    #[allow(
        dead_code,
        reason = "used exclusively by the consensus-gated destination resolution seam"
    )]
    fn record_rejection(
        &mut self,
        intent: &Intent,
        intent_hash: B256,
        reason: u8,
    ) -> ZoneResult<u8> {
        self.write_intent(intent, intent_hash, intent.principal)?;
        self.rejection_reasons[intent.transferId].write(reason)?;
        self.states[intent.transferId].write(STATE_REJECTED)?;
        self.emit_event(FastTransferEvent::rejected(
            intent.transferId,
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
            .ok_or_else(|| FastTransferError::arithmetic_overflow())?;
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

    /// Applies already-verified finalized source receipt and ancestry evidence exactly once.
    #[allow(
        dead_code,
        reason = "consensus-gated ancestry verifier calls this seam after fast-fork activation"
    )]
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
            .ok_or_else(|| FastTransferError::invalid_retirement_evidence())?;
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
