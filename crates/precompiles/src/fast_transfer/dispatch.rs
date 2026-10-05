//! ABI dispatch for the dormant `FastTransfer` precompile.

use alloy_primitives::{Address, U256};
use revm::precompile::PrecompileResult;
use tempo_precompiles::{charge_input_cost, dispatch, storage::Handler, view};
use tempo_zone_contracts::{FastTransferError, IFastTransfer};

use crate::{
    dispatch::{mutate, view as typed_view},
    storage::{L1State, L1StorageReader},
};

use super::{FastTransfer, MAX_CERTIFICATE_BYTES, MAX_RETIREMENT_PROOF_BYTES};

impl FastTransfer {
    pub(crate) fn call<P: L1StorageReader>(
        &mut self,
        l1: &L1State<P>,
        calldata: &[u8],
        caller: Address,
    ) -> PrecompileResult {
        if let Some(err) = charge_input_cost(&mut self.storage, calldata) {
            return err;
        }

        dispatch!(calldata, |call| match call {
            IFastTransfer::IFastTransferCalls {
                MAX_CERTIFICATE_BYTES(call) => view(call, |_| Ok(U256::from(MAX_CERTIFICATE_BYTES))),
                MAX_RETIREMENT_PROOF_BYTES(call) => view(call, |_| Ok(U256::from(MAX_RETIREMENT_PROOF_BYTES))),
                status(call) => typed_view(call, |call| {
                    self.ensure_active()?;
                    self.status(call.transferId)
                }),
                poolState(call) => typed_view(call, |call| {
                    self.ensure_active()?;
                    self.pool_state(call.token)
                }),
                exposure(call) => typed_view(call, |call| {
                    self.ensure_active()?;
                    Ok::<_, crate::ZonePrecompileError>(IFastTransfer::exposureReturn {
                        unsettled: self.unsettled_exposure[call.token][call.sourceZone].read()?,
                        limit: self.exposure_limits[call.token][call.sourceZone].read()?,
                    })
                }),
                lock(call) => mutate(call, caller, |sender, call| {
                    self.ensure_active()?;
                    self.lock_verified(l1, sender, call.intent, call.intentHash)
                }),
                resolve(call) => mutate(call, caller, |_sender, call| {
                    self.ensure_active()?;
                    // Certificate verification remains unreachable until the finalized fast-epoch
                    // registry is available to this precompile. Never treat calldata as verified.
                    let _ = (call.intent, call.intentHash, call.lockCertificate, call.cancel, call.rejectionReason);
                    Err::<u8, crate::ZonePrecompileError>(
                        FastTransferError::invalid_certificate().into(),
                    )
                }),
                recordOutcome(call) => mutate(call, caller, |_sender, call| {
                    self.ensure_active()?;
                    let _ = (call.intent, call.intentHash, call.outcomeCertificate);
                    Err::<u8, crate::ZonePrecompileError>(
                        FastTransferError::invalid_certificate().into(),
                    )
                }),
                disposeEscrow(call) => mutate(call, caller, |_sender, call| {
                    self.ensure_active()?;
                    self.dispose_escrow_verified(l1, call.transferId)
                }),
                fundPool(call) => mutate(call, caller, |sender, call| {
                    self.ensure_active()?;
                    self.fund_pool_verified(l1, sender, call.token, call.amount, call.minimumReserve)
                }),
                withdrawPool(call) => mutate(call, caller, |sender, call| {
                    self.ensure_active()?;
                    self.withdraw_pool_verified(l1, sender, call.token, call.recipient, call.amount)
                }),
                setExposureLimit(call) => mutate(call, caller, |sender, call| {
                    self.ensure_active()?;
                    self.set_exposure_limit_verified(sender, call.token, call.sourceZone, call.limit)
                }),
                retireExposure(call) => mutate(call, caller, |_sender, call| {
                    self.ensure_active()?;
                    let proof_len = call.evidence.receiptProof.len().saturating_add(call.evidence.headerChain.len());
                    if proof_len > MAX_RETIREMENT_PROOF_BYTES {
                        return Err(FastTransferError::retirement_proof_too_large().into());
                    }
                    // As above, finalized receipt inclusion and ancestry verification needs the
                    // source Portal commitment seam. The unverified ABI payload has no effects.
                    Err::<(), crate::ZonePrecompileError>(
                        FastTransferError::invalid_retirement_evidence().into(),
                    )
                }),
            }
        })
    }
}
