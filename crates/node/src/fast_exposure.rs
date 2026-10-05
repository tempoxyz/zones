//! Automatic accepted-source-release proof construction and native exposure retirement.
//!
//! A source disposition certificate is only a durable trigger. Capacity is released after the
//! exact successful source receipt is proven into the source Portal prefix read at this node's
//! hash-canonical imported Tempo anchor and `retireExposure` commits through the local Raft path.

use std::{collections::BTreeMap, future::IntoFuture, sync::Arc, time::Duration};

use alloy_consensus::{BlockHeader as _, Sealable as _};
use alloy_eips::{BlockId, NumHash, eip2718::Encodable2718 as _};
use alloy_network::{ReceiptResponse as _, primitives::HeaderResponse as _};
use alloy_primitives::{Address, B256, TxKind, U256, keccak256};
use alloy_provider::{DynProvider, Provider as _, ProviderBuilder};
use alloy_rlp::Decodable as _;
use alloy_sol_types::SolCall as _;
use futures::{StreamExt as _, stream::FuturesUnordered};
use reth_storage_api::{BlockNumReader, BlockReader, HeaderProvider, ReceiptProvider};
use reth_trie_common::{HashBuilder, Nibbles, proof::ProofRetainer};
use tempo_alloy::{TempoNetwork, rpc::TempoTransactionReceipt};
use tempo_primitives::{Block, TempoHeader, TempoReceipt};
use tempo_zone_contracts::{FAST_TRANSFER_ADDRESS, IFastTransfer, ZonePortal};
use tokio::sync::{Semaphore, watch};
use tokio_util::sync::CancellationToken;
use zone_primitives::fast_transfer::{
    CanonicalEncode as _, ExposureRetirementEvidence, HeaderAncestryProof, MAX_RETIREMENT_HEADERS,
    MAX_RETIREMENT_PROOF_BYTES, OutcomeCertificate, ReceiptInclusionProof, TransferIntent,
    TransferOutcome, ZoneDomain,
};

use crate::{
    fast_execution::CanonicalFastExecution, fast_raft_state_machine::CommittedStateHandle,
    fast_service::FastServiceError, fast_service_adapters::FastServiceHandle,
};

const EXPOSURE_RETIREMENT_CONCURRENCY: usize = 64;
const EXPOSURE_RETIREMENT_CONCURRENCY_PER_SOURCE: usize = 8;
const MAX_ANCESTRY_HEADERS: usize = 1_000_000;

const fn adjust_index_for_rlp(index: usize, len: usize) -> usize {
    if index > 0x7f {
        index
    } else if index == 0x7f || index + 1 == len {
        0
    } else {
        index + 1
    }
}

/// One explicitly configured authenticated read-only source Zone provider.
#[derive(Clone)]
pub struct SourceExposureProofProvider {
    pub source_zone_id: u32,
    pub source_chain_id: u64,
    pub source_portal: Address,
    provider: DynProvider<TempoNetwork>,
}

impl SourceExposureProofProvider {
    /// Connect an explicit endpoint and authenticate its chain identity. Endpoints are never
    /// derived from peer transport addresses or guessed from the Zone ID.
    pub async fn connect(
        source_zone_id: u32,
        source_chain_id: u64,
        source_portal: Address,
        rpc_endpoint: &str,
    ) -> Result<Self, FastExposureError> {
        if source_zone_id == 0
            || source_chain_id == 0
            || source_portal.is_zero()
            || rpc_endpoint.is_empty()
        {
            return Err(FastExposureError::InvalidConfiguration);
        }
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(rpc_endpoint)
            .await
            .map_err(provider_error)?
            .erased();
        if provider.get_chain_id().await.map_err(provider_error)? != source_chain_id {
            return Err(FastExposureError::InvalidConfiguration);
        }
        Ok(Self {
            source_zone_id,
            source_chain_id,
            source_portal,
            provider,
        })
    }
}

