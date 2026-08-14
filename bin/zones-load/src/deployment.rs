use alloy::{
    primitives::Address,
    providers::{Provider, ProviderBuilder},
};
use eyre::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tempo_alloy::TempoNetwork;
use tempo_zone_contracts::{ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS, ZonePortal};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DeploymentContext {
    pub(crate) portal: Address,
    pub(crate) zone_id: u32,
    pub(crate) token: Address,
    pub(crate) l1_rpc_url: String,
    #[serde(default)]
    pub(crate) l1_chain_id: Option<u64>,
    pub(crate) zone_rpc_url: String,
    #[serde(default)]
    pub(crate) zone_chain_id: Option<u64>,
    pub(crate) zone_submit_rpc_url: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct DoctorReport {
    pub(crate) healthy: bool,
    pub(crate) deployment: DeploymentContext,
    pub(crate) checks: Vec<DoctorCheck>,
    pub(crate) missing_requirements: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct DoctorCheck {
    pub(crate) name: String,
    pub(crate) passed: bool,
    pub(crate) detail: String,
}

impl DeploymentContext {
    pub(crate) fn new(
        portal: Option<Address>,
        zone_id: Option<u32>,
        token: Option<Address>,
        l1_rpc_url: Option<String>,
        zone_rpc_url: Option<String>,
        zone_submit_rpc_url: Option<String>,
    ) -> Result<Self> {
        let portal = portal.ok_or_else(|| {
            eyre::eyre!(
                "--portal is required for doctor, run, and replay (or set L1_PORTAL_ADDRESS)"
            )
        })?;
        let zone_id = zone_id.ok_or_else(|| {
            eyre::eyre!(
                "--zone-id is required for doctor, run, and replay (or set ZONES_LOAD_ZONE_ID)"
            )
        })?;
        let token = token.ok_or_else(|| {
            eyre::eyre!("--token is required for doctor, run, and replay (or set ZONES_LOAD_TOKEN)")
        })?;
        ensure!(!portal.is_zero(), "--portal must not be the zero address");
        ensure!(zone_id != 0, "--zone-id must not be zero");
        ensure!(!token.is_zero(), "--token must not be the zero address");

        let l1_rpc_url = l1_rpc_url.filter(|url| !url.is_empty()).ok_or_else(|| {
            eyre::eyre!("L1 RPC is required; pass --l1-rpc-url or set L1_RPC_URL")
        })?;
        let zone_rpc_url = zone_rpc_url.filter(|url| !url.is_empty()).ok_or_else(|| {
            eyre::eyre!("Zone RPC is required; pass --zone-rpc-url or set ZONE_RPC_URL")
        })?;
        let zone_submit_rpc_url = zone_submit_rpc_url
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| zone_rpc_url.clone());

        Ok(Self {
            portal,
            zone_id,
            token,
            l1_rpc_url: normalize_http(&l1_rpc_url),
            l1_chain_id: None,
            zone_rpc_url: normalize_http(&zone_rpc_url),
            zone_chain_id: None,
            zone_submit_rpc_url: normalize_http(&zone_submit_rpc_url),
        })
    }

    pub(crate) async fn resolve(mut self) -> Result<Self> {
        let l1_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&self.l1_rpc_url)
            .await
            .wrap_err("failed connecting to L1 RPC")?;
        self.l1_chain_id = Some(
            l1_provider
                .get_chain_id()
                .await
                .wrap_err("failed reading L1 chain ID")?,
        );
        let zone_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&self.zone_rpc_url)
            .await
            .wrap_err("failed connecting to Zone RPC")?;
        self.zone_chain_id = Some(
            zone_provider
                .get_chain_id()
                .await
                .wrap_err("failed reading Zone chain ID")?,
        );
        Ok(self)
    }

    pub(crate) fn command_env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert("L1_RPC_URL".to_owned(), self.l1_rpc_url.clone());
        env.insert(
            "ZONES_BENCH_L1_QUERY_RPC_URL".to_owned(),
            self.l1_rpc_url.clone(),
        );
        insert_default(&mut env, "L1_WS_RPC_URL", websocket_url(&self.l1_rpc_url));
        env.insert("ZONE_RPC_URL".to_owned(), self.zone_rpc_url.clone());
        env.insert(
            "ZONE_REDACTED_RPC_URL".to_owned(),
            self.zone_submit_rpc_url.clone(),
        );
        insert_default(
            &mut env,
            "ZONE_WS_RPC_URL",
            websocket_url(&self.zone_rpc_url),
        );
        env.insert("L1_PORTAL_ADDRESS".to_owned(), self.portal.to_string());
        env.insert(
            "ZONES_BENCH_EXPECTED_ZONE_ID".to_owned(),
            self.zone_id.to_string(),
        );
        env.insert("ZONES_LOAD_ZONE_ID".to_owned(), self.zone_id.to_string());
        if let Some(chain_id) = self.zone_chain_id {
            env.insert(
                "ZONES_BENCH_EXPECTED_ZONE_CHAIN_ID".to_owned(),
                chain_id.to_string(),
            );
            env.insert(
                "ZONES_LOAD_EXPECTED_ZONE_CHAIN_ID".to_owned(),
                chain_id.to_string(),
            );
        }
        if let Some(chain_id) = self.l1_chain_id {
            env.insert(
                "ZONES_BENCH_EXPECTED_L1_CHAIN_ID".to_owned(),
                chain_id.to_string(),
            );
            env.insert(
                "ZONES_LOAD_EXPECTED_L1_CHAIN_ID".to_owned(),
                chain_id.to_string(),
            );
        }
        env.insert("ZONES_BENCH_TOKEN".to_owned(), self.token.to_string());
        env.insert("ZONES_LOAD_TOKEN".to_owned(), self.token.to_string());

        insert_default(&mut env, "ZONES_BENCH_L1_MAX_FEE_PER_GAS", "12000000000");
        insert_default(&mut env, "ZONES_BENCH_L1_MAX_PRIORITY_FEE_PER_GAS", "0");
        insert_default(&mut env, "ZONES_BENCH_ZONE_MAX_FEE_PER_GAS", "10000000000");
        insert_default(&mut env, "ZONES_BENCH_ZONE_MAX_PRIORITY_FEE_PER_GAS", "0");
        insert_default(&mut env, "ZONES_BENCH_DEPOSIT_AMOUNT", "2000000");
        insert_default(&mut env, "ZONES_BENCH_WITHDRAWAL_AMOUNT", "1000000");
        insert_default(&mut env, "ZONES_BENCH_CALLBACK_GAS_LIMIT", "10000000");
        insert_default(&mut env, "ZONES_LOAD_L1_MAX_FEE_PER_GAS", "12000000000");
        insert_default(&mut env, "ZONES_LOAD_ZONE_MAX_FEE_PER_GAS", "10000000000");
        insert_default(&mut env, "ZONES_LOAD_DEPOSIT_AMOUNT", "2000000");
        insert_default(&mut env, "ZONES_LOAD_WITHDRAWAL_AMOUNT", "1000000");
        insert_default(&mut env, "ZONES_LOAD_ACCOUNT_START", "16");
        insert_default(&mut env, "ZONES_LOAD_ACCOUNT_END", "116");
        env.insert(
            "ZONES_LOAD_INBOX".to_owned(),
            ZONE_INBOX_ADDRESS.to_string(),
        );
        env.insert(
            "ZONES_LOAD_OUTBOX".to_owned(),
            ZONE_OUTBOX_ADDRESS.to_string(),
        );
        if std::env::var("ZONES_LOAD_MNEMONIC").is_err()
            && let Ok(mnemonic) = std::env::var("ZONES_BENCH_MNEMONIC")
        {
            env.insert("ZONES_LOAD_MNEMONIC".to_owned(), mnemonic);
        }
        env
    }

    pub(crate) fn sanitized(&self) -> Self {
        let mut sanitized = self.clone();
        sanitized.l1_rpc_url = sanitize_url(&sanitized.l1_rpc_url);
        sanitized.zone_rpc_url = sanitize_url(&sanitized.zone_rpc_url);
        sanitized.zone_submit_rpc_url = sanitize_url(&sanitized.zone_submit_rpc_url);
        sanitized
    }

    pub(crate) fn missing_requirements(&self, requirements: &[String]) -> Vec<String> {
        let env = self.command_env();
        requirements
            .iter()
            .filter(|name| {
                let synthesized = env.get(*name).is_some_and(|value| !value.is_empty());
                let inherited = std::env::var(name).is_ok_and(|value| !value.is_empty());
                !synthesized && !inherited
            })
            .cloned()
            .collect()
    }

    pub(crate) async fn doctor(&self, requirements: &[String]) -> DoctorReport {
        let mut checks = Vec::new();

        match ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&self.l1_rpc_url)
            .await
        {
            Ok(provider) => {
                match provider.get_chain_id().await {
                    Ok(chain_id) => checks.push(pass("l1.rpc", format!("chain ID {chain_id}"))),
                    Err(error) => checks.push(fail("l1.rpc", error.to_string())),
                }
                match provider.get_block_number().await {
                    Ok(block) => checks.push(pass("l1.head", format!("block {block}"))),
                    Err(error) => checks.push(fail("l1.head", error.to_string())),
                }
                match provider.get_code_at(self.portal).await {
                    Ok(code) if !code.is_empty() => checks.push(pass(
                        "l1.portal.code",
                        format!("{} bytes at {}", code.len(), self.portal),
                    )),
                    Ok(_) => checks.push(fail(
                        "l1.portal.code",
                        format!("no code at {}", self.portal),
                    )),
                    Err(error) => checks.push(fail("l1.portal.code", error.to_string())),
                }

                let portal = ZonePortal::new(self.portal, &provider);
                match portal.zoneId().call().await {
                    Ok(zone_id) if zone_id == self.zone_id => {
                        checks.push(pass("l1.portal.zone_id", format!("zone ID {zone_id}")))
                    }
                    Ok(zone_id) => checks.push(fail(
                        "l1.portal.zone_id",
                        format!(
                            "portal reports {zone_id}, configured Zone ID is {}",
                            self.zone_id
                        ),
                    )),
                    Err(error) => checks.push(fail("l1.portal.zone_id", error.to_string())),
                }
                match portal.enabled_tokens().await {
                    Ok(tokens) if tokens.contains(&self.token) => checks.push(pass(
                        "l1.portal.enabled_tokens",
                        format!(
                            "configured token {} is one of {} enabled token(s)",
                            self.token,
                            tokens.len()
                        ),
                    )),
                    Ok(tokens) => checks.push(fail(
                        "l1.portal.enabled_tokens",
                        format!(
                            "configured token {} is not among {} enabled token(s)",
                            self.token,
                            tokens.len()
                        ),
                    )),
                    Err(error) => checks.push(fail("l1.portal.enabled_tokens", error.to_string())),
                }
                match portal.encryption_key().await {
                    Ok((_, index)) => checks.push(pass(
                        "l1.portal.encryption_key",
                        format!("active key index {index}"),
                    )),
                    Err(error) => checks.push(fail("l1.portal.encryption_key", error.to_string())),
                }
            }
            Err(error) => checks.push(fail("l1.connect", error.to_string())),
        }

        match ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&self.zone_rpc_url)
            .await
        {
            Ok(provider) => {
                match provider.get_chain_id().await {
                    Ok(chain_id) if Some(chain_id) == self.zone_chain_id => {
                        checks.push(pass("zone.rpc", format!("chain ID {chain_id}")))
                    }
                    Ok(chain_id) => checks.push(fail(
                        "zone.rpc",
                        format!(
                            "RPC reports chain ID {chain_id}, resolved chain ID is {}",
                            self.zone_chain_id.map_or_else(
                                || "unknown".to_owned(),
                                |expected| expected.to_string()
                            )
                        ),
                    )),
                    Err(error) => checks.push(fail("zone.rpc", error.to_string())),
                }
                match provider.get_block_number().await {
                    Ok(block) => checks.push(pass("zone.head", format!("block {block}"))),
                    Err(error) => checks.push(fail("zone.head", error.to_string())),
                }
                for (name, address) in [
                    ("zone.inbox.code", ZONE_INBOX_ADDRESS),
                    ("zone.outbox.code", ZONE_OUTBOX_ADDRESS),
                ] {
                    match provider.get_code_at(address).await {
                        Ok(code) if !code.is_empty() => {
                            checks.push(pass(name, format!("{} bytes at {address}", code.len())))
                        }
                        Ok(_) => checks.push(fail(name, format!("no code at {address}"))),
                        Err(error) => checks.push(fail(name, error.to_string())),
                    }
                }
            }
            Err(error) => checks.push(fail("zone.connect", error.to_string())),
        }

        let missing_requirements = self.missing_requirements(requirements);
        if missing_requirements.is_empty() {
            checks.push(pass("workload.environment", "all required values are set"));
        } else {
            checks.push(fail(
                "workload.environment",
                format!("missing {} value(s)", missing_requirements.len()),
            ));
        }

        DoctorReport {
            healthy: checks.iter().all(|check| check.passed),
            deployment: self.sanitized(),
            checks,
            missing_requirements,
        }
    }
}

