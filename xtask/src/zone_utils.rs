use alloy::{
    network::primitives::ReceiptResponse,
    primitives::{Address, B256, U256, address},
    providers::Provider,
    rpc::types::Filter,
    sol_types::SolEvent,
};
use eyre::{WrapErr as _, eyre};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tempo_alloy::TempoNetwork;
use tempo_contracts::precompiles::ITIP20 as TIP20Token;
use zone::abi::{ZoneInbox, ZonePortal};

pub(crate) const L1_EXPLORER: &str = "https://explore.moderato.tempo.xyz/tx";
pub(crate) const MODERATO_ZONE_FACTORY: Address =
    address!("0x7Cc496Dc634b718289c192b59CF90262C5228545");
pub(crate) const STABLECOIN_DEX_ADDRESS: Address =
    address!("0xDEc0000000000000000000000000000000000000");
pub(crate) const ROUTER_CALLBACK_GAS_LIMIT: u64 = 2_000_000;
const DEFAULT_WAIT_ATTEMPTS: usize = 120;
const DEFAULT_WAIT_POLL: Duration = Duration::from_millis(500);

pub(crate) struct ZoneMetadata {
    path: PathBuf,
    value: Value,
}

impl ZoneMetadata {
    pub(crate) fn load(zone_dir: &Path) -> eyre::Result<Self> {
        let path = zone_dir.join("zone.json");
        let contents = std::fs::read_to_string(&path)
            .wrap_err_with(|| format!("failed reading {}", path.display()))?;
        let value: Value = serde_json::from_str(&contents)
            .wrap_err_with(|| format!("failed parsing {}", path.display()))?;
        if !value.is_object() {
            return Err(eyre!(
                "expected {} to contain a JSON object",
                path.display()
            ));
        }
        Ok(Self { path, value })
    }

    pub(crate) fn save(&self) -> eyre::Result<()> {
        std::fs::write(
            &self.path,
            serde_json::to_string_pretty(&self.value).wrap_err("failed encoding zone.json")?,
        )
        .wrap_err_with(|| format!("failed writing {}", self.path.display()))
    }

    pub(crate) fn get_required_address(&self, key: &str) -> eyre::Result<Address> {
        let value = self
            .value
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| eyre!("{key} not found in {}", self.path.display()))?;
        value
            .parse()
            .wrap_err_with(|| format!("invalid {key} address in {}", self.path.display()))
    }

    pub(crate) fn get_optional_address(&self, key: &str) -> eyre::Result<Option<Address>> {
        self.value
            .get(key)
            .and_then(Value::as_str)
            .map(|value| {
                value
                    .parse()
                    .wrap_err_with(|| format!("invalid {key} address in {}", self.path.display()))
            })
            .transpose()
    }

    pub(crate) fn get_optional_u32(&self, key: &str) -> eyre::Result<Option<u32>> {
        self.value
            .get(key)
            .map(|value| {
                value
                    .as_u64()
                    .ok_or_else(|| eyre!("{key} in {} is not an integer", self.path.display()))
                    .and_then(|value| {
                        u32::try_from(value).map_err(|_| {
                            eyre!("{key} in {} does not fit in u32", self.path.display())
                        })
                    })
            })
            .transpose()
    }

    pub(crate) fn get_optional_string(&self, key: &str) -> Option<String> {
        self.value
            .get(key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    }

    pub(crate) fn set_address(&mut self, key: &str, value: Address) {
        self.set_string(key, value.to_string());
    }

    pub(crate) fn set_string(&mut self, key: &str, value: impl Into<String>) {
        if let Some(obj) = self.value.as_object_mut() {
            obj.insert(key.to_string(), Value::String(value.into()));
        }
    }
}

pub(crate) fn normalize_http_rpc(rpc_url: &str) -> String {
    rpc_url
        .replace("wss://", "https://")
        .replace("ws://", "http://")
}

