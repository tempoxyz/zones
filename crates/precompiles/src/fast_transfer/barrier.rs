//! Canonical bounded proof that a delayed lock belongs to a finalized source barrier.

use alloc::vec::Vec;

use alloy_primitives::{Address, B256, keccak256};
use zone_primitives::fast_transfer::{CanonicalEncode, OutcomeCertificate};

use super::{MAX_BARRIER_PROOF_BYTES, MAX_DRAIN_LOCKS, MAX_DRAIN_PROOF_DEPTH};

const PROOF_FORMAT_V1: u8 = 1;
const LOCK_LEAF_DOMAIN: &[u8] = b"TEMPO_ZONE_FAST_DRAIN_LOCK_LEAF_T14_V1";
const MERKLE_NODE_DOMAIN: &[u8] = b"TEMPO_ZONE_FAST_DRAIN_MERKLE_NODE_T14_V1";
const MERKLE_ROOT_DOMAIN: &[u8] = b"TEMPO_ZONE_FAST_DRAIN_MERKLE_ROOT_T14_V1";

/// The signed-barrier coordinates and count-bound Merkle path supplied to `resolve`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BarrierInclusionProof {
    pub destination_portal: Address,
    pub destination_epoch: u64,
    pub closure_hash: B256,
    pub source_portal: Address,
    pub source_epoch: u64,
    pub imported_anchor_number: u64,
    pub imported_anchor_hash: B256,
    pub barrier_hash: B256,
    pub lock_log_watermark: u64,
    pub complete_lock_root: B256,
    pub leaf_index: u32,
    pub leaf_count: u32,
    pub siblings: Vec<B256>,
}

impl BarrierInclusionProof {
    const FIXED_LEN: usize = 1 + 20 + 8 + 32 + 20 + 8 + 8 + 32 + 32 + 8 + 32 + 4 + 4 + 1;

    pub(super) fn decode(bytes: &[u8]) -> Result<Self, ()> {
        if bytes.len() < Self::FIXED_LEN || bytes.len() > MAX_BARRIER_PROOF_BYTES {
            return Err(());
        }
        let mut reader = Reader::new(bytes);
        if reader.u8()? != PROOF_FORMAT_V1 {
            return Err(());
        }
        let proof = Self {
            destination_portal: reader.address()?,
            destination_epoch: reader.u64()?,
            closure_hash: reader.b256()?,
            source_portal: reader.address()?,
            source_epoch: reader.u64()?,
            imported_anchor_number: reader.u64()?,
            imported_anchor_hash: reader.b256()?,
            barrier_hash: reader.b256()?,
            lock_log_watermark: reader.u64()?,
            complete_lock_root: reader.b256()?,
            leaf_index: reader.u32()?,
            leaf_count: reader.u32()?,
            siblings: {
                let count = usize::from(reader.u8()?);
                if count > MAX_DRAIN_PROOF_DEPTH {
                    return Err(());
                }
                let mut siblings = Vec::with_capacity(count);
                for _ in 0..count {
                    siblings.push(reader.b256()?);
                }
                siblings
            },
        };
        if !reader.is_empty() {
            return Err(());
        }
        proof.validate_shape()?;
        Ok(proof)
    }

