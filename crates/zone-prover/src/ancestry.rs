//! Tempo ancestry chain verification.
//!
//! When the anchor block number differs from the Tempo block number (ancestry mode),
//! the prover must verify a parent-hash chain from `tempo_block_number` to
//! `anchor_block_number`, ending at the anchor block hash.

use alloy_primitives::{B256, keccak256};

use crate::types::ProverError;

/// Verify the Tempo ancestry chain.
///
/// In ancestry mode (`anchor_block_number > tempo_block_number`), verifies a
/// parent-hash chain from the finalized Tempo block hash through intermediate
/// headers to the anchor block.
///
/// The `ancestry_headers` must be ordered from `tempo_block_number + 1` to
/// `anchor_block_number` (inclusive). Each header's parent hash must equal
/// the hash of the previous header (or `tempo_block_hash` for the first one).
///
/// # Arguments
///
/// * `tempo_block_hash` - Hash of the Tempo block at `tempo_block_number`
/// * `tempo_block_number` - The finalized Tempo block number
/// * `anchor_block_number` - The anchor block number (from EIP-2935)
/// * `anchor_block_hash` - The expected anchor block hash
/// * `ancestry_headers` - RLP-encoded headers from `tempo_block_number + 1` to
///   `anchor_block_number`
pub fn verify_tempo_ancestry_chain(
    tempo_block_hash: B256,
    tempo_block_number: u64,
    anchor_block_number: u64,
    anchor_block_hash: B256,
    ancestry_headers: &[Vec<u8>],
) -> Result<(), ProverError> {
    let expected_count = (anchor_block_number - tempo_block_number) as usize;

    if ancestry_headers.len() != expected_count {
        return Err(ProverError::InconsistentState(format!(
            "ancestry headers: expected {expected_count} headers \
             (blocks {}..={}), got {}",
            tempo_block_number + 1,
            anchor_block_number,
            ancestry_headers.len()
        )));
    }

    let mut prev_hash = tempo_block_hash;

    for (i, header_rlp) in ancestry_headers.iter().enumerate() {
        let expected_number = tempo_block_number + 1 + i as u64;

        // Extract parent hash from the RLP-encoded header.
        // The parent hash is the first field in the header list.
        let parent_hash = extract_parent_hash_from_rlp(header_rlp).map_err(|e| {
            ProverError::RlpDecode(format!(
                "ancestry header {expected_number}: {e}"
            ))
        })?;

        // Verify parent hash chain continuity.
        if parent_hash != prev_hash {
            return Err(ProverError::InconsistentState(format!(
                "ancestry chain broken at block {expected_number}: \
                 parent_hash={parent_hash}, expected={prev_hash}"
            )));
        }

        // This header's hash becomes the next expected parent.
        prev_hash = keccak256(header_rlp);
    }

    // The final header's hash must equal the anchor block hash.
    if prev_hash != anchor_block_hash {
        return Err(ProverError::InconsistentState(format!(
            "ancestry chain does not reach anchor: \
             final_hash={prev_hash}, anchor_hash={anchor_block_hash}"
        )));
    }

    Ok(())
}

/// Extract the parent hash from an RLP-encoded Tempo header.
///
/// Tempo headers are encoded as: `rlp([general_gas_limit, shared_gas_limit,
/// timestamp_millis_part, inner])` where `inner` is a standard Ethereum header.
/// The parent hash is the first field of the inner header.
pub(crate) fn extract_parent_hash_from_rlp(header_rlp: &[u8]) -> Result<B256, String> {
    // Decode the outer list.
    let outer_payload = decode_rlp_list_payload(header_rlp)
        .map_err(|e| format!("outer list: {e}"))?;

    // Skip the first 3 wrapper fields (general_gas_limit, shared_gas_limit, timestamp_millis_part).
    let mut offset = 0;
    for i in 0..3 {
        let item_len = rlp_item_total_length(&outer_payload[offset..])
            .map_err(|e| format!("wrapper field {i}: {e}"))?;
        offset += item_len;
    }

    // The 4th item is the inner Ethereum header (a list).
    let inner_rlp = &outer_payload[offset..];
    let inner_payload = decode_rlp_list_payload(inner_rlp)
        .map_err(|e| format!("inner list: {e}"))?;

    // The first field of the inner header is parentHash (32 bytes).
    decode_rlp_bytes32(inner_payload)
        .map_err(|e| format!("parentHash: {e}"))
}