/// Complete real resources required by the recovery driver.
pub struct FastExposureRetirementResources {
    pub source_providers: BTreeMap<u32, SourceExposureProofProvider>,
    pub tempo_l1: DynProvider<TempoNetwork>,
    /// Current finalized Tempo block actually imported by local committed execution.
    pub imported_anchor: watch::Receiver<Option<NumHash>>,
    pub poll_interval: Duration,
    pub rpc_timeout: Duration,
    pub shutdown: CancellationToken,
}

impl FastExposureRetirementResources {
    async fn validate(&self, service: &FastServiceHandle) -> Result<(), FastExposureError> {
        if self.poll_interval.is_zero()
            || self.rpc_timeout.is_zero()
            || self.source_providers.len() != 9
        {
            return Err(FastExposureError::InvalidConfiguration);
        }
        let routes = service.exposure_routes();
        if routes.len() != 9
            || routes.keys().copied().collect::<Vec<_>>()
                != self.source_providers.keys().copied().collect::<Vec<_>>()
        {
            return Err(FastExposureError::InvalidConfiguration);
        }
        let expected_l1 = routes
            .values()
            .next()
            .ok_or(FastExposureError::InvalidConfiguration)?
            .l1_chain_id;
        if provider_with_timeout(self.rpc_timeout, self.tempo_l1.get_chain_id()).await?
            != expected_l1
        {
            return Err(FastExposureError::InvalidConfiguration);
        }
        for (zone, domain) in routes {
            let provider = self
                .source_providers
                .get(&zone)
                .ok_or(FastExposureError::InvalidConfiguration)?;
            validate_provider_route(provider, domain)?;
        }
        Ok(())
    }
}

