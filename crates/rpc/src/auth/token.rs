use alloy_primitives::{Address, B256, hex, keccak256};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::AuthError;

/// Magic prefix: "TempoZoneRPC" left-padded to 32 bytes.
const TEMPO_ZONE_RPC_MAGIC: [u8; 32] = {
    let mut buf = [0u8; 32];
    let s = b"TempoZoneRPC";
    let mut i = 0;
    while i < s.len() {
        buf[i] = s[i];
        i += 1;
    }
    buf
};

/// Size of the fixed token fields (version + zoneId + chainId + zonePortal + issuedAt +
/// expiresAt).
const TOKEN_FIELDS_LEN: usize = 1 + 4 + 8 + 20 + 8 + 8; // 49 bytes

/// HTTP header name for the authorization token.
pub const X_AUTHORIZATION_TOKEN: &str = "x-authorization-token";

/// The authenticated caller context extracted from a valid authorization token.
#[derive(Debug, Clone)]
pub struct AuthContext {
    /// The authenticated account address.
    pub caller: Address,
    /// Whether this caller is the sequencer.
    pub is_sequencer: bool,
    /// Token expiry timestamp (unix seconds).
    pub expires_at: u64,
}

/// Parsed authorization token fields (before signature verification).
///
/// The token is a hex-encoded blob: `<signature><version:1><zoneId:4><chainId:8><zonePortal:20><issuedAt:8><expiresAt:8>`.
/// The last 49 bytes are always the fixed fields; everything before is the variable-length signature.
///
/// See `docs/pages/protocol/privacy/rpc.md` — "Transport" and "Message" sections.
#[derive(Debug, Clone)]
pub struct AuthorizationToken {
    /// Spec version (must be 0).
    pub version: u8,
    /// Zone ID.
    pub zone_id: u32,
    /// Chain ID.
    pub chain_id: u64,
    /// ZonePortal address on Tempo L1.
    pub zone_portal: Address,
    /// Issuance timestamp (unix seconds).
    pub issued_at: u64,
    /// Expiry timestamp (unix seconds).
    pub expires_at: u64,
    /// The raw signature bytes (everything before the last 49 bytes).
    pub signature: Vec<u8>,
    /// The signing digest (keccak256 of the packed message).
    pub digest: B256,
}

impl AuthorizationToken {
    /// Parse the raw bytes of an authorization token blob.
    ///
    /// Does NOT verify the signature — call [`Self::validate`] and then recover the signer
    /// separately.
    pub fn parse(blob: &[u8]) -> Result<Self, AuthError> {
        if blob.len() < TOKEN_FIELDS_LEN + 1 {
            return Err(AuthError::TooShort);
        }

        let fields_start = blob.len() - TOKEN_FIELDS_LEN;
        let fields = &blob[fields_start..];
        let signature = blob[..fields_start].to_vec();

        let version = fields[0];
        let zone_id = u32::from_be_bytes(fields[1..5].try_into().unwrap());
        let chain_id = u64::from_be_bytes(fields[5..13].try_into().unwrap());
        let zone_portal = Address::from_slice(&fields[13..33]);
        let issued_at = u64::from_be_bytes(fields[33..41].try_into().unwrap());
        let expires_at = u64::from_be_bytes(fields[41..49].try_into().unwrap());

        // Build the signing digest
        let mut msg = Vec::with_capacity(32 + TOKEN_FIELDS_LEN);
        msg.extend_from_slice(&TEMPO_ZONE_RPC_MAGIC);
        msg.push(version);
        msg.extend_from_slice(&zone_id.to_be_bytes());
        msg.extend_from_slice(&chain_id.to_be_bytes());
        msg.extend_from_slice(zone_portal.as_slice());
        msg.extend_from_slice(&issued_at.to_be_bytes());
        msg.extend_from_slice(&expires_at.to_be_bytes());
        let digest = keccak256(&msg);

        Ok(Self {
            version,
            zone_id,
            chain_id,
            zone_portal,
            issued_at,
            expires_at,
            signature,
            digest,
        })
    }

    /// Validate token fields against the server's zone configuration.
    pub fn validate(
        &self,
        expected_zone_id: u32,
        expected_chain_id: u64,
        expected_portal: Address,
    ) -> Result<(), AuthError> {
        if self.version != 0 {
            return Err(AuthError::UnsupportedVersion(self.version));
        }
        if self.zone_id != expected_zone_id {
            return Err(AuthError::ZoneIdMismatch);
        }
        if self.chain_id != expected_chain_id {
            return Err(AuthError::ChainIdMismatch);
        }
        if self.zone_portal != expected_portal {
            return Err(AuthError::ZonePortalMismatch);
        }
        if self.expires_at.saturating_sub(self.issued_at) > 1800 {
            return Err(AuthError::WindowTooLarge);
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_secs();

        if self.expires_at <= now {
            return Err(AuthError::Expired);
        }
        if self.issued_at > now + 60 {
            return Err(AuthError::IssuedInFuture);
        }

        Ok(())
    }
}

/// Build the unsigned token fields and their signing digest.
///
/// Returns `(fields, digest)` where `fields` is the 49-byte suffix
/// and `digest` is the keccak256 hash to be signed.
pub fn build_token_fields(
    zone_id: u32,
    chain_id: u64,
    zone_portal: Address,
    issued_at: u64,
    expires_at: u64,
) -> ([u8; TOKEN_FIELDS_LEN], B256) {
    let mut fields = [0u8; TOKEN_FIELDS_LEN];
    fields[0] = 0; // version
    fields[1..5].copy_from_slice(&zone_id.to_be_bytes());
    fields[5..13].copy_from_slice(&chain_id.to_be_bytes());
    fields[13..33].copy_from_slice(zone_portal.as_slice());
    fields[33..41].copy_from_slice(&issued_at.to_be_bytes());
    fields[41..49].copy_from_slice(&expires_at.to_be_bytes());

    let mut msg = Vec::with_capacity(32 + TOKEN_FIELDS_LEN);
    msg.extend_from_slice(&TEMPO_ZONE_RPC_MAGIC);
    msg.extend_from_slice(&fields);
    let digest = keccak256(&msg);

    (fields, digest)
}

/// Parse a hex-encoded authorization token from the header value.
pub fn parse_auth_header(header_value: &str) -> Result<AuthorizationToken, AuthError> {
    let hex_str = header_value.strip_prefix("0x").unwrap_or(header_value);
    let blob = hex::decode(hex_str).map_err(|_| AuthError::InvalidHex)?;
    AuthorizationToken::parse(&blob)
}
