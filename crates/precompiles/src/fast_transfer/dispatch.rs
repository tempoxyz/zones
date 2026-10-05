//! ABI dispatch for the T14 `FastTransfer` precompile.

use alloy_consensus::crypto::secp256k1::recover_signer;
use alloy_primitives::{Address, B256, Signature, U256};
use revm::precompile::PrecompileResult;
use tempo_precompiles::{charge_input_cost, dispatch, storage::Handler, view};
use tempo_zone_contracts::{FastTransferError, IFastTransfer};
use zone_primitives::fast_transfer::{
    CancellationRequest, ExposureRetirementEvidence, OutcomeCertificate, QuoteCertificate,
    RejectionReason, TransferIntent, TransferOutcome,
};

use crate::{
    dispatch::{mutate, view as typed_view},
    storage::{L1State, L1StorageReader},
};

use super::{
    EpochUse, FastTransfer, MAX_BARRIER_PROOF_BYTES, MAX_CERTIFICATE_BYTES,
    MAX_RETIREMENT_PROOF_BYTES, barrier::BarrierInclusionProof,
};
use crate::ZoneOutbox;

impl FastTransfer {
    pub(crate) fn call<P: L1StorageReader>(
        &mut self,
        l1: &L1State<P>,
        calldata: &[u8],
        caller: Address,
        tx_hash: B256,
        fee_payer: Address,
    ) -> PrecompileResult {
        if let Some(err) = charge_input_cost(&mut self.storage, calldata) {
            return err;
        }

        dispatch!(calldata, |call| match call {
            IFastTransfer::IFastTransferCalls {
                MAX_CERTIFICATE_BYTES(call) => view(call, |_| Ok(U256::from(MAX_CERTIFICATE_BYTES))),
                MAX_RETIREMENT_PROOF_BYTES(call) => view(call, |_| Ok(U256::from(MAX_RETIREMENT_PROOF_BYTES))),
                status(call) => typed_view(call, |call| self.status(call.transferId)),
                poolState(call) => typed_view(call, |call| self.pool_state(call.token)),
                inventoryJob(call) => typed_view(call, |call| self.inventory_job(call.jobId)),
                replenishmentRoute(call) => typed_view(call, |call| {
                    self.replenishment_treasury_operators[call.token][call.treasury].read()
                }),
                replenishmentCredit(call) => typed_view(call, |call| {
                    self.replenishment_credits[call.jobId].read()
                }),
                importedBarrier(call) => typed_view(call, |call| {
                    self.imported_barrier(
                        call.destinationEpoch,
                        call.sourcePortal,
                        call.sourceEpoch,
                    )
                }),
                exposure(call) => typed_view(call, |call| {
                    Ok::<_, crate::ZonePrecompileError>(IFastTransfer::exposureReturn {
                        unsettled: self.unsettled_exposure[call.token][call.sourceZone].read()?,
                        limit: self.exposure_limits[call.token][call.sourceZone].read()?,
                    })
                }),
                lock(call) => mutate(call, caller, |sender, call| {
                    let intent = TransferIntent::decode(&call.canonicalIntent)
                        .map_err(|_| FastTransferError::invalid_intent())?;
                    if intent.source.portal != l1.portal() {
                        return Err(FastTransferError::unauthorized().into());
                    }
                    Self::epoch_verifier(l1, intent.source, EpochUse::Admission)?;
                    let (destination, _) =
                        Self::epoch_verifier(l1, intent.destination, EpochUse::Admission)?;
                    let quote = QuoteCertificate::decode(&call.quoteCertificate)
                        .map_err(|_| FastTransferError::invalid_certificate())?;
                    destination
                        .verify_quote(&quote)
                        .map_err(|_| FastTransferError::invalid_certificate())?;
                    let body = &quote.quote;
                    if body.source != intent.source
                        || body.destination != intent.destination
                        || body.asset != intent.asset
                        || body.quote_id != intent.quote_id
                        || body.fee != intent.fee
                        || body.maximum_principal < intent.principal
                        || body.expiry_height != intent.destination_expiry_height
                        || body.destination_pool != intent.destination_pool
                        || body.reimbursement_account != intent.reimbursement_account
                    {
                        return Err(FastTransferError::invalid_certificate().into());
                    }
                    self.lock_verified(l1, sender, intent)
                }),
                resolve(call) => mutate(call, caller, |_sender, call| {
                    let intent = TransferIntent::decode(&call.canonicalIntent)
                        .map_err(|_| FastTransferError::invalid_intent())?;
                    if intent.destination.portal != l1.portal() {
                        return Err(FastTransferError::unauthorized().into());
                    }
                    let (_, destination_config) = Self::epoch_verifier(
                        l1,
                        intent.destination,
                        EpochUse::DestinationResolution,
                    )?;
                    let (source, _) =
                        Self::epoch_verifier(l1, intent.source, EpochUse::Historical)?;
                    let lock = OutcomeCertificate::decode(&call.lockCertificate)
                        .map_err(|_| FastTransferError::invalid_certificate())?;
                    source
                        .verify_outcome(&lock, &intent)
                        .map_err(|_| FastTransferError::invalid_certificate())?;
                    if !matches!(lock.body.outcome, TransferOutcome::Locked { .. }) {
                        return Err(FastTransferError::invalid_certificate().into());
                    }
                    if destination_config.closed {
                        if call.barrierProof.is_empty()
                            || call.barrierProof.len() > MAX_BARRIER_PROOF_BYTES
                        {
                            return Err(FastTransferError::invalid_certificate().into());
                        }
                        let proof = BarrierInclusionProof::decode(&call.barrierProof)
                            .map_err(|_| FastTransferError::invalid_certificate())?;
                        Self::verify_closed_barrier(
                            l1,
                            &intent,
                            &lock,
                            &destination_config,
                            &proof,
                        )?;
                    } else if !call.barrierProof.is_empty() {
                        return Err(FastTransferError::invalid_certificate().into());
                    }
                    let rejection = if call.cancellation.is_empty() {
                        None
                    } else {
                        let cancellation = CancellationRequest::decode(&call.cancellation)
                            .map_err(|_| FastTransferError::invalid_certificate())?;
                        if cancellation.transfer_id != intent.transfer_id()
                            || cancellation.intent_hash != intent.intent_hash()
                            || cancellation.sender != intent.sender
                            || cancellation.source != intent.source
                        {
                            return Err(FastTransferError::invalid_certificate().into());
                        }
                        let signature = Signature::try_from(cancellation.signature.0.as_slice())
                            .map_err(|_| FastTransferError::invalid_certificate())?;
                        let signer = recover_signer(&signature, cancellation.request_hash())
                            .map_err(|_| FastTransferError::invalid_certificate())?;
                        if signer != intent.sender {
                            return Err(FastTransferError::unauthorized().into());
                        }
                        Some(RejectionReason::Cancelled as u8)
                    };
                    self.resolve_verified(l1, intent, rejection)
                }),
                recordOutcome(call) => mutate(call, caller, |_sender, call| {
                    let intent = TransferIntent::decode(&call.canonicalIntent)
                        .map_err(|_| FastTransferError::invalid_intent())?;
                    if intent.source.portal != l1.portal() {
                        return Err(FastTransferError::unauthorized().into());
                    }
                    let (destination, _) =
                        Self::epoch_verifier(l1, intent.destination, EpochUse::Historical)?;
                    let certificate = OutcomeCertificate::decode(&call.outcomeCertificate)
                        .map_err(|_| FastTransferError::invalid_certificate())?;
                    destination
                        .verify_outcome(&certificate, &intent)
                        .map_err(|_| FastTransferError::invalid_certificate())?;
                    let outcome = match certificate.body.outcome {
                        TransferOutcome::Paid { .. } => super::OUTCOME_PAID,
                        TransferOutcome::Rejected { .. } => super::OUTCOME_REJECTED,
                        _ => return Err(FastTransferError::invalid_certificate().into()),
                    };
                    self.record_outcome_verified(intent.transfer_id(), intent.intent_hash(), outcome)
                }),
                disposeEscrow(call) => mutate(call, caller, |_sender, call| {
                    self.dispose_escrow_verified(l1, call.transferId)
                }),
                fundPool(call) => mutate(call, caller, |sender, call| {
                    self.fund_pool_verified(l1, sender, call.token, call.amount, call.minimumReserve)
                }),
                withdrawPool(call) => mutate(call, caller, |sender, call| {
                    self.withdraw_pool_verified(l1, sender, call.token, call.recipient, call.amount)
                }),
                setExposureLimit(call) => mutate(call, caller, |sender, call| {
                    self.set_exposure_limit_verified(sender, call.token, call.sourceZone, call.limit)
                }),
                recordAncestryCheckpoint(call) => mutate(call, caller, |_sender, call| {
                    if call.headerChain.len() > MAX_RETIREMENT_PROOF_BYTES {
                        return Err(FastTransferError::retirement_proof_too_large().into());
                    }
                    self.record_ancestry_checkpoint_verified(
                        l1,
                        call.sourcePortal,
                        &call.headerChain,
                    )
                }),
                retireExposure(call) => mutate(call, caller, |_sender, call| {
                    if call.canonicalEvidence.len() > MAX_RETIREMENT_PROOF_BYTES {
                        return Err(FastTransferError::retirement_proof_too_large().into());
                    }
                    let evidence = ExposureRetirementEvidence::decode(&call.canonicalEvidence)
                        .map_err(|_| FastTransferError::invalid_retirement_evidence())?;
                    self.verify_retirement_evidence(l1, &evidence)?;
                    self.retire_exposure_verified(
                        evidence.transfer_id,
                        evidence.intent_hash,
                        evidence.destination_token,
                        evidence.beneficiary,
                        Self::amount_u128(evidence.principal)?,
                    )
                }),
                recordImportedBarrier(call) => mutate(call, caller, |_sender, call| {
                    self.record_imported_barrier_verified(
                        l1,
                        &call.canonicalInventory,
                        &call.sourceBarrierCertificate,
                    )
                }),
                configureReplenishmentRoute(call) => mutate(call, caller, |sender, call| {
                    self.configure_replenishment_route_verified(
                        l1,
                        sender,
                        call.token,
                        call.treasury,
                        call.enabled,
                    )
                }),
                allocateInventoryAndWithdraw(call) => mutate(call, caller, |sender, call| {
                    let mut outbox = ZoneOutbox::new();
                    self.allocate_inventory_and_withdraw_verified(
                        l1,
                        &mut outbox,
                        sender,
                        fee_payer,
                        tx_hash,
                        call.jobId,
                        call.transferIds,
                        call.token,
                        call.treasury,
                    )
                }),
            }
        })
    }
}
