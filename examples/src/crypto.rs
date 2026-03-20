use anyhow::{Context, Result, anyhow, bail};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::Rng;
use rand_core::OsRng;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::model::{RecipientCert, ResolveToken, RouteIntent, SETTLEMENT_SERVICE_ID};

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should move forward")
        .as_secs()
}

pub fn generate_signing_key() -> SigningKey {
    SigningKey::generate(&mut OsRng)
}

pub fn verifying_key_hex(key: &VerifyingKey) -> String {
    hex::encode(key.to_bytes())
}

pub fn signing_key_verifying_hex(key: &SigningKey) -> String {
    verifying_key_hex(&key.verifying_key())
}

pub fn decode_verifying_key(hex_value: &str) -> Result<VerifyingKey> {
    let bytes = hex::decode(hex_value).context("verifying key is not valid hex")?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("verifying key must be 32 bytes"))?;
    VerifyingKey::from_bytes(&bytes).context("invalid ed25519 verifying key")
}

fn decode_signature(hex_value: &str) -> Result<Signature> {
    let bytes = hex::decode(hex_value).context("signature is not valid hex")?;
    let bytes: [u8; 64] = bytes
        .try_into()
        .map_err(|_| anyhow!("signature must be 64 bytes"))?;
    Ok(Signature::from_bytes(&bytes))
}

pub fn sign_message_hex(signing_key: &SigningKey, message: &str) -> String {
    let signature = signing_key.sign(message.as_bytes());
    hex::encode(signature.to_bytes())
}

pub fn verify_message_hex(
    verifying_key_hex: &str,
    message: &str,
    signature_hex: &str,
) -> Result<()> {
    let verifying_key = decode_verifying_key(verifying_key_hex)?;
    let signature = decode_signature(signature_hex)?;
    verifying_key
        .verify(message.as_bytes(), &signature)
        .context("signature verification failed")
}

pub fn hash_handle(email: &str) -> String {
    sha256_hex(format!("handoff:handle:v1|{}", email.trim()))
}

pub fn route_leaf_hash(inbox_id: &str, inbox_token: &str) -> String {
    sha256_hex(format!("handoff:leaf:v1|{inbox_id}|{inbox_token}"))
}

pub fn random_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

pub fn verification_code() -> String {
    rand::thread_rng().gen_range(100_000..=999_999).to_string()
}

pub fn sha256_hex<T: AsRef<[u8]>>(value: T) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value);
    hex::encode(hasher.finalize())
}

pub fn recipient_cert_message(
    handle_hash: &str,
    route_root: &str,
    settlement_service: &str,
    valid_until: u64,
    seq_no: u64,
) -> String {
    format!(
        "handoff:recipient-cert:v1|{handle_hash}|{route_root}|{settlement_service}|{valid_until}|{seq_no}"
    )
}

pub fn resolve_token_message(
    recipient_id: &str,
    handle_hash: &str,
    route_root: &str,
    asset: &str,
    amount: &str,
    expires_at: u64,
    nonce: &str,
) -> String {
    format!(
        "handoff:resolve-token:v1|{recipient_id}|{handle_hash}|{route_root}|{asset}|{amount}|{expires_at}|{nonce}"
    )
}

pub fn route_intent_message(
    route_id: &str,
    leaf_hash: &str,
    route_root: &str,
    asset: &str,
    amount: &str,
    expires_at: u64,
    settlement_service: &str,
) -> String {
    format!(
        "handoff:route-intent:v1|{route_id}|{leaf_hash}|{route_root}|{asset}|{amount}|{expires_at}|{settlement_service}"
    )
}

pub fn build_recipient_cert(
    email: &str,
    route_root: &str,
    recipient_signing_key: &SigningKey,
    valid_until: u64,
    seq_no: u64,
) -> RecipientCert {
    let handle_hash = hash_handle(email);
    let recipient_verifying_key = signing_key_verifying_hex(recipient_signing_key);
    let message = recipient_cert_message(
        &handle_hash,
        route_root,
        SETTLEMENT_SERVICE_ID,
        valid_until,
        seq_no,
    );
    let signature = sign_message_hex(recipient_signing_key, &message);

    RecipientCert {
        handle_hash,
        route_root: route_root.to_string(),
        settlement_service: SETTLEMENT_SERVICE_ID.to_string(),
        valid_until,
        seq_no,
        recipient_verifying_key,
        signature,
    }
}

pub fn verify_recipient_cert(cert: &RecipientCert, email: &str) -> Result<()> {
    let expected_hash = hash_handle(email);
    if cert.handle_hash != expected_hash {
        bail!("recipient cert handle hash does not match the resolved email");
    }

    if cert.settlement_service != SETTLEMENT_SERVICE_ID {
        bail!("recipient cert settlement service is not recognized");
    }

    if cert.valid_until <= now_unix() {
        bail!("recipient cert has expired");
    }

    let message = recipient_cert_message(
        &cert.handle_hash,
        &cert.route_root,
        &cert.settlement_service,
        cert.valid_until,
        cert.seq_no,
    );
    verify_message_hex(&cert.recipient_verifying_key, &message, &cert.signature)
}

pub fn verify_resolve_token(
    token: &ResolveToken,
    identity_verifying_key: &str,
    expected_recipient_id: &str,
    expected_amount: &str,
    expected_asset: &str,
) -> Result<()> {
    if token.recipient_id != expected_recipient_id {
        bail!("resolve token recipient id does not match");
    }
    if token.amount != expected_amount {
        bail!("resolve token amount does not match");
    }
    if token.asset != expected_asset {
        bail!("resolve token asset does not match");
    }
    if token.expires_at <= now_unix() {
        bail!("resolve token has expired");
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
    verify_message_hex(identity_verifying_key, &message, &token.signature)
}

pub fn verify_route_intent(
    intent: &RouteIntent,
    settlement_verifying_key: &str,
    expected_route_root: &str,
    expected_amount: &str,
    expected_asset: &str,
) -> Result<()> {
    if intent.route_root != expected_route_root {
        bail!("route root does not match the recipient commitment");
    }
    if intent.amount != expected_amount {
        bail!("route intent amount does not match");
    }
    if intent.asset != expected_asset {
        bail!("route intent asset does not match");
    }
    if intent.expires_at <= now_unix() {
        bail!("route intent has expired");
    }
    if intent.settlement_service != SETTLEMENT_SERVICE_ID {
        bail!("route intent settlement service is not recognized");
    }

    let message = route_intent_message(
        &intent.route_id,
        &intent.leaf_hash,
        &intent.route_root,
        &intent.asset,
        &intent.amount,
        intent.expires_at,
        &intent.settlement_service,
    );
    verify_message_hex(settlement_verifying_key, &message, &intent.signature)
}