fn normalize_http(url: &str) -> String {
    url.replace("wss://", "https://")
        .replace("ws://", "http://")
}

fn websocket_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        url.to_owned()
    }
}

fn sanitize_url(url: &str) -> String {
    let (prefix, rest) = url
        .split_once("://")
        .map_or(("", url), |(scheme, rest)| (scheme, rest));
    let rest = rest.split_once('#').map_or(rest, |(value, _)| value);
    let had_query = rest.contains('?');
    let rest = rest.split_once('?').map_or(rest, |(value, _)| value);
    let (authority, suffix) = rest
        .split_once('/')
        .map_or((rest, ""), |(authority, suffix)| (authority, suffix));
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let suffix = if suffix.is_empty() {
        if had_query { "?<redacted>" } else { "" }
    } else if had_query {
        "/<redacted>?<redacted>"
    } else {
        "/<redacted>"
    };
    if prefix.is_empty() {
        format!("{authority}{suffix}")
    } else {
        format!("{prefix}://{authority}{suffix}")
    }
}

fn insert_default(env: &mut BTreeMap<String, String>, name: &str, default: impl Into<String>) {
    let value = std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.into());
    env.insert(name.to_owned(), value);
}

fn pass(name: &str, detail: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        name: name.to_owned(),
        passed: true,
        detail: detail.into(),
    }
}

