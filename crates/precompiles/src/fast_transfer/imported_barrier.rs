//! Authenticated, economically inert commitment of a complete imported source barrier.

use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_sol_types::SolValue;
use tempo_precompiles::{storage::Handler, zone_factory::ZonePortalStorage};
use tempo_zone_contracts::{
    FastTransferError, FastTransferEvent, IFastTransfer, IMPORTED_BARRIER_CERTIFICATE_BYTES,
    IMPORTED_BARRIER_CERTIFICATE_VERSION,
};
use zone_fast_transfer::{EpochRoster, QuorumVerifier, drain::DrainCertificate};
use zone_primitives::{
    constants::decode_l1_chain_id,
    fast_transfer::{FastBarrierStatement, SignatureBytes, ZoneDomain},
};

use crate::{
    ZoneResult,
    storage::{L1State, L1StorageReader},
};

use super::{FastTransfer, read_epoch_registry};

impl FastTransfer {
    pub(super) fn imported_barrier_key(
        destination_epoch: u64,
        source_portal: Address,
        source_epoch: u64,
    ) -> B256 {
        keccak256(
            (
                keccak256("TEMPO_ZONE_FAST_IMPORTED_BARRIER_KEY_T14_V1"),
                U256::from(destination_epoch),
                source_portal,
                U256::from(source_epoch),
            )
                .abi_encode(),
        )
    }

    pub(super) fn imported_barrier(
        &self,
        destination_epoch: u64,
        source_portal: Address,
        source_epoch: u64,
    ) -> ZoneResult<B256> {
        Ok(self.imported_barriers
            [Self::imported_barrier_key(destination_epoch, source_portal, source_epoch)]
        .read()?)
    }

    fn store_imported_barrier_digest(&mut self, key: B256, digest: B256) -> ZoneResult<()> {
        let existing = self.imported_barriers[key].read()?;
        if !existing.is_zero() && existing != digest {
            return Err(FastTransferError::imported_barrier_conflict().into());
        }
        if existing.is_zero() {
            self.imported_barriers[key].write(digest)?;
        }
        Ok(())
    }

