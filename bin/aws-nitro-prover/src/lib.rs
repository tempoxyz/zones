#![no_std]

extern crate alloc;

use alloc::{string::String, vec::Vec};

use serde::{Deserialize, Serialize};

/// Request accepted by the Nitro prover echo service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProveBatchRequest {
    /// Previous proven zone block hash.
    #[serde(with = "hex_32")]
    pub prev_block_hash: [u8; 32],
    /// New zone block hash being proven.
    #[serde(with = "hex_32")]
    pub next_block_hash: [u8; 32],
}

/// Response returned by the Nitro prover echo service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProveBatchResponse {
    /// Previous proven zone block hash.
    #[serde(with = "hex_32")]
    pub prev_block_hash: [u8; 32],
    /// New zone block hash being proven.
    #[serde(with = "hex_32")]
    pub next_block_hash: [u8; 32],
    /// Opaque verifier config bytes consumed by the on-chain verifier.
    #[serde(with = "hex_bytes")]
    pub verifier_config: Vec<u8>,
    /// Opaque proof bytes consumed by the on-chain verifier.
    #[serde(with = "hex_bytes")]
    pub proof: Vec<u8>,
}

impl ProveBatchRequest {
    /// Returns the attested batch payload: `prev_block_hash || next_block_hash`.
    pub fn verifier_config(&self) -> [u8; 64] {
        let mut payload = [0u8; 64];
        payload[..32].copy_from_slice(&self.prev_block_hash);
        payload[32..].copy_from_slice(&self.next_block_hash);
        payload
    }
}

mod hex_32 {
    use super::*;
    use serde::{Deserializer, Serializer, de::Error as _};

    pub(crate) fn serialize<S>(value: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&const_hex::encode_prefixed(value))
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 32], D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        let mut value = [0u8; 32];
        const_hex::decode_to_slice(encoded, &mut value).map_err(D::Error::custom)?;
        Ok(value)
    }
}

mod hex_bytes {
    use super::*;
    use serde::{Deserializer, Serializer, de::Error as _};

    pub(crate) fn serialize<S>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&const_hex::encode_prefixed(value))
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        const_hex::decode(encoded).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_config_concatenates_prev_and_next_hashes() {
        let request = ProveBatchRequest {
            prev_block_hash: [0x11; 32],
            next_block_hash: [0x22; 32],
        };

        let payload = request.verifier_config();

        assert_eq!(&payload[..32], &[0x11; 32]);
        assert_eq!(&payload[32..], &[0x22; 32]);
    }

    #[test]
    fn request_json_uses_prefixed_hex_strings() {
        let request = ProveBatchRequest {
            prev_block_hash: [0x11; 32],
            next_block_hash: [0x22; 32],
        };

        let encoded = serde_json::to_string(&request).expect("serialize request");
        let decoded: ProveBatchRequest = serde_json::from_str(&encoded).expect("decode request");

        assert_eq!(decoded, request);
        assert!(encoded.contains("0x11111111"));
        assert!(encoded.contains("0x22222222"));
    }
}
