use std::process::Command;

use alloy::{
    primitives::{Address, address},
    signers::local::PrivateKeySigner,
    sol,
};
use anyhow::{Context, Result};

pub const DEFAULT_L1_RPC_URL: &str = "https://rpc.moderato.tempo.xyz";
pub const DEFAULT_ZONE_RPC_URL: &str = "http://127.0.0.1:8546";
pub const DEFAULT_TOKEN_ADDRESS: Address = address!("0x20C0000000000000000000000000000000000000");
pub const FIXED_DEPOSIT_GAS: u128 = 100_000;

sol! {
    struct EncryptedDepositPayload {
        bytes32 ephemeralPubkeyX;
        uint8 ephemeralPubkeyYParity;
        bytes ciphertext;
        bytes12 nonce;
        bytes16 tag;
    }

    #[sol(rpc)]
    contract ZonePortal {
        function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity);
        function encryptionKeyCount() external view returns (uint256);
        function zoneGasRate() external view returns (uint128);
        function depositEncrypted(
            address token,
            uint128 amount,
            uint256 keyIndex,
            EncryptedDepositPayload payload
        ) external;
    }

    #[sol(rpc)]
    contract TIP20Token {
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
        function symbol() external view returns (string);
    }
}

#[derive(Debug, Clone)]
pub struct DetectedZoneConfig {
    pub portal_address: String,
    pub l1_rpc_url: Option<String>,
    pub zone_id: Option<String>,
}

pub fn parse_private_key(private_key: &str) -> Result<PrivateKeySigner> {
    private_key
        .trim()
        .strip_prefix("0x")
        .unwrap_or(private_key.trim())
        .parse()
        .context("sender private key is not a valid hex-encoded secp256k1 key")
}

pub fn normalize_http_rpc(rpc_url: &str) -> String {
    rpc_url
        .replace("wss://", "https://")
        .replace("ws://", "http://")
}

pub fn detect_local_zone_config(zone_rpc_url: &str) -> Result<Option<DetectedZoneConfig>> {
    let zone_port = reqwest::Url::parse(zone_rpc_url)
        .ok()
        .and_then(|url| url.port_or_known_default());

    let output = Command::new("ps")
        .args(["-ax", "-o", "pid=,args="])
        .output()
        .context("failed to inspect local processes for a running tempo-zone node")?;
    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8(output.stdout).context("ps output was not valid UTF-8")?;
    for line in stdout.lines() {
        if !line.contains("tempo-zone node") {
            continue;
        }

        let tokens: Vec<&str> = line.split_whitespace().collect();
        let http_port = arg_value(&tokens, "--http.port")
            .and_then(|value| value.parse::<u16>().ok())
            .or(zone_port);

        if zone_port.is_some() && http_port != zone_port {
            continue;
        }

        if let Some(portal_address) = arg_value(&tokens, "--l1.portal-address") {
            return Ok(Some(DetectedZoneConfig {
                portal_address: portal_address.to_string(),
                l1_rpc_url: arg_value(&tokens, "--l1.rpc-url").map(str::to_string),
                zone_id: arg_value(&tokens, "--zone.id").map(str::to_string),
            }));
        }
    }

    Ok(None)
}

fn arg_value<'a>(tokens: &'a [&str], flag: &str) -> Option<&'a str> {
    for (index, token) in tokens.iter().enumerate() {
        if *token == flag {
            return tokens.get(index + 1).copied();
        }
        if let Some((name, value)) = token.split_once('=')
            && name == flag
        {
            return Some(value);
        }
    }
    None
}