    pub(super) fn record_imported_barrier_verified<P: L1StorageReader>(
        &mut self,
        l1: &L1State<P>,
        compact: &IFastTransfer::SourceBarrierStatement,
        source_barrier_certificate: &[u8],
    ) -> ZoneResult<B256> {
        let statement = decode_statement(compact);
        let certificate = decode_certificate(source_barrier_certificate)?;
        // ContractStorage is initialized from the EVM configuration. Its chain ID is the Zone
        // EIP-155 chain ID, from which the canonical parent Tempo chain ID is derived. Neither the
        // compact statement nor the retained inventory is allowed to choose this domain.
        let l1_chain_id = decode_l1_chain_id(self.storage.chain_id())
            .map_err(|_| FastTransferError::invalid_imported_barrier())?;
        let barrier_digest = statement.registry_digest(l1_chain_id);
        if certificate.digest != barrier_digest
            || statement.destination_portal != l1.portal()
            || statement.destination_epoch == 0
            || statement.source_portal.is_zero()
            || statement.source_portal == l1.portal()
            || statement.source_epoch == 0
            || statement.imported_anchor_number == 0
            || statement.imported_anchor_hash.is_zero()
            || statement.log_index == 0
            || statement.block_height.is_zero()
            || statement.block_hash.is_zero()
            || statement.state_root.is_zero()
            || statement.complete_lock_root.is_zero()
            || statement.unresolved_root.is_zero()
            || statement.lock_log_watermark > statement.log_index
            || statement.unresolved_count > super::MAX_DRAIN_LOCKS as u64
            || l1
                .get_anchor()
                .is_none_or(|anchor| statement.imported_anchor_number > anchor)
        {
            return Err(FastTransferError::invalid_imported_barrier().into());
        }

        // The destination closure and source signer keys are read from the finalized imported
        // registry state. Neither is accepted from transaction metadata.
        let destination_portal = ZonePortalStorage::new(l1.portal());
        if l1.read_l1(destination_portal.fast_epoch_handler())? != statement.destination_epoch {
            return Err(FastTransferError::invalid_imported_barrier().into());
        }
        let destination = read_epoch_registry(l1, l1.portal(), statement.destination_epoch)?;
        if !destination.config.closed
            || destination.config.retired
            || destination.config.closure_hash != statement.closure_hash
            || !destination.peers.contains(&statement.source_portal)
        {
            return Err(FastTransferError::invalid_imported_barrier().into());
        }
        let source = read_epoch_registry(l1, statement.source_portal, statement.source_epoch)?;
        if source.config.activated_at_tempo_block == 0
            || source.config.activated_at_tempo_block > statement.imported_anchor_number
        {
            return Err(FastTransferError::invalid_imported_barrier().into());
        }
        let protocol_version = u16::try_from(source.config.protocol_version)
            .map_err(|_| FastTransferError::invalid_imported_barrier())?;
        let roster = EpochRoster::from_finalized_registry(
            ZoneDomain {
                l1_chain_id,
                zone_id: 0,
                chain_id: 0,
                portal: statement.source_portal,
                authority_epoch: statement.source_epoch,
                roster_hash: source.config.roster_hash,
                protocol_version,
            },
            source.members,
        )
        .map_err(|_| FastTransferError::invalid_imported_barrier())?;
        QuorumVerifier::new(roster)
            .verify_source_barrier(&statement, &certificate.signatures)
            .map_err(|_| FastTransferError::invalid_imported_barrier())?;

        let key = Self::imported_barrier_key(
            statement.destination_epoch,
            statement.source_portal,
            statement.source_epoch,
        );
        self.store_imported_barrier_digest(key, barrier_digest)?;
        // Emit at the same native execution point for both the first commit and an exact replay.
        // The event exposes only source-signed commitments; the full object remains in the
        // fsynced protocol record bound to this receipt.
        self.emit_event(FastTransferEvent::imported_barrier_recorded(
            statement.destination_epoch,
            statement.source_portal,
            statement.source_epoch,
            barrier_digest,
            statement.complete_lock_root,
        ))?;
        Ok(barrier_digest)
    }
}

fn decode_statement(value: &IFastTransfer::SourceBarrierStatement) -> FastBarrierStatement {
    FastBarrierStatement {
        destination_portal: value.destinationPortal,
        destination_epoch: value.destinationEpoch,
        closure_hash: value.closureHash,
        source_portal: value.sourcePortal,
        source_epoch: value.sourceEpoch,
        imported_anchor_number: value.importedAnchorNumber,
        imported_anchor_hash: value.importedAnchorHash,
        log_term: value.logTerm,
        log_index: value.logIndex,
        block_height: value.blockHeight,
        block_hash: value.blockHash,
        state_root: value.stateRoot,
        lock_log_watermark: value.lockLogWatermark,
        complete_lock_root: value.completeLockRoot,
        unresolved_root: value.unresolvedRoot,
        unresolved_count: value.unresolvedCount,
    }
}

