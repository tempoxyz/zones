use alloy::{
    primitives::{Address, B256, Bytes, FixedBytes, U256, address},
    providers::Provider,
    signers::{Signer, local::PrivateKeySigner},
    sol_types::SolValue,
};
use eyre::{WrapErr as _, eyre};
use k256::{AffinePoint, ProjectivePoint, Scalar, elliptic_curve::sec1::ToEncodedPoint};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read as _},
    path::{Path, PathBuf},
};
use tempo_alloy::TempoNetwork;
use zone::{
    abi::{
        EncryptedDepositPayload, SwapAndDepositRouterEncryptedCallback, ZoneFactory, ZonePortal,
    },
    precompiles::ecies::encrypt_deposit,
};

use crate::zone_utils::{MODERATO_ZONE_FACTORY, ZoneMetadata, check};

#[derive(Debug, Clone)]
pub(crate) struct ResolvedZone {
    pub(crate) reference: String,
    pub(crate) portal: Address,
    pub(crate) zone_id: Option<u32>,
    pub(crate) zone_dir: Option<PathBuf>,
    pub(crate) sequencer_key: Option<String>,
    pub(crate) router: Option<Address>,
}

#[derive(Debug, Clone)]
pub(crate) struct BuiltEncryptedDepositPayload {
    pub(crate) target_portal: Address,
    pub(crate) key_index: U256,
    pub(crate) encrypted_deposit_payload: EncryptedDepositPayload,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum EncryptionKeyMode<'a> {
    ReadOnly {
        expected_sequencer_private_key: Option<&'a str>,
    },
    EnsureRegistered {
        sequencer_private_key: &'a str,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BuiltEncryptedDepositPayloadJson {
    #[serde(rename = "targetPortal")]
    pub(crate) target_portal: String,
    #[serde(rename = "keyIndex")]
    pub(crate) key_index: String,
    #[serde(rename = "encryptedDepositPayload")]
    pub(crate) encrypted_deposit_payload: EncryptedDepositPayloadJson,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EncryptedDepositPayloadJson {
    #[serde(rename = "ephemeralPubkeyX")]
    pub(crate) ephemeral_pubkey_x: String,
    #[serde(rename = "ephemeralPubkeyYParity")]
    pub(crate) ephemeral_pubkey_y_parity: u8,
    pub(crate) ciphertext: String,
    pub(crate) nonce: String,
    pub(crate) tag: String,
}

impl BuiltEncryptedDepositPayloadJson {
    pub(crate) fn from_built(payload: &BuiltEncryptedDepositPayload) -> Self {
        Self {
            target_portal: payload.target_portal.to_string(),
            key_index: payload.key_index.to_string(),
            encrypted_deposit_payload: EncryptedDepositPayloadJson {
                ephemeral_pubkey_x: payload
                    .encrypted_deposit_payload
                    .ephemeralPubkeyX
                    .to_string(),
                ephemeral_pubkey_y_parity: payload.encrypted_deposit_payload.ephemeralPubkeyYParity,
                ciphertext: hex_string(payload.encrypted_deposit_payload.ciphertext.as_ref()),
                nonce: hex_string(payload.encrypted_deposit_payload.nonce.as_slice()),
                tag: hex_string(payload.encrypted_deposit_payload.tag.as_slice()),
            },
        }
    }

    pub(crate) fn into_built(self) -> eyre::Result<BuiltEncryptedDepositPayload> {
        let ciphertext = Bytes::from(decode_hex(&self.encrypted_deposit_payload.ciphertext)?);
        let nonce = fixed_bytes::<12>(&self.encrypted_deposit_payload.nonce)?;
        let tag = fixed_bytes::<16>(&self.encrypted_deposit_payload.tag)?;

        Ok(BuiltEncryptedDepositPayload {
            target_portal: self
                .target_portal
                .parse()
                .wrap_err("invalid targetPortal in payload JSON")?,
            key_index: self
                .key_index
                .parse()
                .wrap_err("invalid keyIndex in payload JSON")?,
            encrypted_deposit_payload: EncryptedDepositPayload {
                ephemeralPubkeyX: self
                    .encrypted_deposit_payload
                    .ephemeral_pubkey_x
                    .parse()
                    .wrap_err("invalid ephemeralPubkeyX in payload JSON")?,
                ephemeralPubkeyYParity: self.encrypted_deposit_payload.ephemeral_pubkey_y_parity,
                ciphertext,
                nonce,
                tag,
            },
        })
    }
}

pub(crate) fn payload_json_to_string(
    payload: &BuiltEncryptedDepositPayload,
) -> eyre::Result<String> {
    serde_json::to_string_pretty(&BuiltEncryptedDepositPayloadJson::from_built(payload))
        .wrap_err("failed to encode payload JSON")
}

pub(crate) fn read_payload_json(path: &str) -> eyre::Result<BuiltEncryptedDepositPayload> {
    let contents = if path == "-" {
        let mut input = String::new();
        io::stdin()
            .read_to_string(&mut input)
            .wrap_err("failed to read payload JSON from stdin")?;
        input
    } else {
        fs::read_to_string(path)
            .wrap_err_with(|| format!("failed reading payload JSON from {path}"))?
    };

    let json: BuiltEncryptedDepositPayloadJson =
        serde_json::from_str(&contents).wrap_err("failed to parse payload JSON")?;
    json.into_built()
}

pub(crate) fn parse_private_key(private_key: &str) -> eyre::Result<PrivateKeySigner> {
    private_key
        .strip_prefix("0x")
        .unwrap_or(private_key)
        .parse()
        .wrap_err("invalid private key")
}

pub(crate) fn signer_address_from_private_key(private_key: &str) -> eyre::Result<Address> {
    Ok(parse_private_key(private_key)?.address())
}

pub(crate) fn resolve_token_ref(reference: &str) -> eyre::Result<Address> {
    let normalized = reference.to_ascii_lowercase();
    match normalized.as_str() {
        "pathusd" | "path-usd" | "path_usd" => {
            Ok(address!("0x20C0000000000000000000000000000000000000"))
        }
        "alphausd" | "alpha-usd" | "alpha_usd" => {
            Ok(address!("0x20c0000000000000000000000000000000000001"))
        }
        "betausd" | "beta-usd" | "beta_usd" => {
            Ok(address!("0x20c0000000000000000000000000000000000002"))
        }
        _ => reference
            .parse()
            .wrap_err_with(|| format!("invalid token reference: {reference}")),
    }
}

pub(crate) async fn resolve_zone_ref<P: Provider<TempoNetwork>>(
    reference: &str,
    l1: &P,
    zone_factory: Option<Address>,
) -> eyre::Result<ResolvedZone> {
    resolve_zone_ref_in(reference, Path::new("generated"), l1, zone_factory).await
}

pub(crate) async fn resolve_zone_ref_in<P: Provider<TempoNetwork>>(
    reference: &str,
    generated_root: &Path,
    l1: &P,
    zone_factory: Option<Address>,
) -> eyre::Result<ResolvedZone> {
    if reference.starts_with("0x") {
        let portal: Address = reference
            .parse()
            .wrap_err_with(|| format!("invalid portal address: {reference}"))?;
        if let Some(local) = find_local_zone_by_portal(generated_root, portal)? {
            return Ok(local);
        }

        return Ok(ResolvedZone {
            reference: reference.to_string(),
            portal,
            zone_id: None,
            zone_dir: None,
            sequencer_key: None,
            router: None,
        });
    }

    if let Ok(zone_id) = reference.parse::<u32>() {
        if let Some(local) = find_local_zone_by_zone_id(generated_root, zone_id)? {
            return Ok(local);
        }

        let factory = ZoneFactory::new(zone_factory.unwrap_or(MODERATO_ZONE_FACTORY), l1);
        let info = factory
            .zones(zone_id)
            .call()
            .await
            .wrap_err_with(|| format!("failed to resolve zone ID {zone_id} via ZoneFactory"))?;
        if info.portal == Address::ZERO {
            return Err(eyre!("zone {zone_id} does not exist"));
        }

        return Ok(ResolvedZone {
            reference: reference.to_string(),
            portal: info.portal,
            zone_id: Some(zone_id),
            zone_dir: None,
            sequencer_key: None,
            router: None,
        });
    }

    load_named_zone(reference, generated_root)
}

pub(crate) fn build_encrypted_router_callback(
    token_out: Address,
    payload: &BuiltEncryptedDepositPayload,
    min_amount_out: u128,
) -> Bytes {
    let callback = SwapAndDepositRouterEncryptedCallback {
        token_out,
        target_portal: payload.target_portal,
        key_index: payload.key_index,
        encrypted: payload.encrypted_deposit_payload.clone(),
        min_amount_out,
    };

    Bytes::from(callback.abi_encode())
}

pub(crate) async fn build_encrypted_deposit_payload<P: Provider<TempoNetwork>>(
    l1: &P,
    target_portal: Address,
    recipient: Address,
    memo: B256,
    key_mode: EncryptionKeyMode<'_>,
) -> eyre::Result<BuiltEncryptedDepositPayload> {
    let portal = ZonePortal::new(target_portal, l1);
    let (key, key_index) = match key_mode {
        EncryptionKeyMode::ReadOnly {
            expected_sequencer_private_key,
        } => {
            let (key, key_index) = fetch_active_encryption_key(&portal).await?;
            if let Some(private_key) = expected_sequencer_private_key {
                validate_expected_sequencer_key(private_key, &key)?;
            }
            (key, key_index)
        }
        EncryptionKeyMode::EnsureRegistered {
            sequencer_private_key,
        } => ensure_sequencer_encryption_key(&portal, target_portal, sequencer_private_key).await?,
    };

    let y_parity = key.normalized_y_parity().ok_or_else(|| {
        eyre!(
            "unexpected yParity {:#x}, expected 0/1 or 0x02/0x03",
            key.yParity
        )
    })?;

    let encrypted = encrypt_deposit(&key.x, y_parity, recipient, memo, target_portal, key_index)
        .ok_or_else(|| eyre!("ECIES encryption failed — invalid active portal key?"))?;

    Ok(BuiltEncryptedDepositPayload {
        target_portal,
        key_index,
        encrypted_deposit_payload: EncryptedDepositPayload {
            ephemeralPubkeyX: encrypted.eph_pub_x,
            ephemeralPubkeyYParity: encrypted.eph_pub_y_parity,
            ciphertext: Bytes::from(encrypted.ciphertext),
            nonce: encrypted.nonce.into(),
            tag: encrypted.tag.into(),
        },
    })
}

pub(crate) async fn build_encrypted_deposit_payload_for_zone<P: Provider<TempoNetwork>>(
    l1: &P,
    target_zone: &ResolvedZone,
    recipient: Address,
    memo: B256,
) -> eyre::Result<BuiltEncryptedDepositPayload> {
    build_encrypted_deposit_payload(
        l1,
        target_zone.portal,
        recipient,
        memo,
        EncryptionKeyMode::ReadOnly {
            expected_sequencer_private_key: target_zone.sequencer_key.as_deref(),
        },
    )
    .await
}

pub(crate) async fn fetch_active_encryption_key<P: Provider<TempoNetwork>>(
    portal: &ZonePortal::ZonePortalInstance<&P, TempoNetwork>,
) -> eyre::Result<(ZonePortal::sequencerEncryptionKeyReturn, U256)> {
    let key_count = portal
        .encryptionKeyCount()
        .call()
        .await
        .wrap_err("failed to read portal encryption key count")?;
    if key_count == U256::ZERO {
        return Err(eyre!("no active portal encryption key is registered"));
    }

    let key = portal
        .sequencerEncryptionKey()
        .call()
        .await
        .wrap_err("failed to read the active portal encryption key")?;
    let key_index = key_count - U256::from(1);
    Ok((key, key_index))
}

pub(crate) async fn ensure_sequencer_encryption_key<P: Provider<TempoNetwork>>(
    portal: &ZonePortal::ZonePortalInstance<&P, TempoNetwork>,
    portal_address: Address,
    sequencer_private_key: &str,
) -> eyre::Result<(ZonePortal::sequencerEncryptionKeyReturn, U256)> {
    let (expected_x, expected_y_parity) = derive_encryption_public_key(sequencer_private_key)
        .wrap_err("failed to derive the sequencer encryption public key from SEQUENCER_KEY")?;
    let key_count = portal
        .encryptionKeyCount()
        .call()
        .await
        .wrap_err("failed to read portal encryption key count")?;

    let needs_registration = if key_count == U256::ZERO {
        true
    } else {
        let current_key = portal
            .sequencerEncryptionKey()
            .call()
            .await
            .wrap_err("failed to read the active sequencer encryption key")?;
        let current_y_parity = current_key.normalized_y_parity().ok_or_else(|| {
            eyre!(
                "unexpected portal yParity {:#x}, expected 0/1 or 0x02/0x03",
                current_key.yParity
            )
        })?;
        current_key.x != expected_x || current_y_parity != expected_y_parity
    };

    if needs_registration {
        register_sequencer_encryption_key(portal, portal_address, sequencer_private_key).await?;
    }

    fetch_active_encryption_key(portal).await
}

pub(crate) fn derive_encryption_public_key(
    sequencer_private_key: &str,
) -> eyre::Result<(B256, u8)> {
    let key_str = sequencer_private_key
        .strip_prefix("0x")
        .unwrap_or(sequencer_private_key);
    let enc_key = k256::SecretKey::from_slice(&const_hex::decode(key_str)?)?;
    let scalar: Scalar = *enc_key.to_nonzero_scalar();
    let pub_point = AffinePoint::from(ProjectivePoint::GENERATOR * scalar);
    let encoded = pub_point.to_encoded_point(true);
    let x = B256::from_slice(encoded.x().unwrap().as_slice());
    let y_parity = encoded.as_bytes()[0];
    Ok((x, y_parity))
}

fn validate_expected_sequencer_key(
    sequencer_private_key: &str,
    active_key: &ZonePortal::sequencerEncryptionKeyReturn,
) -> eyre::Result<()> {
    let (expected_x, expected_y_parity) = derive_encryption_public_key(sequencer_private_key)
        .wrap_err("failed to derive the expected local sequencer encryption public key")?;
    let active_y_parity = active_key.normalized_y_parity().ok_or_else(|| {
        eyre!(
            "unexpected portal yParity {:#x}, expected 0/1 or 0x02/0x03",
            active_key.yParity
        )
    })?;
    if active_key.x != expected_x || active_y_parity != expected_y_parity {
        return Err(eyre!(
            "active portal encryption key does not match the local sequencerKey metadata"
        ));
    }
    Ok(())
}

async fn register_sequencer_encryption_key<P: Provider<TempoNetwork>>(
    portal: &ZonePortal::ZonePortalInstance<&P, TempoNetwork>,
    portal_address: Address,
    sequencer_private_key: &str,
) -> eyre::Result<()> {
    let (x, y_parity) = derive_encryption_public_key(sequencer_private_key)
        .wrap_err("failed to derive the sequencer encryption public key")?;
    let signer = parse_private_key(sequencer_private_key)?;
    let message =
        alloy::primitives::keccak256((portal_address, x, U256::from(y_parity)).abi_encode());
    let sig = signer
        .sign_hash(&message)
        .await
        .wrap_err("failed to sign the encryption key proof-of-possession")?;
    let pop_v = sig.v() as u8 + 27;
    let pop_r = B256::from(sig.r().to_be_bytes::<32>());
    let pop_s = B256::from(sig.s().to_be_bytes::<32>());

    let receipt = portal
        .setSequencerEncryptionKey(x, y_parity, pop_v, pop_r, pop_s)
        .send_sync()
        .await
        .wrap_err("failed to send setSequencerEncryptionKey")?;
    check(&receipt, "setSequencerEncryptionKey")
}

fn load_named_zone(reference: &str, generated_root: &Path) -> eyre::Result<ResolvedZone> {
    let zone_dir = generated_root.join(reference);
    if !zone_dir.join("zone.json").is_file() {
        return Err(eyre!(
            "{} not found. Expected {}",
            reference,
            zone_dir.join("zone.json").display()
        ));
    }
    load_zone_from_dir(reference.to_string(), &zone_dir)
}

fn find_local_zone_by_portal(
    generated_root: &Path,
    portal: Address,
) -> eyre::Result<Option<ResolvedZone>> {
    find_single_local_zone(generated_root, &format!("portal {portal}"), |zone| {
        Ok(zone.portal == portal)
    })
}

fn find_local_zone_by_zone_id(
    generated_root: &Path,
    zone_id: u32,
) -> eyre::Result<Option<ResolvedZone>> {
    find_single_local_zone(generated_root, &format!("zone ID {zone_id}"), |zone| {
        Ok(zone.zone_id == Some(zone_id))
    })
}

fn find_single_local_zone<F>(
    generated_root: &Path,
    description: &str,
    mut predicate: F,
) -> eyre::Result<Option<ResolvedZone>>
where
    F: FnMut(&ResolvedZone) -> eyre::Result<bool>,
{
    let mut matches = Vec::new();
    for zone in list_local_zones(generated_root)? {
        if predicate(&zone)? {
            matches.push(zone);
        }
    }

    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        _ => Err(eyre!(
            "multiple local zones matched {description}: {}",
            matches
                .iter()
                .map(|zone| zone
                    .zone_dir
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| zone.reference.clone()))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn list_local_zones(generated_root: &Path) -> eyre::Result<Vec<ResolvedZone>> {
    let mut zones = Vec::new();
    if !generated_root.exists() {
        return Ok(zones);
    }

    for entry in fs::read_dir(generated_root)
        .wrap_err_with(|| format!("failed reading {}", generated_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let zone_dir = entry.path();
        if !zone_dir.join("zone.json").is_file() {
            continue;
        }
        zones.push(load_zone_from_dir(
            entry.file_name().to_string_lossy().into_owned(),
            &zone_dir,
        )?);
    }

    Ok(zones)
}

fn load_zone_from_dir(reference: String, zone_dir: &Path) -> eyre::Result<ResolvedZone> {
    let metadata = ZoneMetadata::load(zone_dir)?;
    let portal = metadata.get_required_address("portal")?;
    Ok(ResolvedZone {
        reference,
        portal,
        zone_id: metadata.get_optional_u32("zoneId")?,
        zone_dir: zone_dir
            .canonicalize()
            .ok()
            .or_else(|| Some(zone_dir.to_path_buf())),
        sequencer_key: metadata.get_optional_string("sequencerKey"),
        router: metadata.get_optional_address("swapAndDepositRouter")?,
    })
}

fn decode_hex(value: &str) -> eyre::Result<Vec<u8>> {
    const_hex::decode(value.trim_start_matches("0x"))
        .wrap_err_with(|| format!("invalid hex string: {value}"))
}

fn fixed_bytes<const N: usize>(value: &str) -> eyre::Result<FixedBytes<N>> {
    let bytes = decode_hex(value)?;
    if bytes.len() != N {
        return Err(eyre!("expected {N} bytes, got {}", bytes.len()));
    }
    Ok(FixedBytes::<N>::from_slice(&bytes))
}

fn hex_string(bytes: &[u8]) -> String {
    format!("0x{}", const_hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::providers::ProviderBuilder;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_generated_root() -> eyre::Result<PathBuf> {
        let root = std::env::temp_dir().join(format!(
            "tempo-xtask-bridge-utils-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&root)?;
        Ok(root)
    }

    fn write_zone_json(
        generated_root: &Path,
        name: &str,
        zone_id: u32,
        portal: &str,
        sequencer_key: Option<&str>,
        router: Option<&str>,
    ) -> eyre::Result<()> {
        let zone_dir = generated_root.join(name);
        fs::create_dir_all(&zone_dir)?;
        let mut value = serde_json::json!({
            "zoneId": zone_id,
            "portal": portal,
        });
        if let Some(sequencer_key) = sequencer_key {
            value["sequencerKey"] = serde_json::Value::String(sequencer_key.to_string());
        }
        if let Some(router) = router {
            value["swapAndDepositRouter"] = serde_json::Value::String(router.to_string());
        }
        fs::write(
            zone_dir.join("zone.json"),
            serde_json::to_string_pretty(&value)?,
        )?;
        Ok(())
    }

    #[test]
    fn resolves_named_zone_from_generated_directory() -> eyre::Result<()> {
        let generated_root = make_generated_root()?;
        write_zone_json(
            &generated_root,
            "zone-a",
            1,
            "0x1000000000000000000000000000000000000001",
            None,
            Some("0x2000000000000000000000000000000000000002"),
        )?;

        let zone = load_named_zone("zone-a", &generated_root)?;
        assert_eq!(
            zone.portal,
            "0x1000000000000000000000000000000000000001".parse::<Address>()?
        );
        assert_eq!(zone.zone_id, Some(1));
        assert_eq!(
            zone.router,
            Some("0x2000000000000000000000000000000000000002".parse::<Address>()?)
        );
        Ok(())
    }

    #[tokio::test]
    async fn resolve_zone_ref_accepts_local_name() -> eyre::Result<()> {
        let generated_root = make_generated_root()?;
        write_zone_json(
            &generated_root,
            "zone-a",
            11,
            "0x1100000000000000000000000000000000000011",
            None,
            Some("0x2200000000000000000000000000000000000022"),
        )?;

        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_http("http://127.0.0.1:1".parse().unwrap());
        let zone = resolve_zone_ref_in("zone-a", &generated_root, &provider, None).await?;
        assert_eq!(zone.reference, "zone-a");
        assert_eq!(zone.zone_id, Some(11));
        assert_eq!(
            zone.portal,
            "0x1100000000000000000000000000000000000011".parse::<Address>()?
        );
        assert_eq!(
            zone.router,
            Some("0x2200000000000000000000000000000000000022".parse::<Address>()?)
        );
        Ok(())
    }

    #[test]
    fn resolves_zone_by_local_zone_id() -> eyre::Result<()> {
        let generated_root = make_generated_root()?;
        write_zone_json(
            &generated_root,
            "zone-a",
            7,
            "0x7000000000000000000000000000000000000007",
            None,
            None,
        )?;

        let zone = find_local_zone_by_zone_id(&generated_root, 7)?
            .ok_or_else(|| eyre!("expected a local zone match"))?;
        assert_eq!(zone.reference, "zone-a");
        assert_eq!(
            zone.portal,
            "0x7000000000000000000000000000000000000007".parse::<Address>()?
        );
        Ok(())
    }

    #[tokio::test]
    async fn resolve_zone_ref_accepts_local_zone_id() -> eyre::Result<()> {
        let generated_root = make_generated_root()?;
        write_zone_json(
            &generated_root,
            "zone-id-match",
            12,
            "0x1200000000000000000000000000000000000012",
            Some("0x1234"),
            None,
        )?;

        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_http("http://127.0.0.1:1".parse().unwrap());
        let zone = resolve_zone_ref_in("12", &generated_root, &provider, None).await?;
        assert_eq!(zone.reference, "zone-id-match");
        assert_eq!(zone.zone_id, Some(12));
        assert_eq!(
            zone.portal,
            "0x1200000000000000000000000000000000000012".parse::<Address>()?
        );
        Ok(())
    }

    #[test]
    fn resolves_zone_by_local_portal() -> eyre::Result<()> {
        let generated_root = make_generated_root()?;
        write_zone_json(
            &generated_root,
            "zone-b",
            8,
            "0x8000000000000000000000000000000000000008",
            Some("0x1234"),
            None,
        )?;

        let zone = find_local_zone_by_portal(
            &generated_root,
            "0x8000000000000000000000000000000000000008".parse()?,
        )?
        .ok_or_else(|| eyre!("expected a local zone match"))?;
        assert_eq!(zone.reference, "zone-b");
        assert_eq!(zone.sequencer_key.as_deref(), Some("0x1234"));
        Ok(())
    }

    #[tokio::test]
    async fn resolve_zone_ref_accepts_local_portal_address() -> eyre::Result<()> {
        let generated_root = make_generated_root()?;
        write_zone_json(
            &generated_root,
            "zone-portal-match",
            13,
            "0x1300000000000000000000000000000000000013",
            None,
            None,
        )?;

        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_http("http://127.0.0.1:1".parse().unwrap());
        let zone = resolve_zone_ref_in(
            "0x1300000000000000000000000000000000000013",
            &generated_root,
            &provider,
            None,
        )
        .await?;
        assert_eq!(zone.reference, "zone-portal-match");
        assert_eq!(zone.zone_id, Some(13));
        assert_eq!(
            zone.portal,
            "0x1300000000000000000000000000000000000013".parse::<Address>()?
        );
        Ok(())
    }

    #[test]
    fn payload_json_round_trips() -> eyre::Result<()> {
        let payload = BuiltEncryptedDepositPayload {
            target_portal: "0x9000000000000000000000000000000000000009".parse()?,
            key_index: U256::from(3),
            encrypted_deposit_payload: EncryptedDepositPayload {
                ephemeralPubkeyX:
                    "0x0000000000000000000000000000000000000000000000000000000000001234".parse()?,
                ephemeralPubkeyYParity: 2,
                ciphertext: Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]),
                nonce: FixedBytes::<12>::from_slice(&[0x11; 12]),
                tag: FixedBytes::<16>::from_slice(&[0x22; 16]),
            },
        };

        let json = payload_json_to_string(&payload)?;
        let reparsed: BuiltEncryptedDepositPayloadJson = serde_json::from_str(&json)?;
        let reparsed = reparsed.into_built()?;
        assert_eq!(reparsed.target_portal, payload.target_portal);
        assert_eq!(reparsed.key_index, payload.key_index);
        assert_eq!(
            reparsed.encrypted_deposit_payload.ciphertext,
            payload.encrypted_deposit_payload.ciphertext
        );
        Ok(())
    }

    #[test]
    fn resolves_token_aliases() -> eyre::Result<()> {
        assert_eq!(
            resolve_token_ref("pathusd")?,
            "0x20C0000000000000000000000000000000000000".parse::<Address>()?
        );
        assert_eq!(
            resolve_token_ref("alpha-usd")?,
            "0x20c0000000000000000000000000000000000001".parse::<Address>()?
        );
        assert_eq!(
            resolve_token_ref("0x20c0000000000000000000000000000000000002")?,
            "0x20c0000000000000000000000000000000000002".parse::<Address>()?
        );
        Ok(())
    }
}