/// Proof construction or retirement failure. Transient provider/L1 failures remain queued and
/// are retried; none of these errors authorizes a refund or a synthetic retirement.
#[derive(Debug, thiserror::Error)]
pub enum FastExposureError {
    #[error("invalid exposure-retirement configuration")]
    InvalidConfiguration,
    #[error("source release is not yet in the accepted source prefix")]
    ReleaseNotAccepted,
    #[error("invalid source release proof: {0}")]
    InvalidProof(&'static str),
    #[error("provider request unavailable")]
    Provider,
    #[error(transparent)]
    Service(#[from] FastServiceError),
}

/// Run the automatic retry loop. Source or L1 outages leave the paid claim reserved and are
/// retried after `poll_interval`; cancellation only stops the worker and never changes token state.
pub async fn run_fast_exposure_retirement<P>(
    service: FastServiceHandle,
    committed: CommittedStateHandle<CanonicalFastExecution<P>>,
    resources: FastExposureRetirementResources,
) -> Result<(), FastExposureError>
where
    P: BlockNumReader
        + BlockReader<Block = Block>
        + HeaderProvider<Header = TempoHeader>
        + ReceiptProvider<Receipt = TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    resources.validate(&service).await?;
    loop {
        if resources.shutdown.is_cancelled() {
            return Ok(());
        }
        let candidates = service.exposure_retirement_candidates()?;
        service.set_exposure_recovery_backlog(candidates.len() >= EXPOSURE_RETIREMENT_CONCURRENCY);
        let anchor = *resources.imported_anchor.borrow();
        if let Some(anchor) = anchor {
            let global = Arc::new(Semaphore::new(EXPOSURE_RETIREMENT_CONCURRENCY));
            let per_source = resources
                .source_providers
                .keys()
                .map(|source| {
                    (
                        *source,
                        Arc::new(Semaphore::new(EXPOSURE_RETIREMENT_CONCURRENCY_PER_SOURCE)),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            let mut retirements = FuturesUnordered::new();
            for (intent, release) in &candidates {
                let source = per_source
                    .get(&intent.source.zone_id)
                    .cloned()
                    .ok_or(FastExposureError::InvalidConfiguration)?;
                let global = global.clone();
                let service = &service;
                let committed = &committed;
                let resources = &resources;
                retirements.push(async move {
                    let _global = global
                        .acquire_owned()
                        .await
                        .map_err(|_| FastExposureError::InvalidConfiguration)?;
                    let _source = source
                        .acquire_owned()
                        .await
                        .map_err(|_| FastExposureError::InvalidConfiguration)?;
                    let result =
                        retire_one(&service, &committed, &resources, anchor, intent, release).await;
                    Ok::<_, FastExposureError>((intent.transfer_id(), result))
                });
            }
            loop {
                tokio::select! {
                    () = resources.shutdown.cancelled() => return Ok(()),
                    result = retirements.next(), if !retirements.is_empty() => {
                        let Some(result) = result else { break };
                        let (transfer_id, result) = result?;
                        if let Err(_error) = result {
                            tracing::warn!(
                                target: "zone::fast::exposure",
                                %transfer_id,
                                "accepted-release exposure retirement remains queued"
                            );
                        }
                    }
                    else => break,
                }
            }
        }
        tokio::select! {
            () = resources.shutdown.cancelled() => return Ok(()),
            () = tokio::time::sleep(resources.poll_interval) => {}
        }
    }
}

async fn retire_one<P>(
    service: &FastServiceHandle,
    committed: &CommittedStateHandle<CanonicalFastExecution<P>>,
    resources: &FastExposureRetirementResources,
    imported_anchor: NumHash,
    intent: &TransferIntent,
    release: &OutcomeCertificate,
) -> Result<(), FastExposureError>
where
    P: BlockNumReader
        + BlockReader<Block = Block>
        + HeaderProvider<Header = TempoHeader>
        + ReceiptProvider<Receipt = TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    validate_release_certificate(intent, release)?;
    let source = resources
        .source_providers
        .get(&intent.source.zone_id)
        .ok_or(FastExposureError::InvalidConfiguration)?;
    validate_provider_route(source, intent.source)?;

    let accepted_hash = provider_with_timeout(
        resources.rpc_timeout,
        ZonePortal::new(source.source_portal, &resources.tempo_l1)
            .blockHash()
            .block(BlockId::hash_canonical(imported_anchor.hash))
            .call(),
    )
    .await?;
    if accepted_hash.is_zero() {
        return Err(FastExposureError::ReleaseNotAccepted);
    }

    let (receipt_proof, release_header) =
        source_receipt_proof(source, intent, release, resources.rpc_timeout).await?;
    let headers = source_header_ancestry(
        service,
        source,
        release_header,
        accepted_hash,
        resources.rpc_timeout,
    )
    .await?;
    let header_chain = HeaderAncestryProof { headers };
    ensure_chunk_bound(&header_chain)?;
    let authenticated_descendant = decode_header(
        header_chain
            .headers
            .last()
            .ok_or(FastExposureError::InvalidProof("empty final ancestry"))?,
    )?
    .hash_slow();
    let evidence = ExposureRetirementEvidence {
        transfer_id: intent.transfer_id(),
        intent_hash: intent.intent_hash(),
        accepted_source_block_hash: authenticated_descendant,
        release_receipt_hash: keccak256(&receipt_proof.receipt),
        destination_token: intent.asset.destination_token,
        beneficiary: intent.reimbursement_account,
        principal: intent.principal,
        receipt_proof: receipt_proof.canonical_bytes(),
        header_chain: header_chain.canonical_bytes(),
    };
    if evidence.canonical_bytes().len() > MAX_RETIREMENT_PROOF_BYTES {
        return Err(FastExposureError::InvalidProof(
            "canonical retirement evidence exceeds 512 KiB",
        ));
    }
    let submission = service
        .submit_exposure_retirement(committed, evidence, intent.source.domain_hash())
        .await?;
    service.complete_exposure_retirement(intent.transfer_id(), submission)?;
    Ok(())
}

fn validate_provider_route(
    provider: &SourceExposureProofProvider,
    domain: ZoneDomain,
) -> Result<(), FastExposureError> {
    if provider.source_zone_id != domain.zone_id
        || provider.source_chain_id != domain.chain_id
        || provider.source_portal != domain.portal
    {
        return Err(FastExposureError::InvalidConfiguration);
    }
    Ok(())
}

fn validate_release_certificate(
    intent: &TransferIntent,
    certificate: &OutcomeCertificate,
) -> Result<(), FastExposureError> {
    let total = intent
        .principal
        .checked_add(intent.fee)
        .ok_or(FastExposureError::InvalidProof(
            "source escrow amount overflow",
        ))?;
    if certificate.body.transfer_id != intent.transfer_id()
        || certificate.body.intent_hash != intent.intent_hash()
        || certificate.body.zone != intent.source
        || certificate.body.block_hash.is_zero()
        || certificate.body.block_height == 0
        || certificate.body.transaction_hash.is_zero()
        || !matches!(
            certificate.body.outcome,
            TransferOutcome::Released { beneficiary, amount }
                if beneficiary == intent.reimbursement_account && amount == total
        )
    {
        return Err(FastExposureError::InvalidProof(
            "source release certificate/body mismatch",
        ));
    }
    Ok(())
}

async fn source_receipt_proof(
    source: &SourceExposureProofProvider,
    intent: &TransferIntent,
    release: &OutcomeCertificate,
    rpc_timeout: Duration,
) -> Result<(ReceiptInclusionProof, Vec<u8>), FastExposureError> {
    let transaction = provider_with_timeout(
        rpc_timeout,
        source
            .provider
            .get_transaction_by_hash(release.body.transaction_hash),
    )
    .await?
    .ok_or(FastExposureError::ReleaseNotAccepted)?;
    let calls = transaction.inner.calls().collect::<Vec<_>>();
    let exact_disposition = calls.len() == 1
        && calls[0].0 == TxKind::Call(FAST_TRANSFER_ADDRESS)
        && IFastTransfer::disposeEscrowCall::abi_decode(calls[0].1.as_ref())
            .is_ok_and(|call| call.transferId == intent.transfer_id());
    if transaction.block_hash != Some(release.body.block_hash)
        || transaction.block_number != Some(release.body.block_height)
        || !exact_disposition
    {
        return Err(FastExposureError::InvalidProof(
            "release transaction target, calldata, or block identity mismatch",
        ));
    }
    let receipt = provider_with_timeout(
        rpc_timeout,
        source
            .provider
            .get_transaction_receipt(release.body.transaction_hash),
    )
    .await?
    .ok_or(FastExposureError::ReleaseNotAccepted)?;
    if !receipt.status()
        || receipt.block_hash() != Some(release.body.block_hash)
        || receipt.block_number() != Some(release.body.block_height)
    {
        return Err(FastExposureError::InvalidProof(
            "release receipt status or block identity mismatch",
        ));
    }
    validate_escrow_disposed_log(&receipt, intent)?;
    let index = receipt
        .transaction_index()
        .ok_or(FastExposureError::InvalidProof(
            "receipt has no transaction index",
        ))?;
    let block = provider_with_timeout(
        rpc_timeout,
        source.provider.get_block_by_hash(release.body.block_hash),
    )
    .await?
    .ok_or(FastExposureError::ReleaseNotAccepted)?;
    let header = block.header.as_ref().clone();
    if block.header.hash() != release.body.block_hash
        || header.hash_slow() != release.body.block_hash
        || header.number() != release.body.block_height
    {
        return Err(FastExposureError::InvalidProof("release header mismatch"));
    }
    let receipts = provider_with_timeout(
        rpc_timeout,
        source
            .provider
            .get_block_receipts(BlockId::hash_canonical(release.body.block_hash)),
    )
    .await?
    .ok_or(FastExposureError::ReleaseNotAccepted)?;
    let encoded = canonical_receipts(receipts)?;
    let target = usize::try_from(index)
        .map_err(|_| FastExposureError::InvalidProof("receipt index overflow"))?;
    if target >= encoded.len() {
        return Err(FastExposureError::InvalidProof(
            "receipt index out of bounds",
        ));
    }
    let target_key = alloy_rlp::encode(index);
    let target_nibbles = Nibbles::unpack(&target_key);
    let mut builder: HashBuilder =
        HashBuilder::default().with_proof_retainer(ProofRetainer::new(vec![target_nibbles]));
    for insertion in 0..encoded.len() {
        let receipt_index = adjust_index_for_rlp(insertion, encoded.len());
        let key = alloy_rlp::encode(receipt_index);
        builder.add_leaf(Nibbles::unpack(&key), &encoded[receipt_index]);
    }
    let root = builder.root();
    if root != header.receipts_root() {
        return Err(FastExposureError::InvalidProof(
            "complete block receipts do not match the source header root",
        ));
    }
    let nodes = builder
        .take_proof_nodes()
        .matching_nodes_sorted(&target_nibbles)
        .into_iter()
        .map(|(_, node)| node.to_vec())
        .collect();
    let proof = ReceiptInclusionProof {
        transaction_index: index,
        receipt: encoded[target].clone(),
        nodes,
    };
    if keccak256(&proof.receipt) != keccak256(canonical_receipt(&receipt)) {
        return Err(FastExposureError::InvalidProof(
            "release transaction receipt differs from its block receipt",
        ));
    }
    Ok((proof, alloy_rlp::encode(header)))
}

fn canonical_receipts(
    mut receipts: Vec<TempoTransactionReceipt>,
) -> Result<Vec<Vec<u8>>, FastExposureError> {
    receipts.sort_by_key(|receipt| receipt.transaction_index());
    receipts
        .iter()
        .enumerate()
        .map(|(expected, receipt)| {
            if receipt.transaction_index() != Some(expected as u64) {
                return Err(FastExposureError::InvalidProof(
                    "block receipts are missing or duplicated",
                ));
            }
            Ok(canonical_receipt(receipt))
        })
        .collect()
}

fn canonical_receipt(receipt: &TempoTransactionReceipt) -> Vec<u8> {
    receipt
        .inner
        .inner
        .clone()
        .map_receipt(|receipt| receipt.map_logs(|log| log.into_inner()))
        .encoded_2718()
}

fn validate_escrow_disposed_log(
    receipt: &TempoTransactionReceipt,
    intent: &TransferIntent,
) -> Result<(), FastExposureError> {
    let topic = keccak256("EscrowDisposed(bytes32,uint8,address,uint128)");
    let beneficiary = B256::left_padding_from(intent.reimbursement_account.as_slice());
    let total = intent
        .principal
        .checked_add(intent.fee)
        .ok_or(FastExposureError::InvalidProof(
            "source escrow amount overflow",
        ))?;
    let matches = receipt
        .inner
        .inner
        .receipt
        .logs
        .iter()
        .filter(|log| {
            let topics = log.inner.data.topics();
            let data = log.inner.data.data.as_ref();
            log.inner.address == FAST_TRANSFER_ADDRESS
                && topics == [topic, intent.transfer_id(), beneficiary]
                && data.len() == 64
                && data[..31].iter().all(|byte| *byte == 0)
                && data[31] == 1
                && U256::from_be_slice(&data[32..]) == total
        })
        .count();
    if matches != 1 {
        return Err(FastExposureError::InvalidProof(
            "receipt lacks the exact paid EscrowDisposed effect",
        ));
    }
    Ok(())
}

async fn source_header_ancestry(
    service: &FastServiceHandle,
    source: &SourceExposureProofProvider,
    release_header: Vec<u8>,
    accepted_hash: B256,
    rpc_timeout: Duration,
) -> Result<Vec<Vec<u8>>, FastExposureError> {
    let release = decode_header(&release_header)?;
    let release_hash = release.hash_slow();
    let mut descending = Vec::with_capacity(MAX_RETIREMENT_HEADERS);
    let mut next_hash = accepted_hash;
    let mut traversed = 0usize;
    loop {
        if traversed >= MAX_ANCESTRY_HEADERS {
            return Err(FastExposureError::InvalidProof(
                "accepted ancestry exceeds retained bound",
            ));
        }
        let block =
            provider_with_timeout(rpc_timeout, source.provider.get_block_by_hash(next_hash))
                .await?
                .ok_or(FastExposureError::ReleaseNotAccepted)?;
        let header = block.header.as_ref().clone();
        if block.header.hash() != next_hash || header.hash_slow() != next_hash {
            return Err(FastExposureError::InvalidProof(
                "source header hash mismatch",
            ));
        }
        traversed += 1;
        descending.push(alloy_rlp::encode(header.clone()));
        if next_hash == release_hash {
            break;
        }
        if header.number() <= release.number() {
            return Err(FastExposureError::ReleaseNotAccepted);
        }
        next_hash = header.parent_hash();
        if descending.len() == MAX_RETIREMENT_HEADERS {
            let oldest_authenticated = descending
                .last()
                .cloned()
                .ok_or(FastExposureError::InvalidProof("empty ancestry chunk"))?;
            let mut checkpoint_headers = descending;
            checkpoint_headers.reverse();
            let checkpoint = HeaderAncestryProof {
                headers: checkpoint_headers,
            };
            ensure_chunk_bound(&checkpoint)?;
            service
                .submit_ancestry_checkpoint(source.source_portal, checkpoint)
                .await?;
            descending = Vec::with_capacity(MAX_RETIREMENT_HEADERS);
            descending.push(oldest_authenticated);
        }
    }
    descending.reverse();
    Ok(descending)
}

fn decode_header(encoded: &[u8]) -> Result<TempoHeader, FastExposureError> {
    let mut input = encoded;
    let header = TempoHeader::decode(&mut input)
        .map_err(|_| FastExposureError::InvalidProof("source header RLP"))?;
    if !input.is_empty() {
        return Err(FastExposureError::InvalidProof(
            "trailing source header RLP",
        ));
    }
    Ok(header)
}

fn ensure_chunk_bound(proof: &HeaderAncestryProof) -> Result<(), FastExposureError> {
    let size = proof.canonical_bytes().len();
    if proof.headers.is_empty()
        || proof.headers.len() > MAX_RETIREMENT_HEADERS
        || size > MAX_RETIREMENT_PROOF_BYTES
    {
        return Err(FastExposureError::InvalidProof(
            "ancestry chunk exceeds 256 headers or 512 KiB",
        ));
    }
    Ok(())
}

fn provider_error(_error: impl ToString) -> FastExposureError {
    FastExposureError::Provider
}

async fn provider_with_timeout<T, E>(
    timeout: Duration,
    future: impl IntoFuture<Output = Result<T, E>>,
) -> Result<T, FastExposureError>
where
    E: ToString,
{
    tokio::time::timeout(timeout, future.into_future())
        .await
        .map_err(|_| FastExposureError::Provider)?
        .map_err(provider_error)
}

#[cfg(test)]
mod tests {
    use std::{future::Future, sync::Arc, time::Duration};

    use alloy_primitives::{Address, B256, U256};
    use futures::{StreamExt as _, stream::FuturesUnordered};
    use tokio::sync::{Semaphore, oneshot};
    use zone_primitives::fast_transfer::{
        AssetId, CertificateBody, OutcomeCertificate, SignatureBytes, TransferIntent,
        TransferOutcome, ZoneDomain,
    };

    use super::{
        EXPOSURE_RETIREMENT_CONCURRENCY, EXPOSURE_RETIREMENT_CONCURRENCY_PER_SOURCE,
        FastExposureError, provider_with_timeout, validate_release_certificate,
    };

    fn domain(zone_id: u32, byte: u8) -> ZoneDomain {
        ZoneDomain {
            l1_chain_id: 1,
            zone_id,
            chain_id: 1_000 + u64::from(zone_id),
            portal: Address::repeat_byte(byte),
            authority_epoch: 7,
            roster_hash: B256::repeat_byte(byte),
            protocol_version: 1,
        }
    }

    fn intent() -> TransferIntent {
        TransferIntent {
            source: domain(1, 1),
            destination: domain(2, 2),
            asset: AssetId {
                l1_token: Address::repeat_byte(3),
                source_token: Address::repeat_byte(4),
                destination_token: Address::repeat_byte(5),
                decimals: 6,
            },
            sender: Address::repeat_byte(6),
            recipient: Address::repeat_byte(7),
            refund_account: Address::repeat_byte(6),
            destination_pool: Address::repeat_byte(8),
            reimbursement_account: Address::repeat_byte(9),
            principal: U256::from(100),
            fee: U256::from(2),
            quote_id: B256::repeat_byte(10),
            destination_expiry_height: 100,
            transfer_nonce: 11,
        }
    }

    fn release(intent: &TransferIntent) -> OutcomeCertificate {
        OutcomeCertificate {
            body: CertificateBody {
                transfer_id: intent.transfer_id(),
                intent_hash: intent.intent_hash(),
                zone: intent.source,
                log_term: 1,
                log_index: 2,
                block_height: 3,
                block_hash: B256::repeat_byte(12),
                state_root: B256::repeat_byte(13),
                transaction_hash: B256::repeat_byte(14),
                outcome: TransferOutcome::Released {
                    beneficiary: intent.reimbursement_account,
                    amount: intent.principal + intent.fee,
                },
            },
            signatures: [SignatureBytes([15; 65]), SignatureBytes([16; 65])],
        }
    }

    #[test]
    fn only_exact_post_release_body_can_trigger_proof_io() {
        let intent = intent();
        let valid = release(&intent);
        assert!(validate_release_certificate(&intent, &valid).is_ok());

        let mut forged = valid.clone();
        forged.body.transaction_hash = B256::ZERO;
        assert!(validate_release_certificate(&intent, &forged).is_err());

        let mut wrong_beneficiary = valid.clone();
        wrong_beneficiary.body.outcome = TransferOutcome::Released {
            beneficiary: Address::repeat_byte(99),
            amount: intent.principal + intent.fee,
        };
        assert!(validate_release_certificate(&intent, &wrong_beneficiary).is_err());

        let mut pre_release = valid;
        pre_release.body.outcome = TransferOutcome::Paid {
            pool: intent.destination_pool,
            recipient: intent.recipient,
            principal: intent.principal,
        };
        assert!(validate_release_certificate(&intent, &pre_release).is_err());
    }

    #[tokio::test]
    async fn unavailable_provider_is_retryable_without_producing_evidence() {
        let result = provider_with_timeout(std::time::Duration::from_millis(1), async {
            std::future::pending::<Result<(), &'static str>>().await
        })
        .await;
        assert!(matches!(result, Err(FastExposureError::Provider)));
    }

    #[tokio::test]
    async fn stalled_source_does_not_block_an_independent_retirement_route() {
        let global = Arc::new(Semaphore::new(EXPOSURE_RETIREMENT_CONCURRENCY));
        let source_a = Arc::new(Semaphore::new(EXPOSURE_RETIREMENT_CONCURRENCY_PER_SOURCE));
        let source_b = Arc::new(Semaphore::new(EXPOSURE_RETIREMENT_CONCURRENCY_PER_SOURCE));
        let (release, stalled) = oneshot::channel::<()>();
        let mut work: FuturesUnordered<std::pin::Pin<Box<dyn Future<Output = u32> + Send + '_>>> =
            FuturesUnordered::new();
        let failed_global = global.clone();
        work.push(Box::pin(async move {
            let _global = failed_global.acquire_owned().await.unwrap();
            let _source = source_a.acquire_owned().await.unwrap();
            let _ = stalled.await;
            1
        }));
        work.push(Box::pin(async move {
            let _global = global.acquire_owned().await.unwrap();
            let _source = source_b.acquire_owned().await.unwrap();
            2
        }));

        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), work.next())
                .await
                .unwrap(),
            Some(2)
        );
        let _ = release.send(());
        assert_eq!(work.next().await, Some(1));
    }
}
