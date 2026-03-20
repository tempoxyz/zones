use std::collections::{HashMap, HashSet};

use alloy::{
    primitives::{Address, B256, U256},
    providers::ProviderBuilder,
};
use axum::http::StatusCode;
use ed25519_dalek::SigningKey;
use thiserror::Error;

use crate::{
    chain::{ZonePortal, normalize_http_rpc},
    crypto::{
        generate_signing_key, hash_handle, now_unix, random_id, recipient_cert_message,
        resolve_token_message, route_intent_message, route_leaf_hash, sign_message_hex,
        signing_key_verifying_hex, verification_code, verify_message_hex,
    },
    model::{
        CompleteRegistrationRequest, EncryptedPayloadResponse, LinkRecipientRequest, MetaResponse,
        MintRouteRequest, MintRouteResponse, RecipientCert, ResolveRequest, ResolveResponse,
        ResolveToken, RouteIntent, RouteProof, SETTLEMENT_SERVICE_ID, StartRegistrationRequest,
        StartRegistrationResponse, StatusResponse,
    },
};

#[derive(Debug, Clone)]
pub struct StoreConfig {
    pub l1_rpc_url: String,
    pub portal_address: Address,
    pub token_address: Address,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Unauthorized(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Internal(String),
}

impl StoreError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

#[derive(Debug, Clone)]
struct PendingRegistration {
    email: String,
    recipient_id: String,
    verification_code: String,
    recipient_verifying_key: String,
}

#[derive(Debug, Clone)]
struct ActiveRecipient {
    email: String,
    recipient_id: String,
    cert: RecipientCert,
}

#[derive(Debug, Clone)]
struct LinkedRecipient {
    zone_address: Address,
    route_root: String,
}

pub struct Stores {
    config: StoreConfig,
    identity_signing_key: SigningKey,
    settlement_signing_key: SigningKey,
    pending_registrations: HashMap<String, PendingRegistration>,
    active_recipients_by_email: HashMap<String, ActiveRecipient>,
    active_recipients_by_id: HashMap<String, ActiveRecipient>,
    linked_recipients_by_id: HashMap<String, LinkedRecipient>,
    used_resolve_nonces: HashSet<String>,
}

impl Stores {
    pub fn new(config: StoreConfig) -> Self {
        Self {
            config,
            identity_signing_key: generate_signing_key(),
            settlement_signing_key: generate_signing_key(),
            pending_registrations: HashMap::new(),
            active_recipients_by_email: HashMap::new(),
            active_recipients_by_id: HashMap::new(),
            linked_recipients_by_id: HashMap::new(),
            used_resolve_nonces: HashSet::new(),
        }
    }

    pub fn meta(&self) -> MetaResponse {
        MetaResponse {
            identity_verifying_key: signing_key_verifying_hex(&self.identity_signing_key),
            settlement_verifying_key: signing_key_verifying_hex(&self.settlement_signing_key),
            settlement_service: SETTLEMENT_SERVICE_ID.to_string(),
        }
    }

    pub fn start_registration(
        &mut self,
        request: StartRegistrationRequest,
    ) -> Result<StartRegistrationResponse, StoreError> {
        let email = request.email.trim().to_string();
        if email.is_empty() {
            return Err(StoreError::BadRequest(
                "email must not be empty".to_string(),
            ));
        }
        if self.active_recipients_by_email.contains_key(&email) {
            return Err(StoreError::Conflict(
                "email is already registered".to_string(),
            ));
        }

        let recipient_id = random_id("rec");
        let verification_code = verification_code();
        self.pending_registrations.insert(
            recipient_id.clone(),
            PendingRegistration {
                email,
                recipient_id: recipient_id.clone(),
                verification_code: verification_code.clone(),
                recipient_verifying_key: request.recipient_verifying_key,
            },
        );

        Ok(StartRegistrationResponse {
            recipient_id,
            verification_code,
        })
    }