/// Decode an RLP list and return its payload (the bytes after the list prefix).
fn decode_rlp_list_payload(data: &[u8]) -> Result<&[u8], &'static str> {
    if data.is_empty() {
        return Err("empty data");
    }

    let prefix = data[0];
    if prefix < 0xc0 {
        return Err("not a list");
    }

    if prefix <= 0xf7 {
        // Short list: 0xc0 + length
        let len = (prefix - 0xc0) as usize;
        if data.len() < 1 + len {
            return Err("short list truncated");
        }
        Ok(&data[1..1 + len])
    } else {
        // Long list: 0xf7 + length_of_length
        let len_len = (prefix - 0xf7) as usize;
        if data.len() < 1 + len_len {
            return Err("long list length truncated");
        }
        let mut len: usize = 0;
        for &b in &data[1..1 + len_len] {
            len = (len << 8) | (b as usize);
        }
        let offset = 1 + len_len;
        if data.len() < offset + len {
            return Err("long list data truncated");
        }
        Ok(&data[offset..offset + len])
    }
}

/// Get the total length (prefix + payload) of an RLP item.
fn rlp_item_total_length(data: &[u8]) -> Result<usize, &'static str> {
    if data.is_empty() {
        return Err("empty data");
    }

    let prefix = data[0];

    if prefix <= 0x7f {
        Ok(1)
    } else if prefix <= 0xb7 {
        let len = (prefix - 0x80) as usize;
        Ok(1 + len)
    } else if prefix <= 0xbf {
        let len_len = (prefix - 0xb7) as usize;
        if data.len() < 1 + len_len {
            return Err("long string length truncated");
        }
        let mut len: usize = 0;
        for &b in &data[1..1 + len_len] {
            len = (len << 8) | (b as usize);
        }
        Ok(1 + len_len + len)
    } else if prefix <= 0xf7 {
        let len = (prefix - 0xc0) as usize;
        Ok(1 + len)
    } else {
        let len_len = (prefix - 0xf7) as usize;
        if data.len() < 1 + len_len {
            return Err("long list length truncated");
        }
        let mut len: usize = 0;
        for &b in &data[1..1 + len_len] {
            len = (len << 8) | (b as usize);
        }
        Ok(1 + len_len + len)
    }
}

/// Extract the state root from an RLP-encoded Tempo header.
///
/// Tempo headers are encoded as: `rlp([general_gas_limit, shared_gas_limit,
/// timestamp_millis_part, inner])` where `inner` is a standard Ethereum header.
///
/// The state root is the 4th field (index 3) of the inner Ethereum header:
/// `[parentHash, ommersHash, beneficiary, **stateRoot**, transactionsRoot, ...]`
pub(crate) fn extract_state_root_from_rlp(header_rlp: &[u8]) -> Result<B256, String> {
    // Decode the outer list.
    let outer_payload = decode_rlp_list_payload(header_rlp)
        .map_err(|e| format!("outer list: {e}"))?;

    // Skip the first 3 wrapper fields (general_gas_limit, shared_gas_limit, timestamp_millis_part).
    let mut offset = 0;
    for i in 0..3 {
        let item_len = rlp_item_total_length(&outer_payload[offset..])
            .map_err(|e| format!("wrapper field {i}: {e}"))?;
        offset += item_len;
    }

    // The 4th item is the inner Ethereum header (a list).
    let inner_rlp = &outer_payload[offset..];
    let inner_payload = decode_rlp_list_payload(inner_rlp)
        .map_err(|e| format!("inner list: {e}"))?;

    // Skip the first 3 fields (parentHash, ommersHash, beneficiary) to reach stateRoot.
    let mut inner_offset = 0;
    for i in 0..3 {
        if inner_offset >= inner_payload.len() {
            return Err(format!("inner header too short at field {i}"));
        }
        let item_len = rlp_item_total_length(&inner_payload[inner_offset..])
            .map_err(|e| format!("inner field {i}: {e}"))?;
        inner_offset += item_len;
    }

    if inner_offset >= inner_payload.len() {
        return Err("inner header too short for stateRoot field".into());
    }

    // The 4th field is the state root (bytes32).
    decode_rlp_bytes32(&inner_payload[inner_offset..])
        .map_err(|e| format!("stateRoot: {e}"))
}