    #[cfg(test)]
    pub(super) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::FIXED_LEN + self.siblings.len() * 32);
        out.push(PROOF_FORMAT_V1);
        out.extend_from_slice(self.destination_portal.as_slice());
        out.extend_from_slice(&self.destination_epoch.to_be_bytes());
        out.extend_from_slice(self.closure_hash.as_slice());
        out.extend_from_slice(self.source_portal.as_slice());
        out.extend_from_slice(&self.source_epoch.to_be_bytes());
        out.extend_from_slice(&self.imported_anchor_number.to_be_bytes());
        out.extend_from_slice(self.imported_anchor_hash.as_slice());
        out.extend_from_slice(self.barrier_hash.as_slice());
        out.extend_from_slice(&self.lock_log_watermark.to_be_bytes());
        out.extend_from_slice(self.complete_lock_root.as_slice());
        out.extend_from_slice(&self.leaf_index.to_be_bytes());
        out.extend_from_slice(&self.leaf_count.to_be_bytes());
        out.push(self.siblings.len() as u8);
        for sibling in &self.siblings {
            out.extend_from_slice(sibling.as_slice());
        }
        out
    }

    pub(super) fn verify_lock(
        &self,
        transfer_id: B256,
        intent_hash: B256,
        lock: &OutcomeCertificate,
    ) -> Result<(), ()> {
        self.validate_shape()?;
        if lock.body.log_index == 0 || lock.body.log_index > self.lock_log_watermark {
            return Err(());
        }
        if lock.body.transfer_id != transfer_id || lock.body.intent_hash != intent_hash {
            return Err(());
        }
        let mut node = lock_leaf(lock);
        let mut cursor = self.leaf_index;
        for sibling in &self.siblings {
            node = if cursor & 1 == 0 {
                merkle_node(node, *sibling)
            } else {
                merkle_node(*sibling, node)
            };
            cursor >>= 1;
        }
        (bind_root(self.leaf_count, node) == self.complete_lock_root)
            .then_some(())
            .ok_or(())
    }

    fn validate_shape(&self) -> Result<(), ()> {
        let count = usize::try_from(self.leaf_count).map_err(|_| ())?;
        let index = usize::try_from(self.leaf_index).map_err(|_| ())?;
        if count == 0
            || count > MAX_DRAIN_LOCKS
            || index >= count
            || self.siblings.len() != proof_depth(count)
            || self.destination_portal.is_zero()
            || self.destination_epoch == 0
            || self.closure_hash.is_zero()
            || self.source_portal.is_zero()
            || self.source_epoch == 0
            || self.imported_anchor_number == 0
            || self.imported_anchor_hash.is_zero()
            || self.barrier_hash.is_zero()
            || self.complete_lock_root.is_zero()
        {
            return Err(());
        }
        Ok(())
    }
}

pub(crate) fn lock_leaf(lock: &OutcomeCertificate) -> B256 {
    tagged_hash(
        LOCK_LEAF_DOMAIN,
        &[
            lock.body.transfer_id.as_slice(),
            lock.body.intent_hash.as_slice(),
            &lock.body.log_term.to_be_bytes(),
            &lock.body.log_index.to_be_bytes(),
            lock.body.body_hash().as_slice(),
            &lock.canonical_bytes(),
        ],
    )
}

fn merkle_node(left: B256, right: B256) -> B256 {
    tagged_hash(MERKLE_NODE_DOMAIN, &[left.as_slice(), right.as_slice()])
}

fn bind_root(count: u32, root: B256) -> B256 {
    tagged_hash(MERKLE_ROOT_DOMAIN, &[&count.to_be_bytes(), root.as_slice()])
}

fn tagged_hash(domain: &[u8], fields: &[&[u8]]) -> B256 {
    let mut encoded = Vec::with_capacity(
        4 + domain.len() + fields.iter().map(|field| 4 + field.len()).sum::<usize>(),
    );
    encoded.extend_from_slice(&(domain.len() as u32).to_be_bytes());
    encoded.extend_from_slice(domain);
    for field in fields {
        encoded.extend_from_slice(&(field.len() as u32).to_be_bytes());
        encoded.extend_from_slice(field);
    }
    keccak256(encoded)
}

