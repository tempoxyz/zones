use serde::{Deserialize, Serialize};

pub const SETTLEMENT_SERVICE_ID: &str = "handoff-settlement-v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetaResponse {
    pub identity_verifying_key: String,
    pub settlement_verifying_key: String,
    pub settlement_service: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipientCert {
    pub handle_hash: String,
    pub route_root: String,
    pub settlement_service: String,
    pub valid_until: u64,
    pub seq_no: u64,
    pub recipient_verifying_key: String,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveToken {
    pub recipient_id: String,
    pub handle_hash: String,
    pub route_root: String,
    pub asset: String,
    pub amount: String,
    pub expires_at: u64,
    pub nonce: String,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteIntent {
    pub route_id: String,
    pub leaf_hash: String,
    pub route_root: String,
    pub asset: String,
    pub amount: String,
    pub expires_at: u64,
    pub settlement_service: String,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteProof {
    pub leaf_hash: String,
    pub merkle_proof: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartRegistrationRequest {
    pub email: String,
    pub recipient_verifying_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartRegistrationResponse {
    pub recipient_id: String,
    pub verification_code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteRegistrationRequest {
    pub recipient_id: String,
    pub verification_code: String,
    pub cert: RecipientCert,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkRecipientRequest {
    pub recipient_id: String,
    pub zone_address: String,
    pub route_secret: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveRequest {
    pub email: String,
    pub asset: String,
    pub amount: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveResponse {
    pub recipient_id: String,
    pub cert: RecipientCert,
    pub resolve_token: ResolveToken,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MintRouteRequest {
    pub resolve_token: ResolveToken,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncryptedPayloadResponse {
    pub ephemeral_pubkey_x: String,
    pub ephemeral_pubkey_y_parity: u8,
    pub ciphertext: String,
    pub nonce: String,
    pub tag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MintRouteResponse {
    pub route_intent: RouteIntent,
    pub route_proof: RouteProof,
    pub portal_address: String,
    pub token_address: String,
    pub key_index: String,
    pub encrypted_payload: EncryptedPayloadResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusResponse {
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}