    pub fn complete_registration(
        &mut self,
        request: CompleteRegistrationRequest,
    ) -> Result<StatusResponse, StoreError> {
        let pending = self
            .pending_registrations
            .get(&request.recipient_id)
            .cloned()
            .ok_or_else(|| {
                StoreError::NotFound("pending registration was not found".to_string())
            })?;

        if pending.recipient_id != request.recipient_id {
            return Err(StoreError::BadRequest(
                "recipient id mismatch during registration".to_string(),
            ));
        }
        if pending.verification_code != request.verification_code {
            return Err(StoreError::Unauthorized(
                "verification code did not match".to_string(),
            ));
        }
        if request.cert.recipient_verifying_key != pending.recipient_verifying_key {
            return Err(StoreError::BadRequest(
                "recipient cert uses a different verifying key than the registration".to_string(),
            ));
        }
        if request.cert.handle_hash != hash_handle(&pending.email) {
            return Err(StoreError::BadRequest(
                "recipient cert does not match the verified email".to_string(),
            ));
        }
        if request.cert.settlement_service != SETTLEMENT_SERVICE_ID {
            return Err(StoreError::BadRequest(
                "recipient cert references an unsupported settlement service".to_string(),
            ));
        }
        if request.cert.valid_until <= now_unix() {
            return Err(StoreError::BadRequest(
                "recipient cert has already expired".to_string(),
            ));
        }

        let message = recipient_cert_message(
            &request.cert.handle_hash,
            &request.cert.route_root,
            &request.cert.settlement_service,
            request.cert.valid_until,
            request.cert.seq_no,
        );
        verify_message_hex(
            &request.cert.recipient_verifying_key,
            &message,
            &request.cert.signature,
        )
        .map_err(|error| StoreError::BadRequest(error.to_string()))?;

        let active = ActiveRecipient {
            email: pending.email.clone(),
            recipient_id: request.recipient_id.clone(),
            cert: request.cert,
        };
        self.pending_registrations.remove(&request.recipient_id);
        self.active_recipients_by_email
            .insert(pending.email, active.clone());
        self.active_recipients_by_id
            .insert(request.recipient_id, active);

        Ok(StatusResponse {
            status: "verified".to_string(),
        })
    }

    pub fn link_recipient(
        &mut self,
        request: LinkRecipientRequest,
    ) -> Result<StatusResponse, StoreError> {
        let recipient = self
            .active_recipients_by_id
            .get(&request.recipient_id)
            .cloned()
            .ok_or_else(|| {
                StoreError::NotFound(
                    "recipient must finish registration before linking a destination".to_string(),
                )
            })?;

        let zone_address: Address = request.zone_address.parse().map_err(|_| {
            StoreError::BadRequest("zone address is not a valid 0x-prefixed address".to_string())
        })?;
        // The server never stores the public email as the settlement target. It only accepts
        // the hidden destination if the caller can open the recipient's signed route commitment.
        let route_root = route_leaf_hash(&format!("{zone_address:#x}"), &request.route_secret);
        if route_root != recipient.cert.route_root {
            return Err(StoreError::BadRequest(
                "linked zone destination does not match the recipient route commitment".to_string(),
            ));
        }

        self.linked_recipients_by_id.insert(
            request.recipient_id,
            LinkedRecipient {
                zone_address,
                route_root,
            },
        );

        Ok(StatusResponse {
            status: "linked".to_string(),
        })
    }

    pub fn resolve(&mut self, request: ResolveRequest) -> Result<ResolveResponse, StoreError> {
        let email = request.email.trim().to_string();
        let recipient = self
            .active_recipients_by_email
            .get(&email)
            .cloned()
            .ok_or_else(|| StoreError::NotFound("recipient email was not found".to_string()))?;

        if recipient.email != email {
            return Err(StoreError::BadRequest(
                "recipient email lookup returned an inconsistent record".to_string(),
            ));
        }
        if recipient.cert.valid_until <= now_unix() {
            return Err(StoreError::BadRequest(
                "recipient cert is no longer valid".to_string(),
            ));
        }
        if request.asset != self.expected_asset() {
            return Err(StoreError::BadRequest(format!(
                "only {} is supported by this demo server",
                self.expected_asset()
            )));
        }
        parse_amount(&request.amount)?;

        let expires_at = now_unix() + 300;
        let nonce = random_id("resolve");
        let message = resolve_token_message(
            &recipient.recipient_id,
            &recipient.cert.handle_hash,
            &recipient.cert.route_root,
            &request.asset,
            &request.amount,
            expires_at,
            &nonce,
        );
        let signature = sign_message_hex(&self.identity_signing_key, &message);
        let resolve_token = ResolveToken {
            recipient_id: recipient.recipient_id.clone(),
            handle_hash: recipient.cert.handle_hash.clone(),
            route_root: recipient.cert.route_root.clone(),
            asset: request.asset,
            amount: request.amount,
            expires_at,
            nonce,
            signature,
        };

        Ok(ResolveResponse {
            recipient_id: recipient.recipient_id,
            cert: recipient.cert,
            resolve_token,
        })
    }