/// Extract the block number from an RLP-encoded Tempo header.
///
/// Tempo headers are encoded as: `rlp([general_gas_limit, shared_gas_limit,
/// timestamp_millis_part, inner])` where `inner` is a standard Ethereum header.
///
/// The block number is the 9th field (index 8) of the inner Ethereum header:
/// `[parentHash, ommersHash, beneficiary, stateRoot, transactionsRoot,
///  receiptsRoot, logsBloom, difficulty, **number**, ...]`
pub(crate) fn extract_block_number_from_rlp(header_rlp: &[u8]) -> Result<u64, String> {
    // Decode the outer list.
    let outer_payload = decode_rlp_list_payload(header_rlp)
        .map_err(|e| format!("outer list: {e}"))?;

    // Skip the first 3 wrapper fields (general_gas_limit, shared_gas_limit, timestamp_millis_part).
    let mut offset = 0;
    for i in 0..3 {
        let item_len = rlp_item_total_length(&outer_payload[offset..])
            .map_err(|e| format!("wrapper field {i}: {e}"))?;
        offset += item_len;
    }

    // The 4th item is the inner Ethereum header (a list).
    let inner_rlp = &outer_payload[offset..];
    let inner_payload = decode_rlp_list_payload(inner_rlp)
        .map_err(|e| format!("inner list: {e}"))?;

    // Skip the first 8 fields to reach `number` (index 8).
    let mut inner_offset = 0;
    for i in 0..8 {
        if inner_offset >= inner_payload.len() {
            return Err(format!("inner header too short at field {i}"));
        }
        let item_len = rlp_item_total_length(&inner_payload[inner_offset..])
            .map_err(|e| format!("inner field {i}: {e}"))?;
        inner_offset += item_len;
    }

    if inner_offset >= inner_payload.len() {
        return Err("inner header too short for number field".into());
    }

    // The 9th field is the block number (U256, variable-length big-endian integer).
    decode_rlp_u64(&inner_payload[inner_offset..])
        .map_err(|e| format!("block number: {e}"))
}

/// Decode a u64 from the start of an RLP payload.
///
/// Handles RLP-encoded integers: single byte (0x00..0x7f), short string (0x80 + len),
/// or empty (0x80 = zero).
fn decode_rlp_u64(data: &[u8]) -> Result<u64, &'static str> {
    if data.is_empty() {
        return Err("empty data");
    }

    let prefix = data[0];

    if prefix == 0x80 {
        // Empty bytes = zero
        Ok(0)
    } else if prefix <= 0x7f {
        // Single byte value
        Ok(prefix as u64)
    } else if prefix <= 0xb7 {
        // Short string: 0x80 + length, then the bytes
        let len = (prefix - 0x80) as usize;
        if len > 8 {
            return Err("integer too large for u64");
        }
        if data.len() < 1 + len {
            return Err("integer data truncated");
        }
        let mut value: u64 = 0;
        for &b in &data[1..1 + len] {
            value = (value << 8) | (b as u64);
        }
        Ok(value)
    } else {
        Err("unexpected prefix for integer")
    }
}

/// Decode a bytes32 (B256) from the start of an RLP payload.
fn decode_rlp_bytes32(data: &[u8]) -> Result<B256, &'static str> {
    if data.is_empty() {
        return Err("empty data");
    }

    let prefix = data[0];

    if prefix == 0xa0 {
        // 32-byte string: prefix 0x80 + 32 = 0xa0
        if data.len() < 33 {
            return Err("bytes32 truncated");
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&data[1..33]);
        Ok(B256::from(hash))
    } else if prefix <= 0x7f {
        // Single byte value
        let mut hash = [0u8; 32];
        hash[31] = prefix;
        Ok(B256::from(hash))
    } else if prefix == 0x80 {
        // Empty bytes = zero
        Ok(B256::ZERO)
    } else {
        Err("unexpected prefix for bytes32")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rlp_item_length() {
        // Single byte
        assert_eq!(rlp_item_total_length(&[0x42]).unwrap(), 1);

        // Short string (empty)
        assert_eq!(rlp_item_total_length(&[0x80]).unwrap(), 1);

        // Short string (5 bytes)
        assert_eq!(rlp_item_total_length(&[0x85, 1, 2, 3, 4, 5]).unwrap(), 6);

        // Short list (empty)
        assert_eq!(rlp_item_total_length(&[0xc0]).unwrap(), 1);
    }
}