fn proof_depth(mut count: usize) -> usize {
    let mut depth = 0;
    while count > 1 {
        count = count.div_ceil(2);
        depth += 1;
    }
    depth
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], ()> {
        let end = self.offset.checked_add(len).ok_or(())?;
        let value = self.bytes.get(self.offset..end).ok_or(())?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, ()> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, ()> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().map_err(|_| ())?,
        ))
    }

    fn u64(&mut self) -> Result<u64, ()> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().map_err(|_| ())?,
        ))
    }

    fn address(&mut self) -> Result<Address, ()> {
        Ok(Address::from_slice(self.take(20)?))
    }

    fn b256(&mut self) -> Result<B256, ()> {
        Ok(B256::from_slice(self.take(32)?))
    }

    fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use std::collections::{BTreeMap, BTreeSet};
    use zone_fast_transfer::drain::{BarrierInventory, CommittedSourceLock};
    use zone_primitives::fast_transfer::{
        AssetId, CertificateBody, SignatureBytes, TransferIntent, TransferOutcome, ZoneDomain,
    };

    fn fixture() -> (TransferIntent, OutcomeCertificate) {
        let source = ZoneDomain {
            l1_chain_id: 1,
            zone_id: 2,
            chain_id: 3,
            portal: Address::repeat_byte(5),
            authority_epoch: 6,
            roster_hash: B256::repeat_byte(7),
            protocol_version: 1,
        };
        let destination = ZoneDomain {
            l1_chain_id: 1,
            zone_id: 20,
            chain_id: 21,
            portal: Address::repeat_byte(20),
            authority_epoch: 21,
            roster_hash: B256::repeat_byte(22),
            protocol_version: 1,
        };
        let intent = TransferIntent {
            source,
            destination,
            asset: AssetId {
                l1_token: Address::repeat_byte(3),
                source_token: Address::repeat_byte(3),
                destination_token: Address::repeat_byte(3),
                decimals: 6,
            },
            sender: Address::repeat_byte(4),
            recipient: Address::repeat_byte(8),
            refund_account: Address::repeat_byte(4),
            destination_pool: Address::repeat_byte(9),
            reimbursement_account: Address::repeat_byte(10),
            principal: U256::from_limbs([4, 0, 0, 0]),
            fee: U256::from_limbs([1, 0, 0, 0]),
            quote_id: B256::repeat_byte(10),
            destination_expiry_height: 100,
            transfer_nonce: 11,
        };
        let lock = OutcomeCertificate {
            body: CertificateBody {
                transfer_id: intent.transfer_id(),
                intent_hash: intent.intent_hash(),
                outcome: TransferOutcome::Locked {
                    escrow: Address::repeat_byte(14),
                    amount: U256::from_limbs([5, 0, 0, 0]),
                },
                zone: source,
                log_term: 8,
                log_index: 9,
                block_height: 10,
                block_hash: B256::repeat_byte(11),
                state_root: B256::repeat_byte(12),
                transaction_hash: B256::repeat_byte(13),
            },
            signatures: [SignatureBytes([1; 65]), SignatureBytes([2; 65])],
        };
        (intent, lock)
    }

    #[test]
    fn native_leaf_and_proof_match_the_complete_lock_tree_producer() {
        let (intent, lock) = fixture();
        let inventory = BarrierInventory::build(
            intent.source.l1_chain_id,
            intent.destination.portal,
            intent.destination.authority_epoch,
            B256::repeat_byte(23),
            intent.source.portal,
            intent.source.authority_epoch,
            24,
            B256::repeat_byte(24),
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
        .unwrap();
        let producer_proof = &inventory.locks[0].complete_proof;
        let root = bind_root(1, lock_leaf(&lock));
        assert_eq!(root, inventory.statement.complete_lock_root);
        let proof = BarrierInclusionProof {
            destination_portal: intent.destination.portal,
            destination_epoch: intent.destination.authority_epoch,
            closure_hash: inventory.statement.closure_hash,
            source_portal: lock.body.zone.portal,
            source_epoch: lock.body.zone.authority_epoch,
            imported_anchor_number: inventory.statement.imported_anchor_number,
            imported_anchor_hash: inventory.statement.imported_anchor_hash,
            barrier_hash: B256::repeat_byte(25),
            lock_log_watermark: lock.body.log_index,
            complete_lock_root: root,
            leaf_index: producer_proof.leaf_index,
            leaf_count: producer_proof.leaf_count,
            siblings: producer_proof.siblings.clone(),
        };
        let decoded = BarrierInclusionProof::decode(&proof.encode()).unwrap();
        assert_eq!(decoded, proof);
        assert!(
            decoded
                .verify_lock(lock.body.transfer_id, lock.body.intent_hash, &lock)
                .is_ok()
        );

        let mut wrong = decoded;
        wrong.lock_log_watermark -= 1;
        assert!(
            wrong
                .verify_lock(lock.body.transfer_id, lock.body.intent_hash, &lock)
                .is_err()
        );
    }
}