    pub async fn mint_route(
        &mut self,
        request: MintRouteRequest,
    ) -> Result<MintRouteResponse, StoreError> {
        let token = request.resolve_token;
        if token.expires_at <= now_unix() {
            return Err(StoreError::BadRequest(
                "resolve token has expired".to_string(),
            ));
        }
        if token.asset != self.expected_asset() {
            return Err(StoreError::BadRequest(format!(
                "resolve token asset does not match configured token {}",
                self.expected_asset()
            )));
        }

        let active = self
            .active_recipients_by_id
            .get(&token.recipient_id)
            .cloned()
            .ok_or_else(|| StoreError::NotFound("recipient id was not found".to_string()))?;
        let linked = self
            .linked_recipients_by_id
            .get(&token.recipient_id)
            .cloned()
            .ok_or_else(|| {
                StoreError::NotFound(
                    "recipient does not have a linked hidden zone destination".to_string(),
                )
            })?;

        if token.handle_hash != active.cert.handle_hash {
            return Err(StoreError::BadRequest(
                "resolve token handle hash does not match the active recipient".to_string(),
            ));
        }
        if token.route_root != active.cert.route_root || token.route_root != linked.route_root {
            return Err(StoreError::BadRequest(
                "resolve token route root does not match the linked recipient commitment"
                    .to_string(),
            ));
        }

        let message = resolve_token_message(
            &token.recipient_id,
            &token.handle_hash,
            &token.route_root,
            &token.asset,
            &token.amount,
            token.expires_at,
            &token.nonce,
        );
        verify_message_hex(
            &signing_key_verifying_hex(&self.identity_signing_key),
            &message,
            &token.signature,
        )
        .map_err(|error| StoreError::BadRequest(error.to_string()))?;

        if !self.used_resolve_nonces.insert(token.nonce.clone()) {
            return Err(StoreError::Conflict(
                "resolve token has already been used".to_string(),
            ));
        }

        let amount = parse_amount(&token.amount)?;
        let provider = ProviderBuilder::new().connect_http(
            normalize_http_rpc(&self.config.l1_rpc_url)
                .parse()
                .map_err(|error| {
                    StoreError::Internal(format!("invalid L1 RPC URL in server config: {error}"))
                })?,
        );
        let portal = ZonePortal::new(self.config.portal_address, &provider);

        let key_result = portal
            .sequencerEncryptionKey()
            .call()
            .await
            .map_err(|error| {
                StoreError::Internal(format!(
                    "failed to fetch the portal encryption key: {error:#}"
                ))
            })?;
        let key_count = portal.encryptionKeyCount().call().await.map_err(|error| {
            StoreError::Internal(format!(
                "failed to fetch the portal encryption key count: {error:#}"
            ))
        })?;
        if key_count == U256::ZERO {
            return Err(StoreError::Internal(
                "portal does not have an encryption key".to_string(),
            ));
        }
        let key_index = key_count - U256::from(1);

        // This is the real settlement handoff: the server turns the signed recipient commitment
        // into a fresh encrypted ZonePortal payload without revealing the hidden zone address
        // back to the sender.
        let encrypted = zone_precompiles::ecies::encrypt_deposit(
            &key_result.x,
            key_result.yParity,
            linked.zone_address,
            B256::ZERO,
            self.config.portal_address,
            key_index,
        )
        .ok_or_else(|| {
            StoreError::Internal("failed to ECIES-encrypt the zone deposit".to_string())
        })?;

        let route_id = random_id("route");
        let expires_at = now_unix() + 300;
        let leaf_hash = linked.route_root.clone();
        let intent_message = route_intent_message(
            &route_id,
            &leaf_hash,
            &token.route_root,
            &token.asset,
            &token.amount,
            expires_at,
            SETTLEMENT_SERVICE_ID,
        );
        let signature = sign_message_hex(&self.settlement_signing_key, &intent_message);
        let route_intent = RouteIntent {
            route_id,
            leaf_hash: leaf_hash.clone(),
            route_root: token.route_root,
            asset: token.asset,
            amount: amount.to_string(),
            expires_at,
            settlement_service: SETTLEMENT_SERVICE_ID.to_string(),
            signature,
        };

        Ok(MintRouteResponse {
            route_intent,
            route_proof: RouteProof {
                leaf_hash,
                merkle_proof: Vec::new(),
            },
            portal_address: format!("{:#x}", self.config.portal_address),
            token_address: self.expected_asset(),
            key_index: key_index.to_string(),
            encrypted_payload: EncryptedPayloadResponse {
                ephemeral_pubkey_x: format!("{:#x}", encrypted.eph_pub_x),
                ephemeral_pubkey_y_parity: encrypted.eph_pub_y_parity,
                ciphertext: prefixed_hex(&encrypted.ciphertext),
                nonce: prefixed_hex(encrypted.nonce),
                tag: prefixed_hex(encrypted.tag),
            },
        })
    }

    fn expected_asset(&self) -> String {
        format!("{:#x}", self.config.token_address)
    }
}

fn parse_amount(amount: &str) -> Result<u128, StoreError> {
    let value: u128 = amount
        .parse()
        .map_err(|_| StoreError::BadRequest("amount must be a base-10 u128 string".to_string()))?;
    if value == 0 {
        return Err(StoreError::BadRequest(
            "amount must be greater than zero".to_string(),
        ));
    }
    Ok(value)
}

fn prefixed_hex(bytes: impl AsRef<[u8]>) -> String {
    format!("0x{}", hex::encode(bytes))
}