fn decode_certificate(bytes: &[u8]) -> ZoneResult<DrainCertificate> {
    if bytes.len() != IMPORTED_BARRIER_CERTIFICATE_BYTES
        || bytes[0] != IMPORTED_BARRIER_CERTIFICATE_VERSION
    {
        return Err(FastTransferError::invalid_imported_barrier().into());
    }
    let digest = B256::from_slice(&bytes[1..33]);
    let first: [u8; 65] = bytes[33..98]
        .try_into()
        .map_err(|_| FastTransferError::invalid_imported_barrier())?;
    let second: [u8; 65] = bytes[98..163]
        .try_into()
        .map_err(|_| FastTransferError::invalid_imported_barrier())?;
    Ok(DrainCertificate {
        digest,
        signatures: [SignatureBytes(first), SignatureBytes(second)],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{
        collections::{BTreeMap, BTreeSet},
        vec::Vec,
    };
    use tempo_precompiles::storage::StorageCtx;
    use tempo_zone_contracts::source_barrier_statement;
    use zone_fast_transfer::drain::{
        BarrierInventory, CommittedSourceLock, MAX_DRAIN_OBJECT_BYTES,
    };
    use zone_primitives::fast_transfer::{
        AssetId, CertificateBody, OutcomeCertificate, TransferIntent, TransferOutcome,
    };

    use crate::test_utils::{test_context, test_storage_provider};

    #[test]
    fn imported_barrier_key_is_typed_and_stable() {
        let source = Address::repeat_byte(7);
        let key = FastTransfer::imported_barrier_key(4, source, 9);
        assert!(!key.is_zero());
        assert_ne!(key, FastTransfer::imported_barrier_key(5, source, 9));
        assert_ne!(
            key,
            FastTransfer::imported_barrier_key(4, Address::repeat_byte(8), 9)
        );
        assert_ne!(key, FastTransfer::imported_barrier_key(4, source, 10));
    }

    #[test]
    fn certificate_decoder_is_exact() {
        let digest = B256::repeat_byte(3);
        let statement = statement();
        let certificate = tempo_zone_contracts::imported_barrier_call(
            &statement,
            digest,
            [SignatureBytes([4; 65]), SignatureBytes([5; 65])],
        );
        // Decode only the fixed certificate through a directly constructed representation; the
        // ABI builder itself has an independent contracts-crate test.
        let mut encoded = Vec::with_capacity(IMPORTED_BARRIER_CERTIFICATE_BYTES);
        encoded.push(IMPORTED_BARRIER_CERTIFICATE_VERSION);
        encoded.extend_from_slice(digest.as_slice());
        encoded.extend_from_slice(&[4; 65]);
        encoded.extend_from_slice(&[5; 65]);
        let decoded = decode_certificate(&encoded).expect("valid fixed certificate");
        assert_eq!(decoded.digest, digest);
        assert_eq!(decoded.signatures[0], SignatureBytes([4; 65]));
        assert!(decode_certificate(&encoded[..encoded.len() - 1]).is_err());
        assert_eq!(certificate.barrier_digest, digest);
    }

    #[test]
    fn compact_statement_mutation_wrong_closure_and_wrong_chain_change_signed_digest() {
        let statement = statement();
        let compact = source_barrier_statement(&statement);
        assert_eq!(decode_statement(&compact), statement);
        let mut mutated = compact.clone();
        mutated.unresolvedRoot = B256::repeat_byte(99);
        assert_ne!(
            decode_statement(&compact).registry_digest(1_337),
            decode_statement(&mutated).registry_digest(1_337)
        );
        let mut wrong_closure = compact.clone();
        wrong_closure.closureHash = B256::repeat_byte(98);
        assert_ne!(
            decode_statement(&compact).registry_digest(1_337),
            decode_statement(&wrong_closure).registry_digest(1_337)
        );
        assert_ne!(
            decode_statement(&compact).registry_digest(1_337),
            decode_statement(&compact).registry_digest(1_338)
        );
    }

    #[test]
    fn permanent_signed_digest_is_idempotent_and_conflicts_fail_closed() -> eyre::Result<()> {
        let mut ctx = test_context();
        let mut storage = test_storage_provider(&mut ctx, u64::MAX, false);
        StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
            let mut fast = FastTransfer::new();
            fast.initialize()?;
            let key = FastTransfer::imported_barrier_key(2, Address::repeat_byte(4), 5);
            let digest = B256::repeat_byte(31);
            fast.store_imported_barrier_digest(key, digest)?;
            fast.store_imported_barrier_digest(key, digest)?;
            assert_eq!(fast.imported_barriers[key].read()?, digest);
            assert!(
                fast.store_imported_barrier_digest(key, B256::repeat_byte(32))
                    .is_err()
            );
            assert_eq!(fast.imported_barriers[key].read()?, digest);
            Ok(())
        })
    }

    #[test]
    fn near_limit_valid_unresolved_inventory_keeps_native_calldata_bounded() {
        const LOCKS: usize = 9_999;
        let locks = (1..=LOCKS as u64).map(lock).collect::<Vec<_>>();
        let inventory = BarrierInventory::build(
            1_337,
            Address::repeat_byte(1),
            2,
            B256::repeat_byte(3),
            Address::repeat_byte(4),
            5,
            6,
            B256::repeat_byte(7),
            8,
            LOCKS as u64,
            10_001,
            B256::repeat_byte(9),
            B256::repeat_byte(10),
            locks,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
        )
        .expect("near-limit unresolved inventory");
        inventory.verify_complete().expect("complete signed roots");
        assert_eq!(inventory.statement.unresolved_count, LOCKS as u64);
        let full_inventory = inventory.durable_bytes().expect("durable full inventory");
        assert!(full_inventory.len() <= MAX_DRAIN_OBJECT_BYTES);

        let digest = inventory.statement.registry_digest(inventory.l1_chain_id);
        let call = tempo_zone_contracts::imported_barrier_call(
            &inventory.statement,
            digest,
            [SignatureBytes([11; 65]), SignatureBytes([12; 65])],
        );
        assert_eq!(call.calldata.len(), 772);
        assert!(full_inventory.len() > call.calldata.len() * 100);
        std::println!(
            "valid_unresolved={} retained_inventory_bytes={} native_calldata_bytes={}",
            LOCKS,
            full_inventory.len(),
            call.calldata.len()
        );
        assert_eq!(call.barrier_digest, digest);
        assert_eq!(
            call.complete_lock_root,
            inventory.statement.complete_lock_root
        );
    }

    fn domain(zone_id: u32, portal: u8, epoch: u64) -> ZoneDomain {
        ZoneDomain {
            l1_chain_id: 1_337,
            zone_id,
            chain_id: (1_337u64 << 32) | u64::from(zone_id),
            portal: Address::repeat_byte(portal),
            authority_epoch: epoch,
            roster_hash: B256::repeat_byte(portal),
            protocol_version: 1,
        }
    }

    fn lock(index: u64) -> CommittedSourceLock {
        let intent = TransferIntent {
            source: domain(1, 4, 5),
            destination: domain(2, 1, 2),
            asset: AssetId {
                l1_token: Address::repeat_byte(20),
                source_token: Address::repeat_byte(21),
                destination_token: Address::repeat_byte(22),
                decimals: 6,
            },
            sender: Address::repeat_byte(23),
            recipient: Address::repeat_byte(24),
            refund_account: Address::repeat_byte(23),
            destination_pool: Address::repeat_byte(25),
            reimbursement_account: Address::repeat_byte(26),
            principal: U256::from(100),
            fee: U256::from(2),
            quote_id: B256::repeat_byte(27),
            destination_expiry_height: 20_000,
            transfer_nonce: index,
        };
        let certificate = OutcomeCertificate {
            body: CertificateBody {
                transfer_id: intent.transfer_id(),
                intent_hash: intent.intent_hash(),
                zone: intent.source,
                log_term: 8,
                log_index: index,
                block_height: index + 10_001,
                block_hash: B256::from(U256::from(index + 20_001)),
                state_root: B256::from(U256::from(index + 30_001)),
                transaction_hash: B256::from(U256::from(index + 40_001)),
                outcome: TransferOutcome::Locked {
                    escrow: tempo_zone_contracts::FAST_TRANSFER_ADDRESS,
                    amount: U256::from(102),
                },
            },
            signatures: [SignatureBytes([11; 65]), SignatureBytes([12; 65])],
        };
        CommittedSourceLock {
            intent,
            lock: certificate,
        }
    }

    fn statement() -> FastBarrierStatement {
        FastBarrierStatement {
            destination_portal: Address::repeat_byte(1),
            destination_epoch: 2,
            closure_hash: B256::repeat_byte(3),
            source_portal: Address::repeat_byte(4),
            source_epoch: 5,
            imported_anchor_number: 6,
            imported_anchor_hash: B256::repeat_byte(7),
            log_term: 8,
            log_index: 9,
            block_height: U256::from(10),
            block_hash: B256::repeat_byte(11),
            state_root: B256::repeat_byte(12),
            lock_log_watermark: 9,
            complete_lock_root: B256::repeat_byte(13),
            unresolved_root: B256::repeat_byte(14),
            unresolved_count: 9,
        }
    }
}