fn fail(name: &str, detail: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        name: name.to_owned(),
        passed: false,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_environment_from_explicit_deployment_inputs() {
        let portal = Address::repeat_byte(0x11);
        let token = Address::repeat_byte(0x22);
        let mut deployment = DeploymentContext::new(
            Some(portal),
            Some(7),
            Some(token),
            Some("ws://l1.example.test".to_owned()),
            Some("wss://zone.example.test".to_owned()),
            None,
        )
        .unwrap();
        deployment.l1_chain_id = Some(42431);
        deployment.zone_chain_id = Some(424310007);

        let env = deployment.command_env();
        assert_eq!(deployment.l1_rpc_url, "http://l1.example.test");
        assert_eq!(deployment.zone_rpc_url, "https://zone.example.test");
        assert_eq!(deployment.zone_submit_rpc_url, deployment.zone_rpc_url);
        assert_eq!(env["L1_PORTAL_ADDRESS"], portal.to_string());
        assert_eq!(env["ZONES_LOAD_ZONE_ID"], "7");
        assert_eq!(env["ZONES_LOAD_TOKEN"], token.to_string());
        assert_eq!(env["ZONES_LOAD_EXPECTED_L1_CHAIN_ID"], "42431");
        assert_eq!(env["ZONES_LOAD_EXPECTED_ZONE_CHAIN_ID"], "424310007");
    }

    #[test]
    fn rejects_zero_deployment_identifiers() {
        let error = DeploymentContext::new(
            Some(Address::ZERO),
            Some(7),
            Some(Address::repeat_byte(0x22)),
            Some("http://l1.example.test".to_owned()),
            Some("http://zone.example.test".to_owned()),
            None,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("--portal must not be the zero address")
        );
    }

    #[test]
    fn normalizes_rpc_schemes() {
        assert_eq!(normalize_http("ws://localhost:1"), "http://localhost:1");
        assert_eq!(websocket_url("https://example.test"), "wss://example.test");
        assert_eq!(
            sanitize_url("https://user:secret@example.test/rpc?token=secret"),
            "https://example.test/<redacted>?<redacted>"
        );
    }
}