pub(crate) fn check(receipt: &impl ReceiptResponse, label: &str) -> eyre::Result<()> {
    if !receipt.status() {
        return Err(eyre!("{label} reverted"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EncryptedDepositResult {
    Processed {
        block: u64,
        token: Address,
        sender: Address,
        to: Address,
        amount: u128,
        memo: B256,
    },
    Failed {
        block: u64,
        token: Address,
        sender: Address,
        amount: u128,
    },
}

pub(crate) async fn fund_l1_wallet<P: Provider<TempoNetwork>>(
    provider: &P,
    address: Address,
) -> eyre::Result<()> {
    let _: Vec<alloy::primitives::B256> = provider
        .raw_request("tempo_fundAddress".into(), (address,))
        .await
        .wrap_err("tempo_fundAddress RPC request failed")?;
    Ok(())
}

pub(crate) async fn token_balance<P: Provider<TempoNetwork>>(
    provider: &P,
    token: Address,
    account: Address,
) -> eyre::Result<U256> {
    TIP20Token::new(token, provider)
        .balanceOf(account)
        .call()
        .await
        .wrap_err("balanceOf failed")
}

pub(crate) async fn wait_for_token_enabled<P: Provider<TempoNetwork>>(
    l2: &P,
    from_block: u64,
    token: Address,
) -> eyre::Result<u64> {
    let filter = Filter::new()
        .address(zone::abi::ZONE_INBOX_ADDRESS)
        .event_signature(ZoneInbox::TokenEnabled::SIGNATURE_HASH)
        .from_block(from_block);

    for _ in 0..DEFAULT_WAIT_ATTEMPTS {
        let logs = l2.get_logs(&filter).await.unwrap_or_default();
        for log in &logs {
            if let Ok(event) = ZoneInbox::TokenEnabled::decode_log(&log.inner)
                && event.data.token == token
            {
                return Ok(log.block_number.unwrap_or(0));
            }
        }
        tokio::time::sleep(DEFAULT_WAIT_POLL).await;
    }

    Err(eyre!("timeout waiting for TokenEnabled event on L2"))
}

pub(crate) async fn wait_for_deposit_processed<P: Provider<TempoNetwork>>(
    l2: &P,
    from_block: u64,
    sender: Address,
    to: Address,
    token: Address,
) -> eyre::Result<u64> {
    let filter = Filter::new()
        .address(zone::abi::ZONE_INBOX_ADDRESS)
        .event_signature(ZoneInbox::DepositProcessed::SIGNATURE_HASH)
        .from_block(from_block);

    for _ in 0..DEFAULT_WAIT_ATTEMPTS {
        let logs = l2.get_logs(&filter).await.unwrap_or_default();
        for log in &logs {
            if let Ok(event) = ZoneInbox::DepositProcessed::decode_log(&log.inner)
                && event.data.sender == sender
                && event.data.to == to
                && event.data.token == token
            {
                return Ok(log.block_number.unwrap_or(0));
            }
        }
        tokio::time::sleep(DEFAULT_WAIT_POLL).await;
    }

    Err(eyre!("timeout waiting for DepositProcessed"))
}

pub(crate) async fn wait_for_balance<P: Provider<TempoNetwork>>(
    provider: &P,
    token: Address,
    account: Address,
    expected_balance: U256,
    label: &str,
) -> eyre::Result<U256> {
    for _ in 0..DEFAULT_WAIT_ATTEMPTS {
        let balance = token_balance(provider, token, account)
            .await
            .unwrap_or_default();
        if balance >= expected_balance {
            return Ok(balance);
        }
        tokio::time::sleep(DEFAULT_WAIT_POLL).await;
    }

    Err(eyre!(
        "timeout waiting for {label} balance to reach {expected_balance}"
    ))
}

pub(crate) async fn wait_for_withdrawal_processed<P: Provider<TempoNetwork>>(
    l1: &P,
    from_block: u64,
    portal: Address,
    to: Address,
    token: Address,
    amount: u128,
    callback_success: bool,
) -> eyre::Result<u64> {
    let filter = Filter::new()
        .address(portal)
        .event_signature(ZonePortal::WithdrawalProcessed::SIGNATURE_HASH)
        .from_block(from_block);

    for _ in 0..(DEFAULT_WAIT_ATTEMPTS * 2) {
        let logs = l1.get_logs(&filter).await.unwrap_or_default();
        for log in &logs {
            if let Ok(event) = ZonePortal::WithdrawalProcessed::decode_log(&log.inner)
                && event.data.to == to
                && event.data.token == token
                && event.data.amount == amount
                && event.data.callbackSuccess == callback_success
            {
                return Ok(log.block_number.unwrap_or(0));
            }
        }
        tokio::time::sleep(DEFAULT_WAIT_POLL).await;
    }

    Err(eyre!(
        "timeout waiting for WithdrawalProcessed(to={to}, token={token}, amount={amount}, callbackSuccess={callback_success})"
    ))
}

pub(crate) async fn wait_for_encrypted_deposit_result<P: Provider<TempoNetwork>>(
    l2: &P,
    from_block: u64,
    sender: Address,
    to: Option<Address>,
    token: Option<Address>,
    memo: Option<B256>,
) -> eyre::Result<EncryptedDepositResult> {
    let processed_filter = Filter::new()
        .address(zone::abi::ZONE_INBOX_ADDRESS)
        .event_signature(ZoneInbox::EncryptedDepositProcessed::SIGNATURE_HASH)
        .from_block(from_block);

    let failed_filter = Filter::new()
        .address(zone::abi::ZONE_INBOX_ADDRESS)
        .event_signature(ZoneInbox::EncryptedDepositFailed::SIGNATURE_HASH)
        .from_block(from_block);

    loop {
        let logs = l2.get_logs(&processed_filter).await.unwrap_or_default();
        for log in &logs {
            if let Ok(event) = ZoneInbox::EncryptedDepositProcessed::decode_log(&log.inner)
                && event.data.sender == sender
                && to.is_none_or(|to| event.data.to == to)
                && token.is_none_or(|token| event.data.token == token)
                && memo.is_none_or(|memo| event.data.memo == memo)
            {
                return Ok(EncryptedDepositResult::Processed {
                    block: log.block_number.unwrap_or(0),
                    token: event.data.token,
                    sender: event.data.sender,
                    to: event.data.to,
                    amount: event.data.amount,
                    memo: event.data.memo,
                });
            }
        }

        let failed_logs = l2.get_logs(&failed_filter).await.unwrap_or_default();
        for log in &failed_logs {
            if let Ok(event) = ZoneInbox::EncryptedDepositFailed::decode_log(&log.inner)
                && event.data.sender == sender
                && token.is_none_or(|token| event.data.token == token)
            {
                return Ok(EncryptedDepositResult::Failed {
                    block: log.block_number.unwrap_or(0),
                    token: event.data.token,
                    sender: event.data.sender,
                    amount: event.data.amount,
                });
            }
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
